//! Installations of a workspace's app, and the installation → workspace
//! binding webhook routing reads.

use cratefield_core::{Database, Row, Statement};

use crate::app::{StateProof, StatePurpose};
use crate::common::{
    StoreError, bounded, check_github_id, get_int, get_opt_int, get_opt_text, get_text, int,
    require_admin, text, unix,
};

/// An installation's lifecycle as GitHub reports it. `Deleted` is terminal:
/// GitHub never reuses an installation id, so nothing moves a deleted row
/// back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallationStatus {
    Active,
    Suspended,
    Deleted,
}

impl InstallationStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Suspended => "suspended",
            Self::Deleted => "deleted",
        }
    }

    fn parse(value: &str) -> Self {
        match value {
            "active" => Self::Active,
            "suspended" => Self::Suspended,
            // Anything unrecognised is treated as gone, never as active.
            _ => Self::Deleted,
        }
    }
}

/// The account an installation sits on, as GitHub's payload names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub login: String,
    /// `User` or `Organization`.
    pub kind: String,
}

/// One installation, as the store holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installation {
    pub installation_id: i64,
    pub app_id: i64,
    /// `None` until an owner or admin binds it.
    pub workspace_id: Option<String>,
    pub account: Account,
    pub status: InstallationStatus,
    pub bound_by: Option<String>,
    pub bound_at: Option<i64>,
}

const COLUMNS: &str = "installation_id, app_id, workspace_id, account_login, account_type, \
                       status, bound_by, bound_at";

fn installation_from(row: &Row) -> Installation {
    Installation {
        installation_id: get_int(row, "installation_id"),
        app_id: get_int(row, "app_id"),
        workspace_id: get_opt_text(row, "workspace_id"),
        account: Account {
            login: get_text(row, "account_login"),
            kind: get_text(row, "account_type"),
        },
        status: InstallationStatus::parse(&get_text(row, "status")),
        bound_by: get_opt_text(row, "bound_by"),
        bound_at: get_opt_int(row, "bound_at"),
    }
}

async fn read(db: &dyn Database, installation_id: i64) -> Result<Option<Installation>, StoreError> {
    let rows = db
        .query(&Statement::with_values(
            format!("SELECT {COLUMNS} FROM github_installations WHERE installation_id = ?"),
            vec![int(installation_id)],
        ))
        .await?;
    Ok(rows.first().map(installation_from))
}

/// Records an installation a **verified** webhook named (`installation`
/// `created`, or any event carrying `installation.id`) for `app_id` — the
/// app whose webhook secret verified the delivery. A new row is unbound; an
/// existing row gets its account refreshed and nothing else: a webhook can
/// never bind, move or revive an installation.
///
/// Answers `false` (and writes nothing) when the installation is already
/// held under a *different* app id: one app's webhook cannot speak for
/// another app's installation.
///
/// # Errors
///
/// A storage failure, or an invalid id.
pub async fn observe_installation(
    db: &dyn Database,
    now: time::OffsetDateTime,
    app_id: i64,
    installation_id: i64,
    account: &Account,
) -> Result<bool, StoreError> {
    check_github_id(app_id, "app id")?;
    check_github_id(installation_id, "installation id")?;
    let now = unix(now);
    let login = bounded(&account.login, 100);
    let kind = bounded(&account.kind, 20);
    db.execute(&Statement::with_values(
        "INSERT INTO github_installations \
           (installation_id, app_id, account_login, account_type, status, created_at, updated_at) \
         VALUES (?, ?, ?, ?, 'active', ?, ?) \
         ON CONFLICT (installation_id) DO UPDATE SET \
           account_login = excluded.account_login, \
           account_type = excluded.account_type, \
           updated_at = excluded.updated_at \
         WHERE github_installations.app_id = excluded.app_id",
        vec![
            int(installation_id),
            int(app_id),
            text(&login),
            text(&kind),
            int(now),
            int(now),
        ],
    ))
    .await?;
    Ok(read(db, installation_id)
        .await?
        .is_some_and(|row| row.app_id == app_id))
}

/// What [`bind_installation`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindOutcome {
    /// The installation is now bound to the proof's workspace.
    Bound,
    /// It already was. A success: the caller asked for a state it is in.
    AlreadyThisWorkspace,
    /// It is bound to another workspace. Nothing changed (no hijack).
    BoundElsewhere,
    /// It is deleted, or held under an app that is not this workspace's.
    /// Nothing changed.
    Refused,
    /// The workspace has no app, so there is nothing to bind to.
    NoApp,
}

