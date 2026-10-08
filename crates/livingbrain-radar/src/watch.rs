//! What the radar watches, and what it refuses to watch.
//!
//! A watch list is derived from the wiki: a page about a project is a
//! statement that this workspace cares about retrieval, ranking or whatever
//! that page is about, and a page about a customer is a statement that it
//! cares about that customer's problem. The derivation is deliberately
//! shallow — a [`PageSummary`] is a slug, an entity type and some text, not
//! a live page read — so this crate needs no store and no request context.
//!
//! **Every topic remembers its source page.** That is what makes the radar
//! page honest: "why it matters here" links back to `[[the-page]]` because
//! the topic came from there, not because a model guessed a related
//! project.
//!
//! **Mutes are per source, not per feed.** A mute names a host or a
//! [`SourceKind`] rather than a topic string, because "stop mailing me
//! Hacker News" is a decision about a publisher, not about a subject.

use std::collections::BTreeSet;

use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};

use crate::advisory::{Advisory, Dependency};
use crate::item::{Item, SourceKind};
use crate::screen::injection_in;

pub const ARXIV_API: &str = "https://export.arxiv.org/api/query";

/// The most arXiv categories one query will OR together. Past this the
/// query string stops being readable and starts being a scan.
const MAX_CATEGORIES: usize = 4;

/// The most topics one keyword query will OR together, for the same
/// reason as [`MAX_CATEGORIES`].
const MAX_KEYWORD_TOPICS: usize = 4;

/// One topic's text is capped at this many characters, because the text
/// goes into the judge's instructions and a page body pasted into a summary
/// should not become a 40k-character question.
const MAX_TOPIC_CHARS: usize = 120;

/// One thing the workspace is working on, and the page it came from.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Topic {
    /// The text handed to the judge as the question's subject. A phrase,
    /// not a keyword: the question asks "is this about {text}", so
    /// "retrieval ranking" reads better than "retrieval,ranking".
    pub text: String,
    /// The page slug this topic was derived from, if it came from one. The
    /// radar page links it; a hand-added topic has none.
    pub source: Option<String>,
}

impl Topic {
    #[must_use]
    pub fn manual(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            source: None,
        }
    }

    /// A topic derived from `slug`, matching [`WatchList::mute_topic`].
    pub(crate) fn is_muted(&self, watch: &WatchList) -> bool {
        watch.muted_topics.contains(&self.text.to_ascii_lowercase())
    }
}

/// A wiki page, reduced to what the radar needs from it. The caller builds
/// these from whatever it read; this crate never opens a store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageSummary {
    pub slug: String,
    /// The entity type's wire name. Unrecognised names contribute nothing,
    /// so a new entity type cannot silently change the watch list.
    pub entity_type: String,
    /// The page's title or keywords. Both, separated by whitespace: a page
    /// that has only one is fine.
    pub text: String,
}

impl PageSummary {
    /// A page summary from its parts.
    #[must_use]
    pub fn new(
        slug: impl Into<String>,
        entity_type: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        Self {
            slug: slug.into(),
            entity_type: entity_type.into(),
            text: text.into(),
        }
    }

    /// Whether this page's kind contributes topics. A person's page and a
    /// glossary term do not: a person is not a research subject and a
    /// definition is not a project.
    #[must_use]
    pub fn is_watchable(&self) -> bool {
        matches!(
            self.entity_type.as_str(),
            "project" | "system" | "decision" | "customer"
        )
    }
}

/// The watch list: what to look for, what to ignore, what is locked.
#[derive(Debug, Clone, Default)]
pub struct WatchList {
    topics: Vec<Topic>,
    muted_topics: BTreeSet<String>,
    muted_sources: BTreeSet<SourceKind>,
    muted_hosts: BTreeSet<String>,
    arxiv_categories: Vec<String>,
    dependencies: Vec<Dependency>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvisoryHit {
    /// The locked dependency in the vulnerable range.
    pub dependency: Dependency,
}

impl WatchList {
    /// A watch list from the wiki's pages, plus any hand-added topics.
    /// Duplicate topics (two pages about the same thing) collapse to one,
    /// keeping the first source page.
    #[must_use]
    pub fn from_pages<'a>(
        pages: impl IntoIterator<Item = &'a PageSummary>,
        manual: impl IntoIterator<Item = Topic>,
    ) -> Self {
        let mut watch = WatchList::default();
        for page in pages {
            if !page.is_watchable() {
                continue;
            }
            watch.push_topic(&page.text, Some(page.slug.clone()));
        }
        for topic in manual {
            watch.push_topic(&topic.text, topic.source);
        }
        watch
    }

    /// Adds a normalised topic unless it is empty, already watched, or is
    /// itself a prompt-injection payload.
    ///
    /// The last is the one worth stating: a topic goes into the
    /// classifier's instructions, so a page titled `ignore all previous
    /// instructions` would be a payload delivered *by the workspace to the
    /// classifier*. A page that trips [`injection_in`] contributes no topic
    /// at all — the page still exists, it is simply not watched.
    fn push_topic(&mut self, text: &str, source: Option<String>) {
        let text = normalise_topic(text);
        if text.is_empty()
            || injection_in(&text).is_some()
            || self.topics.iter().any(|t| t.text == text)
        {
            return;
        }
        self.topics.push(Topic { text, source });
    }

