//! The customer-safe `public` scope (issue #51): the publication ledger, and
//! the extractive answer built from what is on it.
//!
//! **A page is public only through a publication.** An owner publishes a page
//! of the workspace's `shared` scope: its head is *copied* into the
//! workspace's `public` scope under the same slug, and a row here records who
//! did it, when, and which version. The copy is a snapshot — editing the
//! internal page later changes nothing a customer can read until the owner
//! publishes again — and a page with no row here is not public, whatever the
//! `public` scope holds. Personal and channel spaces are never a source.
//!
//! The answer is **extractive**: no model is called. [`extract`] scores the
//! passages of a published page against the question's terms and returns the
//! best ones verbatim; a question nothing covers well enough gets no answer.

use std::collections::BTreeSet;
use std::sync::Arc;

use cratefield_core::{Clock, Database, Statement};
use time::format_description::well_known::Rfc3339;

use crate::entity;
use crate::keys;
use crate::query::{QueryTerm, query_terms};
use crate::store::{PageError, int, text};

/// The longest answer text returned, in characters.
pub const MAX_ANSWER_CHARS: usize = 1_200;
/// The longest single passage, in characters; a longer paragraph is cut at a
/// sentence boundary into windows of about this size.
const PASSAGE_CHARS: usize = 600;
/// The most passages one answer joins.
pub const MAX_PASSAGES: usize = 2;
/// The share of a question's terms a passage must cover to answer it, in
/// per cent. Below it the answer is `null`: saying "I don't know" is the
/// right outcome for a question the published pages do not address.
pub const MIN_COVERAGE_PERCENT: usize = 60;

/// One publication, as the ledger holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Publication {
    /// The workspace's public page scope.
    pub scope: String,
    /// The page's slug (the same in the source scope and the public one).
    pub slug: String,
    /// Shown in a citation.
    pub title: String,
    /// Where a citation links to (http or https).
    pub url: String,
    /// The scope the copy was taken from.
    pub source_scope: String,
    /// The head version that was copied.
    pub source_version: u32,
    /// The owner who published it.
    pub published_by: String,
    /// When, RFC 3339.
    pub published_at: String,
}

/// The publication ledger.
pub struct PublicStore {
    db: Arc<dyn Database>,
    clock: Arc<dyn Clock>,
}

impl PublicStore {
    /// A ledger over the database and the clock.
    #[must_use]
    pub fn new(db: Arc<dyn Database>, clock: Arc<dyn Clock>) -> Self {
        Self { db, clock }
    }

    /// Records (or refreshes) a publication.
    ///
    /// # Errors
    ///
    /// [`PageError::InvalidScope`] / [`PageError::InvalidSlug`] outside the
    /// slug rule, and [`PageError::Store`].
    pub async fn publish(
        &self,
        workspace_id: &str,
        publication: &Publication,
    ) -> Result<(), PageError> {
        if !entity::is_slug(&publication.scope) {
            return Err(PageError::InvalidScope(publication.scope.clone()));
        }
        if !entity::is_slug(&publication.slug) {
            return Err(PageError::InvalidSlug(publication.slug.clone()));
        }
        let now = self.clock.now().format(&Rfc3339).unwrap_or_default();
        self.db
            .execute(&Statement::with_values(
                "INSERT INTO public_pages \
                   (scope, slug, workspace_id, title, url, source_scope, source_version, \
                    published_by, published_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT (scope, slug) DO UPDATE SET \
                   title = excluded.title, url = excluded.url, \
                   source_scope = excluded.source_scope, \
                   source_version = excluded.source_version, \
                   published_by = excluded.published_by, published_at = excluded.published_at",
                vec![
                    text(&publication.scope),
                    text(&publication.slug),
                    text(workspace_id),
                    text(&publication.title),
                    text(&publication.url),
                    text(&publication.source_scope),
                    int(publication.source_version),
                    text(&publication.published_by),
                    text(&now),
                ],
            ))
            .await
            .map_err(PageError::Store)?;
        Ok(())
    }

