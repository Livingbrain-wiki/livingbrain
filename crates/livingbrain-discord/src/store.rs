//! The tables of the Discord connection: servers, their channels and threads
//! with each one's cursors, and the channel-days waiting to be filed.
//!
//! Admin writes (connect, disconnect, include or exclude) are made by the
//! routes after [`crate::common::require_admin`]. The cursor and progress
//! writes are made only by the cron drain, under a channel lease, so an
//! overlapping run cannot double-read a channel.

use std::collections::BTreeSet;

use cratefield_core::{Database, Statement};
use livingbrain_d1::{BIND_BUDGET, distinct_chunks, placeholders};

use crate::common::{
    StoreError, get_int, get_opt_int, get_opt_text, get_text, int, opt_int, opt_text, text, unix,
};

/// A connected server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connection {
    pub guild_id: String,
    pub workspace_id: String,
    pub guild_name: String,
    pub connected_by: Option<String>,
    pub connected_at: i64,
    pub disconnected_at: Option<i64>,
    pub channels_synced_at: Option<i64>,
    pub last_error: Option<String>,
}

const CONNECTION_COLUMNS: &str = "guild_id, workspace_id, guild_name, connected_by, connected_at, \
     disconnected_at, channels_synced_at, last_error";

fn connection_from(row: &cratefield_core::Row) -> Connection {
    Connection {
        guild_id: get_text(row, "guild_id"),
        workspace_id: get_text(row, "workspace_id"),
        guild_name: get_text(row, "guild_name"),
        connected_by: get_opt_text(row, "connected_by"),
        connected_at: get_int(row, "connected_at"),
        disconnected_at: get_opt_int(row, "disconnected_at"),
        channels_synced_at: get_opt_int(row, "channels_synced_at"),
        last_error: get_opt_text(row, "last_error"),
    }
}

/// One server by id, connected or not.
///
/// # Errors
///
/// A storage failure.
pub async fn connection(
    db: &dyn Database,
    guild_id: &str,
) -> Result<Option<Connection>, StoreError> {
    let rows = db
        .query(&Statement::with_values(
            format!("SELECT {CONNECTION_COLUMNS} FROM discord_connections WHERE guild_id = ?"),
            vec![text(guild_id)],
        ))
        .await?;
    Ok(rows.rows.first().map(connection_from))
}

/// The workspace's servers, connected ones first, by name.
///
/// # Errors
///
/// A storage failure.
pub async fn connections_of(
    db: &dyn Database,
    workspace_id: &str,
) -> Result<Vec<Connection>, StoreError> {
    let rows = db
        .query(&Statement::with_values(
            format!(
                "SELECT {CONNECTION_COLUMNS} FROM discord_connections WHERE workspace_id = ? \
                 ORDER BY disconnected_at IS NOT NULL, guild_name, guild_id"
            ),
            vec![text(workspace_id)],
        ))
        .await?;
    Ok(rows.rows.iter().map(connection_from).collect())
}

/// Every server still connected (across workspaces: the drain is the
/// system's), least recently synced first.
///
/// # Errors
///
/// A storage failure.
pub async fn active_connections(
    db: &dyn Database,
    limit: u32,
) -> Result<Vec<Connection>, StoreError> {
    let rows = db
        .query(&Statement::with_values(
            format!(
                "SELECT {CONNECTION_COLUMNS} FROM discord_connections \
                 WHERE disconnected_at IS NULL \
                 ORDER BY channels_synced_at IS NULL DESC, channels_synced_at, guild_id LIMIT ?"
            ),
            vec![int(i64::from(limit.min(100)))],
        ))
        .await?;
    Ok(rows.rows.iter().map(connection_from).collect())
}

/// What a connect did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectOutcome {
    Connected,
    /// Another workspace holds this server connected right now.
    BoundElsewhere,
}

