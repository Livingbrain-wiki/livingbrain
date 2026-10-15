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
use cratefield_core::axum::routing::{get, post, put};
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
use crate::curated;
use crate::jev::{self, JevConnector, JevKey, Source};
use crate::jobs;
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

/// `PATCH /{role}` on a role with no connection (or one whose key no
/// longer opens): there is no key to switch the model on.
const NOT_CONNECTED: ProblemDef = ProblemDef {
    slug: "models/not-connected",
    status: StatusCode::CONFLICT,
    title: "No connection to switch",
    description: "This role has no connection with a usable key.",
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
    /// How Jev's classifier is built, when the venture wires TypeSafe.
    jev: Option<Arc<dyn JevConnector>>,
}

/// The module's routes.
pub(crate) fn router(ctx: ModuleContext, jev: Option<Arc<dyn JevConnector>>) -> axum::Router {
    let state = Arc::new(ModuleState { ctx, jev });
    axum::Router::new()
        .route("/", get(list))
        .route("/discover", post(discover))
        // Jev, the compiler's optional triage classifier: static segments,
        // so they win over `/{role}` the way `/discover` does.
        .route("/jev", get(jev_status).put(jev_set))
        .route("/jev/key", axum::routing::delete(jev_remove_key))
        .route("/jev/test", post(jev_test))
        // A member's own runner (`crate::jobs`): static segments, so they win
        // over `/{role}` the way `/discover` does.
        .route("/mine", get(mine))
        .route("/mine/{role}", put(set_mine).delete(remove_mine))
        // `/discover` above is a static segment, so it wins the match: a
        // `GET /v1/models/discover` is the method router's 405, and every
        // other non-role path reaches `effective`'s role check.
        .route(
            "/{role}",
            get(effective)
                .put(connect)
                .patch(switch_model)
                .delete(remove),
        )
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
    let documented = curated::context(&endpoint.provider, &body.model);
    let probe_result = probe::probe(&*http, target, &body.model, documented)
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

/// What `POST /discover` answers: the models to offer, whether the
/// provider listed them itself (`false` is the curated list, so the picker
/// is never empty), and a sentence to show when it did not.
#[derive(Debug, Serialize)]
struct DiscoverView {
    models: Vec<curated::Model>,
    listed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<&'static str>,
}

/// The note under a curated list.
const NOT_LISTED: &str = "This provider doesn't publish a model list; showing known models.";
/// The note when there is no curated list either.
const NOT_LISTED_NONE_KNOWN: &str =
    "This provider doesn't publish a model list; enter the model id it gives you.";

/// `POST /discover` — the models a provider lists for a key, fetched here so
/// the browser never calls a provider with the key itself. The same guards
/// as a connect: admin or owner only, the SSRF check before any request, and
/// a refusal that names the HTTP status and never the provider's body.
/// Nothing is stored.
///
/// Many Anthropic-compatible endpoints (Z.AI, MiniMax, Kimi, Qwen,
/// DeepSeek's) serve no model list at all, and answer the request with a
/// `404` or a `405`. That is not an error: the answer is the curated list
/// for the provider ([`crate::curated`]) with `listed: false`. Only a
/// refused key (`401`/`403`) is a 422.
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
    let listing = curated::listing(&endpoint.provider);
    let listed = probe::list_models(&*http, target, &listing)
        .await
        .map_err(refused_key)?;
    let view = match listed {
        probe::Listed::Models(models) if !models.is_empty() => DiscoverView {
            models: curated::annotate(&endpoint.provider, models),
            listed: true,
            note: None,
        },
        _ => {
            let models = curated::models(&endpoint.provider);
            let note = if models.is_empty() {
                NOT_LISTED_NONE_KNOWN
            } else {
                NOT_LISTED
            };
            DiscoverView {
                models,
                listed: false,
                note: Some(note),
            }
        }
    };
    Ok(Json(view).into_response())
}

/// The request body for `PATCH /{role}`.
#[derive(Debug, Deserialize)]
struct SwitchRequest {
    model: String,
}

