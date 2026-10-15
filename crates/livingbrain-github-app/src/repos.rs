//! The repositories an installation reaches, per workspace.
//!
//! Configuration (project, scope, included) is an owner's or admin's to
//! change; GitHub's view (added, removed, renamed) arrives through verified
//! webhooks; the backfill columns are **read** here and written only by the
//! cron drain ([`crate::drain`]).

use cratefield_core::{Database, Row, Statement};

use crate::common::{
    StoreError, bounded, check_github_id, check_id, get_int, get_opt_int, get_opt_text, get_text,
    int, require_admin, text, unix,
};

/// Where a repository's backfill stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackfillPhase {
    /// Not started; the drain picks it up.
    Pending,
    /// Started; the cursor says where the next batch begins.
    Running,
    /// Every historical item has been read; webhooks carry it from here.
    Done,
}

impl BackfillPhase {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Done => "done",
        }
    }

    fn parse(value: &str) -> Self {
        match value {
            "running" => Self::Running,
            "done" => Self::Done,
            _ => Self::Pending,
        }
    }
}

/// One repository, as a settings page or the drain reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    pub workspace_id: String,
    pub installation_id: i64,
    pub repo_id: i64,
    pub full_name: String,
    /// The Living Brain project its pages file under.
    pub project: String,
    /// `shared`, or `user:<member id>` for one member's own scope.
    pub scope: String,
    /// An admin's choice: `false` stops it without forgetting it.
    pub included: bool,
    /// Whether work runs for it now: included, not removed by GitHub, and
    /// its installation active.
    pub active: bool,
    pub backfill_state: BackfillPhase,
    /// The drain's opaque JSON cursor.
    pub backfill_cursor: Option<String>,
    pub backfill_items: i64,
    pub backfill_batches: i64,
    pub backfill_started_at: Option<i64>,
    pub backfill_finished_at: Option<i64>,
    pub removed_at: Option<i64>,
    pub last_webhook_at: Option<i64>,
    pub last_error: Option<String>,
    pub last_error_at: Option<i64>,
}

pub(crate) const REPO_COLUMNS: &str = "workspace_id, installation_id, repo_id, full_name, project, \
     scope, included, active, backfill_state, backfill_cursor, backfill_items, backfill_batches, \
     backfill_started_at, backfill_finished_at, removed_at, last_webhook_at, last_error, \
     last_error_at";

pub(crate) fn repo_from(row: &Row) -> Repo {
    Repo {
        workspace_id: get_text(row, "workspace_id"),
        installation_id: get_int(row, "installation_id"),
        repo_id: get_int(row, "repo_id"),
        full_name: get_text(row, "full_name"),
        project: get_text(row, "project"),
        scope: get_text(row, "scope"),
        included: get_int(row, "included") != 0,
        active: get_int(row, "active") != 0,
        backfill_state: BackfillPhase::parse(&get_text(row, "backfill_state")),
        backfill_cursor: get_opt_text(row, "backfill_cursor"),
        backfill_items: get_int(row, "backfill_items"),
        backfill_batches: get_int(row, "backfill_batches"),
        backfill_started_at: get_opt_int(row, "backfill_started_at"),
        backfill_finished_at: get_opt_int(row, "backfill_finished_at"),
        removed_at: get_opt_int(row, "removed_at"),
        last_webhook_at: get_opt_int(row, "last_webhook_at"),
        last_error: get_opt_text(row, "last_error"),
        last_error_at: get_opt_int(row, "last_error_at"),
    }
}

/// A repository as GitHub names it in a verified payload or API response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRef {
    pub repo_id: i64,
    /// `owner/name`.
    pub full_name: String,
}

/// The default project for a repository: its name without the owner, in the
/// case GitHub spells it. Agent sessions file under the repository basename
/// in its original case (`EarthOS.world`), so a GitHub repository must keep
/// that case to merge with them.
fn default_project(full_name: &str) -> String {
    let name = full_name.rsplit('/').next().unwrap_or(full_name);
    project_name(name)
}

/// A project name: ASCII letters, digits, `-`, `_` and `.` in their original
/// case, 1..=64 characters. Anything else becomes `-`. The case is kept on
/// purpose: project matching is exact, and the session side never lowercases.
fn project_name(value: &str) -> String {
    let name: String = value
        .chars()
        .take(64)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    if name.is_empty() {
        "repo".to_owned()
    } else {
        name
    }
}

