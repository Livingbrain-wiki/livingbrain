//! The source import endpoint, at `/v1/pages/sources` (issue #81).
//!
//! One route, `POST /`, that records a file this brain did not write: where it
//! came from, what it says, and who sent it. It is mounted **inside** the pages
//! module for the same reason the MCP surface is — the harness scopes `Blob`
//! per module name, so only a surface built from the pages module's own
//! context can open a body that module sealed.
//!
//! Three things are decided here rather than left to the store, because the
//! store is the wrong place to decide them:
//!
//! - **The asker decides the scope, from their own grant.** The body names a
//!   scope as `personal` or `shared`; the server folds it with the workspace
//!   and checks the result against [`page_scopes`] (issue #106). A caller
//!   cannot name a scope that is not theirs, because the string they send is
//!   not the scope it becomes.
//! - **The server redacts; the client never gets the choice.** Body *and* path
//!   go through the ingest pipeline's redaction (issue #77), so a client that
//!   skipped it (issue #108) lands the same ledger as one that did not.
//! - **The credential is the MCP endpoint's.** No cookie, no second way in: a
//!   missing or rejected bearer gets the same 401, with the same
//!   `WWW-Authenticate` challenge, as [`crate::router`] answers.

use std::sync::Arc;

use cratefield_core::axum::body::Bytes;
use cratefield_core::axum::extract::State;
use cratefield_core::axum::extract::rejection::BytesRejection;
use cratefield_core::axum::http::{HeaderMap, StatusCode, Uri};
use cratefield_core::axum::response::{IntoResponse, Response};
use cratefield_core::axum::routing::post;
use cratefield_core::axum::{self, Json, Router};
use cratefield_core::{ModuleContext, Problem, ProblemDef};
use cratefield_kms::Kms;
use livingbrain_access::{Scope, UserId};
use livingbrain_pages::{
    IngestError, IngestOutcome, Ingestor, SourceIngest, SourceKind, SourceStore,
};
use serde_json::{Value, json};

use crate::McpState;
use crate::auth::{Asker, BearerAuth, page_scope, page_scopes};
use crate::{port, unauthorized};

/// A `scope` this endpoint does not have. A closed set, like the source kind:
/// the string a client sends names a grant, and only these two name one.
const UNKNOWN_SCOPE: ProblemDef = ProblemDef {
    slug: "sources/unknown-scope",
    status: StatusCode::BAD_REQUEST,
    title: "Unknown scope",
    description: "A source lands in the caller's own memory (`personal`) or in the \
                  workspace's shared memory (`shared`).",
};

/// A `kind` this endpoint does not have. Two exist: a file brought in from
/// outside (`import`), and a coding agent's session log (`agent_log`, the
/// CLI's `logs sync`); the bare word `import` is still the default, so an
/// old client does not have to know the second one landed.
const UNKNOWN_KIND: ProblemDef = ProblemDef {
    slug: "sources/unknown-kind",
    status: StatusCode::BAD_REQUEST,
    title: "Unknown source kind",
    description: "The kinds are `import` (a file from outside) and `agent_log` (a coding \
                  agent's session).",
};

/// A body that is not the JSON this endpoint reads, or one missing the two
/// fields it cannot do without.
const BAD_BODY: ProblemDef = ProblemDef {
    slug: "sources/bad-body",
    status: StatusCode::BAD_REQUEST,
    title: "A source needs a path and a body",
    description: "POST {\"path\": \"notes/one.md\", \"body\": \"# One\"}.",
};

/// A path that is not a relative name inside a vault: absolute, empty, over
/// 1024 bytes, or carrying a `..` segment, a backslash or a NUL.
const INVALID_PATH: ProblemDef = ProblemDef {
    slug: "sources/invalid-path",
    status: StatusCode::BAD_REQUEST,
    title: "That is not a path in a vault",
    description: "A source path is relative, has no `..` segment and no backslash, and is \
                  at most 1024 bytes.",
};

/// A folded scope the asker's own grant does not include. A 403 rather than a
/// 404: the caller named a scope that exists, and this one is not theirs.
const NOT_YOURS: ProblemDef = ProblemDef {
    slug: "sources/not-your-scope",
    status: StatusCode::FORBIDDEN,
    title: "That scope is not yours",
    description: "You may import into your own memory or the workspace's shared memory.",
};

