//! The audit log (ADR 0004): [`record`], which every module writes through,
//! and the `audit` module's one read route, `GET /v1/audit`, for owners and
//! admins.

use std::collections::HashMap;
use std::sync::Arc;

use cratefield_core::axum::extract::{Query, State as AxumState};
use cratefield_core::axum::http::{HeaderMap, Method};
use cratefield_core::axum::response::{IntoResponse, Response};
use cratefield_core::axum::routing::get;
use cratefield_core::axum::{self, Json};
use cratefield_core::{
    Config, ConfigError, Database, IdGen, Migrations, Module, ModuleContext, PersonalDataSet, Port,
    Problem,
};
use cratefield_module_orgs::audit::{self, AuditEntry};
use serde_json::json;
use time::OffsetDateTime;

use crate::common::{State, database, forbidden};

/// One thing worth recording.
#[derive(Debug, Clone)]
pub struct Event<'a> {
    /// The workspace.
    pub workspace_id: &'a str,
    /// `member` or `agent`.
    pub actor_kind: &'a str,
    /// The member or agent account id.
    pub actor_id: &'a str,
    /// The member an agent acted for.
    pub on_behalf_of: Option<&'a str>,
    /// The credential's public prefix, or `session`.
    pub credential: &'a str,
    /// A dotted verb: `agent.create`, `grant.delete`, `page.write`, …
    pub action: &'a str,
    /// What it touched.
    pub resource: &'a str,
    /// Anything else worth keeping, short. Never a secret, never page text.
    pub detail: &'a str,
}

/// Appends `event` to the audit log. Best effort by design: a lost audit
/// line must never undo the action it describes, which has already happened
/// by the time this runs — the failure is logged, not raised.
pub async fn record(db: &dyn Database, id_gen: &dyn IdGen, now: OffsetDateTime, event: Event<'_>) {
    let entry = AuditEntry {
        id: id_gen.ulid(),
        org_id: event.workspace_id.to_owned(),
        actor_kind: event.actor_kind.to_owned(),
        actor_id: event.actor_id.to_owned(),
        on_behalf_of: event.on_behalf_of.map(str::to_owned),
        credential: event.credential.to_owned(),
        action: event.action.to_owned(),
        resource: event.resource.to_owned(),
        detail: event.detail.to_owned(),
        at: String::new(),
    };
    if audit::record(db, &entry, now).await.is_err() {
        eprintln!("an audit entry could not be written: {}", event.action);
    }
}

/// The `audit` module: `GET /v1/audit`. Its table is applied by
/// [`crate::Access`].
#[derive(Debug, Default)]
pub struct Audit;

impl Audit {
    /// The module.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Module for Audit {
    fn name(&self) -> &'static str {
        "audit"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    fn requires(&self) -> &'static [Port] {
        &[Port::Db, Port::Signer, Port::Clock]
    }

    fn tables(&self) -> &'static [&'static str] {
        &[]
    }

    fn personal_data(&self) -> &'static [PersonalDataSet] {
        &[]
    }

    fn validate_config(&self, _cfg: &dyn Config) -> Result<(), ConfigError> {
        Ok(())
    }

    fn migrations(&self) -> Migrations {
        Migrations::sqlite(&[])
    }

    fn router(&self, ctx: ModuleContext) -> axum::Router {
        axum::Router::new()
            .route("/", get(list))
            .with_state(Arc::new(State { ctx }))
    }
}

/// `GET /?before=&limit=` — the workspace's log, newest first. Owners and
/// admins only.
async fn list(
    AxumState(state): AxumState<Arc<State>>,
    method: Method,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, Problem> {
    let caller = state.caller(&method, &headers).await?;
    if !caller.is_admin {
        return Err(forbidden());
    }
    let limit = query
        .get("limit")
        .and_then(|raw| raw.parse::<usize>().ok())
        .unwrap_or(50);
    let before = query
        .get("before")
        .map(String::as_str)
        .filter(|id| !id.is_empty() && id.len() <= 64);
    let db = state.db()?;
    let entries = audit::list(&*db, &caller.workspace_id, before, limit)
        .await
        .map_err(database)?;
    let next = (entries.len() >= limit.clamp(1, audit::MAX_PAGE))
        .then(|| entries.last().map(|entry| entry.id.clone()))
        .flatten();
    Ok(Json(json!({
        "entries": entries.iter().map(|entry| json!({
            "id": entry.id,
            "at": entry.at,
            "actor": { "kind": entry.actor_kind, "id": entry.actor_id },
            "on_behalf_of": entry.on_behalf_of,
            "credential": entry.credential,
            "action": entry.action,
            "resource": entry.resource,
            "detail": entry.detail,
        })).collect::<Vec<_>>(),
        "next": next,
    }))
    .into_response())
}