/// Records a server for `workspace_id`, connected by `member_id`. A server
/// that another workspace still holds connected is refused; one that was
/// disconnected is taken over, and its channels move with it.
///
/// # Errors
///
/// A storage failure.
pub async fn connect(
    db: &dyn Database,
    now: time::OffsetDateTime,
    workspace_id: &str,
    guild_id: &str,
    guild_name: &str,
    member_id: &str,
) -> Result<ConnectOutcome, StoreError> {
    let now = unix(now);
    if let Some(existing) = connection(db, guild_id).await?
        && existing.disconnected_at.is_none()
        && existing.workspace_id != workspace_id
    {
        return Ok(ConnectOutcome::BoundElsewhere);
    }
    let moved = connection(db, guild_id)
        .await?
        .is_some_and(|row| row.workspace_id != workspace_id);
    db.execute(&Statement::with_values(
        "INSERT INTO discord_connections \
           (guild_id, workspace_id, guild_name, connected_by, connected_at, disconnected_at, \
            channels_synced_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, NULL, NULL, ?) \
         ON CONFLICT (guild_id) DO UPDATE SET \
           workspace_id = excluded.workspace_id, guild_name = excluded.guild_name, \
           connected_by = excluded.connected_by, connected_at = excluded.connected_at, \
           disconnected_at = NULL, channels_synced_at = NULL, updated_at = excluded.updated_at",
        vec![
            text(guild_id),
            text(workspace_id),
            text(guild_name),
            opt_text(Some(member_id)),
            int(now),
            int(now),
        ],
    ))
    .await?;
    if moved {
        for table in ["discord_channels", "discord_days"] {
            db.execute(&Statement::with_values(
                format!("UPDATE {table} SET workspace_id = ? WHERE guild_id = ?"),
                vec![text(workspace_id), text(guild_id)],
            ))
            .await?;
        }
    }
    Ok(ConnectOutcome::Connected)
}

/// Stops a server: its channels stop reading at once (their leases are
/// dropped, so a run in the middle of one can save nothing more). The sources
/// already filed stay. Answers whether a connected server was found.
///
/// # Errors
///
/// A storage failure.
pub async fn disconnect(
    db: &dyn Database,
    now: time::OffsetDateTime,
    workspace_id: &str,
    guild_id: &str,
) -> Result<bool, StoreError> {
    let now = unix(now);
    let done = db
        .execute(&Statement::with_values(
            "UPDATE discord_connections SET disconnected_at = ?, updated_at = ? \
             WHERE guild_id = ? AND workspace_id = ? AND disconnected_at IS NULL",
            vec![int(now), int(now), text(guild_id), text(workspace_id)],
        ))
        .await?
        == 1;
    db.execute(&Statement::with_values(
        "UPDATE discord_channels SET lease_token = NULL, lease_until = NULL WHERE guild_id = ?",
        vec![text(guild_id)],
    ))
    .await?;
    Ok(done)
}

/// A readable channel or thread of a server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Channel {
    pub guild_id: String,
    pub channel_id: String,
    pub name: String,
    pub kind: i64,
    pub nsfw: bool,
    pub parent_id: Option<String>,
    pub included: bool,
    pub present: bool,
    pub backfill_state: String,
    pub oldest_id: Option<String>,
    pub newest_id: Option<String>,
    pub messages: i64,
    pub archive_state: String,
    pub archive_before: Option<String>,
    pub lease_token: Option<String>,
    pub lease_until: Option<i64>,
    pub polled_at: Option<i64>,
    pub retry_at: Option<i64>,
    pub last_error: Option<String>,
    pub last_error_at: Option<i64>,
}

const CHANNEL_COLUMNS: &str = "guild_id, channel_id, name, kind, nsfw, parent_id, included, present, \
     backfill_state, oldest_id, newest_id, messages, archive_state, archive_before, lease_token, \
     lease_until, polled_at, retry_at, last_error, last_error_at";

