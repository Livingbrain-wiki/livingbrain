//! The answer seam: a cited answer to a question, from the asker's own pages.
//!
//! [`Answers`] is what a chat surface asks when somebody says the brain's
//! name. It is a trait rather than a [`PageStore`] call because the caller is
//! a different module: the Slack agent in `livingbrain-workspaces`
//! (issue #123) has no page store and must not be handed one — a page store
//! is a tenant-wide capability, and handing one across a module boundary is
//! how "read only what the asker may read" stops being true.
//!
//! [`PageAnswers`] is the implementation over the pages. It reads only the
//! scopes [`page_scopes_for`] grants the asker, and **when nothing matches it
//! says so** with no citation attached (issue #123): a cited answer nobody
//! wrote is worse than no answer, because it is indistinguishable from a
//! real one at a glance.

use std::sync::Arc;

use async_trait::async_trait;
use livingbrain_access::Location;
use livingbrain_redact::{Policy, redact};

use crate::scope::page_scopes_for;
use crate::store::{Page, PageStore};
use crate::{Frontmatter, parse_frontmatter};

/// How many pages one answer may lean on.
const ANSWER_PAGES: usize = 5;
/// How much of a body an answer quotes.
const SNIPPET_CHARS: usize = 240;
/// The longest title an answer may print, so a page's own frontmatter
/// cannot make a Slack message unreadable.
const MAX_TITLE_CHARS: usize = 120;

/// The person an answer is for, the workspace they are in, and where they
/// asked.
///
/// Deliberately **not** the MCP server's `Asker`: that one also carries a
/// `BearerAuth` identity and belongs to a surface that is going away. This is
/// what the answer itself needs, and nothing else.
///
/// The location is not decoration. A question asked in a public channel is
/// granted shared pages only, and one asked in a DM is granted the asker's
/// own — so the caller, who is the only party that knows where the question
/// was asked, decides it (issue #106).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asker {
    /// The workspace the question was asked in.
    pub workspace_id: String,
    /// The person who asked, in that workspace.
    pub user_id: String,
    location: Location,
}

impl Asker {
    /// An asker who asked in a direct message.
    ///
    /// **The default is the risky end of [`Location`], deliberately.**
    /// `Dm` grants the asker's own pages *plus* the shared ones, where
    /// `PublicChannel` grants the shared ones only — so forgetting
    /// [`Asker::at`] widens the grant rather than narrowing it, and a caller
    /// who never learned where the question was asked answers from more than
    /// the asker could have read where they were standing. Every caller
    /// passes `.at(..)` immediately after: the Slack agent knows the message
    /// was a DM or a channel message (issue #123), and the MCP server's own
    /// asker is a `BearerAuth` identity whose conversation is a DM by
    /// construction. This default exists so `Asker` cannot be constructed
    /// with no location at all, not so a caller can skip the decision.
    #[must_use]
    pub fn new(workspace_id: impl Into<String>, user_id: impl Into<String>) -> Self {
        Self {
            workspace_id: workspace_id.into(),
            user_id: user_id.into(),
            location: Location::Dm,
        }
    }

    /// The same asker, asking somewhere else.
    #[must_use]
    pub fn at(mut self, location: Location) -> Self {
        self.location = location;
        self
    }
}

/// One page an answer leans on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Citation {
    /// The page's title, as its frontmatter names it.
    pub title: String,
    /// Where the page is, absolute — the surface renders it as a link.
    pub url: String,
}

/// An answer and the pages it came from.
///
/// The citations are a field rather than part of the text because the caller
/// renders them: Slack wants link blocks, a future surface wants a footnote
/// list, and neither should have to re-parse prose to find them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answered {
    /// The words to send. Say plainly when nothing was found.
    pub text: String,
    /// What the words came from. Empty when nothing was found.
    pub citations: Vec<Citation>,
}

/// Why a question could not be answered.
///
/// Every variant is a refusal to say anything rather than an answer with a
/// hole in it: a caller that gets an `Err` sends nothing, and one that gets
/// an `Answered` with no citations says it found nothing. Neither fabricates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerError {
    /// The question, or what came back, was something redaction refused to
    /// handle. Answering from it would mean putting what was refused into a
    /// search.
    Redacted,
    /// The pages could not be read. Not the asker's business, and not
    /// something a chat message should carry.
    Unavailable,
}

/// Answers a question from the pages the asker may read.
#[async_trait]
pub trait Answers: Send + Sync {
    /// The answer, with its citations.
    ///
    /// # Errors
    ///
    /// [`AnswerError`] when there is no answer to give.
    async fn answer(&self, asker: &Asker, question: &str) -> Result<Answered, AnswerError>;
}

/// [`Answers`] over a [`PageStore`], for one site.
///
/// `base` is a parameter rather than a constant because the MCP server and
/// the deployment do not necessarily publish pages at the same origin: a
/// composition that serves its own wiki passes its own.
///
/// Where the question was asked is **not** a parameter here: it arrives on
/// the [`Asker`], because the caller is the only party that knows it.
pub struct PageAnswers {
    store: Arc<PageStore>,
    base: String,
}