    /// Adds a topic by hand.
    pub fn add_topic(&mut self, topic: Topic) {
        self.push_topic(&topic.text, topic.source);
    }

    /// Stops watching `topic`, case-insensitively.
    pub fn mute_topic(&mut self, topic: &str) {
        self.muted_topics.insert(normalise_topic(topic));
    }

    /// Stops reading items from `source`, whatever it is about.
    pub fn mute_source(&mut self, source: SourceKind) {
        self.muted_sources.insert(source);
    }

    /// Stops reading items served by `host` (`example.com`, or the full
    /// host a feed's items carry).
    pub fn mute_host(&mut self, host: &str) {
        self.muted_hosts.insert(
            host.trim()
                .trim_start_matches("https://")
                .to_ascii_lowercase(),
        );
    }

    /// Watches an arXiv category (`cs.IR`, `cs.CL`), for the fetch query.
    ///
    /// The category is kept as written: arXiv's own taxonomy is
    /// case-sensitive (`cs.IR` is information retrieval, `cs.ir` is not a
    /// category at all), and a lowercased one would silently query nothing.
    pub fn watch_category(&mut self, category: &str) {
        let category = category.trim();
        if category.is_empty() || self.arxiv_categories.contains(&category.to_owned()) {
            return;
        }
        if self.arxiv_categories.len() < MAX_CATEGORIES {
            self.arxiv_categories.push(category.to_owned());
        }
    }

    /// Records the dependencies a lockfile named, so an advisory for one of
    /// them is urgent.
    pub fn lock(&mut self, dependencies: impl IntoIterator<Item = Dependency>) {
        for dependency in dependencies {
            let seen = self
                .dependencies
                .iter()
                .any(|d| d.ecosystem == dependency.ecosystem && d.name == dependency.name);
            if !seen {
                self.dependencies.push(dependency);
            }
        }
    }

    /// The topics to score against, in the order they were added.
    #[must_use]
    pub fn topics(&self) -> &[Topic] {
        &self.topics
    }

    /// Whether this item is muted by kind or by host.
    #[must_use]
    pub fn is_muted(&self, item: &Item) -> bool {
        self.muted_sources.contains(&item.source)
            || item
                .host()
                .is_some_and(|host| self.muted_hosts.contains(&host.to_ascii_lowercase()))
    }

    /// The locked dependency `advisory` is about, if the workspace has one
    /// in the vulnerable range.
    ///
    /// The package name must match *and* the locked version must fall
    /// inside the range: an advisory for a package at a version that is not
    /// shipped is not urgent, and a matching name at the wrong version is
    /// the miss this exists to avoid.
    #[must_use]
    pub fn match_advisory(&self, advisory: &Advisory) -> Option<AdvisoryHit> {
        self.dependencies
            .iter()
            .find(|dependency| {
                dependency.ecosystem == advisory.ecosystem
                    && dependency.name == advisory.package
                    && advisory.affects(&dependency.version)
            })
            .cloned()
            .map(|dependency| AdvisoryHit { dependency })
    }

    /// The arXiv query URLs for tonight, in order: one per watched category,
    /// and — when no category is watched — **one keyword query built from
    /// the wiki's own topics**.
    ///
    /// The keyword query is what makes a seeded wiki radar work at all: a
    /// workspace that has pages and has never named a category still asks
    /// arXiv for `all:"search ranking retrieval" OR all:"late interaction
    /// retrieval"`. The topics are capped at [`MAX_KEYWORD_TOPICS`] so the
    /// query stays readable, and the whole search term is percent-encoded,
    /// so a topic's spaces and quotes are arXiv's to parse and not the
    /// query string's.
    ///
    /// Empty only when there is neither a category nor a topic: an arXiv
    /// query with nothing to ask about is a scan, and the radar is not a
    /// scanner.
    pub fn arxiv_queries(&self) -> impl Iterator<Item = String> + '_ {
        let keyword = self.keyword_query();
        self.arxiv_categories
            .iter()
            .map(|category| query_url(&format!("cat:{category}")))
            .chain(keyword)
    }

    /// The one keyword query, if the wiki gave us anything to ask.
    fn keyword_query(&self) -> Option<String> {
        if self.topics.is_empty() {
            return None;
        }
        let terms: Vec<String> = self
            .topics
            .iter()
            .take(MAX_KEYWORD_TOPICS)
            .map(|topic| format!("all:\"{}\"", topic.text))
            .collect();
        Some(query_url(&terms.join(" OR ")))
    }
}

/// One arXiv query, with the search term percent-encoded.
fn query_url(search: &str) -> String {
    let encoded = utf8_percent_encode(search, NON_ALPHANUMERIC);
    format!(
        "{ARXIV_API}?search_query={encoded}&start=0&max_results=25&sortBy=submittedDate&sortOrder=descending"
    )
}

/// Collapses whitespace, folds punctuation to spaces and lowercases, so
/// `"  Search ranking: retrieval,  RAG "` and `"search ranking retrieval
/// rag"` are the same topic, the same mute, and the same readable subject
/// of the judge's question.
#[must_use]
pub fn normalise_topic(text: &str) -> String {
    let spaced: String = text
        .chars()
        .map(|ch| if ch.is_alphanumeric() { ch } else { ' ' })
        .collect();
    spaced
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
        .chars()
        .take(MAX_TOPIC_CHARS)
        .collect()
}