/// Records (or renames) repositories an installation reaches — the install
/// callback's initial list, and `installation_repositories` `added` from a
/// verified webhook. The workspace is **derived from the binding**: a row is
/// written only when `installation_id` is bound to `workspace_id` and
/// active, so a webhook cannot file a repository into a workspace its
/// installation is not bound to. A new row starts `pending`, included, in
/// the `shared` scope, under the repository's own name as project. A
/// returning repository (removed, then added again) comes back active and
/// keeps its settings and cursor.
///
/// Answers how many rows were written (0 when the installation is not
/// bound to that workspace or not active).
///
/// # Errors
///
/// A storage failure, or an invalid id.
pub async fn upsert_repos(
    db: &dyn Database,
    now: time::OffsetDateTime,
    workspace_id: &str,
    installation_id: i64,
    repos: &[RepoRef],
) -> Result<u64, StoreError> {
    check_id(workspace_id, "workspace id")?;
    check_github_id(installation_id, "installation id")?;
    let now = unix(now);
    let mut written = 0;
    for repo in repos {
        check_github_id(repo.repo_id, "repo id")?;
        let full_name = bounded(&repo.full_name, 200);
        written += db
            .execute(&Statement::with_values(
                "INSERT INTO github_repos \
                   (installation_id, repo_id, workspace_id, full_name, project, \
                    created_at, updated_at) \
                 SELECT i.installation_id, ?, i.workspace_id, ?, ?, ?, ? \
                   FROM github_installations i \
                  WHERE i.installation_id = ? AND i.workspace_id = ? AND i.status = 'active' \
                 ON CONFLICT (installation_id, repo_id) DO UPDATE SET \
                   full_name = excluded.full_name, \
                   removed_at = NULL, \
                   active = github_repos.included, \
                   updated_at = excluded.updated_at",
                vec![
                    int(repo.repo_id),
                    text(&full_name),
                    text(&default_project(&full_name)),
                    int(now),
                    int(now),
                    int(installation_id),
                    text(workspace_id),
                ],
            ))
            .await?;
    }
    Ok(written)
}

/// `installation_repositories` `removed`: the repository stops and is
/// marked removed, its settings and cursor kept in case it comes back.
/// Scoped to the workspace the installation routes to.
///
/// # Errors
///
/// A storage failure.
pub async fn remove_repo(
    db: &dyn Database,
    now: time::OffsetDateTime,
    workspace_id: &str,
    installation_id: i64,
    repo_id: i64,
) -> Result<bool, StoreError> {
    let now = unix(now);
    Ok(db
        .execute(&Statement::with_values(
            "UPDATE github_repos SET active = 0, removed_at = ?, updated_at = ? \
             WHERE workspace_id = ? AND installation_id = ? AND repo_id = ?",
            vec![
                int(now),
                int(now),
                text(workspace_id),
                int(installation_id),
                int(repo_id),
            ],
        ))
        .await?
        == 1)
}

/// Every repository in `workspace_id`, by full name.
///
/// # Errors
///
/// A storage failure.
pub async fn list_repos(db: &dyn Database, workspace_id: &str) -> Result<Vec<Repo>, StoreError> {
    let rows = db
        .query(&Statement::with_values(
            format!(
                "SELECT {REPO_COLUMNS} FROM github_repos WHERE workspace_id = ? \
                 ORDER BY full_name, repo_id"
            ),
            vec![text(workspace_id)],
        ))
        .await?;
    Ok(rows.rows.iter().map(repo_from).collect())
}

/// One repository in `workspace_id`, or `None` — including when it exists
/// in another workspace.
///
/// # Errors
///
/// A storage failure.
pub async fn get_repo(
    db: &dyn Database,
    workspace_id: &str,
    installation_id: i64,
    repo_id: i64,
) -> Result<Option<Repo>, StoreError> {
    let rows = db
        .query(&Statement::with_values(
            format!(
                "SELECT {REPO_COLUMNS} FROM github_repos \
                 WHERE workspace_id = ? AND installation_id = ? AND repo_id = ?"
            ),
            vec![text(workspace_id), int(installation_id), int(repo_id)],
        ))
        .await?;
    Ok(rows.first().map(repo_from))
}

