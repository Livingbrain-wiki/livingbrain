//! The `ask` module: `POST /v1/ask`, the route the CLI's
//! `livingbrain ask` speaks.
//!
//! The answer is a **retrieval brief** and deliberately nothing more: the
//! pages that mention the question, with their full text, plus citations.
//! No model is called anywhere in the venture yet — the brief is the input
//! a model would get, served as-is, so the CLI is honest about what it
//! knows today.

use std::sync::Arc;

use cratefield_core::axum::Json;
use cratefield_core::axum::extract::State;
use cratefield_core::axum::http::{HeaderMap, Method};
use cratefield_core::axum::routing::post;
use cratefield_core::{Config, ConfigError, Migrations, Module, ModuleContext, Port, Problem};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::Wiki;
use crate::facts::{as_of, brief_lines, brief_section, relevant};
use crate::recipes::{CONTEXT_PAGES, find_in_project, snippet};
use crate::service::Service;

/// What an ask answers with when nothing in the asker's pages mentions the
/// question. Honest by design: the MCP server asks its model to say this
/// rather than guess, and a brief with no pages in it is the same answer.
const NOTHING_FOUND: &str = "Nothing in the pages you can read mentions this.";

/// The `ask` module, constructed with the pages access the venture
/// injected.
pub struct Ask {
    wiki: Wiki,
}

impl Ask {
    /// An ask module over the given pages access. See [`Wiki`] for where
    /// each field comes from.
    #[must_use]
    pub fn new(wiki: Wiki) -> Self {
        Self { wiki }
    }
}

impl Module for Ask {
    fn name(&self) -> &'static str {
        "ask"
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
            .route("/", post(ask))
            .with_state(state)
    }
}

/// A `POST /v1/ask` body, as the CLI sends it.
#[derive(Debug, Deserialize)]
struct AskRequest {
    question: String,
    #[serde(default)]
    project: Option<String>,
    /// Answer as of this partial date (docs/temporal-facts.md §5) rather
    /// than now. Without it, a time the question names is used.
    #[serde(default)]
    as_of: Option<String>,
}

/// `POST /` — the pages that mention the question, as one brief.
///
/// The sections follow `brain_context_for`'s format, minus its MCP-specific
/// framing: a numbered heading per found page, then its body. Each section
/// heading is also a structured citation, with a quote a reader can match
/// the section to. `project`, when sent, narrows the search to the pages
/// filed under it.
///
/// # Errors
///
/// 401 without a credential, 422 for an empty question, and the store's
/// mapped problem otherwise.
async fn ask(
    State(state): State<Arc<Service>>,
    method: Method,
    headers: HeaderMap,
    Json(request): Json<AskRequest>,
) -> Result<Json<Value>, Problem> {
    let asker = state.asker(&method, &headers).await?;
    let question = request.question.trim();
    if question.is_empty() {
        return Err(Problem::validation_failed("`question` is required"));
    }

    let store = state.store()?;
    let scopes = state.scopes(&asker);
    let found = find_in_project(
        &store,
        &scopes,
        question,
        CONTEXT_PAGES,
        request.project.as_deref(),
    )
    .await?;

    // The facts the question is about, as of the time it asks about: an
    // explicit `as_of`, else a time the question names, else now.
    let facts = state.facts_store()?;
    let now = facts.now();
    let at = match request.as_of.as_deref() {
        Some(raw) => as_of(Some(raw), now)?.1,
        None => livingbrain_memory::timeline::as_of_in_question(question, now).unwrap_or(now),
    };
    let timelines = relevant(&facts, &asker, &scopes, question).await?;
    let (lines, fact_citations) = brief_lines(&timelines, at);

    let mut brief = brief_section(&lines, at);
    for (index, page) in found.iter().enumerate() {
        brief.push_str(&format!(
            "\n[{}] {} — {}\n\n{}\n",
            index + 1,
            page.title,
            page.reference(),
            page.body.trim(),
        ));
    }
    if found.is_empty() && lines.is_empty() {
        brief.push_str(NOTHING_FOUND);
    }
    let mut citations: Vec<Value> = found
        .iter()
        .map(|page| {
            json!({
                "title": page.title,
                "url": page.url,
                "quote": snippet(&page.body),
            })
        })
        .collect();
    citations.extend(fact_citations);
    let fact_keys: Vec<Value> = lines
        .iter()
        .map(|(index, line)| {
            json!({
                "subject": timelines[*index].subject,
                "attribute": timelines[*index].attribute,
                "line": line,
            })
        })
        .collect();
    Ok(Json(json!({
        "answer": brief,
        "citations": citations,
        "as_of": livingbrain_memory::timeline::rfc3339(at),
        "facts": fact_keys,
    })))
}
