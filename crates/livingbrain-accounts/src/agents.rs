//! `/v1/agents` (ADR 0004): agent accounts and their API keys.
//!
//! | Route | Who |
//! |---|---|
//! | `GET /` | admins see every agent; a member sees the ones they own |
//! | `POST /` | any member, for themselves; an admin may name another owner |
//! | `GET/PATCH/DELETE /{id}` | the owner, or an admin; only an admin hands one over |
//! | `GET/POST /{id}/keys`, `POST /{id}/keys/{prefix}/rotate`, `DELETE /{id}/keys/{prefix}` | the owner, or an admin |
//!
//! An agent account has no access until a grant gives it some, and it can
//! never sign in or reach these routes itself: they are session-only. A key
//! is returned once, by the call that mints it.

use std::sync::Arc;

use cratefield_core::axum::extract::{Path, State as AxumState};
use cratefield_core::axum::http::{HeaderMap, Method, StatusCode};
use cratefield_core::axum::response::{IntoResponse, Response};
use cratefield_core::axum::routing::{get, post};
use cratefield_core::axum::{self, Json};
use cratefield_core::{
    Config, ConfigError, IdGen, Migrations, Module, ModuleContext, PersonalDataSet, Port, Problem,
};
use cratefield_module_orgs::grants;
use cratefield_module_orgs::service_accounts::{
    self as accounts, KeyInfo, NewKey, NewServiceAccount, ServiceAccount, ServiceAccountError,
};
use livingbrain_pages::access::agent_principal;
use livingbrain_tokens::{AGENT_KEY_BRAND, Member};
use serde::Deserialize;
use serde_json::{Value, json};
use time::Duration;

use crate::audit::{Event, record};
use crate::common::{State, database, forbidden, invalid, not_found, port};

/// The longest a key may live, in days, when one asks for an expiry.
const MAX_KEY_DAYS: i64 = 366;

/// The `agents` module, mounted at `/v1/agents`. Its tables are applied by
/// [`crate::Access`].
#[derive(Debug, Default)]
pub struct Agents;

impl Agents {
    /// The module.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Module for Agents {
    fn name(&self) -> &'static str {
        "agents"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    fn requires(&self) -> &'static [Port] {
        &[Port::Db, Port::Signer, Port::Clock, Port::IdGen]
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
            .route("/", get(list).post(create))
            .route("/{id}", get(one).patch(update).delete(remove))
            .route("/{id}/keys", get(keys).post(mint))
            .route("/{id}/keys/{prefix}", axum::routing::delete(revoke))
            .route("/{id}/keys/{prefix}/rotate", post(rotate))
            .with_state(Arc::new(State { ctx }))
    }
}

fn agent_json(agent: &ServiceAccount) -> Value {
    json!({
        "id": agent.id,
        "name": agent.name,
        "description": agent.description,
        "owner_id": agent.owner_sub,
        "created_by": agent.created_by,
        "created_at": agent.created_at,
        "disabled": agent.disabled_at.is_some(),
        "disabled_at": agent.disabled_at,
    })
}

fn key_json(key: &KeyInfo) -> Value {
    json!({
        "prefix": key.prefix,
        "name": key.name,
        "scopes": (!key.scopes.is_empty())
            .then(|| key.scopes.split_whitespace().collect::<Vec<_>>()),
        "created_at": key.created_at,
        "expires_at": key.expires_at,
        "revoked_at": key.revoked_at,
        "last_used_at": key.last_used_at,
    })
}

fn account_error(error: ServiceAccountError) -> Problem {
    match error {
        ServiceAccountError::Invalid(detail) => invalid(detail),
        ServiceAccountError::NotFound => not_found(),
        ServiceAccountError::Entropy | ServiceAccountError::Db(_) => Problem::internal(),
    }
}

/// The agent `id` in the caller's workspace, if the caller may manage it:
/// its owner, or an admin. Anybody else gets the 404 an unknown id gets.
async fn managed(state: &State, caller: &Member, id: &str) -> Result<ServiceAccount, Problem> {
    let db = state.db()?;
    let agent = accounts::get(&*db, &caller.workspace_id, id)
        .await
        .map_err(database)?
        .ok_or_else(not_found)?;
    if caller.is_admin || agent.owner_sub == caller.user_id {
        Ok(agent)
    } else {
        Err(not_found())
    }
}

async fn audit(state: &State, caller: &Member, action: &str, resource: &str, detail: &str) {
    let (Ok(db), Ok(now), Ok(id_gen)) = (
        state.db(),
        state.now(),
        port::<dyn IdGen>(state.ctx.ports.id_gen.clone()),
    ) else {
        return;
    };
    record(
        &*db,
        &*id_gen,
        now,
        Event {
            workspace_id: &caller.workspace_id,
            actor_kind: "member",
            actor_id: &caller.user_id,
            on_behalf_of: None,
            credential: "session",
            action,
            resource,
            detail,
        },
    )
    .await;
}

