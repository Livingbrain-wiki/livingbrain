//! The module's routes, mounted at `/v1/models`.
//!
//! A member with admin or owner rights connects a model per role, from any
//! provider in the vendored catalog ([`crate::catalog`]) or from a `custom`
//! endpoint that names its own wire and auth; any member can list them and
//! ask which connection serves a role (`GET /{role}`). The key
//! is encrypted before it is stored, and only its last four characters are
//! ever shown — in `GET` and in `PUT`. No route returns the ciphertext, the
//! full key, or a provider's response body, which can echo the key back.
//!
//! `fallback_to_managed` defaults `false` and is only set when a member
//! explicitly sends it: nothing falls back to the managed model silently.

use std::collections::BTreeMap;
use std::sync::Arc;

use cratefield_core::axum::extract::{Path, State};
use cratefield_core::axum::http::{HeaderMap, StatusCode};
use cratefield_core::axum::response::{IntoResponse, Response};
use cratefield_core::axum::routing::{get, post};
use cratefield_core::axum::{self, Json};
use cratefield_core::{DbError, ModuleContext, Problem, ProblemDef};
// The one "who is calling" every route in this venture answers with: a
// session cookie or a personal access token (issue #72).
use livingbrain_tokens::authenticate;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::format_description::well_known::Rfc3339;

use crate::catalog::{self, Auth, Wire};
use crate::crypto;
use crate::probe;
use crate::selection;
use crate::ssrf;
use crate::store;

/// The roles a model can be connected for.
pub(crate) const ROLES: &[&str] = &["triage", "main", "research"];

/// Only a workspace admin or owner can connect or remove models.
const FORBIDDEN: ProblemDef = ProblemDef {
    slug: "models/forbidden",
    status: StatusCode::FORBIDDEN,
    title: "Admin or owner access required",
    description: "Only a workspace admin or owner can connect or remove models.",
};

/// No connection serves the role the caller asked about: with nothing
/// connected, the managed model answers. A 404, not a connection — so a
/// job can tell "Main is serving" from "the managed model is".
const MANAGED: ProblemDef = ProblemDef {
    slug: "models/managed",
    status: StatusCode::NOT_FOUND,
    title: "No model connected",
    description: "No connection serves this role, so the managed model answers.",
};

/// The endpoint URL was refused by the SSRF guard.
const SSRF_REFUSED: ProblemDef = ProblemDef {
    slug: "models/ssrf-refused",
    status: StatusCode::UNPROCESSABLE_ENTITY,
    title: "Endpoint URL refused",
    description: "The endpoint URL is not allowed: it must be HTTPS and resolve to a public address.",
};

/// The model endpoint was unreachable: the first probe call failed.
const UNREACHABLE: ProblemDef = ProblemDef {
    slug: "models/unreachable",
    status: StatusCode::UNPROCESSABLE_ENTITY,
    title: "Model endpoint unreachable",
    description: "The model endpoint did not respond successfully.",
};

/// The encryption secret is missing: keys cannot be stored safely.
const NO_SECRET: ProblemDef = ProblemDef {
    slug: "models/no-encryption-secret",
    status: StatusCode::INTERNAL_SERVER_ERROR,
    title: "Encryption key not configured",
    description: "MODEL_KEYS_SECRET is not set; the key cannot be stored safely.",
};

/// The router state: the module's ports, with the config-derived
/// encryption key resolved lazily per request.
struct ModuleState {
    ctx: ModuleContext,
}

/// The module's routes.
pub(crate) fn router(ctx: ModuleContext) -> axum::Router {
    let state = Arc::new(ModuleState { ctx });
    axum::Router::new()
        .route("/", get(list))
        .route("/discover", post(discover))
        // `/discover` above is a static segment, so it wins the match: a
        // `GET /v1/models/discover` is the method router's 405, and every
        // other non-role path reaches `effective`'s role check.
        .route("/{role}", get(effective).put(connect).delete(remove))
        .with_state(state)
}

/// A port the module declared and the runtime did not supply.
fn port<T: ?Sized>(slot: Option<Arc<T>>) -> Result<Arc<T>, Problem> {
    slot.ok_or_else(Problem::internal)
}

/// A failed statement. The adapter's message never reaches the body.
fn database(_error: DbError) -> Problem {
    Problem::internal()
}

