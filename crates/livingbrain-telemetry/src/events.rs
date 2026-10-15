//! The structured event log: one JSON line per thing that happened.
//!
//! A brain turn, a nightly-evolve change, an MCP call, a colony hand-off —
//! each is one line against a versioned schema, and each line is built from
//! the types in this file. [`Event::to_json_line`] is the only way to get one
//! out, and [`Event::schema`] describes the same fields to a reader that a
//! schema file or a dashboard would want.
//!
//! # The failure this exists to prevent
//!
//! **No field of [`Event`] can hold message text or a page body.** Content is
//! referenced, never copied: a [`Subject`] is a kind and an opaque id, and
//! the reader of the log is expected to fetch the page by id if it wants the
//! page. This is not a style preference. An event log is the artefact most
//! likely to be shipped somewhere — to an aggregator, a support engineer, a
//! bug report — and the moment a page body is allowed into a line, that text
//! has left the machine in a form nobody chose to send it. `Event` has no
//! `Deserialize`, so a line that arrived from elsewhere cannot be read back
//! into a typed event and re-serialised into something the compiler thinks
//! was vetted.
//!
//! # What the id fields are for
//!
//! [`Event::trace_id`], [`Subject::id`] and [`Event::colonizer_colony`] are
//! validated as opaque ids — non-empty, at most 128 characters, drawn from
//! `[A-Za-z0-9._:-]` — because a `String` field with no rule on it is a
//! back door around every closed enum in the crate. See `tests/anonymity.rs`
//! for what the charset does and does not catch; the honest summary is that
//! it stops free text and nothing more.
//!
//! # Non-goals
//!
//! Not a queryable store and not a transport: there is no sink, no buffering
//! and no rotation here, and [`Event`] is write-only by construction.

use serde::Serialize;
use serde_json::{Value, json};

use crate::usage::{ErrorKind, Outcome, validate_opaque_id};

/// The version of the event schema this build writes.
///
/// Every line carries it, because a log that outlives a release is read by
/// something that was not built with it. Adding an [`EventKind`] or a field
/// is a bump of this number, not an edit in place.
pub const EVENT_SCHEMA_VERSION: u32 = 1;

/// The longest an opaque id in an event may be, in characters.
pub const MAX_ID_LEN: usize = 128;

/// What happened.
///
/// Closed: a new kind is a schema-version bump, a reviewable decision about
/// what a log line is allowed to say, and not a variant appended on a
/// Friday. [`EventKind::Operational`] is where work goes that is not about a person's
/// content — a nightly pass that had nothing to do, a rebuild, a health
/// check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// One ask of the brain.
    BrainTurn,
    /// One change made by the nightly evolve pass.
    NightlyEvolve,
    /// One MCP tool call.
    McpCall,
    /// One colony handed off to a successor.
    ColonyHandoff,
    /// Work that is not about any content.
    Operational,
}

impl EventKind {
    /// The wire name of this kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BrainTurn => "brain_turn",
            Self::NightlyEvolve => "nightly_evolve",
            Self::McpCall => "mcp_call",
            Self::ColonyHandoff => "colony_handoff",
            Self::Operational => "operational",
        }
    }
}

/// Which store an id in a [`Subject`] refers to.
///
/// A kind rather than a bare id, because `01H…` means nothing on its own: an
/// id is only a reference if the reader knows which store to ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubjectKind {
    /// A page in the pages store.
    Page,
    /// A message in a channel (Slack, Discord, mail).
    Message,
    /// A workspace.
    Workspace,
    /// An MCP tool.
    Tool,
}

impl SubjectKind {
    /// The wire name of this kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Page => "page",
            Self::Message => "message",
            Self::Workspace => "workspace",
            Self::Tool => "tool",
        }
    }
}

/// The MCP tools this build knows about.
///
/// Closed for the same reason [`EventKind`] is: the name is what makes an MCP
/// call line useful, and an open `String` here would be a free-text field on
/// the one line type that is most likely to be aggregated by name. These are
/// the three tools the CLI's MCP server registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolName {
    /// Search the team brain.
    BrainSearch,
    /// Read a page by slug.
    BrainPage,
    /// Add a note to the brain.
    BrainNote,
}

impl ToolName {
    /// The wire name of this tool — the exact string the MCP client sent,
    /// which is what a log reader will match against.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BrainSearch => "brain_search",
            Self::BrainPage => "brain_page",
            Self::BrainNote => "brain_note",
        }
    }
}

/// Why an opaque id was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidId {
    /// What was wrong, in one sentence. Never the value: an id that was
    /// rejected is not a thing to echo into a log, and this is a log crate.
    pub reason: &'static str,
}

impl std::fmt::Display for InvalidId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.reason)
    }
}

impl std::error::Error for InvalidId {}

