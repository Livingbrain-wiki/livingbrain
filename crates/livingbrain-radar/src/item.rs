//! What a source published: one item, and the arXiv Atom reader that
//! produces them.
//!
//! The [`Item`] type is **wider than this crate's fetcher**. arXiv is the
//! only source fetched here, but [`SourceKind`] names the others a caller
//! will want, and an item from any of them travels the same path: screened,
//! judged, read, paged. A future RSS or GitHub-releases fetcher is an
//! addition to this module, not a change to the run.
//!
//! **The Atom reader is hand-rolled on purpose.** The five basic XML
//! entities and one element per field are all arXiv's Atom feed is, and a
//! full XML parser would be a dependency in a Worker binary for the sake of
//! a document whose shape is fixed. What it does not read it does not guess
//! at: an element it does not know is skipped whole, and a document that
//! yields no entry is nothing rather than a half-read page.
//!
//! **CDATA is not unwrapped.** arXiv's feed is character data in ordinary
//! elements, and a `<![CDATA[…]]>` section would arrive here as a wrapper
//! this reader does not strip. That is not a hole: the screen treats
//! `<…>` as markup and checks the text inside it as text, so a payload
//! wrapped in CDATA is quarantined rather than read.
//!
//! # Politeness
//!
//! [`crate::USER_AGENT`] is fixed and identifying on every request, and a
//! run makes at most [`crate::MAX_ARXIV_QUERIES`] of them (a caller may
//! lower that further through [`crate::RadarBudget::max_requests`]).
//! arXiv asks for three seconds between requests; four requests over a
//! whole night is far inside that, which is the point of the cap.
//! **robots.txt parsing, conditional requests and per-request pacing are
//! follow-ups** — the cap and the User-Agent are the whole contract today.

use crate::advisory::Advisory;

pub const MAX_FEED_BYTES: usize = 256 * 1024;

/// Where an item came from. The set is closed so a page can name its
/// source, and open in practice: a caller may hand this crate an item from
/// a source this crate does not fetch yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SourceKind {
    /// arXiv, the one fetcher here.
    Arxiv,
    /// An RSS or Atom feed a caller reads.
    Feed,
    /// A Hacker News story a caller reads.
    HackerNews,
    /// A GitHub release a caller reads.
    GithubRelease,
    /// A security advisory, matched against a lockfile.
    Advisory,
}

impl SourceKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Arxiv => "arxiv",
            Self::Feed => "feed",
            Self::HackerNews => "hackernews",
            Self::GithubRelease => "github-release",
            Self::Advisory => "advisory",
        }
    }

    #[must_use]
    pub fn host(self) -> Option<&'static str> {
        match self {
            Self::Arxiv => Some("arxiv.org"),
            Self::HackerNews => Some("news.ycombinator.com"),
            Self::GithubRelease | Self::Advisory => Some("github.com"),
            Self::Feed => None,
        }
    }
}

/// One published item: an advisory's fields are held by its [`Advisory`]
/// rather than smeared across this struct, so a feed item and an advisory
/// stay distinguishable at the type level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub source: SourceKind,
    /// The source's own id — the arXiv id, a feed's guid, a release tag.
    /// Also the seen key, so it must be stable across runs.
    pub id: String,
    pub title: String,
    pub url: String,
    pub summary: String,
    /// The feed host, when it is not the source's default. Used for mutes.
    pub feed_host: Option<String>,
    pub advisory: Option<Advisory>,
}

impl Item {
    #[must_use]
    pub fn new(
        source: SourceKind,
        id: impl Into<String>,
        title: impl Into<String>,
        url: impl Into<String>,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            source,
            id: id.into(),
            title: title.into(),
            url: url.into(),
            summary: summary.into(),
            feed_host: None,
            advisory: None,
        }
    }

    #[must_use]
    pub fn from_advisory(advisory: Advisory) -> Self {
        Self {
            source: SourceKind::Advisory,
            id: advisory.id.clone(),
            title: advisory.title.clone(),
            url: advisory.url.clone(),
            summary: advisory.summary.clone(),
            feed_host: None,
            advisory: Some(advisory),
        }
    }

    /// The feed host: the one this item carries, or its source's default.
    #[must_use]
    pub fn host(&self) -> Option<&str> {
        self.feed_host.as_deref().or_else(|| self.source.host())
    }

    #[must_use]
    pub fn advisory(&self) -> Option<&Advisory> {
        self.advisory.as_ref()
    }

    /// The seen key: source and id, so the same paper from two sources is
    /// two items and the same paper from one source is one.
    #[must_use]
    pub fn key(&self) -> String {
        format!("{}:{}", self.source.as_str(), self.id)
    }

    #[must_use]
    pub fn scored_text(&self) -> String {
        if self.summary.trim().is_empty() {
            self.title.clone()
        } else {
            format!("{}\n\n{}", self.title, self.summary)
        }
    }
}

/// Reads an arXiv Atom feed into items.
///
/// Never fails on a document it half understands: an `<entry>` missing an
/// id is skipped, and one missing a title keeps the one it has. A document
/// with no entries yields nothing, which the run treats as "nothing
/// published", not as an error — an empty night is a normal night.
#[must_use]
pub fn parse_atom(body: &str) -> Vec<Item> {
    body.split("<entry>")
        .skip(1)
        .filter_map(|chunk| {
            let chunk = chunk.split("</entry>").next().unwrap_or(chunk);
            let id = text_of(chunk, "id")?;
            let title = text_of(chunk, "title").unwrap_or_default();
            let summary = text_of(chunk, "summary").unwrap_or_default();
            // arXiv's `<id>` is the API's own URL; the human page is the
            // `/abs/` form, which is what a reader should be sent to.
            let url = text_of(chunk, "link")
                .filter(|href| !href.is_empty())
                .unwrap_or_else(|| id.clone());
            Some(Item::new(
                SourceKind::Arxiv,
                arxiv_id(&id),
                title,
                url,
                summary,
            ))
        })
        .collect()
}

fn arxiv_id(id: &str) -> String {
    id.rsplit("abs/")
        .next()
        .unwrap_or(id)
        .trim_end_matches('/')
        .to_owned()
}

fn text_of(chunk: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let start = chunk.find(&open)? + open.len();
    let rest = &chunk[start..];
    let end = rest.find(&format!("</{name}>"))?;
    Some(decode_entities(rest[..end].trim()))
}

/// Decodes the five XML entities arXiv's Atom feed uses, and numeric
/// character references, in one left-to-right pass. Anything else stays as
/// written: a `&foo;` that is not an entity is text, and silently dropping
/// it would change what the classifier reads.
#[must_use]
pub fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        let Some(end) = rest.find(';').filter(|end| *end <= 10) else {
            // No closing `;` near enough to be an entity: the `&` is text.
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        match entity {
            "amp" => out.push('&'),
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "quot" => out.push('"'),
            "apos" => out.push('\''),
            _ => match numeric_reference(entity) {
                Some(ch) => out.push(ch),
                None => out.push_str(&rest[..=end]),
            },
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

fn numeric_reference(entity: &str) -> Option<char> {
    let digits = entity.strip_prefix('#')?;
    let (digits, radix) = match digits.strip_prefix(['x', 'X']) {
        Some(hex) => (hex, 16),
        None => (digits, 10),
    };
    if digits.is_empty() {
        return None;
    }
    let code = u32::from_str_radix(digits, radix).ok()?;
    char::from_u32(code).filter(|ch| !ch.is_control() && *ch != '\u{fffd}')
}
