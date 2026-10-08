//! JSON-RPC 2.0 and the MCP method routing.
//!
//! Stateless Streamable HTTP (the 2025-06-18 revision): one `POST` per
//! message, one JSON response, no session id and no server-to-client stream.
//! The 2025-03-26 revision is advertised too, and behaves the same way
//! except that it also permits a plain-JSON answer to a POST and no
//! `Mcp-Session-Id` — which is what this server already does.

use cratefield_core::axum::http::{HeaderMap, header};
use serde_json::{Value, json};

use crate::McpState;
use crate::auth::{Asker, page_scopes};
use crate::tools::{self, Call};

/// The revision this server speaks when a client asks for one it does not.
const PROTOCOL_VERSION: &str = "2025-06-18";
/// The revisions whose shape this server implements.
const SUPPORTED: [&str; 2] = ["2025-06-18", "2025-03-26"];

/// What a client is told once, at `initialize`, so the guidance is in the
/// model's context from the first tool call rather than left to the docs.
const INSTRUCTIONS: &str = "\
This is one person's brain, and you are reading it with their access: every page \
here is a page they may read, and no page of anybody else's exists as far as you \
are concerned. Before you plan or write code in a repository, call \
brain_context_for(repo, task) and read what it comes back with — it is a brief, \
not a search result. Cite every claim you take from this brain with the [n] marker \
the tool printed; those markers resolve to the numbered citations at the end of the \
same message, each carrying a title, a ref and a URL. If a tool returns nothing, say \
so and move on — the pages you cannot see are not pages that do not exist. \
brain_note writes to the caller's own private memory, redacted of secrets first.";

/// A request this server will not serve. A tool that cannot answer is not one
/// of these: it is an `isError` **result**, so the model sees the text and
/// can retry.
pub(crate) enum Refusal {
    /// The body was not JSON, so there is no id to answer.
    Parse(String),
    /// JSON, but not a request this server can answer.
    Invalid(String),
    /// A method this server does not have.
    Method(String),
    /// A `tools/call` whose arguments were not the shape the schema says.
    Params(String),
    /// The server's own fault.
    Internal(String),
}

impl Refusal {
    fn code(&self) -> i64 {
        match self {
            Self::Parse(_) => -32700,
            Self::Invalid(_) => -32600,
            Self::Method(_) => -32601,
            Self::Params(_) => -32602,
            Self::Internal(_) => -32603,
        }
    }

    fn message(&self) -> &str {
        match self {
            Self::Parse(m)
            | Self::Invalid(m)
            | Self::Method(m)
            | Self::Params(m)
            | Self::Internal(m) => m,
        }
    }
}

/// A JSON-RPC 2.0 error envelope. The id is unknown for a parse error —
/// there was no JSON to read one from — so the caller passes `null`, which
/// is what JSON-RPC 2.0 §5 asks a parse-error response to carry.
pub(crate) fn error(id: &Value, refusal: Refusal) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": refusal.code(), "message": refusal.message() },
    })
}

/// Does this method speak for somebody? The handshake does not: a client has
/// to be able to learn the server exists, and to read the 401 that tells it
/// how to authenticate, before it holds a credential.
pub(crate) fn needs_asker(method: &str) -> bool {
    matches!(method, "tools/list" | "tools/call")
}

/// The `Authorization: Bearer …` value, or `None` for a request that carries
/// none. The scheme is matched case-insensitively (RFC 7235 §2.1).
pub(crate) fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then_some(token)
}

/// Answer one request, envelope and all.
pub(crate) async fn dispatch(
    state: &McpState,
    asker: &Asker,
    id: &Value,
    request: &Value,
) -> Value {
    let params = request.get("params");
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let answered: Result<Value, Refusal> = match method {
        "initialize" => Ok(initialize(params)),
        // Liveness and the client handshake carry no data, so nothing is
        // scoped and a client holding no credential may still send them.
        "ping" | "notifications/initialized" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools::descriptors() })),
        "tools/call" => return call(state, asker, id, params).await,
        other => Err(Refusal::Method(format!("method not found: {other}"))),
    };
    match answered {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(refusal) => error(id, refusal),
    }
}

/// The `initialize` result: the revision this server will speak, the one
/// capability it has, and who it is.
fn initialize(params: Option<&Value>) -> Value {
    let asked = params
        .and_then(|params| params.get("protocolVersion"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    json!({
        "protocolVersion": if SUPPORTED.contains(&asked) { asked } else { PROTOCOL_VERSION },
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "livingbrain", "version": env!("CARGO_PKG_VERSION") },
        "instructions": INSTRUCTIONS,
    })
}

/// `tools/call`: parse, then run. The parse is what a `-32602` is for. Past
/// that point the call is well-formed, and whatever went wrong is the tool's
/// answer.
async fn call(state: &McpState, asker: &Asker, id: &Value, params: Option<&Value>) -> Value {
    let name = params
        .and_then(|params| params.get("name"))
        .and_then(Value::as_str);
    let Some(name) = name else {
        return error(
            id,
            Refusal::Params("tools/call needs a tool name".to_owned()),
        );
    };
    let arguments = params
        .and_then(|params| params.get("arguments"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let parsed: Call = match tools::parse(name, &arguments) {
        Ok(parsed) => parsed,
        Err(message) => return error(id, Refusal::Params(message)),
    };
    // The caller's scopes, resolved once: a tool never learns anyone else's.
    let scopes = page_scopes(asker);
    // A page store this module cannot build is its own fault and the caller's
    // cannot fix it, so it is the one thing that is an error object rather
    // than a result.
    let store = match state.store() {
        Ok(store) => store,
        Err(problem) => {
            let message = problem.detail.unwrap_or_else(|| problem.title.to_owned());
            return error(id, Refusal::Internal(message));
        }
    };
    match tools::run(&store, asker, &scopes, parsed).await {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(text) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "content": [{ "type": "text", "text": text }], "isError": true },
        }),
    }
}