/// A reference to content: what kind of thing, and which one.
///
/// Never the thing. There is no field here that holds a title, a slug, a
/// summary or a body, and there must not be one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Subject {
    /// Which store the id refers to.
    pub kind: SubjectKind,
    /// The opaque id. Validated by [`Subject::new`]; the field is public so a
    /// reader can read it, and assigning to it directly is the one way to
    /// bypass the validator — so do not.
    pub id: String,
}

impl Subject {
    /// Reference an item by id.
    ///
    /// # Errors
    ///
    /// [`InvalidId`] when `id` is empty, longer than [`MAX_ID_LEN`], or holds
    /// anything outside `[A-Za-z0-9._:-]`. That last rule is what stops a page
    /// body, a subject line or an email from arriving here as an "id".
    pub fn new(kind: SubjectKind, id: &str) -> Result<Self, InvalidId> {
        validate_opaque_id(
            id,
            "an id is letters, digits, dot, underscore, colon or hyphen only",
        )
        .map_err(|reason| InvalidId { reason })?;
        Ok(Self {
            kind,
            id: id.to_owned(),
        })
    }
}

/// One line of the event log.
///
/// Every field is a reference, a closed vocabulary word, a number or an
/// opaque id. There is no content field and no `Deserialize`, and the doc on
/// this type says why twice because it is the point of the whole crate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Event {
    /// Always [`EVENT_SCHEMA_VERSION`]; set by [`Event::new`].
    pub schema_version: u32,
    /// The id carried across Slack, the Worker, the MCP server and the CLI, so
    /// one turn can be followed from the message that caused it to the page it
    /// produced. Opaque, and validated as one.
    pub trace_id: String,
    /// What happened.
    pub kind: EventKind,
    /// When, in seconds since the Unix epoch.
    pub at_unix: u64,
    /// Whether it worked, and if it was attempted at all.
    pub outcome: Outcome,
    /// How long it took, in milliseconds.
    ///
    /// An exact figure, unlike the usage payload's buckets, because an event
    /// line stays on the machine that wrote it and is read by whoever is
    /// debugging that machine. Bucketing here would cost the reader the thing
    /// they opened the log for.
    pub duration_ms: u64,
    /// What it was about, if anything.
    pub subject: Option<Subject>,
    /// How it failed, if it did.
    pub error_kind: Option<ErrorKind>,
    /// Which tool, for an MCP call.
    pub tool: Option<ToolName>,
    /// Which colony, for a hand-off. Opaque id, validated like the others.
    pub colonizer_colony: Option<String>,
}

impl Event {
    /// Start an event.
    ///
    /// `at_unix` is passed in rather than read from a clock: this crate holds
    /// no clock and no RNG, for the same reason it holds no HTTP client.
    ///
    /// # Errors
    ///
    /// [`InvalidId`] when `trace_id` is not an opaque id.
    pub fn new(kind: EventKind, trace_id: &str, at_unix: u64) -> Result<Self, InvalidId> {
        validate_opaque_id(
            trace_id,
            "a trace id is letters, digits, dot, underscore, colon or hyphen only",
        )
        .map_err(|reason| InvalidId { reason })?;
        Ok(Self {
            schema_version: EVENT_SCHEMA_VERSION,
            trace_id: trace_id.to_owned(),
            kind,
            at_unix,
            outcome: Outcome::Success,
            duration_ms: 0,
            subject: None,
            error_kind: None,
            tool: None,
            colonizer_colony: None,
        })
    }

    /// Say how it turned out.
    #[must_use]
    pub fn with_outcome(mut self, outcome: Outcome) -> Self {
        self.outcome = outcome;
        self
    }

    /// Say how long it took.
    #[must_use]
    pub fn with_duration(mut self, duration_ms: u64) -> Self {
        self.duration_ms = duration_ms;
        self
    }

    /// Say what it was about, by reference.
    #[must_use]
    pub fn with_subject(mut self, subject: Subject) -> Self {
        self.subject = Some(subject);
        self
    }

    /// Say how it failed. A failure without a kind is [`ErrorKind::Internal`]
    /// in everything but the log, so this is how a failure is recorded.
    #[must_use]
    pub fn with_error(mut self, kind: ErrorKind) -> Self {
        self.outcome = Outcome::Failure;
        self.error_kind = Some(kind);
        self
    }

    /// Name the tool, for an MCP call.
    #[must_use]
    pub fn with_tool(mut self, tool: ToolName) -> Self {
        self.tool = Some(tool);
        self
    }

    /// Name the colony, by opaque id.
    ///
    /// # Errors
    ///
    /// [`InvalidId`] when `colony` is not an opaque id.
    pub fn with_colony(mut self, colony: &str) -> Result<Self, InvalidId> {
        validate_opaque_id(
            colony,
            "a colony id is letters, digits, dot, underscore, colon or hyphen only",
        )
        .map_err(|reason| InvalidId { reason })?;
        self.colonizer_colony = Some(colony.to_owned());
        Ok(self)
    }

