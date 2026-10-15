//! The model's answer, parsed and bounded: a closed schema of page edits,
//! a log summary and temporal facts. Nothing else a model says is acted on.

use serde::Deserialize;
use serde_json::Value;

use crate::facts::{self, Fact, MAX_FACTS};

/// The most pages one call may write.
pub const MAX_EDITS: usize = 16;
/// The longest page body one edit may carry, in bytes.
pub const MAX_BODY_BYTES: usize = 48 * 1024;
/// The longest summary line, in characters.
const MAX_SUMMARY_CHARS: usize = 300;
/// The longest title, in characters.
const MAX_TITLE_CHARS: usize = 120;

/// One page the model wants written.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PageEdit {
    pub slug: String,
    #[serde(default)]
    pub base_version: Option<u32>,
    #[serde(default)]
    pub title: Option<String>,
    pub body: String,
}

/// A parsed answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Output {
    pub summary: String,
    pub pages: Vec<PageEdit>,
    /// The facts that passed the v1 schema.
    pub facts: Vec<Fact>,
    /// Edits and facts refused at parse time: over a cap, or failing the
    /// schema. Counted, never acted on.
    pub refused: u32,
}

/// The raw shape, before the caps.
#[derive(Debug, Deserialize)]
struct Raw {
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    pages: Vec<Value>,
    #[serde(default)]
    facts: Vec<Value>,
}

/// Why an answer is not usable at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadOutput;

/// Parses the model's reply: the first JSON object in it (a model that
/// wraps the object in a code fence or a sentence still parses), then the
/// closed schema, then the caps.
///
/// # Errors
///
/// [`BadOutput`] when no JSON object of the expected shape is there.
pub fn parse(reply: &str) -> Result<Output, BadOutput> {
    let start = reply.find('{').ok_or(BadOutput)?;
    let end = reply.rfind('}').ok_or(BadOutput)?;
    if end < start {
        return Err(BadOutput);
    }
    let raw: Raw = serde_json::from_str(&reply[start..=end]).map_err(|_| BadOutput)?;
    let mut refused = 0u32;
    let mut pages = Vec::new();
    for value in raw.pages {
        match serde_json::from_value::<PageEdit>(value) {
            Ok(edit)
                if pages.len() < MAX_EDITS
                    && edit.body.len() <= MAX_BODY_BYTES
                    && !edit.slug.trim().is_empty() =>
            {
                pages.push(PageEdit {
                    slug: edit.slug.trim().to_ascii_lowercase(),
                    title: edit.title.map(|t| one_line(&t, MAX_TITLE_CHARS)),
                    ..edit
                });
            }
            _ => refused += 1,
        }
    }
    let mut kept = Vec::new();
    for value in raw.facts {
        match serde_json::from_value::<Fact>(value) {
            Ok(fact) if kept.len() < MAX_FACTS && facts::valid(&fact) => kept.push(fact),
            _ => refused += 1,
        }
    }
    Ok(Output {
        summary: one_line(raw.summary.as_deref().unwrap_or(""), MAX_SUMMARY_CHARS),
        pages,
        facts: kept,
        refused,
    })
}

/// Text on one line, trimmed and capped: what goes into a frontmatter
/// value or a log line, where a newline would end the field.
#[must_use]
pub fn one_line(text: &str, max: usize) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    crate::record::cap(&flat, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fenced_answer_parses() {
        let reply = "```json\n{\"format\":1,\"summary\":\"did it\",\"pages\":[{\"slug\":\"Coding-Agents--Index\",\"base_version\":null,\"title\":\"Index\",\"body\":\"# x\"}],\"facts\":[]}\n```";
        let output = parse(reply).expect("parses");
        assert_eq!(output.summary, "did it");
        assert_eq!(output.pages[0].slug, "coding-agents--index");
        assert_eq!(output.refused, 0);
    }

    #[test]
    fn garbage_is_bad_output() {
        assert_eq!(parse("I could not do that."), Err(BadOutput));
        assert_eq!(parse("{not json}"), Err(BadOutput));
    }

    #[test]
    fn malformed_edits_and_facts_are_refused_not_fatal() {
        let reply = r#"{"summary":"s","pages":[{"slug":"a"},{"slug":"coding-agents--index","body":"ok"}],
            "facts":[{"subject":"agent/x","attribute":"a","value":"v"},
                     {"subject":"project/demo","attribute":"db","value":"D1","status":"current"},
                     {"subject":"project/demo","attribute":"db","value":"D1"}]}"#;
        let output = parse(reply).expect("parses");
        assert_eq!(output.pages.len(), 1);
        assert_eq!(output.facts.len(), 1);
        assert_eq!(output.refused, 3);
    }

    #[test]
    fn a_summary_is_one_line() {
        assert_eq!(one_line("a\nb\tc  d", 100), "a b c d");
    }
}
