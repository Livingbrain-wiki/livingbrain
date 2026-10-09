//! The presentation recipes the first-party contract answers with.
//!
//! Mirrored from `livingbrain-mcp/src/tools.rs`, which keeps them private:
//! the same constants, the same title rule, the same citation URLs, the same
//! note slugs and the same snippets — so an agent and a person reading the
//! same page see the same thing, and a citation the CLI prints resolves to
//! the page an MCP client cited. **Change them together or not at all**;
//! the constants below must keep the original values.

use livingbrain_pages::{Frontmatter, Page, PageStore, parse_frontmatter};
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
/// The site's wiki root, which every citation URL is built against. Mirror
/// of `WIKI_BASE`.
pub(crate) const WIKI_BASE: &str = "https://livingbrain.wiki";

/// The citation URL for one page: the same address the site serves it at,
/// and the same one the MCP server cites.
pub(crate) fn wiki_url(scope: &str, slug: &str) -> String {
    format!("{WIKI_BASE}/brain/{scope}/{slug}")
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
    /// The parsed frontmatter, which the project filter reads.
    pub frontmatter: Frontmatter,
}

impl Found {
    /// Prepares one read page.
    pub(crate) fn of(page: &Page) -> Self {
        Self {
            scope: page.scope.clone(),
            slug: page.slug.clone(),
            url: wiki_url(&page.scope, &page.slug),
            title: title_of(page),
            body: body_of(page),
            frontmatter: page.frontmatter.clone(),
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
            Ok(Some(page)) => found.push(Found::of(&page)),
            Err(error) => return Err(page_read_error(error)),
        }
    }
    Ok(found)
}

/// Keeps only the pages whose frontmatter names `project`, exactly — the
/// `project` a note carries in its fence, or a page written with one. A
/// page with no `project` key is not a match: an unlabelled page answers no
/// labelled question.
pub(crate) fn keep_project(found: Vec<Found>, project: Option<&str>) -> Vec<Found> {
    match project {
        Some(project) => found
            .into_iter()
            .filter(|page| page.frontmatter.get("project") == Some(project))
            .collect(),
        None => found,
    }
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
