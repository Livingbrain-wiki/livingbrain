//! `livingbrain-mcp`: the remote MCP server (issue #24) and the source import
//! endpoint (issue #81).
//!
//! Four tools over `livingbrain-pages` — `brain_search`, `brain_page`,
//! `brain_context_for`, `brain_note` — on stateless Streamable HTTP, plus one
//! `POST /v1/pages/sources` for a file that arrived from outside. Both are
//! mounted *inside* the pages module: the harness scopes `Blob` per module
//! name, so a sibling module could never open a body this one writes.
//!
//! Every tool reads through [`page_scopes`] and none takes a scope argument,
//! so an agent sees what its user sees; every page a tool quotes comes back as
//! a numbered citation the model is told to cite.

#![forbid(unsafe_code)]

mod auth;
mod protocol;
mod sources;
mod tools;

pub use auth::{Asker, AuthError, BearerAuth, page_scope, page_scopes};
pub use sources::sources_router;

use std::sync::Arc;

use cratefield_core::axum::body::Bytes;
use cratefield_core::axum::extract::State;
use cratefield_core::axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use cratefield_core::axum::response::{IntoResponse, Response};
use cratefield_core::axum::routing::post;
use cratefield_core::axum::{self, Json, Router};
use cratefield_core::{ModuleContext, Problem, ProblemDef};
use cratefield_kms::Kms;
use livingbrain_pages::PageStore;
use serde_json::Value;

/// The RFC 9728 §5.1 challenge, naming the protected-resource metadata of the
/// authorization server — issue #72, which has not landed, so a client that
/// follows it today gets a 404. Both surfaces here answer with it, so one
/// definition covers both.
pub(crate) const CHALLENGE: &str = "Bearer resource_metadata=\"https://mcp.livingbrain.wiki/.well-known/oauth-protected-resource\"";

/// No credential, or one that speaks for nobody — one definition for both, so
/// the body says the same thing either way.
pub(crate) const UNAUTHORIZED: ProblemDef = ProblemDef {
    slug: "mcp/unauthorized",
    status: StatusCode::UNAUTHORIZED,
    title: "Bearer token required",
    description: "The MCP endpoint answers only with a valid bearer token, and every \
                  read is scoped to the user it speaks for.",
};

/// A port the module declared and the runtime did not supply.
pub(crate) fn port<T: ?Sized>(slot: Option<Arc<T>>) -> Result<Arc<T>, Problem> {
    slot.ok_or_else(Problem::internal)
}

/// The router state: the pages module's own context, plus the two things the
/// ports do not carry — a key custodian and a way to resolve a bearer
/// credential.
///
/// The context is held as the `Arc` the pages module handed over rather than as
/// the ports alone: `Ports` is neither `Clone` nor copyable, and this crate
/// must not hand-copy one.
pub(crate) struct McpState {
    pub(crate) ctx: Arc<ModuleContext>,
    pub(crate) kms: Arc<dyn Kms>,
    pub(crate) auth: Arc<dyn BearerAuth>,
}

impl McpState {
    /// The page store for one request, over the pages module's own ports.
    fn store(&self) -> Result<PageStore, Problem> {
        let ports = &self.ctx.ports;
        Ok(PageStore::new(
            port(ports.db.clone())?,
            port(ports.blob.clone())?,
            Arc::clone(&self.kms),
            port(ports.clock.clone())?,
            port(ports.id_gen.clone())?,
        ))
    }
}

/// The MCP routes, built from the pages module's context.
///
/// `ctx` is that module's own `ModuleContext`, which is the point: its `Blob`
/// is scoped to `pages`, so a page the module writes is a page this surface
/// can open. `auth` is the credential resolver — until the OAuth server lands
/// (issue #72) the session cookie the web app already issues.
pub fn router(ctx: Arc<ModuleContext>, kms: Arc<dyn Kms>, auth: Arc<dyn BearerAuth>) -> Router {
    let state = Arc::new(McpState { ctx, kms, auth });
    axum::Router::new()
        .route(
            "/",
            post(call).get(no_server_channel).delete(no_server_channel),
        )
        .with_state(state)
}

/// `POST /` — one JSON-RPC message in, one response out. Nothing streams and
/// nothing is remembered, so a request needs no session id.
async fn call(State(state): State<Arc<McpState>>, headers: HeaderMap, body: Bytes) -> Response {
    let Ok(request) = serde_json::from_slice::<Value>(&body) else {
        return jsonrpc(protocol::error(
            &Value::Null,
            protocol::Refusal::Parse("the body is not JSON".to_owned()),
        ));
    };
    // Only an object can be a request. An array (a batch) or a bare scalar is
    // refused rather than read as a notification, which is what a missing
    // `id` on something that is not a request would otherwise mean.
    let Some(object) = request.as_object() else {
        return jsonrpc(protocol::error(
            &Value::Null,
            protocol::Refusal::Invalid("a request is a JSON-RPC object".to_owned()),
        ));
    };
    // A request with no id is a notification (JSON-RPC 2.0 §4.1): answered,
    // never replied to, whatever its method.
    if !object.contains_key("id") {
        return StatusCode::ACCEPTED.into_response();
    }
    let id = object["id"].clone();
    let Some(method) = object.get("method").and_then(Value::as_str) else {
        return jsonrpc(protocol::error(
            &id,
            protocol::Refusal::Invalid("the request names no method".to_owned()),
        ));
    };
    // Only the methods that read are authenticated, so a client can complete
    // the handshake and read the 401 that says how to authenticate.
    let asker = if protocol::needs_asker(method) {
        let Some(token) = protocol::bearer(&headers) else {
            return unauthorized();
        };
        match state.auth.authenticate(&state.ctx.ports, token).await {
            // An asker with no workspace or no user id names nobody: it is
            // the credential that is wrong, and a 401 says so without saying
            // which part was.
            Ok(asker) if !asker.workspace_id.is_empty() && !asker.user_id.is_empty() => asker,
            _ => return unauthorized(),
        }
    } else {
        Asker {
            workspace_id: String::new(),
            user_id: String::new(),
        }
    };
    jsonrpc(protocol::dispatch(&state, &asker, &id, &request).await)
}

/// The 401, with the challenge that says where to authenticate. Shared with
/// the source importer, which answers a missing credential the same way.
pub(crate) fn unauthorized() -> Response {
    let mut response = Problem::new(&UNAUTHORIZED).into_response();
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static(CHALLENGE),
    );
    response
}

/// `GET` and `DELETE` — the transport's optional server-initiated channels.
/// This server opens neither, so neither is routed: 405 with the one method
/// that is served.
async fn no_server_channel() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [(header::ALLOW, "POST")],
        "This endpoint speaks stateless Streamable HTTP: POST is the only method it serves.",
    )
        .into_response()
}

/// A JSON-RPC response, as JSON.
fn jsonrpc(message: Value) -> Response {
    Json(message).into_response()
}
