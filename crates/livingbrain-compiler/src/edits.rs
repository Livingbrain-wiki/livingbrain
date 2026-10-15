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

/// One page the model wants written: the whole new body, or exact
/// find/replace edits to the version it was shown ([`PageEdit::replacements`],
/// then `body` is empty until [`resolve`] fills it in).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageEdit {
    pub slug: String,
    pub base_version: Option<u32>,
    pub title: Option<String>,
    pub body: String,
    /// Edits to the shown body, applied in order; empty for a whole body.
    pub replacements: Vec<Replace>,
}

/// One exact edit to a page body: `find` must occur in it exactly once.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Replace {
    pub find: String,
    #[serde(alias = "replace_with", alias = "with")]
    pub replace: String,
}

/// The most find/replace edits one page edit may carry.
pub const MAX_REPLACEMENTS: usize = 64;

/// One page edit as the model wrote it. Common drift in the body's name
/// (`content`, `markdown`) is read as the body.
#[derive(Debug, Deserialize)]
struct RawEdit {
    slug: String,
    #[serde(default)]
    base_version: Option<u32>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default, alias = "content", alias = "markdown")]
    body: Option<String>,
    #[serde(default)]
    edits: Option<Vec<Replace>>,
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
    /// Why each refused page edit was refused, by path (`pages[2]: …`):
    /// fixed wording, never the model's own text. What a repair round
    /// sends back.
    pub problems: Vec<String>,
}

/// The raw shape, before the caps.
#[derive(Debug)]
struct Raw {
    summary: Option<String>,
    pages: Vec<Value>,
    facts: Vec<Value>,
}

/// Why an answer is not usable at all. Every message is the parser's own
/// wording plus a position or a field path — never the model's text, which
/// may quote a transcript — so it is safe for a status column and a log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BadOutput {
    /// No `{` in the reply at all.
    NoJson,
    /// The JSON ends before it closes: the reply was cut off.
    Unterminated { line: usize, column: usize },
    /// The JSON is malformed at this position.
    Syntax { line: usize, column: usize },
    /// A JSON object is there, but not of the answer's shape.
    Schema(String),
}

impl BadOutput {
    /// Whether the reply looks cut off (an unterminated JSON value).
    #[must_use]
    pub fn unterminated(&self) -> bool {
        matches!(self, Self::Unterminated { .. })
    }
}

impl std::fmt::Display for BadOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoJson => write!(f, "no JSON object in the reply"),
            Self::Unterminated { line, column } => {
                write!(f, "JSON cut off at line {line} column {column}")
            }
            Self::Syntax { line, column } => {
                write!(f, "invalid JSON at line {line} column {column}")
            }
            Self::Schema(path) => write!(f, "schema: {path}"),
        }
    }
}

/// The edit list's JSON Schema: what a forced tool call or a JSON-schema
/// response format is constrained to, and the shape [`parse`] reads.
#[must_use]
pub fn schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "format": {"type": "integer"},
            "summary": {"type": "string", "description": "One line: what this session did, for the append-only log."},
            "pages": {
                "type": "array",
                "maxItems": MAX_EDITS,
                "items": {
                    "type": "object",
                    "properties": {
                        "slug": {"type": "string"},
                        "base_version": {"type": ["integer", "null"]},
                        "title": {"type": "string"},
                        "body": {"type": "string", "description": "The FULL new Markdown body. For a new page, or a short page rewritten whole."},
                        "edits": {
                            "type": "array",
                            "maxItems": MAX_REPLACEMENTS,
                            "description": "Exact edits to the shown body of an existing page, instead of body.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "find": {"type": "string"},
                                    "replace": {"type": "string"}
                                },
                                "required": ["find", "replace"]
                            }
                        }
                    },
                    "required": ["slug"]
                }
            },
            "facts": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "subject": {"type": "string"},
                        "attribute": {"type": "string"},
                        "value": {"type": "string"},
                        "valid_from": {"type": "string"},
                        "source": {"type": "string"},
                        "quote": {"type": "string"},
                        "confidence": {"type": "number"}
                    },
                    "required": ["subject", "attribute", "value"]
                }
            }
        },
        "required": ["summary", "pages"]
    })
}

