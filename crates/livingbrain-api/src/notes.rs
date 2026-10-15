//! The `notes` module: `POST /v1/notes`, the route the CLI's
//! `livingbrain note` speaks.
//!
//! A note is a page in the asker's own private scope, written by the brain
//! and slug-named after the *redacted* text — so the same note lands on the
//! same page instead of accumulating near-duplicates, and a second `POST`
//! of the same text bumps that page's version rather than making a new one.
//! The project a note is filed under, when one is sent, rides in the
//! frontmatter fence, which is what `GET /v1/search?project=…` filters on.
//!
//! A caller that keeps one note per *thing* — `livingbrain logs sync`
//! keeps one per agent session, whose text grows as the session does —
//! names the page itself with `slug`. A keyed note is updated in place
//! rather than re-hashed into a new page, and a keyed write whose page
//! already holds exactly that markdown is not written at all: the answer
//! says `unchanged: true` and the version does not move, so a re-sync is a
//! no-op.
//!
//! **Session logs.** A keyed note under `agent-log-…` is a coding-agent
//! session's log (`livingbrain logs sync`, Colonizer): a *source*, so it is
//! written as a [`EntityType::Session`] page, not a decision, and the web
//! app keeps it out of the wiki's Pages list. A client that sends only the
//! id-only placeholder title (`colonizer session a15f…`) gets one named from
//! the note itself ([`livingbrain_pages::session_title`]): `EarthOS.world ·
//! Colony a15fba01 · #123 Fix auth redirect`. A session page still filed as
//! a decision is moved over in place on its next write.
//!
//! **The backfill.** This module's cron names the pages written before
//! titles were kept: each untitled page gets its title from its own body,
//! and each session log still filed as a decision is moved to `session` and,
//! when its title is a placeholder, retitled. A bounded batch per tick; a
//! page once handled is never picked again, so the pass is idempotent and
//! stops by itself.

use std::sync::Arc;

use cratefield_core::axum::Json;
use cratefield_core::axum::extract::State;
use cratefield_core::axum::http::{HeaderMap, Method};
use cratefield_core::axum::routing::post;
use cratefield_core::{
    AnyError, BoxFuture, Clock, Config, ConfigError, Migrations, Module, ModuleContext, Port,
    Problem, ScheduledBudget,
};
use livingbrain_pages::{
    Author, EntityType, PageStore, PageWrite, frontmatter_title, is_placeholder_title,
    is_session_slug, is_slug, session_title,
};
use livingbrain_redact::{Policy, redact};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::Wiki;
use crate::facts::{FactRequest, inputs, record_all};
use crate::problems::{fact_error, page_read_error, page_write_error, redaction_refused};
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

    /// The title backfill (see the module docs): one bounded batch a tick.
    fn scheduled<'a>(
        &'a self,
        ctx: &'a ModuleContext,
        _cron: &'a str,
    ) -> BoxFuture<'a, Result<(), AnyError>> {
        Box::pin(async move {
            let (Some(db), Some(blob), Some(clock), Some(id_gen)) = (
                ctx.ports.db.clone(),
                ctx.ports.blob.clone(),
                ctx.ports.clock.clone(),
                ctx.ports.id_gen.clone(),
            ) else {
                return Ok(());
            };
            let store = PageStore::new(db, blob, self.wiki.kms.clone(), clock.clone(), id_gen);
            backfill(&store, &ctx.scheduled, clock.as_ref(), BACKFILL_BATCH)
                .await
                .map(|_| ())
                .map_err(|error| Box::new(error) as AnyError)
        })
    }
}

/// How many pages one cron tick names at most. Each is a body read and at
/// most one write; the backlog this exists for (a few hundred session logs)
/// is gone within the hour.
pub const BACKFILL_BATCH: usize = 25;

/// What one backfill pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Backfilled {
    /// Session logs moved from `decision` to `session`.
    pub retyped: usize,
    /// Session logs given a real title (a new version of the page).
    pub retitled: usize,
    /// Pages whose listed title was set from their own body.
    pub titled: usize,
    /// Pages whose body could not be opened: marked so they are not tried
    /// again.
    pub unreadable: usize,
}

