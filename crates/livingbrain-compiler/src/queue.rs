//! The queue: one job per `agent_log` source body, with a status.
//!
//! **Enqueueing is derived, not pushed.** The ledger already holds every
//! synced session, keyed `(scope, body_sha256)`; [`Queue::enqueue_from_ledger`]
//! inserts a job for every `agent_log` row that has none, with
//! `ON CONFLICT DO NOTHING` on the same key. So it is idempotent per source
//! body by construction, it needs no hook in the ingest path, and it picks
//! up the sessions synced before the compiler existed — the backlog the
//! person asked for — as well as every new one.
//!
//! **A session that grew** is a new body (the CLI re-syncs it whole), so it
//! is a new job; the older job of the same session (`session_key`, the
//! ledger's `rel_path`), if it has not run yet, is marked `superseded`
//! rather than compiled twice.
//!
//! **Claims are compare-and-set.** A job is claimed by one `UPDATE` that
//! names the status and `updated_at` it was read at; zero rows changed means
//! somebody else claimed it. A claim carries a lease, so a Worker that died
//! mid-job leaves a job that becomes claimable again when the lease lapses.

use std::collections::BTreeMap;
use std::sync::Arc;

use cratefield_core::{Database, DbError, Row, Statement};
use sea_query::Value as SeaValue;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};

/// How many times a failing job is tried before it stays `failed`.
pub const MAX_ATTEMPTS: u32 = 3;

/// A job's status. The set is closed and each one is shown to the person.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum JobStatus {
    /// Waiting for a drain.
    Queued,
    /// A drain holds it under a lease.
    Processing,
    /// Compiled: its pages are written.
    Done,
    /// Tried and failed; retried with backoff up to [`MAX_ATTEMPTS`].
    Failed,
    /// No model is connected in the workspace. Re-checked hourly, and at
    /// once by `Process now`.
    SkippedNoModel,
    /// The workspace's compile budget is spent for now. Re-checked when it
    /// may have reset.
    OverBudget,
    /// A newer body of the same session arrived before this one ran.
    Superseded,
    /// Nothing to compile: the body is not a session record, or its source
    /// is gone (its scope was forgotten).
    Skipped,
    /// A model call is out on the owner's own runner (their subscription,
    /// on their device); the compile carries on when the answer arrives.
    WaitingRunner,
}

impl JobStatus {
    /// Every status, in the order a status card lists them.
    pub const ALL: [JobStatus; 9] = [
        Self::Queued,
        Self::Processing,
        Self::Done,
        Self::Failed,
        Self::SkippedNoModel,
        Self::OverBudget,
        Self::Superseded,
        Self::Skipped,
        Self::WaitingRunner,
    ];

    /// The stored and wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Processing => "processing",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::SkippedNoModel => "skipped-no-model",
            Self::OverBudget => "over-budget",
            Self::Superseded => "superseded",
            Self::Skipped => "skipped",
            Self::WaitingRunner => "waiting-runner",
        }
    }

    /// Parses a stored name.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|status| status.as_str() == value)
    }
}

/// One job row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub scope: String,
    pub body_sha256: String,
    pub source_id: String,
    pub workspace: String,
    pub owner: String,
    pub session_key: String,
    pub project: Option<String>,
    pub source_created_at: String,
    pub status: JobStatus,
    pub attempts: u32,
    pub last_error: Option<String>,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// The slugs the compile wrote.
    pub pages: Vec<String>,
    pub rejected: u32,
    pub facts: u32,
    pub queued_at: String,
    pub updated_at: String,
    pub lease_until: Option<String>,
    pub next_attempt_at: Option<String>,
    /// Transcript chunks already applied by earlier runs of this job.
    pub chunks_done: u32,
}

/// Which jobs a claim may take.
#[derive(Debug, Clone, Default)]
pub struct ClaimFilter {
    /// Only this workspace's jobs.
    pub workspace: Option<String>,
    /// Only jobs in these scopes (the asker's own, for `Process now`).
    pub scopes: Option<Vec<String>>,
    /// Ignore a parked job's `next_attempt_at` — `Process now` re-checks a
    /// `skipped-no-model` or `over-budget` job at once, because the person
    /// pressing it may just have connected a model.
    pub ignore_backoff: bool,
    /// Only this source's job — the runner's answer to one of its calls
    /// arrived, and the compile carries on at once.
    pub source_id: Option<String>,
}