/// Parses the model's reply: the JSON object in it, then the closed
/// schema, then the caps.
///
/// Extraction is tolerant, validation is not. A ```` ```json ```` fence, a
/// sentence before or after, a stray `{` in that sentence, a trailing comma
/// before a `}` or `]`, or a thinking model's leftover reasoning in the
/// text (the transport already drops `thinking` blocks) do not lose the
/// answer: every `{` is tried in turn as the start of one complete JSON
/// value, and the first that is an object with a `pages` or `summary`
/// member is the answer. What is in it then goes through the closed schema
/// field by field.
///
/// # Errors
///
/// [`BadOutput`] when no JSON object of the expected shape is there, saying
/// why: no JSON, JSON cut off, malformed JSON, or the wrong shape.
pub fn parse(reply: &str) -> Result<Output, BadOutput> {
    let raw = extract(reply)?;
    let mut refused = 0u32;
    let mut problems = Vec::new();
    let mut pages = Vec::new();
    for (index, value) in raw.pages.into_iter().enumerate() {
        match page_edit(value) {
            Ok(_) if pages.len() >= MAX_EDITS => {
                problems.push(format!("pages[{index}]: over the {MAX_EDITS}-page limit"));
                refused += 1;
            }
            Ok(edit) => pages.push(edit),
            Err(problem) => {
                problems.push(format!("pages[{index}]: {problem}"));
                refused += 1;
            }
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
        problems,
    })
}

/// One page edit through the closed schema, or why not (fixed wording).
fn page_edit(value: Value) -> Result<PageEdit, String> {
    if !value.is_object() {
        return Err("not an object".to_owned());
    }
    let edit: RawEdit = serde_json::from_value(value).map_err(|error| {
        // serde's message can quote a value; only its kind is kept.
        let message = error.to_string();
        if message.starts_with("missing field `slug`") {
            "missing `slug`".to_owned()
        } else if message.starts_with("unknown field") {
            "an `edits` item has a field other than `find` and `replace`".to_owned()
        } else {
            "a field has the wrong type (`slug`, `title`, `body` are strings, `base_version` an integer or null, `edits` a list of {find, replace})".to_owned()
        }
    })?;
    let slug = edit.slug.trim().to_ascii_lowercase();
    if slug.is_empty() {
        return Err("empty `slug`".to_owned());
    }
    let replacements = edit.edits.unwrap_or_default();
    let body = match (edit.body, replacements.is_empty()) {
        (Some(_), false) => return Err("both `body` and `edits`; send one".to_owned()),
        (None, true) => return Err("missing `body` (or `edits`)".to_owned()),
        (Some(body), true) => body,
        (None, false) => String::new(),
    };
    if body.len() > MAX_BODY_BYTES {
        return Err(format!("`body` over {MAX_BODY_BYTES} bytes"));
    }
    if replacements.len() > MAX_REPLACEMENTS {
        return Err(format!("over {MAX_REPLACEMENTS} `edits`"));
    }
    if replacements.iter().any(|r| r.find.is_empty()) {
        return Err("an `edits` item has an empty `find`".to_owned());
    }
    if !replacements.is_empty() && edit.base_version.is_none() {
        return Err("`edits` on a page with no `base_version`; a new page needs `body`".to_owned());
    }
    Ok(PageEdit {
        slug,
        base_version: edit.base_version,
        title: edit.title.map(|t| one_line(&t, MAX_TITLE_CHARS)),
        body,
        replacements,
    })
}

/// The new body `edit` asks for over `shown` (the body of the version the
/// model was shown): `edit.body` when it is a whole body, else each
/// replacement applied in turn, each one's `find` occurring exactly once.
///
/// # Errors
///
/// Which replacement did not apply (fixed wording, by index).
pub fn resolve(edit: &PageEdit, shown: &str) -> Result<String, String> {
    if edit.replacements.is_empty() {
        return Ok(edit.body.clone());
    }
    let mut body = shown.to_owned();
    for (index, replacement) in edit.replacements.iter().enumerate() {
        match body.matches(replacement.find.as_str()).count() {
            1 => body = body.replacen(&replacement.find, &replacement.replace, 1),
            0 => return Err(format!("edits[{index}]: `find` is not in the page")),
            _ => return Err(format!("edits[{index}]: `find` occurs more than once")),
        }
    }
    if body.len() > MAX_BODY_BYTES {
        return Err(format!("the edited body is over {MAX_BODY_BYTES} bytes"));
    }
    Ok(body)
}