/// `GET /` — every model connection in the caller's workspace. Any member
/// can list them; the key column shows `…` followed by the last four
/// characters only.
async fn list(
    State(state): State<Arc<ModuleState>>,
    headers: HeaderMap,
) -> Result<Response, Problem> {
    let db = port(state.ctx.ports.db.clone())?;
    let c = authenticate(&state.ctx.ports, &headers).await?;
    let rows = store::list(&*db, &c.workspace_id).await.map_err(database)?;
    Ok(Json(rows.into_iter().map(view).collect::<Vec<_>>()).into_response())
}

/// `GET /{role}` — the connection that serves a job for `role`. Any member
/// can ask, as with [`list`]. The resolution is [`crate::selection`]'s
/// product rule: the role's own connection if it has one, else Main.
///
/// The body's `role` is the *serving* connection's role, not the one the
/// caller asked about: a triage job with no triage connection gets the
/// Main connection back, `role: "main"` — the answer to "which model will
/// run this job". When nothing serves the role at all the answer is the
/// managed model's, and the response is a 404 naming `models/managed`
/// rather than a body that looked like a connection.
async fn effective(
    State(state): State<Arc<ModuleState>>,
    Path(role): Path<String>,
    headers: HeaderMap,
) -> Result<Response, Problem> {
    let db = port(state.ctx.ports.db.clone())?;
    let c = authenticate(&state.ctx.ports, &headers).await?;
    if !ROLES.contains(&role.as_str()) {
        return Err(Problem::validation_failed(format!(
            "role must be one of triage|main|research, got {role:?}"
        )));
    }
    let rows = store::list(&*db, &c.workspace_id).await.map_err(database)?;
    let serving = selection::resolve(&rows, &role).ok_or_else(|| {
        Problem::new(&MANAGED).with_detail("No model connected — the managed model answers.")
    })?;
    Ok(Json(view(serving.clone())).into_response())
}

/// `PUT /{role}` — connect (or replace) a model for a role. Admin or owner
/// only.
async fn connect(
    State(state): State<Arc<ModuleState>>,
    Path(role): Path<String>,
    headers: HeaderMap,
    Json(body): Json<ConnectRequest>,
) -> Result<Response, Problem> {
    let clock = port(state.ctx.ports.clock.clone())?;
    let db = port(state.ctx.ports.db.clone())?;
    let http = port(state.ctx.ports.http.clone())?;
    let id_gen = port(state.ctx.ports.id_gen.clone())?;

    let c = authenticate(&state.ctx.ports, &headers).await?;
    if !c.is_admin {
        return Err(Problem::new(&FORBIDDEN));
    }
    if !ROLES.contains(&role.as_str()) {
        return Err(Problem::validation_failed(format!(
            "role must be one of triage|main|research, got {role:?}"
        )));
    }

    let endpoint = resolve(&body.endpoint)?;
    let api_key = body.api_key;

    // SSRF: validate the URL structure, then resolve the host via DoH
    // and reject if any address is private. Everything after this point —
    // the probe, and the base URL stored on the row — uses `url`, the
    // parsed and normalized form the guard just approved, never the raw
    // string a member sent.
    let url = ssrf::validate_url(&endpoint.base_url).map_err(ssrf_refused)?;
    ssrf::check_destination(&*http, &url)
        .await
        .map_err(ssrf_refused)?;
    let base_url = url.as_str().to_owned();

    // Probe: send a few small requests to check tool calling, JSON output
    // and context size, over the provider's own wire and auth. If the
    // first call fails, refuse with 422 naming the HTTP status only —
    // never the provider's body.
    let target = probe::Target {
        base: &url,
        wire: endpoint.wire,
        auth: endpoint.auth,
        api_key: api_key.as_str(),
    };
    let probe_result = probe::probe(&*http, target, &body.model)
        .await
        .map_err(unreachable_error)?;

    // The ledger (issue #12): the probe's tokens are real usage by this
    // workspace; the failure is ignored on purpose — a lost usage count
    // must never fail the connect that spent the tokens.
    if let Some(usage) = probe_result.usage {
        let _ = livingbrain_usage::record_model(
            &*db,
            clock.now(),
            &c.workspace_id,
            usage.prompt_tokens,
            usage.completion_tokens,
        )
        .await;
    }

    // Encrypt the key. If the secret is missing, fail with a 500 rather
    // than storing plaintext.
    let enc_key = crypto::derive_key(&*state.ctx.config).ok_or_else(|| Problem::new(&NO_SECRET))?;
    let ciphertext = crypto::encrypt(
        &enc_key,
        api_key.as_str().as_bytes(),
        &c.workspace_id,
        &role,
        &*id_gen,
    )
    .map_err(|_| Problem::internal())?;

    let now = clock.now().format(&Rfc3339).unwrap_or_default();
    let missing_json =
        serde_json::to_string(&probe_result.missing).unwrap_or_else(|_| "[]".to_owned());

    store::upsert(
        &*db,
        &c.workspace_id,
        &role,
        store::ConnectionFields {
            provider: &endpoint.provider,
            base_url: &base_url,
            auth: endpoint.auth.as_str(),
            wire: endpoint.wire.as_str(),
            model: &body.model,
            key_ciphertext: &ciphertext.blob,
            key_last4: api_key.last4(),
            fallback_to_managed: body.fallback_to_managed.unwrap_or(false),
            status: probe_result.status,
            missing: &missing_json,
            context_size: probe_result.context_size.as_deref(),
            checked_at: &now,
            updated_by: &c.user_id,
        },
    )
    .await
    .map_err(database)?;

    let row = store::get(&*db, &c.workspace_id, &role)
        .await
        .map_err(database)?
        .ok_or_else(Problem::internal)?;
    Ok(Json(view(row)).into_response())
}

