//! The body of the cron drain: the only place GitHub history is read and
//! filed into the ledger.
//!
//! Reached only from `drain::run` (the module's `scheduled`), with the
//! [`Drain`] that run minted; every progress write and queue claim below
//! takes it. No route, CLI command or agent reaches this file.
//!
//! # One tick
//!
//! 1. **Budget.** The tick may make `min(planner::DEFAULT_BUDGET,
//!    ctx.scheduled.remaining)` GitHub calls, and stops at the scheduled
//!    deadline. Every call — token mint included — spends one step first.
//! 2. **Stale reopen.** A repository done for more than a day goes back to
//!    `running`: its stored cursor is the planner's `next_run`, so this is
//!    an incremental pass that catches anything a lost webhook missed.
//! 3. **Queue** (at most half the tick when repositories are due): `item`
//!    re-reads one issue or pull request with its comments and reviews and
//!    files it; `push` turns a finished repository's incremental pass on
//!    (or flags a running one to run again when it finishes).
//! 4. **Backfill.** The due repositories share what is left, oldest
//!    `updated_at` first (every save moves a repository to the back), each
//!    getting `ceil(left / repositories left)` calls, at least 4.
//!
//! # A repository's pass
//!
//! The planner (`livingbrain_github::planner`) decides the order: the commit
//! listing, the issue listing, then each queued item's comments and
//! reviews, then each commit's detail. A listing is held back while its
//! queue has [`LIST_THROTTLE`] entries, so the stored cursor stays small.
//!
//! An item is fetched as a **unit**: the issue (unless the listing just
//! returned it), every page of its comments (up to 10) and, for a pull
//! request, of its reviews (up to 3), then mapped and filed — all in one
//! tick, so nothing GitHub said is ever kept between ticks except the
//! cursor. A unit that runs out of budget is dropped and retried next tick;
//! when it is the first work of its tick it is filed with what was read
//! instead, so a very long thread cannot stall a repository forever.
//!
//! Listings and commit details go through `planner::apply`; a finished
//! unit, or an item or commit GitHub no longer has (404/410/422), is taken
//! off the queue directly.
//!
//! # Limits and failures
//!
//! `rate::decide` reads every answer. A wait (403/429 with limit headers,
//! `retry-after`, a low quota, a 5xx, a network failure) stores
//! `wait_until` in the cursor and stops every repository of that
//! installation for the tick. A failure (another status, an unexpected
//! body) is recorded on the repository (`last_error`: a status code and a
//! fixed text, never a header, token or body) and backs the repository off
//! exponentially, 5 minutes doubling to 6 hours. A queue item fails into
//! the store's retry, and is parked dead after its fifth attempt.
//!
//! # Filing
//!
//! Every record goes through `livingbrain_pages::Ingestor` — normalise,
//! redact, dedupe on `(scope, sha256)`, store sealed — as `github_commit`,
//! `github_issue` or `github_pr`, in the repository's scope (`shared` →
//! the workspace's shared page scope; `user:<id>` → that member's), under
//! the repository's project, with `origin_ref` the GitHub permalink and
//! `imported_by` the member who bound the installation. The same GitHub
//! state yields the same bytes, so a re-read files nothing new.

use std::collections::HashMap;
use std::str::FromStr as _;
use std::sync::Arc;

use cratefield_core::axum::http::Method;
use cratefield_core::{Clock, Database, HttpClient, ModuleContext, ScheduledBudget};
use cratefield_kms::Kms;
use livingbrain_access::Scope;
use livingbrain_github::api::{CommitDetail, CommitSummary, IssueComment, Item, PullReview};
use livingbrain_github::planner::{self, BackfillState, Request, Response};
use livingbrain_github::rate::{self, Backoff};
use livingbrain_github::record::{
    self, GithubKind, ItemKind, MapOptions, RepoRef as GhRepo, SourceParts,
};
use livingbrain_pages::{IngestError, Ingestor, SourceIngest, SourceKind, SourceStore};
use serde::{Deserialize, Serialize};

use crate::common::{StoreError, unix};
use crate::drain::{self, BackfillProgress, Claimed, Drain};
use crate::github::{self, Auth, GhResponse, TokenCache};
use crate::seal::Secret;
use crate::{AppCredentials, BackfillPhase, Repo, Shared};