/// How a compile ended, as the queue records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finish {
    pub status: JobStatus,
    /// A short, fixed-vocabulary reason, and for a model failure what the
    /// provider said, made safe to show. Never model output or source text.
    pub error: Option<String>,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub pages: Vec<String>,
    pub rejected: u32,
    pub facts: u32,
    /// When a parked or failed job may be tried again.
    pub retry_at: Option<OffsetDateTime>,
    /// How many transcript chunks are applied now, when this run moved it.
    pub chunks_done: Option<u32>,
    /// The run handed a call to the runner and only waits: it does not
    /// count as one of the job's attempts.
    pub refund_attempt: bool,
}

impl Finish {
    /// A finish with only a status and a reason.
    #[must_use]
    pub fn status(status: JobStatus, error: Option<&str>) -> Self {
        Self {
            status,
            error: error.map(str::to_owned),
            prompt_tokens: 0,
            completion_tokens: 0,
            pages: Vec::new(),
            rejected: 0,
            facts: 0,
            retry_at: None,
            chunks_done: None,
            refund_attempt: false,
        }
    }
}

/// The queue over the `Database` port.
#[derive(Clone)]
pub struct Queue {
    db: Arc<dyn Database>,
}

impl Queue {
    #[must_use]
    pub fn new(db: Arc<dyn Database>) -> Self {
        Self { db }
    }

    /// Inserts a `queued` job for every unheld `agent_log` source that has
    /// none — in one workspace, or in all of them — and supersedes the
    /// not-yet-run jobs of sessions that have a newer body. Answers how
    /// many jobs were added.
    ///
    /// # Errors
    ///
    /// [`DbError`] from the database.
    pub async fn enqueue_from_ledger(
        &self,
        now: OffsetDateTime,
        workspace: Option<&str>,
    ) -> Result<u64, DbError> {
        let stamp = rfc3339(now);
        let mut values = vec![text(&stamp), text(&stamp)];
        let mut sql = String::from(
            "INSERT INTO coding_agent_jobs \
             (scope, body_sha256, source_id, workspace, owner, session_key, project, \
              source_created_at, status, attempts, prompt_tokens, completion_tokens, pages, \
              rejected, facts, queued_at, updated_at) \
             SELECT s.scope, s.body_sha256, s.id, s.workspace, s.imported_by, s.rel_path, \
                    s.project, s.created_at, 'queued', 0, 0, 0, '[]', 0, 0, ?, ? \
             FROM sources s WHERE s.kind = 'agent_log' AND s.held = 0",
        );
        if let Some(workspace) = workspace {
            sql.push_str(" AND s.workspace = ?");
            values.push(text(workspace));
        }
        sql.push_str(" ON CONFLICT (scope, body_sha256) DO NOTHING");
        let added = self
            .db
            .execute(&Statement::with_values(sql, values))
            .await?;
        self.supersede(now).await?;
        Ok(added)
    }

    /// Marks every job that has not compiled yet, and whose session has a
    /// newer body in the same scope, `superseded`.
    async fn supersede(&self, now: OffsetDateTime) -> Result<u64, DbError> {
        self.db
            .execute(&Statement::with_values(
                "UPDATE coding_agent_jobs SET status = 'superseded', updated_at = ?, \
                        lease_until = NULL, next_attempt_at = NULL \
                 WHERE status IN ('queued', 'failed', 'skipped-no-model', 'over-budget', \
                                  'waiting-runner') \
                   AND session_key <> '' \
                   AND EXISTS (SELECT 1 FROM coding_agent_jobs newer \
                               WHERE newer.scope = coding_agent_jobs.scope \
                                 AND newer.session_key = coding_agent_jobs.session_key \
                                 AND newer.source_created_at > coding_agent_jobs.source_created_at)",
                vec![text(&rfc3339(now))],
            ))
            .await
    }

