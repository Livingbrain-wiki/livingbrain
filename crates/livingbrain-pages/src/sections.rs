//! The parts of a page a brief quotes, under a token cap (issue #281).
//!
//! `brain_context_for` used to inline five whole pages, which is ~50k tokens
//! when the pages are session transcripts. A brief now quotes the **sections**
//! of each page that mention the task's words — a section is the text under
//! one Markdown heading — and packs them, in order, into one budget. A page
//! whose words sit only in its title, so that no section mentions them,
//! contributes its opening section instead, so a title match is not silently
//! dropped.
//!
//! Tokens are estimated, not counted: [`estimate_tokens`] is four bytes to a
//! token, the usual rule of thumb. It is a bound on what a brief sends, not a
//! tokenizer, and it errs on the side of more tokens than a model would count.

use crate::query::QueryTerm;

/// The longest a brief's quoted sections may run, in estimated tokens. About
/// 2,000 tokens of pages (roughly 8 KB), however many pages match.
pub const BRIEF_TOKEN_CAP: usize = 2_000;

/// Bytes per token in [`estimate_tokens`].
const BYTES_PER_TOKEN: usize = 4;

/// The fewest tokens worth quoting from a section that does not fit whole:
/// below this a cut-off section is a stub, and the brief stops instead.
const MIN_PARTIAL_TOKENS: usize = 32;

/// One section of a page: the heading that opens it (empty for the text
/// before a page's first heading) and the text under it, heading included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// The heading line, without its `#`s. Empty for an untitled opening.
    pub heading: String,
    /// The section's Markdown, heading line included.
    pub text: String,
    /// How many distinct query words the section holds.
    pub matched: usize,
}

/// An estimate of how many tokens `text` costs a model.
#[must_use]
pub fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(BYTES_PER_TOKEN)
}

/// Splits a body at its Markdown headings (`#` to `######`). The text before
/// the first heading is a section of its own, with an empty heading, when
/// there is any.
#[must_use]
pub fn split_sections(body: &str) -> Vec<Section> {
    let mut sections: Vec<Section> = Vec::new();
    let mut current = Section {
        heading: String::new(),
        text: String::new(),
        matched: 0,
    };
    for line in body.lines() {
        if let Some(heading) = heading_text(line) {
            if !current.text.trim().is_empty() {
                sections.push(current);
            }
            current = Section {
                heading: heading.to_owned(),
                text: String::new(),
                matched: 0,
            };
        }
        current.text.push_str(line);
        current.text.push('\n');
    }
    if !current.text.trim().is_empty() {
        sections.push(current);
    }
    sections
}

