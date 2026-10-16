//! The `dreaming` module: a workspace's dreaming settings and what it did
//! (`docs/temporal-facts.md`, section 9).
//!
//! - `GET /v1/dreaming` — any member: whether dreaming is on, whether the
//!   quick rules run, the model it calls and the models this workspace could
//!   choose ([`MODEL_ROLES`], usable only when connected), and a status
//!   strip: subjects queued, subjects reviewed and changes applied in the
//!   last seven days, the last run.
//! - `PUT /v1/dreaming` — owner or admin: a partial body of `enabled`,
//!   `rules_enabled` and `model` (`main`, `triage` or `research`, and only a
//!   connected one). Nothing else is written.
//! - `GET /v1/dreaming/decisions?limit=&before=` — any member: the decision
//!   log of the scopes the caller may read, newest first, with the fact
//!   each decision was about (attribute and value), the reason, whether it
//!   applied and when. `before` is the `next` of the previous page.
//! - `POST /v1/dreaming/run` — owner or admin: asks the next cron tick to
//!   read this workspace's subjects. Nothing is computed here; a workspace
//!   with dreaming off answers 409.
//!
//! Every route answers to a bearer or the web app's session cookie, and an
//! agent account is refused: these settings act for a member. A cookie write
//! must pass the same-origin check ([`Service::asker`]).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use cratefield_core::axum::Json;
use cratefield_core::axum::extract::{Query, State};
use cratefield_core::axum::http::{HeaderMap, Method, StatusCode};
use cratefield_core::axum::routing::{get, post};
use cratefield_core::{
    Config, ConfigError, Migrations, Module, ModuleContext, Port, Problem, ProblemDef,
};
use cratefield_module_orgs::grants::Permission;
use livingbrain_compiler::dream_settings::{self, DreamSettings};
use livingbrain_models::invoke;
use livingbrain_pages::access::{KIND_FACTS, subject_path};
use livingbrain_pages::{DreamDecisionRecord, FactStore};
use serde::Deserialize;
use serde_json::{Value, json};
use time::Duration;
use time::format_description::well_known::Rfc3339;

use crate::Wiki;
use crate::facts::scope_label;
use crate::problems::fact_error;
use crate::service::Service;

/// The job slots a dream may call, in the order the page lists them.
pub const MODEL_ROLES: [&str; 3] = ["main", "triage", "research"];
/// The most decisions one page of the log lists.
const MAX_PAGE: usize = 100;
/// The page a request without `limit` gets.
const DEFAULT_PAGE: usize = 50;

/// Only an owner or an admin changes a workspace's dreaming.
const FORBIDDEN: ProblemDef = ProblemDef {
    slug: "dreaming/forbidden",
    status: StatusCode::FORBIDDEN,
    title: "Owner or admin access required",
    description: "Only a workspace owner or admin can change how dreaming runs. Members can \
                  read the settings and the log.",
};

/// A model the workspace has not connected: a dream could not call it.
const MODEL_NOT_CONNECTED: ProblemDef = ProblemDef {
    slug: "dreaming/model-not-connected",
    status: StatusCode::UNPROCESSABLE_ENTITY,
    title: "That model is not connected here",
    description: "Dreaming can only use a model this workspace has connected in Settings \
                  → Models. Nothing was saved.",
};

/// A run asked of a workspace with dreaming off.
const DREAMING_OFF: ProblemDef = ProblemDef {
    slug: "dreaming/off",
    status: StatusCode::CONFLICT,
    title: "Dreaming is off",
    description: "Turn dreaming on before asking it to run. Nothing was queued.",
};

/// The `dreaming` module, constructed with the page access the venture
/// injected ([`Wiki`]), as the facts module is.
pub struct Dreaming {
    wiki: Wiki,
}

impl Dreaming {
    /// A dreaming module over the given page access.
    #[must_use]
    pub fn new(wiki: Wiki) -> Self {
        Self { wiki }
    }
}

