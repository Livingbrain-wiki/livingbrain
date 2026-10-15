//! The in-Worker cron drain, and the writes only it may make.
//!
//! **Backfill and ingest run only as a system inside Living Brain**: the
//! Worker's scheduled handler drains the queue and walks each repository's
//! history with the cursor and counters stored here. No agent, local
//! script, CLI command or admin route drives an import from outside.
//!
//! That rule is made structural rather than conventional. Every write of
//! backfill progress and every queue claim takes a [`Drain`], and a `Drain`
//! can only be made by [`run`] — the body of [`Module::scheduled`]. Its one
//! constructor is private to this file, and a test
//! (`only_the_scheduled_drain_mints_a_drain`) fails the build if any other
//! source file in the crate names it, so a route added later cannot quietly
//! become an import trigger. Nothing outside the crate can make one at all,
//! and the CLI crate does not depend on this one (also tested).
//!
//! [`Module::scheduled`]: cratefield_core::Module::scheduled

use cratefield_core::{Database, ModuleContext, Statement};

use crate::common::{
    StoreError, bounded, get_int, get_opt_int, get_text, int, random_hex, text, unix,
};
use crate::repos::{BackfillPhase, REPO_COLUMNS, Repo, repo_from};

/// The capability to write backfill progress and to claim queued work.
/// Held only by the scheduled drain: see the module docs.
#[derive(Debug)]
pub struct Drain {
    _private: (),
}

impl Drain {
    /// The one constructor. Called from [`run`] and from this crate's own
    /// tests, nowhere else.
    fn for_scheduled_run() -> Self {
        Self { _private: () }
    }

    #[cfg(test)]
    pub(crate) fn for_tests() -> Self {
        Self::for_scheduled_run()
    }
}

/// The scheduled entry point (`Module::scheduled`). It mints the [`Drain`],
/// does the housekeeping that needs no GitHub call (forgetting expired
/// state tokens and deliveries past their retention), then hands the
/// `Drain` to the backfill and queue work in `sync.rs`, which runs within
/// `ctx.scheduled`'s budget and Part A's per-tick planner budget.
pub(crate) async fn run(
    ctx: &ModuleContext,
    _cron: &str,
    shared: &crate::Shared,
) -> Result<(), StoreError> {
    let (Some(db), Some(clock)) = (ctx.ports.db.clone(), ctx.ports.clock.clone()) else {
        return Ok(());
    };
    let drain = Drain::for_scheduled_run();
    let now = clock.now();
    crate::deliveries::prune_deliveries(&*db, now).await?;
    crate::app::prune_states(&*db, now).await?;
    crate::sync::run(ctx, &drain, shared).await
}

/// Puts repositories whose backfill finished more than `older_than_secs`
/// ago back to `running`, so their stored incremental cursor (the
/// planner's `next_run`) catches anything a lost webhook missed. Only
/// included, active, not-removed repositories.
///
/// # Errors
///
/// A storage failure.
pub async fn reopen_stale(
    db: &dyn Database,
    _drain: &Drain,
    now: time::OffsetDateTime,
    older_than_secs: i64,
) -> Result<u64, StoreError> {
    let now = unix(now);
    Ok(db
        .execute(&Statement::with_values(
            "UPDATE github_repos SET backfill_state = 'running', backfill_finished_at = NULL, \
               updated_at = ? \
             WHERE backfill_state = 'done' AND active = 1 AND included = 1 \
               AND removed_at IS NULL AND backfill_finished_at < ?",
            vec![int(now), int(now - older_than_secs)],
        ))
        .await?)
}

/// The longest error text kept on a row or queue item. The drain must
/// never pass a token, a header or a response body: a status code and a
/// short reason are what belongs here.
pub const MAX_ERROR_CHARS: usize = 300;
/// Attempts after which a queue item is parked as dead.
pub const MAX_ATTEMPTS: i64 = 5;
/// The largest backfill cursor the store accepts, in bytes: room for the
/// planner state of `livingbrain-github` (`planner::BackfillState`) with a
/// few pages of queued commits and items.
pub const MAX_CURSOR_BYTES: usize = 64 * 1024;