/// `GET /`.
async fn list(
    AxumState(state): AxumState<Arc<State>>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, Problem> {
    let caller = state.caller(&method, &headers).await?;
    let db = state.db()?;
    let owner = (!caller.is_admin).then_some(caller.user_id.as_str());
    let agents = accounts::list(&*db, &caller.workspace_id, owner)
        .await
        .map_err(database)?;
    Ok(
        Json(json!({ "agents": agents.iter().map(agent_json).collect::<Vec<_>>() }))
            .into_response(),
    )
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateBody {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    owner_id: Option<String>,
}

/// A live member of the caller's workspace, or the 422 that says it is not.
async fn live_member(state: &State, workspace_id: &str, user_id: &str) -> Result<(), Problem> {
    let db = state.db()?;
    livingbrain_workspaces::caller_for(&*db, workspace_id, user_id)
        .await
        .map_err(database)?
        .map(|_| ())
        .ok_or_else(|| invalid("`owner_id` is not a member of this workspace"))
}

/// `POST /` — create an agent account.
async fn create(
    AxumState(state): AxumState<Arc<State>>,
    method: Method,
    headers: HeaderMap,
    Json(body): Json<CreateBody>,
) -> Result<Response, Problem> {
    let caller = state.caller(&method, &headers).await?;
    let owner = body
        .owner_id
        .as_deref()
        .map(str::trim)
        .filter(|owner| !owner.is_empty())
        .unwrap_or(&caller.user_id)
        .to_owned();
    if owner != caller.user_id {
        if !caller.is_admin {
            return Err(forbidden());
        }
        live_member(&state, &caller.workspace_id, &owner).await?;
    }
    let id = state.id("agt_")?;
    let db = state.db()?;
    let agent = accounts::create(
        &*db,
        &caller.workspace_id,
        &NewServiceAccount {
            id: &id,
            name: &body.name,
            description: &body.description,
            owner_sub: &owner,
            created_by: &caller.user_id,
        },
        state.now()?,
    )
    .await
    .map_err(account_error)?;
    audit(
        &state,
        &caller,
        "agent.create",
        &format!("agent:{id}"),
        &format!("owner={owner}"),
    )
    .await;
    Ok((StatusCode::CREATED, Json(agent_json(&agent))).into_response())
}

/// `GET /{id}`.
async fn one(
    AxumState(state): AxumState<Arc<State>>,
    method: Method,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Problem> {
    let caller = state.caller(&method, &headers).await?;
    let agent = managed(&state, &caller, &id).await?;
    Ok(Json(agent_json(&agent)).into_response())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    disabled: Option<bool>,
    #[serde(default)]
    owner_id: Option<String>,
}

/// `PATCH /{id}` — rename, re-describe, disable or enable, hand over.
async fn update(
    AxumState(state): AxumState<Arc<State>>,
    method: Method,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<UpdateBody>,
) -> Result<Response, Problem> {
    let caller = state.caller(&method, &headers).await?;
    let agent = managed(&state, &caller, &id).await?;
    let db = state.db()?;
    let now = state.now()?;
    if let Some(owner) = body.owner_id.as_deref().map(str::trim)
        && owner != agent.owner_sub
    {
        // Handing an agent over is an admin's call, and to a member.
        if !caller.is_admin {
            return Err(forbidden());
        }
        live_member(&state, &caller.workspace_id, owner).await?;
        accounts::set_owner(&*db, &caller.workspace_id, &id, owner)
            .await
            .map_err(account_error)?;
        audit(
            &state,
            &caller,
            "agent.owner",
            &format!("agent:{id}"),
            &format!("owner={owner}"),
        )
        .await;
    }
    if body.name.is_some() || body.description.is_some() {
        accounts::update(
            &*db,
            &caller.workspace_id,
            &id,
            body.name.as_deref(),
            body.description.as_deref(),
        )
        .await
        .map_err(account_error)?;
        audit(&state, &caller, "agent.update", &format!("agent:{id}"), "").await;
    }
    if let Some(disabled) = body.disabled {
        // An agent whose owner was removed was disabled for that reason; it
        // comes back only once an admin has handed it to a member.
        if !disabled {
            let current = accounts::get(&*db, &caller.workspace_id, &id)
                .await
                .map_err(database)?
                .ok_or_else(not_found)?;
            live_member(&state, &caller.workspace_id, &current.owner_sub).await?;
        }
        accounts::set_disabled(&*db, &caller.workspace_id, &id, disabled, now)
            .await
            .map_err(account_error)?;
        let action = if disabled {
            "agent.disable"
        } else {
            "agent.enable"
        };
        audit(&state, &caller, action, &format!("agent:{id}"), "").await;
    }
    let agent = accounts::get(&*db, &caller.workspace_id, &id)
        .await
        .map_err(database)?
        .ok_or_else(not_found)?;
    Ok(Json(agent_json(&agent)).into_response())
}

/// `DELETE /{id}` — the account, its keys and every grant naming it.
async fn remove(
    AxumState(state): AxumState<Arc<State>>,
    method: Method,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Problem> {
    let caller = state.caller(&method, &headers).await?;
    managed(&state, &caller, &id).await?;
    let db = state.db()?;
    grants::delete_grants_for(&*db, &caller.workspace_id, &agent_principal(&id))
        .await
        .map_err(database)?;
    accounts::delete(&*db, &caller.workspace_id, &id)
        .await
        .map_err(database)?;
    audit(&state, &caller, "agent.delete", &format!("agent:{id}"), "").await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `GET /{id}/keys` — prefixes, labels, scopes and dates; never a key.
async fn keys(
    AxumState(state): AxumState<Arc<State>>,
    method: Method,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Problem> {
    let caller = state.caller(&method, &headers).await?;
    managed(&state, &caller, &id).await?;
    let db = state.db()?;
    let keys = accounts::keys(&*db, &caller.workspace_id, &id)
        .await
        .map_err(database)?;
    Ok(Json(json!({ "keys": keys.iter().map(key_json).collect::<Vec<_>>() })).into_response())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MintBody {
    #[serde(default)]
    name: String,
    #[serde(default)]
    scopes: Option<Vec<String>>,
    #[serde(default)]
    expires_in_days: Option<i64>,
}

/// `POST /{id}/keys` — mint a key. The answer is the only time it exists.
async fn mint(
    AxumState(state): AxumState<Arc<State>>,
    method: Method,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<MintBody>,
) -> Result<Response, Problem> {
    let caller = state.caller(&method, &headers).await?;
    managed(&state, &caller, &id).await?;
    let now = state.now()?;
    let expires_at = match body.expires_in_days {
        None => None,
        Some(days) if (1..=MAX_KEY_DAYS).contains(&days) => Some(now + Duration::days(days)),
        Some(_) => return Err(invalid("`expires_in_days` is 1 to 366")),
    };
    // Each entry is one scope: a space inside one would split into two on
    // the way into the table, and the second half could read wider than the
    // whole was meant to (`read: personal:x` is not `read:personal:x`).
    if body.scopes.as_ref().is_some_and(|list| {
        list.is_empty()
            || list
                .iter()
                .any(|entry| entry.is_empty() || entry.chars().any(char::is_whitespace))
    }) {
        return Err(invalid(
            "`scopes` is a non-empty list of scopes, each without spaces",
        ));
    }
    let scopes = body.scopes.map(|list| list.join(" "));
    let db = state.db()?;
    let minted = accounts::mint_key(
        &*db,
        &caller.workspace_id,
        &id,
        &NewKey {
            brand: AGENT_KEY_BRAND,
            name: &body.name,
            scopes: scopes.as_deref(),
            expires_at,
        },
        now,
    )
    .await
    .map_err(account_error)?;
    audit(
        &state,
        &caller,
        "agent.key.mint",
        &format!("agent:{id}"),
        &format!("prefix={}", minted.info.prefix),
    )
    .await;
    let mut answer = key_json(&minted.info);
    answer["key"] = Value::String(minted.key);
    Ok((StatusCode::CREATED, Json(answer)).into_response())
}

/// `POST /{id}/keys/{prefix}/rotate` — a new key with the old one's label,
/// scopes and expiry; the old one is revoked.
async fn rotate(
    AxumState(state): AxumState<Arc<State>>,
    method: Method,
    headers: HeaderMap,
    Path((id, prefix)): Path<(String, String)>,
) -> Result<Response, Problem> {
    let caller = state.caller(&method, &headers).await?;
    managed(&state, &caller, &id).await?;
    let db = state.db()?;
    let minted = accounts::rotate_key(
        &*db,
        &caller.workspace_id,
        &id,
        &prefix,
        AGENT_KEY_BRAND,
        state.now()?,
    )
    .await
    .map_err(account_error)?;
    audit(
        &state,
        &caller,
        "agent.key.rotate",
        &format!("agent:{id}"),
        &format!("old={prefix} new={}", minted.info.prefix),
    )
    .await;
    let mut answer = key_json(&minted.info);
    answer["key"] = Value::String(minted.key);
    Ok((StatusCode::CREATED, Json(answer)).into_response())
}

/// `DELETE /{id}/keys/{prefix}` — revoke a key; it stops at once.
async fn revoke(
    AxumState(state): AxumState<Arc<State>>,
    method: Method,
    headers: HeaderMap,
    Path((id, prefix)): Path<(String, String)>,
) -> Result<Response, Problem> {
    let caller = state.caller(&method, &headers).await?;
    managed(&state, &caller, &id).await?;
    let db = state.db()?;
    if !accounts::revoke_key(&*db, &caller.workspace_id, &id, &prefix, state.now()?)
        .await
        .map_err(database)?
    {
        return Err(not_found());
    }
    audit(
        &state,
        &caller,
        "agent.key.revoke",
        &format!("agent:{id}"),
        &format!("prefix={prefix}"),
    )
    .await;
    Ok(StatusCode::NO_CONTENT.into_response())
}
