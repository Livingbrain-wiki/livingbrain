//! Reading an `agent_log` source: the CLI's normalised session record, read
//! **version-tolerantly**.
//!
//! The record is versioned (`schema_version`, `collector_version`) and grows:
//! version 1 carries turns with text and tool *names* only; a later collector
//! may carry full messages and tool calls with their inputs and outputs. The
//! compiler must read every version it will ever be handed, so nothing here
//! deserialises into a fixed struct — a field this build does not know is
//! rendered generically rather than refused, and a field that is missing is
//! simply absent. What the compiler needs is the session's identity (agent,
//! id, project, dates) and a transcript it can chunk.

use serde_json::Value;

/// The most characters one tool call's detail (its input or output, when a
/// newer record carries them) contributes to the transcript. The rest is
/// elided with a marker — the model needs to know a command ran, not to
/// reread a whole file.
const MAX_TOOL_DETAIL_CHARS: usize = 600;

/// One session, as much as the compiler reads of it.
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub schema_version: u64,
    pub collector_version: u64,
    /// The record's `agent` as written (`claude_code`, `codex`, …).
    pub agent: String,
    pub session_id: String,
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub models: Vec<String>,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub turns: Vec<Turn>,
    /// A one-line rendering of the totals object, whatever keys it has.
    pub totals: Option<String>,
    pub truncated: bool,
}

/// One turn, rendered to text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub role: String,
    pub at: Option<String>,
    pub text: String,
    /// One line per tool call: its name, and its detail when the record
    /// carries one.
    pub tools: Vec<String>,
    /// The same tool calls, field by field, for the compaction pass
    /// ([`crate::compact`]): which ones failed, what ran, what came back.
    pub calls: Vec<ToolCall>,
}

/// One tool call as the record carries it. A version-1 record has only the
/// name; a `full` record has the rest. A field this build does not know is
/// kept, on one line, in `extra`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ToolCall {
    pub name: String,
    /// How the input reads: `shell`, `diff`, `json` or `text`.
    pub format: Option<String>,
    /// The file the call read or changed.
    pub path: Option<String>,
    /// The command, patch or arguments (clipped by the collector).
    pub input: Option<String>,
    /// The result (clipped to head and tail by the collector).
    pub output: Option<String>,
    /// `ok`, `error` or `interrupted`, when the record says.
    pub status: Option<String>,
    pub exit_code: Option<i64>,
    /// Every other field, as one JSON line.
    pub extra: Option<String>,
    /// The subagent this call launched, with its own turns (record schema
    /// 3: an `Agent`/`Task` call, a Codex `spawn_agent`).
    pub subagent: Option<Box<Subagent>>,
}

/// A subagent's session, nested in the call that launched it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Subagent {
    /// The kind of agent (`Explore`, `general-purpose`, a Codex role).
    pub name: String,
    /// What it was asked to do.
    pub description: Option<String>,
    /// `completed`, `failed`, `stopped` or `running`, when the record says.
    pub status: Option<String>,
    pub turns: Vec<Turn>,
}

impl ToolCall {
    /// This call, then every call of its subagent (and theirs), in order.
    #[must_use]
    pub fn with_nested(&self) -> Vec<&ToolCall> {
        let mut out = vec![self];
        if let Some(agent) = &self.subagent {
            for turn in &agent.turns {
                for call in &turn.calls {
                    out.extend(call.with_nested());
                }
            }
        }
        out
    }

    /// Whether the call failed: an `error` status or a non-zero exit code.
    #[must_use]
    pub fn failed(&self) -> bool {
        self.status.as_deref() == Some("error") || self.exit_code.is_some_and(|code| code != 0)
    }
}

/// Why a body is not a session record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordError {
    /// Not JSON at all, or not an object.
    NotJson,
    /// JSON, but no `agent` or no `session_id`: not a session record.
    NotASession,
}

impl std::fmt::Display for RecordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotJson => write!(f, "the source is not a JSON session record"),
            Self::NotASession => write!(f, "the source names no agent or session id"),
        }
    }
}