fn channel_from(row: &cratefield_core::Row) -> Channel {
    Channel {
        guild_id: get_text(row, "guild_id"),
        channel_id: get_text(row, "channel_id"),
        name: get_text(row, "name"),
        kind: get_int(row, "kind"),
        nsfw: get_int(row, "nsfw") != 0,
        parent_id: get_opt_text(row, "parent_id"),
        included: get_int(row, "included") != 0,
        present: get_int(row, "present") != 0,
        backfill_state: get_text(row, "backfill_state"),
        oldest_id: get_opt_text(row, "oldest_id"),
        newest_id: get_opt_text(row, "newest_id"),
        messages: get_int(row, "messages"),
        archive_state: get_text(row, "archive_state"),
        archive_before: get_opt_text(row, "archive_before"),
        lease_token: get_opt_text(row, "lease_token"),
        lease_until: get_opt_int(row, "lease_until"),
        polled_at: get_opt_int(row, "polled_at"),
        retry_at: get_opt_int(row, "retry_at"),
        last_error: get_opt_text(row, "last_error"),
        last_error_at: get_opt_int(row, "last_error_at"),
    }
}

/// One channel of a server.
///
/// # Errors
///
/// A storage failure.
pub async fn channel(
    db: &dyn Database,
    guild_id: &str,
    channel_id: &str,
) -> Result<Option<Channel>, StoreError> {
    let rows = db
        .query(&Statement::with_values(
            format!("SELECT {CHANNEL_COLUMNS} FROM discord_channels WHERE guild_id = ? AND channel_id = ?"),
            vec![text(guild_id), text(channel_id)],
        ))
        .await?;
    Ok(rows.rows.first().map(channel_from))
}

/// Every channel and thread of a server, the present ones first.
///
/// # Errors
///
/// A storage failure.
pub async fn channels_of(db: &dyn Database, guild_id: &str) -> Result<Vec<Channel>, StoreError> {
    let rows = db
        .query(&Statement::with_values(
            format!(
                "SELECT {CHANNEL_COLUMNS} FROM discord_channels WHERE guild_id = ? \
                 ORDER BY present DESC, name, channel_id"
            ),
            vec![text(guild_id)],
        ))
        .await?;
    Ok(rows.rows.iter().map(channel_from).collect())
}

/// Adds or refreshes one channel from Discord's listing. A new row takes
/// `included`; an existing row keeps the admin's choice and its cursors.
///
/// # Errors
///
/// A storage failure.
#[allow(clippy::too_many_arguments)]
pub async fn upsert_channel(
    db: &dyn Database,
    now: time::OffsetDateTime,
    workspace_id: &str,
    guild_id: &str,
    channel_id: &str,
    name: &str,
    kind: i64,
    nsfw: bool,
    parent_id: Option<&str>,
    included: bool,
) -> Result<(), StoreError> {
    let now = unix(now);
    db.execute(&Statement::with_values(
        "INSERT INTO discord_channels \
           (guild_id, channel_id, workspace_id, name, kind, nsfw, parent_id, included, present, \
            backfill_state, messages, archive_state, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, 1, 'pending', 0, 'pending', ?) \
         ON CONFLICT (guild_id, channel_id) DO UPDATE SET \
           workspace_id = excluded.workspace_id, name = excluded.name, kind = excluded.kind, \
           nsfw = excluded.nsfw, parent_id = excluded.parent_id, present = 1, \
           updated_at = excluded.updated_at",
        vec![
            text(guild_id),
            text(channel_id),
            text(workspace_id),
            text(name),
            int(kind),
            int(i64::from(nsfw)),
            opt_text(parent_id),
            int(i64::from(included)),
            int(now),
        ],
    ))
    .await?;
    Ok(())
}

