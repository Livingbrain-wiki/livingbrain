//! The `sources` module: `GET /v1/sources/{id}`, the route a citation
//! resolves through (issue #77), and the two listing routes beside it —
//! `GET /v1/sources` (the asker's sources, newest first, filterable by kind
//! and project, paginated) and `GET /v1/sources/projects` (the projects
//! those sources are filed under, for a filter dropdown).
//!
//! The listings answer columns only — never a body — and apply the same two
//! clauses the citation route does, to every row: the row's workspace is
//! the asker's, and its scope is one of the asker's page scopes. A member's
//! personal sources therefore list for that member alone, the shared ones
//! for every member of the workspace, and nothing of another workspace's
//! lists for anybody.
//!
//! One read route, and the whole of its job is deciding who may see one row
//! of the source ledger. The answer is the metadata a citation names — kind,
//! scope, origin, author, when it arrived, whether it is held — plus the
//! redacted body, opened from the blob store exactly as a page body is.
//!
//! The access rule has three clauses and one answer: a source is returned
//! only when its **workspace** is the asker's (the scope folds the workspace
//! away as a hash, so the row carries the plain id back) *and* its scope is
//! among the asker's page scopes. Any other case — an id that is not there,
//! another workspace's source, a scope the credential was narrowed past —
//! is the same 404, shaped like [`crate::problems::PAGE_NOT_FOUND`]: saying
//! which would be an answer about somebody else's source.
//!
//! A **held** source is visible to a member who may read it, flagged
//! `held: true`: screening's verdict is about extraction, not secrecy — the
//! source is stored in the ledger, and hiding it would break the citation
//! that names it.

use std::collections::HashMap;
use std::sync::Arc;

use cratefield_core::axum::Json;
use cratefield_core::axum::extract::{Path, Query, State};
use cratefield_core::axum::http::{HeaderMap, Method};
use cratefield_core::axum::routing::get;
use cratefield_core::{Config, ConfigError, Migrations, Module, ModuleContext, Port, Problem};
use livingbrain_pages::{SourceFilter, SourceKind, is_project};
use serde_json::{Value, json};

use crate::Wiki;
use crate::problems::SOURCE_NOT_FOUND;
use crate::service::Service;

/// The `sources` module: the citation route, constructed with the key
/// custodian and credential resolver the venture injected ([`Wiki`]).
pub struct Sources {
    wiki: Wiki,
}

impl Sources {
    /// A sources module over the given page access. See [`Wiki`] for where
    /// each field comes from.
    #[must_use]
    pub fn new(wiki: Wiki) -> Self {
        Self { wiki }
    }
}

impl Module for Sources {
    fn name(&self) -> &'static str {
        "sources"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    /// The store is built per request from these; the blob is this module's
    /// own per-request view, which the composition has rooted on the pages
    /// key space by planting [`RebindBlob`](crate::RebindBlob) under the
    /// scope the harness applies. Undeclared, the view would never arrive.
    fn requires(&self) -> &'static [Port] {
        &[Port::Db, Port::Blob, Port::Clock, Port::IdGen]
    }

    /// `Signer` is optional for the same reason the pages module's is: the
    /// credential resolver runs on this module's port view, and the views
    /// only carry what a module declared.
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
            .route("/", get(list))
            .route("/projects", get(projects))
            .route("/{id}", get(one))
            .with_state(state)
    }
}

/// How many sources `GET /v1/sources` lists when the caller names no limit.
const DEFAULT_LIST_LIMIT: usize = 50;
/// The ceiling on a caller-supplied listing limit: a limit above it is
/// clamped, not refused. A row is a handful of short columns (the path is
/// capped at 1 KiB, the project at 64 bytes), so a full page stays far under
/// the CLI stack's 1 MiB response buffer.
const MAX_LIST_LIMIT: usize = 100;
/// The longest cursor the listing reads. A cursor is the previous page's
/// last id, a 26-character ULID; anything much longer is not one.
const MAX_CURSOR_LEN: usize = 64;