/// The first JSON object in `reply` that has the answer's shape; else why
/// there is none, judged from the first object-shaped candidate.
fn extract(reply: &str) -> Result<Raw, BadOutput> {
    let text = strip_fence(reply);
    let mut first_error: Option<BadOutput> = None;
    let repaired = without_trailing_commas(text);
    for candidate in [text, repaired.as_str()] {
        for (start, _) in candidate.match_indices('{') {
            let mut values =
                serde_json::Deserializer::from_str(&candidate[start..]).into_iter::<Value>();
            let object = match values.next() {
                Some(Ok(Value::Object(object))) => object,
                Some(Err(error)) => {
                    if first_error.is_none() {
                        first_error = Some(json_error(&candidate[..start], &error));
                    }
                    continue;
                }
                _ => continue,
            };
            if !object.contains_key("pages") && !object.contains_key("summary") {
                continue;
            }
            match shape(object) {
                Ok(raw) => return Ok(raw),
                Err(problem) => {
                    first_error = Some(BadOutput::Schema(problem));
                }
            }
        }
        if repaired.len() == text.len() {
            break;
        }
    }
    Err(first_error.unwrap_or(if text.contains('{') {
        BadOutput::Schema("no object with `summary` or `pages`".to_owned())
    } else {
        BadOutput::NoJson
    }))
}

/// The answer's top-level shape, or which member is wrong.
fn shape(object: serde_json::Map<String, Value>) -> Result<Raw, String> {
    let mut summary = None;
    let mut pages = Vec::new();
    let mut facts = Vec::new();
    for (key, value) in object {
        match (key.as_str(), value) {
            ("summary", Value::String(text)) => summary = Some(text),
            ("summary", Value::Null) | ("pages" | "facts", Value::Null) => {}
            ("summary", _) => return Err("`summary` is not a string".to_owned()),
            ("pages", Value::Array(list)) => pages = list,
            ("pages", _) => return Err("`pages` is not a list".to_owned()),
            ("facts", Value::Array(list)) => facts = list,
            ("facts", _) => return Err("`facts` is not a list".to_owned()),
            _ => {}
        }
    }
    Ok(Raw {
        summary,
        pages,
        facts,
    })
}

/// A JSON error as a [`BadOutput`], its position counted in the whole
/// candidate text (`before` is what precedes the value).
fn json_error(before: &str, error: &serde_json::Error) -> BadOutput {
    let offset_lines = before.matches('\n').count();
    let line = error.line() + offset_lines;
    let column = if error.line() == 1 {
        error.column()
            + before
                .rsplit('\n')
                .next()
                .map_or(0, |tail| tail.chars().count())
    } else {
        error.column()
    };
    if error.is_eof() {
        BadOutput::Unterminated { line, column }
    } else {
        BadOutput::Syntax { line, column }
    }
}

/// `text` with every comma that only precedes a `}` or `]` (whitespace
/// between) removed, outside strings: the one JSON slip models make most.
fn without_trailing_commas(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut escaped = false;
    let chars: Vec<char> = text.chars().collect();
    for (index, &c) in chars.iter().enumerate() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            ',' if matches!(
                chars[index + 1..].iter().find(|next| !next.is_whitespace()),
                Some('}' | ']')
            ) => {}
            _ => out.push(c),
        }
    }
    out
}