/// Marks the server's non-thread channels that Discord no longer listed as
/// gone, so the drain stops reading them. Their cursors are kept. Threads are
/// left alone: archived threads are missing from the active listing by design,
/// and a deleted one is found by its own `404` (see [`remove_channel`]).
///
/// # Errors
///
/// A storage failure.
pub async fn mark_missing(
    db: &dyn Database,
    now: time::OffsetDateTime,
    guild_id: &str,
    listed: &[String],
) -> Result<(), StoreError> {
    let now = unix(now);
    // The server's present, non-thread channels that Discord did not list.
    // One read of the server's channels, then the update names only the
    // missing ones, in groups: a server can have more channels than one
    // statement may bind, and `NOT IN (every listed id)` cannot be split
    // into statements that each keep their own list.
    let present = db
        .query(&Statement::with_values(
            "SELECT channel_id FROM discord_channels \
             WHERE guild_id = ? AND present = 1 AND kind NOT IN (10, 11, 12)",
            vec![text(guild_id)],
        ))
        .await?;
    let listed: BTreeSet<&str> = listed.iter().map(String::as_str).collect();
    let missing: Vec<String> = present
        .rows
        .iter()
        .filter_map(|row| row.get::<String>("channel_id"))
        .filter(|id| !listed.contains(id.as_str()))
        .collect();
    let statements: Vec<Statement> = distinct_chunks(&missing, BIND_BUDGET - 2)
        .iter()
        .map(|group| {
            let mut values = vec![int(now), text(guild_id)];
            values.extend(group.iter().map(|id| text(id)));
            Statement::with_values(
                format!(
                    "UPDATE discord_channels SET present = 0, updated_at = ? \
                     WHERE guild_id = ? AND present = 1 AND kind NOT IN (10, 11, 12) \
                       AND channel_id IN ({})",
                    placeholders(group.len())
                ),
                values,
            )
        })
        .collect();
    if !statements.is_empty() {
        db.batch_atomic(&statements).await?;
    }
    Ok(())
}

/// Stamps the server's channel list as read now.
///
/// # Errors
///
/// A storage failure.
pub async fn set_channels_synced(
    db: &dyn Database,
    now: time::OffsetDateTime,
    guild_id: &str,
) -> Result<(), StoreError> {
    let now = unix(now);
    db.execute(&Statement::with_values(
        "UPDATE discord_connections SET channels_synced_at = ?, updated_at = ? WHERE guild_id = ?",
        vec![int(now), int(now), text(guild_id)],
    ))
    .await?;
    Ok(())
}

/// Sets whether a channel is imported. Answers whether the channel was found
/// in the workspace's server.
///
/// # Errors
///
/// A storage failure.
pub async fn set_included(
    db: &dyn Database,
    now: time::OffsetDateTime,
    workspace_id: &str,
    guild_id: &str,
    channel_id: &str,
    included: bool,
) -> Result<bool, StoreError> {
    Ok(db
        .execute(&Statement::with_values(
            "UPDATE discord_channels SET included = ?, updated_at = ? \
             WHERE guild_id = ? AND channel_id = ? AND workspace_id = ? AND present = 1",
            vec![
                int(i64::from(included)),
                int(unix(now)),
                text(guild_id),
                text(channel_id),
                text(workspace_id),
            ],
        ))
        .await?
        == 1)
}

/// Takes a channel's lease for one run. Answers whether it was taken: the
/// channel is included, present, not refused for a while, and not leased.
///
/// # Errors
///
/// A storage failure.
pub async fn claim(
    db: &dyn Database,
    now: time::OffsetDateTime,
    guild_id: &str,
    channel_id: &str,
    token: &str,
    lease_secs: i64,
) -> Result<bool, StoreError> {
    let now = unix(now);
    Ok(db
        .execute(&Statement::with_values(
            "UPDATE discord_channels SET lease_token = ?, lease_until = ? \
             WHERE guild_id = ? AND channel_id = ? AND included = 1 AND present = 1 \
               AND (lease_until IS NULL OR lease_until < ?) \
               AND (retry_at IS NULL OR retry_at <= ?)",
            vec![
                text(token),
                int(now + lease_secs),
                text(guild_id),
                text(channel_id),
                int(now),
                int(now),
            ],
        ))
        .await?
        == 1)
}

/// Gives a lease back.
///
/// # Errors
///
/// A storage failure.
pub async fn release(
    db: &dyn Database,
    guild_id: &str,
    channel_id: &str,
    token: &str,
) -> Result<(), StoreError> {
    db.execute(&Statement::with_values(
        "UPDATE discord_channels SET lease_token = NULL, lease_until = NULL \
         WHERE guild_id = ? AND channel_id = ? AND lease_token = ?",
        vec![text(guild_id), text(channel_id), text(token)],
    ))
    .await?;
    Ok(())
}