/// `PATCH /{role}` — switch the model of an existing connection, on the key
/// already stored for it, so the settings page can change what Main runs
/// without asking for the key again. Admin or owner only, like a connect;
/// the stored base URL passes the SSRF guard again and the new model is
/// probed before anything is written. The key is decrypted for the probe
/// and dropped; the row keeps its ciphertext, so nothing about the key
/// changes.
async fn switch_model(
    State(state): State<Arc<ModuleState>>,
    Path(role): Path<String>,
    headers: HeaderMap,
    Json(body): Json<SwitchRequest>,
) -> Result<Response, Problem> {
    let clock = port(state.ctx.ports.clock.clone())?;
    let db = port(state.ctx.ports.db.clone())?;
    let http = port(state.ctx.ports.http.clone())?;

    let c = authenticate(&state.ctx.ports, &headers).await?;
    if !c.is_admin {
        return Err(Problem::new(&FORBIDDEN));
    }
    if !ROLES.contains(&role.as_str()) {
        return Err(Problem::validation_failed(format!(
            "role must be one of triage|main|research, got {role:?}"
        )));
    }
    let model = body.model.trim();
    if model.is_empty() || model.len() > 200 || model.chars().any(char::is_control) {
        return Err(Problem::validation_failed(
            "model must be a model id of at most 200 characters",
        ));
    }
    let row = store::get(&*db, &c.workspace_id, &role)
        .await
        .map_err(database)?
        .ok_or_else(|| {
            Problem::new(&NOT_CONNECTED)
                .with_detail("Connect this role before switching its model.")
        })?;
    let (Some(wire), Some(auth)) = (Wire::parse(&row.wire), Auth::parse(&row.auth)) else {
        return Err(Problem::internal());
    };
    let secret = crypto::derive_key(&*state.ctx.config).ok_or_else(|| Problem::new(&NO_SECRET))?;
    let ciphertext = store::key_ciphertext(&*db, &c.workspace_id, &role)
        .await
        .map_err(database)?
        .ok_or_else(Problem::internal)?;
    let key = crypto::decrypt(&secret, &ciphertext, &c.workspace_id, &role)
        .ok()
        .and_then(|key| String::from_utf8(key).ok())
        .ok_or_else(|| {
            Problem::new(&NOT_CONNECTED)
                .with_detail("The stored key could not be opened; connect this role again.")
        })?;

    let url = ssrf::validate_url(&row.base_url).map_err(ssrf_refused)?;
    ssrf::check_destination(&*http, &url)
        .await
        .map_err(ssrf_refused)?;
    let target = probe::Target {
        base: &url,
        wire,
        auth,
        api_key: key.as_str(),
    };
    let documented = curated::context(&row.provider, model);
    let probe_result = probe::probe(&*http, target, model, documented).await;
    drop(key);
    let probe_result = probe_result.map_err(unreachable_error)?;
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

    let now = clock.now().format(&Rfc3339).unwrap_or_default();
    let missing_json =
        serde_json::to_string(&probe_result.missing).unwrap_or_else(|_| "[]".to_owned());
    store::upsert(
        &*db,
        &c.workspace_id,
        &role,
        store::ConnectionFields {
            provider: &row.provider,
            base_url: &row.base_url,
            auth: &row.auth,
            wire: &row.wire,
            model,
            key_ciphertext: &ciphertext,
            key_last4: &row.key_last4,
            fallback_to_managed: row.fallback_to_managed,
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

/// The provider refused the key while listing its models.
const KEY_REFUSED: ProblemDef = ProblemDef {
    slug: "models/key-refused",
    status: StatusCode::UNPROCESSABLE_ENTITY,
    title: "The provider refused the key",
    description: "The provider answered 401 or 403: the API key is wrong, revoked or not allowed.",
};

fn refused_key(error: probe::Unreachable) -> Problem {
    let status = error.0.map_or_else(String::new, |s| format!(" (HTTP {s})"));
    Problem::new(&KEY_REFUSED).with_detail(format!(
        "The provider refused this API key{status}. Check the key and that it belongs to this provider."
    ))
}

fn unreachable_error(error: probe::Unreachable) -> Problem {
    let detail = match error.0 {
        Some(status) => format!("HTTP {status}"),
        None => "the endpoint did not respond or the request failed".to_owned(),
    };
    Problem::new(&UNREACHABLE).with_detail(detail)
}

/// The response view of a member's runner row.
#[derive(Debug, Serialize)]
struct RunnerView {
    role: String,
    kind: String,
    agent: String,
    model: String,
    settings: String,
    updated_at: String,
    last_seen_at: Option<String>,
}

impl From<jobs::Runner> for RunnerView {
    fn from(row: jobs::Runner) -> Self {
        Self {
            role: row.role,
            kind: row.kind,
            agent: row.agent,
            model: row.model,
            settings: row.settings,
            updated_at: row.updated_at,
            last_seen_at: row.last_seen_at,
        }
    }
}

/// `GET /mine` — the caller's own runner rows: which of their jobs run on
/// their own device, and when that device last asked for work.
async fn mine(
    State(state): State<Arc<ModuleState>>,
    headers: HeaderMap,
) -> Result<Response, Problem> {
    let db = port(state.ctx.ports.db.clone())?;
    let c = authenticate(&state.ctx.ports, &headers).await?;
    let rows = jobs::runners(&*db, &c.workspace_id, &c.user_id)
        .await
        .map_err(database)?;
    let runners: Vec<RunnerView> = rows.into_iter().map(RunnerView::from).collect();
    Ok(Json(serde_json::json!({ "runners": runners })).into_response())
}

/// The request body for `PUT /mine/{role}`.
#[derive(Debug, Deserialize)]
struct RunnerRequest {
    kind: String,
    agent: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    settings: Option<String>,
}

/// `PUT /mine/{role}` — run the caller's own jobs for `role` on their own
/// device. Any member may: it spends their subscription, not the
/// workspace's key, and no credential travels — the body names a CLI and a
/// model alias, nothing more.
async fn set_mine(
    State(state): State<Arc<ModuleState>>,
    Path(role): Path<String>,
    headers: HeaderMap,
    Json(body): Json<RunnerRequest>,
) -> Result<Response, Problem> {
    let db = port(state.ctx.ports.db.clone())?;
    let clock = port(state.ctx.ports.clock.clone())?;
    let c = authenticate(&state.ctx.ports, &headers).await?;
    if !ROLES.contains(&role.as_str()) {
        return Err(Problem::validation_failed(format!(
            "role must be one of triage|main|research, got {role:?}"
        )));
    }
    if body.kind != jobs::LOCAL_RUNNER {
        return Err(Problem::validation_failed("kind must be local_runner"));
    }
    if !jobs::AGENTS.contains(&body.agent.as_str()) {
        return Err(Problem::validation_failed("agent must be claude or codex"));
    }
    let model = body.model.trim();
    if model.len() > 128
        || !model.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '/' | '[' | ']')
        })
    {
        return Err(Problem::validation_failed(
            "model must be a model alias or id: letters, digits and - _ . : / [ ]",
        ));
    }
    let settings = body.settings.as_deref().unwrap_or("isolated");
    if !jobs::SETTINGS.contains(&settings) {
        return Err(Problem::validation_failed(
            "settings must be isolated or user",
        ));
    }
    jobs::set_runner(
        &*db,
        clock.now(),
        &c.workspace_id,
        &c.user_id,
        &role,
        &body.agent,
        model,
        settings,
    )
    .await
    .map_err(database)?;
    let row = jobs::runners(&*db, &c.workspace_id, &c.user_id)
        .await
        .map_err(database)?
        .into_iter()
        .find(|row| row.role == role)
        .ok_or_else(Problem::internal)?;
    Ok(Json(RunnerView::from(row)).into_response())
}