/// `GET /?kind=&project=&agent=&limit=&cursor=` — the asker's sources,
/// newest first, without their bodies.
///
/// `kind` is one of the ledger's kinds (`agent_log`, `import`, …) and
/// `project` a project name, `agent` the agent an `agent_log` session was
/// written by (`claude_code`, `codex`, `colonizer`); each narrows the list, and a value outside
/// its rule is a 400 (`validation-failed`) rather than an empty list, so a typo is told apart from
/// "nothing filed there". `limit` defaults to 50 and is clamped to 100.
/// `next_cursor` is the last row's id when the page was full, and `null`
/// when it was the last page: pass it back as `cursor` for the next one.
///
/// # Errors
///
/// [`crate::problems::UNAUTHORIZED`] without a credential, 400 for a query
/// parameter outside its rule, and [`Problem::internal`] when the ledger
/// fails.
async fn list(
    State(state): State<Arc<Service>>,
    method: Method,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<Value>, Problem> {
    let asker = state.asker(&method, &headers).await?;
    let kind = kind_param(&query)?;
    let project = match query.get("project").map(|raw| raw.trim()) {
        None | Some("") => None,
        Some(raw) if is_project(raw) => Some(raw.to_owned()),
        Some(raw) => {
            return Err(Problem::validation_failed(format!(
                "`project` must be 1..=64 bytes of ascii letters, digits, `.`, `_` and `-`, \
                 got {raw:?}"
            )));
        }
    };
    let agent = match query.get("agent").map(|raw| raw.trim()) {
        None | Some("") => None,
        Some(raw)
            if raw.len() <= 32
                && raw.bytes().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-'
                }) =>
        {
            Some(raw.to_owned())
        }
        Some(raw) => {
            return Err(Problem::validation_failed(format!(
                "`agent` must be 1..=32 bytes of lowercase ascii letters, digits, `_` and `-`, \
                 got {raw:?}"
            )));
        }
    };
    let before = match query.get("cursor").map(String::as_str) {
        None | Some("") => None,
        Some(raw)
            if raw.len() <= MAX_CURSOR_LEN && raw.bytes().all(|b| b.is_ascii_alphanumeric()) =>
        {
            Some(raw.to_owned())
        }
        Some(raw) => {
            return Err(Problem::validation_failed(format!(
                "`cursor` must be the `next_cursor` of a previous page, got {raw:?}"
            )));
        }
    };
    let limit = match query.get("limit") {
        None => DEFAULT_LIST_LIMIT,
        Some(raw) => (raw.parse::<u32>().map_err(|_| {
            Problem::validation_failed(format!("`limit` must be a positive integer, got {raw:?}"))
        })? as usize)
            .clamp(1, MAX_LIST_LIMIT),
    };

    let scopes = state.scopes(&asker);
    let names: Vec<&str> = scopes.iter().map(String::as_str).collect();
    let listed = state
        .sources_store()?
        .list(
            &asker.workspace_id,
            &names,
            &SourceFilter {
                kind,
                project,
                agent,
                before,
            },
            limit,
        )
        .await
        .map_err(source_failed)?;
    // A full page may have a next one; a short page is the last. One extra
    // empty request at the very end is the price of not counting.
    let next_cursor = (listed.len() == limit)
        .then(|| listed.last().map(|source| source.id.clone()))
        .flatten();
    let sources: Vec<Value> = listed
        .iter()
        .map(|source| {
            json!({
                "id": source.id,
                "kind": source.kind.as_str(),
                "scope": scope_label(&source.scope),
                "path": source.rel_path,
                "project": source.project,
                "held": source.held,
                "created_at": source.created_at,
            })
        })
        .collect();
    Ok(Json(json!({
        "sources": sources,
        "next_cursor": next_cursor,
    })))
}