/// A listing waits while its queue holds this many entries.
pub(crate) const LIST_THROTTLE: usize = 200;
/// Comment pages read for one item (100 each).
pub(crate) const MAX_COMMENT_PAGES: u32 = 10;
/// Review pages read for one pull request (100 each).
pub(crate) const MAX_REVIEW_PAGES: u32 = 3;
/// The smallest share of a tick a due repository is given.
const MIN_SHARE: u32 = 4;
/// How long a claimed queue item is held.
const QUEUE_LEASE_SECS: i64 = 5 * 60;
/// A finished repository's incremental pass reopens after this long.
pub(crate) const STALE_AFTER_SECS: i64 = 24 * 60 * 60;
/// The first failure backoff; it doubles per consecutive failure.
const FAIL_BACKOFF_SECS: i64 = 5 * 60;
/// The longest failure backoff.
const MAX_FAIL_BACKOFF_SECS: i64 = 6 * 60 * 60;
/// Who files a record when the binding member was erased.
pub(crate) const SYSTEM_IMPORTER: &str = "brain:github";

/// The drain's cursor for one repository: the planner state and the few
/// facts the drain itself needs between ticks. Stored as the opaque
/// `backfill_cursor` JSON. Holds no GitHub content: shas, numbers, page
/// numbers, timestamps and ETags.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DrainCursor {
    #[serde(default)]
    pub v: u32,
    #[serde(default)]
    pub state: BackfillState,
    /// A push arrived while this pass ran: start the next incremental pass
    /// as soon as this one finishes.
    #[serde(default)]
    pub rerun: bool,
    /// No call for this repository before this unix second.
    #[serde(default)]
    pub wait_until: Option<i64>,
    /// Consecutive failed passes, for the backoff.
    #[serde(default)]
    pub failures: u32,
    /// `ETag`s of the first listing pages, for `If-None-Match` on the next
    /// incremental pass (a 304 costs no quota).
    #[serde(default)]
    pub etag_commits: Option<String>,
    #[serde(default)]
    pub etag_issues: Option<String>,
}

const CURSOR_VERSION: u32 = 1;

impl DrainCursor {
    pub(crate) fn parse(raw: Option<&str>) -> Self {
        raw.and_then(|raw| serde_json::from_str(raw).ok())
            .unwrap_or_default()
    }

    fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_owned())
    }
}

/// When the repository may next be read, for the settings page.
pub(crate) fn waiting_until(cursor: Option<&str>) -> Option<i64> {
    DrainCursor::parse(cursor).wait_until
}

/// The tick. See the module docs.
pub(crate) async fn run(
    ctx: &ModuleContext,
    drain: &Drain,
    shared: &Shared,
) -> Result<(), StoreError> {
    let Some(kms) = shared.kms.clone() else {
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
    let cap = budget
        .remaining_subrequests()
        .unwrap_or(planner::DEFAULT_BUDGET)
        .min(planner::DEFAULT_BUDGET);
    let mut tick = Tick {
        db: &*db,
        http: &*http,
        clock: &*clock,
        kms: &*kms,
        budget: &budget,
        start,
        cap,
        tokens: &shared.tokens,
        ingestor: &ingestor,
        drain,
        creds: HashMap::new(),
        importers: HashMap::new(),
        waits: HashMap::new(),
    };

    drain::reopen_stale(&*db, drain, clock.now(), STALE_AFTER_SECS).await?;
    let now = unix(clock.now());
    let due: Vec<Repo> = drain::repos_due(&*db, drain, 100)
        .await?
        .into_iter()
        .filter(|repo| {
            DrainCursor::parse(repo.backfill_cursor.as_deref())
                .wait_until
                .is_none_or(|until| until <= now)
        })
        .collect();

    // The queue first, with at most half the tick when backfills wait.
    let queue_cap = if due.is_empty() { cap } else { cap / 2 };
    while tick.used() < queue_cap && tick.left() >= UNIT_RESERVE {
        let mut claimed = drain::claim(&*db, drain, clock.now(), 1, QUEUE_LEASE_SECS).await?;
        let Some(item) = claimed.pop() else {
            break;
        };
        match tick.work_item(&item).await? {
            ItemOutcome::Done => {
                drain::complete(&*db, drain, &item).await?;
            }
            ItemOutcome::Retry { after, error } => {
                drain::fail(&*db, drain, clock.now(), &item, &error, after).await?;
            }
        }
    }

    let count = due.len();
    for (index, repo) in due.iter().enumerate() {
        let left = tick.left();
        if left < 2 {
            break;
        }
        let remaining_repos = u32::try_from(count - index).unwrap_or(u32::MAX).max(1);
        let share = left.div_ceil(remaining_repos).max(MIN_SHARE).min(left);
        tick.backfill(repo, share).await?;
    }
    Ok(())
}

/// The budget one queued `item` is not started below: the issue, a page of
/// comments, a page of reviews, and the token.
const UNIT_RESERVE: u32 = 4;

enum ItemOutcome {
    Done,
    Retry { after: i64, error: String },
}

/// What a GET came back as.
enum Got {
    Ok(GhResponse),
    NotModified,
    /// 404, 410 or 422: the thing is not there (any more).
    Gone,
    /// 409 (an empty repository's commit listing).
    Empty,
    Wait(u64),
    Fail(String),
    NoBudget,
}

/// Why a repository's pass stopped.
enum Stop {
    Done,
    Budget,
    Wait(u64),
    Fail(String),
    /// A ledger write failed: nothing from this tick is saved, so the next
    /// tick re-reads it (and dedupe absorbs what did land).
    Abort(String),
}

/// One fetched issue or pull request with its activity.
enum Unit {
    Read {
        item: Box<Item>,
        comments: Vec<IssueComment>,
        reviews: Vec<PullReview>,
    },
    Gone,
    NoBudget,
    Wait(u64),
    Fail(String),
}

/// What a repository's records are filed as.
struct Filing {
    workspace: String,
    scope: String,
    project: String,
    imported_by: String,
}

struct Tick<'a> {
    db: &'a dyn Database,
    http: &'a dyn HttpClient,
    clock: &'a dyn Clock,
    kms: &'a dyn Kms,
    budget: &'a ScheduledBudget,
    start: u32,
    cap: u32,
    tokens: &'a TokenCache,
    ingestor: &'a Ingestor,
    drain: &'a Drain,
    /// Per workspace: its unsealed app credentials, `None` when it has no
    /// app or they would not open. Dropped (and wiped) with the tick.
    creds: HashMap<String, Option<Arc<AppCredentials>>>,
    /// Per installation: the member who bound it.
    importers: HashMap<i64, String>,
    /// Per installation: no call before this unix second (this tick).
    waits: HashMap<i64, i64>,
}