/// An admin's settings for one repository. `None` leaves a field as it is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoSettings {
    pub project: Option<String>,
    /// `shared`, or `user:<member id>` naming a current member.
    pub scope: Option<String>,
    pub included: Option<bool>,
}

/// Changes one repository's settings. Owner or admin only, and only in
/// their own workspace. Excluding stops the repository; including resumes
/// it when GitHub has not removed it and its installation is active.
/// Answers whether the repository was found.
///
/// # Errors
///
/// [`StoreError::Forbidden`] for anyone but an owner or admin;
/// [`StoreError::Invalid`] for a scope that is not `shared` or a current
/// member's `user:` scope; a storage failure.
pub async fn set_repo_settings(
    db: &dyn Database,
    now: time::OffsetDateTime,
    workspace_id: &str,
    member_id: &str,
    installation_id: i64,
    repo_id: i64,
    settings: &RepoSettings,
) -> Result<bool, StoreError> {
    require_admin(db, workspace_id, member_id).await?;
    if let Some(scope) = settings.scope.as_deref() {
        check_scope(db, workspace_id, scope).await?;
    }
    let Some(current) = get_repo(db, workspace_id, installation_id, repo_id).await? else {
        return Ok(false);
    };
    let project = settings
        .project
        .as_deref()
        .map_or(current.project, project_name);
    let scope = settings.scope.clone().unwrap_or(current.scope);
    let included = settings.included.unwrap_or(current.included);
    db.execute(&Statement::with_values(
        "UPDATE github_repos SET project = ?, scope = ?, included = ?, \
           active = CASE WHEN ? = 1 AND removed_at IS NULL AND EXISTS \
             (SELECT 1 FROM github_installations i \
               WHERE i.installation_id = github_repos.installation_id \
                 AND i.workspace_id = github_repos.workspace_id AND i.status = 'active') \
             THEN 1 ELSE 0 END, \
           updated_at = ? \
         WHERE workspace_id = ? AND installation_id = ? AND repo_id = ?",
        vec![
            text(&project),
            text(&scope),
            int(i64::from(included)),
            int(i64::from(included)),
            int(unix(now)),
            text(workspace_id),
            int(installation_id),
            int(repo_id),
        ],
    ))
    .await?;
    Ok(true)
}

async fn check_scope(db: &dyn Database, workspace_id: &str, scope: &str) -> Result<(), StoreError> {
    if scope == "shared" {
        return Ok(());
    }
    let member = scope
        .strip_prefix("user:")
        .ok_or(StoreError::Invalid("scope"))?;
    check_id(member, "scope")?;
    match livingbrain_workspaces::caller_for(db, workspace_id, member).await? {
        Some(_) => Ok(()),
        None => Err(StoreError::Invalid("scope")),
    }
}

/// Stamps `last_webhook_at` on a repository a verified delivery named, in
/// the workspace the installation routes to.
///
/// # Errors
///
/// A storage failure.
pub async fn note_webhook(
    db: &dyn Database,
    now: time::OffsetDateTime,
    workspace_id: &str,
    installation_id: i64,
    repo_id: i64,
) -> Result<(), StoreError> {
    db.execute(&Statement::with_values(
        "UPDATE github_repos SET last_webhook_at = ? \
         WHERE workspace_id = ? AND installation_id = ? AND repo_id = ?",
        vec![
            int(unix(now)),
            text(workspace_id),
            int(installation_id),
            int(repo_id),
        ],
    ))
    .await?;
    Ok(())
}

#[cfg(test)]
mod project_name_tests {
    use super::{default_project, project_name};

    #[test]
    fn a_repository_keeps_its_original_case_as_its_project() {
        // The same spelling an agent session files under (its repo basename).
        assert_eq!(default_project("World-360/EarthOS.world"), "EarthOS.world");
        assert_eq!(default_project("World-360/Brain_Repo"), "Brain_Repo");
        assert!(livingbrain_pages::is_project(&default_project(
            "World-360/EarthOS.world"
        )));
    }

    #[test]
    fn a_user_chosen_project_keeps_its_case_and_loses_only_unsafe_characters() {
        assert_eq!(project_name("Brain Repo!"), "Brain-Repo-");
        assert_eq!(project_name("EarthOS.world"), "EarthOS.world");
    }
}
