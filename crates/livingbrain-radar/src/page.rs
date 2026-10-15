//! One page per surviving item: what it is, why it matters here, how it
//! could be used, what it would cost, and where it came from.
//!
//! **Attribution is not decoration.** Every section carries the source URL,
//! and every claim *about the published work* is prefixed "The paper
//! reports" or attributed to the advisory. What follows it — how it could
//! be used here, what it would cost, what could go wrong — is the model's
//! reading and is marked as such. The distinction is the page's whole
//! value: a reader deciding whether to trust a number needs to know which
//! sentences a paper's authors wrote and which ones this workspace wrote
//! at 3am.
//!
//! **The slug** is `radar-<sanitised id>`, which satisfies the pages
//! module's rule (lowercase ascii, digits and `-`, at most
//! [`MAX_SLUG_LEN`] characters) without the radar naming that crate —
//! [`sanitise`] is the rule spelled out here, and the tests assert it
//! against the real `is_slug`, because a radar that silently produced an
//! unpaginatable slug would be a slow failure.
//!
//! **The entity type** is [`EntityType::Radar`](
//! https://github.com/Livingbrain-wiki/livingbrain), not a glossary term:
//! a radar page is a thing published elsewhere that this workspace
//! watches, and its frontmatter (`source`, `url`, `topic`, `score`,
//! `urgent`) is a different shape from a definition's. Adding it was four
//! arms in the pages module and no migration — `entity_type` is a `TEXT`
//! column with a comment naming the types, not a `CHECK` constraint.
//!
//! **Untrusted text cannot write a link.** The title, the summary, the
//! topic and the model's reading are all prose from outside this
//! workspace, so every one of them goes through [`prose`], which collapses
//! whitespace and escapes `[` and `]` — a paper titled
//! `Poisoned [[admin-runbook]]` yields a page whose only link is the one
//! this crate intended. The one link it does write, `[[slug]]`, names the
//! wiki page the topic came from. The url is the other half of the same
//! problem: it is validated by the screen, but a validated url may still
//! contain the characters Markdown reads as link syntax, so it is
//! percent-encoded for every target it lands in ([`link_target`]).

use crate::Reading;
use crate::advisory::Advisory;
use crate::item::Item;
use crate::watch::Topic;

pub const MAX_SLUG_LEN: usize = 128;

pub const ENTITY_TYPE: &str = "radar";

/// A rendered radar page, ready to be written by the nightly pass.
#[derive(Debug, Clone, PartialEq)]
pub struct RadarPage {
    pub slug: String,
    pub markdown: String,
    /// The entity type this page is, named as the wire string so the
    /// nightly pass can hand it to `PageWrite` without this crate
    /// depending on a module.
    pub entity_type: &'static str,
    pub topic: String,
    pub score: f32,
    pub urgent: bool,
    pub url: String,
}

/// Renders the page for one surviving item.
///
/// `reading` is the model's **screened** answer: every field of it, and
/// every untrusted field of the item, is rendered through [`prose`]. A
/// reader that returned nothing still gets a page — the sections say so
/// rather than inventing text, because an empty section a person can see
/// is better than a confident sentence nobody wrote.
#[must_use]
pub fn render(
    item: &Item,
    topic: &Topic,
    score: f32,
    urgent: bool,
    reading: &Reading,
) -> RadarPage {
    let target = link_target(&item.url);
    let cite = format!("[source]({target})");
    let mut markdown = format!(
        "---\ntype: {ENTITY_TYPE}\ntitle: {}\nsource: {}\nurl: {}\ntopic: {}\nscore: {score:.2}\nurgent: {}\n---\n\n",
        frontmatter_value(&item.title),
        item.source.as_str(),
        target,
        frontmatter_value(&topic.text),
        if urgent { "yes" } else { "no" },
    );
    markdown.push_str(&format!("# {}\n\n", prose(&item.title)));

    markdown.push_str("## What it is\n\n");
    markdown.push_str(&match item.advisory() {
        Some(advisory) => advisory_what(advisory),
        None => format!(
            "The paper reports:\n\n{}\n\nCite this instead of the summary: {cite}.\n\n",
            prose(&item.summary)
        ),
    });

    markdown.push_str("## Why it matters here\n\n");
    match &topic.source {
        Some(slug) => markdown.push_str(&format!(
            "This workspace watches *{}* because of [[{}]]. {cite}.\n\n",
            prose(&topic.text),
            sanitise(slug)
        )),
        None => markdown.push_str(&format!(
            "This workspace watches *{}.* {cite}.\n\n",
            prose(&topic.text)
        )),
    }

    markdown.push_str("## How it could be used here\n\n");
    markdown.push_str(&if reading.use_here.trim().is_empty() {
        format!("Nothing concrete came back from the reading pass tonight. {cite}.\n\n")
    } else {
        format!(
            "The reading pass suggests: {}. This is the workspace's reading, not the source's; {cite}.\n\n",
            sentence(&reading.use_here)
        )
    });

    markdown.push_str("## Effort and risk\n\n");
    markdown.push_str(&format!(
        "Adopting it looks {} of work, with {} risk — both this workspace's reading, not the source's. {cite}.\n\n",
        labelled(&prose(&reading.effort), "not assessed"),
        labelled(&prose(&reading.risk), "not assessed"),
    ));
    markdown.push_str(&if reading.worth_read {
        format!("**Recommendation:** read the full work, not just the abstract. {cite}.\n\n")
    } else {
        String::from(
            "**Recommendation:** the abstract was enough to judge it; reading the whole work is optional.\n\n",
        )
    });

    markdown.push_str("## Sources\n\n");
    markdown.push_str(&format!(
        "- [{}]({target}) — the only source for this page.\n",
        item.source.as_str(),
    ));
    if let Some(advisory) = item.advisory() {
        markdown.push_str(&format!(
            "- The affected range is `{}` for `{}`.\n",
            prose(&advisory.vulnerable),
            prose(&advisory.package)
        ));
    }

    RadarPage {
        slug: slug_for(item),
        markdown,
        entity_type: ENTITY_TYPE,
        topic: topic.text.clone(),
        score,
        urgent,
        url: item.url.clone(),
    }
}