/// One step of a channel's progress, written under its lease. `None` keeps a
/// cursor as it is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Advance<'a> {
    pub backfill_state: Option<&'a str>,
    pub oldest_id: Option<&'a str>,
    pub newest_id: Option<&'a str>,
    /// Messages this step read.
    pub added: i64,
    /// Stamps the forward poll as done now.
    pub polled: bool,
}

/// Saves a step. Lands only while `token` still holds the channel's lease, so
/// a run whose lease was taken (or dropped by a disconnect) saves nothing.
/// Answers whether it landed.
///
/// # Errors
///
/// A storage failure.
pub async fn advance(
    db: &dyn Database,
    now: time::OffsetDateTime,
    token: &str,
    guild_id: &str,
    channel_id: &str,
    step: &Advance<'_>,
) -> Result<bool, StoreError> {
    let now = unix(now);
    Ok(db
        .execute(&Statement::with_values(
            "UPDATE discord_channels SET \
               backfill_state = COALESCE(?, backfill_state), \
               oldest_id = COALESCE(?, oldest_id), \
               newest_id = COALESCE(?, newest_id), \
               messages = messages + ?, \
               polled_at = CASE WHEN ? = 1 THEN ? ELSE polled_at END, \
               last_error = NULL, last_error_at = NULL, retry_at = NULL, updated_at = ? \
             WHERE guild_id = ? AND channel_id = ? AND lease_token = ?",
            vec![
                opt_text(step.backfill_state),
                opt_text(step.oldest_id),
                opt_text(step.newest_id),
                int(step.added),
                int(i64::from(step.polled)),
                int(now),
                int(now),
                text(guild_id),
                text(channel_id),
                text(token),
            ],
        ))
        .await?
        == 1)
}

/// Records why a channel's read failed and when to try it again. The text is
/// a status and a fixed phrase, never a body.
///
/// # Errors
///
/// A storage failure.
pub async fn record_error(
    db: &dyn Database,
    now: time::OffsetDateTime,
    guild_id: &str,
    channel_id: &str,
    error: &str,
    retry_at: Option<i64>,
) -> Result<(), StoreError> {
    let now = unix(now);
    db.execute(&Statement::with_values(
        "UPDATE discord_channels SET last_error = ?, last_error_at = ?, retry_at = ?, updated_at = ? \
         WHERE guild_id = ? AND channel_id = ?",
        vec![
            text(&crate::common::bounded(error, 200)),
            int(now),
            opt_int(retry_at),
            int(now),
            text(guild_id),
            text(channel_id),
        ],
    ))
    .await?;
    Ok(())
}

/// A text channel whose archived threads are still to be listed.
///
/// # Errors
///
/// A storage failure.
pub async fn channels_to_archive(
    db: &dyn Database,
    guild_id: &str,
) -> Result<Vec<Channel>, StoreError> {
    let rows = db
        .query(&Statement::with_values(
            format!(
                "SELECT {CHANNEL_COLUMNS} FROM discord_channels \
                 WHERE guild_id = ? AND archive_state = 'pending' AND present = 1 \
                   AND included = 1 AND kind IN (0, 5) ORDER BY name, channel_id LIMIT 20"
            ),
            vec![text(guild_id)],
        ))
        .await?;
    Ok(rows.rows.iter().map(channel_from).collect())
}

/// Saves where an archived-thread listing reached.
///
/// # Errors
///
/// A storage failure.
pub async fn set_archive(
    db: &dyn Database,
    guild_id: &str,
    channel_id: &str,
    state: &str,
    before: Option<&str>,
) -> Result<(), StoreError> {
    db.execute(&Statement::with_values(
        "UPDATE discord_channels SET archive_state = ?, archive_before = ? \
         WHERE guild_id = ? AND channel_id = ?",
        vec![
            text(state),
            opt_text(before),
            text(guild_id),
            text(channel_id),
        ],
    ))
    .await?;
    Ok(())
}