/// Repositories whose backfill is not done and which may run now: included,
/// not removed, and under a bound, active installation — across every
/// workspace (the drain is the system's, not a tenant's), oldest first.
///
/// # Errors
///
/// A storage failure.
pub async fn repos_due(
    db: &dyn Database,
    _drain: &Drain,
    limit: u32,
) -> Result<Vec<Repo>, StoreError> {
    let columns = REPO_COLUMNS
        .split(", ")
        .map(|c| format!("r.{}", c.trim()))
        .collect::<Vec<_>>()
        .join(", ");
    let rows = db
        .query(&Statement::with_values(
            format!(
                "SELECT {columns} FROM github_repos r \
                   JOIN github_installations i \
                     ON i.installation_id = r.installation_id \
                    AND i.workspace_id = r.workspace_id \
                  WHERE r.active = 1 AND r.included = 1 AND r.removed_at IS NULL \
                    AND r.backfill_state <> 'done' AND i.status = 'active' \
                  ORDER BY r.updated_at, r.installation_id, r.repo_id \
                  LIMIT ?"
            ),
            vec![int(i64::from(limit.min(100)))],
        ))
        .await?;
    Ok(rows.rows.iter().map(repo_from).collect())
}

/// One batch of backfill progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackfillProgress {
    /// Where the next batch starts (opaque JSON, at most
    /// [`MAX_CURSOR_BYTES`]), or `None` once done.
    pub cursor: Option<String>,
    /// Items this batch read.
    pub items: u32,
    /// Whether history is exhausted.
    pub done: bool,
}

/// Records one batch: the cursor, the counters, and `running` or `done`.
/// Clears the last error. Only lands on a repository that is still active
/// in `workspace_id` — a repository excluded, removed or suspended while
/// the batch ran keeps its old cursor. Answers whether it landed.
///
/// # Errors
///
/// [`StoreError::Invalid`] for a cursor that is not JSON or is too large; a
/// storage failure.
#[allow(clippy::too_many_arguments)]
pub async fn save_backfill_progress(
    db: &dyn Database,
    drain: &Drain,
    now: time::OffsetDateTime,
    workspace_id: &str,
    installation_id: i64,
    repo_id: i64,
    progress: &BackfillProgress,
) -> Result<bool, StoreError> {
    save_progress(
        db,
        drain,
        now,
        (workspace_id, installation_id, repo_id),
        progress,
        None,
    )
    .await
}

/// [`save_backfill_progress`], landing only while the stored cursor is
/// still `expected` (`None`: still no cursor) — a compare-and-set, so a
/// pass that lost its repository to another run, or a push that read a
/// cursor another run has since moved, writes nothing. Answers whether it
/// landed.
///
/// # Errors
///
/// As [`save_backfill_progress`].
#[allow(clippy::too_many_arguments)]
pub async fn save_backfill_progress_if(
    db: &dyn Database,
    drain: &Drain,
    now: time::OffsetDateTime,
    workspace_id: &str,
    installation_id: i64,
    repo_id: i64,
    progress: &BackfillProgress,
    expected: Option<&str>,
) -> Result<bool, StoreError> {
    save_progress(
        db,
        drain,
        now,
        (workspace_id, installation_id, repo_id),
        progress,
        Some(expected),
    )
    .await
}