impl Module for Dreaming {
    fn name(&self) -> &'static str {
        "dreaming"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    /// The same ports as the facts module it reads through; the table is the
    /// coding-agents module's (`dreaming_settings`), so this module migrates
    /// nothing.
    fn requires(&self) -> &'static [Port] {
        &[Port::Db, Port::Blob, Port::Clock, Port::IdGen]
    }

    fn optional(&self) -> &'static [Port] {
        &[Port::Signer]
    }

    fn migrations(&self) -> Migrations {
        Migrations::EMPTY
    }

    fn validate_config(&self, _cfg: &dyn Config) -> Result<(), ConfigError> {
        Ok(())
    }

    fn router(&self, ctx: ModuleContext) -> cratefield_core::axum::Router {
        let state = Arc::new(Service::from_wiki(ctx, &self.wiki));
        cratefield_core::axum::Router::new()
            .route("/", get(read).put(update))
            .route("/decisions", get(decisions))
            .route("/run", post(run))
            .with_state(state)
    }
}

/// A `PUT /v1/dreaming` body: any of the three settings, the rest unchanged.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Patch {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    rules_enabled: Option<bool>,
    #[serde(default)]
    model: Option<String>,
}

/// Whether the caller may change the workspace's dreaming: an owner or an
/// admin, by a credential that is not narrowed to some scopes.
async fn is_admin(state: &Service, headers: &HeaderMap) -> bool {
    livingbrain_tokens::authenticate(&state.ctx().ports, headers)
        .await
        .is_ok_and(|member| member.is_admin)
}

/// `GET /`.
async fn read(
    State(state): State<Arc<Service>>,
    method: Method,
    headers: HeaderMap,
) -> Result<Json<Value>, Problem> {
    let asker = state.asker(&method, &headers).await?;
    Service::member_only(&asker)?;
    let admin = is_admin(&state, &headers).await;
    view(&state, &asker.workspace_id, admin).await.map(Json)
}

/// `PUT /`.
async fn update(
    State(state): State<Arc<Service>>,
    method: Method,
    headers: HeaderMap,
    Json(patch): Json<Patch>,
) -> Result<Json<Value>, Problem> {
    let asker = state.asker(&method, &headers).await?;
    Service::member_only(&asker)?;
    if !is_admin(&state, &headers).await {
        return Err(Problem::new(&FORBIDDEN));
    }
    let ctx = state.ctx();
    let db = crate::port(ctx.ports.db.clone())?;
    let clock = crate::port(ctx.ports.clock.clone())?;
    let workspace = asker.workspace_id.as_str();
    let mut next = dream_settings::load(&*db, workspace)
        .await
        .map_err(|_| Problem::internal())?;
    if let Some(enabled) = patch.enabled {
        next.enabled = enabled;
    }
    if let Some(enabled) = patch.rules_enabled {
        next.rules_enabled = enabled;
    }
    if let Some(role) = patch.model {
        if !usable(&*db, workspace, &role).await? {
            return Err(Problem::new(&MODEL_NOT_CONNECTED));
        }
        next.model_role = role;
    }
    dream_settings::save(&*db, workspace, clock.now(), &asker.user_id, &next)
        .await
        .map_err(|_| Problem::internal())?;
    view(&state, workspace, true).await.map(Json)
}