/// `GET /projects?kind=` — the projects the asker's sources are filed
/// under, alphabetically, with how many sources each holds.
///
/// Read through the same two clauses as the listing, so every name it
/// offers is one `GET /v1/sources?project=` finds something under — and a
/// project only another member's personal sources name is not offered.
///
/// # Errors
///
/// [`crate::problems::UNAUTHORIZED`] without a credential, 400 for an
/// unknown `kind`, and [`Problem::internal`] when the ledger fails.
async fn projects(
    State(state): State<Arc<Service>>,
    method: Method,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<Value>, Problem> {
    let asker = state.asker(&method, &headers).await?;
    let kind = kind_param(&query)?;
    let scopes = state.scopes(&asker);
    let names: Vec<&str> = scopes.iter().map(String::as_str).collect();
    let found = state
        .sources_store()?
        .projects(&asker.workspace_id, &names, kind)
        .await
        .map_err(source_failed)?;
    let projects: Vec<Value> = found
        .iter()
        .map(
            |entry| json!({ "project": entry.project, "key": entry.key, "sources": entry.sources }),
        )
        .collect();
    Ok(Json(json!({ "projects": projects })))
}

/// The `kind` query parameter: absent, or one of the ledger's kinds.
fn kind_param(query: &HashMap<String, String>) -> Result<Option<SourceKind>, Problem> {
    match query.get("kind").map(String::as_str) {
        None | Some("") => Ok(None),
        Some(raw) => SourceKind::parse(raw).map(Some).ok_or_else(|| {
            Problem::validation_failed(format!("`kind` is not a source kind, got {raw:?}"))
        }),
    }
}

/// Which of the asker's memories a folded scope is, by the prefix the fold
/// keeps (`livingbrain_pages::page_scope`): the hash says nothing a listing
/// needs to show, the kind of memory does.
fn scope_label(scope: &str) -> &'static str {
    if scope.starts_with("user-") {
        "personal"
    } else if scope.starts_with("channel-") {
        "channel"
    } else {
        "shared"
    }
}

/// `GET /{id}` — one source, for a member whose grant includes it.
///
/// # Errors
///
/// [`crate::problems::UNAUTHORIZED`] without a credential,
/// [`SOURCE_NOT_FOUND`](crate::problems::SOURCE_NOT_FOUND) for an unknown,
/// other-workspace or out-of-scope id — one body for all three — and
/// [`Problem::internal`] when the row is there and its body is not.
async fn one(
    State(state): State<Arc<Service>>,
    method: Method,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, Problem> {
    let asker = state.asker(&method, &headers).await?;
    let store = state.sources_store()?;
    let Some(source) = store.by_id(&id).await.map_err(source_failed)? else {
        return Err(Problem::new(&SOURCE_NOT_FOUND));
    };
    // The three clauses, decided on the row and the credential alone. An id
    // outside both is the same 404 as an id that is not there.
    let readable =
        source.workspace == asker.workspace_id && state.scopes(&asker).contains(&source.scope);
    if !readable {
        return Err(Problem::new(&SOURCE_NOT_FOUND));
    }
    let body = store
        .open(&source.scope, &source.body_sha256)
        .await
        .map_err(source_failed)?;
    // The body travels inside this JSON, so a near-cap body answers near the
    // CLI stack's 1 MiB response buffer — the ceiling export's zip already
    // lives under (`serve_cli`'s `MAX_RESPONSE_BUFFER`).
    Ok(Json(json!({
        "id": source.id,
        "kind": source.kind.as_str(),
        "scope": source.scope,
        "workspace": source.workspace,
        "origin_ref": source.origin_ref,
        "author": source.author,
        "project": source.project,
        "created_at": source.created_at,
        "held": source.held,
        "sha256": source.body_sha256,
        "body": body,
    })))
}

/// A ledger failure as a 500 with none of it in the body. Every way a read
/// can fail after the authz check — a shredded scope, a missing blob, a
/// corrupt row — is infrastructure the caller cannot act on, and the detail
/// of a store error names scopes and hashes that belong in no answer. The
/// operator's side gets the variant and nothing else, the same discipline
/// the import route keeps.
fn source_failed(error: livingbrain_pages::SourceError) -> Problem {
    // The variant and nothing else: the store error's own text names scopes
    // and hashes, and those belong in no answer and no 500's log line.
    eprintln!("a source could not be read: {}", error.variant());
    Problem::internal()
}