impl PageAnswers {
    /// Answers from `store`, citing pages under `base`.
    ///
    /// The store is taken as an `Arc` because a composition builds it inside
    /// a closure that runs per request and an agent holds the answer across
    /// many: two owners of one store, one set of sealed keys.
    #[must_use]
    pub fn new(store: impl Into<Arc<PageStore>>, base: impl Into<String>) -> Self {
        Self {
            store: store.into(),
            base: base.into(),
        }
    }
}

#[async_trait]
impl Answers for PageAnswers {
    async fn answer(&self, asker: &Asker, question: &str) -> Result<Answered, AnswerError> {
        // **Redact before the words become a query** (issue #108). A secret
        // in the question would otherwise be written into the blind index as
        // a keyed HMAC of itself, and the redacted question is also what
        // gets quoted back in the answer.
        let (query, findings) =
            redact(question, Policy::Redact).map_err(|_| AnswerError::Redacted)?;
        let redacted = findings.len();
        if query.trim().is_empty() {
            return Ok(nothing(redacted));
        }
        let scopes = page_scopes_for(
            &asker.workspace_id,
            &asker.user_id,
            asker.location.clone(),
            &livingbrain_access::ChannelMemberships::new(),
        );
        let names: Vec<&str> = scopes.iter().map(String::as_str).collect();
        let hits = self
            .store
            .search(&names, &query, ANSWER_PAGES)
            .await
            .map_err(|_| AnswerError::Unavailable)?;

        let mut text = String::new();
        let mut citations = Vec::with_capacity(hits.len());
        for hit in hits {
            // A hit whose page went away between the search and the read is
            // skipped, exactly as the MCP tools skip it.
            let Ok(Some(page)) = self.store.read(&hit.scope, &hit.slug).await else {
                continue;
            };
            let quoted = quote(&page);
            text.push_str(&format!(
                "\n[{}] {}\n\n{}\n",
                citations.len() + 1,
                quoted.title,
                quoted.body
            ));
            citations.push(Citation {
                title: quoted.title,
                url: format!("{}/brain/{}/{}", self.base, hit.scope, hit.slug),
            });
        }
        if citations.is_empty() {
            return Ok(nothing(redacted));
        }
        let mut said = format!(
            "From the pages you can read:\n{text}\nAnswer from those pages, and say so \
             if they do not settle it."
        );
        if redacted > 0 {
            said.push_str(&format!(
                "\n\nNote: {redacted} sensitive span(s) were removed from your question \
                 before it was looked up, so this may not be the answer you wanted."
            ));
        }
        Ok(Answered {
            text: said,
            citations,
        })
    }
}

/// The answer to "I looked and there is nothing": plain, cited by nothing.
fn nothing(redacted: usize) -> Answered {
    let mut said =
        "Nothing in the pages you can read answers that. Try fewer, more specific words, \
         or say what you were trying to do."
            .to_owned();
    if redacted > 0 {
        said.push_str(&format!(
            " ({redacted} sensitive span(s) were removed from your question before it \
             was looked up.)"
        ));
    }
    Answered {
        text: said,
        citations: Vec::new(),
    }
}

/// One page prepared for printing: its title, and a bounded piece of its
/// body.
struct Quoted {
    title: String,
    body: String,
}

/// The title a page is known by, and the part of its body an answer quotes.
///
/// The same frontmatter keys and the same bound as the MCP tools, so a page
/// reads the same whichever surface cites it.
fn quote(page: &Page) -> Quoted {
    let (_, body) = parse_frontmatter(&page.markdown, page.entity_type)
        .unwrap_or_else(|_| (Frontmatter::default(), page.markdown.clone()));
    let title = page
        .frontmatter
        .get("name")
        .or_else(|| page.frontmatter.get("title"))
        .or_else(|| page.frontmatter.get("term"))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(page.slug.as_str())
        .chars()
        .take(MAX_TITLE_CHARS)
        .collect::<String>();
    Quoted {
        title,
        body: snippet(&body),
    }
}

/// The first [`SNIPPET_CHARS`] characters of a body on one line, so a long
/// page cannot push the answer out of a Slack message.
fn snippet(body: &str) -> String {
    let flat = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= SNIPPET_CHARS {
        return flat;
    }
    let mut out: String = flat.chars().take(SNIPPET_CHARS).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_body_is_bounded_to_one_line() {
        let body = format!("{} tail", "word ".repeat(SNIPPET_CHARS));
        let quoted = snippet(&body);
        assert!(quoted.chars().count() <= SNIPPET_CHARS + 1);
        assert!(!quoted.contains('\n'));
        assert!(quoted.ends_with('…'));
    }

    #[test]
    fn nothing_found_is_plain_and_cites_nothing() {
        let answered = nothing(0);
        assert!(answered.citations.is_empty());
        assert!(answered.text.contains("Nothing in the pages you can read"));
    }
}
