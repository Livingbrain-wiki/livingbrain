//! The work queue between a verified webhook and the cron drain.
//!
//! The webhook endpoint only **enqueues** ([`enqueue`]); claiming, finishing
//! and failing items is the drain's ([`crate::drain`]). A row is a
//! reference — a kind and a subject such as `issues` / `42` — never a copy
//! of GitHub content: the drain re-reads the current state, so a replayed
//! or reordered delivery costs a refetch and nothing else, and no issue
//! body or comment sits in this table waiting to be erased.

use cratefield_core::{Database, Statement};

use crate::common::{StoreError, check_github_id, check_id, int, opt_int, random_hex, text, unix};

/// One unit of work a verified delivery leaves for the drain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkItem {
    /// The repository it concerns, when it concerns one.
    pub repo_id: Option<i64>,
    /// What to re-read: `issues`, `pull_request`, `push`, … (lowercase
    /// ASCII, `_` and `.`, at most 64).
    pub kind: String,
    /// Which one: an issue number, a ref, a commit sha (printable ASCII, no
    /// spaces, at most 256). Empty for "the whole repository".
    pub subject: String,
}

fn check_item(item: &WorkItem) -> Result<(), StoreError> {
    let kind_ok = !item.kind.is_empty()
        && item.kind.len() <= 64
        && item
            .kind
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.'));
    let subject_ok =
        item.subject.len() <= 256 && item.subject.bytes().all(|b| b.is_ascii_graphic());
    if !kind_ok {
        return Err(StoreError::Invalid("work item kind"));
    }
    if !subject_ok {
        return Err(StoreError::Invalid("work item subject"));
    }
    if let Some(repo_id) = item.repo_id {
        check_github_id(repo_id, "repo id")?;
    }
    Ok(())
}

/// Queues `item` for the drain. Like a repository upsert, the row is
/// written only when `installation_id` is bound to `workspace_id` and
/// active (and, when the item names a repository, that repository is
/// active in it) — so a verified delivery for an unbound, suspended or
/// deleted installation queues nothing. Answers the item's id, or `None`
/// when nothing was queued.
///
/// # Errors
///
/// [`StoreError::Invalid`] for a malformed item; a storage failure.
pub async fn enqueue(
    db: &dyn Database,
    now: time::OffsetDateTime,
    workspace_id: &str,
    installation_id: i64,
    item: &WorkItem,
) -> Result<Option<String>, StoreError> {
    check_id(workspace_id, "workspace id")?;
    check_github_id(installation_id, "installation id")?;
    check_item(item)?;
    let id = random_hex(16)?;
    let now = unix(now);
    let inserted = db
        .execute(&Statement::with_values(
            "INSERT INTO github_queue \
               (id, workspace_id, installation_id, repo_id, kind, subject, enqueued_at, \
                available_at) \
             SELECT ?, i.workspace_id, i.installation_id, ?, ?, ?, ?, ? \
               FROM github_installations i \
              WHERE i.installation_id = ? AND i.workspace_id = ? AND i.status = 'active' \
                AND (? IS NULL OR EXISTS (SELECT 1 FROM github_repos r \
                      WHERE r.installation_id = i.installation_id AND r.repo_id = ? \
                        AND r.workspace_id = i.workspace_id AND r.active = 1))",
            vec![
                text(&id),
                opt_int(item.repo_id),
                text(&item.kind),
                text(&item.subject),
                int(now),
                int(now),
                int(installation_id),
                text(workspace_id),
                opt_int(item.repo_id),
                opt_int(item.repo_id),
            ],
        ))
        .await?;
    Ok((inserted == 1).then_some(id))
}

/// How many items are waiting (not dead) in `workspace_id`, for a status
/// line.
///
/// # Errors
///
/// A storage failure.
pub async fn queued_count(db: &dyn Database, workspace_id: &str) -> Result<i64, StoreError> {
    let rows = db
        .query(&Statement::with_values(
            "SELECT COUNT(*) AS n FROM github_queue WHERE workspace_id = ? AND dead = 0",
            vec![text(workspace_id)],
        ))
        .await?;
    Ok(rows
        .first()
        .map_or(0, |row| crate::common::get_int(row, "n")))
}

/// How many items in `workspace_id` were parked as dead after
/// [`crate::drain::MAX_ATTEMPTS`], for a status line.
///
/// # Errors
///
/// A storage failure.
pub async fn dead_count(db: &dyn Database, workspace_id: &str) -> Result<i64, StoreError> {
    let rows = db
        .query(&Statement::with_values(
            "SELECT COUNT(*) AS n FROM github_queue WHERE workspace_id = ? AND dead = 1",
            vec![text(workspace_id)],
        ))
        .await?;
    Ok(rows
        .first()
        .map_or(0, |row| crate::common::get_int(row, "n")))
}