    /// Withdraws a publication. `true` when there was one. The page stops
    /// being public at once: every answer is drawn from the ledger.
    ///
    /// # Errors
    ///
    /// [`PageError::Store`].
    pub async fn unpublish(&self, scope: &str, slug: &str) -> Result<bool, PageError> {
        let removed = self
            .db
            .execute(&Statement::with_values(
                "DELETE FROM public_pages WHERE scope = ? AND slug = ?",
                vec![text(scope), text(slug)],
            ))
            .await
            .map_err(PageError::Store)?;
        Ok(removed > 0)
    }

    /// Every publication in `scope`, newest first.
    ///
    /// # Errors
    ///
    /// [`PageError::Store`].
    pub async fn list(&self, scope: &str) -> Result<Vec<Publication>, PageError> {
        let rows = self
            .db
            .query(&Statement::with_values(
                "SELECT scope, slug, title, url, source_scope, source_version, published_by, \
                        published_at \
                 FROM public_pages WHERE scope = ? ORDER BY published_at DESC, slug ASC",
                vec![text(scope)],
            ))
            .await
            .map_err(PageError::Store)?;
        Ok(rows
            .rows
            .iter()
            .map(|row| {
                let get = |name: &str| row.get::<String>(name).unwrap_or_default();
                Publication {
                    scope: get("scope"),
                    slug: get("slug"),
                    title: get("title"),
                    url: get("url"),
                    source_scope: get("source_scope"),
                    source_version: row.get::<u32>("source_version").unwrap_or_default(),
                    published_by: get("published_by"),
                    published_at: get("published_at"),
                }
            })
            .collect())
    }
}

/// One passage that answers a question, and how well.
#[derive(Debug, Clone, PartialEq)]
pub struct Passage {
    /// The passage, verbatim from the page (whitespace folded).
    pub text: String,
    /// The share of the question's terms the passage (with its heading)
    /// covers, 0..=1.
    pub coverage: f64,
}

/// The passages of a Markdown `body`, each with the heading it sits under.
fn passages(body: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut heading = String::new();
    let mut block: Vec<&str> = Vec::new();
    let flush = |block: &mut Vec<&str>, heading: &str, out: &mut Vec<(String, String)>| {
        if block.is_empty() {
            return;
        }
        let joined = block.join(" ");
        block.clear();
        for window in windows(&fold(&joined)) {
            out.push((heading.to_owned(), window));
        }
    };
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            flush(&mut block, &heading, &mut out);
        } else if let Some(title) = trimmed.strip_prefix('#') {
            flush(&mut block, &heading, &mut out);
            heading = title.trim_start_matches('#').trim().to_owned();
        } else {
            block.push(trimmed);
        }
    }
    flush(&mut block, &heading, &mut out);
    out
}

/// Whitespace folded to single spaces.
fn fold(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A paragraph cut into windows of about [`PASSAGE_CHARS`] at sentence ends.
fn windows(paragraph: &str) -> Vec<String> {
    if paragraph.chars().count() <= PASSAGE_CHARS {
        return vec![paragraph.to_owned()];
    }
    let mut out = Vec::new();
    let mut current = String::new();
    for sentence in paragraph.split_inclusive(['.', '!', '?']) {
        if !current.is_empty() && current.chars().count() + sentence.chars().count() > PASSAGE_CHARS
        {
            out.push(current.trim().to_owned());
            current.clear();
        }
        current.push_str(sentence);
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_owned());
    }
    // A run with no sentence end at all is cut by length.
    out.into_iter()
        .flat_map(|window| {
            if window.chars().count() <= PASSAGE_CHARS * 2 {
                vec![window]
            } else {
                window
                    .chars()
                    .collect::<Vec<_>>()
                    .chunks(PASSAGE_CHARS)
                    .map(|chunk| chunk.iter().collect::<String>())
                    .collect()
            }
        })
        .collect()
}

