//! The body of the cron drain: the only place Discord history is read and
//! filed as sources. Reached only from `drain::run`, with the [`Drain`] that
//! run minted.
//!
//! # One tick
//!
//! The tick may make `min(DISCORD_TICK_BUDGET (default 200), the invocation's
//! remaining subrequests)` calls, and stops at the scheduled deadline. In order:
//!
//! 1. **Each connected server, least recently listed first.** If its channel
//!    list is over an hour old it is re-read (channels, active threads), and
//!    each text channel's archived threads are listed a page at a time.
//! 2. **Each included channel that may be read now.** A channel is claimed under
//!    a lease first (one compare-and-set, so overlapping runs never read the
//!    same channel). Backfill pages backwards with `before=` (at most
//!    [`BACKFILL_PAGES`] pages a tick) until an empty page; then the forward poll
//!    reads `after=` the live cursor (at most [`POLL_PAGES`] pages). Messages
//!    mark their channel-days dirty; the cursors move only after the marks land.
//! 3. **Dirty days** (at most [`MAX_DAY_FILINGS`] a tick): each is re-read in
//!    full by its snowflake bounds and filed as one transcript.
//!
//! Any `429`, rate-limit exhaustion, `5xx` or transport failure holds all calls
//! for the rest of the tick (see `api::decide`); nothing is slept. A `403` or
//! `404` on one channel is recorded on that channel and the tick goes on.
//!
//! # Why a day is re-read rather than appended
//!
//! Backfill walks newest to oldest, so a day can arrive across several pages,
//! and a transcript written from a partial day would be wrong. The drain keeps
//! no message content: it re-reads a dirty day whole and files that. A busy
//! day is refiled at most once an hour (`REFILE_EVERY_SECS`), except a closed
//! day, which is filed at once.
//!
//! # Filing
//!
//! Every transcript goes through `livingbrain_pages::Ingestor` (screen, redact,
//! dedupe on `(scope, sha256)`, seal, queue for compilation) in the workspace's
//! shared scope, with `origin_ref` the channel's permalink and `imported_by`
//! the member who connected the server. A day whose text is unchanged files
//! nothing new.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use cratefield_core::{Clock, Database, HttpClient, ModuleContext, ScheduledBudget};
use livingbrain_pages::{IngestError, Ingestor, SourceIngest, SourceKind, SourceStore};

use crate::Shared;
use crate::api::{self, ActiveThreads, ArchivedThreads, Channel as ApiChannel, Message, Verdict};
use crate::common::{StoreError, unix};
use crate::config::DiscordApp;
use crate::drain::Drain;
use crate::render;
use crate::store::{self, Advance, Connection, Day};

/// The default number of calls a tick may make.
const TICK_BUDGET: u32 = 200;
/// The most calls a tick may be configured to make.
const MAX_TICK_BUDGET: u32 = 2_000;
/// The page size Discord allows for message and thread listings.
const PAGE: usize = 100;
/// Backfill pages read for one channel in one tick.
const BACKFILL_PAGES: u32 = 5;
/// Forward-poll pages read for one channel in one tick.
const POLL_PAGES: u32 = 3;
/// Servers handled per tick.
const CONNECTION_LIMIT: u32 = 25;
/// Channel-days filed per tick.
const MAX_DAY_FILINGS: u32 = 20;
/// Archived-thread pages read per tick, across the server's text channels.
const ARCHIVE_PAGES: u32 = 20;
/// How long a channel lease is held. The cron fires every five minutes and an
/// invocation may run for fifteen, so this is the invocation's own wall.
const LEASE_SECS: i64 = 15 * 60;
/// The channel list is re-read this often.
const DISCOVERY_EVERY_SECS: i64 = 60 * 60;
/// A busy day is refiled at most this often.
const REFILE_EVERY_SECS: i64 = 60 * 60;
/// A channel Discord refused is tried again after this long.
const REFUSED_RETRY_SECS: i64 = 60 * 60;
/// Calls to hold after a transport failure.
const TRANSPORT_HOLD_SECS: u64 = 30;
/// Who files a transcript when the member who connected the server is gone.
pub(crate) const SYSTEM_IMPORTER: &str = "brain:discord";

/// The channel kinds a server's listing imports, and the thread kinds.
const CHANNEL_KINDS: [u8; 4] = [0, 2, 5, 13];
const THREAD_KINDS: [u8; 3] = [10, 11, 12];