    /// The jobs a drain may take now, oldest first, at most `limit` — read,
    /// not claimed. See [`Queue::claim`].
    ///
    /// # Errors
    ///
    /// [`DbError`] from the database.
    pub async fn ready(
        &self,
        now: OffsetDateTime,
        filter: &ClaimFilter,
        limit: usize,
    ) -> Result<Vec<Job>, DbError> {
        let stamp = rfc3339(now);
        let mut values = Vec::new();
        let parked = if filter.ignore_backoff {
            "status IN ('skipped-no-model', 'over-budget', 'waiting-runner')".to_owned()
        } else {
            values.push(text(&stamp));
            "(status IN ('skipped-no-model', 'over-budget', 'waiting-runner') \
             AND next_attempt_at <= ?)"
                .to_owned()
        };
        values.push(text(&stamp));
        values.push(SeaValue::BigInt(Some(i64::from(MAX_ATTEMPTS))));
        values.push(text(&stamp));
        let mut sql = format!(
            "SELECT {COLUMNS} FROM coding_agent_jobs WHERE (status = 'queued' OR {parked} \
             OR (status = 'failed' AND next_attempt_at <= ? AND attempts < ?) \
             OR (status = 'processing' AND lease_until < ?))"
        );
        if let Some(workspace) = &filter.workspace {
            sql.push_str(" AND workspace = ?");
            values.push(text(workspace));
        }
        if let Some(source_id) = &filter.source_id {
            sql.push_str(" AND source_id = ?");
            values.push(text(source_id));
        }
        if let Some(scopes) = &filter.scopes {
            if scopes.is_empty() {
                return Ok(Vec::new());
            }
            sql.push_str(&format!(" AND scope IN ({})", placeholders(scopes.len())));
            values.extend(scopes.iter().map(|scope| text(scope)));
        }
        sql.push_str(" ORDER BY queued_at ASC, source_created_at ASC LIMIT ?");
        values.push(SeaValue::BigInt(Some(
            i64::try_from(limit).unwrap_or(i64::MAX),
        )));
        let rows = self.db.query(&Statement::with_values(sql, values)).await?;
        Ok(rows.rows.iter().filter_map(job_from).collect())
    }

    /// Claims one job read by [`Queue::ready`]: `processing` under a lease
    /// that ends `lease` from now, one more attempt counted. `false` when
    /// somebody else claimed it, or it changed, since it was read.
    ///
    /// # Errors
    ///
    /// [`DbError`] from the database.
    pub async fn claim(
        &self,
        job: &Job,
        now: OffsetDateTime,
        lease: Duration,
    ) -> Result<bool, DbError> {
        let changed = self
            .db
            .execute(&Statement::with_values(
                "UPDATE coding_agent_jobs SET status = 'processing', attempts = attempts + 1, \
                        lease_until = ?, updated_at = ? \
                 WHERE scope = ? AND body_sha256 = ? AND status = ? AND updated_at = ?",
                vec![
                    text(&rfc3339(now + lease)),
                    text(&rfc3339(now)),
                    text(&job.scope),
                    text(&job.body_sha256),
                    text(job.status.as_str()),
                    text(&job.updated_at),
                ],
            ))
            .await?;
        Ok(changed == 1)
    }

    /// Parks a job that was read but not claimed — no model, no budget —
    /// without counting an attempt. Compare-and-set like a claim.
    ///
    /// # Errors
    ///
    /// [`DbError`] from the database.
    pub async fn park(
        &self,
        job: &Job,
        status: JobStatus,
        now: OffsetDateTime,
        retry_at: OffsetDateTime,
        reason: &str,
    ) -> Result<bool, DbError> {
        let changed = self
            .db
            .execute(&Statement::with_values(
                "UPDATE coding_agent_jobs SET status = ?, last_error = ?, next_attempt_at = ?, \
                        lease_until = NULL, updated_at = ? \
                 WHERE scope = ? AND body_sha256 = ? AND status = ? AND updated_at = ?",
                vec![
                    text(status.as_str()),
                    text(reason),
                    text(&rfc3339(retry_at)),
                    text(&rfc3339(now)),
                    text(&job.scope),
                    text(&job.body_sha256),
                    text(job.status.as_str()),
                    text(&job.updated_at),
                ],
            ))
            .await?;
        Ok(changed == 1)
    }