impl Tick<'_> {
    fn now(&self) -> i64 {
        unix(self.clock.now())
    }

    /// Calls made this tick.
    fn used(&self) -> u32 {
        self.budget.spent().saturating_sub(self.start)
    }

    /// Calls this tick may still make.
    fn left(&self) -> u32 {
        if self.budget.expired(self.clock.now()) {
            return 0;
        }
        let by_cap = self.cap.saturating_sub(self.used());
        self.budget
            .remaining_subrequests()
            .map_or(by_cap, |remaining| remaining.min(by_cap))
    }

    /// Spends one step for one call, or answers `false`.
    fn spend(&self) -> bool {
        self.left() > 0 && self.budget.try_spend(1)
    }

    fn waiting(&self, installation_id: i64) -> bool {
        let now = self.now();
        self.waits
            .get(&installation_id)
            .is_some_and(|until| *until > now)
    }

    fn hold(&mut self, installation_id: i64, seconds: u64) {
        let until = self.now() + i64::try_from(seconds).unwrap_or(i64::MAX / 4);
        let slot = self.waits.entry(installation_id).or_insert(until);
        *slot = (*slot).max(until);
    }

    async fn credentials(
        &mut self,
        workspace_id: &str,
    ) -> Result<Option<Arc<AppCredentials>>, StoreError> {
        if let Some(found) = self.creds.get(workspace_id) {
            return Ok(found.clone());
        }
        let opened = match crate::app_credentials(self.kms, self.db, workspace_id).await {
            Ok(creds) => creds.map(Arc::new),
            Err(StoreError::Kms(_)) => None,
            Err(other) => return Err(other),
        };
        self.creds.insert(workspace_id.to_owned(), opened.clone());
        Ok(opened)
    }

    async fn importer(
        &mut self,
        workspace_id: &str,
        installation_id: i64,
    ) -> Result<String, StoreError> {
        if let Some(found) = self.importers.get(&installation_id) {
            return Ok(found.clone());
        }
        let member = crate::list_installations(self.db, workspace_id)
            .await?
            .into_iter()
            .find(|i| i.installation_id == installation_id)
            .and_then(|i| i.bound_by)
            .unwrap_or_else(|| SYSTEM_IMPORTER.to_owned());
        self.importers.insert(installation_id, member.clone());
        Ok(member)
    }

    /// The installation token, minting one (one call) when none is cached.
    async fn token(&mut self, creds: &AppCredentials, installation_id: i64) -> Result<Secret, Got> {
        let now = self.now();
        // A cached token costs nothing; only a mint spends a step.
        if let Some(token) = self.tokens.cached(creds.app_id, installation_id, now) {
            return Ok(token);
        }
        if !self.spend() {
            return Err(Got::NoBudget);
        }
        match github::installation_token(
            self.http,
            self.tokens,
            &creds.private_key_pem,
            creds.app_id,
            installation_id,
            now,
        )
        .await
        {
            Ok(token) => Ok(token),
            Err(github::GhError::Status(status)) => {
                let backoff = rate::decide(status, rate::RateLimit::default(), 0);
                Err(match backoff {
                    Backoff::Wait { seconds } => Got::Wait(seconds),
                    _ => Got::Fail(format!("GitHub refused an installation token ({status})")),
                })
            }
            Err(github::GhError::Transport) => Err(Got::Wait(rate::SERVER_ERROR_SECS)),
            Err(other) => Err(Got::Fail(other.to_string())),
        }
    }

    /// One GET with the installation token, its answer classified.
    async fn get(
        &mut self,
        installation_id: i64,
        app_id: i64,
        token: &Secret,
        path: &str,
        etag: Option<&str>,
    ) -> Got {
        if !self.spend() {
            return Got::NoBudget;
        }
        let response = match github::call(
            self.http,
            Method::GET,
            path,
            Auth::Bearer(token.expose()),
            etag,
        )
        .await
        {
            Ok(response) => response,
            Err(_) => {
                self.hold(installation_id, rate::SERVER_ERROR_SECS);
                return Got::Wait(rate::SERVER_ERROR_SECS);
            }
        };
        let now = u64::try_from(self.now()).unwrap_or_default();
        let decision = rate::decide(response.status, response.rate(), now);
        if let Backoff::Wait { seconds } = decision {
            self.hold(installation_id, seconds);
            // A good answer that leaves the quota low is still used; the
            // installation rests afterwards.
            if !response.ok() {
                return Got::Wait(seconds);
            }
        }
        match response.status {
            200..=299 => Got::Ok(response),
            304 => Got::NotModified,
            404 | 410 | 422 => Got::Gone,
            409 => Got::Empty,
            401 => {
                // The token was revoked or expired early: mint anew next time.
                self.tokens.forget(app_id, installation_id);
                Got::Fail("GitHub refused the installation token (401)".to_owned())
            }
            status => Got::Fail(format!("GitHub answered {status}")),
        }
    }

    // -----------------------------------------------------------------------
    // Filing

    async fn file(&self, filing: &Filing, parts: SourceParts) -> Result<(), Stop> {
        let kind = match parts.kind {
            GithubKind::Commit => SourceKind::GithubCommit,
            GithubKind::Issue => SourceKind::GithubIssue,
            GithubKind::PullRequest => SourceKind::GithubPr,
        };
        let source = |project: Option<String>| SourceIngest {
            kind,
            workspace: filing.workspace.clone(),
            scope: filing.scope.clone(),
            origin_ref: Some(parts.origin_ref.clone()),
            author: None,
            rel_path: parts.rel_path.clone(),
            imported_by: filing.imported_by.clone(),
            body: parts.body.clone(),
            project,
        };
        match self
            .ingestor
            .ingest(source(Some(filing.project.clone())))
            .await
        {
            Ok(_) => Ok(()),
            // A project an admin set that the ledger will not take: file it
            // without one rather than not at all.
            Err(IngestError::InvalidProject(_)) => match self.ingestor.ingest(source(None)).await {
                Ok(_) => Ok(()),
                Err(error) => skip_or_stop(&error),
            },
            Err(error) => skip_or_stop(&error),
        }
    }

    // -----------------------------------------------------------------------
    // The queue

    async fn work_item(&mut self, item: &Claimed) -> Result<ItemOutcome, StoreError> {
        let Some(repo_id) = item.repo_id else {
            return Ok(ItemOutcome::Done);
        };
        let Some(repo) =
            crate::get_repo(self.db, &item.workspace_id, item.installation_id, repo_id).await?
        else {
            return Ok(ItemOutcome::Done);
        };
        if !repo.active || !repo.included {
            return Ok(ItemOutcome::Done);
        }
        match item.kind.as_str() {
            "push" => {
                self.turn_on_incremental(&repo).await?;
                Ok(ItemOutcome::Done)
            }
            "item" => {
                let Ok(number) = item.subject.parse::<u64>() else {
                    return Ok(ItemOutcome::Done);
                };
                self.refresh_item(&repo, number, item.attempts).await
            }
            _ => Ok(ItemOutcome::Done),
        }
    }

    /// A push: a finished repository starts its incremental pass; a running
    /// one is flagged to run again when it finishes. No call to GitHub.
    async fn turn_on_incremental(&self, repo: &Repo) -> Result<(), StoreError> {
        let mut cursor = DrainCursor::parse(repo.backfill_cursor.as_deref());
        if repo.backfill_state == BackfillPhase::Pending {
            // The first pass has not started; it will read the push anyway.
            return Ok(());
        }
        if repo.backfill_state != BackfillPhase::Done {
            if cursor.rerun {
                return Ok(());
            }
            cursor.rerun = true;
        }
        cursor.v = CURSOR_VERSION;
        drain::save_backfill_progress(
            self.db,
            self.drain,
            self.clock.now(),
            &repo.workspace_id,
            repo.installation_id,
            repo.repo_id,
            &BackfillProgress {
                cursor: Some(cursor.to_json()),
                items: 0,
                done: false,
            },
        )
        .await?;
        Ok(())
    }

    /// An `item` reference: re-read the issue or pull request and file it.
    async fn refresh_item(
        &mut self,
        repo: &Repo,
        number: u64,
        attempts: i64,
    ) -> Result<ItemOutcome, StoreError> {
        let retry_after = FAIL_BACKOFF_SECS * attempts.clamp(1, 12);
        if self.waiting(repo.installation_id) {
            return Ok(ItemOutcome::Retry {
                after: self.waits[&repo.installation_id] - self.now(),
                error: "waiting for the rate limit".to_owned(),
            });
        }
        let Ok(gh_repo) = GhRepo::parse(&repo.full_name) else {
            return Ok(ItemOutcome::Done);
        };
        let Some(filing) = self.filing(repo).await? else {
            return Ok(ItemOutcome::Done);
        };
        let Some(creds) = self.credentials(&repo.workspace_id).await? else {
            return Ok(ItemOutcome::Done);
        };
        let token = match self.token(&creds, repo.installation_id).await {
            Ok(token) => token,
            Err(Got::Wait(seconds)) => {
                return Ok(ItemOutcome::Retry {
                    after: i64::try_from(seconds).unwrap_or(retry_after),
                    error: "GitHub asked to wait".to_owned(),
                });
            }
            Err(Got::Fail(error)) => {
                return Ok(ItemOutcome::Retry {
                    after: retry_after,
                    error,
                });
            }
            Err(_) => {
                return Ok(ItemOutcome::Retry {
                    after: 0,
                    error: "no budget left this tick".to_owned(),
                });
            }
        };
        let unit = self
            .fetch_unit(
                repo.installation_id,
                creds.app_id,
                &token,
                &gh_repo,
                number,
                None,
                true,
            )
            .await;
        Ok(match unit {
            Unit::Read {
                item,
                comments,
                reviews,
            } => {
                let kind = if item.pull_request.is_some() {
                    ItemKind::PullRequest
                } else {
                    ItemKind::Issue
                };
                let record = record::map_item(&gh_repo, &item, kind, &comments, &reviews);
                match self
                    .file(&filing, record::item_source(&gh_repo, &record))
                    .await
                {
                    Ok(()) | Err(Stop::Done) => ItemOutcome::Done,
                    Err(_) => ItemOutcome::Retry {
                        after: retry_after,
                        error: "the ledger refused the write".to_owned(),
                    },
                }
            }
            Unit::Gone => ItemOutcome::Done,
            Unit::Wait(seconds) => ItemOutcome::Retry {
                after: i64::try_from(seconds).unwrap_or(retry_after),
                error: "GitHub asked to wait".to_owned(),
            },
            Unit::Fail(error) => ItemOutcome::Retry {
                after: retry_after,
                error,
            },
            Unit::NoBudget => ItemOutcome::Retry {
                after: 0,
                error: "no budget left this tick".to_owned(),
            },
        })
    }

    /// Where a repository's records go, or `None` (recorded on the
    /// repository) when its scope is not one a record may land in.
    async fn filing(&mut self, repo: &Repo) -> Result<Option<Filing>, StoreError> {
        let scope = match Scope::from_str(&repo.scope) {
            Ok(scope @ (Scope::Shared | Scope::User(_))) => {
                livingbrain_pages::page_scope(&repo.workspace_id, &scope)
            }
            _ => return Ok(None),
        };
        Ok(Some(Filing {
            workspace: repo.workspace_id.clone(),
            scope,
            project: repo.project.clone(),
            imported_by: self
                .importer(&repo.workspace_id, repo.installation_id)
                .await?,
        }))
    }

    /// Reads one issue or pull request with its comments and reviews. With
    /// `truncate`, running out of budget files what was read instead of
    /// giving up.
    #[allow(clippy::too_many_arguments)]
    async fn fetch_unit(
        &mut self,
        installation_id: i64,
        app_id: i64,
        token: &Secret,
        repo: &GhRepo,
        number: u64,
        listed: Option<Item>,
        truncate: bool,
    ) -> Unit {
        let base = format!("/repos/{}/{}", repo.owner(), repo.name());
        let item = match listed {
            Some(item) => item,
            None => {
                let path = format!("{base}/issues/{number}");
                match self.get(installation_id, app_id, token, &path, None).await {
                    Got::Ok(response) => match response.json::<Item>() {
                        Ok(item) => item,
                        Err(error) => return Unit::Fail(error.to_string()),
                    },
                    Got::Gone | Got::Empty | Got::NotModified => return Unit::Gone,
                    Got::NoBudget => return Unit::NoBudget,
                    Got::Wait(seconds) => return Unit::Wait(seconds),
                    Got::Fail(error) => return Unit::Fail(error),
                }
            }
        };
        let mut comments = Vec::new();
        let mut reviews = Vec::new();
        let lists: [(bool, &str, u32); 2] = [
            (item.comments > 0, "issues", MAX_COMMENT_PAGES),
            (item.pull_request.is_some(), "pulls", MAX_REVIEW_PAGES),
        ];
        'lists: for (index, (needed, segment, max_pages)) in lists.into_iter().enumerate() {
            if !needed {
                continue;
            }
            let leaf = if index == 0 { "comments" } else { "reviews" };
            let mut page = 1;
            while page <= max_pages {
                let path = format!(
                    "{base}/{segment}/{number}/{leaf}?per_page={}&page={page}",
                    planner::PER_PAGE
                );
                match self.get(installation_id, app_id, token, &path, None).await {
                    Got::Ok(response) => {
                        let parsed = if index == 0 {
                            response
                                .json::<Vec<IssueComment>>()
                                .map(|got| comments.extend(got))
                        } else {
                            response
                                .json::<Vec<PullReview>>()
                                .map(|got| reviews.extend(got))
                        };
                        if let Err(error) = parsed {
                            return Unit::Fail(error.to_string());
                        }
                        match response.next_page() {
                            Some(next) if next > page => page = next,
                            _ => break,
                        }
                    }
                    Got::Gone | Got::Empty | Got::NotModified => break,
                    Got::NoBudget if truncate => break 'lists,
                    Got::NoBudget => return Unit::NoBudget,
                    Got::Wait(_) if truncate && self.left() == 0 => break 'lists,
                    Got::Wait(seconds) => return Unit::Wait(seconds),
                    Got::Fail(error) => return Unit::Fail(error),
                }
            }
        }
        Unit::Read {
            item: Box::new(item),
            comments,
            reviews,
        }
    }

    // -----------------------------------------------------------------------
    // The backfill

    /// One repository's share of the tick.
    async fn backfill(&mut self, repo: &Repo, share: u32) -> Result<(), StoreError> {
        if self.waiting(repo.installation_id) {
            return Ok(());
        }
        let Ok(gh_repo) = GhRepo::parse(&repo.full_name) else {
            return self
                .record_error(repo, "the repository name is not usable as a project")
                .await;
        };
        let Some(filing) = self.filing(repo).await? else {
            return self
                .record_error(repo, "the repository's scope is not a page scope")
                .await;
        };
        let Some(creds) = self.credentials(&repo.workspace_id).await? else {
            return self
                .record_error(repo, "the workspace's GitHub App credentials are missing")
                .await;
        };
        let mut cursor = DrainCursor::parse(repo.backfill_cursor.as_deref());
        let token = match self.token(&creds, repo.installation_id).await {
            Ok(token) => token,
            Err(got) => {
                let stop = match got {
                    Got::Wait(seconds) => {
                        self.hold(repo.installation_id, seconds);
                        Stop::Wait(seconds)
                    }
                    Got::Fail(error) => Stop::Fail(error),
                    _ => return Ok(()),
                };
                return self.settle(repo, cursor, stop, 0).await;
            }
        };

        // Measured after the token, so "the first work of the tick" (which
        // may file a truncated unit rather than stall) is the first read.
        let spent_before = self.used();
        let mut listed: HashMap<u64, Item> = HashMap::new();
        let mut filed = 0_u32;
        let stop = loop {
            let spent = self.used().saturating_sub(spent_before);
            if spent >= share || self.left() == 0 {
                break Stop::Budget;
            }
            if self.waiting(repo.installation_id) {
                let until = self.waits[&repo.installation_id] - self.now();
                break Stop::Wait(u64::try_from(until).unwrap_or(1));
            }
            let Some(request) = pick(&cursor.state) else {
                break Stop::Done;
            };
            let step = self
                .step(
                    repo,
                    &gh_repo,
                    &filing,
                    &creds,
                    &token,
                    &mut cursor,
                    &mut listed,
                    request,
                    spent == 0,
                )
                .await;
            match step {
                Ok(n) => filed += n,
                Err(stop) => break stop,
            }
        };
        self.settle(repo, cursor, stop, filed).await
    }

    /// Makes the next planned request and applies it. Answers how many
    /// records it filed, or why the pass stops.
    #[allow(clippy::too_many_arguments)]
    async fn step(
        &mut self,
        repo: &Repo,
        gh_repo: &GhRepo,
        filing: &Filing,
        creds: &AppCredentials,
        token: &Secret,
        cursor: &mut DrainCursor,
        listed: &mut HashMap<u64, Item>,
        request: Request,
        first: bool,
    ) -> Result<u32, Stop> {
        let inst = repo.installation_id;
        let app = creds.app_id;
        match request {
            Request::CommitList { page } => {
                let etag = (page == 1).then(|| cursor.etag_commits.clone()).flatten();
                let path = request.path(gh_repo);
                let response = match self.get(inst, app, token, &path, etag.as_deref()).await {
                    Got::Ok(response) => {
                        let items: Vec<CommitSummary> = response.json().map_err(fail)?;
                        if page == 1 {
                            cursor.etag_commits = response.header("etag").map(str::to_owned);
                        }
                        Response::CommitList {
                            items,
                            next_page: response.next_page(),
                        }
                    }
                    Got::NotModified | Got::Empty => Response::CommitList {
                        items: Vec::new(),
                        next_page: None,
                    },
                    other => return Err(stop_for(other, "the commit listing")),
                };
                cursor.state = planner::apply(cursor.state.clone(), &request, &response)
                    .map_err(|_| Stop::Fail("the commit listing did not apply".to_owned()))?
                    .state;
                Ok(0)
            }
            Request::IssueList { page, .. } => {
                let etag = (page == 1).then(|| cursor.etag_issues.clone()).flatten();
                let path = request.path(gh_repo);
                let response = match self.get(inst, app, token, &path, etag.as_deref()).await {
                    Got::Ok(response) => {
                        let items: Vec<Item> = response.json().map_err(fail)?;
                        if page == 1 {
                            cursor.etag_issues = response.header("etag").map(str::to_owned);
                        }
                        Response::IssueList {
                            items,
                            next_page: response.next_page(),
                        }
                    }
                    // Issues disabled (410) or nothing new (304): an empty page.
                    Got::NotModified | Got::Gone | Got::Empty => Response::IssueList {
                        items: Vec::new(),
                        next_page: None,
                    },
                    other => return Err(stop_for(other, "the issue listing")),
                };
                let applied = planner::apply(cursor.state.clone(), &request, &response)
                    .map_err(|_| Stop::Fail("the issue listing did not apply".to_owned()))?;
                let Response::IssueList { items, .. } = response else {
                    return Ok(0);
                };
                let mut by_number: HashMap<u64, Item> =
                    items.into_iter().map(|item| (item.number, item)).collect();
                let mut filed = 0;
                for number in &applied.finished_items {
                    if let Some(item) = by_number.remove(number) {
                        let kind = if item.pull_request.is_some() {
                            ItemKind::PullRequest
                        } else {
                            ItemKind::Issue
                        };
                        let record = record::map_item(gh_repo, &item, kind, &[], &[]);
                        self.file(filing, record::item_source(gh_repo, &record))
                            .await?;
                        filed += 1;
                    }
                }
                listed.extend(by_number);
                cursor.state = applied.state;
                Ok(filed)
            }
            Request::IssueComments { number, .. } | Request::PullReviews { number, .. } => {
                let hint = listed.remove(&number);
                let unit = self
                    .fetch_unit(inst, app, token, gh_repo, number, hint.clone(), first)
                    .await;
                match unit {
                    Unit::Read {
                        item,
                        comments,
                        reviews,
                    } => {
                        let pull_request = item.pull_request.is_some()
                            || cursor
                                .state
                                .item_queue
                                .iter()
                                .any(|job| job.number == number && job.pull_request);
                        let kind = if pull_request {
                            ItemKind::PullRequest
                        } else {
                            ItemKind::Issue
                        };
                        let record = record::map_item(gh_repo, &item, kind, &comments, &reviews);
                        self.file(filing, record::item_source(gh_repo, &record))
                            .await?;
                        cursor.state.item_queue.retain(|job| job.number != number);
                        Ok(1)
                    }
                    Unit::Gone => {
                        cursor.state.item_queue.retain(|job| job.number != number);
                        Ok(0)
                    }
                    Unit::NoBudget => {
                        if let Some(item) = hint {
                            listed.insert(number, item);
                        }
                        Err(Stop::Budget)
                    }
                    Unit::Wait(seconds) => Err(Stop::Wait(seconds)),
                    Unit::Fail(error) => Err(Stop::Fail(error)),
                }
            }
            Request::CommitDetail { ref sha } => {
                let path = request.path(gh_repo);
                match self.get(inst, app, token, &path, None).await {
                    Got::Ok(response) => {
                        let detail: CommitDetail = response.json().map_err(fail)?;
                        let record = record::map_commit(gh_repo, &detail, MapOptions::default());
                        self.file(filing, record::commit_source(gh_repo, &record))
                            .await?;
                        let response = Response::CommitDetail(detail);
                        match planner::apply(cursor.state.clone(), &request, &response) {
                            Ok(applied) => cursor.state = applied.state,
                            Err(_) => cursor.state.commit_queue.retain(|queued| queued != sha),
                        }
                        Ok(1)
                    }
                    Got::Gone | Got::Empty | Got::NotModified => {
                        cursor.state.commit_queue.retain(|queued| queued != sha);
                        Ok(0)
                    }
                    other => Err(stop_for(other, "a commit")),
                }
            }
        }
    }

    /// Writes where a pass stopped: the cursor, the counters and, for a
    /// finished pass, the incremental cursor of the next one.
    async fn settle(
        &mut self,
        repo: &Repo,
        mut cursor: DrainCursor,
        stop: Stop,
        filed: u32,
    ) -> Result<(), StoreError> {
        let now = self.now();
        cursor.v = CURSOR_VERSION;
        let mut done = false;
        let mut error = None;
        match stop {
            Stop::Done => {
                cursor.state = cursor.state.next_run();
                cursor.failures = 0;
                cursor.wait_until = None;
                if cursor.rerun {
                    cursor.rerun = false;
                } else {
                    done = true;
                }
            }
            Stop::Budget => {
                cursor.failures = 0;
                cursor.wait_until = None;
            }
            Stop::Wait(seconds) => {
                cursor.wait_until = Some(now + i64::try_from(seconds).unwrap_or(60));
            }
            Stop::Fail(message) => {
                cursor.failures = cursor.failures.saturating_add(1);
                let backoff = FAIL_BACKOFF_SECS
                    .saturating_mul(1_i64 << cursor.failures.saturating_sub(1).min(10))
                    .min(MAX_FAIL_BACKOFF_SECS);
                cursor.wait_until = Some(now + backoff);
                error = Some(message);
            }
            Stop::Abort(message) => return self.record_error(repo, &message).await,
        }
        let saved = drain::save_backfill_progress(
            self.db,
            self.drain,
            self.clock.now(),
            &repo.workspace_id,
            repo.installation_id,
            repo.repo_id,
            &BackfillProgress {
                cursor: Some(cursor.to_json()),
                items: filed,
                done,
            },
        )
        .await;
        match saved {
            Ok(_) => {}
            Err(StoreError::Invalid(_)) => {
                error = Some("the backfill cursor outgrew its limit".to_owned());
            }
            Err(other) => return Err(other),
        }
        if let Some(message) = error {
            self.record_error(repo, &message).await?;
        }
        Ok(())
    }

    /// Records why a repository's pass failed: a fixed text or a status,
    /// never a header, token or body.
    async fn record_error(&self, repo: &Repo, message: &str) -> Result<(), StoreError> {
        drain::record_repo_error(
            self.db,
            self.drain,
            self.clock.now(),
            &repo.workspace_id,
            repo.installation_id,
            repo.repo_id,
            message,
        )
        .await
    }
}

