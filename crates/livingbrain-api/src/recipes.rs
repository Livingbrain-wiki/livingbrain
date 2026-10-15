//! The presentation recipes the first-party contract answers with.
//!
//! Mirrored from `livingbrain-mcp/src/tools.rs`, which keeps them private:
//! the same constants, the same title rule, the same citation URLs, the same
//! note slugs and the same snippets — so an agent and a person reading the
//! same page see the same thing, and a citation the CLI prints resolves to
//! the page an MCP client cited. **Change them together or not at all**;
//! the constants below must keep the original values.

use livingbrain_pages::{Page, PageStore, parse_frontmatter};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::problems::page_read_error;
use cratefield_core::Problem;

/// How many hits `GET /v1/search` returns when the caller names no limit.
/// Mirror of `DEFAULT_LIMIT` in `livingbrain-mcp/src/tools.rs`.
pub(crate) const DEFAULT_LIMIT: usize = 10;
/// The ceiling on a caller-supplied search limit. Mirror of `MAX_LIMIT`.
pub(crate) const MAX_LIMIT: usize = 25;
/// How many pages `POST /v1/ask` reads, after deduplication. Mirror of
/// `CONTEXT_PAGES`.
pub(crate) const CONTEXT_PAGES: usize = 5;
/// How much of a body a search result quotes back. Mirror of
/// `SNIPPET_CHARS`.
pub(crate) const SNIPPET_CHARS: usize = 240;
/// The longest title a note's frontmatter may carry. Mirror of
/// `MAX_TITLE_CHARS`.
pub(crate) const MAX_TITLE_CHARS: usize = 120;
/// Hex characters of the note slug's SHA-256. Mirror of `SLUG_HEX`.
pub(crate) const SLUG_HEX: usize = 12;
/// The citation URL for one page: the web app's page address under the
/// deployment's own origin (`base`, from [`Service::base`]), the same one
/// the MCP server cites ([`livingbrain_pages::page_url`]).
///
/// [`Service::base`]: crate::service::Service::base
pub(crate) fn wiki_url(base: &str, scope: &str, slug: &str) -> String {
    livingbrain_pages::page_url(base, scope, slug)
}

/// A page's title by the shared rule: frontmatter `name`, then `title`,
/// then `term`, else the slug. The store refuses a page without whichever
/// key its entity type requires, so the slug is a last resort.
pub(crate) fn title_of(page: &Page) -> String {
    page.frontmatter
        .get("name")
        .or_else(|| page.frontmatter.get("title"))
        .or_else(|| page.frontmatter.get("term"))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(&page.slug)
        .to_owned()
}

/// A page's body: the markdown after the frontmatter fence, which is what
/// a snippet quotes. A page whose fence no longer parses keeps its whole
/// markdown rather than answering nothing.
pub(crate) fn body_of(page: &Page) -> String {
    parse_frontmatter(&page.markdown, page.entity_type)
        .map(|(_, body)| body)
        .unwrap_or_else(|_| page.markdown.clone())
}

/// One page a route quoted, prepared once: the title rule, the citation URL
/// and the body without its frontmatter, resolved here so no handler
/// re-parses a page it already has.
pub(crate) struct Found {
    /// The page's scope — a route never shows it without the slug beside it.
    pub scope: String,
    /// The page's slug.
    pub slug: String,
    /// The page's title by the shared rule: frontmatter `name`, then
    /// `title`, then `term`, else the slug.
    pub title: String,
    /// The citation URL.
    pub url: String,
    /// The body after the frontmatter fence, which is what a snippet quotes.
    pub body: String,
}

impl Found {
    /// Prepares one read page, its citation URL under `base`.
    pub(crate) fn of(base: &str, page: &Page) -> Self {
        Self {
            scope: page.scope.clone(),
            slug: page.slug.clone(),
            url: wiki_url(base, &page.scope, &page.slug),
            title: title_of(page),
            body: body_of(page),
        }
    }

    /// The `scope/slug` reference, as a brief's section heading names it.
    pub(crate) fn reference(&self) -> String {
        format!("{}/{}", self.scope, self.slug)
    }
}

