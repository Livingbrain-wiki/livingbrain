//! The `app-tools` module: `POST /v1/tools/{name}`, the five tools a
//! connected app calls (issues #51 and #146; ADR 0005).
//!
//! The module is mounted as `app-tools` (the harness already has a `tools`
//! module, the MCP-client one, with no routes of its own) and the Worker
//! serves it at `/v1/tools/…`, which is the contract SupportGenius and
//! EarthOS already speak: `POST {base}/v1/tools/<name>` with JSON and
//! `Authorization: Bearer <credential>`.
//!
//! | Tool | Input | Answer |
//! |---|---|---|
//! | `brain_answer_public` | `{question}` | `{answer \| null, citations: [{title, url, scope: "public"}]}` |
//! | `brain_memory_recall` | `{subject}` | `{memories: [{text, …}]}` (at most 20, 500 characters each) |
//! | `brain_memory_note` | `{subject, text, source}` | `{id}` |
//! | `brain_memory_forget` | `{subject, id}` | `{forgotten: n}` |
//! | `brain_memory_forget_all` | `{subject}` | `{forgotten: n}` |
//!
//! ## Who may call
//!
//! Only an **app credential**: an agent account's key whose grants name
//! exactly one `app:<name>` space and the tool in question. Nothing else —
//! not a member's token, not an agent with page grants — reaches these
//! routes, and an app credential has no page grants, so it reaches no other
//! route of the API either. Revoking the key, disabling the account or
//! deleting its grants is a 401/403 on the next request: the grants are read
//! on every call.
//!
//! ## Tenancy
//!
//! `subject` is the app's own name for an end member (EarthOS sends
//! `user:<id>`). It never selects a scope on its own: the scope is
//! `page_scope(workspace, User("<app>:<subject>"))` where `<app>` comes from
//! the **credential's grants**, so a credential can only touch subjects under
//! its own app prefix, and the same subject string under another app or
//! workspace is a different scope.
//!
//! ## `brain_answer_public` is extractive
//!
//! No model is called. The question is looked up in the workspace's `public`
//! scope, the best-matching passages of published pages are returned
//! verbatim, and the answer is `null` when nothing covers the question well
//! enough ([`livingbrain_pages::MIN_COVERAGE_PERCENT`]).
//!
//! Every call is audited by tool and app/subject; the memory text is never
//! logged.

use std::collections::BTreeMap;
use std::sync::Arc;

use cratefield_core::axum::Json;
use cratefield_core::axum::extract::{Path, State};
use cratefield_core::axum::http::{HeaderMap, Method};
use cratefield_core::axum::response::{IntoResponse, Response};
use cratefield_core::axum::routing::post;
use cratefield_core::{Config, ConfigError, Migrations, Module, ModuleContext, Port, Problem};
use cratefield_module_orgs::grants::{self, Permission, Resource};
use livingbrain_access::Scope;
use livingbrain_mcp::Asker;
use livingbrain_pages::access::ScopeFilter;
use livingbrain_pages::{
    MAX_ANSWER_CHARS, MAX_MEMORY_CHARS, MAX_PASSAGES, MAX_SOURCE_CHARS, MemoryError, Publication,
    SearchOptions, clip, extract, is_memory_id, page_scope,
};
use livingbrain_radar::screen::screen_text;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::Wiki;
use crate::apps::{TOOLS, TOOLS_KIND, app_space, is_subject, subject_scope};
use crate::problems::{SCREENED, TOOL_INPUT, TOOL_NOT_ALLOWED, TOOL_NOT_FOUND, page_read_error};
use crate::service::Service;

/// How many memories a recall returns at most.
pub const RECALL_MAX: usize = 20;
/// How many characters of a memory a recall returns.
pub const RECALL_CHARS: usize = 500;
/// The longest question answered, in characters.
pub const QUESTION_MAX: usize = 1_000;
/// How many published pages a question is looked up in.
const CANDIDATES: usize = 8;
/// The ceiling on any answer, in bytes — what the client reads at most.
pub const RESPONSE_MAX_BYTES: usize = 256 * 1024;

/// The `app-tools` module.
pub struct AppTools {
    wiki: Wiki,
}

impl AppTools {
    /// An app-tools module over the given page access.
    #[must_use]
    pub fn new(wiki: Wiki) -> Self {
        Self { wiki }
    }
}

impl Module for AppTools {
    fn name(&self) -> &'static str {
        "app-tools"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    fn requires(&self) -> &'static [Port] {
        &[Port::Db, Port::Blob, Port::Clock, Port::IdGen]
    }

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
            .route("/{name}", post(call))
            .with_state(state)
    }
}

