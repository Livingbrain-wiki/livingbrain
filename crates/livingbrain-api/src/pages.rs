//! The page routes the web app reads and writes, merged at the pages
//! module's own mount root: `GET /v1/pages`, `GET /v1/pages/{slug}`,
//! `PUT /v1/pages/{slug}`.
//!
//! The response shape of the single-page routes is **pinned by the web
//! app** (`app/assets/app.js`, `pages.wiki`): `slug`, `title`, `markdown`,
//! `url`, `version`, `entity_type`, `backlinks` and `citations` — every one
//! of those fields is read by the app, so none may be dropped or renamed
//! without an edit there. A `PUT` answers with the same shape, re-read
//! after the write, so a save renders what the store actually kept.
//!
//! The write is the web app's optimistic-concurrency contract: an existing
//! page takes the `base_version` the caller read it at (a stale one is a
//! 409, and the app tells the editor to reload), a new page takes
//! `base_version: null`. A client-supplied `title` is merged into the
//! frontmatter fence — but a fence that already carries the page's title
//! wins, because the markdown in the same request is the newer text.

use std::collections::HashMap;
use std::sync::Arc;

use cratefield_core::axum::extract::{Path, Query, State};
use cratefield_core::axum::http::HeaderMap;
use cratefield_core::axum::routing::get;
use cratefield_core::axum::{Json, Router};
use cratefield_core::{ModuleContext, Problem};
use cratefield_kms::Kms;
use livingbrain_access::{Scope, UserId};
use livingbrain_mcp::{BearerAuth, page_scope};
use livingbrain_pages::{
    Author, EntityType, Page, PageStore, PageWrite, is_slug, parse_frontmatter,
};
use livingbrain_redact::{Policy, redact};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::problems::{
    PAGE_CONFLICT, PAGE_NOT_FOUND, PAGE_REFUSED, page_read_error, page_write_error,
    redaction_refused, slug_not_found, slug_refused,
};
use crate::recipes::{one_line, title_of, wiki_url};
use crate::service::Service;

/// How many pages `GET /v1/pages` lists when the caller names no limit.
const DEFAULT_PAGE_LIMIT: usize = 100;
/// The ceiling on a caller-supplied page count.
const MAX_PAGE_LIMIT: usize = 200;

/// The page routes, built from the pages module's own context.
///
/// `ctx` is that module's `ModuleContext`, shared with the module's nested
/// surfaces, which is the point: its `Blob` is scoped to `pages`, so a body
/// the page store writes is a body these routes can open. The venture merges this at the module's mount root via
/// `Pages::surface`, so the routes are served at `/v1/pages`.
pub fn pages_routes(
    ctx: Arc<ModuleContext>,
    kms: Arc<dyn Kms>,
    auth: Arc<dyn BearerAuth>,
) -> Router {
    let state = Arc::new(Service::for_pages(ctx, kms, auth));
    Router::new()
        .route("/", get(list))
        .route("/{slug}", get(read_one).put(edit))
        .with_state(state)
}

