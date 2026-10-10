//! The `sources` module: `GET /v1/sources/{id}`, the route a citation
//! resolves through (issue #77).
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

use std::sync::Arc;

use cratefield_core::axum::Json;
use cratefield_core::axum::extract::{Path, State};
use cratefield_core::axum::http::HeaderMap;
use cratefield_core::axum::routing::get;
use cratefield_core::{Config, ConfigError, Migrations, Module, ModuleContext, Port, Problem};
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
            .route("/{id}", get(one))
            .with_state(state)
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
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, Problem> {
    let asker = state.asker(&headers).await?;
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