/// The heading a line opens, without its `#`s and surrounding space, when the
/// line is a Markdown ATX heading.
fn heading_text(line: &str) -> Option<&str> {
    let hashes = line.bytes().take_while(|byte| *byte == b'#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = &line[hashes..];
    if !rest.starts_with(' ') && !rest.is_empty() {
        return None;
    }
    Some(rest.trim())
}

/// The distinct query words a text holds, counted over its lowercased
/// alphanumeric words and their plural/singular forms.
fn matched_terms(text: &str, terms: &[QueryTerm]) -> usize {
    let words: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect();
    terms
        .iter()
        .filter(|term| words.iter().any(|word| term.matches(word)))
        .count()
}

/// The sections of `body` that mention any of `terms`, best first: more
/// distinct query words first, then the earlier section. A section with no
/// query word is not a match. When no section mentions one — the words sit in
/// the page's title, say — the opening section comes back instead, so the
/// page still speaks for itself.
#[must_use]
pub fn matching_sections(body: &str, terms: &[QueryTerm]) -> Vec<Section> {
    let mut sections = split_sections(body);
    for section in &mut sections {
        section.matched = matched_terms(&section.text, terms);
    }
    let mut matching: Vec<(usize, Section)> = sections
        .into_iter()
        .enumerate()
        .filter(|(_, section)| section.matched > 0)
        .collect();
    if matching.is_empty() {
        return split_sections(body).into_iter().take(1).collect();
    }
    matching.sort_by(|(i, a), (j, b)| b.matched.cmp(&a.matched).then_with(|| i.cmp(j)));
    matching.into_iter().map(|(_, section)| section).collect()
}

/// Packs `sections`, in the order given, into at most `cap` estimated tokens.
///
/// A section that fits whole is quoted whole. One that does not is cut at a
/// line boundary to the budget that is left, provided at least
/// [`MIN_PARTIAL_TOKENS`] remain for it; below that, packing stops. Nothing
/// is ever quoted past `cap`, however many sections there are.
#[must_use]
pub fn pack_sections(sections: &[Section], cap: usize) -> String {
    let mut out = String::new();
    let mut used = 0_usize;
    for section in sections {
        let left = cap.saturating_sub(used);
        let cost = estimate_tokens(&section.text);
        if cost <= left {
            out.push_str(section.text.trim_end());
            out.push_str("\n\n");
            used += cost;
            continue;
        }
        if left < MIN_PARTIAL_TOKENS {
            break;
        }
        let budget = left * BYTES_PER_TOKEN;
        let mut kept = String::new();
        for line in section.text.lines() {
            if kept.len() + line.len() + 1 > budget {
                break;
            }
            kept.push_str(line);
            kept.push('\n');
        }
        if !kept.trim().is_empty() {
            out.push_str(kept.trim_end());
            out.push_str("\n\n");
        }
        break;
    }
    out.trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query_terms;

    fn body() -> String {
        [
            "# Deploys",
            "Deploys run from the release pipeline and promote by tag.",
            "",
            "## Rollback",
            "Rollback re-runs the previous release. Keep the old image.",
            "",
            "## Billing",
            "Invoices are monthly and unrelated to the release.",
        ]
        .join("\n")
    }

    #[test]
    fn a_body_splits_at_its_headings_and_keeps_the_opening() {
        let sections = split_sections(&body());
        let headings: Vec<&str> = sections.iter().map(|s| s.heading.as_str()).collect();
        assert_eq!(headings, ["Deploys", "Rollback", "Billing"]);
        let preamble = split_sections("just text, no heading\n# Later\nmore");
        assert_eq!(preamble[0].heading, "");
        assert_eq!(preamble[1].heading, "Later");
    }

    #[test]
    fn only_the_sections_that_mention_the_query_are_quoted() {
        let terms = query_terms("rollback of the release");
        let matching = matching_sections(&body(), &terms);
        let headings: Vec<&str> = matching.iter().map(|s| s.heading.as_str()).collect();
        // Rollback holds two of the words, Deploys one, Billing one; it is
        // ranked by how many it holds, and the unrelated section never quotes.
        assert_eq!(headings, ["Rollback", "Deploys", "Billing"]);
        let only_rollback = matching_sections(&body(), &query_terms("rollback"));
        assert_eq!(only_rollback.len(), 1);
        assert_eq!(only_rollback[0].heading, "Rollback");
    }

    #[test]
    fn a_title_only_match_quotes_the_opening_section() {
        let page = "# Retry policy\nBackoff is exponential.\n## Limits\nCap at five.";
        let found = matching_sections(page, &query_terms("retry"));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].heading, "Retry policy");
    }

    #[test]
    fn the_token_cap_is_never_exceeded() {
        let long: String = (0..400)
            .map(|i| format!("Line {i} says something about the release process.\n"))
            .collect();
        let page = format!("# Release\n{long}");
        let sections = matching_sections(&page, &query_terms("release"));
        for cap in [0, 10, 100, 500, BRIEF_TOKEN_CAP] {
            let packed = pack_sections(&sections, cap);
            assert!(
                estimate_tokens(&packed) <= cap,
                "cap {cap}: {} tokens",
                estimate_tokens(&packed)
            );
        }
        // A whole page of sections, many of them, stays under the default cap
        // where the old brief quoted five full pages.
        let many: Vec<Section> = (0..50).flat_map(|_| sections.clone()).collect();
        assert!(estimate_tokens(&pack_sections(&many, BRIEF_TOKEN_CAP)) <= BRIEF_TOKEN_CAP);
    }

    #[test]
    fn a_section_that_does_not_fit_is_cut_on_a_line_or_dropped() {
        let lines: String = (0..100).map(|_| "word ".repeat(20) + "\n").collect();
        let text = "# Big\n".to_owned() + &lines;
        let section = Section {
            heading: "Big".into(),
            text,
            matched: 1,
        };
        let packed = pack_sections(&[section], 200);
        assert!(estimate_tokens(&packed) <= 200);
        assert!(packed.starts_with("# Big"));
        // Too little room for a stub: nothing from that section.
        let tiny = pack_sections(
            &[Section {
                heading: "Big".into(),
                text: format!("# Big\n{}\n", "lots of words here ".repeat(10)),
                matched: 1,
            }],
            MIN_PARTIAL_TOKENS - 1,
        );
        assert!(tiny.is_empty());
    }
}