/// `GET /decisions`.
async fn decisions(
    State(state): State<Arc<Service>>,
    method: Method,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<Value>, Problem> {
    let asker = state.asker(&method, &headers).await?;
    Service::member_only(&asker)?;
    let limit = query
        .get("limit")
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_PAGE)
        .clamp(1, MAX_PAGE);
    let before = match query.get("before").map(|raw| raw.trim()) {
        None | Some("") => None,
        Some(raw) => Some(parse_cursor(raw)?),
    };
    let store = state.facts_store()?;
    let plan = state.access(&asker).await?;
    let names = plan.scopes(KIND_FACTS, Permission::Read);
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let before_refs = before
        .as_ref()
        .map(|(at, scope, seq)| (at.as_str(), scope.as_str(), *seq));
    let mut rows = store
        .dream_decisions(&refs, before_refs, limit + 1)
        .await
        .map_err(fact_error)?;
    let more = rows.len() > limit;
    rows.truncate(limit);
    let next = if more { rows.last().map(cursor) } else { None };
    let visible: Vec<&DreamDecisionRecord> = rows
        .iter()
        .filter(|row| {
            plan.allows(
                KIND_FACTS,
                &row.scope,
                &subject_path(&row.subject),
                Permission::Read,
            )
        })
        .collect();
    let facts = fact_lookup(&store, &visible).await?;
    let items: Vec<Value> = visible
        .iter()
        .map(|row| {
            let fact = facts.get(&(row.scope.clone(), row.fact_id.clone()));
            let by = row
                .by_fact_id
                .as_ref()
                .and_then(|id| facts.get(&(row.scope.clone(), id.clone())));
            json!({
                "at": row.decided_at,
                "op": row.op,
                "applied": row.applied,
                "reason": row.reason,
                "subject": row.subject,
                "where": scope_label(&row.scope),
                "fact": fact.cloned(),
                "by": by.cloned(),
                "new_attribute": row.new_attribute,
                "new_valid_to": row.new_valid_to,
                "model": row.model,
            })
        })
        .collect();
    Ok(Json(json!({ "decisions": items, "next": next })))
}

/// `POST /run`.
async fn run(
    State(state): State<Arc<Service>>,
    method: Method,
    headers: HeaderMap,
) -> Result<(StatusCode, Json<Value>), Problem> {
    let asker = state.asker(&method, &headers).await?;
    Service::member_only(&asker)?;
    if !is_admin(&state, &headers).await {
        return Err(Problem::new(&FORBIDDEN));
    }
    let ctx = state.ctx();
    let db = crate::port(ctx.ports.db.clone())?;
    let clock = crate::port(ctx.ports.clock.clone())?;
    let workspace = asker.workspace_id.as_str();
    let settings = dream_settings::load(&*db, workspace)
        .await
        .map_err(|_| Problem::internal())?;
    if !settings.enabled {
        return Err(Problem::new(&DREAMING_OFF));
    }
    dream_settings::request_run(&*db, workspace, clock.now())
        .await
        .map_err(|_| Problem::internal())?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "requested": true,
            "message": "Dreaming will read this workspace's facts at its next run."
        })),
    ))
}

/// The page's answer: the settings, the models they could choose, and the
/// status strip.
async fn view(state: &Service, workspace: &str, editable: bool) -> Result<Value, Problem> {
    let ctx = state.ctx();
    let db = crate::port(ctx.ports.db.clone())?;
    let clock = crate::port(ctx.ports.clock.clone())?;
    let settings: DreamSettings = dream_settings::load(&*db, workspace)
        .await
        .map_err(|_| Problem::internal())?;
    let mut models = Vec::new();
    let mut current = json!({ "role": settings.model_role, "usable": false });
    for role in MODEL_ROLES {
        let serving = invoke::serving(&*db, workspace, role)
            .await
            .map_err(|_| Problem::internal())?
            .filter(|serving| serving.role == role);
        let entry = match &serving {
            Some(serving) => json!({
                "role": role,
                "usable": true,
                "provider": serving.provider,
                "model": serving.model,
            }),
            None => json!({ "role": role, "usable": false }),
        };
        if role == settings.model_role {
            current = entry.clone();
        }
        models.push(entry);
    }
    let now = clock.now();
    let week_start = (now - Duration::days(7))
        .format(&Rfc3339)
        .unwrap_or_default();
    let status = dream_settings::status(&*db, workspace, &week_start)
        .await
        .map_err(|_| Problem::internal())?;
    Ok(json!({
        "enabled": settings.enabled,
        "rules_enabled": settings.rules_enabled,
        "model": current,
        "models": models,
        "editable": editable,
        "status": {
            "queued": status.queued,
            "reviewed_week": status.reviewed_week,
            "last_run_at": status.last_run_at,
            "changes_week": status
                .changes_week
                .iter()
                .map(|(op, n)| (op.clone(), json!(n)))
                .collect::<serde_json::Map<String, Value>>(),
            "run_requested": settings.run_requested_at.is_some(),
        },
        "now": now.format(&Rfc3339).unwrap_or_default(),
    }))
}