fn advisory_what(advisory: &Advisory) -> String {
    format!(
        "The advisory reports a vulnerability in `{}` ({}) in versions `{}`. \
         Read the advisory for the detail: [source]({}).\n\n{}\n\n",
        prose(&advisory.package),
        advisory.ecosystem.as_str(),
        prose(&advisory.vulnerable),
        link_target(&advisory.url),
        prose(&advisory.summary),
    )
}

/// A url as a Markdown link target: the three characters that end a link
/// or begin one — `(`, `)` and whitespace — percent-encoded, so a url like
/// `https://en.wikipedia.org/wiki/Foo_(bar)` links to the page it names
/// rather than to `bar` with a broken bracket around it.
#[must_use]
pub fn link_target(url: &str) -> String {
    let mut out = String::with_capacity(url.len());
    for ch in url.chars() {
        match ch {
            '(' => out.push_str("%28"),
            ')' => out.push_str("%29"),
            ' ' => out.push_str("%20"),
            '\t' => out.push_str("%09"),
            other => out.push(other),
        }
    }
    out
}

/// Untrusted text as page prose: whitespace collapsed, and the two
/// characters the pages crate reads as a `[[wiki-link]]` escaped. A paper
/// titled `Poisoned [[admin-runbook]]` is a title, not a link.
#[must_use]
pub fn prose(text: &str) -> String {
    paragraph(text).replace('[', "\\[").replace(']', "\\]")
}

/// `radar-<sanitised id>`, within [`MAX_SLUG_LEN`].
///
/// The source id is sanitised rather than rejected: an arXiv id is
/// `2401.01234v2` and a feed guid may be anything at all, and a page that
/// could not be named because its id had a `:` in it is a page nobody
/// finds. Two ids sanitising to the same slug collide, and the run's
/// tie-break (by id) makes that deterministic rather than arbitrary.
#[must_use]
pub fn slug_for(item: &Item) -> String {
    let prefix = "radar-";
    let tail: String = sanitise(&item.id)
        .chars()
        .take(MAX_SLUG_LEN - prefix.len())
        .collect();
    // An id that was entirely unsanitisable (all punctuation) still gets a
    // page, named by its source.
    let tail = if tail.is_empty() {
        item.source.as_str().to_owned()
    } else {
        tail
    };
    format!("{prefix}{}", tail.trim_matches('-'))
}

/// The slug rule, spelled out: lowercase ascii alphanumerics and `-`,
/// every other character a `-`, runs collapsed, ends trimmed.
#[must_use]
pub fn sanitise(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last_was_dash = false;
    for ch in text.chars() {
        let lowered = ch.to_ascii_lowercase();
        if lowered.is_ascii_alphanumeric() || lowered == '-' {
            out.push(lowered);
            last_was_dash = lowered == '-';
        } else if !last_was_dash {
            out.push('-');
            last_was_dash = true;
        }
    }
    out.trim_matches('-').to_owned()
}

/// A frontmatter value: one line, no brackets, no surrounding quotes or
/// heading marks — the same prose rule the body follows, since a frontmatter
/// line is a line of Markdown too.
#[must_use]
fn frontmatter_value(text: &str) -> String {
    prose(text).trim_matches(['"', '-', '#']).to_owned()
}

#[must_use]
fn paragraph(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// One sentence of prose: trailing punctuation dropped, the period put
/// back, and the brackets escaped — after, so an escaped `]` does not
/// leave the sentence's own full stop stranded.
#[must_use]
fn sentence(text: &str) -> String {
    let trimmed = text.trim().trim_end_matches(['.', ' ']);
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{}.", prose(trimmed))
    }
}

#[must_use]
fn labelled(text: &str, fallback: &'static str) -> String {
    let text = text.trim();
    if text.is_empty() {
        fallback.to_owned()
    } else {
        text.to_owned()
    }
}
