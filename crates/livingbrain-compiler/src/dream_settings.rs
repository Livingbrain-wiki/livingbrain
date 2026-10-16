//! A workspace's dreaming settings (`dreaming_settings`, docs/temporal-facts.md
//! section 9): whether the cron tidies its facts at all, whether the quick
//! rule pass runs, which connected model the model pass calls, and a "Run
//! now" request for the next tick.
//!
//! A missing row is the defaults ([`DreamSettings::default`]): dreaming on,
//! the rules on, the Main model. The cron reads them through
//! [`DreamTuning`]; a read that fails is the defaults, the same as a
//! workspace that never saved anything.

use std::sync::Arc;

use cratefield_core::{Database, DbError, Statement};
use sea_query::Value as SeaValue;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// The job slot a dream calls when a workspace has not chosen one: Main,
/// which is what dreaming called before it had a setting.
pub const DEFAULT_ROLE: &str = "main";

/// One workspace's dreaming settings, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DreamSettings {
    /// The cron tidies this workspace's subjects (the model pass) and its
    /// facts' rules (the rule pass) when set. Off: neither runs for it.
    pub enabled: bool,
    /// The quick rule pass (no model) runs for this workspace.
    pub rules_enabled: bool,
    /// The job slot the model pass calls: `main`, `triage` or `research`.
    pub model_role: String,
    /// RFC 3339 of a "Run now" not yet taken by a tick.
    pub run_requested_at: Option<String>,
    /// RFC 3339 of the last save, and who saved it (a user id), when stored.
    pub updated_at: Option<String>,
    pub updated_by: Option<String>,
}

impl Default for DreamSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            rules_enabled: true,
            model_role: DEFAULT_ROLE.to_owned(),
            run_requested_at: None,
            updated_at: None,
            updated_by: None,
        }
    }
}

/// Where the cron reads a workspace's dreaming settings from. The
/// composition wires [`StoredDreamTuning`]; a compiler without one dreams
/// with the defaults.
#[async_trait::async_trait]
pub trait DreamTuning: Send + Sync {
    /// The workspace's settings, or the defaults when none are stored or the
    /// read fails.
    async fn settings(&self, workspace: &str) -> DreamSettings;
    /// Takes a pending "Run now" for the workspace: `true` once, when one was
    /// requested and not yet taken, and clears it.
    async fn take_run_request(&self, workspace: &str) -> bool;
}

/// [`DreamTuning`] over the `dreaming_settings` table.
pub struct StoredDreamTuning {
    pub db: Arc<dyn Database>,
}

#[async_trait::async_trait]
impl DreamTuning for StoredDreamTuning {
    async fn settings(&self, workspace: &str) -> DreamSettings {
        load(&*self.db, workspace).await.unwrap_or_default()
    }

    async fn take_run_request(&self, workspace: &str) -> bool {
        take_run(&*self.db, workspace).await.unwrap_or(false)
    }
}

/// The workspace's stored settings, or the defaults when no row exists.
///
/// # Errors
///
/// [`DbError`] from the database.
pub async fn load(db: &dyn Database, workspace: &str) -> Result<DreamSettings, DbError> {
    let rows = db
        .query(&Statement::with_values(
            "SELECT enabled, rules_enabled, model_role, run_requested_at, updated_at, updated_by \
             FROM dreaming_settings WHERE workspace_id = ?",
            vec![text(workspace)],
        ))
        .await?;
    let Some(row) = rows.rows.first() else {
        return Ok(DreamSettings::default());
    };
    let defaults = DreamSettings::default();
    Ok(DreamSettings {
        enabled: row
            .get::<i64>("enabled")
            .map_or(defaults.enabled, |v| v != 0),
        rules_enabled: row
            .get::<i64>("rules_enabled")
            .map_or(defaults.rules_enabled, |v| v != 0),
        model_role: row
            .get::<String>("model_role")
            .filter(|role| !role.is_empty())
            .unwrap_or(defaults.model_role),
        run_requested_at: row.get::<String>("run_requested_at"),
        updated_at: row.get::<String>("updated_at"),
        updated_by: row.get::<String>("updated_by"),
    })
}

/// Saves the choices (`enabled`, `rules_enabled`, `model_role`) for
/// `workspace`, by `by` at `at`. A pending "Run now" is left as it is.
///
/// # Errors
///
/// [`DbError`] from the database.
pub async fn save(
    db: &dyn Database,
    workspace: &str,
    at: OffsetDateTime,
    by: &str,
    settings: &DreamSettings,
) -> Result<(), DbError> {
    let at = at.format(&Rfc3339).unwrap_or_default();
    db.execute(&Statement::with_values(
        "INSERT INTO dreaming_settings (workspace_id, enabled, rules_enabled, model_role, \
         updated_at, updated_by) VALUES (?, ?, ?, ?, ?, ?) \
         ON CONFLICT (workspace_id) DO UPDATE SET enabled = excluded.enabled, \
         rules_enabled = excluded.rules_enabled, model_role = excluded.model_role, \
         updated_at = excluded.updated_at, updated_by = excluded.updated_by",
        vec![
            text(workspace),
            SeaValue::BigInt(Some(i64::from(settings.enabled))),
            SeaValue::BigInt(Some(i64::from(settings.rules_enabled))),
            text(&settings.model_role),
            text(&at),
            text(by),
        ],
    ))
    .await?;
    Ok(())
}