/// `DELETE /mine/{role}` — the caller's jobs for `role` go back to the
/// workspace's connection.
async fn remove_mine(
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
    jobs::delete_runner(&*db, &c.workspace_id, &c.user_id, &role)
        .await
        .map_err(database)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/* --- Jev ---------------------------------------------------------------- */

/// `POST /jev/test` while Jev is off for the workspace and no key was sent.
const JEV_OFF: ProblemDef = ProblemDef {
    slug: "models/jev-off",
    status: StatusCode::CONFLICT,
    title: "Jev is off",
    description: "Jev is switched off, or no TypeSafe key is set for this workspace or deployment.",
};

/// TypeSafe refused the test call (a bad key, an unknown model).
const JEV_REFUSED: ProblemDef = ProblemDef {
    slug: "models/jev-refused",
    status: StatusCode::UNPROCESSABLE_ENTITY,
    title: "TypeSafe refused the call",
    description: "TypeSafe refused the classification: the key is wrong or revoked, or the model is unknown.",
};

/// TypeSafe could not be reached, or asked to retry.
const JEV_UNREACHABLE: ProblemDef = ProblemDef {
    slug: "models/jev-unreachable",
    status: StatusCode::BAD_GATEWAY,
    title: "TypeSafe unreachable",
    description: "TypeSafe did not answer the classification; try again later.",
};

/// What every Jev route answers: where the workspace's Jev runs from, and
/// what a member may change. The workspace key shows `…` and four
/// characters, never more.
#[derive(Debug, Serialize)]
struct JevView {
    /// The workspace's switch (on unless someone turned it off).
    enabled: bool,
    /// `off`, `deployment` or `workspace`: what a compile runs on now.
    source: &'static str,
    /// Whether the deployment carries its own `TYPESAFE_API_KEY`.
    deployment_key: bool,
    /// The workspace's own key, as `…abcd`, or `null`.
    workspace_key: Option<String>,
    /// The model Jev runs, and the one it runs when none is chosen.
    model: String,
    default_model: String,
    /// Whether the workspace chose its own model.
    custom_model: bool,
    /// Whether this server can call TypeSafe at all (the venture wired it).
    available: bool,
    updated_at: Option<String>,
}

/// The view for `workspace`, from its row and the connector.
async fn jev_view(state: &ModuleState, workspace: &str) -> Result<JevView, Problem> {
    let db = port(state.ctx.ports.db.clone())?;
    let stored = jev::read(&*db, workspace)
        .await
        .map_err(database)?
        .unwrap_or_default();
    let (deployment_key, default_model) = state.jev.as_ref().map_or((false, String::new()), |c| {
        (c.deployment_key(), c.default_model())
    });
    let source = if state.jev.is_some() {
        jev::decide(stored.enabled, stored.has_key, deployment_key)
    } else {
        Source::Off
    };
    Ok(JevView {
        enabled: stored.enabled != Some(false),
        source: source.as_str(),
        deployment_key,
        workspace_key: stored
            .has_key
            .then(|| format!("…{}", stored.key_last4.unwrap_or_default())),
        model: stored
            .model
            .clone()
            .unwrap_or_else(|| default_model.clone()),
        default_model,
        custom_model: stored.model.is_some(),
        available: state.jev.is_some(),
        updated_at: stored.updated_at,
    })
}

/// `GET /jev` — where the workspace's Jev runs from. Any member may ask.
async fn jev_status(
    State(state): State<Arc<ModuleState>>,
    headers: HeaderMap,
) -> Result<Response, Problem> {
    let c = authenticate(&state.ctx.ports, &headers).await?;
    Ok(Json(jev_view(&state, &c.workspace_id).await?).into_response())
}

/// The body of `PUT /jev`: every field optional, and only what is sent
/// changes. `model: ""` (or `null`) goes back to the default model.
#[derive(Debug, Deserialize)]
struct JevRequest {
    enabled: Option<bool>,
    api_key: Option<crypto::ApiKey>,
    #[serde(default, deserialize_with = "present")]
    model: Option<Option<String>>,
}

/// `Some(None)` for an explicit `null`, so "not sent" and "cleared" differ.
fn present<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(deserializer).map(Some)
}