/// Marks channel-days dirty: they changed (or were first seen) and must be
/// re-read and filed.
///
/// # Errors
///
/// A storage failure.
pub async fn mark_days_dirty(
    db: &dyn Database,
    now: time::OffsetDateTime,
    workspace_id: &str,
    guild_id: &str,
    channel_id: &str,
    days: &[String],
) -> Result<(), StoreError> {
    let now = unix(now);
    for day in days {
        db.execute(&Statement::with_values(
            "INSERT INTO discord_days (guild_id, channel_id, workspace_id, day, state, updated_at) \
             VALUES (?, ?, ?, ?, 'dirty', ?) \
             ON CONFLICT (guild_id, channel_id, day) DO UPDATE SET \
               state = 'dirty', workspace_id = excluded.workspace_id, updated_at = excluded.updated_at",
            vec![
                text(guild_id),
                text(channel_id),
                text(workspace_id),
                text(day),
                int(now),
            ],
        ))
        .await?;
    }
    Ok(())
}

/// A channel-day waiting to be filed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Day {
    pub workspace_id: String,
    pub guild_id: String,
    pub channel_id: String,
    pub day: String,
    /// The instant it was last marked dirty; a filing that started before it
    /// must not mark the day filed.
    pub updated_at: i64,
}

/// Dirty days of connected servers whose channel is included, oldest first.
/// A day is due when it was never filed, when it was last filed before
/// `refile_before` (so a busy day is refiled at most once in that window), or
/// when it is a closed day (before `today`), which is filed at once.
///
/// # Errors
///
/// A storage failure.
pub async fn dirty_days(
    db: &dyn Database,
    now: time::OffsetDateTime,
    refile_before: time::OffsetDateTime,
    today: &str,
    limit: u32,
) -> Result<Vec<Day>, StoreError> {
    let now = unix(now);
    let rows = db
        .query(&Statement::with_values(
            "SELECT d.workspace_id, d.guild_id, d.channel_id, d.day, d.updated_at FROM discord_days d \
               JOIN discord_connections k ON k.guild_id = d.guild_id AND k.disconnected_at IS NULL \
               JOIN discord_channels c ON c.guild_id = d.guild_id AND c.channel_id = d.channel_id \
                 AND c.included = 1 AND c.present = 1 \
                 AND (c.retry_at IS NULL OR c.retry_at <= ?) \
             WHERE d.state = 'dirty' AND (d.filed_at IS NULL OR d.filed_at <= ? OR d.day < ?) \
             ORDER BY d.updated_at, d.guild_id, d.channel_id, d.day LIMIT ?",
            vec![
                int(now),
                int(unix(refile_before)),
                text(today),
                int(i64::from(limit.min(100))),
            ],
        ))
        .await?;
    Ok(rows
        .rows
        .iter()
        .map(|row| Day {
            workspace_id: get_text(row, "workspace_id"),
            guild_id: get_text(row, "guild_id"),
            channel_id: get_text(row, "channel_id"),
            day: get_text(row, "day"),
            updated_at: get_int(row, "updated_at"),
        })
        .collect())
}

/// Marks a day filed, unless it went dirty again after `filing_started`.
/// Answers whether it was marked.
///
/// # Errors
///
/// A storage failure.
pub async fn mark_day_filed(
    db: &dyn Database,
    now: time::OffsetDateTime,
    day: &Day,
) -> Result<bool, StoreError> {
    let now = unix(now);
    Ok(db
        .execute(&Statement::with_values(
            "UPDATE discord_days SET state = 'filed', filed_at = ? \
             WHERE guild_id = ? AND channel_id = ? AND day = ? AND state = 'dirty' AND updated_at <= ?",
            vec![
                int(now),
                text(&day.guild_id),
                text(&day.channel_id),
                text(&day.day),
                int(day.updated_at),
            ],
        ))
        .await?
        == 1)
}

/// How many channel-days are waiting, and how many have been filed, for one
/// workspace's server.
///
/// # Errors
///
/// A storage failure.
pub async fn day_counts(db: &dyn Database, guild_id: &str) -> Result<(i64, i64), StoreError> {
    let rows = db
        .query(&Statement::with_values(
            "SELECT SUM(CASE WHEN state = 'dirty' THEN 1 ELSE 0 END) AS dirty, \
                    SUM(CASE WHEN state = 'filed' THEN 1 ELSE 0 END) AS filed \
             FROM discord_days WHERE guild_id = ?",
            vec![text(guild_id)],
        ))
        .await?;
    let row = rows.rows.first();
    Ok((
        row.map_or(0, |row| get_int(row, "dirty")),
        row.map_or(0, |row| get_int(row, "filed")),
    ))
}