/// `DELETE /{role}` — remove a model connection. Admin or owner only.
async fn remove(
    State(state): State<Arc<ModuleState>>,
    Path(role): Path<String>,
    headers: HeaderMap,
) -> Result<Response, Problem> {
    let db = port(state.ctx.ports.db.clone())?;

    let c = authenticate(&state.ctx.ports, &headers).await?;
    if !c.is_admin {
        return Err(Problem::new(&FORBIDDEN));
    }
    if !ROLES.contains(&role.as_str()) {
        return Err(Problem::validation_failed(format!(
            "role must be one of triage|main|research, got {role:?}"
        )));
    }
    store::delete(&*db, &c.workspace_id, &role)
        .await
        .map_err(database)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Where a connection goes: the provider id it is stored under, the base
/// URL (variables filled, not yet SSRF-checked), and how to talk to it.
struct Endpoint {
    provider: String,
    base_url: String,
    auth: Auth,
    wire: Wire,
}

/// The provider half of a connect or a discover request.
#[derive(Debug, Deserialize)]
struct EndpointRequest {
    /// A catalog id, or `custom`.
    provider: String,
    /// Required for `custom`. For a catalog provider it replaces the
    /// catalog's base URL — the "edit" a member makes when their account
    /// lives on a regional or dedicated host — and is SSRF-checked the same.
    base_url: Option<String>,
    /// Values for a catalog base URL's `${VAR}` placeholders.
    #[serde(default)]
    variables: BTreeMap<String, String>,
    /// `custom` only: `bearer` (the default) or `x-api-key`. Ignored for a
    /// catalog provider, whose auth the catalog decides.
    auth: Option<String>,
    /// `custom` only: `openai` (the default, which is what a custom endpoint
    /// meant before the catalog) or `anthropic`. Ignored for a catalog
    /// provider.
    wire: Option<String>,
}

/// Resolves a provider to an [`Endpoint`]. A catalog provider takes its
/// auth and wire from the catalog and its base URL from the catalog (with
/// variables filled) unless the member sent one; `custom` needs a base URL
/// and may name its wire and auth; anything else is refused.
fn resolve(request: &EndpointRequest) -> Result<Endpoint, Problem> {
    let sent_base = request
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty());
    if request.provider == "custom" {
        let base_url = sent_base.ok_or_else(|| {
            Problem::validation_failed("base_url is required for the custom provider")
        })?;
        let wire = match request.wire.as_deref() {
            None => Wire::Openai,
            Some(wire) => Wire::parse(wire)
                .ok_or_else(|| Problem::validation_failed("wire must be openai or anthropic"))?,
        };
        let auth = match request.auth.as_deref() {
            None => Auth::Bearer,
            Some(auth) => Auth::parse(auth)
                .ok_or_else(|| Problem::validation_failed("auth must be bearer or x-api-key"))?,
        };
        return Ok(Endpoint {
            provider: "custom".to_owned(),
            base_url: base_url.to_owned(),
            auth,
            wire,
        });
    }
    let entry = catalog::find(&request.provider).ok_or_else(|| {
        Problem::validation_failed(format!("unknown provider: {}", request.provider))
    })?;
    let base_url = match sent_base {
        Some(url) => url.to_owned(),
        None => catalog::fill(entry, &request.variables).map_err(Problem::validation_failed)?,
    };
    Ok(Endpoint {
        provider: entry.id.clone(),
        base_url,
        auth: entry.auth,
        wire: entry.wire,
    })
}