/// Why a step stopped. `Stop` ends the tick (a wait, the budget, or a failed
/// transport); the rest is for the caller.
enum Halt {
    Stop,
    Store(StoreError),
}

impl From<StoreError> for Halt {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

/// What one Discord call gave back.
enum Got {
    Ok(api::Reply),
    Missing,
    Denied,
    Failed(String),
}

pub(crate) async fn run(
    ctx: &ModuleContext,
    drain: &Drain,
    shared: &Shared,
) -> Result<(), StoreError> {
    let Some(kms) = shared.kms.clone() else {
        return Ok(());
    };
    let Some(app) = DiscordApp::from_config(ctx.config.as_ref()) else {
        return Ok(());
    };
    let ports = &ctx.ports;
    let (Some(db), Some(clock), Some(http), Some(blob), Some(id_gen), Some(defer)) = (
        ports.db.clone(),
        ports.clock.clone(),
        ports.http.clone(),
        ports.blob.clone(),
        ports.id_gen.clone(),
        ports.defer.clone(),
    ) else {
        return Ok(());
    };
    let ingestor = Ingestor::new(
        SourceStore::new(db.clone(), blob, kms.clone(), clock.clone(), id_gen),
        defer,
    );
    let budget = Arc::clone(&ctx.scheduled);
    let start = budget.spent();
    let ceiling = tick_budget(ctx.config.get("DISCORD_TICK_BUDGET").as_deref());
    let cap = budget
        .remaining_subrequests()
        .map_or(ceiling, |remaining| remaining.min(ceiling));
    let mut tick = Tick {
        db: &*db,
        http: &*http,
        clock: &*clock,
        budget: &budget,
        start,
        cap,
        ingestor: &ingestor,
        _drain: drain,
        token: app.bot_token.clone(),
        hold_until: None,
    };
    tick.run().await
}

/// The tick's call budget from `DISCORD_TICK_BUDGET`: a whole number, clamped
/// to `1..=2000`, default 200.
#[must_use]
pub fn tick_budget(raw: Option<&str>) -> u32 {
    raw.and_then(|value| value.trim().parse::<u32>().ok())
        .map_or(TICK_BUDGET, |value| value.clamp(1, MAX_TICK_BUDGET))
}

struct Tick<'a> {
    db: &'a dyn Database,
    http: &'a dyn HttpClient,
    clock: &'a dyn Clock,
    budget: &'a ScheduledBudget,
    start: u32,
    cap: u32,
    ingestor: &'a Ingestor,
    /// Held for the tick: the capability that makes this code the scheduled
    /// drain's (see `drain.rs`).
    _drain: &'a Drain,
    token: crate::common::Secret,
    /// No call before this unix second (set by a wait this tick).
    hold_until: Option<i64>,
}