/// The app an app credential speaks for, and nothing else: its grants must
/// name exactly one `app:<name>` space and allow `tool` there.
async fn authorize(state: &Service, asker: &Asker, tool: &str) -> Result<String, Problem> {
    let denied = || Problem::new(&TOOL_NOT_ALLOWED);
    if !asker.is_agent() || asker.token_scopes.is_some() {
        return Err(denied());
    }
    let (db, clock, _) = state.core()?;
    let principal = livingbrain_pages::access::agent_principal(&asker.user_id);
    let stored = grants::grants_for(&*db, &asker.workspace_id, &[principal.as_str()])
        .await
        .map_err(|_| Problem::internal())?;
    let now = clock.now();
    let apps: std::collections::BTreeSet<&str> = stored
        .iter()
        .filter(|grant| grant.selector.kind() == TOOLS_KIND && grant.is_live(now))
        .filter_map(|grant| grant.selector.space().strip_prefix("app:"))
        .collect();
    let mut apps = apps.into_iter();
    let (Some(app), None) = (apps.next(), apps.next()) else {
        return Err(denied());
    };
    let resource = Resource::new(TOOLS_KIND, app_space(app), vec![tool.to_owned()])
        .map_err(|_| Problem::internal())?;
    if grants::decide(
        &stored,
        &[principal.as_str()],
        &resource,
        Permission::Write,
        now,
    )
    .is_allowed()
    {
        Ok(app.to_owned())
    } else {
        Err(denied())
    }
}

/// `POST /{name}`.
async fn call(
    State(state): State<Arc<Service>>,
    method: Method,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(input): Json<Value>,
) -> Result<Response, Problem> {
    let asker = state.asker(&method, &headers).await?;
    let Some(tool) = TOOLS.iter().copied().find(|tool| *tool == name) else {
        // A name nobody serves is the same answer to every caller who is
        // signed in at all.
        return Err(Problem::new(&TOOL_NOT_FOUND));
    };
    let app = match authorize(&state, &asker, tool).await {
        Ok(app) => app,
        Err(refused) => {
            state.audit(&asker, "tool.denied", tool).await;
            return Err(refused);
        }
    };
    // Every authorized call is one audit line: the tool and who it was for
    // (`<app>:<subject>`), and never what the memory said.
    let target = match input.get("subject").and_then(Value::as_str) {
        Some(subject) => format!("{app}:{}", clip(subject, 200)),
        None => format!("app:{app}"),
    };
    state.audit(&asker, &format!("tool.{tool}"), &target).await;
    let answer = match tool {
        "brain_answer_public" => answer_public(&state, &asker, input).await?,
        "brain_memory_recall" => recall(&state, &asker, &app, input).await?,
        "brain_memory_note" => note(&state, &asker, &app, input).await?,
        "brain_memory_forget" => forget(&state, &asker, &app, input).await?,
        _ => forget_all(&state, &asker, &app, input).await?,
    };
    debug_assert!(answer.to_string().len() <= RESPONSE_MAX_BYTES);
    Ok(Json(answer).into_response())
}

fn input<T: for<'de> Deserialize<'de>>(value: Value) -> Result<T, Problem> {
    serde_json::from_value(value).map_err(|error| {
        // serde names the field and the expected type, never the value.
        Problem::new(&TOOL_INPUT).with_detail(error.to_string())
    })
}

fn subject_of(subject: &str) -> Result<(), Problem> {
    if is_subject(subject) {
        Ok(())
    } else {
        Err(Problem::new(&TOOL_INPUT).with_detail(
            "`subject` must be 1-200 characters, with no control character and no \
             surrounding whitespace",
        ))
    }
}

#[derive(Deserialize)]
struct AnswerInput {
    question: String,
}

/// `brain_answer_public`: the best published passages, verbatim, or `null`.
async fn answer_public(state: &Service, asker: &Asker, value: Value) -> Result<Value, Problem> {
    let AnswerInput { question } = input(value)?;
    if question.trim().is_empty() || question.chars().count() > QUESTION_MAX {
        return Err(Problem::new(&TOOL_INPUT).with_detail("`question` must be 1-1000 characters"));
    }
    let none = json!({ "answer": null, "citations": [] });
    let scope = page_scope(&asker.workspace_id, &Scope::Public);
    let published: BTreeMap<String, Publication> = state
        .public_store()?
        .list(&scope)
        .await
        .map_err(page_read_error)?
        .into_iter()
        .map(|publication| (publication.slug.clone(), publication))
        .collect();
    if published.is_empty() {
        return Ok(none);
    }
    let store = state.store()?;
    // The store's candidates, with no score floor: the passages below judge
    // each one against the question (`extract`), at 60% of its terms.
    let hits = store
        .search_outcome(
            &[ScopeFilter::whole(scope.as_str(), "")],
            &question,
            CANDIDATES,
            SearchOptions::candidates(),
        )
        .await
        .map_err(page_read_error)?
        .hits;
    // (coverage, rank, passage, publication): best coverage first, the
    // search's own order breaking ties.
    let mut found: Vec<(f64, usize, String, &Publication)> = Vec::new();
    for (rank, hit) in hits.iter().enumerate() {
        // Only what the ledger says is published: a page the public scope
        // holds without a row is not public.
        let Some(publication) = published.get(&hit.slug) else {
            continue;
        };
        let Some(page) = store
            .read(&scope, &hit.slug)
            .await
            .map_err(page_read_error)?
        else {
            continue;
        };
        for passage in extract(&question, &publication.title, &page.markdown) {
            found.push((passage.coverage, rank, passage.text, publication));
        }
    }
    found.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.cmp(&b.1))
    });
    found.truncate(MAX_PASSAGES);
    if found.is_empty() {
        return Ok(none);
    }
    let text = found
        .iter()
        .map(|(_, _, passage, _)| passage.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut citations: Vec<Value> = Vec::new();
    for (_, _, _, publication) in &found {
        if !citations.iter().any(|c| {
            c["url"] == publication.url.as_str() && c["title"] == publication.title.as_str()
        }) {
            citations.push(json!({
                "title": publication.title,
                "url": publication.url,
                "scope": "public",
            }));
        }
    }
    Ok(json!({ "answer": clip(&text, MAX_ANSWER_CHARS), "citations": citations }))
}