/// Asks for a run: the next tick marks the workspace's subjects due.
///
/// # Errors
///
/// [`DbError`] from the database.
pub async fn request_run(
    db: &dyn Database,
    workspace: &str,
    at: OffsetDateTime,
) -> Result<(), DbError> {
    let at = at.format(&Rfc3339).unwrap_or_default();
    db.execute(&Statement::with_values(
        "INSERT INTO dreaming_settings (workspace_id, enabled, rules_enabled, model_role, \
         run_requested_at, updated_at) VALUES (?, 1, 1, 'main', ?, ?) \
         ON CONFLICT (workspace_id) DO UPDATE SET run_requested_at = excluded.run_requested_at",
        vec![text(workspace), text(&at), text(&at)],
    ))
    .await?;
    Ok(())
}

/// Takes a pending run request: `true` when there was one (now cleared).
///
/// # Errors
///
/// [`DbError`] from the database.
pub async fn take_run(db: &dyn Database, workspace: &str) -> Result<bool, DbError> {
    let rows = db
        .query(&Statement::with_values(
            "SELECT run_requested_at FROM dreaming_settings WHERE workspace_id = ?",
            vec![text(workspace)],
        ))
        .await?;
    let pending = rows
        .rows
        .first()
        .and_then(|row| row.get::<String>("run_requested_at"))
        .is_some();
    if pending {
        db.execute(&Statement::with_values(
            "UPDATE dreaming_settings SET run_requested_at = NULL WHERE workspace_id = ?",
            vec![text(workspace)],
        ))
        .await?;
    }
    Ok(pending)
}

fn text(value: &str) -> SeaValue {
    SeaValue::String(Some(Box::new(value.to_owned())))
}

/// What dreaming did in one workspace, for the Dreaming page's status strip.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DreamStatus {
    /// Subjects queued or running now.
    pub queued: i64,
    /// Subjects whose last pass finished since `week_start`.
    pub reviewed_week: i64,
    /// RFC 3339 of the last finished pass, across the workspace's subjects.
    pub last_run_at: Option<String>,
    /// Applied decisions since `week_start`, by op (`supersede`, `end`,
    /// `retract`, `rename`), sorted by op.
    pub changes_week: Vec<(String, i64)>,
}

/// The workspace's dreaming status since `week_start` (an RFC 3339 instant).
/// Reads only the scopes the workspace's compile jobs are in, so one
/// workspace never sees another's passes.
///
/// # Errors
///
/// [`DbError`] from the database.
pub async fn status(
    db: &dyn Database,
    workspace: &str,
    week_start: &str,
) -> Result<DreamStatus, DbError> {
    const SCOPES: &str = "SELECT DISTINCT scope FROM coding_agent_jobs WHERE workspace = ?";
    let counts = db
        .query(&Statement::with_values(
            format!(
                "SELECT \
                 COALESCE(SUM(CASE WHEN status IN ('queued', 'running') THEN 1 ELSE 0 END), 0) AS queued, \
                 COALESCE(SUM(CASE WHEN dreamt_at >= ? THEN 1 ELSE 0 END), 0) AS reviewed, \
                 MAX(dreamt_at) AS last_run \
                 FROM fact_dreams WHERE scope IN ({SCOPES})"
            ),
            vec![text(week_start), text(workspace)],
        ))
        .await?;
    let row = counts.rows.first();
    let mut status = DreamStatus {
        queued: row.and_then(|r| r.get::<i64>("queued")).unwrap_or(0),
        reviewed_week: row.and_then(|r| r.get::<i64>("reviewed")).unwrap_or(0),
        last_run_at: row.and_then(|r| r.get::<String>("last_run")),
        changes_week: Vec::new(),
    };
    let changes = db
        .query(&Statement::with_values(
            format!(
                "SELECT op, COUNT(*) AS n FROM fact_dream_decisions \
                 WHERE applied = 1 AND decided_at >= ? AND scope IN ({SCOPES}) \
                 GROUP BY op ORDER BY op"
            ),
            vec![text(week_start), text(workspace)],
        ))
        .await?;
    status.changes_week = changes
        .rows
        .iter()
        .filter_map(|row| Some((row.get::<String>("op")?, row.get::<i64>("n").unwrap_or(0))))
        .collect();
    Ok(status)
}