/// A Jev model id: the adapter sends it as the request's `model`.
fn jev_model(model: &str) -> Result<Option<&str>, Problem> {
    let model = model.trim();
    if model.is_empty() {
        return Ok(None);
    }
    if model.len() > 64
        || !model
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(Problem::validation_failed(
            "model must be a TypeSafe model id: letters, digits and - _ .",
        ));
    }
    Ok(Some(model))
}

/// A TypeSafe key as sent: trimmed, non-empty, printable, bounded.
fn jev_key(key: &crypto::ApiKey) -> Result<crypto::ApiKey, Problem> {
    let trimmed = key.as_str().trim();
    if trimmed.is_empty() || trimmed.len() > 512 || trimmed.chars().any(char::is_control) {
        return Err(Problem::validation_failed(
            "api_key must be the TypeSafe API key, at most 512 characters",
        ));
    }
    Ok(crypto::ApiKey::new(trimmed.to_owned()))
}

/// `PUT /jev` — switch Jev on or off, store or replace the workspace's own
/// TypeSafe key, choose the model. Admin or owner only. The key is sealed
/// before it is stored, exactly like a model connection's, and the answer
/// shows its last four characters only.
async fn jev_set(
    State(state): State<Arc<ModuleState>>,
    headers: HeaderMap,
    Json(body): Json<JevRequest>,
) -> Result<Response, Problem> {
    let db = port(state.ctx.ports.db.clone())?;
    let clock = port(state.ctx.ports.clock.clone())?;
    let id_gen = port(state.ctx.ports.id_gen.clone())?;
    let c = authenticate(&state.ctx.ports, &headers).await?;
    if !c.is_admin {
        return Err(Problem::new(&FORBIDDEN));
    }
    let model = match &body.model {
        None => None,
        Some(None) => Some(None),
        Some(Some(model)) => Some(jev_model(model)?),
    };
    let sealed = match &body.api_key {
        None => None,
        Some(key) => {
            let key = jev_key(key)?;
            let secret =
                crypto::derive_key(&*state.ctx.config).ok_or_else(|| Problem::new(&NO_SECRET))?;
            let blob = crypto::encrypt(
                &secret,
                key.as_str().as_bytes(),
                &c.workspace_id,
                jev::AAD_ROLE,
                &*id_gen,
            )
            .map_err(|_| Problem::internal())?;
            Some((blob.blob, key.last4().to_owned()))
        }
    };
    let now = clock.now().format(&Rfc3339).unwrap_or_default();
    jev::write(
        &*db,
        &c.workspace_id,
        jev::Change {
            enabled: body.enabled,
            model,
            key: sealed
                .as_ref()
                .map(|(blob, last4)| Some((blob.as_slice(), last4.as_str()))),
        },
        &c.user_id,
        &now,
    )
    .await
    .map_err(database)?;
    Ok(Json(jev_view(&state, &c.workspace_id).await?).into_response())
}