/// The next request the planner wants, holding a listing back while its
/// queue is long.
fn pick(state: &BackfillState) -> Option<Request> {
    planner::plan(state, u32::MAX)
        .into_iter()
        .find(|request| match request {
            Request::CommitList { .. } => state.commit_queue.len() < LIST_THROTTLE,
            Request::IssueList { .. } => state.item_queue.len() < LIST_THROTTLE,
            _ => true,
        })
}

fn fail(error: github::GhError) -> Stop {
    Stop::Fail(error.to_string())
}

fn stop_for(got: Got, what: &str) -> Stop {
    match got {
        Got::NoBudget => Stop::Budget,
        Got::Wait(seconds) => Stop::Wait(seconds),
        Got::Fail(error) => Stop::Fail(format!("{what}: {error}")),
        Got::Gone => Stop::Fail(format!("{what}: GitHub has no such repository (404)")),
        Got::Ok(_) | Got::NotModified | Got::Empty => {
            Stop::Fail(format!("{what}: unexpected answer"))
        }
    }
}

/// A record the ledger will not take (too large to redact, or text the
/// redactor refuses) is skipped; a store failure stops the pass unsaved.
fn skip_or_stop(error: &IngestError) -> Result<(), Stop> {
    match error {
        IngestError::TooLarge
        | IngestError::Refused(_)
        | IngestError::InvalidPath(_)
        | IngestError::InvalidProject(_) => Ok(()),
        IngestError::InvalidScope(_) => Err(Stop::Abort(
            "the repository's scope is not a page scope".to_owned(),
        )),
        IngestError::Store(_) => Err(Stop::Abort(format!(
            "the ledger refused the write ({})",
            error.variant()
        ))),
    }
}