impl Tick<'_> {
    fn now(&self) -> i64 {
        unix(self.clock.now())
    }

    fn used(&self) -> u32 {
        self.budget.spent().saturating_sub(self.start)
    }

    /// Calls this tick may still make.
    fn left(&self) -> u32 {
        if self.budget.expired(self.clock.now())
            || self.hold_until.is_some_and(|until| until > self.now())
        {
            return 0;
        }
        let by_cap = self.cap.saturating_sub(self.used());
        self.budget
            .remaining_subrequests()
            .map_or(by_cap, |remaining| remaining.min(by_cap))
    }

    fn spend(&self) -> bool {
        self.left() > 0 && self.budget.try_spend(1)
    }

    fn hold(&mut self, seconds: u64) {
        let until = self.now() + i64::try_from(seconds).unwrap_or(i64::MAX / 4);
        self.hold_until = Some(self.hold_until.map_or(until, |current| current.max(until)));
    }

    /// Makes one bot `GET`. A usable answer (even with an empty bucket) is
    /// returned; a wait, a budget end or a transport failure stops the tick.
    async fn get(&mut self, path: &str) -> Result<Got, Halt> {
        if !self.spend() {
            return Err(Halt::Stop);
        }
        let reply = match api::bot_get(self.http, &self.token, path).await {
            Ok(reply) => reply,
            Err(_) => {
                self.hold(TRANSPORT_HOLD_SECS);
                return Err(Halt::Stop);
            }
        };
        let status = reply.status;
        match api::decide(status, reply.rate) {
            Verdict::Ok => Ok(Got::Ok(reply)),
            Verdict::Wait { seconds } => {
                self.hold(seconds);
                if (200..300).contains(&status) {
                    Ok(Got::Ok(reply))
                } else {
                    Err(Halt::Stop)
                }
            }
            Verdict::Missing => Ok(Got::Missing),
            Verdict::Denied => Ok(Got::Denied),
            Verdict::Failed => Ok(Got::Failed(format!("Discord answered {status}"))),
        }
    }

    async fn run(&mut self) -> Result<(), StoreError> {
        let connections = store::active_connections(self.db, CONNECTION_LIMIT).await?;
        for connection in &connections {
            if self.left() < 2 {
                break;
            }
            match self.connection(connection).await {
                Ok(()) => {}
                Err(Halt::Stop) => return Ok(()),
                Err(Halt::Store(error)) => return Err(error),
            }
        }
        match self.file_days().await {
            Ok(()) | Err(Halt::Stop) => Ok(()),
            Err(Halt::Store(error)) => Err(error),
        }
    }

    async fn connection(&mut self, connection: &Connection) -> Result<(), Halt> {
        let now = self.now();
        let due = connection
            .channels_synced_at
            .is_none_or(|at| at + DISCOVERY_EVERY_SECS <= now);
        if due && !self.discover(connection).await? {
            return Ok(());
        }
        self.archives(connection).await?;

        let mut channels = store::channels_of(self.db, &connection.guild_id).await?;
        // Backfills first (new history matters most), then the least recently polled.
        channels.sort_by_key(|c| {
            (
                c.backfill_state == "done",
                c.polled_at.unwrap_or(0),
                c.channel_id.clone(),
            )
        });
        for channel in channels {
            if !channel.included || !channel.present || self.left() < 1 {
                continue;
            }
            let token = crate::common::random_hex(12)?;
            if !store::claim(
                self.db,
                self.clock.now(),
                &channel.guild_id,
                &channel.channel_id,
                &token,
                LEASE_SECS,
            )
            .await?
            {
                continue;
            }
            let outcome = self.channel(connection, &channel, &token).await;
            store::release(self.db, &channel.guild_id, &channel.channel_id, &token).await?;
            outcome?;
        }
        Ok(())
    }

    /// Re-reads a server's channel list. Answers `false` when Discord would not
    /// list it, which is recorded on the server and retried next tick.
    async fn discover(&mut self, connection: &Connection) -> Result<bool, Halt> {
        let guild = connection.guild_id.clone();
        let listed = self.get(&format!("/guilds/{guild}/channels")).await?;
        let channels: Vec<ApiChannel> = match listed {
            Got::Ok(reply) => match reply.json() {
                Some(channels) => channels,
                None => {
                    return self
                        .discovery_failed(
                            &guild,
                            "Discord sent a channel list this import could not read",
                        )
                        .await;
                }
            },
            Got::Missing => {
                return self
                    .discovery_failed(&guild, "Discord no longer lists this server")
                    .await;
            }
            Got::Denied => {
                return self
                    .discovery_failed(&guild, "the bot can no longer see this server")
                    .await;
            }
            Got::Failed(_) => {
                return self
                    .discovery_failed(&guild, "Discord would not list this server's channels")
                    .await;
            }
        };
        let threads: Vec<ApiChannel> =
            match self.get(&format!("/guilds/{guild}/threads/active")).await? {
                Got::Ok(reply) => reply
                    .json::<ActiveThreads>()
                    .map(|a| a.threads)
                    .unwrap_or_default(),
                _ => Vec::new(),
            };
        let nsfw_of: HashMap<&str, bool> = channels
            .iter()
            .map(|c| (c.id.as_str(), c.nsfw.unwrap_or(false)))
            .collect();
        let mut seen = Vec::new();
        for channel in channels.iter().filter(|c| CHANNEL_KINDS.contains(&c.kind)) {
            let nsfw = channel.nsfw.unwrap_or(false);
            store::upsert_channel(
                self.db,
                self.clock.now(),
                &connection.workspace_id,
                &guild,
                &channel.id,
                channel.name.as_deref().unwrap_or(""),
                i64::from(channel.kind),
                nsfw,
                None,
                !nsfw,
            )
            .await?;
            seen.push(channel.id.clone());
        }
        for thread in threads.iter().filter(|t| THREAD_KINDS.contains(&t.kind)) {
            let parent_nsfw = thread
                .parent_id
                .as_deref()
                .and_then(|p| nsfw_of.get(p).copied())
                .unwrap_or(false);
            store::upsert_channel(
                self.db,
                self.clock.now(),
                &connection.workspace_id,
                &guild,
                &thread.id,
                thread.name.as_deref().unwrap_or(""),
                i64::from(thread.kind),
                parent_nsfw,
                thread.parent_id.as_deref(),
                !parent_nsfw,
            )
            .await?;
        }
        store::mark_missing(self.db, self.clock.now(), &guild, &seen).await?;
        store::set_channels_synced(self.db, self.clock.now(), &guild).await?;
        store::record_connection_error(self.db, self.clock.now(), &guild, None).await?;
        Ok(true)
    }

    /// A server Discord would not list is recorded on the server and tried
    /// again next tick; nothing is dropped.
    async fn discovery_failed(&mut self, guild: &str, text: &str) -> Result<bool, Halt> {
        store::record_connection_error(self.db, self.clock.now(), guild, Some(text)).await?;
        Ok(false)
    }

    /// Lists the archived public threads of the server's text channels, a page
    /// at a time, within [`ARCHIVE_PAGES`].
    async fn archives(&mut self, connection: &Connection) -> Result<(), Halt> {
        let guild = connection.guild_id.clone();
        for channel in store::channels_to_archive(self.db, &guild)
            .await?
            .into_iter()
            .take(ARCHIVE_PAGES as usize)
        {
            if self.left() < 1 {
                break;
            }
            let mut path = format!(
                "/channels/{}/threads/archived/public?limit={PAGE}",
                channel.channel_id
            );
            if let Some(before) = &channel.archive_before {
                path.push_str("&before=");
                path.push_str(&api::encode(before));
            }
            match self.get(&path).await? {
                Got::Ok(reply) => {
                    let page = reply.json::<ArchivedThreads>().unwrap_or(ArchivedThreads {
                        threads: Vec::new(),
                        has_more: false,
                    });
                    for thread in &page.threads {
                        store::upsert_channel(
                            self.db,
                            self.clock.now(),
                            &connection.workspace_id,
                            &guild,
                            &thread.id,
                            thread.name.as_deref().unwrap_or(""),
                            i64::from(thread.kind),
                            channel.nsfw,
                            Some(&channel.channel_id),
                            !channel.nsfw,
                        )
                        .await?;
                    }
                    let next = page
                        .has_more
                        .then(|| {
                            page.threads
                                .last()
                                .and_then(|t| t.thread_metadata.as_ref()?.archive_timestamp.clone())
                        })
                        .flatten();
                    let state = if next.is_some() { "pending" } else { "done" };
                    store::set_archive(
                        self.db,
                        &guild,
                        &channel.channel_id,
                        state,
                        next.as_deref(),
                    )
                    .await?;
                }
                Got::Missing | Got::Denied | Got::Failed(_) => {
                    // Nothing to list here: stop asking, keep the channel.
                    store::set_archive(self.db, &guild, &channel.channel_id, "done", None).await?;
                }
            }
        }
        Ok(())
    }

    /// Reads one channel: backfill, then the forward poll. A refusal from
    /// Discord is recorded on the channel and is not an error for the tick.
    async fn channel(
        &mut self,
        connection: &Connection,
        channel: &store::Channel,
        token: &str,
    ) -> Result<(), Halt> {
        let guild = connection.guild_id.clone();
        let id = channel.channel_id.clone();
        let mut oldest = channel.oldest_id.clone();
        let mut newest = channel.newest_id.clone();
        let mut state = channel.backfill_state.clone();

        if state != "done" {
            for _ in 0..BACKFILL_PAGES {
                let path = match &oldest {
                    Some(before) => format!("/channels/{id}/messages?limit={PAGE}&before={before}"),
                    None => format!("/channels/{id}/messages?limit={PAGE}"),
                };
                let messages = match self.get(&path).await? {
                    Got::Ok(reply) => match reply.json::<Vec<Message>>() {
                        Some(messages) => messages,
                        None => return self.refused(&guild, &id, unreadable(), token).await,
                    },
                    other => return self.refused(&guild, &id, other, token).await,
                };
                if messages.is_empty() {
                    let newest_now = newest.clone().or_else(|| Some("0".to_owned()));
                    let step = Advance {
                        backfill_state: Some("done"),
                        newest_id: newest_now
                            .as_deref()
                            .filter(|_| channel.newest_id.is_none()),
                        ..Advance::default()
                    };
                    store::advance(self.db, self.clock.now(), token, &guild, &id, &step).await?;
                    state = "done".to_owned();
                    newest = newest_now;
                    break;
                }
                self.mark_dirty(connection, &id, &messages).await?;
                let page_oldest = messages
                    .iter()
                    .map(|m| m.id.clone())
                    .min_by_key(|id| render::id_number(id));
                let page_newest = messages
                    .iter()
                    .map(|m| m.id.clone())
                    .max_by_key(|id| render::id_number(id));
                let added = messages.iter().filter(|m| render::keeps(m)).count();
                let set_newest = if newest.is_none() {
                    page_newest.clone()
                } else {
                    None
                };
                let step = Advance {
                    backfill_state: Some("running"),
                    oldest_id: page_oldest.as_deref(),
                    newest_id: set_newest.as_deref(),
                    added: i64::try_from(added).unwrap_or(0),
                    polled: false,
                };
                store::advance(self.db, self.clock.now(), token, &guild, &id, &step).await?;
                oldest = page_oldest;
                if newest.is_none() {
                    newest = page_newest;
                }
                if self.left() < 1 {
                    return Ok(());
                }
            }
        }

        if state == "done" {
            for _ in 0..POLL_PAGES {
                let after = newest.clone().unwrap_or_else(|| "0".to_owned());
                let messages = match self
                    .get(&format!(
                        "/channels/{id}/messages?limit={PAGE}&after={after}"
                    ))
                    .await?
                {
                    Got::Ok(reply) => match reply.json::<Vec<Message>>() {
                        Some(messages) => messages,
                        None => return self.refused(&guild, &id, unreadable(), token).await,
                    },
                    other => return self.refused(&guild, &id, other, token).await,
                };
                if messages.is_empty() {
                    break;
                }
                self.mark_dirty(connection, &id, &messages).await?;
                let page_newest = messages
                    .iter()
                    .map(|m| m.id.clone())
                    .max_by_key(|id| render::id_number(id));
                let added = messages.iter().filter(|m| render::keeps(m)).count();
                let step = Advance {
                    newest_id: page_newest.as_deref(),
                    added: i64::try_from(added).unwrap_or(0),
                    ..Advance::default()
                };
                store::advance(self.db, self.clock.now(), token, &guild, &id, &step).await?;
                newest = page_newest;
                if messages.len() < PAGE || self.left() < 1 {
                    break;
                }
            }
            let polled = Advance {
                polled: true,
                ..Advance::default()
            };
            store::advance(self.db, self.clock.now(), token, &guild, &id, &polled).await?;
        }
        Ok(())
    }

    /// Marks the days of the kept messages dirty; returns them.
    async fn mark_dirty(
        &mut self,
        connection: &Connection,
        channel_id: &str,
        messages: &[Message],
    ) -> Result<Vec<String>, Halt> {
        let days: BTreeSet<String> = messages
            .iter()
            .filter(|m| render::keeps(m))
            .filter_map(|m| render::day_of(&m.timestamp))
            .collect();
        let days: Vec<String> = days.into_iter().collect();
        store::mark_days_dirty(
            self.db,
            self.clock.now(),
            &connection.workspace_id,
            &connection.guild_id,
            channel_id,
            &days,
        )
        .await?;
        Ok(days)
    }

    /// A channel Discord refused: recorded on the channel, and the tick goes on.
    async fn refused(
        &mut self,
        guild: &str,
        channel_id: &str,
        got: Got,
        token: &str,
    ) -> Result<(), Halt> {
        let now = self.clock.now();
        match got {
            Got::Missing => store::remove_channel(self.db, now, guild, channel_id).await?,
            Got::Denied => {
                let retry = unix(now) + REFUSED_RETRY_SECS;
                store::record_error(
                    self.db,
                    now,
                    guild,
                    channel_id,
                    "the bot may not read this channel",
                    Some(retry),
                )
                .await?;
            }
            Got::Failed(text) => {
                let retry = unix(now) + REFUSED_RETRY_SECS;
                store::record_error(self.db, now, guild, channel_id, &text, Some(retry)).await?;
            }
            Got::Ok(_) => {}
        }
        store::release(self.db, guild, channel_id, token).await?;
        Ok(())
    }

    /// Files dirty channel-days, oldest first, within [`MAX_DAY_FILINGS`].
    async fn file_days(&mut self) -> Result<(), Halt> {
        let now = self.clock.now();
        let today = utc_day(now);
        let refile_before = now - time::Duration::seconds(REFILE_EVERY_SECS);
        let days = store::dirty_days(self.db, now, refile_before, &today, MAX_DAY_FILINGS).await?;
        for day in days {
            if self.left() < 1 {
                break;
            }
            self.file_day(&day).await?;
        }
        Ok(())
    }

    /// Re-reads one channel-day whole and files it. A day that cannot be read
    /// now stays dirty for the next tick.
    async fn file_day(&mut self, day: &Day) -> Result<(), Halt> {
        let Some(connection) = store::connection(self.db, &day.guild_id).await? else {
            return Ok(());
        };
        let Some(channel) = store::channel(self.db, &day.guild_id, &day.channel_id).await? else {
            return Ok(());
        };
        let Some((start, end)) = render::day_bounds(&day.day) else {
            store::mark_day_filed(self.db, self.clock.now(), day).await?;
            return Ok(());
        };
        let mut messages: Vec<Message> = Vec::new();
        let mut after = start.saturating_sub(1);
        loop {
            let page = match self
                .get(&format!(
                    "/channels/{}/messages?limit={PAGE}&after={after}",
                    day.channel_id
                ))
                .await?
            {
                Got::Ok(reply) => match reply.json::<Vec<Message>>() {
                    Some(page) => page,
                    None => return Ok(()),
                },
                Got::Missing => {
                    store::remove_channel(
                        self.db,
                        self.clock.now(),
                        &day.guild_id,
                        &day.channel_id,
                    )
                    .await?;
                    return Ok(());
                }
                Got::Denied => {
                    let retry = unix(self.clock.now()) + REFUSED_RETRY_SECS;
                    store::record_error(
                        self.db,
                        self.clock.now(),
                        &day.guild_id,
                        &day.channel_id,
                        "the bot may not read this channel",
                        Some(retry),
                    )
                    .await?;
                    return Ok(());
                }
                Got::Failed(text) => {
                    let retry = unix(self.clock.now()) + REFUSED_RETRY_SECS;
                    store::record_error(
                        self.db,
                        self.clock.now(),
                        &day.guild_id,
                        &day.channel_id,
                        &text,
                        Some(retry),
                    )
                    .await?;
                    return Ok(());
                }
            };
            if page.is_empty() {
                break;
            }
            let newest = page
                .iter()
                .filter_map(|m| m.id.parse::<u64>().ok())
                .max()
                .unwrap_or(0);
            messages.extend(
                page.iter()
                    .filter(|m| {
                        let id = render::id_number(&m.id);
                        id >= start && id < end
                    })
                    .cloned(),
            );
            if page.len() < PAGE || newest >= end {
                break;
            }
            after = newest;
        }

        let name = channel.name.clone();
        if let Some(body) = render::transcript(&connection.guild_name, &name, &day.day, &messages) {
            let scope = livingbrain_pages::page_scope(
                &connection.workspace_id,
                &livingbrain_access::Scope::Shared,
            );
            let source = SourceIngest {
                kind: SourceKind::Discord,
                workspace: connection.workspace_id.clone(),
                scope,
                origin_ref: Some(render::origin_ref(&day.guild_id, &day.channel_id)),
                author: None,
                rel_path: render::rel_path(&day.guild_id, &day.channel_id, &day.day),
                imported_by: connection
                    .connected_by
                    .clone()
                    .unwrap_or_else(|| SYSTEM_IMPORTER.to_owned()),
                body,
                project: project_of(&connection.guild_name),
            };
            match self.ingestor.ingest(source).await {
                Ok(_) => {}
                Err(IngestError::Store(error)) => {
                    return Err(Halt::Store(StoreError::Ingest(error.to_string())));
                }
                Err(IngestError::InvalidScope(_)) => {
                    return Err(Halt::Store(StoreError::Ingest(
                        "the workspace scope is not valid".to_owned(),
                    )));
                }
                // Too large, refused by redaction, or a path the store will not
                // take: this day can never file, so it is marked done.
                Err(_) => {}
            }
        }
        store::mark_day_filed(self.db, self.clock.now(), day).await?;
        Ok(())
    }
}

/// `YYYY-MM-DD` of an instant, in UTC.
fn utc_day(now: time::OffsetDateTime) -> String {
    let date = now.to_offset(time::UtcOffset::UTC).date();
    format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        u8::from(date.month()),
        date.day()
    )
}

/// The server's name as a project a source may be filed under, or `None`.
fn project_of(guild_name: &str) -> Option<String> {
    let slug: String = guild_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug: String = slug
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let slug: String = slug.chars().take(40).collect();
    (!slug.is_empty() && livingbrain_pages::is_project(&slug)).then_some(slug)
}

/// The answer for a listing that did not parse.
fn unreadable() -> Got {
    Got::Failed("Discord sent an unreadable message list".to_owned())
}
