//! The module's routes, mounted at `/v1/models`.
//!
//! A member with admin or owner rights connects a provider key or a custom
//! OpenAI-compatible endpoint per role; any member can list them. The key
//! is encrypted before it is stored, and only its last four characters are
//! ever shown — in `GET` and in `PUT`. No route returns the ciphertext, the
//! full key, or a provider's response body, which can echo the key back.
//!
//! `fallback_to_managed` defaults `false` and is only set when a member
//! explicitly sends it: nothing falls back to the managed model silently.

use std::sync::Arc;

use cratefield_core::axum::extract::{Path, State};
use cratefield_core::axum::http::{HeaderMap, StatusCode};
use cratefield_core::axum::response::{IntoResponse, Response};
use cratefield_core::axum::routing::{get, put};
use cratefield_core::axum::{self, Json};
use cratefield_core::{DbError, ModuleContext, Problem, ProblemDef};
// The one "who is calling" every route in this venture answers with: a
// session cookie or a personal access token (issue #72).
use livingbrain_tokens::authenticate;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::format_description::well_known::Rfc3339;

use crate::crypto;
use crate::probe;
use crate::ssrf;
use crate::store;

/// The roles a model can be connected for.
const ROLES: &[&str] = &["triage", "main", "research"];

/// Provider presets with their default base URLs.
const OPENAI_BASE: &str = "https://api.openai.com/v1";
const OPENROUTER_BASE: &str = "https://openrouter.ai/api/v1";
const DEEPSEEK_BASE: &str = "https://api.deepseek.com/v1";
const ANTHROPIC_BASE: &str = "https://api.anthropic.com/v1";

/// Only a workspace admin or owner can connect or remove models.
const FORBIDDEN: ProblemDef = ProblemDef {
    slug: "models/forbidden",
    status: StatusCode::FORBIDDEN,
    title: "Admin or owner access required",
    description: "Only a workspace admin or owner can connect or remove models.",
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
        .route("/{role}", put(connect).delete(remove))
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

    let base_url = resolve_base_url(&body.provider, body.base_url.as_deref())?;
    let api_key = body.api_key;

    // SSRF: validate the URL structure, then resolve the host via DoH
    // and reject if any address is private. Everything after this point —
    // the probe, and the base URL stored on the row — uses `url`, the
    // parsed and normalized form the guard just approved, never the raw
    // string a member sent.
    let url = ssrf::validate_url(&base_url).map_err(ssrf_refused)?;
    ssrf::check_destination(&*http, &url)
        .await
        .map_err(ssrf_refused)?;
    let base_url = url.as_str().to_owned();

    // Probe: send a few small requests to check tool calling, JSON output
    // and context size. If the first call fails, refuse with 422 naming
    // the HTTP status only — never the provider's body.
    let probe_result = probe::probe(&*http, &url, &body.model, api_key.as_str())
        .await
        .map_err(unreachable_error)?;

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
            provider: &body.provider,
            base_url: &base_url,
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

/// Resolves the base URL for a provider. Presets use their default; custom
/// requires `base_url`.
fn resolve_base_url(provider: &str, base_url: Option<&str>) -> Result<String, Problem> {
    match provider {
        "openai" => Ok(OPENAI_BASE.to_owned()),
        "openrouter" => Ok(OPENROUTER_BASE.to_owned()),
        "deepseek" => Ok(DEEPSEEK_BASE.to_owned()),
        "anthropic" => Ok(ANTHROPIC_BASE.to_owned()),
        "custom" => base_url
            .filter(|url| !url.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| {
                Problem::validation_failed("base_url is required for the custom provider")
            }),
        other => Err(Problem::validation_failed(format!(
            "unknown provider: {other}"
        ))),
    }
}

/// The request body for `PUT /{role}`. `Debug` is safe because the key
/// arrives already wrapped in an [`ApiKey`](crate::crypto::ApiKey), whose
/// `Debug` shows four characters and a `…`.
#[derive(Debug, Deserialize)]
struct ConnectRequest {
    provider: String,
    base_url: Option<String>,
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