/// A body over redaction's cap: 413, and an instruction the caller can act on.
const TOO_LARGE: ProblemDef = ProblemDef {
    slug: "sources/too-large",
    status: StatusCode::PAYLOAD_TOO_LARGE,
    title: "That body is too large to read safely",
    description: "Split the document on a line or record boundary and send it again.",
};

/// Anything else redaction refused. 422: the endpoint cannot fix it by being
/// asked differently, and nothing was stored.
const NOT_REDACTABLE: ProblemDef = ProblemDef {
    slug: "sources/not-redactable",
    status: StatusCode::UNPROCESSABLE_ENTITY,
    title: "That body could not be read for storage",
    description: "Nothing was stored: the body was refused before it was sealed.",
};

/// The routes, built from the pages module's own context.
///
/// `ctx` is the same module context [`crate::router`] is handed, and `kms` the
/// same custodian: a source body is sealed exactly as a page body is (issue
/// #43), so an endpoint without one has nothing it may write, and answers 500
/// rather than sealing a body under a key nobody holds.
pub fn sources_router(
    ctx: Arc<ModuleContext>,
    kms: Arc<dyn Kms>,
    auth: Arc<dyn BearerAuth>,
) -> Router {
    let state = Arc::new(McpState { ctx, kms, auth });
    axum::Router::new()
        .route("/", post(create))
        .with_state(state)
        // The harness wraps every module router in a 64 KiB `DefaultBodyLimit`,
        // which is right for a tool call and wrong here: a vault file is up to
        // `livingbrain_redact::MAX_INPUT_BYTES` of Markdown, and this endpoint
        // promises to take one. This layer is inner and per-route, and axum
        // reads the limit out of the request extensions the innermost one put
        // there, so it is this ceiling the extractor enforces — `/mcp` keeps
        // the harness one. It is the same number the pages module declares as
        // its coarse pre-buffer ceiling, so the two agree and a body the
        // runtime admits is one this route can read.
        .layer(axum::extract::DefaultBodyLimit::max(
            livingbrain_pages::MAX_SOURCE_BODY_BYTES,
        ))
}