async fn save_progress(
    db: &dyn Database,
    _drain: &Drain,
    now: time::OffsetDateTime,
    (workspace_id, installation_id, repo_id): (&str, i64, i64),
    progress: &BackfillProgress,
    expected: Option<Option<&str>>,
) -> Result<bool, StoreError> {
    if let Some(cursor) = progress.cursor.as_deref()
        && (cursor.len() > MAX_CURSOR_BYTES
            || serde_json::from_str::<serde_json::Value>(cursor).is_err())
    {
        return Err(StoreError::Invalid("backfill cursor"));
    }
    let state = if progress.done {
        BackfillPhase::Done
    } else {
        BackfillPhase::Running
    };
    let now = unix(now);
    let finished = progress.done.then_some(now);
    let sql = format!(
        "UPDATE github_repos SET \
               backfill_state = ?, backfill_cursor = ?, \
               backfill_items = backfill_items + ?, backfill_batches = backfill_batches + 1, \
               backfill_started_at = COALESCE(backfill_started_at, ?), \
               backfill_finished_at = ?, last_error = NULL, last_error_at = NULL, \
               updated_at = ? \
             WHERE workspace_id = ? AND installation_id = ? AND repo_id = ? AND active = 1{}",
        if expected.is_some() {
            " AND backfill_cursor IS ?"
        } else {
            ""
        }
    );
    let mut values = vec![
        text(state.as_str()),
        crate::common::opt_text(progress.cursor.as_deref()),
        int(i64::from(progress.items)),
        int(now),
        crate::common::opt_int(finished),
        int(now),
        text(workspace_id),
        int(installation_id),
        int(repo_id),
    ];
    if let Some(expected) = expected {
        values.push(crate::common::opt_text(expected));
    }
    Ok(db.execute(&Statement::with_values(sql, values)).await? == 1)
}

/// Swaps a repository's cursor from `expected` to `new` and nothing else —
/// not its state, counters or `updated_at` — only while it is still
/// `expected` (`None`: no cursor yet). How a pass takes a repository's
/// lease and gives it back unused. Answers whether it swapped.
///
/// # Errors
///
/// [`StoreError::Invalid`] for a `new` cursor that is not JSON or is too
/// large; a storage failure.
pub async fn swap_cursor(
    db: &dyn Database,
    _drain: &Drain,
    (workspace_id, installation_id, repo_id): (&str, i64, i64),
    expected: Option<&str>,
    new: Option<&str>,
) -> Result<bool, StoreError> {
    if let Some(cursor) = new
        && (cursor.len() > MAX_CURSOR_BYTES
            || serde_json::from_str::<serde_json::Value>(cursor).is_err())
    {
        return Err(StoreError::Invalid("backfill cursor"));
    }
    Ok(db
        .execute(&Statement::with_values(
            "UPDATE github_repos SET backfill_cursor = ? \
             WHERE workspace_id = ? AND installation_id = ? AND repo_id = ? AND active = 1 \
               AND backfill_cursor IS ?",
            vec![
                crate::common::opt_text(new),
                text(workspace_id),
                int(installation_id),
                int(repo_id),
                crate::common::opt_text(expected),
            ],
        ))
        .await?
        == 1)
}

/// Records why a repository's last batch failed. The cursor is kept, so the
/// next run retries from where it stood. `error` is bounded to
/// [`MAX_ERROR_CHARS`] and stripped of control characters — but the caller
/// must still never pass a credential, a header or a response body.
///
/// # Errors
///
/// A storage failure.
#[allow(clippy::too_many_arguments)]
pub async fn record_repo_error(
    db: &dyn Database,
    _drain: &Drain,
    now: time::OffsetDateTime,
    workspace_id: &str,
    installation_id: i64,
    repo_id: i64,
    error: &str,
) -> Result<(), StoreError> {
    let now = unix(now);
    db.execute(&Statement::with_values(
        "UPDATE github_repos SET last_error = ?, last_error_at = ?, updated_at = ? \
         WHERE workspace_id = ? AND installation_id = ? AND repo_id = ?",
        vec![
            text(&bounded(error, MAX_ERROR_CHARS)),
            int(now),
            int(now),
            text(workspace_id),
            int(installation_id),
            int(repo_id),
        ],
    ))
    .await?;
    Ok(())
}

/// A queue item the drain holds a lease on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claimed {
    pub id: String,
    /// Proof of this claim: [`complete`] and [`fail`] need it, so a drain
    /// whose lease expired cannot finish an item another run now holds.
    pub lease_token: String,
    pub workspace_id: String,
    pub installation_id: i64,
    pub repo_id: Option<i64>,
    pub kind: String,
    pub subject: String,
    /// Including this claim.
    pub attempts: i64,
}