/// Parses a stored `agent_log` body.
///
/// # Errors
///
/// [`RecordError`] when the body is not a session record of any version.
pub fn parse(body: &str) -> Result<Session, RecordError> {
    let value: Value = serde_json::from_str(body).map_err(|_| RecordError::NotJson)?;
    let object = value.as_object().ok_or(RecordError::NotJson)?;
    let text = |key: &str| {
        object
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let agent = text("agent").ok_or(RecordError::NotASession)?;
    let session_id = text("session_id")
        .or_else(|| text("id"))
        .ok_or(RecordError::NotASession)?;
    let turns = object
        .get("turns")
        .or_else(|| object.get("messages"))
        .and_then(Value::as_array)
        .map(|turns| turns.iter().filter_map(turn).collect())
        .unwrap_or_default();
    Ok(Session {
        schema_version: object
            .get("schema_version")
            .and_then(Value::as_u64)
            .unwrap_or(1),
        collector_version: object
            .get("collector_version")
            .and_then(Value::as_u64)
            .unwrap_or(1),
        agent,
        session_id,
        repo: text("repo"),
        branch: text("branch"),
        models: object
            .get("models")
            .and_then(Value::as_array)
            .map(|models| {
                models
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        started_at: text("started_at"),
        ended_at: text("ended_at"),
        turns,
        totals: object.get("totals").and_then(one_line),
        truncated: object
            .get("truncated")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

/// One turn of any version: `role`, `at`/`timestamp`, the text from `text`
/// or `content` (a string or an array of text blocks), and tool calls from
/// `tool_calls`/`tools`.
fn turn(value: &Value) -> Option<Turn> {
    let object = value.as_object()?;
    let role = object
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    let at = object
        .get("at")
        .or_else(|| object.get("timestamp"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let text = object
        .get("text")
        .or_else(|| object.get("content"))
        .map(content_text)
        .unwrap_or_default();
    let raw = object
        .get("tool_calls")
        .or_else(|| object.get("tools"))
        .and_then(Value::as_array);
    let tools = raw
        .map(|calls| calls.iter().map(tool_line).collect())
        .unwrap_or_default();
    let calls = raw
        .map(|calls| calls.iter().map(tool_call).collect())
        .unwrap_or_default();
    Some(Turn {
        role,
        at,
        text,
        tools,
        calls,
    })
}

/// A message's text, whatever shape it took: a string, or an array of
/// blocks whose `text` fields are joined.
fn content_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                Value::String(text) => Some(text.clone()),
                Value::Object(map) => map.get("text").and_then(Value::as_str).map(str::to_owned),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// One tool call as a line: the name, then whatever else the record
/// carries for it (an input, an output, a status), compacted and capped.
fn tool_line(value: &Value) -> String {
    match value {
        Value::String(name) => name.clone(),
        Value::Object(map) => {
            let name = map
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_owned();
            let mut rest = map.clone();
            rest.remove("name");
            if rest.is_empty() {
                return name;
            }
            let detail = Value::Object(rest).to_string();
            format!("{name} {}", cap(&detail, MAX_TOOL_DETAIL_CHARS))
        }
        other => cap(&other.to_string(), MAX_TOOL_DETAIL_CHARS),
    }
}

/// One tool call, field by field: the fields the collector writes are read
/// by name (a string or a number where a string is expected is rendered),
/// and whatever else the call carries is kept in `extra`.
fn tool_call(value: &Value) -> ToolCall {
    let Value::Object(map) = value else {
        return ToolCall {
            name: value
                .as_str()
                .map_or_else(|| cap(&value.to_string(), 80), str::to_owned),
            ..ToolCall::default()
        };
    };
    let field = |key: &str| {
        map.get(key).and_then(|value| match value {
            Value::Null => None,
            Value::String(text) => Some(text.clone()),
            other => Some(other.to_string()),
        })
    };
    let mut rest = map.clone();
    for key in [
        "name",
        "format",
        "path",
        "input",
        "output",
        "status",
        "exit_code",
        "subagent",
    ] {
        rest.remove(key);
    }
    let subagent = map.get("subagent").and_then(Value::as_object).map(|agent| {
        let text = |key: &str| {
            agent
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        };
        Box::new(Subagent {
            name: text("name").unwrap_or_else(|| "subagent".to_owned()),
            description: text("description"),
            status: text("status"),
            turns: agent
                .get("turns")
                .and_then(Value::as_array)
                .map(|turns| turns.iter().filter_map(turn).collect())
                .unwrap_or_default(),
        })
    });
    ToolCall {
        name: field("name").unwrap_or_else(|| "tool".to_owned()),
        format: field("format"),
        path: field("path"),
        input: field("input"),
        output: field("output"),
        status: field("status"),
        exit_code: map.get("exit_code").and_then(Value::as_i64),
        extra: (!rest.is_empty()).then(|| Value::Object(rest).to_string()),
        subagent,
    }
}

/// A JSON value on one line, or `None` for null.
fn one_line(value: &Value) -> Option<String> {
    (!value.is_null()).then(|| value.to_string())
}

/// One turn as the transcript shows it: a heading, the text, one line per
/// tool call.
fn render_turn(index: usize, turn: &Turn) -> String {
    let mut rendered = format!(
        "### turn {} · {}{}\n",
        index + 1,
        turn.role,
        turn.at
            .as_deref()
            .map(|at| format!(" · {at}"))
            .unwrap_or_default()
    );
    if !turn.text.trim().is_empty() {
        rendered.push_str(turn.text.trim());
        rendered.push('\n');
    }
    for tool in &turn.tools {
        rendered.push_str("- tool: ");
        rendered.push_str(tool);
        rendered.push('\n');
    }
    rendered.push('\n');
    rendered
}

/// At most `max` characters of `text`, with a marker when it was cut.
#[must_use]
pub fn cap(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{kept}… [cut]")
}

impl Session {
    /// The session's day, `YYYY-MM-DD`, from its first timestamp.
    #[must_use]
    pub fn day(&self) -> Option<String> {
        self.started_at
            .as_deref()
            .or(self.ended_at.as_deref())
            .filter(|at| at.len() >= 10)
            .map(|at| at[..10].to_owned())
    }

    /// The transcript rendered and cut into chunks of at most `budget`
    /// characters each, on turn boundaries, at most `max_chunks` of them. A
    /// single turn longer than a chunk is cut to fit. The bool is whether
    /// anything was left out.
    #[must_use]
    pub fn chunks(&self, budget: usize, max_chunks: usize) -> (Vec<String>, bool) {
        let budget = budget.max(200);
        let mut chunks: Vec<String> = Vec::new();
        let mut current = String::new();
        // Counted as it grows: re-counting the chunk per turn would be
        // quadratic in a long transcript.
        let mut current_chars = 0usize;
        let mut omitted = false;
        for (index, turn) in self.turns.iter().enumerate() {
            let mut rendered = render_turn(index, turn);
            let mut chars = rendered.chars().count();
            if chars > budget {
                rendered = cap(&rendered, budget.saturating_sub(16));
                rendered.push('\n');
                chars = rendered.chars().count();
            }
            if current_chars + chars > budget && !current.is_empty() {
                if chunks.len() + 1 >= max_chunks {
                    omitted = true;
                    break;
                }
                chunks.push(std::mem::take(&mut current));
                current_chars = 0;
            }
            current.push_str(&rendered);
            current_chars += chars;
        }
        if !current.is_empty() {
            chunks.push(current);
        }
        (chunks, omitted || self.truncated)
    }

    /// The characters the whole transcript renders to, uncut — what
    /// [`Session::chunks`] would send with no budget.
    #[must_use]
    pub fn rendered_chars(&self) -> usize {
        self.turns
            .iter()
            .enumerate()
            .map(|(index, turn)| render_turn(index, turn).chars().count())
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const V1: &str = r#"{"schema_version":1,"agent":"claude_code","collector_version":1,
        "session_id":"s-1","repo":"~/code/demo","branch":"main","models":["m"],
        "started_at":"2026-10-15T01:00:00Z","ended_at":"2026-10-15T02:00:00Z",
        "turns":[{"role":"user","at":"2026-10-15T01:00:00Z","text":"switch to D1","tool_calls":[]},
                 {"role":"assistant","at":null,"text":"done","tool_calls":[{"name":"Bash"}]}],
        "totals":{"input_tokens":10},"outcome":null,"truncated":false}"#;

    #[test]
    fn version_one_reads() {
        let session = parse(V1).expect("a v1 record");
        assert_eq!(session.agent, "claude_code");
        assert_eq!(session.session_id, "s-1");
        assert_eq!(session.turns.len(), 2);
        assert_eq!(session.turns[1].tools, ["Bash"]);
        assert_eq!(session.day().as_deref(), Some("2026-10-15"));
    }

    #[test]
    fn a_later_version_with_full_tool_calls_and_unknown_fields_reads() {
        let v2 = r#"{"schema_version":2,"agent":"codex","collector_version":3,
            "session_id":"s-2","detail":"full","new_field":{"x":1},
            "turns":[{"role":"assistant","timestamp":"2026-10-15T03:00:00Z",
              "content":[{"type":"text","text":"ran it"}],
              "tool_calls":[{"name":"shell","input":{"cmd":"cargo test"},"output":"ok"}]}]}"#;
        let session = parse(v2).expect("a later record");
        assert_eq!(session.schema_version, 2);
        assert_eq!(session.turns[0].text, "ran it");
        assert!(session.turns[0].tools[0].starts_with("shell "));
        assert!(session.turns[0].tools[0].contains("cargo test"));
    }

    #[test]
    fn not_a_session_is_refused() {
        assert_eq!(parse("# a note"), Err(RecordError::NotJson));
        assert_eq!(parse("{\"x\":1}"), Err(RecordError::NotASession));
    }

    #[test]
    fn a_long_transcript_chunks_on_turn_boundaries() {
        let mut session = parse(V1).expect("a record");
        session.turns = (0..50)
            .map(|i| Turn {
                role: "user".to_owned(),
                at: None,
                text: format!("message {i} {}", "x".repeat(100)),
                tools: Vec::new(),
                calls: Vec::new(),
            })
            .collect();
        let (chunks, omitted) = session.chunks(1_000, 3);
        assert_eq!(chunks.len(), 3);
        assert!(omitted);
        assert!(chunks.iter().all(|chunk| chunk.chars().count() <= 1_000));
        let (all, omitted) = session.chunks(100_000, 3);
        assert_eq!(all.len(), 1);
        assert!(!omitted);
    }
}