/// `POST /` — one imported file in, one row out.
///
/// The body arrives as a `Result` so a body over
/// [`livingbrain_pages::MAX_SOURCE_BODY_BYTES`] answers the same
/// `sources/too-large` problem any other unreadable body does, rather than the
/// extractor's own error.
async fn create(
    State(state): State<Arc<McpState>>,
    headers: HeaderMap,
    uri: Uri,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, Problem> {
    let asker = match asker(&state, &headers).await {
        Ok(asker) => asker,
        // The 401 the MCP endpoint answers with, challenge and all — one
        // definition, so both surfaces say the same thing to a client that
        // arrives at the wrong one.
        Err(()) => return Ok(unauthorized(&headers, &uri)),
    };
    let body = body.map_err(|_| Problem::new(&TOO_LARGE))?;
    let request = parse(&body)?;

    let kind = match request.kind.as_deref().unwrap_or("import") {
        "import" => SourceKind::Import,
        // The CLI's `logs sync` files one normalised agent session here; the
        // record itself, not the tool that wrote it, is the ledger's unit.
        "agent_log" => SourceKind::AgentLog,
        other => {
            return Err(Problem::new(&UNKNOWN_KIND)
                .with_detail(format!("`{other}` is not a kind this endpoint has")));
        }
    };

    let scope = match request.scope.as_deref().unwrap_or("personal") {
        "personal" => Scope::User(UserId::new(asker.user_id.clone())),
        "shared" => Scope::Shared,
        other => {
            return Err(Problem::new(&UNKNOWN_SCOPE)
                .with_detail(format!("`{other}` is not a scope this endpoint has")));
        }
    };
    // The wire answer names the scope the caller asked for; what it becomes is
    // the folded one, and only the folded one is checked against the grant.
    let scope_name = match scope {
        Scope::User(_) => "personal",
        _ => "shared",
    };
    let folded = page_scope(&asker.workspace_id, &scope);
    if !page_scopes(&asker).contains(&folded) {
        return Err(Problem::new(&NOT_YOURS));
    }

    // The one ingest pipeline (issue #77): normalise, redact body and path
    // — the client does not hold that decision, and a path is a place
    // secrets end up as readily as a body is — dedupe, store, and defer an
    // extraction on the port this module declares. No extractor exists yet,
    // so the enqueue is the only thing this route's write carries beyond the
    // row.
    let store = SourceStore::new(
        port(state.ctx.ports.db.clone())?,
        port(state.ctx.ports.blob.clone())?,
        Arc::clone(&state.kms),
        port(state.ctx.ports.clock.clone())?,
        port(state.ctx.ports.id_gen.clone())?,
    );
    // Defer is a declared port, not a nice-to-have: silently running without
    // it would store the row and quietly never extract it.
    let defer = port(state.ctx.ports.defer.clone())?;
    let ingested = Ingestor::new(store, defer)
        .ingest(SourceIngest {
            kind,
            workspace: asker.workspace_id.clone(),
            scope: folded,
            // An import has no permalink or message id to name — the path
            // is filing, not an origin — and no authoring member: whoever
            // wrote the file wrote it outside the workspace. The importer
            // travels as `imported_by`.
            origin_ref: None,
            author: None,
            imported_by: asker.user_id.clone(),
            rel_path: request.path,
            body: request.body,
        })
        .await
        .map_err(ingest_failed)?;
    let source = ingested.source;

    // 201 for the import that created the row (held or not — it is a new
    // source either way) and 200 for the one that found it: the same body is
    // one source, and the difference is what the caller has to know about to
    // keep its own bookkeeping straight.
    let status = if matches!(ingested.outcome, IngestOutcome::Duplicate) {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    // The path in the answer is the row's, not the request's: a source found
    // rather than created keeps the path it was first imported under, and a
    // caller keeping a mirror of the vault has to be told where this body
    // actually lives rather than where it asked to put it.
    Ok((
        status,
        Json(json!({
            "id": source.id,
            "kind": source.kind.as_str(),
            "scope": scope_name,
            "path": source.rel_path,
            "sha256": source.body_sha256,
            "wikilinks": source.wikilinks,
            "redacted": ingested.redactions,
            "created": !matches!(ingested.outcome, IngestOutcome::Duplicate),
            "created_at": source.created_at,
        })),
    )
        .into_response())
}

/// What a client sent. `path` and `body` are required; `scope` and `kind` are
/// optional and defaulted. Anything else in the object is ignored: this is not
/// a place a client configures the store.
#[derive(Debug, Default)]
struct Request {
    path: String,
    body: String,
    scope: Option<String>,
    kind: Option<String>,
}

fn parse(body: &[u8]) -> Result<Request, Problem> {
    let bad = |detail: &str| Problem::new(&BAD_BODY).with_detail(detail);
    let value: Value = serde_json::from_slice(body).map_err(|_| bad("the body is not JSON"))?;
    let required = |name: &str| -> Result<String, Problem> {
        value
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| bad(&format!("`{name}` is missing or not a string")))
    };
    let optional = |name: &str| -> Result<Option<String>, Problem> {
        match value.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(found)) => Ok(Some(found.clone())),
            Some(_) => Err(bad(&format!("`{name}` is not a string"))),
        }
    };
    Ok(Request {
        path: required("path")?,
        body: required("body")?,
        scope: optional("scope")?,
        kind: optional("kind")?,
    })
}

/// Redacts nothing here any more — the ingest pipeline owns that — but the
/// route still owns the vocabulary: over the cap is a 413 the caller can act
/// on, a text redaction will not handle a 422 it cannot, and a path outside
/// the vault rule the 400 that says which rule.
fn ingest_failed(error: IngestError) -> Problem {
    match error {
        IngestError::InvalidPath(path) => {
            Problem::new(&INVALID_PATH).with_detail(format!("`{path}` is not a path in a vault"))
        }
        IngestError::TooLarge => Problem::new(&TOO_LARGE),
        IngestError::Refused(_) => Problem::new(&NOT_REDACTABLE),
        // The refusals that were the store's to make travel as their variant
        // name only (`SourceError::variant`): the store error's own text
        // names scopes and hashes, and those belong in no answer and no
        // 500's log line.
        other => {
            eprintln!("a source could not be recorded: {}", other.variant());
            Problem::internal()
        }
    }
}

/// The credential, resolved exactly as the MCP endpoint resolves it: no
/// `Authorization` header, or one that names nobody — including an asker with
/// no workspace or no user id — gets the same 401.
async fn asker(state: &McpState, headers: &HeaderMap) -> Result<Asker, ()> {
    let Some(token) = crate::protocol::bearer(headers) else {
        return Err(());
    };
    match state.auth.authenticate(&state.ctx.ports, token).await {
        Ok(asker) if !asker.workspace_id.is_empty() && !asker.user_id.is_empty() => Ok(asker),
        _ => Err(()),
    }
}