/// The request body for `POST /discover`.
#[derive(Debug, Deserialize)]
struct DiscoverRequest {
    #[serde(flatten)]
    endpoint: EndpointRequest,
    api_key: crypto::ApiKey,
}

#[derive(Debug, Serialize)]
struct DiscoverView {
    models: Vec<String>,
}

/// `POST /discover` — the models a provider lists for a key, fetched here so
/// the browser never calls a provider with the key itself. The same guards
/// as a connect: admin or owner only, the SSRF check before any request, and
/// a refusal that names the HTTP status and never the provider's body.
/// Nothing is stored.
async fn discover(
    State(state): State<Arc<ModuleState>>,
    headers: HeaderMap,
    Json(body): Json<DiscoverRequest>,
) -> Result<Response, Problem> {
    let http = port(state.ctx.ports.http.clone())?;
    let c = authenticate(&state.ctx.ports, &headers).await?;
    if !c.is_admin {
        return Err(Problem::new(&FORBIDDEN));
    }
    let endpoint = resolve(&body.endpoint)?;
    let url = ssrf::validate_url(&endpoint.base_url).map_err(ssrf_refused)?;
    ssrf::check_destination(&*http, &url)
        .await
        .map_err(ssrf_refused)?;
    let target = probe::Target {
        base: &url,
        wire: endpoint.wire,
        auth: endpoint.auth,
        api_key: body.api_key.as_str(),
    };
    let models = probe::list_models(&*http, target)
        .await
        .map_err(unreachable_error)?;
    Ok(Json(DiscoverView { models }).into_response())
}

/// The request body for `PUT /{role}`. `Debug` is safe because the key
/// arrives already wrapped in an [`ApiKey`](crate::crypto::ApiKey), whose
/// `Debug` shows four characters and a `…`.
#[derive(Debug, Deserialize)]
struct ConnectRequest {
    #[serde(flatten)]
    endpoint: EndpointRequest,
    api_key: crypto::ApiKey,
    model: String,
    /// Defaults to `false`: nothing falls back to the managed model
    /// silently.
    fallback_to_managed: Option<bool>,
}

/// The response view of a model connection. The key column shows `…abcd`
/// only — never the ciphertext, never the full key.
#[derive(Debug, Serialize)]
struct ConnectionView {
    role: String,
    provider: String,
    base_url: String,
    auth: String,
    wire: String,
    model: String,
    key: String,
    status: String,
    missing: Value,
    context_size: Option<String>,
    fallback_to_managed: bool,
    checked_at: String,
}

fn view(row: store::ModelConnection) -> ConnectionView {
    let missing: Value = serde_json::from_str(&row.missing).unwrap_or(Value::Array(vec![]));
    ConnectionView {
        role: row.role,
        provider: row.provider,
        base_url: row.base_url,
        auth: row.auth,
        wire: row.wire,
        model: row.model,
        key: format!("…{}", row.key_last4),
        status: row.status,
        missing,
        context_size: row.context_size,
        fallback_to_managed: row.fallback_to_managed,
        checked_at: row.checked_at,
    }
}

fn ssrf_refused(error: ssrf::SsrfError) -> Problem {
    Problem::new(&SSRF_REFUSED).with_detail(error.to_string())
}

fn unreachable_error(error: probe::Unreachable) -> Problem {
    let detail = match error.0 {
        Some(status) => format!("HTTP {status}"),
        None => "the endpoint did not respond or the request failed".to_owned(),
    };
    Problem::new(&UNREACHABLE).with_detail(detail)
}