/// Claims up to `limit` ready items for `lease_secs`: not dead, available,
/// unleased or with an expired lease, and under an active installation
/// bound to the item's workspace. One compare-and-set `UPDATE`, so two
/// overlapping runs never hold the same item.
///
/// # Errors
///
/// A storage failure.
pub async fn claim(
    db: &dyn Database,
    _drain: &Drain,
    now: time::OffsetDateTime,
    limit: u32,
    lease_secs: i64,
) -> Result<Vec<Claimed>, StoreError> {
    let token = random_hex(16)?;
    let now = unix(now);
    db.execute(&Statement::with_values(
        "UPDATE github_queue SET lease_token = ?, lease_until = ?, attempts = attempts + 1 \
         WHERE dead = 0 AND (lease_until IS NULL OR lease_until < ?) AND id IN \
           (SELECT q.id FROM github_queue q \
             WHERE q.dead = 0 AND q.available_at <= ? \
               AND (q.lease_until IS NULL OR q.lease_until < ?) \
               AND EXISTS (SELECT 1 FROM github_installations i \
                            WHERE i.installation_id = q.installation_id \
                              AND i.workspace_id = q.workspace_id AND i.status = 'active') \
             ORDER BY q.available_at, q.enqueued_at, q.id LIMIT ?)",
        vec![
            text(&token),
            int(now + lease_secs.clamp(1, 15 * 60)),
            int(now),
            int(now),
            int(now),
            int(i64::from(limit.min(100))),
        ],
    ))
    .await?;
    let rows = db
        .query(&Statement::with_values(
            "SELECT id, lease_token, workspace_id, installation_id, repo_id, kind, subject, \
                    attempts FROM github_queue WHERE lease_token = ? \
             ORDER BY available_at, enqueued_at, id",
            vec![text(&token)],
        ))
        .await?;
    Ok(rows
        .rows
        .iter()
        .map(|row| Claimed {
            id: get_text(row, "id"),
            lease_token: get_text(row, "lease_token"),
            workspace_id: get_text(row, "workspace_id"),
            installation_id: get_int(row, "installation_id"),
            repo_id: get_opt_int(row, "repo_id"),
            kind: get_text(row, "kind"),
            subject: get_text(row, "subject"),
            attempts: get_int(row, "attempts"),
        })
        .collect())
}

/// Finishes a claimed item: it is deleted. Answers `false` when the lease
/// is no longer this claim's.
///
/// # Errors
///
/// A storage failure.
pub async fn complete(
    db: &dyn Database,
    _drain: &Drain,
    item: &Claimed,
) -> Result<bool, StoreError> {
    Ok(db
        .execute(&Statement::with_values(
            "DELETE FROM github_queue WHERE id = ? AND lease_token = ?",
            vec![text(&item.id), text(&item.lease_token)],
        ))
        .await?
        == 1)
}

/// Fails a claimed item: released for retry after `retry_after_secs`, or
/// parked as dead once it has had [`MAX_ATTEMPTS`]. Answers `false` when the
/// lease is no longer this claim's. The same rule as
/// [`record_repo_error`] holds for `error`.
///
/// # Errors
///
/// A storage failure.
pub async fn fail(
    db: &dyn Database,
    _drain: &Drain,
    now: time::OffsetDateTime,
    item: &Claimed,
    error: &str,
    retry_after_secs: i64,
) -> Result<bool, StoreError> {
    Ok(db
        .execute(&Statement::with_values(
            "UPDATE github_queue SET lease_token = NULL, lease_until = NULL, \
               available_at = ?, last_error = ?, \
               dead = CASE WHEN attempts >= ? THEN 1 ELSE 0 END \
             WHERE id = ? AND lease_token = ?",
            vec![
                int(unix(now) + retry_after_secs.max(0)),
                text(&bounded(error, MAX_ERROR_CHARS)),
                int(MAX_ATTEMPTS),
                text(&item.id),
                text(&item.lease_token),
            ],
        ))
        .await?
        == 1)
}