    /// The event as one line of JSON, with no newline in it.
    ///
    /// One line is the format: a log that needs a second line to be read is a
    /// log a tool has to parse, and the whole point is to be tailed. JSON
    /// escapes control characters, so no value that reached this struct can
    /// break the line — the assertion is the belt to that braces.
    #[must_use]
    pub fn to_json_line(&self) -> String {
        let line = serde_json::to_string(self).unwrap_or_else(|_| "{}".to_owned());
        debug_assert!(!line.contains(['\n', '\r']), "an event line is one line");
        line
    }

    /// The schema, as a value: every field with its name, its type and what
    /// it means.
    ///
    /// Generated from the same types that serialise the data, so it cannot
    /// drift from them — which is the whole reason this crate needs no
    /// JSON-schema dependency. `docs/brain-events.schema.json` is checked
    /// against this.
    #[must_use]
    pub fn schema() -> Value {
        json!({
            "schema_version": EVENT_SCHEMA_VERSION,
            "title": "Living Brain brain event",
            "description":
                "One JSON object per brain turn, nightly-evolve change, MCP call or \
                 colony hand-off. Content is referenced by opaque id and never \
                 included; there is no field that can hold message text or a page body.",
            "fields": [
                {
                    "name": "schema_version",
                    "type": "integer",
                    "description": "The version of this schema. Always the current one."
                },
                {
                    "name": "trace_id",
                    "type": "string",
                    "description": "Opaque id carried across Slack, Worker, MCP and CLI. \
                                    Non-empty, at most 128 characters, [A-Za-z0-9._:-] only."
                },
                {
                    "name": "kind",
                    "type": "enum",
                    "description": "What happened. Closed; a new kind is a schema-version bump."
                },
                {
                    "name": "at_unix",
                    "type": "integer",
                    "description": "When, in seconds since the Unix epoch."
                },
                {
                    "name": "outcome",
                    "type": "enum",
                    "description": "success, failure or skipped."
                },
                {
                    "name": "duration_ms",
                    "type": "integer",
                    "description": "How long it took, in milliseconds. Not bucketed: this \
                                    line stays on the machine that wrote it."
                },
                {
                    "name": "subject",
                    "type": "object?",
                    "description": "{ kind, id } — what it was about, by opaque id. Never \
                                    the content. Null when there was none."
                },
                {
                    "name": "error_kind",
                    "type": "enum?",
                    "description": "config, upstream, timeout, decode, permission or internal. \
                                    Null when it did not fail."
                },
                {
                    "name": "tool",
                    "type": "enum?",
                    "description": "The MCP tool called: brain_search, brain_page or brain_note. \
                                    Null when no tool was called."
                },
                {
                    "name": "colonizer_colony",
                    "type": "string?",
                    "description": "Opaque colony id for a hand-off. Null when this is not a \
                                    hand-off."
                }
            ],
            "vocabularies": {
                "kind": [
                    EventKind::BrainTurn.as_str(),
                    EventKind::NightlyEvolve.as_str(),
                    EventKind::McpCall.as_str(),
                    EventKind::ColonyHandoff.as_str(),
                    EventKind::Operational.as_str(),
                ],
                "subject_kind": [
                    SubjectKind::Page.as_str(),
                    SubjectKind::Message.as_str(),
                    SubjectKind::Workspace.as_str(),
                    SubjectKind::Tool.as_str(),
                ],
                "outcome": [
                    Outcome::Success.as_str(),
                    Outcome::Failure.as_str(),
                    Outcome::Skipped.as_str(),
                ],
                "error_kind": [
                    ErrorKind::Config.as_str(),
                    ErrorKind::Upstream.as_str(),
                    ErrorKind::Timeout.as_str(),
                    ErrorKind::Decode.as_str(),
                    ErrorKind::Permission.as_str(),
                    ErrorKind::Internal.as_str(),
                ],
                "tool": [
                    ToolName::BrainSearch.as_str(),
                    ToolName::BrainPage.as_str(),
                    ToolName::BrainNote.as_str(),
                ],
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        EVENT_SCHEMA_VERSION, ErrorKind, Event, EventKind, InvalidId, Outcome, Subject,
        SubjectKind, ToolName,
    };

    #[test]
    fn event_names_match_serde() {
        for (kind, text) in [
            (EventKind::BrainTurn, "brain_turn"),
            (EventKind::NightlyEvolve, "nightly_evolve"),
            (EventKind::McpCall, "mcp_call"),
            (EventKind::ColonyHandoff, "colony_handoff"),
            (EventKind::Operational, "operational"),
        ] {
            assert_eq!(kind.as_str(), text);
            assert_eq!(serde_json::to_string(&kind).unwrap(), format!("\"{text}\""));
        }
        for (kind, text) in [
            (SubjectKind::Page, "page"),
            (SubjectKind::Message, "message"),
            (SubjectKind::Workspace, "workspace"),
            (SubjectKind::Tool, "tool"),
        ] {
            assert_eq!(kind.as_str(), text);
            assert_eq!(serde_json::to_string(&kind).unwrap(), format!("\"{text}\""));
        }
        for (tool, text) in [
            (ToolName::BrainSearch, "brain_search"),
            (ToolName::BrainPage, "brain_page"),
            (ToolName::BrainNote, "brain_note"),
        ] {
            assert_eq!(tool.as_str(), text);
            assert_eq!(serde_json::to_string(&tool).unwrap(), format!("\"{text}\""));
        }
    }

    /// A line is one line, with the schema version on it.
    #[test]
    fn a_line_is_one_line_of_json() {
        let event = Event::new(EventKind::BrainTurn, "01JABC.123-x", 1_700_000_000)
            .unwrap()
            .with_outcome(Outcome::Success)
            .with_duration(420);
        let line = event.to_json_line();
        assert!(!line.contains('\n'));
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["schema_version"], EVENT_SCHEMA_VERSION);
        assert_eq!(value["trace_id"], "01JABC.123-x");
        assert_eq!(value["kind"], "brain_turn");
        assert_eq!(value["duration_ms"], 420);
        assert!(value["subject"].is_null());
    }

    /// A failure records how it failed, not what it said.
    #[test]
    fn a_failure_carries_a_kind() {
        let event = Event::new(EventKind::McpCall, "trace-1", 1)
            .unwrap()
            .with_error(ErrorKind::Permission)
            .with_tool(ToolName::BrainPage);
        assert_eq!(event.outcome, Outcome::Failure);
        assert_eq!(event.error_kind, Some(ErrorKind::Permission));
        let line = event.to_json_line();
        assert!(line.contains("\"error_kind\":\"permission\""));
        assert!(line.contains("\"tool\":\"brain_page\""));
    }

    /// Opaque ids are refused when they are not opaque.
    #[test]
    fn ids_are_ids() {
        assert!(Subject::new(SubjectKind::Page, "01JABC123").is_ok());
        assert!(Subject::new(SubjectKind::Page, "a.b_c:d-e").is_ok());
        assert!(Subject::new(SubjectKind::Page, "").is_err());
        assert!(Subject::new(SubjectKind::Page, &"a".repeat(129)).is_err());
        assert!(Subject::new(SubjectKind::Page, "a page body").is_err());
        assert!(Event::new(EventKind::Operational, "trace-1", 1).is_ok());
        assert!(Event::new(EventKind::Operational, "trace 1", 1).is_err());
        assert!(
            Event::new(EventKind::ColonyHandoff, "trace-1", 1)
                .unwrap()
                .with_colony("colony-9")
                .is_ok()
        );
        assert!(
            Event::new(EventKind::ColonyHandoff, "trace-1", 1)
                .unwrap()
                .with_colony("a colony")
                .is_err()
        );
        let err: InvalidId = Subject::new(SubjectKind::Page, "").unwrap_err();
        assert!(!err.to_string().is_empty());
    }

    /// The schema lists every field the serialiser writes, and nothing else.
    #[test]
    fn the_schema_lists_the_serialised_fields() {
        let schema = Event::schema();
        let mut named: Vec<&str> = schema["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|field| field["name"].as_str().unwrap())
            .collect();
        named.sort_unstable();
        let event = Event::new(EventKind::Operational, "t", 1).unwrap();
        let line = event.to_json_line();
        let parsed = serde_json::from_str::<serde_json::Value>(&line).unwrap();
        let mut written: Vec<&str> = parsed
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        // Both sides are sorted because the comparison is about *which* fields
        // are written, not about their order in the document. `serde_json::Map`
        // iterates as a `BTreeMap` unless some crate in the workspace has
        // switched on its `preserve_order` feature (`schemars`, via
        // `cratefield-core`, does), and Cargo unifies features across the
        // workspace — so key order here is a property of whatever else happens
        // to be compiled, and is not something this test may rely on.
        written.sort_unstable();
        assert_eq!(named, written);
        // No described field is named after content. Checked against the
        // field names rather than the whole document, because the prose is
        // allowed to say "there is no field for a page body".
        for content_word in ["body", "text", "content", "markdown", "title"] {
            assert!(
                !named.contains(&content_word),
                "the schema describes a `{content_word}` field"
            );
        }
        assert_eq!(schema["schema_version"], EVENT_SCHEMA_VERSION);
    }
}