    /// Records how a claimed job ended. Tokens and page lists accumulate
    /// across attempts, so a retried job shows everything it spent and
    /// everything it touched.
    ///
    /// # Errors
    ///
    /// [`DbError`] from the database.
    pub async fn finish(
        &self,
        job: &Job,
        finish: &Finish,
        now: OffsetDateTime,
    ) -> Result<(), DbError> {
        let current = self.get(&job.scope, &job.body_sha256).await?;
        let mut pages = current
            .as_ref()
            .map(|j| j.pages.clone())
            .unwrap_or_default();
        for slug in &finish.pages {
            if !pages.contains(slug) {
                pages.push(slug.clone());
            }
        }
        let pages_json = serde_json::to_string(&pages).unwrap_or_else(|_| "[]".to_owned());
        let attempts = current.as_ref().map_or(job.attempts, |j| j.attempts);
        let attempts = if finish.refund_attempt {
            attempts.saturating_sub(1)
        } else {
            attempts
        };
        let chunks_done = finish
            .chunks_done
            .or_else(|| current.as_ref().map(|j| j.chunks_done))
            .unwrap_or(0);
        self.db
            .execute(&Statement::with_values(
                "UPDATE coding_agent_jobs SET status = ?, last_error = ?, \
                        prompt_tokens = prompt_tokens + ?, completion_tokens = completion_tokens + ?, \
                        pages = ?, rejected = rejected + ?, facts = facts + ?, \
                        lease_until = NULL, next_attempt_at = ?, updated_at = ?, \
                        attempts = ?, chunks_done = ? \
                 WHERE scope = ? AND body_sha256 = ?",
                vec![
                    text(finish.status.as_str()),
                    opt_text(finish.error.as_deref()),
                    big(finish.prompt_tokens),
                    big(finish.completion_tokens),
                    text(&pages_json),
                    big(u64::from(finish.rejected)),
                    big(u64::from(finish.facts)),
                    opt_text(finish.retry_at.map(rfc3339).as_deref()),
                    text(&rfc3339(now)),
                    big(u64::from(attempts)),
                    big(u64::from(chunks_done)),
                    text(&job.scope),
                    text(&job.body_sha256),
                ],
            ))
            .await?;
        Ok(())
    }

    /// One job by its key.
    ///
    /// # Errors
    ///
    /// [`DbError`] from the database.
    pub async fn get(&self, scope: &str, body_sha256: &str) -> Result<Option<Job>, DbError> {
        let rows = self
            .db
            .query(&Statement::with_values(
                format!(
                    "SELECT {COLUMNS} FROM coding_agent_jobs WHERE scope = ? AND body_sha256 = ?"
                ),
                vec![text(scope), text(body_sha256)],
            ))
            .await?;
        Ok(rows.first().and_then(job_from))
    }