/// One backfill pass: up to `limit` pages with no listed title, each named
/// from its own body; a session log still filed as a decision moved to
/// `session` first, and retitled when its title is a placeholder.
///
/// Idempotent: every page it handles leaves the candidate set (its title
/// column is set, to an empty string when there is nothing to show), so a
/// second pass over the same store does nothing.
///
/// # Errors
///
/// Only when the candidate query itself fails; a page that cannot be
/// handled is marked and skipped, never an error for the rest.
pub async fn backfill(
    store: &PageStore,
    budget: &ScheduledBudget,
    clock: &dyn Clock,
    limit: usize,
) -> Result<Backfilled, livingbrain_pages::PageError> {
    let mut done = Backfilled::default();
    for candidate in store.title_candidates(limit).await? {
        if budget.exhausted(clock.now()) {
            break;
        }
        let (scope, slug) = (candidate.scope.as_str(), candidate.slug.as_str());
        let mut kind = candidate.entity_type;
        if kind == EntityType::Decision
            && is_session_slug(slug)
            && let Ok(true) = store
                .retype(scope, slug, EntityType::Decision, EntityType::Session)
                .await
        {
            done.retyped += 1;
            kind = EntityType::Session;
        }
        let page = match store.read(scope, slug).await {
            Ok(Some(page)) => page,
            Ok(None) => continue,
            Err(_) => {
                // A forgotten scope, a retired key: nothing to name it with.
                let _ = store.set_title(scope, slug, None).await;
                done.unreadable += 1;
                continue;
            }
        };
        let current = frontmatter_title(&page.frontmatter);
        if kind == EntityType::Session
            && is_placeholder_title(current.as_deref().unwrap_or(""), slug)
        {
            let title = one_line(&session_title(
                slug,
                page.frontmatter.get("project"),
                &page.markdown,
            ));
            if !title.is_empty() {
                let markdown = with_title(&page.markdown, &title);
                let wrote = store
                    .write(
                        scope,
                        slug,
                        PageWrite {
                            entity_type: EntityType::Session,
                            markdown,
                            author: Author::Brain { reconciles: None },
                            base_version: Some(page.version),
                        },
                    )
                    .await;
                if wrote.is_ok() {
                    done.retitled += 1;
                    continue;
                }
                // A human edit pending, or a sync racing this pass: the list
                // shows the derived name, and the next sync writes it.
                let _ = store.set_title(scope, slug, Some(&title)).await;
                done.titled += 1;
                continue;
            }
        }
        if store
            .set_title(scope, slug, current.as_deref())
            .await
            .is_ok()
        {
            done.titled += 1;
        }
    }
    Ok(done)
}

/// A body with its fence's `title:` line replaced (or added): the session
/// retitle, which changes nothing else in the page.
fn with_title(markdown: &str, title: &str) -> String {
    let mut lines = markdown.split_inclusive('\n');
    let Some(first) = lines.next() else {
        return format!("---\ntitle: {title}\n---\n\n");
    };
    if first.trim_end() != "---" {
        return format!("---\ntitle: {title}\n---\n\n{markdown}");
    }
    let mut out = String::with_capacity(markdown.len() + title.len());
    out.push_str(first);
    let mut replaced = false;
    let mut in_fence = true;
    for line in lines {
        if in_fence && line.trim_end() == "---" {
            if !replaced {
                out.push_str(&format!("title: {title}\n"));
                replaced = true;
            }
            in_fence = false;
            out.push_str(line);
            continue;
        }
        if in_fence && line.trim_start().to_ascii_lowercase().starts_with("title:") {
            if !replaced {
                out.push_str(&format!("title: {title}\n"));
                replaced = true;
            }
            continue;
        }
        out.push_str(line);
    }
    out
}