/// `DELETE /jev/key` — forget the workspace's own TypeSafe key. Admin or
/// owner only. The switch and the model stay; the deployment's key (if
/// any) serves again.
async fn jev_remove_key(
    State(state): State<Arc<ModuleState>>,
    headers: HeaderMap,
) -> Result<Response, Problem> {
    let db = port(state.ctx.ports.db.clone())?;
    let clock = port(state.ctx.ports.clock.clone())?;
    let c = authenticate(&state.ctx.ports, &headers).await?;
    if !c.is_admin {
        return Err(Problem::new(&FORBIDDEN));
    }
    let now = clock.now().format(&Rfc3339).unwrap_or_default();
    jev::write(
        &*db,
        &c.workspace_id,
        jev::Change {
            key: Some(None),
            ..jev::Change::default()
        },
        &c.user_id,
        &now,
    )
    .await
    .map_err(database)?;
    Ok(Json(jev_view(&state, &c.workspace_id).await?).into_response())
}

/// The body of `POST /jev/test`: optionally a key and a model to try
/// before saving them. Without a key, the workspace's effective Jev is
/// tested.
#[derive(Debug, Deserialize, Default)]
struct JevTestRequest {
    api_key: Option<crypto::ApiKey>,
    model: Option<String>,
}

/// What the test call reads: invented, short, and nothing of anyone's.
const TEST_STATE: &str = "A test call from Living Brain's settings page. A coding agent \
    edited src/lib.rs, ran the test suite, and every test passed.";