/// Binds `installation_id` to the workspace a spent **install** state names.
///
/// The rules, all enforced here rather than trusted from the caller:
///
/// - the proof is for [`StatePurpose::Install`], and its member is an owner
///   or admin of the workspace *now* (re-read);
/// - the installation belongs to **this workspace's** app: a row already
///   held under another app id is refused, and a new row is created under
///   this workspace's app id;
/// - an installation bound to another workspace is never moved — the bind
///   is one compare-and-set (`workspace_id IS NULL OR workspace_id = ?`), so
///   two racing binds cannot both win;
/// - a deleted installation is never bound.
///
/// **The caller must first confirm with GitHub** — `GET
/// /app/installations/{installation_id}` signed with this workspace's app
/// JWT — that the id is an installation of this app: the `installation_id`
/// on the setup URL is attacker-controlled text, and the store cannot make
/// an HTTP call. Pass the account from that verified response.
///
/// # Errors
///
/// [`StoreError::Forbidden`] for a proof of another purpose or a member who
/// is no longer an owner or admin; a storage failure.
pub async fn bind_installation(
    db: &dyn Database,
    now: time::OffsetDateTime,
    proof: StateProof,
    installation_id: i64,
    account: &Account,
) -> Result<BindOutcome, StoreError> {
    if proof.purpose != StatePurpose::Install {
        return Err(StoreError::Forbidden);
    }
    check_github_id(installation_id, "installation id")?;
    let workspace_id = proof.workspace_id.as_str();
    require_admin(db, workspace_id, &proof.member_id).await?;
    let Some(app) = crate::app::app_summary(db, workspace_id).await? else {
        return Ok(BindOutcome::NoApp);
    };

    // Seen-or-create under this workspace's app; a row under another app is
    // left exactly as it was (and refused below).
    observe_installation(db, now, app.app_id, installation_id, account).await?;

    let bound = db
        .execute(&Statement::with_values(
            "UPDATE github_installations \
             SET workspace_id = ?, bound_by = ?, bound_at = ?, updated_at = ? \
             WHERE installation_id = ? AND app_id = ? AND status <> 'deleted' \
               AND workspace_id IS NULL",
            vec![
                text(workspace_id),
                text(&proof.member_id),
                int(unix(now)),
                int(unix(now)),
                int(installation_id),
                int(app.app_id),
            ],
        ))
        .await?;
    if bound == 1 {
        return Ok(BindOutcome::Bound);
    }
    let Some(row) = read(db, installation_id).await? else {
        return Ok(BindOutcome::Refused);
    };
    Ok(
        if row.app_id != app.app_id || row.status == InstallationStatus::Deleted {
            BindOutcome::Refused
        } else if row.workspace_id.as_deref() == Some(workspace_id) {
            BindOutcome::AlreadyThisWorkspace
        } else {
            BindOutcome::BoundElsewhere
        },
    )
}

/// Where a verified webhook for `app_id` about `installation_id` goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub workspace_id: String,
    pub status: InstallationStatus,
}

/// The workspace a verified webhook routes to, or `None` when the
/// installation is unknown, unbound, or held under a different app than the
/// one whose secret verified the delivery. A `Some` with a status other
/// than `Active` means: accept the delivery, do no work.
///
/// # Errors
///
/// A storage failure.
pub async fn route_installation(
    db: &dyn Database,
    app_id: i64,
    installation_id: i64,
) -> Result<Option<Route>, StoreError> {
    let rows = db
        .query(&Statement::with_values(
            "SELECT workspace_id, status FROM github_installations \
             WHERE installation_id = ? AND app_id = ? AND workspace_id IS NOT NULL",
            vec![int(installation_id), int(app_id)],
        ))
        .await?;
    Ok(rows.first().and_then(|row| {
        Some(Route {
            workspace_id: get_opt_text(row, "workspace_id")?,
            status: InstallationStatus::parse(&get_text(row, "status")),
        })
    }))
}

/// The installations bound to `workspace_id`, deleted ones included (a
/// settings page shows what went away), by id.
///
/// # Errors
///
/// A storage failure.
pub async fn list_installations(
    db: &dyn Database,
    workspace_id: &str,
) -> Result<Vec<Installation>, StoreError> {
    let rows = db
        .query(&Statement::with_values(
            format!(
                "SELECT {COLUMNS} FROM github_installations WHERE workspace_id = ? \
                 ORDER BY installation_id"
            ),
            vec![text(workspace_id)],
        ))
        .await?;
    Ok(rows.rows.iter().map(installation_from).collect())
}

/// Applies a lifecycle event from a **verified** webhook for `app_id`
/// (`installation` `suspend`, `unsuspend`, `deleted`). In one atomic batch:
///
/// - `Suspended`: the installation and every repository under it stop
///   (`active = 0`); queued work stays and is skipped until it resumes.
/// - `Active` (unsuspend): the installation and its still-included,
///   not-removed repositories resume. A deleted installation stays deleted.
/// - `Deleted`: terminal. The repositories stop and the queued work is
///   dropped.
///
/// Answers whether a row changed (an unknown installation, another app's,
/// or reviving a deleted one changes nothing).
///
/// # Errors
///
/// A storage failure.
pub async fn set_installation_status(
    db: &dyn Database,
    now: time::OffsetDateTime,
    app_id: i64,
    installation_id: i64,
    status: InstallationStatus,
) -> Result<bool, StoreError> {
    let now = unix(now);
    let Some(row) = read(db, installation_id).await? else {
        return Ok(false);
    };
    if row.app_id != app_id || row.status == InstallationStatus::Deleted || row.status == status {
        return Ok(false);
    }
    let mut statements = vec![Statement::with_values(
        "UPDATE github_installations SET status = ?, updated_at = ? \
         WHERE installation_id = ? AND app_id = ? AND status <> 'deleted'",
        vec![
            text(status.as_str()),
            int(now),
            int(installation_id),
            int(app_id),
        ],
    )];
    match status {
        InstallationStatus::Suspended | InstallationStatus::Deleted => {
            statements.push(Statement::with_values(
                "UPDATE github_repos SET active = 0, updated_at = ? WHERE installation_id = ?",
                vec![int(now), int(installation_id)],
            ));
        }
        InstallationStatus::Active => {
            // A repository an admin excluded, or GitHub removed from the
            // installation (`removed_at`), stays stopped.
            statements.push(Statement::with_values(
                "UPDATE github_repos SET active = 1, updated_at = ? \
                 WHERE installation_id = ? AND included = 1 AND removed_at IS NULL",
                vec![int(now), int(installation_id)],
            ));
        }
    }
    if status == InstallationStatus::Deleted {
        statements.push(Statement::with_values(
            "DELETE FROM github_queue WHERE installation_id = ?",
            vec![int(installation_id)],
        ));
    }
    db.batch_atomic(&statements).await?;
    Ok(true)
}