/// Whether `role` is one a dream may call here: a job slot that resolves to
/// a connection of its own (Main, or Triage or Research when connected).
async fn usable(
    db: &dyn cratefield_core::Database,
    workspace: &str,
    role: &str,
) -> Result<bool, Problem> {
    if !MODEL_ROLES.contains(&role) {
        return Err(Problem::validation_failed(
            "`model` must be main, triage or research",
        ));
    }
    Ok(invoke::serving(db, workspace, role)
        .await
        .map_err(|_| Problem::internal())?
        .is_some_and(|serving| serving.role == role))
}

/// The `(dream_at, scope, seq)` a page continues from.
fn parse_cursor(raw: &str) -> Result<(String, String, i64), Problem> {
    let parts: Vec<&str> = raw.splitn(3, '|').collect();
    match parts.as_slice() {
        [at, scope, seq] if !at.is_empty() && !scope.is_empty() => seq
            .parse::<i64>()
            .map(|seq| ((*at).to_owned(), (*scope).to_owned(), seq))
            .map_err(|_| bad_cursor()),
        _ => Err(bad_cursor()),
    }
}

fn bad_cursor() -> Problem {
    Problem::validation_failed("`before` is not a cursor from this log")
}

/// The cursor of a row: its pass, scope and position.
fn cursor(row: &DreamDecisionRecord) -> String {
    format!("{}|{}|{}", row.dream_at, row.scope, row.seq)
}

/// The attribute and value of each fact the rows name, read from the
/// subjects' timelines in the rows' own scopes (so only facts the caller can
/// read, since the scopes are the caller's).
async fn fact_lookup(
    store: &FactStore,
    rows: &[&DreamDecisionRecord],
) -> Result<HashMap<(String, String), Value>, Problem> {
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut facts = HashMap::new();
    for row in rows {
        if !seen.insert((row.scope.clone(), row.subject.clone())) {
            continue;
        }
        for timeline in store
            .timelines(&[row.scope.as_str()], Some(&row.subject), None)
            .await
            .map_err(fact_error)?
        {
            for fact in timeline.facts {
                facts.insert(
                    (row.scope.clone(), fact.id.clone()),
                    json!({
                        "id": fact.id,
                        "attribute": fact.attribute,
                        "value": fact.value,
                    }),
                );
            }
        }
    }
    Ok(facts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cursor_round_trips_and_a_bad_one_is_refused() {
        let row = DreamDecisionRecord {
            scope: "user-abc".to_owned(),
            dream_at: "2026-10-16T10:00:00Z".to_owned(),
            seq: 3,
            subject: "project/demo".to_owned(),
            op: "end".to_owned(),
            fact_id: "fact-1".to_owned(),
            by_fact_id: None,
            new_valid_to: None,
            new_attribute: None,
            reason: None,
            applied: true,
            model: "main".to_owned(),
            decided_at: "2026-10-16T10:00:01Z".to_owned(),
        };
        let encoded = cursor(&row);
        let (at, scope, seq) = parse_cursor(&encoded).expect("a cursor");
        assert_eq!(
            (at.as_str(), scope.as_str(), seq),
            (row.dream_at.as_str(), "user-abc", 3)
        );
        assert!(parse_cursor("not-a-cursor").is_err());
        assert!(parse_cursor("2026|scope|x").is_err());
        assert!(parse_cursor("|scope|1").is_err());
    }

    #[test]
    fn the_model_slots_are_the_three_the_models_module_serves() {
        assert_eq!(MODEL_ROLES, ["main", "triage", "research"]);
    }
}