/// Search the asker's scopes and read back each hit — the MCP server's
/// `find`, over the same store calls with exactly the asker's scopes.
///
/// A hit whose page is gone — a scope shredded between the search and the
/// read — is skipped rather than failing the whole call; a store failure is.
///
/// # Errors
///
/// [`Problem`] from the store, mapped by [`crate::problems`].
pub(crate) async fn find(
    store: &PageStore,
    base: &str,
    scopes: &[String],
    query: &str,
    limit: usize,
) -> Result<Vec<Found>, Problem> {
    let names: Vec<&str> = scopes.iter().map(String::as_str).collect();
    let hits = store
        .search(&names, query, limit)
        .await
        .map_err(page_read_error)?;
    let mut found = Vec::with_capacity(hits.len());
    for hit in hits {
        match store.read(&hit.scope, &hit.slug).await {
            Ok(None) => {}
            Ok(Some(page)) => found.push(Found::of(base, &page)),
            Err(error) => return Err(page_read_error(error)),
        }
    }
    Ok(found)
}

/// How many search hits a project-filtered query examines before giving
/// up on finding `limit` pages filed under the project. The filter reads
/// frontmatter, which only a page read reveals, so the scan is bounded
/// rather than open-ended; it stops early as soon as `limit` pages match.
pub(crate) const PROJECT_SCAN: usize = 250;

/// [`find`], narrowed to the pages whose frontmatter names `project` — the
/// `project` a note carries in its fence, or a page written with one —
/// matched on the canonical project key ([`livingbrain_pages::project_key`]),
/// so a session filed under `EarthOS.world` and a compiled page under
/// `earthos-world` are one project. A page with no `project` key is not a
/// match: an unlabelled page answers no labelled question.
///
/// **The filter runs before the limit**, not after it: the search
/// over-fetches up to [`PROJECT_SCAN`] hits and the reads stop as soon as
/// `limit` of them match. Filtering the first `limit` hits instead would
/// answer a narrow project with nothing whenever unlabelled pages happened
/// to sort ahead of it. Without a project this is exactly [`find`].
///
/// # Errors
///
/// [`Problem`] from the store, mapped by [`crate::problems`].
pub(crate) async fn find_in_project(
    store: &PageStore,
    base: &str,
    scopes: &[String],
    query: &str,
    limit: usize,
    project: Option<&str>,
) -> Result<Vec<Found>, Problem> {
    let Some(project) = project else {
        return find(store, base, scopes, query, limit).await;
    };
    let names: Vec<&str> = scopes.iter().map(String::as_str).collect();
    let hits = store
        .search(&names, query, PROJECT_SCAN.max(limit))
        .await
        .map_err(page_read_error)?;
    let want = livingbrain_pages::project_key(project);
    let same = |named: Option<&str>| match (&want, named) {
        (Some(want), Some(named)) => livingbrain_pages::project_key(named).as_ref() == Some(want),
        (None, Some(named)) => named == project,
        (_, None) => false,
    };
    let mut found = Vec::with_capacity(limit);
    for hit in hits {
        if found.len() >= limit {
            break;
        }
        match store.read(&hit.scope, &hit.slug).await {
            Ok(None) => {}
            Ok(Some(page)) if same(page.frontmatter.get("project")) => {
                found.push(Found::of(base, &page));
            }
            Ok(Some(_)) => {}
            Err(error) => return Err(page_read_error(error)),
        }
    }
    Ok(found)
}

/// A body with its whitespace collapsed, cut to [`SNIPPET_CHARS`] on a
/// character boundary, with an ellipsis when there was more.
pub(crate) fn snippet(body: &str) -> String {
    let joined = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = joined.chars().take(SNIPPET_CHARS).collect();
    if joined.chars().nth(SNIPPET_CHARS).is_some() {
        out.push('…');
    }
    out
}

/// Whitespace collapsed to single spaces, cut to [`MAX_TITLE_CHARS`] on a
/// character boundary — the shape a frontmatter `key: value` line survives.
pub(crate) fn one_line(value: &str) -> String {
    let mut out: String = value.split_whitespace().collect::<Vec<_>>().join(" ");
    out.truncate(
        out.char_indices()
            .nth(MAX_TITLE_CHARS)
            .map_or(out.len(), |(index, _)| index),
    );
    out
}

/// The note slug for a body: SHA-256 of the *redacted* text, hex-truncated
/// to [`SLUG_HEX`]. The same redaction, the same note, the same page.
pub(crate) fn note_slug(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    let hex: String = digest
        .iter()
        .take(SLUG_HEX / 2)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("note-{hex}")
}

/// A search result, as the response carries it.
pub(crate) fn result_json(found: &Found) -> Value {
    serde_json::json!({
        "title": found.title,
        "url": found.url,
        "snippet": snippet(&found.body),
    })
}