/// `GET /` — the heads of every page the asker can read, newest first, as
/// the store orders them.
///
/// `?limit` (default 100, capped at 200) bounds the list; a limit that does
/// not parse is a 422, not a silently different page count.
///
/// # Errors
///
/// [`crate::problems::UNAUTHORIZED`] without a credential, 422 for an
/// unparseable limit, and the store's mapped problem otherwise.
async fn list(
    State(state): State<Arc<Service>>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<Value>, Problem> {
    let asker = state.asker(&headers).await?;
    let limit = match query.get("limit") {
        None => DEFAULT_PAGE_LIMIT,
        Some(raw) => raw
            .parse::<u32>()
            .map_err(|_| {
                Problem::validation_failed(format!(
                    "`limit` must be a positive integer, got {raw:?}"
                ))
            })?
            .min(MAX_PAGE_LIMIT as u32) as usize,
    };
    let store = state.store()?;
    let scopes = state.scopes(&asker);
    let names: Vec<&str> = scopes.iter().map(String::as_str).collect();
    let listed = store.list(&names, limit).await.map_err(page_read_error)?;
    let pages: Vec<Value> = listed
        .iter()
        .map(|summary| {
            json!({
                "scope": summary.scope,
                "slug": summary.slug,
                "entity_type": summary.entity_type.as_str(),
                "version": summary.version,
                "updated_at": summary.updated_at,
                "url": wiki_url(&summary.scope, &summary.slug),
            })
        })
        .collect();
    Ok(Json(json!({ "pages": pages })))
}

/// `GET /{slug}` — one page, in the shape the web app pins.
///
/// The scopes are read most specific first, so a private page wins over a
/// shared one of the same slug; a slug that fails the slug rule and a slug
/// nobody can read are the same 404.
///
/// # Errors
///
/// [`crate::problems::PAGE_NOT_FOUND`] for an unknown or impossible slug,
/// and the store's mapped problem otherwise.
async fn read_one(
    State(state): State<Arc<Service>>,
    headers: HeaderMap,
    Path(slug): Path<String>,
) -> Result<Json<Value>, Problem> {
    let asker = state.asker(&headers).await?;
    if !is_slug(&slug) {
        return Err(slug_not_found());
    }
    let store = state.store()?;
    let scopes = state.scopes(&asker);
    let page = read_first(&store, &scopes, &slug)
        .await?
        .ok_or_else(|| Problem::new(&PAGE_NOT_FOUND))?;
    Ok(Json(page_view(&store, &page).await?))
}

/// The first readable page of `slug` across the asker's scopes, most
/// specific first — the MCP server's `brain_page` rule: a direct read per
/// scope in order, and the first hit wins.
async fn read_first(
    store: &PageStore,
    scopes: &[String],
    slug: &str,
) -> Result<Option<Page>, Problem> {
    for scope in scopes {
        match store.read(scope, slug).await {
            Ok(Some(page)) => return Ok(Some(page)),
            Ok(None) => {}
            Err(error) => return Err(page_read_error(error)),
        }
    }
    Ok(None)
}

/// One page, as the web app pins the shape: the markdown, the title by the
/// shared rule, where the site serves it, the version, and what links in
/// and out.
///
/// # Errors
///
/// [`Problem::internal`] when the backlink query fails — the page itself is
/// already read, so a `PageError` here is infrastructure.
async fn page_view(store: &PageStore, page: &Page) -> Result<Value, Problem> {
    let backlinks = store
        .backlinks(&page.scope, &page.slug)
        .await
        .map_err(page_read_error)?;
    let citations: Vec<Value> = page
        .links
        .iter()
        .map(|link| {
            json!({
                "title": link,
                "url": wiki_url(&page.scope, link),
            })
        })
        .collect();
    Ok(json!({
        "slug": page.slug,
        "title": title_of(page),
        "markdown": page.markdown,
        "url": wiki_url(&page.scope, &page.slug),
        "version": page.version,
        "entity_type": page.entity_type.as_str(),
        "backlinks": backlinks,
        "citations": citations,
    }))
}

/// A `PUT /{slug}` body, as the web app sends it. A `base_version` of
/// `null` (or absent) creates; a number edits.
#[derive(Debug, Deserialize)]
struct PagePut {
    markdown: String,
    #[serde(default)]
    base_version: Option<u32>,
    #[serde(default)]
    title: Option<String>,
}

/// `PUT /{slug}` — create a page (`base_version: null`) or edit one (the
/// version the caller read it at).
///
/// **Markdown and title are both redacted before anything is looked up**:
/// nothing a client sends reaches the store with a secret still in it.
/// The page a slug resolves to decides the scope and the entity type — an
/// edit can never move a page into a scope the caller named, and a new
/// page lands in the caller's own scope as a `Decision` (the one entity
/// type whose only required key is a `title`).
///
/// # Errors
///
/// 401 without a credential, 422 for a slug outside the slug rule, redaction
/// refusals and new-page-with-base, 409 for the conflict family, and the
/// store's mapped problem otherwise.
async fn edit(
    State(state): State<Arc<Service>>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Json(body): Json<PagePut>,
) -> Result<Json<Value>, Problem> {
    let asker = state.asker(&headers).await?;
    if !is_slug(&slug) {
        return Err(slug_refused());
    }
    // Redaction first: a secret never reaches a lookup, let alone a body.
    let (clean, _) = redact(&body.markdown, Policy::Redact).map_err(|_| redaction_refused())?;
    // A title is body text the client chose; a title redaction refuses is
    // read as no title, the MCP server's rule for note titles.
    let title = match body.title.as_deref() {
        Some(raw) => match redact(raw, Policy::Redact) {
            Ok((clean, _)) => {
                let line = one_line(&clean);
                (!line.is_empty()).then_some(line)
            }
            Err(_) => None,
        },
        None => None,
    };

    let store = state.store()?;
    let scopes = state.scopes(&asker);
    let existing = read_first(&store, &scopes, &slug).await?;

    let (scope, entity_type, markdown, base_version) = match existing {
        Some(page) => {
            // An edit must say what it read: a null base_version here means
            // the caller never saw the page, and writing would be a bet
            // against whoever did.
            let base = body.base_version.ok_or_else(|| {
                Problem::new(&PAGE_CONFLICT).with_detail(
                    "nothing was written: that page already exists. Reload it to get \
                     the current version, then send that version as base_version.",
                )
            })?;
            let markdown = match title.as_deref() {
                Some(title) => with_merged_title(&clean, page.entity_type, title)?,
                None => clean,
            };
            (page.scope, page.entity_type, markdown, Some(base))
        }
        None => {
            if body.base_version.is_some() {
                return Err(Problem::new(&PAGE_REFUSED).with_detail(
                    "a new page cannot carry a base_version; leave it null to create one",
                ));
            }
            let scope = page_scope(
                &asker.workspace_id,
                &Scope::User(UserId::new(asker.user_id.clone())),
            );
            // A new page's title goes in the fence it needs anyway; a body
            // that carries its own fence keeps it.
            let markdown = match title.as_deref() {
                Some(title) if !has_frontmatter_fence(&clean) => {
                    format!("---\ntitle: {title}\n---\n\n{clean}")
                }
                _ => clean,
            };
            (scope, EntityType::Decision, markdown, None)
        }
    };

    store
        .write(
            &scope,
            &slug,
            PageWrite {
                entity_type,
                markdown,
                author: Author::Human {
                    id: asker.user_id.clone(),
                },
                base_version,
            },
        )
        .await
        .map_err(page_write_error)?;

    // Re-read, so the answer carries the backlinks and citations of the
    // version the store actually kept — the same shape `GET` answers with.
    let page = store
        .read(&scope, &slug)
        .await
        .map_err(page_read_error)?
        .ok_or_else(Problem::internal)?;
    Ok(Json(page_view(&store, &page).await?))
}

/// Whether a body opens with a closed `---` frontmatter fence.
fn has_frontmatter_fence(markdown: &str) -> bool {
    let mut lines = markdown.split_inclusive('\n');
    let Some(first) = lines.next() else {
        return false;
    };
    if first.trim_end_matches(['\r', '\n']) != "---" {
        return false;
    }
    lines.any(|line| line.trim_end_matches(['\r', '\n']) == "---")
}

/// Merges a client-supplied title into a page's frontmatter.
///
/// A fence that already carries the page's title — under `title`, or under
/// the `name`/`term` key its entity type requires — wins over the request's
/// `title`: the markdown in the same request is the newer text, and the web
/// app always sends the title it *displayed*, which a fence edit has just
/// made stale. A body without a fence gets one. The merged body is
/// validated against the entity type, so a title an entity type does not
/// take (a `title:` on a Person page) is a 422 before the store ever sees it.
///
/// # Errors
///
/// [`crate::problems::PAGE_REFUSED`] when the body or the merge does not
/// fit the entity type's frontmatter.
fn with_merged_title(
    markdown: &str,
    entity_type: EntityType,
    title: &str,
) -> Result<String, Problem> {
    if !has_frontmatter_fence(markdown) {
        let merged = format!("---\ntitle: {title}\n---\n\n{markdown}");
        return validate_title_merge(&merged, entity_type);
    }
    let (frontmatter, _) = parse_frontmatter(markdown, entity_type)
        .map_err(|error| Problem::new(&PAGE_REFUSED).with_detail(error.to_string()))?;
    let already_titled = ["title", "name", "term"].iter().any(|key| {
        frontmatter
            .get(key)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .is_some()
    });
    if already_titled {
        return Ok(markdown.to_owned());
    }
    let merged = insert_title_after_fence(markdown, title);
    validate_title_merge(&merged, entity_type)
}

/// Validates a merged body, as the store will — but here, so the refusal is
/// this route's problem and not a half-applied write.
fn validate_title_merge(markdown: &str, entity_type: EntityType) -> Result<String, Problem> {
    parse_frontmatter(markdown, entity_type)
        .map(|_| markdown.to_owned())
        .map_err(|error| Problem::new(&PAGE_REFUSED).with_detail(error.to_string()))
}

/// Puts one `title:` line into an existing fence, right after the opening
/// `---`. The caller has checked there is a closing fence and no title key.
fn insert_title_after_fence(markdown: &str, title: &str) -> String {
    let mut merged = String::with_capacity(markdown.len() + title.len() + 10);
    let mut lines = markdown.split_inclusive('\n');
    merged.push_str(lines.next().unwrap_or("---\n"));
    merged.push_str(&format!("title: {title}\n"));
    merged.extend(lines);
    merged
}