    /// The jobs in `workspace` whose scope is one of `scopes`, newest first,
    /// optionally only those of one source id. `scopes` must come from the
    /// credential — the same two clauses as the source listing.
    ///
    /// # Errors
    ///
    /// [`DbError`] from the database.
    pub async fn list(
        &self,
        workspace: &str,
        scopes: &[String],
        source_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Job>, DbError> {
        if scopes.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let mut values = vec![text(workspace)];
        values.extend(scopes.iter().map(|scope| text(scope)));
        let mut sql = format!(
            "SELECT {COLUMNS} FROM coding_agent_jobs WHERE workspace = ? AND scope IN ({})",
            placeholders(scopes.len())
        );
        if let Some(id) = source_id {
            sql.push_str(" AND source_id = ?");
            values.push(text(id));
        }
        sql.push_str(" ORDER BY source_created_at DESC LIMIT ?");
        values.push(SeaValue::BigInt(Some(
            i64::try_from(limit).unwrap_or(i64::MAX),
        )));
        let rows = self.db.query(&Statement::with_values(sql, values)).await?;
        Ok(rows.rows.iter().filter_map(job_from).collect())
    }

    /// How many of the asker's jobs sit in each status.
    ///
    /// # Errors
    ///
    /// [`DbError`] from the database.
    pub async fn counts(
        &self,
        workspace: &str,
        scopes: &[String],
    ) -> Result<BTreeMap<JobStatus, u64>, DbError> {
        let mut counts: BTreeMap<JobStatus, u64> = JobStatus::ALL
            .into_iter()
            .map(|status| (status, 0))
            .collect();
        if scopes.is_empty() {
            return Ok(counts);
        }
        let mut values = vec![text(workspace)];
        values.extend(scopes.iter().map(|scope| text(scope)));
        let rows = self
            .db
            .query(&Statement::with_values(
                format!(
                    "SELECT status, COUNT(*) AS n FROM coding_agent_jobs \
                     WHERE workspace = ? AND scope IN ({}) GROUP BY status",
                    placeholders(scopes.len())
                ),
                values,
            ))
            .await?;
        for row in &rows.rows {
            if let Some(status) = row
                .get::<String>("status")
                .and_then(|s| JobStatus::parse(&s))
            {
                let n = row
                    .get::<i64>("n")
                    .and_then(|n| u64::try_from(n).ok())
                    .unwrap_or(0);
                counts.insert(status, n);
            }
        }
        Ok(counts)
    }

    /// Puts every `failed` job in these scopes of `workspace` back to
    /// `queued` with its attempts reset — the "Retry failed" button, for a
    /// job that ran out of attempts (or is waiting out a backoff) on a
    /// failure that is fixed now. The tokens and pages it already recorded
    /// stay, and so does `last_error`, until the next run says otherwise.
    /// Answers how many were requeued.
    ///
    /// # Errors
    ///
    /// [`DbError`] from the database.
    pub async fn requeue_failed(
        &self,
        workspace: &str,
        scopes: &[String],
        now: OffsetDateTime,
    ) -> Result<u64, DbError> {
        if scopes.is_empty() {
            return Ok(0);
        }
        let mut values = vec![text(&rfc3339(now)), text(workspace)];
        values.extend(scopes.iter().map(|scope| text(scope)));
        self.db
            .execute(&Statement::with_values(
                format!(
                    "UPDATE coding_agent_jobs SET status = 'queued', attempts = 0, \
                            next_attempt_at = NULL, lease_until = NULL, updated_at = ? \
                     WHERE status = 'failed' AND workspace = ? AND scope IN ({})",
                    placeholders(scopes.len())
                ),
                values,
            ))
            .await
    }

    /// When this session last compiled, if an earlier body of it did: the
    /// compiler tells the model, so a grown session updates the pages
    /// rather than restating them.
    ///
    /// # Errors
    ///
    /// [`DbError`] from the database.
    pub async fn previously_compiled(&self, job: &Job) -> Result<Option<String>, DbError> {
        if job.session_key.is_empty() {
            return Ok(None);
        }
        let rows = self
            .db
            .query(&Statement::with_values(
                "SELECT updated_at FROM coding_agent_jobs \
                 WHERE scope = ? AND session_key = ? AND body_sha256 <> ? AND status = 'done' \
                 ORDER BY updated_at DESC LIMIT 1",
                vec![
                    text(&job.scope),
                    text(&job.session_key),
                    text(&job.body_sha256),
                ],
            ))
            .await?;
        Ok(rows.first().and_then(|row| row.get::<String>("updated_at")))
    }

    /// Adds `tokens` to the workspace's compile spend for `now`'s UTC day.
    ///
    /// # Errors
    ///
    /// [`DbError`] from the database.
    pub async fn record_spend(
        &self,
        now: OffsetDateTime,
        workspace: &str,
        tokens: u64,
    ) -> Result<(), DbError> {
        self.db
            .execute(&Statement::with_values(
                "INSERT INTO coding_agent_spend (workspace, day, tokens, calls, updated_at) \
                 VALUES (?, ?, ?, 1, ?) \
                 ON CONFLICT (workspace, day) DO UPDATE SET \
                    tokens = coding_agent_spend.tokens + excluded.tokens, \
                    calls = coding_agent_spend.calls + excluded.calls, \
                    updated_at = excluded.updated_at",
                vec![
                    text(workspace),
                    text(&utc_day(now)),
                    big(tokens),
                    text(&rfc3339(now)),
                ],
            ))
            .await?;
        Ok(())
    }

    /// The workspace's compile spend, in tokens, over the UTC days
    /// `first..=last` (`YYYY-MM-DD`, compared as text — chronological on
    /// that format).
    ///
    /// # Errors
    ///
    /// [`DbError`] from the database.
    pub async fn spent(&self, workspace: &str, first: &str, last: &str) -> Result<u64, DbError> {
        let rows = self
            .db
            .query(&Statement::with_values(
                "SELECT tokens FROM coding_agent_spend WHERE workspace = ? AND day >= ? AND day <= ?",
                vec![text(workspace), text(first), text(last)],
            ))
            .await?;
        Ok(rows
            .rows
            .iter()
            .filter_map(|row| row.get::<i64>("tokens"))
            .filter_map(|n| u64::try_from(n).ok())
            .sum())
    }
}

/// The columns every job read selects, in [`job_from`]'s order.
const COLUMNS: &str = "scope, body_sha256, source_id, workspace, owner, session_key, project, \
     source_created_at, status, attempts, last_error, prompt_tokens, completion_tokens, pages, \
     rejected, facts, queued_at, updated_at, lease_until, next_attempt_at, chunks_done";

fn job_from(row: &Row) -> Option<Job> {
    let status = JobStatus::parse(&row.get::<String>("status")?)?;
    let count = |name: &str| {
        row.get::<i64>(name)
            .and_then(|n| u64::try_from(n).ok())
            .unwrap_or(0)
    };
    Some(Job {
        scope: row.get::<String>("scope")?,
        body_sha256: row.get::<String>("body_sha256")?,
        source_id: row.get::<String>("source_id").unwrap_or_default(),
        workspace: row.get::<String>("workspace").unwrap_or_default(),
        owner: row.get::<String>("owner").unwrap_or_default(),
        session_key: row.get::<String>("session_key").unwrap_or_default(),
        project: row.get::<String>("project"),
        source_created_at: row.get::<String>("source_created_at").unwrap_or_default(),
        status,
        attempts: u32::try_from(count("attempts")).unwrap_or(u32::MAX),
        last_error: row.get::<String>("last_error"),
        prompt_tokens: count("prompt_tokens"),
        completion_tokens: count("completion_tokens"),
        pages: row
            .get::<String>("pages")
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default(),
        rejected: u32::try_from(count("rejected")).unwrap_or(u32::MAX),
        facts: u32::try_from(count("facts")).unwrap_or(u32::MAX),
        queued_at: row.get::<String>("queued_at").unwrap_or_default(),
        updated_at: row.get::<String>("updated_at").unwrap_or_default(),
        lease_until: row.get::<String>("lease_until"),
        next_attempt_at: row.get::<String>("next_attempt_at"),
        chunks_done: u32::try_from(count("chunks_done")).unwrap_or(0),
    })
}

/// `?, ?, …` for `n` binds.
fn placeholders(n: usize) -> String {
    vec!["?"; n].join(", ")
}

fn text(value: &str) -> SeaValue {
    SeaValue::String(Some(Box::new(value.to_owned())))
}

fn opt_text(value: Option<&str>) -> SeaValue {
    SeaValue::String(value.map(|value| Box::new(value.to_owned())))
}

fn big(value: u64) -> SeaValue {
    SeaValue::BigInt(Some(i64::try_from(value).unwrap_or(i64::MAX)))
}

/// RFC 3339 in UTC at whole seconds, fixed width (`2026-10-15T04:19:00Z`):
/// the queue compares leases and retry times as text, and a fraction that
/// is sometimes there and sometimes not would make text order lie about
/// time order.
#[must_use]
pub fn rfc3339(at: OffsetDateTime) -> String {
    let at = at.to_offset(time::UtcOffset::UTC);
    let at = at.replace_nanosecond(0).unwrap_or(at);
    at.format(&Rfc3339).unwrap_or_default()
}

/// The UTC day of `at`, `YYYY-MM-DD`.
#[must_use]
pub fn utc_day(at: OffsetDateTime) -> String {
    let date = at.to_offset(time::UtcOffset::UTC).date();
    format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        u8::from(date.month()),
        date.day()
    )
}
