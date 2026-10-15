//! The `notes` module: `POST /v1/notes`, the route the CLI's
//! `livingbrain note` speaks.
//!
//! A note is a page in the asker's own private scope, written by the brain
//! and slug-named after the *redacted* text — so the same note lands on the
//! same page instead of accumulating near-duplicates, and a second `POST`
//! of the same text bumps that page's version rather than making a new one.
//! The project a note is filed under, when one is sent, rides in the
//! frontmatter fence, which is what `GET /v1/search?project=…` filters on.

use std::sync::Arc;

use cratefield_core::axum::Json;
use cratefield_core::axum::extract::State;
use cratefield_core::axum::http::HeaderMap;
use cratefield_core::axum::routing::post;
use cratefield_core::{Config, ConfigError, Migrations, Module, ModuleContext, Port, Problem};
use livingbrain_access::{Scope, UserId};
use livingbrain_mcp::page_scope;
use livingbrain_pages::{Author, EntityType, PageWrite};
use livingbrain_redact::{Policy, redact};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::Wiki;
use crate::problems::{page_read_error, page_write_error, redaction_refused};
use crate::recipes::{note_slug, one_line, wiki_url};
use crate::service::Service;

/// The `notes` module: the note route, constructed with the key custodian
/// and credential resolver the venture injected ([`Wiki`]).
pub struct Notes {
    wiki: Wiki,
}

impl Notes {
    /// A notes module over the given page access. See [`Wiki`] for where
    /// each field comes from.
    #[must_use]
    pub fn new(wiki: Wiki) -> Self {
        Self { wiki }
    }
}

impl Module for Notes {
    fn name(&self) -> &'static str {
        "notes"
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
    /// only carry what a module declared. Without the declaration the view
    /// would arrive with no signer, and `BearerAuth` could never verify a
    /// caller.
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
            .route("/", post(note))
            .with_state(state)
    }
}

/// A `POST /v1/notes` body. `title` is accepted for MCP parity with
/// `brain_note`; `project` is what the CLI's `--project` sends.
#[derive(Debug, Deserialize)]
struct NoteRequest {
    body: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    project: Option<String>,
}

/// `POST /` — one note, redacted and filed in the asker's own scope.
///
/// **The body and the title are both redacted before anything is stored**
/// (issue #108), and the slug is the hash of the *redacted* text, so the
/// same note lands on the same page. The write is the brain's: a note a
/// person edited by hand stays theirs, and the store refuses the overwrite
/// until the edit is reconciled.
///
/// This is a retrieval brief's input, not a conversation: nothing here
/// calls a model, and the note is stored verbatim (after redaction) for
/// search and `ask` to find.
///
/// # Errors
///
/// 401 without a credential, 422 for an empty body or content redaction
/// refused, 409 when the note's page has a conflict pending, and the
/// store's mapped problem otherwise.
async fn note(
    State(state): State<Arc<Service>>,
    headers: HeaderMap,
    Json(request): Json<NoteRequest>,
) -> Result<Json<Value>, Problem> {
    let asker = state.asker(&headers).await?;
    if request.body.trim().is_empty() {
        return Err(Problem::validation_failed("`body` is required"));
    }
    // Nothing a model or a person pasted reaches a body with a secret or a
    // piece of personal data still in it (issue #108).
    let (clean, findings) =
        redact(&request.body, Policy::Redact).map_err(|_| redaction_refused())?;
    let title = match request.title.as_deref() {
        Some(raw) => match redact(raw, Policy::Redact) {
            Ok((clean, _)) => one_line(&clean),
            Err(_) => String::new(),
        },
        None => String::new(),
    };
    let title = if title.is_empty() {
        "Note".to_owned()
    } else {
        title
    };
    let project = request
        .project
        .as_deref()
        .map(one_line)
        .filter(|project| !project.is_empty());

    let scope = page_scope(
        &asker.workspace_id,
        &Scope::User(UserId::new(asker.user_id.clone())),
    );
    let slug = note_slug(&clean);
    let store = state.store()?;
    let head = store
        .read(&scope, &slug)
        .await
        .map_err(page_read_error)?
        .map(|page| page.version);
    let meta = store
        .write(
            &scope,
            &slug,
            PageWrite {
                entity_type: EntityType::Decision,
                markdown: note_markdown(&title, project.as_deref(), &clean),
                author: Author::Brain { reconciles: None },
                base_version: head,
            },
        )
        .await
        .map_err(page_write_error)?;

    let reference = format!("{scope}/{slug}");
    Ok(Json(json!({
        "id": reference,
        "url": wiki_url(&scope, &slug),
        "version": meta.version,
        "redacted": findings.len(),
    })))
}

/// The note's markdown: the same fence `brain_note` writes, plus the
/// project line inside it when the note is filed under one. The project
/// rides in the fence so `search?project=` can filter on it without
/// opening a body.
fn note_markdown(title: &str, project: Option<&str>, clean: &str) -> String {
    match project {
        Some(project) => format!("---\ntitle: {title}\nproject: {project}\n---\n\n{clean}\n"),
        None => format!("---\ntitle: {title}\n---\n\n{clean}\n"),
    }
}