/// Marks one channel gone (Discord answered `404`), with the reason.
///
/// # Errors
///
/// A storage failure.
pub async fn remove_channel(
    db: &dyn Database,
    now: time::OffsetDateTime,
    guild_id: &str,
    channel_id: &str,
) -> Result<(), StoreError> {
    let now = unix(now);
    db.execute(&Statement::with_values(
        "UPDATE discord_channels SET present = 0, last_error = ?, last_error_at = ?, updated_at = ? \
         WHERE guild_id = ? AND channel_id = ?",
        vec![
            text("the channel is gone from Discord"),
            int(now),
            int(now),
            text(guild_id),
            text(channel_id),
        ],
    ))
    .await?;
    Ok(())
}

/// Records why a server's listing failed. Cleared by the next good listing.
///
/// # Errors
///
/// A storage failure.
pub async fn record_connection_error(
    db: &dyn Database,
    now: time::OffsetDateTime,
    guild_id: &str,
    error: Option<&str>,
) -> Result<(), StoreError> {
    let now = unix(now);
    db.execute(&Statement::with_values(
        "UPDATE discord_connections SET last_error = ?, last_error_at = ?, updated_at = ? \
         WHERE guild_id = ?",
        vec![
            opt_text(error),
            opt_int(error.map(|_| now)),
            int(now),
            text(guild_id),
        ],
    ))
    .await?;
    Ok(())
}

#[cfg(test)]
mod d1_binds {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use cratefield_core::{DbError, Row, Rows};
    use livingbrain_d1_fake::BindLimited;
    use sea_query::Value as SeaValue;

    use super::*;

    /// A server's present channels, and every value each batch statement
    /// binds. Every other read and write is refused as not expected here.
    #[derive(Default)]
    struct Channels {
        present: Vec<String>,
        bound: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl Database for Channels {
        async fn execute(&self, _stmt: &Statement) -> Result<u64, DbError> {
            Err(DbError::Execute("not expected".to_owned()))
        }

        async fn query(&self, _stmt: &Statement) -> Result<Rows, DbError> {
            Ok(Rows::new(
                self.present
                    .iter()
                    .map(|id| Row::new(vec![("channel_id".to_owned(), text(id))]))
                    .collect(),
            ))
        }

        async fn batch_atomic(&self, stmts: &[Statement]) -> Result<(), DbError> {
            let mut bound = self.bound.lock().expect("the recorder");
            for stmt in stmts {
                bound.extend(stmt.values.0.iter().filter_map(|value| match value {
                    SeaValue::String(Some(text)) => Some((**text).clone()),
                    _ => None,
                }));
            }
            Ok(())
        }
    }

    #[test]
    fn a_server_with_more_channels_than_one_statement_may_bind_is_marked_in_groups() {
        let name = |i: usize| format!("chan-{i:03}");
        // 250 channels present; Discord lists four in five of them, so 50 are
        // gone, and the gone set alone is over one statement's budget only
        // when it is split.
        let present: Vec<String> = (0..250).map(name).collect();
        let listed: Vec<String> = (0..250).filter(|i| i % 5 != 0).map(name).collect();
        let recorder = Arc::new(Channels {
            present,
            ..Channels::default()
        });
        let db = BindLimited::new(recorder.clone());
        pollster::block_on(mark_missing(
            &*db,
            time::OffsetDateTime::UNIX_EPOCH,
            "guild-1",
            &listed,
        ))
        .expect("the marking stays within D1's bind ceiling");
        assert!(db.widest() <= 100, "widest statement bound {}", db.widest());

        let mut marked: Vec<String> = recorder
            .bound
            .lock()
            .expect("the recorder")
            .iter()
            .filter(|value| value.starts_with("chan-"))
            .cloned()
            .collect();
        marked.sort();
        let gone: Vec<String> = (0..250).filter(|i| i % 5 == 0).map(name).collect();
        assert_eq!(marked, gone);
    }
}