/// How many of `terms` appear among `words`.
fn covered(terms: &[QueryTerm], words: &BTreeSet<String>) -> usize {
    terms
        .iter()
        .filter(|term| words.iter().any(|word| term.matches(word)))
        .count()
}

/// The passages of one published page that answer `question`, best first,
/// each covering at least [`MIN_COVERAGE_PERCENT`] of its terms. The page's
/// `title` and the heading a passage sits under count toward coverage, but a
/// passage must itself contain at least one of the terms: a title alone never
/// answers.
#[must_use]
pub fn extract(question: &str, title: &str, markdown: &str) -> Vec<Passage> {
    let terms = query_terms(question);
    if terms.is_empty() {
        return Vec::new();
    }
    let needed = (terms.len() * MIN_COVERAGE_PERCENT).div_ceil(100).max(1);
    let body = strip_fence(markdown);
    let title_words: BTreeSet<String> = keys::tokenize(title).into_iter().collect();
    let mut found: Vec<Passage> = passages(&body)
        .into_iter()
        .filter_map(|(heading, passage)| {
            let own: BTreeSet<String> = keys::tokenize(&passage).into_iter().collect();
            if covered(&terms, &own) == 0 {
                return None;
            }
            let mut all = own;
            all.extend(keys::tokenize(&heading));
            all.extend(title_words.iter().cloned());
            let hit = covered(&terms, &all);
            (hit >= needed).then(|| Passage {
                text: passage,
                #[allow(clippy::cast_precision_loss)]
                coverage: hit as f64 / terms.len() as f64,
            })
        })
        .collect();
    found.sort_by(|a, b| {
        b.coverage
            .partial_cmp(&a.coverage)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    found
}

/// The body of a page: everything after a leading `---` fence, or the text
/// as it is. The fence is read without the entity schema, like the blind
/// index's title rows are.
fn strip_fence(markdown: &str) -> String {
    let mut lines = markdown.lines();
    if lines.next().map(str::trim_end) != Some("---") {
        return markdown.to_owned();
    }
    let rest: Vec<&str> = lines.collect();
    match rest.iter().position(|line| line.trim_end() == "---") {
        Some(end) => rest[end + 1..].join("\n"),
        None => markdown.to_owned(),
    }
}

/// Clips `text` to `max` characters on a character boundary.
#[must_use]
pub fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_owned()
    } else {
        text.chars().take(max).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = "---\ntitle: Refund policy\n---\n\n# Refunds\n\nCustomers can request a refund within 30 days of purchase. Refunds go back to the original payment method.\n\n# Shipping\n\nWe ship worldwide in 5 to 7 business days.\n";

    #[test]
    fn a_question_the_page_covers_gets_its_passage() {
        let found = extract(
            "How long do I have to request a refund?",
            "Refund policy",
            PAGE,
        );
        assert!(!found.is_empty());
        assert!(found[0].text.contains("within 30 days"));
    }

    #[test]
    fn a_question_the_page_does_not_cover_gets_nothing() {
        assert!(extract("Do you offer student discounts?", "Refund policy", PAGE).is_empty());
    }

    #[test]
    fn a_title_alone_never_answers() {
        // "policy" is in the title and nowhere in a passage.
        assert!(extract("policy", "Refund policy", PAGE).is_empty());
    }

    #[test]
    fn a_stopword_only_question_has_no_answer() {
        assert!(extract("what is it?", "Refund policy", PAGE).is_empty());
    }

    #[test]
    fn the_frontmatter_is_never_returned() {
        let found = extract("refund policy title", "x", PAGE);
        assert!(found.iter().all(|passage| !passage.text.contains("---")));
    }

    #[test]
    fn a_long_paragraph_is_cut_into_windows() {
        let sentence = "The quick brown fox jumps over the lazy dog. ";
        let long = sentence.repeat(40);
        assert!(
            windows(&fold(&long))
                .iter()
                .all(|w| w.chars().count() <= PASSAGE_CHARS + 60)
        );
    }

    #[test]
    fn clip_is_char_safe() {
        assert_eq!(clip("héllo wörld", 5), "héllo");
    }
}