/// The body of a Markdown code fence around the whole reply, when there is
/// one; the reply otherwise.
fn strip_fence(reply: &str) -> &str {
    let trimmed = reply.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    // The info string (`json`, `JSON`, nothing) runs to the first newline.
    let body = rest.split_once('\n').map_or("", |(_, body)| body);
    body.trim_end().strip_suffix("```").unwrap_or(body).trim()
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
    fn prose_braces_and_fences_do_not_lose_the_answer() {
        let reply = "Here is the {edit} you asked for:\n```JSON\n{\"summary\":\"s\",\"pages\":[]}\n```\nDone {ok}.";
        assert_eq!(parse(reply).expect("parses").summary, "s");
        let thinking_left_over = "Let me check {the pages}… {\"draft\": 1}\n{\"summary\":\"t\",\"pages\":[],\"facts\":[]}";
        assert_eq!(parse(thinking_left_over).expect("parses").summary, "t");
        assert_eq!(strip_fence("```\n{}\n```"), "{}");
    }

    #[test]
    fn the_schema_stays_strict() {
        // An object of the wrong shape is not an answer, even if it parses.
        assert_eq!(
            parse("{\"pages\": \"all of them\"}"),
            Err(BadOutput::Schema("`pages` is not a list".to_owned()))
        );
        assert!(matches!(parse("{\"other\": 1}"), Err(BadOutput::Schema(_))));
    }

    #[test]
    fn garbage_is_bad_output() {
        assert_eq!(parse("I could not do that."), Err(BadOutput::NoJson));
        assert!(matches!(
            parse("{not json}"),
            Err(BadOutput::Syntax { line: 1, .. })
        ));
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
        assert_eq!(
            output.problems,
            vec!["pages[0]: missing `body` (or `edits`)"]
        );
    }

    #[test]
    fn prose_around_the_json_and_trailing_commas_are_tolerated() {
        let reply = "Sure! Here is the edit list:\n\n{\"summary\": \"s\", \"pages\": [{\"slug\": \"a\", \"body\": \"x, }\",},],}\n\nLet me know.";
        let output = parse(reply).expect("parses");
        assert_eq!(output.summary, "s");
        // The comma inside the string is the model's text, kept as it was.
        assert_eq!(output.pages[0].body, "x, }");
    }

    #[test]
    fn a_cut_off_answer_says_so_with_its_position() {
        let reply =
            "{\n  \"summary\": \"s\",\n  \"pages\": [{\"slug\": \"a\", \"body\": \"# A\\n- one";
        let error = parse(reply).expect_err("cut off");
        assert!(error.unterminated());
        assert_eq!(error.to_string(), "JSON cut off at line 3 column 45");
        // The message never carries the model's text.
        assert!(!error.to_string().contains("one"));
    }

    #[test]
    fn find_replace_edits_resolve_against_the_shown_body() {
        let reply = r###"{"summary":"s","pages":[{"slug":"p","base_version":3,"edits":[
            {"find":"## Decisions\n","replace":"## Decisions\n- new\n"},
            {"find":"- old","replace":"- ~~old~~ superseded"}]}]}"###;
        let output = parse(reply).expect("parses");
        let edit = &output.pages[0];
        assert_eq!(edit.replacements.len(), 2);
        let shown = "# P\n\n## Decisions\n- old\n";
        assert_eq!(
            resolve(edit, shown).expect("applies"),
            "# P\n\n## Decisions\n- new\n- ~~old~~ superseded\n"
        );
        assert_eq!(
            resolve(edit, "# P\n"),
            Err("edits[0]: `find` is not in the page".to_owned())
        );
        assert_eq!(
            resolve(edit, "## Decisions\n## Decisions\n"),
            Err("edits[0]: `find` occurs more than once".to_owned())
        );
    }

    #[test]
    fn edits_and_body_are_exclusive_and_edits_need_a_base() {
        let both = r#"{"pages":[{"slug":"p","base_version":1,"body":"b","edits":[{"find":"a","replace":"b"}]}]}"#;
        assert_eq!(
            parse(both).expect("parses").problems,
            vec!["pages[0]: both `body` and `edits`; send one"]
        );
        let new =
            r#"{"pages":[{"slug":"p","base_version":null,"edits":[{"find":"a","replace":"b"}]}]}"#;
        assert_eq!(parse(new).expect("parses").refused, 1);
        // Common drift in the body's name still reads as the body.
        let drift = r#"{"pages":[{"slug":"p","content":"b"}]}"#;
        assert_eq!(parse(drift).expect("parses").pages[0].body, "b");
    }

    #[test]
    fn the_schema_names_the_fields_parse_reads() {
        let schema = schema();
        let page = &schema["properties"]["pages"]["items"]["properties"];
        for field in ["slug", "base_version", "title", "body", "edits"] {
            assert!(page.get(field).is_some(), "{field}");
        }
        assert_eq!(schema["required"], serde_json::json!(["summary", "pages"]));
    }

    #[test]
    fn a_summary_is_one_line() {
        assert_eq!(one_line("a\nb\tc  d", 100), "a b c d");
    }
}