/// `POST /jev/test` — one cheap classification call: one yes/no question
/// about a fixed two-line state. Admin or owner only. Nothing is stored;
/// a refusal names the kind of failure, never TypeSafe's response text.
async fn jev_test(
    State(state): State<Arc<ModuleState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, Problem> {
    let db = port(state.ctx.ports.db.clone())?;
    let c = authenticate(&state.ctx.ports, &headers).await?;
    if !c.is_admin {
        return Err(Problem::new(&FORBIDDEN));
    }
    let body: JevTestRequest = if body.is_empty() {
        JevTestRequest::default()
    } else {
        serde_json::from_slice(&body)
            .map_err(|_| Problem::validation_failed("the body is not a Jev test request"))?
    };
    let Some(connector) = state.jev.clone() else {
        return Err(
            Problem::new(&JEV_OFF).with_detail("This server has no TypeSafe adapter wired.")
        );
    };
    let chosen_model = match body.model.as_deref() {
        Some(model) => jev_model(model)?.map(str::to_owned),
        None => None,
    };
    let (source, model, classifier) = if let Some(key) = &body.api_key {
        let key = jev_key(key)?;
        let model = chosen_model.unwrap_or_else(|| connector.default_model());
        let classifier = connector.classifier(JevKey::Workspace(key.as_str()), &model);
        (Source::Workspace, model, classifier)
    } else {
        let effective = jev::effective(&*db, &*state.ctx.config, &c.workspace_id, &*connector)
            .await
            .map_err(database)?;
        if effective.source == Source::Off {
            return Err(Problem::new(&JEV_OFF).with_detail(
                "Jev is off for this workspace: switch it on, or add a TypeSafe key.",
            ));
        }
        match chosen_model {
            // The test names another model than the saved one: the same
            // key, that model.
            Some(model) if model != effective.model => {
                let classifier = if effective.source == Source::Workspace {
                    jev::open_key(&*db, &*state.ctx.config, &c.workspace_id)
                        .await
                        .map_err(database)?
                        .and_then(|key| connector.classifier(JevKey::Workspace(&key), &model))
                } else {
                    connector.classifier(JevKey::Deployment, &model)
                };
                (effective.source, model, classifier)
            }
            _ => (effective.source, effective.model, effective.classifier),
        }
    };
    let classifier = classifier.ok_or_else(|| Problem::new(&JEV_OFF))?;
    let mut questions = std::collections::BTreeMap::new();
    questions.insert(
        "changed_files".to_owned(),
        cratefield_core::Question::Noul {
            instructions: "Did the coding agent change any files in this session?".to_owned(),
        },
    );
    let answers = classifier
        .ask(TEST_STATE, &questions)
        .await
        .map_err(|error| match error {
            cratefield_core::ClassifierError::Rejected(_) => Problem::new(&JEV_REFUSED),
            cratefield_core::ClassifierError::NotConfigured => Problem::new(&JEV_OFF),
            _ => Problem::new(&JEV_UNREACHABLE),
        })?;
    let answer = answers.get("changed_files");
    Ok(Json(serde_json::json!({
        "ok": true,
        "source": source.as_str(),
        "model": answer.and_then(|a| a.model.clone()).unwrap_or(model),
        "answer": answer.map(|a| match &a.value {
            cratefield_core::AnswerValue::Noul(yes) => Value::Bool(*yes),
            cratefield_core::AnswerValue::Choice(label) => Value::String(label.clone()),
            cratefield_core::AnswerValue::Score(score) => serde_json::json!(score),
        }),
        "confidence": answer.map(|a| a.confidence),
    }))
    .into_response())
}