#[derive(Deserialize)]
struct SubjectInput {
    subject: String,
}

/// `brain_memory_recall`: that subject's private scope, newest first.
async fn recall(state: &Service, asker: &Asker, app: &str, value: Value) -> Result<Value, Problem> {
    let SubjectInput { subject } = input(value)?;
    subject_of(&subject)?;
    let scope = subject_scope(&asker.workspace_id, app, &subject);
    let memories = state
        .memories_store()?
        .recall(&scope, RECALL_MAX)
        .await
        .map_err(page_read_error)?;
    Ok(json!({
        "memories": memories.iter().map(|memory| json!({
            "text": clip(&memory.text, RECALL_CHARS),
            "id": memory.id,
            "created_at": memory.created_at,
        })).collect::<Vec<_>>(),
    }))
}

#[derive(Deserialize)]
struct NoteInput {
    subject: String,
    text: String,
    #[serde(default)]
    source: String,
}

/// `brain_memory_note`: keep one thing, after the inbound screen.
async fn note(state: &Service, asker: &Asker, app: &str, value: Value) -> Result<Value, Problem> {
    let NoteInput {
        subject,
        text,
        source,
    } = input(value)?;
    subject_of(&subject)?;
    if text.trim().is_empty() {
        return Err(Problem::new(&TOOL_INPUT).with_detail("`text` is required"));
    }
    if text.chars().count() > MAX_MEMORY_CHARS {
        return Err(Problem::new(&TOOL_INPUT).with_detail("`text` is too long"));
    }
    if source.chars().count() > MAX_SOURCE_CHARS {
        return Err(Problem::new(&TOOL_INPUT).with_detail("`source` is too long"));
    }
    // Hidden text, an instruction aimed at a model, a secret or a piece of
    // personal data never reaches a row: the same screen as every inbound
    // note (#50). The text is not echoed on a refusal.
    let clean = screen_text(&text).map_err(|_| Problem::new(&SCREENED))?;
    let clean = clean.trim();
    if clean.is_empty() {
        return Err(Problem::new(&TOOL_INPUT).with_detail("`text` is required"));
    }
    // The source label is stored beside the text, so it is screened too.
    let source = screen_text(&source).map_err(|_| Problem::new(&SCREENED))?;
    let scope = subject_scope(&asker.workspace_id, app, &subject);
    let id = state
        .memories_store()?
        .note(&asker.workspace_id, app, &scope, clean, source.trim())
        .await
        .map_err(|error| match error {
            MemoryError::Invalid(why) => Problem::new(&TOOL_INPUT).with_detail(why),
            MemoryError::Full => Problem::new(&TOOL_INPUT).with_detail(error.to_string()),
            MemoryError::Store(error) => page_read_error(error),
        })?;
    Ok(json!({ "id": id }))
}

#[derive(Deserialize)]
struct ForgetInput {
    subject: String,
    id: String,
}

/// `brain_memory_forget`: hard-delete one memory of that subject.
async fn forget(state: &Service, asker: &Asker, app: &str, value: Value) -> Result<Value, Problem> {
    let ForgetInput { subject, id } = input(value)?;
    subject_of(&subject)?;
    if !is_memory_id(&id) {
        return Err(Problem::new(&TOOL_INPUT).with_detail("`id` is not a memory id"));
    }
    let scope = subject_scope(&asker.workspace_id, app, &subject);
    let forgotten =
        state
            .memories_store()?
            .forget(&scope, &id)
            .await
            .map_err(|error| match error {
                MemoryError::Invalid(why) => Problem::new(&TOOL_INPUT).with_detail(why),
                MemoryError::Full => Problem::internal(),
                MemoryError::Store(error) => page_read_error(error),
            })?;
    Ok(json!({ "forgotten": forgotten }))
}

/// `brain_memory_forget_all`: complete erasure of that subject.
async fn forget_all(
    state: &Service,
    asker: &Asker,
    app: &str,
    value: Value,
) -> Result<Value, Problem> {
    let SubjectInput { subject } = input(value)?;
    subject_of(&subject)?;
    let scope = subject_scope(&asker.workspace_id, app, &subject);
    let forgotten = state
        .memories_store()?
        .forget_all(&scope)
        .await
        .map_err(page_read_error)?;
    Ok(json!({ "forgotten": forgotten }))
}