/// A `POST /v1/notes` body. `title` is accepted for MCP parity with
/// `brain_note`; `project` is what the CLI's `--project` sends; `slug`,
/// when sent, names the page instead of the hash of the text.
#[derive(Debug, Deserialize)]
struct NoteRequest {
    body: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    slug: Option<String>,
    /// Temporal facts the note states (docs/temporal-facts.md, format v1),
    /// recorded in the same personal scope with the note as their source.
    #[serde(default)]
    facts: Option<Vec<FactRequest>>,
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
/// With `slug`, the note is that page in the asker's own scope, updated in
/// place; a write that would change nothing is skipped and answered with
/// `unchanged: true` at the head version.
///
/// # Errors
///
/// 401 without a credential, 400 for an empty body or a `slug` outside
/// the slug rule, 422 for content redaction refused, 409 when the note's
/// page has a conflict pending, and the store's mapped problem otherwise.
async fn note(
    State(state): State<Arc<Service>>,
    method: Method,
    headers: HeaderMap,
    Json(request): Json<NoteRequest>,
) -> Result<Json<Value>, Problem> {
    let asker = state.asker(&method, &headers).await?;
    if request.body.trim().is_empty() {
        return Err(Problem::validation_failed("`body` is required"));
    }
    let keyed = match request.slug.as_deref() {
        None => None,
        Some(slug) if is_slug(slug) => Some(slug.to_owned()),
        Some(_) => {
            return Err(Problem::validation_failed(
                "`slug` must be 1-128 lowercase ascii letters, digits or `-`",
            ));
        }
    };
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
    // A session log is a source the wiki is compiled from, not a decision,
    // and a placeholder title is replaced by one read from the note itself.
    let session = keyed.as_deref().is_some_and(is_session_slug);
    let entity_type = if session {
        EntityType::Session
    } else {
        EntityType::Decision
    };
    let title = match keyed.as_deref() {
        Some(slug) if session && is_placeholder_title(&title, slug) => one_line(&session_title(
            slug,
            request.project.as_deref().map(one_line).as_deref(),
            &clean,
        )),
        _ => title,
    };
    let title = if title.is_empty() {
        "Note".to_owned()
    } else {
        title
    };
    // The facts are checked against the format before anything is written
    // (below, once the note's reference is known), so a malformed one
    // refuses the whole note rather than leaving half of it stored.
    let facts = match request.facts {
        Some(facts) if !facts.is_empty() => Some(facts),
        _ => None,
    };
    let project = request
        .project
        .as_deref()
        .map(one_line)
        .filter(|project| !project.is_empty());

    let slug = keyed.clone().unwrap_or_else(|| note_slug(&clean));
    // A member's note is their own memory, as always; an agent account's
    // lands where a grant lets it write this slug (ADR 0004).
    let plan = state.access(&asker).await?;
    let scope = crate::recipes::new_page_scope(&plan, &asker, &slug)
        .ok_or_else(|| Problem::new(&crate::problems::SCOPE_NOT_GRANTED))?;
    let markdown = note_markdown(&title, project.as_deref(), &clean);
    let store = state.store()?;
    // A session log written before the `session` type existed is moved
    // over first, so this write keeps its history instead of being refused
    // as a change of type.
    if session {
        store
            .retype(&scope, &slug, EntityType::Decision, EntityType::Session)
            .await
            .map_err(page_write_error)?;
    }
    let head = store.read(&scope, &slug).await.map_err(page_read_error)?;
    let reference = format!("{scope}/{slug}");
    let fact_inputs = match facts {
        Some(facts) => inputs(&asker, facts, Some(&format!("note:{reference}")))?,
        None => Vec::new(),
    };
    // Stated facts land in the same scope, and need a facts grant there too.
    if !fact_inputs.is_empty()
        && !fact_inputs.iter().all(|input| {
            plan.allows(
                livingbrain_pages::access::KIND_FACTS,
                &scope,
                &livingbrain_pages::access::subject_path(&input.subject),
                cratefield_module_orgs::grants::Permission::Write,
            )
        })
    {
        return Err(Problem::new(&crate::problems::SCOPE_NOT_GRANTED));
    }
    if !fact_inputs.is_empty() {
        let checker = state.facts_store()?;
        for input in &fact_inputs {
            checker.check(input).map_err(fact_error)?;
        }
    }
    // A keyed note that already says exactly this is left alone: re-sending
    // the same session is how a sync proves it has nothing new.
    if keyed.is_some()
        && let Some(page) = &head
        && page.entity_type == entity_type
        && page.markdown == markdown
    {
        let facts = record_facts(&state, &scope, fact_inputs, &asker.user_id).await?;
        return Ok(Json(json!({
            "id": reference,
            "url": wiki_url(&state.base(), &scope, &slug),
            "version": page.version,
            "redacted": findings.len(),
            "unchanged": true,
            "facts": facts,
        })));
    }
    let meta = store
        .write(
            &scope,
            &slug,
            PageWrite {
                entity_type,
                markdown,
                author: Author::Brain { reconciles: None },
                base_version: head.map(|page| page.version),
            },
        )
        .await
        .map_err(page_write_error)?;
    if asker.is_agent() {
        state
            .audit(&asker, "note.write", &format!("page:{reference}"))
            .await;
    }

    let facts = record_facts(&state, &scope, fact_inputs, &asker.user_id).await?;
    Ok(Json(json!({
        "id": reference,
        "url": wiki_url(&state.base(), &scope, &slug),
        "version": meta.version,
        "redacted": findings.len(),
        "unchanged": false,
        "facts": facts,
    })))
}

/// The note's facts, recorded in the note's own scope; no store is built
/// for a note that states none.
async fn record_facts(
    state: &Service,
    scope: &str,
    inputs: Vec<livingbrain_pages::FactInput>,
    written_by: &str,
) -> Result<Vec<Value>, Problem> {
    if inputs.is_empty() {
        return Ok(Vec::new());
    }
    record_all(&state.facts_store()?, scope, inputs, written_by).await
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
