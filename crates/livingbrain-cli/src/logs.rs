//! `livingbrain logs` — sync the session logs local coding agents already
//! wrote into the source ledger.
//!
//! Claude Code and Codex keep complete JSONL transcripts of every session on
//! the machine that ran them — `~/.claude/projects`, `~/.codex/sessions`. This
//! module reads those files, normalises both formats into one versioned
//! record, and uploads the sessions whose repositories are opted in (`logs
//! allow`, one JSON file in the CLI's config directory). The order of the work
//! is the point, as in [`crate::import`]: walk, parse, decide, redact — all
//! locally — and only then read a token and open a socket. With nothing opted
//! in, `logs sync` makes no request at all, the auth one included.
//!
//! What a record carries is the repository's **detail level** (`logs allow
//! --detail`): `full` — every message, reasoning summary and tool call with
//! its input and a clipped result; `messages` — the messages, tool names only;
//! `summary` — the messages cut to 2 000 characters, tool names only.
//! **Redaction is local and not optional** — the home prefix becomes `~`, and
//! every string of the record then goes through `livingbrain_redact`, the
//! library the server runs on arrival, *before* it is serialised, so the bytes
//! that cross the socket are the redacted ones and always parse. The body is
//! also the identity: a record serialises deterministically, so the ledger's
//! `(scope, sha256)` key makes a re-sync read rows back instead of sealing
//! copies.
//!
//! Each uploaded session also gets a searchable note filed under its repo's
//! project ([`note`]), because a source alone is not a page and `search`
//! and `ask` read pages.

mod claude;
mod clean;
mod codex;
mod note;
mod route;
mod tools;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use clap::Subcommand;
use ignore::WalkBuilder;
use livingbrain_redact::{Policy, redact};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::api::Client;
use crate::auth::Token;
use crate::{CliError, CliResult, Out, err};
use route::{Destination, Repos, Route};

/// The version of the normalised session schema this build writes: adding a
/// field is a bump of this number, not an edit in place. 2: tool calls carry
/// their input and result, `reasoning` and `event` turns, `detail`,
/// `omitted_turns`. 3: a tool call that launched a subagent carries the
/// subagent's own session (`subagent`, recursively), an assistant turn names
/// its `model` where it changes, and `omitted_subagent_turns`. A version-1 or
/// version-2 record still reads: every field it has kept its name and
/// meaning.
pub(crate) const LOG_SCHEMA_VERSION: u32 = 3;

/// The most text one turn keeps under `summary`, in characters — the cut
/// every turn took before detail levels existed.
const SUMMARY_TURN_CHARS: usize = 2000;

/// The most of one message (or reasoning summary) any level keeps: its first
/// and last 128 KiB. Far past a real message; what it bounds is a pasted log,
/// and what it guarantees is that every string fits redaction's input cap.
const MAX_TEXT_BYTES: usize = 256 * 1024;

/// The most tool calls one turn names; the body cap is what bounds a loop.
const MAX_TOOL_CALLS: usize = 128;

/// The largest body this module builds: three quarters of redaction's input
/// cap, because the server redacts the whole body again on arrival and a
/// replacement token can be longer than what it replaced — and under the
/// ledger's own ceiling besides. A session over it loses turns ([`cap`]) and
/// says so.
const MAX_SESSION_BODY_BYTES: usize = livingbrain_redact::MAX_INPUT_BYTES * 3 / 4;

/// The largest the body may be once escaped into a JSON string: what
/// `GET /v1/sources/{id}` answers it inside, under the edge's 1 MiB response
/// buffer, with room for the envelope around it. A body of quotes and
/// newlines grows when escaped, so this binds before [`MAX_SESSION_BODY_BYTES`]
/// does for a tool-heavy session.
const MAX_ESCAPED_BODY_BYTES: usize = 1024 * 1024 - 32 * 1024;

/// The `kind` agent sessions are filed under on the wire.
const SOURCE_KIND: &str = "agent_log";

/// The scope sessions land in: an agent log is the reader's own activity.
const SCOPE: &str = "personal";

/// The opt-in file, under [`config_dir`].
const ALLOWLIST_FILE: &str = "logs-allow.json";

/// The opt-in file's own shape version, so a future format is migrated rather
/// than guessed at. The `detail` map is an optional field of version 1: a
/// build that predates it ignores it and sends no more than `summary`.
const ALLOWLIST_VERSION: u32 = 1;

/// The version written once a file names a workspace or a route. A build that
/// predates routing refuses it outright ("is not 1"), so it can never read a
/// routed file as if the routes were absent and send everything to the default.
const ROUTING_VERSION: u32 = 2;

/// The top-level keys this build reads. Any other key is an error, not a
/// silent default: a misspelled `routes` would otherwise send its repositories
/// to the default workspace.
const KNOWN_KEYS: [&str; 6] = [
    "version",
    "allowed",
    "detail",
    "workspace",
    "routes",
    "default_workspace",
];

/// The environment variable the Mac app sets with its default workspace's
/// token: the workspace the token must belong to (checked once per run).
const DEFAULT_WORKSPACE_ENV: &str = "LIVINGBRAIN_DEFAULT_WORKSPACE";

/// The agents this build reads. The name is the schema's `agent` value and
/// half the source's title on the wire, so it is spelled once.
#[derive(Debug, Clone, Copy)]
enum Agent {
    /// Claude Code, `~/.claude/projects`.
    ClaudeCode,
    /// Codex, `~/.codex/sessions`.
    Codex,
}

impl Agent {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude_code",
            Self::Codex => "codex",
        }
    }
}

/// How much of a session a repository sends. Ordered from least to most, so
/// the most restrictive of several is their minimum.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
pub enum Detail {
    /// Messages cut to 2 000 characters, tool names only
    Summary,
    /// Every message in full, tool names only — no tool input or output
    Messages,
    /// Every message, reasoning summary and tool call with input and result
    Full,
}

impl Detail {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Summary => "summary",
            Self::Messages => "messages",
            Self::Full => "full",
        }
    }
}

/// The `livingbrain logs` subcommands.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Allow syncing sessions whose log records a working directory inside this one
    Allow {
        /// The repository to allow (default: the current directory)
        path: Option<String>,
        /// How much of each session to send (default for a new allow: full)
        #[arg(long, value_enum)]
        detail: Option<Detail>,
        /// The workspace this repository's sessions go to: a workspace id, or
        /// `default`. Without it, the routes decide (default: the default workspace)
        #[arg(long, value_name = "ID")]
        workspace: Option<String>,
    },
    /// Revoke a path `logs allow` added
    Deny {
        /// The repository to forget (default: the current directory)
        path: Option<String>,
    },
    /// Send an allowed repository's sessions to a workspace (`default` for the
    /// default workspace, even when a route would pick another)
    Workspace {
        /// The allowed repository
        path: String,
        /// A workspace id, or `default`
        workspace: String,
    },
    /// Print which repositories are opted in, at what level and into which
    /// workspace, and the routes that send the rest
    Status,
    /// Choose the default workspace: the one sessions with no workspace of
    /// their own go to. Without an id, clears the choice (the default token is
    /// then used as before)
    DefaultWorkspace {
        /// A workspace id, or nothing to clear
        workspace: Option<String>,
    },
    /// Read local agent session logs and upload the opted-in ones
    Sync {
        /// Read Claude Code sessions from here instead of ~/.claude/projects
        #[arg(long, value_name = "DIR")]
        claude_dir: Option<PathBuf>,
        /// Read Codex sessions from here instead of ~/.codex/sessions
        #[arg(long, value_name = "DIR")]
        codex_dir: Option<PathBuf>,
        /// Parse, normalise and report, but never touch the network
        #[arg(long)]
        dry_run: bool,
        /// Sync only this session file (repeatable). It must sit under the
        /// Claude Code or Codex directory, and its repo must still be opted in
        #[arg(long = "session", value_name = "FILE")]
        sessions: Vec<PathBuf>,
    },
    /// Print the redacted record `logs sync` would send for one session file,
    /// opted in or not. Local only: never touches the network
    Show {
        /// The session's `.jsonl` file
        file: PathBuf,
        /// Read Claude Code sessions from here instead of ~/.claude/projects
        #[arg(long, value_name = "DIR")]
        claude_dir: Option<PathBuf>,
        /// Read Codex sessions from here instead of ~/.codex/sessions
        #[arg(long, value_name = "DIR")]
        codex_dir: Option<PathBuf>,
    },
}

/// Run one `logs` subcommand.
pub fn run(command: &Command, api_url: &str, out: &Out) -> CliResult<()> {
    match command {
        Command::Allow {
            path,
            detail,
            workspace,
        } => allow(path.as_deref(), *detail, workspace.as_deref(), out),
        Command::Deny { path } => deny(path.as_deref(), out),
        Command::Workspace { path, workspace } => set_workspace(path, workspace, out),
        Command::Status => status(out),
        Command::DefaultWorkspace { workspace } => set_default_workspace(workspace.as_deref(), out),
        Command::Sync {
            claude_dir,
            codex_dir,
            dry_run,
            sessions,
        } => sync(
            claude_dir.as_deref(),
            codex_dir.as_deref(),
            *dry_run,
            sessions,
            api_url,
            out,
        ),
        Command::Show {
            file,
            claude_dir,
            codex_dir,
        } => show(file, claude_dir.as_deref(), codex_dir.as_deref(), out),
    }
}

// ---------------------------------------------------------------------------
// The normalised record

/// One agent session, normalised out of whichever log it came from. Field
/// order here is wire order: `serde` writes the fields as declared, which is
/// one half of the determinism the ledger's dedupe rests on.
#[derive(Debug, Serialize)]
struct SessionRecord {
    schema_version: u32,
    agent: &'static str,
    collector_version: u32,
    /// The detail level the record was built at.
    detail: Detail,
    session_id: String,
    /// The first working directory the log recorded, with the home prefix
    /// rewritten to `~` ([`elide_home`]) before anything serialises.
    repo: String,
    /// Every distinct working directory the session ran in — the opt-in is
    /// decided on all of them, never just the first. Local only: it does not
    /// travel, `repo` is the wire's name for where the session happened.
    #[serde(skip)]
    cwds: Vec<String>,
    branch: Option<String>,
    /// Distinct, in first-seen order: a session may switch models mid-way.
    models: Vec<String>,
    started_at: Option<String>,
    ended_at: Option<String>,
    /// The conversation in order: messages, reasoning summaries, events, and
    /// each assistant step's tool calls.
    turns: Vec<Turn>,
    totals: Totals,
    /// How the session ended, when its log says. No format this build reads
    /// records one; the field is here from version 1 so a collector that
    /// learns it does not move every field after it.
    outcome: Option<String>,
    /// Whether anything was cut to fit the size cap.
    truncated: bool,
    /// How many turns the size cap removed; an `event` turn marks where.
    omitted_turns: usize,
    /// How many turns inside subagents the size cap removed — the cap trims
    /// subagents' inner turns before it touches the main conversation.
    omitted_subagent_turns: usize,
    /// Which tool call launched which subagent, as the log links them: the
    /// subagent's id (Claude Code's `agentId`, Codex's agent path) to the
    /// launching call's id. Local only.
    #[serde(skip)]
    agent_links: BTreeMap<String, String>,
    /// How each launched subagent ended, by its launching call's id, as the
    /// log says (a result, a task notification). Local only.
    #[serde(skip)]
    subagent_status: BTreeMap<String, &'static str>,
    /// How the session itself ended, when its log says (Codex's
    /// `task_complete`): what a subagent's status falls back to. Local only.
    #[serde(skip)]
    final_status: Option<&'static str>,
}

/// Who a turn is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Role {
    /// The person.
    User,
    /// The agent: its text, and the tool calls of that step.
    Assistant,
    /// A reasoning summary the log carried in the clear.
    Reasoning,
    /// Something that happened, not something said — an interruption, a
    /// compaction, turns the size cap removed.
    Event,
}

/// One turn of the conversation.
#[derive(Debug, Serialize)]
struct Turn {
    role: Role,
    at: Option<String>,
    /// The model that wrote an assistant turn — named only where it changes
    /// from the turn list's previous assistant turn ([`mark_model_changes`]),
    /// so a session on one model names it once.
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    text: String,
    tool_calls: Vec<ToolCall>,
}

impl Turn {
    fn new(role: Role, at: &str, text: &str) -> Self {
        Self {
            role,
            at: present(at),
            model: None,
            text: text.to_owned(),
            tool_calls: Vec::new(),
        }
    }
}

/// A subagent's own session, nested in the tool call that launched it: who
/// it was, what it ran on, how it ended, what it cost, and its turns — the
/// same turn shape as the session's, recursively, so a subagent that
/// launched one carries it too. Every field is always present, in this
/// order: the wire order.
#[derive(Debug, Default, Serialize)]
struct Subagent {
    /// Claude Code's `agentId`, Codex's thread id.
    id: String,
    /// The kind of agent: Claude Code's `subagent_type`, Codex's role.
    name: String,
    /// What it was asked to do, as the launch described it.
    description: Option<String>,
    /// Distinct, in first-seen order.
    models: Vec<String>,
    started_at: Option<String>,
    ended_at: Option<String>,
    /// `completed`, `failed`, `stopped` or `running`, when the log says.
    status: Option<&'static str>,
    totals: Totals,
    turns: Vec<Turn>,
    /// Whether the size cap trimmed this subagent's turns.
    truncated: bool,
    /// How many of its turns the size cap removed.
    omitted_turns: usize,
}

impl Subagent {
    /// A collected record as the subagent of the call that launched it.
    fn from_record(record: SessionRecord, id: &str, name: &str, description: &str) -> Self {
        Self {
            id: id.to_owned(),
            name: if name.trim().is_empty() {
                "subagent".to_owned()
            } else {
                name.trim().to_owned()
            },
            description: present(description.trim()),
            models: record.models,
            started_at: record.started_at,
            ended_at: record.ended_at,
            status: record.final_status,
            totals: record.totals,
            turns: record.turns,
            truncated: false,
            omitted_turns: 0,
        }
    }

    /// Its last word: the last assistant turn that said something.
    fn final_answer(&self) -> Option<&str> {
        self.turns
            .iter()
            .rev()
            .find(|turn| turn.role == Role::Assistant && !turn.text.trim().is_empty())
            .map(|turn| turn.text.as_str())
    }
}

/// Name each assistant turn's model only where it differs from the previous
/// assistant turn's in the same list (each subagent's list on its own): the
/// collectors set it on every one. Run after the size cap, so the first
/// assistant turn that survives the cap names its model.
fn mark_model_changes(turns: &mut [Turn]) {
    let mut previous: Option<String> = None;
    for turn in turns.iter_mut() {
        for call in &mut turn.tool_calls {
            if let Some(subagent) = call.subagent.as_mut() {
                mark_model_changes(&mut subagent.turns);
            }
        }
        if turn.role != Role::Assistant {
            continue;
        }
        match turn.model.take() {
            Some(model) if previous.as_deref() != Some(model.as_str()) => {
                previous = Some(model.clone());
                turn.model = Some(model);
            }
            _ => {}
        }
    }
}

/// The tool call with this id, anywhere in `turns` (subagents included).
fn call_by_id<'a>(turns: &'a mut [Turn], id: &str) -> Option<&'a mut ToolCall> {
    for turn in turns {
        for call in &mut turn.tool_calls {
            if call.id.as_deref() == Some(id) {
                return Some(call);
            }
            if let Some(found) = call
                .subagent
                .as_mut()
                .and_then(|subagent| call_by_id(&mut subagent.turns, id))
            {
                return Some(found);
            }
        }
    }
    None
}

/// Nest `subagent` in the call it was launched by: its status from what the
/// launching log said (else its own), and — when the call's result was only
/// a background launch's acknowledgement — its last words as that result,
/// the way a foreground call's result already is.
fn attach_subagent(call: &mut ToolCall, mut subagent: Subagent, status: Option<&'static str>) {
    if let Some(status) = status {
        subagent.status = Some(status);
    }
    // Claude Code's background launch answers with a receipt, Codex's
    // spawn with the agent's path: neither is what the subagent said.
    let acknowledged = call.output.as_deref().is_none_or(|output| {
        output.starts_with("Async agent launched")
            || output.trim().is_empty()
            || serde_json::from_str::<Value>(output)
                .ok()
                .and_then(|value| {
                    value
                        .as_object()
                        .map(|map| map.len() == 1 && map.contains_key("task_name"))
                })
                .unwrap_or(false)
    });
    if acknowledged && let Some(answer) = subagent.final_answer() {
        call.output = Some(clean::clip(answer, tools::MAX_TOOL_OUTPUT_BYTES));
    }
    if call.status.is_none() || call.status == Some("ok") {
        call.status = match subagent.status {
            Some("failed") => Some("error"),
            Some("stopped") => Some("interrupted"),
            _ => call.status.or(Some("ok")),
        };
    }
    call.subagent = Some(Box::new(subagent));
}

/// A tool the agent called. Under `full`, what it was called with and what
/// came back ([`tools`]); under the other levels, its name and nothing else.
#[derive(Debug, Default, Serialize)]
struct ToolCall {
    /// The call's id in its log, to pair its result and its subagent with
    /// it. Local only.
    #[serde(skip)]
    id: Option<String>,
    name: String,
    /// How to show `input`: `shell`, `diff`, `json` or `text`.
    #[serde(skip_serializing_if = "Option::is_none")]
    format: Option<&'static str>,
    /// The file the call read or changed.
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    /// The command, patch or arguments, clipped.
    #[serde(skip_serializing_if = "Option::is_none")]
    input: Option<String>,
    /// The result, clipped to its head and tail.
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<String>,
    /// `ok`, `error` or `interrupted`, when the log says.
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i64>,
    /// The subagent this call launched, with its whole session.
    #[serde(skip_serializing_if = "Option::is_none")]
    subagent: Option<Box<Subagent>>,
}

/// What the session cost, as its own log reports it: a collector that reads a
/// cost sums it, one that does not reports `None` rather than inventing it.
#[derive(Debug, Default, Serialize)]
struct Totals {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
    reasoning_tokens: u64,
    cost_usd: Option<f64>,
}

/// A fresh record for one agent, everything this build does not know yet left
/// empty.
fn empty_record(agent: Agent, collector_version: u32) -> SessionRecord {
    SessionRecord {
        schema_version: LOG_SCHEMA_VERSION,
        agent: agent.as_str(),
        collector_version,
        detail: Detail::Full,
        session_id: String::new(),
        repo: String::new(),
        cwds: Vec::new(),
        branch: None,
        models: Vec::new(),
        started_at: None,
        ended_at: None,
        turns: Vec::new(),
        totals: Totals::default(),
        outcome: None,
        truncated: false,
        omitted_turns: 0,
        omitted_subagent_turns: 0,
        agent_links: BTreeMap::new(),
        subagent_status: BTreeMap::new(),
        final_status: None,
    }
}

/// First non-empty wins: the meta fields repeat on every line.
fn note_once(field: &mut String, value: &str) {
    if field.is_empty() && !value.is_empty() {
        *field = value.to_owned();
    }
}

/// The first non-empty cwd becomes the repo; every distinct one is kept — the
/// opt-in is decided on all of them, never just the first.
fn note_cwd(record: &mut SessionRecord, cwd: &str) {
    if cwd.is_empty() {
        return;
    }
    note_once(&mut record.repo, cwd);
    if !record.cwds.iter().any(|seen| seen == cwd) {
        record.cwds.push(cwd.to_owned());
    }
}

/// The first and last timestamp any understood line carried. The log is
/// chronological, so file order is time order; nothing here reads the clock.
fn note_time(record: &mut SessionRecord, at: &str) {
    if at.is_empty() {
        return;
    }
    if record.started_at.is_none() {
        record.started_at = Some(at.to_owned());
    }
    record.ended_at = Some(at.to_owned());
}

/// A model the session used, once, in first-seen order.
fn note_model(record: &mut SessionRecord, model: &str) {
    if !model.is_empty() && !record.models.iter().any(|seen| seen == model) {
        record.models.push(model.to_owned());
    }
}

/// An event turn — unless the turn before it is the same event, which both
/// agents write twice for one interruption.
fn push_event(record: &mut SessionRecord, at: &str, text: &str) {
    if let Some(last) = record.turns.last()
        && last.role == Role::Event
        && last.text == text
    {
        return;
    }
    record.turns.push(Turn::new(Role::Event, at, text));
}

/// The string at `key`, or `""` — every field of every line type is optional,
/// because the formats are read forward-compatibly.
fn str_field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

/// The u64 at `key`, or 0.
fn num_field(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// `Some(value)` unless the string is empty — the shape every optional field
/// of a record takes.
fn present(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

/// Parse one session file with the collector its directory layout names.
fn collect(text: &str, agent: Agent) -> Option<SessionRecord> {
    match agent {
        Agent::ClaudeCode => claude::collect(text),
        Agent::Codex => codex::collect(text),
    }
}

/// The tool calls a record names, across every turn.
fn tool_call_count(record: &SessionRecord) -> usize {
    record.turns.iter().map(|turn| turn.tool_calls.len()).sum()
}

// ---------------------------------------------------------------------------
// Detail, bounding and redacting the body

/// Bring a record down to `detail`: `messages` drops the reasoning turns and
/// every tool call's input and result; `summary` cuts every turn's text to
/// [`SUMMARY_TURN_CHARS`] besides. Applied before anything else touches the
/// record, so what a level leaves out is never elided, redacted or sent.
fn apply_detail(record: &mut SessionRecord, detail: Detail) {
    record.detail = detail;
    if detail == Detail::Full {
        return;
    }
    detail_turns(&mut record.turns, detail);
}

/// [`apply_detail`] over one turn list, and every subagent's inside it: a
/// subagent keeps its header and its messages at the same level, its tool
/// calls their names (and their own subagents).
fn detail_turns(turns: &mut Vec<Turn>, detail: Detail) {
    turns.retain(|turn| turn.role != Role::Reasoning);
    for turn in turns {
        for call in &mut turn.tool_calls {
            let mut subagent = call.subagent.take();
            if let Some(subagent) = subagent.as_mut() {
                detail_turns(&mut subagent.turns, detail);
                if detail == Detail::Summary
                    && let Some(description) = subagent.description.as_mut()
                {
                    truncate_text(description, SUMMARY_TURN_CHARS);
                }
            }
            *call = ToolCall {
                id: call.id.take(),
                name: std::mem::take(&mut call.name),
                subagent,
                ..ToolCall::default()
            };
        }
        // A step that was only tool calls and said nothing stays: its
        // names are the shape of the work.
        if detail == Detail::Summary {
            truncate_text(&mut turn.text, SUMMARY_TURN_CHARS);
        }
    }
}

/// Every string of the record a person or a tool wrote, mutably — the ones
/// home elision and redaction run over. Tool names and formats are this
/// module's vocabulary or the agent's, not content.
fn content_strings(record: &mut SessionRecord) -> Vec<&mut String> {
    let mut strings: Vec<&mut String> = vec![&mut record.repo];
    if let Some(branch) = record.branch.as_mut() {
        strings.push(branch);
    }
    turn_strings(&mut record.turns, &mut strings);
    strings
}

/// The content strings of one turn list, subagents' included.
fn turn_strings<'a>(turns: &'a mut [Turn], strings: &mut Vec<&'a mut String>) {
    for turn in turns {
        strings.push(&mut turn.text);
        for call in &mut turn.tool_calls {
            for field in [&mut call.path, &mut call.input, &mut call.output] {
                if let Some(value) = field.as_mut() {
                    strings.push(value);
                }
            }
            if let Some(subagent) = call.subagent.as_mut() {
                if let Some(description) = subagent.description.as_mut() {
                    strings.push(description);
                }
                turn_strings(&mut subagent.turns, strings);
            }
        }
    }
}

/// The record as bytes. Fields go out in declaration order and nothing in a
/// record is read from the clock or the environment, so one session is one
/// byte string wherever it is parsed — the property the ledger's
/// `(scope, sha256)` idempotency rests on.
fn serialise(record: &SessionRecord) -> String {
    serde_json::to_string(record).expect("a session record always serialises")
}

/// How long `text` is once escaped into a JSON string, quotes excluded.
fn escaped_len(text: &str) -> usize {
    serde_json::to_string(text)
        .expect("a string always serialises")
        .len()
        - 2
}

/// Clip every message to [`MAX_TEXT_BYTES`]: the one per-text bound every
/// level shares, and the reason every string fits redaction's input.
fn clip_texts(record: &mut SessionRecord) {
    clip_turns(&mut record.turns);
}

fn clip_turns(turns: &mut [Turn]) {
    for turn in turns {
        if turn.text.len() > MAX_TEXT_BYTES {
            turn.text = clean::clip(&turn.text, MAX_TEXT_BYTES);
        }
        for call in &mut turn.tool_calls {
            if let Some(subagent) = call.subagent.as_mut() {
                clip_turns(&mut subagent.turns);
            }
        }
    }
}

/// Bound a record for the wire: whole turns off until the body fits both
/// [`MAX_SESSION_BODY_BYTES`] raw and [`MAX_ESCAPED_BODY_BYTES`] escaped —
/// measured once, subtracted arithmetically, not one serialisation per pop.
/// The opening turn stays (it is what the session was for), then the oldest
/// go, so what is kept is the start and the most recent stretch; an `event`
/// turn after the opening says how many were removed, and the record says so
/// in `truncated` and `omitted_turns`. Deterministic, like everything else
/// here: the same session caps to the same bytes.
fn cap(record: &mut SessionRecord) {
    let full = serialise(record);
    let (raw, escaped) = (full.len(), escaped_len(&full));
    if raw <= MAX_SESSION_BODY_BYTES && escaped <= MAX_ESCAPED_BODY_BYTES {
        return;
    }
    // Room for the marker turn the cut leaves behind.
    const MARKER_ROOM: usize = 256;
    // Subagents first: their inner turns are the detail a reader can most
    // spare, and each keeps its header and its final answer.
    let mut excess = (
        raw as isize - MAX_SESSION_BODY_BYTES as isize,
        escaped as isize - MAX_ESCAPED_BODY_BYTES as isize,
    );
    let mut trimmed = 0usize;
    trim_subagents(&mut record.turns, &mut excess, &mut trimmed);
    if trimmed > 0 {
        record.truncated = true;
        record.omitted_subagent_turns += trimmed;
    }
    let full = serialise(record);
    let (raw, escaped) = (full.len(), escaped_len(&full));
    if raw <= MAX_SESSION_BODY_BYTES && escaped <= MAX_ESCAPED_BODY_BYTES {
        return;
    }
    let mut excess_raw = (raw + MARKER_ROOM) as isize - MAX_SESSION_BODY_BYTES as isize;
    let mut excess_escaped = (escaped + 2 * MARKER_ROOM) as isize - MAX_ESCAPED_BODY_BYTES as isize;
    // Keep the opening turn when there is more than one to shed from.
    let keep = usize::from(record.turns.len() > 1);
    let mut cut = keep;
    while (excess_raw > 0 || excess_escaped > 0) && cut < record.turns.len() {
        let turn = serde_json::to_string(&record.turns[cut]).expect("a turn always serialises");
        // A removed turn takes its own JSON plus the comma before it.
        excess_raw -= turn.len() as isize + 1;
        excess_escaped -= escaped_len(&turn) as isize + 1;
        cut += 1;
    }
    let omitted = cut - keep;
    if omitted > 0 {
        let at = record.turns[keep..cut]
            .iter()
            .find_map(|turn| turn.at.clone())
            .unwrap_or_default();
        record.turns.drain(keep..cut);
        record.turns.insert(
            keep,
            Turn::new(
                Role::Event,
                &at,
                &format!("{omitted} earlier turn(s) omitted to fit the upload size cap"),
            ),
        );
    }
    record.truncated = true;
    record.omitted_turns += omitted;
    // A single turn over the cap on its own: its text and tool results go,
    // the shape stays.
    if serialise(record).len() > MAX_SESSION_BODY_BYTES
        || escaped_len(&serialise(record)) > MAX_ESCAPED_BODY_BYTES
    {
        shrink_turns(&mut record.turns);
    }
}

/// The size cap's last resort, recursively: texts cut to the summary length,
/// tool inputs and results gone, the shape kept.
fn shrink_turns(turns: &mut [Turn]) {
    for turn in turns {
        truncate_text(&mut turn.text, SUMMARY_TURN_CHARS);
        for call in &mut turn.tool_calls {
            call.input = None;
            call.output = None;
            if let Some(subagent) = call.subagent.as_mut() {
                shrink_turns(&mut subagent.turns);
            }
        }
    }
}

/// The size cap's first pass: every subagent in `turns`, oldest first, loses
/// its inner turns from the oldest on — never its final answer — until the
/// `excess` (raw, escaped) is paid off; then the subagents among the turns
/// it kept. The removed run becomes one `event` turn, and the subagent says
/// how many went. A subagent nested inside a removed turn goes with it,
/// counted as that one turn.
fn trim_subagents(turns: &mut [Turn], excess: &mut (isize, isize), trimmed: &mut usize) {
    const MARKER_ROOM: isize = 160;
    for turn in turns {
        for call in &mut turn.tool_calls {
            if excess.0 <= 0 && excess.1 <= 0 {
                return;
            }
            let Some(subagent) = call.subagent.as_mut() else {
                continue;
            };
            if subagent.turns.len() < 2 {
                continue;
            }
            let keep = subagent
                .turns
                .iter()
                .rposition(|turn| turn.role == Role::Assistant && !turn.text.trim().is_empty())
                .unwrap_or(subagent.turns.len() - 1);
            let mut cut = 0usize;
            excess.0 += MARKER_ROOM;
            excess.1 += 2 * MARKER_ROOM;
            while (excess.0 > 0 || excess.1 > 0) && cut < keep {
                let turn =
                    serde_json::to_string(&subagent.turns[cut]).expect("a turn always serialises");
                excess.0 -= turn.len() as isize + 1;
                excess.1 -= escaped_len(&turn) as isize + 1;
                cut += 1;
            }
            if cut == 0 {
                excess.0 -= MARKER_ROOM;
                excess.1 -= 2 * MARKER_ROOM;
                // Nothing of its own to spare: what it launched next.
                trim_subagents(&mut subagent.turns, excess, trimmed);
                continue;
            }
            let at = subagent.turns[..cut]
                .iter()
                .find_map(|turn| turn.at.clone())
                .unwrap_or_default();
            subagent.turns.drain(..cut);
            subagent.turns.insert(
                0,
                Turn::new(
                    Role::Event,
                    &at,
                    &format!(
                        "{cut} earlier turn(s) of this subagent omitted to fit the upload size cap"
                    ),
                ),
            );
            subagent.truncated = true;
            subagent.omitted_turns += cut;
            *trimmed += cut;
            // Still over: the subagents among the turns it kept.
            trim_subagents(&mut subagent.turns, excess, trimmed);
        }
    }
}

/// `text` cut to at most `max` characters, on a char boundary.
fn truncate_text(text: &mut String, max: usize) {
    if let Some((cut, _)) = text.char_indices().nth(max) {
        text.truncate(cut);
    }
}

/// Rewrite the places this machine could leak from a record, before redaction
/// runs: the home prefix becomes `~`, in the repo and every string a turn
/// carries, and so does Claude Code's dash-encoded project-dir form of any
/// path **under `$HOME`** — that encoding shows up precisely in text that
/// never wrote the home out. Encoded forms of directories outside `$HOME` are
/// prose as far as the reader is concerned, and no home means nothing to
/// rewrite.
fn elide_home(record: &mut SessionRecord, home: &Option<PathBuf>) {
    let Some(home) = home else { return };
    let home_text = home.to_string_lossy();
    // A bare `/` would rewrite every absolute path into a relative one.
    if home_text.len() <= 1 {
        return;
    }
    let mut needles = vec![home_text.to_string()];
    for ancestor in Path::new(&record.repo).ancestors() {
        if ancestor.starts_with(home.as_path()) && ancestor.components().count() >= 2 {
            needles.push(ancestor.to_string_lossy().replace('/', "-"));
        }
    }
    // Longest first, so `-home-alice-code-demo` is rewritten whole rather
    // than left as `~-demo` by its shorter prefix.
    needles.sort_by_key(|needle| std::cmp::Reverse(needle.len()));
    needles.dedup();
    for text in content_strings(record) {
        for needle in &needles {
            if text.contains(needle.as_str()) {
                *text = text.replace(needle.as_str(), "~");
            }
        }
    }
}

/// Redact every content string of the record in place, on this machine, one
/// value at a time — never the serialised JSON, where a match can swallow
/// half of an escape (`\n` + `name@host` read as `nname@host`) and leave
/// bytes that no longer parse. Returns the number of findings.
fn redact_record(record: &mut SessionRecord, file: &Path) -> CliResult<usize> {
    let mut findings = 0usize;
    for text in content_strings(record) {
        if text.is_empty() {
            continue;
        }
        let (clean, found) = redact(text, Policy::Redact)
            .map_err(|error| err(format!("{}: {error}", file.display())))?;
        findings += found.len();
        *text = clean;
    }
    Ok(findings)
}

/// The body for a record whose strings are already redacted: serialised,
/// then checked as a whole the way the server will read it. Every value was
/// redacted on its own, so this pass ordinarily finds nothing; a finding only
/// the serialised form shows (a pattern straddling an escape or two values)
/// is taken all the same — a secret never travels to keep a body parseable —
/// and a replacement that broke an escape is repaired ([`repair_escapes`]).
fn build_body(record: &SessionRecord, file: &Path) -> CliResult<(String, usize)> {
    let body = serialise(record);
    match redact(&body, Policy::Redact) {
        Ok((_, findings)) if findings.is_empty() => Ok((body, 0)),
        Ok((clean, findings)) => {
            let parses = |text: &str| serde_json::from_str::<Value>(text).is_ok();
            let clean = if parses(&clean) {
                clean
            } else {
                let repaired = repair_escapes(&clean);
                if parses(&repaired) { repaired } else { clean }
            };
            Ok((clean, findings.len()))
        }
        Err(error) => Err(err(format!("{}: {error}", file.display()))),
    }
}

/// Every backslash that does not start a valid JSON escape, doubled — what a
/// replacement that swallowed half an escape (`\n` + match → `\[REDACTED…`)
/// leaves behind becomes a literal backslash again.
fn repair_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.peek() {
            Some('"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't' | 'u') => {
                out.push('\\');
                out.push(chars.next().expect("peeked"));
            }
            _ => out.push_str("\\\\"),
        }
    }
    out
}

/// Everything between a parsed record and its body, in the one order that is
/// safe: the level first (what it leaves out is never touched again), then
/// home elision, the per-text clip, per-value redaction when a body is
/// wanted, and the size cap last, on the redacted sizes. Returns the body and
/// its finding count when `build` is set.
fn prepare(
    record: &mut SessionRecord,
    detail: Detail,
    home: &Option<PathBuf>,
    build: bool,
    file: &Path,
) -> CliResult<Option<(String, usize)>> {
    apply_detail(record, detail);
    elide_home(record, home);
    clip_texts(record);
    if !build {
        cap(record);
        mark_model_changes(&mut record.turns);
        return Ok(None);
    }
    let findings = redact_record(record, file)?;
    cap(record);
    mark_model_changes(&mut record.turns);
    let (body, more) = build_body(record, file)?;
    Ok(Some((body, findings + more)))
}

// ---------------------------------------------------------------------------
// The opt-in file

/// The opt-in file's shape on disk; the allowed paths as a set, so the file
/// reads back the same however many times `logs allow` ran, and each path's
/// detail level beside it. A path with no level is `full`; a level this build
/// does not know makes the file unreadable, never a guess.
///
/// `workspace` (each allowed root's workspace, `default` included) and
/// `routes` are optional fields of version 1, like `detail` was: a build that
/// predates them ignores them and sends everything to the default workspace,
/// so upgrade every CLI that reads this file.
#[derive(Debug, Default, Serialize, Deserialize)]
struct AllowlistFile {
    version: u32,
    allowed: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    detail: BTreeMap<String, Detail>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    workspace: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    routes: Vec<Route>,
    /// The workspace sessions with no workspace of their own go to: the one
    /// whose token the default destination uses. Written by `logs
    /// default-workspace` (the Mac app does this when the user chooses it), so
    /// the app and a terminal agree on it. Absent: the default token is used
    /// as it always was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    default_workspace: Option<String>,
}

impl AllowlistFile {
    /// The level an allowed root sends at.
    fn level(&self, root: &str) -> Detail {
        self.detail.get(root).copied().unwrap_or(Detail::Full)
    }

    /// The workspace an allowed root's sessions go to, `default` when it names
    /// none. Only the explicit map is consulted; routes come after it.
    fn explicit_workspace(&self, root: &str) -> Option<&str> {
        self.workspace.get(root).map(String::as_str)
    }

    /// Whether every stored workspace and route is well formed, and no two
    /// configured workspace ids would share an environment-variable name.
    fn validate(&self) -> Result<(), String> {
        for (root, target) in &self.workspace {
            route::check_target(target).map_err(|error| format!("{root}: {error}"))?;
        }
        if let Some(id) = &self.default_workspace {
            route::check_target(id).map_err(|error| format!("default_workspace: {error}"))?;
        }
        for route in &self.routes {
            route.validate()?;
        }
        let mut names: BTreeMap<String, &str> = BTreeMap::new();
        let configured = self
            .workspace
            .values()
            .map(String::as_str)
            .chain(self.routes.iter().map(|route| route.workspace.as_str()))
            .filter(|id| *id != route::DEFAULT);
        for id in configured {
            let name = crate::auth::workspace_env_var(id);
            match names.get(&name) {
                Some(other) if *other != id => {
                    return Err(format!(
                        "workspace ids `{other}` and `{id}` would share the environment variable {name}; rename one"
                    ));
                }
                Some(_) => {}
                None => {
                    names.insert(name, id);
                }
            }
        }
        Ok(())
    }
}

/// The CLI's configuration directory: `$XDG_CONFIG_HOME/livingbrain` when
/// `XDG_CONFIG_HOME` is set and absolute (as the spec requires), else
/// `$HOME/.config/livingbrain`. The allowlist is the one config file the CLI
/// keeps; tokens stay in the OS keychain.
fn config_dir() -> CliResult<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
    {
        return Ok(dir.join("livingbrain"));
    }
    match std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| !home.as_os_str().is_empty())
    {
        Some(home) => Ok(home.join(".config").join("livingbrain")),
        None => Err(err(
            "could not find a config directory: set XDG_CONFIG_HOME or HOME",
        )),
    }
}

fn allowlist_path() -> CliResult<PathBuf> {
    Ok(config_dir()?.join(ALLOWLIST_FILE))
}

/// The allowed paths, read from disk. A missing file is the ordinary
/// nothing-opted-in case; a corrupt one is an error — silently reading a
/// broken allowlist as empty would sync exactly the sessions the reader meant
/// to keep off the wire.
fn read_allowlist() -> CliResult<AllowlistFile> {
    let path = allowlist_path()?;
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(AllowlistFile {
                version: ALLOWLIST_VERSION,
                ..AllowlistFile::default()
            });
        }
        Err(error) => return Err(err(format!("could not read {}: {error}", path.display()))),
    };
    parse_allowlist(&bytes).map_err(|error| err(format!("{}: {error}", path.display())))
}

/// Parse an allowlist file's bytes, refusing a version this build does not
/// read: guessing at a future format could read a list its writer meant
/// differently.
fn parse_allowlist(bytes: &[u8]) -> Result<AllowlistFile, String> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|error| format!("not a valid opt-in file ({error}); fix or remove it"))?;
    let version = value
        .get("version")
        .and_then(Value::as_u64)
        .ok_or_else(|| "not a valid opt-in file (no version); fix or remove it".to_owned())?;
    if version != u64::from(ALLOWLIST_VERSION) && version != u64::from(ROUTING_VERSION) {
        return Err(format!(
            "opt-in file version {version} is not {ALLOWLIST_VERSION} or {ROUTING_VERSION} (this build's); upgrade the CLI or fix the file"
        ));
    }
    if version == u64::from(ALLOWLIST_VERSION) {
        let names_routing = value
            .get("workspace")
            .and_then(Value::as_object)
            .is_some_and(|map| !map.is_empty())
            || value
                .get("routes")
                .and_then(Value::as_array)
                .is_some_and(|routes| !routes.is_empty());
        if names_routing {
            return Err(format!(
                "opt-in file version {ALLOWLIST_VERSION} cannot name workspaces or routes; it needs version {ROUTING_VERSION} (upgrade the CLI or fix the file)"
            ));
        }
    }
    if let Some(key) = value.as_object().and_then(|object| {
        object
            .keys()
            .find(|key| !KNOWN_KEYS.contains(&key.as_str()))
    }) {
        return Err(format!(
            "opt-in file has an unknown field `{key}`; fix or remove it"
        ));
    }
    let file: AllowlistFile = serde_json::from_value(value)
        .map_err(|error| format!("not a valid opt-in file ({error}); fix or remove it"))?;
    file.validate()?;
    Ok(file)
}

/// The version a file is written as: 1 unless it names a workspace or a route,
/// so a file an older build can still read stays readable by it.
fn version_for(file: &AllowlistFile) -> u32 {
    if file.workspace.is_empty() && file.routes.is_empty() && file.default_workspace.is_none() {
        ALLOWLIST_VERSION
    } else {
        ROUTING_VERSION
    }
}

/// Write the allowlist back, creating the config directory if this is the
/// first thing the CLI has ever persisted there. Returns the path, for the
/// report.
fn write_allowlist(file: &mut AllowlistFile) -> CliResult<PathBuf> {
    // Refuse to save a config that would not read back, or that two workspaces
    // would share a token variable in.
    file.validate().map_err(err)?;
    // Version 1 unless the file uses routing: a file an older build can still
    // read stays readable by it.
    file.version = version_for(file);
    let path = allowlist_path()?;
    let parent = path
        .parent()
        .ok_or_else(|| err("the config path has no parent"))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| err(format!("could not create {}: {error}", parent.display())))?;
    let body = serde_json::to_vec_pretty(file).expect("an allowlist always serialises");
    std::fs::write(&path, body)
        .map_err(|error| err(format!("could not write {}: {error}", path.display())))?;
    Ok(path)
}

/// The canonical form an allowed path is stored and matched under: absolute,
/// lexically clean, and resolved through the filesystem when it exists. A
/// path that does not exist is allowed all the same — a checkout need not be
/// mounted here to be opted in — so the fallback is lexical, not an error.
fn normalise_path(raw: &str) -> CliResult<String> {
    let path = Path::new(raw);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        let cwd = std::env::current_dir()
            .map_err(|error| err(format!("could not read the working directory: {error}")))?;
        cwd.join(path)
    };
    let cleaned = match std::fs::canonicalize(&absolute) {
        Ok(resolved) => resolved,
        Err(_) => lexically_absolute(&absolute),
    };
    Ok(cleaned.to_string_lossy().into_owned())
}

/// An absolute path with `.` and `..` resolved away, for a path the
/// filesystem cannot confirm.
fn lexically_absolute(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Whether `child` is `ancestor` itself or somewhere inside it. The comparison
/// is component-wise, so `/home/alice/code-demo` is not inside
/// `/home/alice/code` however many bytes they share.
fn is_inside(child: &str, ancestor: &str) -> bool {
    // Both sides are cleaned lexically first, so `..` cannot make a path look
    // inside a root it is not inside.
    let child = lexical(child);
    let ancestor = lexical(ancestor);
    child == ancestor
        || (child.len() > ancestor.len()
            && child.starts_with(&ancestor)
            && child.as_bytes()[ancestor.len()] == b'/')
}

/// A path string with `.` and `..` resolved away.
fn lexical(path: &str) -> String {
    lexically_absolute(Path::new(path))
        .to_string_lossy()
        .into_owned()
}

/// One recorded working directory is opted in when it — or the path it really
/// is, once symlinks are resolved — sits inside an allowed root. The raw form
/// counts because a session may have run in a checkout this machine reaches
/// only through a symlink.
fn cwd_opted_in(cwd: &str, allowed: &BTreeSet<String>) -> bool {
    allowed.iter().any(|root| is_inside(cwd, root))
        || std::fs::canonicalize(cwd)
            .map(|resolved| {
                let resolved = resolved.to_string_lossy();
                allowed.iter().any(|root| is_inside(&resolved, root))
            })
            .unwrap_or(false)
}

/// Whether the session may upload: **every** working directory it recorded
/// must be opted in — fail closed, so a session that `cd`'d into another
/// repository does not ride the first repo's allowance.
fn opted_in(record: &SessionRecord, allowed: &BTreeSet<String>) -> bool {
    !record.cwds.is_empty() && record.cwds.iter().all(|cwd| cwd_opted_in(cwd, allowed))
}

/// The level a session is sent at: for each working directory, the level
/// of the most specific allowed root it sits inside (raw or resolved), and
/// across directories the most restrictive of those. Only meaningful for a
/// session [`opted_in`] already admitted.
fn detail_for(record: &SessionRecord, allowlist: &AllowlistFile) -> Detail {
    record
        .cwds
        .iter()
        .filter_map(|cwd| {
            let resolved = std::fs::canonicalize(cwd)
                .map(|path| path.to_string_lossy().into_owned())
                .ok();
            allowlist
                .allowed
                .iter()
                .filter(|root| {
                    is_inside(cwd, root)
                        || resolved
                            .as_deref()
                            .is_some_and(|resolved| is_inside(resolved, root))
                })
                .max_by_key(|root| root.len())
                .map(|root| allowlist.level(root))
        })
        .min()
        .unwrap_or(Detail::Full)
}

/// The environment hint the Mac app sets on every run when it holds a
/// workspace sign-in: the app's keychain items cannot be listed from here, so
/// the app says so.
pub const MULTI_WORKSPACE_ENV: &str = "LIVINGBRAIN_MULTI_WORKSPACE";

/// Whether this machine routes sessions among workspaces: the opt-in file
/// names a workspace or a route, or the Mac app says it holds sign-ins.
fn several_workspaces(allowlist: &AllowlistFile) -> bool {
    !allowlist.workspace.is_empty()
        || !allowlist.routes.is_empty()
        || std::env::var_os(MULTI_WORKSPACE_ENV).as_deref() == Some(std::ffi::OsStr::new("1"))
}

/// `logs allow`: insert one path, at `detail` when given — a new path
/// otherwise at `full`, an allowed one keeping its level. Idempotent, and it
/// writes the file even when nothing changed, so the reader sees where the
/// decision lives.
fn allow(
    path: Option<&str>,
    detail: Option<Detail>,
    workspace: Option<&str>,
    out: &Out,
) -> CliResult<()> {
    if let Some(workspace) = workspace {
        route::check_target(workspace).map_err(err)?;
    }
    let target = normalise_path(path.unwrap_or("."))?;
    let mut allowlist = read_allowlist()?;
    // Without a workspace a new opt-in would take the default token, and on a
    // machine that signs in to several workspaces that is the wrong one for
    // most repositories. A checkout under a parent that names a workspace is
    // the exception: it follows that parent (see `nearest_explicit`). Refuse
    // before anything is written.
    if workspace.is_none()
        && several_workspaces(&allowlist)
        && nearest_explicit(&allowlist, &target).is_none()
    {
        return Err(err(
            "this machine signs in to several workspaces or routes sessions; name the workspace: `--workspace <id>`, or `--workspace default` for the default workspace",
        ));
    }
    let before = allowlist.level(&target);
    let before_workspace = allowlist.explicit_workspace(&target).map(str::to_owned);
    // A bare nested checkout is written with its parent's workspace, so that
    // later changes to the parent (deny, auto) cannot leave it unrouted: a
    // nested checkout with no workspace of its own would fall to the default
    // token.
    let inherited = match workspace {
        Some(_) => None,
        None => nearest_explicit(&allowlist, &target).map(|(_, id)| id.to_owned()),
    };
    let added = allowlist.allowed.insert(target.clone());
    let level = detail.unwrap_or(if added { Detail::Full } else { before });
    allowlist.detail.insert(target.clone(), level);
    if let Some(workspace) = workspace.map(str::to_owned).or(inherited) {
        allowlist.workspace.insert(target.clone(), workspace);
    }
    let workspace_changed =
        allowlist.explicit_workspace(&target).map(str::to_owned) != before_workspace;
    let changed = added || level != before || workspace_changed;
    let file = write_allowlist(&mut allowlist)?;
    print_allowlist(&allowlist, changed, &file, out, || {
        match (added, changed) {
            (true, _) => println!("Allowed {target} (detail: {}).", level.as_str()),
            (false, true) => println!("Allowed {target}, now at detail {}.", level.as_str()),
            (false, false) => println!("Already allowed: {target} (detail: {})", level.as_str()),
        }
        if let Some(workspace) = workspace {
            println!("Its sessions go to workspace {workspace}.");
        }
    });
    Ok(())
}

/// `logs workspace <path> <id|default>`: send an allowed repository's sessions
/// to one workspace. Refused for a path that is not opted in, so a typo cannot
/// quietly create a rule for nothing.
fn set_workspace(path: &str, workspace: &str, out: &Out) -> CliResult<()> {
    if workspace != route::AUTO {
        route::check_target(workspace).map_err(err)?;
    }
    let target = normalise_path(path)?;
    let mut allowlist = read_allowlist()?;
    if !allowlist.allowed.contains(&target) {
        return Err(err(format!(
            "{target} is not opted in; run `livingbrain logs allow {path}` first"
        )));
    }
    let before = allowlist.explicit_workspace(&target).map(str::to_owned);
    let (changed, human) = if workspace == route::AUTO {
        pin_nested_children(&mut allowlist, &target);
        allowlist.workspace.remove(&target);
        (
            before.is_some(),
            format!("{target} now follows its routes again."),
        )
    } else {
        allowlist
            .workspace
            .insert(target.clone(), workspace.to_owned());
        (
            before.as_deref() != Some(workspace),
            format!("{target} now sends to workspace {workspace}."),
        )
    };
    let file = write_allowlist(&mut allowlist)?;
    print_allowlist(&allowlist, changed, &file, out, || println!("{human}"));
    Ok(())
}

/// Before a parent's workspace is removed or its opt-in dropped, write that
/// workspace onto each nested checkout that inherits it by lookup, so the
/// checkout keeps routing where it did. Checkouts that name their own
/// workspace, or sit under another explicit root, are untouched.
fn pin_nested_children(allowlist: &mut AllowlistFile, root: &str) {
    let Some(workspace) = allowlist.explicit_workspace(root).map(str::to_owned) else {
        return;
    };
    let children: Vec<String> = allowlist
        .allowed
        .iter()
        .filter(|child| child.as_str() != root && is_inside(child, root))
        .filter(|child| allowlist.explicit_workspace(child).is_none())
        .filter(|child| nearest_explicit(allowlist, child).is_some_and(|(owner, _)| owner == root))
        .cloned()
        .collect();
    for child in children {
        allowlist.workspace.insert(child, workspace.clone());
    }
}

/// `logs default-workspace [<id>]`: choose the workspace sessions with no
/// workspace of their own go to, or clear the choice.
fn set_default_workspace(workspace: Option<&str>, out: &Out) -> CliResult<()> {
    if let Some(id) = workspace {
        route::check_target(id).map_err(err)?;
    }
    let mut allowlist = read_allowlist()?;
    let changed = allowlist.default_workspace.as_deref() != workspace;
    allowlist.default_workspace = workspace.map(str::to_owned);
    let file = write_allowlist(&mut allowlist)?;
    print_allowlist(&allowlist, changed, &file, out, || match workspace {
        Some(id) => println!("Default workspace is now {id}."),
        None => println!("Default workspace cleared."),
    });
    Ok(())
}

/// `logs deny`: remove one path and its level. Idempotent.
fn deny(path: Option<&str>, out: &Out) -> CliResult<()> {
    let target = normalise_path(path.unwrap_or("."))?;
    let mut allowlist = read_allowlist()?;
    pin_nested_children(&mut allowlist, &target);
    let changed = allowlist.allowed.remove(&target);
    allowlist.detail.remove(&target);
    allowlist.workspace.remove(&target);
    let file = write_allowlist(&mut allowlist)?;
    print_allowlist(&allowlist, changed, &file, out, || {
        if changed {
            println!("No longer allowed: {target}");
        } else {
            println!("Was not opted in: {target}");
        }
    });
    Ok(())
}

/// The report `allow` and `deny` share: the list, each path's level, whether
/// anything changed, and where the file is.
fn print_allowlist(
    allowlist: &AllowlistFile,
    changed: bool,
    file: &Path,
    out: &Out,
    human: impl FnOnce(),
) {
    let list: Vec<&String> = allowlist.allowed.iter().collect();
    out.json_or(
        &json!({
            "allowed": list,
            "detail": levels(allowlist),
            "workspace": workspaces(allowlist),
            "changed": changed,
            "config": file.display().to_string(),
        }),
        || {
            human();
            println!("Opt-in file: {}", file.display());
        },
    );
}

/// Every allowed path's level, `full` filled in where the file names none.
fn levels(allowlist: &AllowlistFile) -> BTreeMap<&str, &'static str> {
    allowlist
        .allowed
        .iter()
        .map(|root| (root.as_str(), allowlist.level(root).as_str()))
        .collect()
}

/// Every allowed path's workspace, `default` filled in where the file names
/// none. A route may still send a root elsewhere; this is the explicit choice.
fn workspaces(allowlist: &AllowlistFile) -> BTreeMap<&str, &str> {
    allowlist
        .allowed
        .iter()
        .map(|root| {
            (
                root.as_str(),
                allowlist.explicit_workspace(root).unwrap_or(route::DEFAULT),
            )
        })
        .collect()
}

/// What `logs status` shows for one allowed root.
struct RootStatus {
    /// Where the root's own sessions go: a workspace id, `default`,
    /// `unroutable`, or `per-repository` (each checkout inside decides).
    effective: String,
    reason: Option<String>,
    /// The parent root whose explicit workspace this root follows.
    explicit_from: Option<String>,
    repo_top: Option<String>,
    /// Checkouts inside a folder that decide for themselves: path, effective, reason.
    nested: Vec<(String, String, Option<String>)>,
}

/// The status word for a root whose checkouts each decide for themselves.
const PER_REPOSITORY: &str = "per-repository";

/// Judge one allowed root for display. Sync judges each session on the
/// checkout it ran in, so a root that is not itself a checkout, or that holds
/// checkouts, has no single answer: it is reported as `per-repository` and its
/// checkouts are listed with their own answers. An explicit choice still
/// covers everything inside, and is reported as such.
fn root_status(root: &str, allowlist: &AllowlistFile, repos: &mut Repos) -> RootStatus {
    let res = resolve(root, allowlist, repos);
    let explicit_from = res.explicit_root.clone().filter(|parent| parent != root);
    let own = |res: Resolution| RootStatus {
        effective: res.destination.key().to_owned(),
        reason: res.route_reason,
        explicit_from: explicit_from.clone(),
        repo_top: res.repo_top,
        nested: Vec::new(),
    };
    if res.explicit_root.is_some() || allowlist.routes.is_empty() {
        return own(res);
    }
    let inside = route::nested_checkouts(root);
    if inside.is_empty() {
        // Either the root is its own checkout, or it is not one and nothing
        // inside is either: git's reason stands.
        return own(res);
    }
    // A root that is itself a checkout is listed first with its own answer, so a
    // held root shows as held and can be pinned like its nested checkouts.
    let mut nested: Vec<(String, String, Option<String>)> = Vec::new();
    if res.repo_top.is_some() {
        nested.push((
            root.to_owned(),
            res.destination.key().to_owned(),
            res.route_reason.clone(),
        ));
    }
    nested.extend(inside.into_iter().map(|path| {
        let judged = resolve(&path, allowlist, repos);
        (
            path,
            judged.destination.key().to_owned(),
            judged.route_reason,
        )
    }));
    RootStatus {
        effective: PER_REPOSITORY.to_owned(),
        reason: Some(PER_REPOSITORY.to_owned()),
        explicit_from: None,
        repo_top: res.repo_top,
        nested,
    }
}

fn status(out: &Out) -> CliResult<()> {
    let allowlist = read_allowlist()?;
    let file = allowlist_path()?;
    let list: Vec<&String> = allowlist.allowed.iter().collect();
    let mut repos = Repos::default();
    let mut rows = BTreeMap::new();
    for root in &allowlist.allowed {
        rows.insert(root.as_str(), root_status(root, &allowlist, &mut repos));
    }
    let explicit: BTreeMap<&str, Option<&str>> = allowlist
        .allowed
        .iter()
        .map(|root| (root.as_str(), allowlist.explicit_workspace(root)))
        .collect();
    out.json_or(
        &json!({
            "allowed": list,
            "detail": levels(&allowlist),
            "workspace": workspaces(&allowlist),
            "workspace_explicit": explicit,
            "workspace_effective": rows.iter().map(|(r, row)| (*r, row.effective.clone())).collect::<BTreeMap<_, _>>(),
            "route_reason": rows.iter().map(|(r, row)| (*r, row.reason.clone())).collect::<BTreeMap<_, _>>(),
            "explicit_from": rows.iter().map(|(r, row)| (*r, row.explicit_from.clone())).collect::<BTreeMap<_, _>>(),
            "repo_top": rows.iter().map(|(r, row)| (*r, row.repo_top.clone())).collect::<BTreeMap<_, _>>(),
            "repos": rows.iter().map(|(r, row)| (*r, row.nested.iter().map(|(path, effective, reason)| json!({
                "path": path, "effective": effective, "route_reason": reason,
            })).collect::<Vec<Value>>())).collect::<BTreeMap<_, _>>(),
            "routes": allowlist.routes,
            "config": file.display().to_string(),
        }),
        || {
            if list.is_empty() {
                println!("No repositories are opted in. Run `livingbrain logs allow` inside one.");
            } else {
                println!("Opted in ({}):", list.len());
                for path in &list {
                    let row = &rows[path.as_str()];
                    let explicit = allowlist.explicit_workspace(path).unwrap_or("auto");
                    let why = match (&row.explicit_from, &row.reason) {
                        (Some(parent), _) => format!(" — from {parent}"),
                        (None, Some(why)) => format!(" — {why}"),
                        (None, None) => String::new(),
                    };
                    println!(
                        "  {path} (detail: {}, workspace: {explicit}, sends to: {}{why})",
                        allowlist.level(path).as_str(),
                        row.effective,
                    );
                    for (nested, effective, reason) in &row.nested {
                        let why = reason.as_deref().map(|r| format!(" — {r}")).unwrap_or_default();
                        println!("      {nested} → {effective}{why}");
                    }
                }
            }
            if !allowlist.routes.is_empty() {
                println!("Routes (first match wins; a root's own workspace comes first):");
                for route in &allowlist.routes {
                    println!("  {} → workspace {}", route.describe(), route.workspace);
                }
            }
            println!("Opt-in file: {}", file.display());
        },
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Sync

/// One parsed session, and everything decided about it. `body` is built only
/// for opted-in sessions — a skipped session never produces bytes.
struct Prepared {
    record: SessionRecord,
    /// The session file, for the local report only — it never travels.
    file: PathBuf,
    opted_in: bool,
    body: Option<String>,
    /// Secrets redacted out of `body`.
    redacted: usize,
    /// The tool-call count, computed once for both reports.
    tool_calls: usize,
    uploaded: Option<String>,
    /// Whether the upload created the row (201) or read one back (200).
    created: bool,
    /// The workspace this session is sent to.
    destination: Destination,
    /// Why the session was held back at upload, when its workspace could not
    /// be signed into: `skipped_no_token` or `skipped_keychain_denied`.
    held: Option<&'static str>,
    /// The session's searchable note — planned for an opted-in session
    /// with a repo, written after its source upload succeeds.
    note: Option<note::Planned>,
}

/// `livingbrain logs sync`: everything before [`upload`] is local — walk,
/// parse, decide, elide, redact — so a machine with nothing opted in runs the
/// whole command without a single packet, the auth request included.
/// `--dry-run` stops after the report for the same reason.
fn sync(
    claude_dir: Option<&Path>,
    codex_dir: Option<&Path>,
    dry_run: bool,
    only: &[PathBuf],
    api_url: &str,
    out: &Out,
) -> CliResult<()> {
    let (home, claude_dir, codex_dir) = session_dirs(claude_dir, codex_dir)?;
    let only = resolve_only(only)?;
    let allowlist = read_allowlist()?;
    let (mut sessions, unreadable, unparsed) = collect_sessions(
        &claude_dir,
        &codex_dir,
        &allowlist,
        &home,
        only.as_ref(),
        false,
    )?;
    let held = upload(&mut sessions, dry_run, api_url)?;
    report(
        &sessions,
        unreadable,
        unparsed,
        dry_run,
        (&claude_dir, &codex_dir),
        &held,
        out,
    );
    Ok(())
}

/// `$HOME` and the two agents' session directories: the flags when given,
/// the defaults under `$HOME` otherwise.
fn session_dirs(
    claude_dir: Option<&Path>,
    codex_dir: Option<&Path>,
) -> CliResult<(Option<PathBuf>, PathBuf, PathBuf)> {
    let home: Option<PathBuf> = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| !home.as_os_str().is_empty());
    let claude_dir = resolve_dir(
        claude_dir,
        home.as_ref()
            .map(|home| home.join(".claude").join("projects")),
        "--claude-dir",
    )?;
    let codex_dir = resolve_dir(
        codex_dir,
        home.as_ref()
            .map(|home| home.join(".codex").join("sessions")),
        "--codex-dir",
    )?;
    Ok((home, claude_dir, codex_dir))
}

/// The `--session` files, resolved through the filesystem so they compare
/// with the walk's paths however they were spelled. `None` when no file was
/// named: the whole tree is scanned.
fn resolve_only(files: &[PathBuf]) -> CliResult<Option<BTreeSet<PathBuf>>> {
    if files.is_empty() {
        return Ok(None);
    }
    files
        .iter()
        .map(|file| {
            std::fs::canonicalize(file)
                .map_err(|error| err(format!("--session {}: {error}", file.display())))
        })
        .collect::<CliResult<BTreeSet<PathBuf>>>()
        .map(Some)
}

/// `livingbrain logs show <file>`: the record exactly as `logs sync` would
/// build it — parsed, home-elided, capped and redacted — printed locally so a
/// reader can see what would leave the machine. Nothing is sent, and no
/// token is read.
fn show(
    file: &Path,
    claude_dir: Option<&Path>,
    codex_dir: Option<&Path>,
    out: &Out,
) -> CliResult<()> {
    let (home, claude_dir, codex_dir) = session_dirs(claude_dir, codex_dir)?;
    let only = resolve_only(&[file.to_path_buf()])?;
    let allowlist = read_allowlist()?;
    let (sessions, _, _) = collect_sessions(
        &claude_dir,
        &codex_dir,
        &allowlist,
        &home,
        only.as_ref(),
        true,
    )?;
    let Some(session) = sessions.into_iter().next() else {
        return Err(err(format!(
            "{}: no agent session in this file",
            file.display()
        )));
    };
    let redacted = session.redacted;
    let body = session
        .body
        .expect("`show` builds a body for every session it reads");
    // Every value is redacted before serialisation, so the body parses;
    // `record` stays nullable for the reader's sake (and an older body).
    let record: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    out.json_or(
        &json!({
            "file": session.file.display().to_string(),
            "opted_in": session.opted_in,
            "redacted": redacted,
            "record": record,
            "body": body,
        }),
        || println!("{body}"),
    );
    Ok(())
}

/// The allowed roots a working directory sits inside, raw or resolved, most
/// specific first.
fn enclosing_roots<'a>(cwd: &str, allowlist: &'a AllowlistFile) -> Vec<&'a str> {
    let resolved = std::fs::canonicalize(cwd)
        .ok()
        .map(|path| path.to_string_lossy().into_owned());
    let mut roots: Vec<&str> = allowlist
        .allowed
        .iter()
        .filter(|root| {
            is_inside(cwd, root)
                || resolved
                    .as_deref()
                    .is_some_and(|resolved| is_inside(resolved, root))
        })
        .map(String::as_str)
        .collect();
    roots.sort_by_key(|root| std::cmp::Reverse(root.len()));
    roots
}

/// The explicit workspace of the nearest enclosing allowed root that names
/// one. A nested checkout inherits its parent's choice this way.
fn nearest_explicit<'a>(allowlist: &'a AllowlistFile, cwd: &str) -> Option<(&'a str, &'a str)> {
    enclosing_roots(cwd, allowlist)
        .into_iter()
        .find_map(|root| {
            allowlist
                .explicit_workspace(root)
                .map(|target| (root, target))
        })
}

/// Where one working directory's session goes, and what decided it.
struct Resolution {
    destination: Destination,
    /// The matched route, or the reason a route could not decide. `None` for
    /// an explicit workspace or a plain default.
    route_reason: Option<String>,
    /// The allowed root whose explicit workspace decided, when one did.
    explicit_root: Option<String>,
    /// The canonical top level of the checkout, when one was read.
    repo_top: Option<String>,
}

impl Resolution {
    fn plain(destination: Destination) -> Self {
        Self {
            destination,
            route_reason: None,
            explicit_root: None,
            repo_top: None,
        }
    }
}

/// Resolve a working directory: the nearest enclosing root's explicit
/// workspace first; else, when routes exist, the first matching route judged
/// on the repository's own facts; else the default. A route that cannot decide
/// holds the directory.
fn resolve(cwd: &str, allowlist: &AllowlistFile, repos: &mut Repos) -> Resolution {
    if let Some((root, target)) = nearest_explicit(allowlist, cwd) {
        return Resolution {
            explicit_root: Some(root.to_owned()),
            ..Resolution::plain(Destination::of_target(target))
        };
    }
    if allowlist.routes.is_empty() {
        return Resolution::plain(Destination::Default);
    }
    match repos.of(cwd) {
        Err(reason) => Resolution {
            destination: Destination::Unroutable(reason.clone()),
            route_reason: Some(reason),
            ..Resolution::plain(Destination::Default)
        },
        Ok(repo) => {
            let repo_top = Some(repo.top.clone());
            match route::first_match(&allowlist.routes, &repo) {
                Err(reason) => Resolution {
                    destination: Destination::Unroutable(reason.clone()),
                    route_reason: Some(reason),
                    repo_top,
                    ..Resolution::plain(Destination::Default)
                },
                Ok(Some((target, why))) => Resolution {
                    destination: Destination::of_target(target),
                    route_reason: Some(why),
                    repo_top,
                    ..Resolution::plain(Destination::Default)
                },
                Ok(None) => Resolution {
                    repo_top,
                    ..Resolution::plain(Destination::Default)
                },
            }
        }
    }
}

/// Where an opted-in session goes: every working directory resolves, and the
/// session's directories must agree (see [`Destination::combine`]).
fn destination_for(
    record: &SessionRecord,
    allowlist: &AllowlistFile,
    repos: &mut Repos,
) -> Destination {
    let destinations: Vec<Destination> = record
        .cwds
        .iter()
        .map(|cwd| resolve(cwd, allowlist, repos).destination)
        .collect();
    Destination::combine(destinations.into_iter())
}

/// Walk both agents' directories, parse every session file, and decide the
/// opt-in — in that order: a session that is not opted in never produces
/// bytes, unless `show_all` (`logs show`, which prints and sends nothing)
/// asks for every body; a session that is not opted in is shown at `full`.
/// Returns the sessions plus two kinds of failure: `unreadable` files could
/// not be read at all, `unparsed` ones were read but yielded no session
/// (empty or foreign `.jsonl`).
fn collect_sessions(
    claude_dir: &Path,
    codex_dir: &Path,
    allowlist: &AllowlistFile,
    home: &Option<PathBuf>,
    only: Option<&BTreeSet<PathBuf>>,
    show_all: bool,
) -> CliResult<(Vec<Prepared>, usize, usize)> {
    let mut sessions = Vec::new();
    let (mut unreadable, mut unparsed) = (0usize, 0usize);
    let mut matched: BTreeSet<PathBuf> = BTreeSet::new();
    // Codex writes a spawned agent's rollout as a file of its own; it is
    // read as part of the session that spawned it, not as one more.
    let spawns = CodexSpawns::index(codex_dir);
    // One repository probe per top level for the whole run, and only when a
    // route needs it.
    let mut repos = Repos::default();
    for (agent, dir, levels) in [
        // Claude Code lays sessions out one directory per project — walk into
        // exactly that one level; Codex nests them by date, so its walk goes
        // all the way down.
        (Agent::ClaudeCode, claude_dir, Some(1usize)),
        (Agent::Codex, codex_dir, None),
    ] {
        for file in session_files(dir, levels) {
            // `--session`: only the named files, matched on their resolved
            // path. The agent is still decided by which tree the file is in.
            let named = only.is_some_and(|only| {
                only.contains(&std::fs::canonicalize(&file).unwrap_or_else(|_| file.clone()))
            });
            if let Some(only) = only {
                let resolved = std::fs::canonicalize(&file).unwrap_or_else(|_| file.clone());
                if !only.contains(&resolved) {
                    continue;
                }
                matched.insert(resolved);
            }
            // A spawned rollout whose spawner is here travels inside it —
            // unless it was named on its own.
            if matches!(agent, Agent::Codex) && spawns.nested.contains(&file) && !named {
                continue;
            }
            let text = match std::fs::read_to_string(&file) {
                Ok(text) => text,
                Err(_) => {
                    unreadable += 1;
                    continue;
                }
            };
            let Some(mut record) = collect(&text, agent) else {
                unparsed += 1;
                continue;
            };
            match agent {
                Agent::ClaudeCode => claude::attach(&mut record, claude::children(&file)),
                Agent::Codex => spawns.attach(&mut record, 0),
            }
            // Opt-in is decided on the cwds as the log recorded them, before
            // anything is rewritten for the wire.
            let opted = opted_in(&record, &allowlist.allowed);
            let detail = if opted {
                detail_for(&record, allowlist)
            } else {
                Detail::Full
            };
            let built = prepare(&mut record, detail, home, opted || show_all, &file)?;
            let (body, redacted) = match built {
                Some((body, redacted)) => (Some(body), redacted),
                None => (None, 0),
            };
            let tool_calls = tool_call_count(&record);
            let planned_note = if opted { note::plan(&record) } else { None };
            let destination = if opted {
                destination_for(&record, allowlist, &mut repos)
            } else {
                Destination::Default
            };
            sessions.push(Prepared {
                record,
                file,
                opted_in: opted,
                body,
                redacted,
                tool_calls,
                uploaded: None,
                created: false,
                note: planned_note,
                destination,
                held: None,
            });
        }
    }
    if let Some(stray) = only.and_then(|only| only.difference(&matched).next()) {
        return Err(err(format!(
            "--session {}: not a session file under {} or {}",
            stray.display(),
            claude_dir.display(),
            codex_dir.display()
        )));
    }
    Ok((sessions, unreadable, unparsed))
}

/// Which Codex rollouts were spawned by which: every rollout's first line,
/// read once per run (the whole tree, whatever `--session` names — a named
/// session's spawned agents are files of their own).
struct CodexSpawns {
    /// Spawned rollouts by the id of the thread that spawned them, in file
    /// order.
    children: BTreeMap<String, Vec<(PathBuf, codex::Spawned)>>,
    /// The spawned rollouts whose spawner is in the tree: not sessions of
    /// their own.
    nested: BTreeSet<PathBuf>,
}

impl CodexSpawns {
    fn index(dir: &Path) -> Self {
        use std::io::BufRead;
        let mut ids: BTreeSet<String> = BTreeSet::new();
        let mut spawned: Vec<(PathBuf, codex::Spawned)> = Vec::new();
        for file in session_files(dir, None) {
            let Ok(handle) = std::fs::File::open(&file) else {
                continue;
            };
            let mut first = String::new();
            if std::io::BufReader::new(handle)
                .read_line(&mut first)
                .is_err()
            {
                continue;
            }
            if let Ok(line) = serde_json::from_str::<Value>(&first)
                && let Some(id) = line
                    .get("payload")
                    .and_then(|payload| payload.get("id"))
                    .and_then(Value::as_str)
            {
                ids.insert(id.to_owned());
            }
            if let Some(spawn) = codex::spawned(&first) {
                spawned.push((file, spawn));
            }
        }
        let mut children: BTreeMap<String, Vec<(PathBuf, codex::Spawned)>> = BTreeMap::new();
        let mut nested = BTreeSet::new();
        for (file, spawn) in spawned {
            if ids.contains(&spawn.parent) {
                nested.insert(file.clone());
                children
                    .entry(spawn.parent.clone())
                    .or_default()
                    .push((file, spawn));
            }
        }
        Self { children, nested }
    }

    /// Nest every rollout `record`'s thread spawned, each with its own.
    fn attach(&self, record: &mut SessionRecord, depth: usize) {
        // Codex nests a handful deep; a loop in a corrupt tree stops here.
        if depth > 8 {
            return;
        }
        let Some(children) = self.children.get(&record.session_id) else {
            return;
        };
        for (file, spawn) in children {
            let Some(mut child) = std::fs::read_to_string(file)
                .ok()
                .and_then(|text| codex::collect(&text))
            else {
                continue;
            };
            self.attach(&mut child, depth + 1);
            codex::attach(record, child, spawn);
        }
    }
}

/// A workspace whose sessions were held at upload, and why.
struct Held {
    workspace: String,
    sessions: usize,
    status: &'static str,
}

/// One workspace group's sessions and how its token was decided.
type Plan<T> = Vec<(String, Lookup<T>, Vec<usize>)>;

/// The token situation of one workspace group.
enum Lookup<T> {
    Ready(T),
    NoToken,
    KeychainDenied,
}

/// Decide each group's token. With the keychain on, the default workspace's own
/// failure is the run's failure, as it always was. With it off (the Mac app's
/// runs) a missing default token is only a held group, decided in `upload`. A
/// routed workspace whose keychain item cannot be read holds only its own
/// sessions (its error text is dropped, it may name the item but never a
/// secret).
fn plan_tokens<T>(
    groups: BTreeMap<String, Vec<usize>>,
    mut lookup: impl FnMut(&str) -> CliResult<Option<T>>,
) -> CliResult<Plan<T>> {
    let mut plan = Vec::new();
    for (workspace, indices) in groups {
        let outcome = match lookup(&workspace) {
            Ok(Some(token)) => Lookup::Ready(token),
            Ok(None) => Lookup::NoToken,
            Err(error) if workspace == route::DEFAULT => return Err(error),
            Err(_) => Lookup::KeychainDenied,
        };
        plan.push((workspace, outcome, indices));
    }
    Ok(plan)
}

/// Upload the opted-in sessions, one POST each, grouped by workspace: each
/// group goes with its own token. This is the command's first socket and its
/// first read of any token; both sit after all the local work on purpose, and
/// `--dry-run` never gets here at all. Every token is read before the first
/// request. With the keychain on, a missing default sign-in fails the run
/// before anything is sent, as it always has; with it off (the Mac app's runs)
/// only the default group is held. The default token must belong to the
/// workspace the app or the opt-in file names, or that group is held too. A
/// session whose workspace cannot be signed into is held (never sent to
/// another workspace) and reported. Unroutable and mixed sessions are never
/// grouped at all.
fn upload(sessions: &mut [Prepared], dry_run: bool, api_url: &str) -> CliResult<Vec<Held>> {
    let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (index, session) in sessions.iter().enumerate() {
        if !session.opted_in {
            continue;
        }
        if matches!(
            session.destination,
            Destination::Default | Destination::Workspace(_)
        ) {
            groups
                .entry(session.destination.key().to_owned())
                .or_default()
                .push(index);
        }
    }
    if dry_run || groups.is_empty() {
        return Ok(Vec::new());
    }
    let from_env = std::env::var(DEFAULT_WORKSPACE_ENV)
        .ok()
        .filter(|id| !id.is_empty());
    let expectation = default_expectation(from_env, &read_allowlist()?);
    let plan = plan_tokens(groups, |workspace| {
        if workspace == route::DEFAULT {
            return match crate::auth::token_for(api_url) {
                Ok(token) => Ok(Some(token)),
                // Keychain off: a run with no default sign-in holds that group.
                Err(CliError::NotLoggedIn(_)) if crate::auth::keychain_gate_process().is_err() => {
                    Ok(None)
                }
                Err(error) => Err(error),
            };
        }
        crate::auth::workspace_token(api_url, workspace)
    })?;
    let mut held = Vec::new();
    for (workspace, outcome, indices) in plan {
        let token = match outcome {
            Lookup::Ready(token) => token,
            Lookup::NoToken => {
                hold(sessions, &indices, "skipped_no_token");
                held.push(Held {
                    workspace,
                    sessions: indices.len(),
                    status: "skipped_no_token",
                });
                continue;
            }
            Lookup::KeychainDenied => {
                hold(sessions, &indices, "skipped_keychain_denied");
                held.push(Held {
                    workspace,
                    sessions: indices.len(),
                    status: "skipped_keychain_denied",
                });
                continue;
            }
        };
        if workspace == route::DEFAULT && default_unconfirmed(&expectation, api_url, &token) {
            hold(sessions, &indices, "skipped_default_mismatch");
            held.push(Held {
                workspace,
                sessions: indices.len(),
                status: "skipped_default_mismatch",
            });
            continue;
        }
        let client = Client::new(api_url).with_token(token);
        for index in indices {
            send(&client, &mut sessions[index])?;
        }
    }
    Ok(held)
}

/// What the default token must belong to in this run.
#[derive(Debug, PartialEq, Eq)]
enum DefaultExpectation {
    /// No default workspace is chosen: the default token is used as it always was.
    Unset,
    /// The default token must belong to this workspace.
    Workspace(String),
    /// The app and the opt-in file name different default workspaces.
    Conflict,
}

/// The expectation from the app's `LIVINGBRAIN_DEFAULT_WORKSPACE` and the
/// opt-in file's `default_workspace`, whichever the run has.
fn default_expectation(from_env: Option<String>, file: &AllowlistFile) -> DefaultExpectation {
    match (from_env, file.default_workspace.as_deref()) {
        (None, None) => DefaultExpectation::Unset,
        (Some(id), None) => DefaultExpectation::Workspace(id),
        (None, Some(id)) => DefaultExpectation::Workspace(id.to_owned()),
        (Some(env), Some(file)) if env == file => DefaultExpectation::Workspace(env),
        _ => DefaultExpectation::Conflict,
    }
}

/// Whether the default token may not be used for this run: the workspaces
/// disagree, or `GET /v1/me` does not confirm that the token belongs to the
/// expected workspace (asked once per run; a failed check holds the group).
/// The token is never printed or logged.
fn default_unconfirmed(expectation: &DefaultExpectation, api_url: &str, token: &Token) -> bool {
    match expectation {
        DefaultExpectation::Unset => false,
        DefaultExpectation::Conflict => true,
        DefaultExpectation::Workspace(id) => {
            match Client::new(api_url).with_token(token.clone()).me() {
                Ok(me) => me["workspace"]["id"].as_str() != Some(id.as_str()),
                Err(_) => true,
            }
        }
    }
}

/// Mark sessions as held at upload, with the status they report.
fn hold(sessions: &mut [Prepared], indices: &[usize], status: &'static str) {
    for &index in indices {
        sessions[index].held = Some(status);
    }
}

/// POST one session's source, then its note.
fn send(client: &Client, session: &mut Prepared) -> CliResult<()> {
    let body = session
        .body
        .as_deref()
        .expect("an opted-in session has a redacted body");
    let title = format!(
        "{} session {}",
        session.record.agent, session.record.session_id
    );
    let project = project_of(&session.record.repo);
    let value = client
        .post_source(SOURCE_KIND, &title, body, SCOPE, project.as_deref())
        .map_err(|error| err(format!("{title}: {error}")))?;
    session.uploaded = Some(crate::text(&value, "id").to_owned());
    session.created = value
        .get("created")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // Uploaded or already there, the source is in the ledger: now the
    // page that makes it findable.
    if let Some(planned) = session.note.as_mut() {
        note::write(
            client,
            &session.record,
            planned,
            session.uploaded.as_deref(),
        );
    }
    Ok(())
}

/// The project a session is filed under: the basename of its repository,
/// cut down to the server's project rule (ASCII letters, digits, `.`, `_`
/// and `-`, starting with a letter or digit, at most 64 bytes). Any other
/// character becomes `-`. `None` when the session recorded no repository, or
/// nothing of its name survives — the upload then simply names no project.
///
/// Only the basename travels in this field: the full (home-elided) path is
/// already inside the record, and a project is a filter, not a location.
fn project_of(repo: &str) -> Option<String> {
    let base = repo
        .rsplit(['/', '\\'])
        .find(|segment| !segment.is_empty() && *segment != "~")?;
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
    let project: String = trimmed.chars().take(64).collect();
    (!project.is_empty()).then_some(project)
}

/// The report, JSON or human: one line per session, then the counts.
fn report(
    sessions: &[Prepared],
    unreadable: usize,
    unparsed: usize,
    dry_run: bool,
    dirs: (&Path, &Path),
    held: &[Held],
    out: &Out,
) {
    let (mut uploaded, mut unchanged, mut skipped, mut mixed, mut unroutable) =
        (0usize, 0usize, 0usize, 0usize, 0usize);
    let mut would_upload = 0usize;
    let mut redacted_total = 0usize;
    let mut notes: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    let mut entries = Vec::new();
    let mut lines = Vec::new();
    for session in sessions {
        let status = if !session.opted_in {
            skipped += 1;
            "skipped"
        } else if session.destination == Destination::Mixed {
            mixed += 1;
            "skipped_mixed_workspace"
        } else if matches!(session.destination, Destination::Unroutable(_)) {
            unroutable += 1;
            "skipped_unroutable"
        } else if let Some(status) = session.held {
            status
        } else if dry_run {
            would_upload += 1;
            "would_upload"
        } else if session.created {
            uploaded += 1;
            "uploaded"
        } else {
            unchanged += 1;
            "unchanged"
        };
        redacted_total += session.redacted;
        let mut entry = json!({
            "agent": session.record.agent,
            "session_id": session.record.session_id,
            "repo": session.record.repo,
            "models": session.record.models,
            "turns": session.record.turns.len(),
            "tool_calls": session.tool_calls,
            "totals": serde_json::to_value(&session.record.totals)
                .expect("totals always serialise"),
            "truncated": session.record.truncated,
            "detail": session.record.detail,
            "status": status,
            // Null for a session that is not opted in: it has no destination.
            "workspace": if session.opted_in {
                json!(session.destination.key())
            } else {
                Value::Null
            },
            "branch": session.record.branch,
            "started_at": session.record.started_at,
            "ended_at": session.record.ended_at,
            "file": session.file.display().to_string(),
        });
        match (status, &session.destination) {
            ("skipped", _) => entry["reason"] = json!("repo not opted in"),
            ("skipped_mixed_workspace", _) => {
                entry["reason"] = json!("its repositories belong to different workspaces");
            }
            ("skipped_unroutable", Destination::Unroutable(why)) => {
                entry["reason"] = json!(format!("cannot route: {why}"));
            }
            ("skipped_no_token", destination) => {
                entry["reason"] =
                    json!(format!("not signed in to workspace {}", destination.key()));
            }
            ("skipped_keychain_denied", destination) => {
                entry["reason"] = json!(format!(
                    "could not read the sign-in for workspace {} from the keychain",
                    destination.key()
                ));
            }
            ("skipped_default_mismatch", destination) => {
                entry["reason"] = json!(format!(
                    "the default sign-in is not confirmed as workspace {}",
                    destination.key()
                ));
            }
            _ => {}
        }
        if let Some(id) = &session.uploaded {
            entry["id"] = json!(id);
        }
        if session.opted_in {
            let (note_entry, note_status) = note::report(session.note.as_ref(), dry_run);
            entry["note"] = note_entry;
            *notes.entry(note_status).or_default() += 1;
        }
        entries.push(entry);
        lines.push(human_line(session, status));
    }

    let held_list = |status: &str| -> Vec<Value> {
        held.iter()
            .filter(|held| held.status == status)
            .map(|held| json!({"workspace": held.workspace, "sessions": held.sessions}))
            .collect()
    };
    let report = json!({
        "scanned": sessions.len(),
        "uploaded": uploaded,
        "unchanged": unchanged,
        "skipped": skipped,
        "skipped_mixed_workspace": mixed,
        "skipped_unroutable": unroutable,
        "skipped_no_token": held_list("skipped_no_token"),
        "skipped_keychain_denied": held_list("skipped_keychain_denied"),
        "skipped_default_mismatch": held_list("skipped_default_mismatch"),
        "unreadable": unreadable,
        "unparsed": unparsed,
        "redacted": redacted_total,
        "dry_run": dry_run,
        "notes": notes,
        "sessions": entries,
    });
    out.json_or(&report, || {
        if sessions.is_empty() {
            println!(
                "No agent sessions under {} or {}.",
                dirs.0.display(),
                dirs.1.display()
            );
            return;
        }
        for line in &lines {
            println!("  {line}");
        }
        let mut summary = if dry_run {
            format!("{would_upload} would upload, {skipped} skipped: repo not opted in")
        } else {
            format!(
                "{uploaded} uploaded, {unchanged} already in the ledger, {skipped} skipped: repo not opted in"
            )
        };
        if mixed > 0 {
            summary.push_str(&format!(
                ", {mixed} skipped: repositories in different workspaces"
            ));
        }
        if unroutable > 0 {
            summary.push_str(&format!(", {unroutable} held: a route cannot read the repository"));
        }
        if unreadable > 0 {
            summary.push_str(&format!(", {unreadable} unreadable"));
        }
        if unparsed > 0 {
            summary.push_str(&format!(", {unparsed} not a session"));
        }
        if redacted_total > 0 {
            summary.push_str(&format!(
                "; {redacted_total} secret(s) redacted on this machine first"
            ));
        }
        println!("{summary}.");
        for held in held {
            match held.status {
                "skipped_keychain_denied" => println!(
                    "  {} session(s) held: could not read workspace {}'s sign-in from the keychain; check Living Brain's keychain access.",
                    held.sessions, held.workspace
                ),
                "skipped_default_mismatch" => println!(
                    "  {} session(s) held: the default sign-in is not confirmed as workspace {}; choose the default workspace again in Living Brain for Mac (Settings → Workspace).",
                    held.sessions, held.workspace
                ),
                _ => println!(
                    "  {} session(s) held: not signed in to workspace {}; sign in to sync them.",
                    held.sessions, held.workspace
                ),
            }
        }
        if !notes.is_empty() {
            let parts: Vec<String> = notes
                .iter()
                .map(|(status, count)| format!("{count} {}", status.replace('_', " ")))
                .collect();
            println!("Notes (searchable with --project <repo>): {}.", parts.join(", "));
        }
        for line in sessions
            .iter()
            .filter_map(|session| note::failure_line(session.note.as_ref()))
        {
            println!("  {line}");
        }
        if dry_run {
            println!("Dry run: nothing was sent.");
        }
    });
}

/// The directory a collector reads: the flag when given (a missing one is a
/// typo, and is said so), otherwise the agent's default under `$HOME` — which
/// may not exist yet, and that is an empty scan, not an error.
fn resolve_dir(
    flag: Option<&Path>,
    default: Option<PathBuf>,
    flag_name: &str,
) -> CliResult<PathBuf> {
    match flag {
        Some(dir) if dir.is_dir() => Ok(dir.to_path_buf()),
        Some(dir) => Err(err(format!(
            "{flag_name} {} is not a directory",
            dir.display()
        ))),
        None => default.ok_or_else(|| {
            err(format!(
                "could not find the default session directory: set HOME or pass {flag_name}"
            ))
        }),
    }
}

/// Every `*.jsonl` session file under `dir`, sorted, so one directory reads
/// and reports in the same order every run. `levels` is how far below `dir`
/// the walk goes: one for Claude Code (`projects/<dir>/<session>.jsonl`),
/// unbounded for Codex (`sessions/YYYY/MM/DD/rollout-*.jsonl`). A missing
/// directory is an empty scan, not an error.
fn session_files(dir: &Path, levels: Option<usize>) -> Vec<PathBuf> {
    let mut builder = WalkBuilder::new(dir);
    // Session logs are data, not notes: no ignore file has a say here, and a
    // `.jsonl` under a dot-directory is still a session.
    builder.hidden(false).git_ignore(false).require_git(false);
    builder.max_depth(levels.map(|levels| levels + 1));
    let mut files: Vec<PathBuf> = builder
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
        })
        .map(|entry| entry.into_path())
        .collect();
    files.sort();
    files
}

/// One line of the human report: the session's shape, and what became of it.
fn human_line(session: &Prepared, status: &str) -> String {
    let totals = &session.record.totals;
    let cost = match totals.cost_usd {
        Some(cost) => format!("${cost:.4}"),
        None => "no cost reported".to_owned(),
    };
    let models = if session.record.models.is_empty() {
        "model not recorded".to_owned()
    } else {
        session.record.models.join(", ")
    };
    let status = match status {
        "skipped" => "skipped: repo not opted in",
        "skipped_mixed_workspace" => "skipped: repositories in different workspaces",
        "skipped_unroutable" => "held: a route cannot read this repository",
        "skipped_no_token" => "held: not signed in to its workspace",
        "skipped_keychain_denied" => "held: keychain denied its sign-in",
        "skipped_default_mismatch" => "held: default sign-in is not its workspace",
        "would_upload" => "would upload",
        "uploaded" => "uploaded",
        _ => "unchanged (already in the ledger)",
    };
    format!(
        "{} {} · {} · {} · {} turn(s), {} tool call(s) · in {}, out {}, cache {} read / {} write · {} · {}",
        session.record.agent,
        short_id(&session.record.session_id),
        session.record.repo,
        models,
        session.record.turns.len(),
        session.tool_calls,
        totals.input_tokens,
        totals.output_tokens,
        totals.cache_read_tokens,
        totals.cache_creation_tokens,
        cost,
        status,
    )
}

/// A session id as a person reads it: the first few characters and an
/// ellipsis. Character-counted, so an id that is not ASCII cannot split.
fn short_id(id: &str) -> String {
    let mut short: String = id.chars().take(8).collect();
    if id.chars().count() > 8 {
        short.push('…');
    }
    short
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::Value;

    use super::{
        Agent, AllowlistFile, Detail, MAX_ESCAPED_BODY_BYTES, MAX_SESSION_BODY_BYTES, Role,
        SUMMARY_TURN_CHARS, apply_detail, cap, collect, elide_home, escaped_len, is_inside,
        parse_allowlist, prepare, project_of, serialise, truncate_text,
    };

    /// The fixtures, embedded at compile time: the same bytes the integration
    /// tests drive the real binary over.
    const CLAUDE_SESSION: &str = include_str!(
        "../tests/fixtures/logs/claude/projects/-home-alice-code-demo/3f2a9c1e-5b7d-4e8a-9c1f-2a4b6d8e0f1a.jsonl"
    );
    const CODEX_SESSION: &str = include_str!(
        "../tests/fixtures/logs/codex/sessions/2026/10/01/rollout-2026-10-01T09-31-00-a1b2c3d4-e5f6-4a9b-8c7d-3e5f7a9b1c2d.jsonl"
    );
    /// The full-detail fixtures: tool calls with inputs and results, a long
    /// output, boilerplate, interruptions, and secrets inside tool output.
    const CLAUDE_FULL: &str = include_str!(
        "../tests/fixtures/logs-full/claude-5e1f2a3b-4c5d-4e6f-8a7b-9c0d1e2f3a4b.jsonl"
    );
    const CODEX_FULL: &str = include_str!(
        "../tests/fixtures/logs-full/codex-b2c3d4e5-f6a7-4b8c-9d0e-1f2a3b4c5d6e.jsonl"
    );

    /// The fixtures with their placeholders filled by key-shaped values,
    /// assembled from fragments so no source file carries one.
    fn planted(text: &str) -> String {
        text.replace(
            "__PLANTED_ANTHROPIC_KEY__",
            &["sk-", "ant-", "api03-fake", &"0".repeat(22)].concat(),
        )
        .replace(
            "__PLANTED_AWS_KEY__",
            &["AKIA", "IOSFODNN7", "EXAMPLE"].concat(),
        )
    }

    /// A record taken all the way to its body, as `logs sync` takes it.
    fn body_of(text: &str, agent: Agent, detail: Detail) -> (String, usize) {
        let mut record = collect(&planted(text), agent).expect("a session");
        prepare(
            &mut record,
            detail,
            &Some("/home/alice".into()),
            true,
            Path::new("fixture.jsonl"),
        )
        .expect("prepares")
        .expect("a body")
    }

    /// The Claude fixture's numbers, hand-computed: the one assistant message
    /// is streamed as two lines repeating the same `message.id`, usage and
    /// `costUSD`, so all three are counted once; the summary, the progress
    /// mark and the torn last line contribute nothing, and the tool result
    /// lands on its call.
    #[test]
    fn claude_totals_count_a_split_message_once() {
        let record = collect(CLAUDE_SESSION, Agent::ClaudeCode).expect("a session");
        assert_eq!(record.schema_version, 3);
        assert_eq!(record.agent, "claude_code");
        assert_eq!(record.session_id, "3f2a9c1e-5b7d-4e8a-9c1f-2a4b6d8e0f1a");
        assert_eq!(record.repo, "/home/alice/code/demo");
        assert_eq!(record.cwds, ["/home/alice/code/demo"]);
        assert_eq!(record.branch.as_deref(), Some("main"));
        assert_eq!(record.models, ["claude-sonnet-4-5"]);
        assert_eq!(
            record.started_at.as_deref(),
            Some("2026-10-01T09:30:00.000Z")
        );
        assert_eq!(record.ended_at.as_deref(), Some("2026-10-01T09:30:08.000Z"));
        assert_eq!(record.totals.input_tokens, 120);
        assert_eq!(record.totals.output_tokens, 45);
        assert_eq!(record.totals.cache_read_tokens, 30);
        assert_eq!(record.totals.cache_creation_tokens, 15);
        assert_eq!(record.totals.reasoning_tokens, 0);
        assert_eq!(record.totals.cost_usd, Some(0.012));
        assert_eq!(record.turns.len(), 2);
        assert_eq!(record.turns[0].role, Role::User);
        assert_eq!(record.turns[1].role, Role::Assistant);
        let call = &record.turns[1].tool_calls[0];
        assert_eq!(call.name, "Bash");
        assert_eq!(call.input.as_deref(), Some("ls ~/.claude/projects"));
        assert_eq!(call.output.as_deref(), Some("demo\nside"));
        assert_eq!(call.status, Some("ok"));
        assert!(record.turns[1].text.contains("They live under"));
        assert!(record.turns[1].text.contains("livingbrain logs sync"));
        assert!(!record.truncated);
    }

    /// The Codex fixture's numbers, hand-computed: two cumulative
    /// `token_count` events, the last one (800/120/96/40) is the session's.
    /// The wrapped `<user_instructions>` opener makes no turn.
    #[test]
    fn codex_takes_the_last_cumulative_token_count() {
        let record = collect(CODEX_SESSION, Agent::Codex).expect("a session");
        assert_eq!(record.agent, "codex");
        assert_eq!(record.session_id, "a1b2c3d4-e5f6-4a9b-8c7d-3e5f7a9b1c2d");
        assert_eq!(record.repo, "/home/alice/code/demo");
        assert_eq!(record.branch.as_deref(), Some("main"));
        assert_eq!(record.models, ["gpt-5.1-codex"]);
        assert_eq!(
            record.started_at.as_deref(),
            Some("2026-10-01T09:31:00.123Z")
        );
        assert_eq!(record.ended_at.as_deref(), Some("2026-10-01T09:31:30.000Z"));
        assert_eq!(record.totals.input_tokens, 800);
        assert_eq!(record.totals.output_tokens, 120);
        assert_eq!(record.totals.cache_read_tokens, 96);
        assert_eq!(record.totals.cache_creation_tokens, 0);
        assert_eq!(record.totals.reasoning_tokens, 40);
        assert_eq!(record.totals.cost_usd, None);
        assert_eq!(record.turns.len(), 3);
        assert_eq!(record.turns[1].role, Role::Assistant);
        assert_eq!(record.turns[1].text, "");
        assert_eq!(record.turns[1].tool_calls[0].name, "shell");
        assert_eq!(record.turns[1].tool_calls[0].input.as_deref(), Some("ls"));
        assert_eq!(
            record.turns[1].tool_calls[0].output.as_deref(),
            Some("README.md\nsrc\n")
        );
        let everywhere: String = serialise(&record);
        assert!(
            !everywhere.contains("user_instructions"),
            "bootstrap context leaked: {everywhere}"
        );
    }

    /// The full Claude fixture: boilerplate gone, thinking as reasoning, the
    /// tool calls in order with inputs and results, the long output clipped
    /// head and tail, the interruption an event, the slash command a turn.
    #[test]
    fn claude_full_detail_reads_the_whole_conversation() {
        let record = collect(CLAUDE_FULL, Agent::ClaudeCode).expect("a session");
        let roles: Vec<Role> = record.turns.iter().map(|turn| turn.role).collect();
        assert_eq!(
            roles,
            [
                Role::User,
                Role::Reasoning,
                Role::Assistant,
                Role::Assistant,
                Role::Assistant,
                Role::Event,
                Role::User,
                Role::Assistant,
            ],
            "{record:#?}"
        );
        assert_eq!(record.turns[0].text, "Fix the failing test in src/lib.rs");
        assert_eq!(
            record.turns[1].text,
            "The test output will say which assertion fails."
        );
        let test_run = &record.turns[2].tool_calls[0];
        assert_eq!(record.turns[2].text, "Running the tests first.");
        assert_eq!(test_run.input.as_deref(), Some("cargo test -p demo"));
        assert_eq!(test_run.format, Some("shell"));
        assert_eq!(test_run.status, Some("error"));
        assert_eq!(test_run.exit_code, Some(101));
        let output = test_run.output.as_deref().expect("an output");
        assert!(
            output.len() <= super::tools::MAX_TOOL_OUTPUT_BYTES + 64,
            "{}",
            output.len()
        );
        assert!(output.starts_with("Exit code 101\ntest case_0000"));
        assert!(output.ends_with("0 passed; 220 failed"));
        assert!(output.contains("bytes omitted"));
        let edit = &record.turns[3].tool_calls[0];
        assert_eq!(edit.name, "Edit");
        assert_eq!(
            edit.path.as_deref(),
            Some("/home/alice/code/demo/src/lib.rs")
        );
        assert_eq!(
            edit.input.as_deref(),
            Some("-assert_eq!(left, right + 1);\n+assert_eq!(left, right);")
        );
        assert_eq!(
            record.turns[5].text,
            "Interrupted by the user during a tool call"
        );
        assert_eq!(record.turns[6].text, "/review src/lib.rs");
        let all = serialise(&record);
        for leaked in [
            "system-reminder",
            "IDE",
            "Image: source",
            "local-command",
            "Reviewed.",
        ] {
            assert!(!all.contains(leaked), "{leaked} leaked: {all}");
        }
    }

    /// The full Codex fixture: developer and bootstrap messages are not
    /// turns, the reasoning summary is (its encrypted content never), the
    /// three calls carry their commands, patch and results, and the
    /// interruption written twice is one event.
    #[test]
    fn codex_full_detail_reads_the_whole_conversation() {
        let record = collect(CODEX_FULL, Agent::Codex).expect("a session");
        let roles: Vec<Role> = record.turns.iter().map(|turn| turn.role).collect();
        assert_eq!(
            roles,
            [
                Role::User,
                Role::Reasoning,
                Role::Assistant,
                Role::Assistant,
                Role::Event,
            ],
            "{record:#?}"
        );
        assert_eq!(record.turns[0].text, "Fix all the TODOs");
        assert!(record.turns[1].text.contains("I'll search the tree first."));
        let calls = &record.turns[2].tool_calls;
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].input.as_deref(), Some("rg -n TODO"));
        assert_eq!(
            calls[0].output.as_deref(),
            Some("src/a.rs:3:// TODO: remove\n")
        );
        assert_eq!(calls[0].exit_code, Some(0));
        assert_eq!(calls[0].status, Some("ok"));
        assert_eq!(calls[1].name, "apply_patch");
        assert_eq!(calls[1].format, Some("diff"));
        assert!(calls[1].input.as_deref().unwrap().contains("+// done"));
        assert_eq!(
            calls[1].output.as_deref(),
            Some("Success. Updated the following files:\nM src/a.rs\n")
        );
        assert_eq!(
            record.turns[3].text,
            "Done: the one TODO in src/a.rs is resolved."
        );
        let all = serialise(&record);
        for leaked in [
            "permissions instructions",
            "environment_context",
            "AGENTS.md",
            "Always run the tests",
            "encrypted",
            "turn_aborted",
        ] {
            assert!(!all.contains(leaked), "{leaked} leaked: {all}");
        }
        assert_eq!(record.totals.input_tokens, 1500);
    }

    /// Secrets inside tool output are redacted on this machine, per value,
    /// and the body stays valid JSON — the regression where redacting the
    /// serialised text let an email match swallow the `n` of a `\n` escape.
    #[test]
    fn secrets_in_tool_output_are_redacted_and_the_body_still_parses() {
        let aws = ["AKIA", "IOSFODNN7"].concat();
        for (text, agent, secret) in [
            (CLAUDE_FULL, Agent::ClaudeCode, "sk-ant-api03-fake"),
            (CODEX_FULL, Agent::Codex, aws.as_str()),
        ] {
            let (body, findings) = body_of(text, agent, Detail::Full);
            assert!(findings >= 1, "{body}");
            assert!(!body.contains(secret), "{body}");
            assert!(!body.contains("/home/alice"), "{body}");
            let parsed: Value = serde_json::from_str(&body).expect("the body parses");
            assert_eq!(parsed["schema_version"], 3);
            assert_eq!(parsed["detail"], "full");
        }
        let (body, _) = body_of(CLAUDE_FULL, Agent::ClaudeCode, Detail::Full);
        let parsed: Value = serde_json::from_str(&body).expect("the body parses");
        let output = parsed["turns"][4]["tool_calls"][0]["output"]
            .as_str()
            .expect("the .env output");
        // The newline before the address survives as a newline.
        assert!(output.contains("contact\n[REDACTED:"), "{output}");
        assert!(!output.contains("bob@example.com"), "{output}");
    }

    /// The escape regression on its own: redacting the serialised text
    /// breaks the JSON; redacting the value first does not.
    #[test]
    fn redacting_values_never_breaks_an_escape() {
        let mut record = super::empty_record(Agent::Codex, 2);
        record.session_id = "s".to_owned();
        record.turns.push(super::Turn::new(
            Role::User,
            "",
            "write to\nbob@example.com\tor\r\"carol@example.org\"",
        ));
        // The old way — the serialised text redacted as one string — leaves
        // a body that no longer parses.
        let (whole, _) =
            livingbrain_redact::redact(&serialise(&record), livingbrain_redact::Policy::Redact)
                .expect("redacts");
        assert!(serde_json::from_str::<Value>(&whole).is_err(), "{whole}");
        let (body, findings) = prepare(&mut record, Detail::Full, &None, true, Path::new("x"))
            .expect("prepares")
            .expect("a body");
        assert_eq!(findings, 2, "{body}");
        let parsed: Value = serde_json::from_str(&body).expect("the body parses");
        let text = parsed["turns"][0]["text"].as_str().expect("text");
        assert!(text.starts_with("write to\n[REDACTED:"), "{text}");
        assert!(!text.contains("example."), "{text}");
    }

    /// `messages` keeps every message and only tool names; `summary` cuts
    /// the text too; both drop reasoning.
    #[test]
    fn detail_levels_decide_what_a_record_carries() {
        let (full, _) = body_of(CLAUDE_FULL, Agent::ClaudeCode, Detail::Full);
        assert!(full.contains("cargo test -p demo"));
        assert!(full.contains("\"output\""));
        assert!(full.contains("\"reasoning\""));

        let (messages, _) = body_of(CLAUDE_FULL, Agent::ClaudeCode, Detail::Messages);
        let parsed: Value = serde_json::from_str(&messages).expect("parses");
        assert_eq!(parsed["detail"], "messages");
        assert!(!messages.contains("cargo test -p demo"), "{messages}");
        assert!(!messages.contains("\"output\""), "{messages}");
        assert!(!messages.contains("\"reasoning\""), "{messages}");
        assert!(
            !messages.contains("[REDACTED:"),
            "nothing to redact: {messages}"
        );
        assert_eq!(
            parsed["turns"][1]["tool_calls"][0],
            serde_json::json!({"name": "Bash"})
        );

        let mut long = super::empty_record(Agent::Codex, 2);
        long.turns
            .push(super::Turn::new(Role::User, "", &"z".repeat(5000)));
        apply_detail(&mut long, Detail::Summary);
        assert_eq!(long.turns[0].text.chars().count(), SUMMARY_TURN_CHARS);
        assert_eq!(long.detail, Detail::Summary);
    }

    /// A replacement that swallowed half an escape is repaired into text
    /// that parses, and valid escapes are left alone.
    #[test]
    fn broken_escapes_are_repaired() {
        let broken = r#"{"t":"line\[REDACTED:email] \n \\ \u00e9 \""}"#;
        assert!(serde_json::from_str::<Value>(broken).is_err());
        let repaired = super::repair_escapes(broken);
        let parsed: Value = serde_json::from_str(&repaired).expect("repaired parses");
        assert_eq!(parsed["t"], "line\\[REDACTED:email] \n \\ é \"");
    }

    /// One session, one byte string, however often it is parsed — the whole
    /// of the ledger's re-sync idempotency.
    #[test]
    fn a_session_serialises_to_the_same_bytes_every_time() {
        for (text, agent) in [
            (CLAUDE_SESSION, Agent::ClaudeCode),
            (CODEX_SESSION, Agent::Codex),
            (CLAUDE_FULL, Agent::ClaudeCode),
            (CODEX_FULL, Agent::Codex),
        ] {
            let first = body_of(text, agent, Detail::Full).0;
            let second = body_of(text, agent, Detail::Full).0;
            assert_eq!(first, second);
            assert!(
                first.starts_with("{\"schema_version\":3,"),
                "schema_version must lead the body: {first}"
            );
        }
    }

    /// A session too big for the wire keeps its opening turn and its most
    /// recent stretch, marks the cut with an event, and says how much went.
    #[test]
    fn an_oversized_session_keeps_its_start_and_its_end() {
        let filler = "y".repeat(4000);
        let mut text = String::new();
        for index in 0..400 {
            text.push_str(&format!(
                "{{\"type\":\"user\",\"sessionId\":\"s\",\"cwd\":\"/opt/work\",\"timestamp\":\"2026-10-01T09:00:{:02}.000Z\",\"message\":{{\"role\":\"user\",\"content\":\"turn {index} {filler}\"}}}}\n",
                index % 60
            ));
        }
        let mut record = collect(&text, Agent::ClaudeCode).expect("a session");
        let turns_before = record.turns.len();
        cap(&mut record);
        assert!(record.truncated);
        let body = serialise(&record);
        assert!(
            body.len() <= MAX_SESSION_BODY_BYTES,
            "the body is still over the cap"
        );
        assert!(escaped_len(&body) <= MAX_ESCAPED_BODY_BYTES);
        assert!(
            record.turns[0].text.starts_with("turn 0 "),
            "the opening stays"
        );
        assert_eq!(record.turns[1].role, Role::Event);
        assert!(record.turns[1].text.contains("omitted"));
        assert!(
            record.turns.last().unwrap().text.starts_with("turn 399 "),
            "the end stays"
        );
        assert_eq!(
            record.omitted_turns + record.turns.len() - 1,
            turns_before,
            "every turn is kept or counted"
        );
    }

    /// A quote-heavy session is bounded by its escaped size too: what
    /// `GET /v1/sources/{id}` answers it inside.
    #[test]
    fn the_escaped_size_is_bounded_too() {
        let quotes = "\\\"".repeat(2000);
        let mut text = String::new();
        for index in 0..300 {
            text.push_str(&format!(
                "{{\"type\":\"user\",\"sessionId\":\"s\",\"cwd\":\"/opt/work\",\"message\":{{\"role\":\"user\",\"content\":\"{index} {quotes}\"}}}}\n"
            ));
        }
        let mut record = collect(&text, Agent::ClaudeCode).expect("a session");
        cap(&mut record);
        let body = serialise(&record);
        assert!(escaped_len(&body) <= MAX_ESCAPED_BODY_BYTES);
        assert!(record.truncated);
    }

    /// Home elision rewrites the real prefix and the dash-encoded project-dir
    /// form of directories under `$HOME` — in tool inputs, paths and outputs
    /// as well as messages; with no home known, nothing is rewritten.
    #[test]
    fn home_elision_rewrites_the_real_and_the_encoded_forms() {
        let mut record = collect(CLAUDE_SESSION, Agent::ClaudeCode).expect("a session");
        elide_home(&mut record, &Some("/home/alice".into()));
        assert_eq!(record.repo, "~/code/demo");
        let prompt = &record.turns[0].text;
        assert!(!prompt.contains("/home/alice"), "{prompt}");
        assert!(!prompt.contains("-home-alice"), "{prompt}");
        assert!(prompt.contains("folder ~ under"), "{prompt}");

        let mut full = collect(CLAUDE_FULL, Agent::ClaudeCode).expect("a session");
        elide_home(&mut full, &Some("/home/alice".into()));
        let edit = &full.turns[3].tool_calls[0];
        assert_eq!(edit.path.as_deref(), Some("~/code/demo/src/lib.rs"));
        assert!(!serialise(&full).contains("/home/alice"));

        let mut untouched = collect(CLAUDE_SESSION, Agent::ClaudeCode).expect("a session");
        elide_home(&mut untouched, &None);
        assert_eq!(untouched.repo, "/home/alice/code/demo");
        assert!(untouched.turns[0].text.contains("-home-alice-code-demo"));
    }

    /// Prefix matching is component-wise: a shared byte prefix is not inside.
    #[test]
    fn a_repo_inside_an_allowed_path_counts() {
        assert!(is_inside("/home/alice/code/demo", "/home/alice/code/demo"));
        assert!(is_inside(
            "/home/alice/code/demo/sub/dir",
            "/home/alice/code/demo"
        ));
        assert!(!is_inside("/home/alice/code-demo", "/home/alice/code"));
        assert!(!is_inside("/home/alice/code", "/home/alice/code/demo"));
        assert!(!is_inside("/home/alan/x", "/home/alice"));
    }

    /// The opt-in file round-trips with its levels; a file without levels
    /// (every file before them) reads as `full`; a corrupt file, a future
    /// version and an unknown level are refused, not read as empty.
    #[test]
    fn the_allowlist_body_round_trips() {
        let mut file = AllowlistFile {
            version: super::ALLOWLIST_VERSION,
            ..AllowlistFile::default()
        };
        file.allowed.insert("/home/alice/code/demo".to_owned());
        file.allowed.insert("/opt/work".to_owned());
        file.detail.insert("/opt/work".to_owned(), Detail::Messages);
        let body = serde_json::to_vec_pretty(&file).expect("serialises");
        let parsed = parse_allowlist(&body).expect("parses");
        assert_eq!(parsed.allowed, file.allowed);
        assert_eq!(parsed.level("/opt/work"), Detail::Messages);
        assert_eq!(parsed.level("/home/alice/code/demo"), Detail::Full);

        let old = parse_allowlist(br#"{"version":1,"allowed":["/opt/work"]}"#).expect("parses");
        assert_eq!(old.level("/opt/work"), Detail::Full);

        assert!(parse_allowlist(b"{not json").is_err());
        assert!(parse_allowlist(b"{}").is_err(), "no version, no allowed");
        let error = parse_allowlist(br#"{"version":99,"allowed":[]}"#)
            .expect_err("an unknown version is refused");
        assert!(error.contains("version 99 is not 1 or 2"), "{error}");
        assert!(
            parse_allowlist(br#"{"version":1,"allowed":["/x"],"detail":{"/x":"everything"}}"#)
                .is_err(),
            "an unknown level is refused"
        );
    }

    /// The opt-in file written before routing reads back with every root on
    /// the default workspace, and no routes; a routed file round-trips with
    /// its workspaces and routes, and an empty one writes neither field.
    #[test]
    fn the_workspace_and_routes_are_optional_and_round_trip() {
        let old = parse_allowlist(br#"{"version":1,"allowed":["/opt/work"]}"#).expect("parses");
        assert!(old.workspace.is_empty() && old.routes.is_empty());
        assert_eq!(old.explicit_workspace("/opt/work"), None);

        let mut file = AllowlistFile {
            version: super::ROUTING_VERSION,
            ..AllowlistFile::default()
        };
        let bare = serde_json::to_string(&file).expect("serialises");
        assert!(
            !bare.contains("workspace") && !bare.contains("routes"),
            "{bare}"
        );

        file.allowed.insert("/opt/work".to_owned());
        file.workspace
            .insert("/opt/work".to_owned(), "ws-chi".to_owned());
        file.routes = vec![super::Route {
            remote_owner: Some("CHI-Ecosystem".to_owned()),
            folder_glob: None,
            host: None,
            workspace: "ws-chi".to_owned(),
        }];
        let body = serde_json::to_vec_pretty(&file).expect("serialises");
        let parsed = parse_allowlist(&body).expect("parses");
        assert_eq!(parsed.explicit_workspace("/opt/work"), Some("ws-chi"));
        assert_eq!(parsed.routes, file.routes);
    }

    /// A misspelled route key is refused, not ignored: ignored, the route
    /// would send its repositories to the default workspace without a word.
    #[test]
    fn a_malformed_route_refuses_the_whole_file() {
        let typo =
            br#"{"version":1,"allowed":[],"routes":[{"folder-glob":"chi-*","workspace":"ws"}]}"#;
        assert!(parse_allowlist(typo).is_err());
        let bad_target = br#"{"version":1,"allowed":["/x"],"workspace":{"/x":"not valid!"}}"#;
        assert!(parse_allowlist(bad_target).is_err());
        let no_matcher = br#"{"version":1,"allowed":[],"routes":[{"workspace":"ws"}]}"#;
        assert!(parse_allowlist(no_matcher).is_err());
    }

    /// The nearest explicit root decides before any route, an explicit
    /// `default` stops a route, and a route that cannot read its repository
    /// holds the session rather than defaulting it. Paths here are not
    /// checkouts, so the route facts are unknown.
    #[test]
    fn explicit_workspace_beats_routes_and_an_unknown_repo_holds() {
        use super::route::Destination;
        let mut file = AllowlistFile {
            version: super::ALLOWLIST_VERSION,
            ..AllowlistFile::default()
        };
        let demo = "/nonexistent-lb-routing/demo".to_owned();
        let pinned = "/nonexistent-lb-routing/pinned".to_owned();
        for root in [&demo, &pinned] {
            file.allowed.insert(root.clone());
        }
        file.workspace.insert(pinned.clone(), "default".to_owned());
        file.routes = vec![super::Route {
            remote_owner: None,
            folder_glob: Some("*".to_owned()),
            host: None,
            workspace: "ws-all".to_owned(),
        }];
        let mut repos = super::Repos::default();
        let record_in = |cwd: &str| {
            let mut record = super::empty_record(Agent::Codex, 2);
            record.cwds = vec![cwd.to_owned()];
            record
        };
        assert!(
            matches!(
                super::destination_for(&record_in(&demo), &file, &mut repos),
                Destination::Unroutable(_)
            ),
            "a route with no readable repository holds, never defaults"
        );
        assert_eq!(
            super::destination_for(&record_in(&pinned), &file, &mut repos),
            Destination::Default,
            "an explicit default is not overridden by a route"
        );
        file.workspace.insert(demo.clone(), "ws-chi".to_owned());
        assert_eq!(
            super::destination_for(&record_in(&demo), &file, &mut repos),
            Destination::Workspace("ws-chi".to_owned()),
            "an explicit workspace beats a route"
        );
        let mut both = record_in(&demo);
        both.cwds.push(pinned.clone());
        assert_eq!(
            super::destination_for(&both, &file, &mut repos),
            Destination::Mixed,
            "two directories resolving to different workspaces make the session mixed"
        );
    }

    /// A nested checkout with no explicit workspace of its own takes the
    /// nearest enclosing root's explicit choice, before any route is consulted.
    #[test]
    fn a_nested_root_inherits_its_parents_workspace() {
        let mut file = AllowlistFile {
            version: super::ALLOWLIST_VERSION,
            ..AllowlistFile::default()
        };
        file.allowed.insert("/opt/work".to_owned());
        file.allowed.insert("/opt/work/team/app".to_owned());
        file.workspace
            .insert("/opt/work".to_owned(), "ws-parent".to_owned());
        file.routes = vec![super::Route {
            remote_owner: None,
            folder_glob: Some("*".to_owned()),
            host: None,
            workspace: "ws-route".to_owned(),
        }];
        assert_eq!(
            super::nearest_explicit(&file, "/opt/work/team/app/src").map(|(_, t)| t),
            Some("ws-parent")
        );
        let mut repos = super::Repos::default();
        let mut record = super::empty_record(Agent::Codex, 2);
        record.cwds = vec!["/opt/work/team/app/src".to_owned()];
        assert_eq!(
            super::destination_for(&record, &file, &mut repos),
            super::route::Destination::Workspace("ws-parent".to_owned()),
        );
    }

    /// Path containment is judged on the cleaned path: `..` cannot make a
    /// directory look inside a root it is not inside.
    #[test]
    fn dot_dot_cannot_escape_or_fake_containment() {
        assert!(!super::is_inside(
            "/home/alice/code/demo/../../etc",
            "/home/alice/code/demo"
        ));
        assert!(super::is_inside(
            "/home/alice/code/../code/demo/x",
            "/home/alice/code/demo"
        ));
        assert!(!super::is_inside(
            "/home/alice/code/demo/../other",
            "/home/alice/code/demo"
        ));
    }

    /// Version 1 while a file has no routing; version 2 as soon as it has a
    /// workspace or a route. A version-2 file with an unknown key is refused,
    /// and so is a version-1 file with one: a misspelt key is never a default.
    #[test]
    fn routing_writes_version_two_and_unknown_keys_are_refused() {
        let plain = AllowlistFile {
            version: super::ALLOWLIST_VERSION,
            allowed: ["/opt/work".to_owned()].into_iter().collect(),
            ..AllowlistFile::default()
        };
        assert_eq!(super::version_for(&plain), 1);
        let mut routed = AllowlistFile {
            version: super::ALLOWLIST_VERSION,
            allowed: plain.allowed.clone(),
            ..AllowlistFile::default()
        };
        routed
            .workspace
            .insert("/opt/work".to_owned(), "ws-chi".to_owned());
        assert_eq!(super::version_for(&routed), 2);

        let v2 = br#"{"version":2,"allowed":["/x"],"route":[]}"#;
        let error = parse_allowlist(v2).expect_err("unknown key in v2 is refused");
        assert!(error.contains("unknown field `route`"), "{error}");
        assert!(parse_allowlist(br#"{"version":1,"allowed":[],"routes":[]}"#).is_ok());
        assert!(parse_allowlist(br#"{"version":1,"allowed":[],"route":[]}"#).is_err());
        let future = parse_allowlist(br#"{"version":3,"allowed":[]}"#).expect_err("future version");
        assert!(future.contains("version 3 is not 1 or 2"), "{future}");
    }

    /// The reproduced leak: two checkouts under one allowed parent, with
    /// different owners. Each session goes where its own remote says, in
    /// either order of arrival.
    #[test]
    fn two_repositories_under_one_root_are_each_judged_on_their_own_remote() {
        let base = std::env::temp_dir().join(format!("lb-logs-{}", std::process::id()));
        let chi = base.join("chi-web");
        let notes = base.join("notes");
        let mut ok = true;
        for (dir, url) in [
            (&chi, "https://github.com/CHI-Ecosystem/chi-web.git"),
            (&notes, "https://github.com/someone/notes.git"),
        ] {
            std::fs::create_dir_all(dir).expect("dir");
            ok &= std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["init", "-q"])
                .status()
                .is_ok_and(|s| s.success());
            ok &= std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["remote", "add", "origin", url])
                .status()
                .is_ok_and(|s| s.success());
        }
        if !ok {
            let _ = std::fs::remove_dir_all(&base);
            return; // no git on this machine
        }
        let base_str = base.to_string_lossy().into_owned();
        let mut file = AllowlistFile {
            version: super::ROUTING_VERSION,
            ..AllowlistFile::default()
        };
        file.allowed.insert(base_str);
        file.routes = vec![super::Route {
            remote_owner: Some("CHI-Ecosystem".to_owned()),
            folder_glob: None,
            host: None,
            workspace: "ws-chi".to_owned(),
        }];
        let record = |cwd: &std::path::Path| {
            let mut record = super::empty_record(Agent::Codex, 2);
            record.cwds = vec![cwd.to_string_lossy().into_owned()];
            record
        };
        for order in [[&chi, &notes], [&notes, &chi]] {
            let mut repos = super::Repos::default();
            for dir in order {
                let got = super::destination_for(&record(dir), &file, &mut repos);
                let want = if dir == &chi {
                    super::route::Destination::Workspace("ws-chi".to_owned())
                } else {
                    super::route::Destination::Default
                };
                assert_eq!(got, want, "{}", dir.display());
            }
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The token plan: a routed workspace whose keychain item cannot be read
    /// holds only its own sessions, a workspace with no sign-in is held as
    /// such, and the default workspace's failure still fails the run.
    #[test]
    fn a_keychain_error_holds_only_its_workspace() {
        let mut groups = std::collections::BTreeMap::new();
        groups.insert("default".to_owned(), vec![0]);
        groups.insert("ws-a".to_owned(), vec![1]);
        groups.insert("ws-b".to_owned(), vec![2]);
        let plan = super::plan_tokens(groups.clone(), |workspace| match workspace {
            "ws-a" => Err(crate::err("denied")),
            "ws-b" => Ok(None),
            _ => Ok(Some(())),
        })
        .expect("the default workspace succeeded");
        let outcome = |name: &str| {
            let (_, lookup, _) = plan.iter().find(|(w, _, _)| w == name).expect("planned");
            match lookup {
                super::Lookup::Ready(_) => "ready",
                super::Lookup::NoToken => "no_token",
                super::Lookup::KeychainDenied => "denied",
            }
        };
        assert_eq!(outcome("default"), "ready");
        assert_eq!(outcome("ws-a"), "denied");
        assert_eq!(outcome("ws-b"), "no_token");

        let failed = super::plan_tokens(groups, |workspace| {
            if workspace == "default" {
                Err(crate::err("not logged in"))
            } else {
                Ok(Some(()))
            }
        });
        assert!(
            failed.is_err(),
            "the default workspace's failure is the run's"
        );
    }

    /// The most specific allowed root decides a directory's level, and the
    /// most restrictive directory decides the session's.
    #[test]
    fn the_most_specific_root_and_the_most_restrictive_cwd_win() {
        let mut file = AllowlistFile {
            version: super::ALLOWLIST_VERSION,
            ..AllowlistFile::default()
        };
        file.allowed.insert("/opt".to_owned());
        file.allowed.insert("/opt/work".to_owned());
        file.detail.insert("/opt".to_owned(), Detail::Summary);
        file.detail.insert("/opt/work".to_owned(), Detail::Full);
        let mut record = super::empty_record(Agent::Codex, 2);
        record.cwds = vec!["/opt/work/sub".to_owned()];
        assert_eq!(super::detail_for(&record, &file), Detail::Full);
        record.cwds.push("/opt/other".to_owned());
        assert_eq!(super::detail_for(&record, &file), Detail::Summary);
    }

    /// An empty record serialises with every field the schema promises, in
    /// order, optionals as nulls — the shape a version-2 reader relies on.
    #[test]
    fn the_schema_shape_is_stable() {
        let record = super::empty_record(Agent::ClaudeCode, 2);
        assert_eq!(
            serialise(&record),
            concat!(
                r#"{"schema_version":3,"agent":"claude_code","collector_version":2,"#,
                r#""detail":"full","session_id":"","repo":"","branch":null,"models":[],"#,
                r#""started_at":null,"ended_at":null,"turns":[],"totals":{"input_tokens":0,"#,
                r#""output_tokens":0,"cache_read_tokens":0,"cache_creation_tokens":0,"#,
                r#""reasoning_tokens":0,"cost_usd":null},"outcome":null,"truncated":false,"#,
                r#""omitted_turns":0,"omitted_subagent_turns":0}"#
            )
        );
    }

    #[test]
    fn text_truncation_is_on_a_char_boundary() {
        let mut accented = "é".repeat(50);
        truncate_text(&mut accented, 10);
        assert_eq!(accented.chars().count(), 10);
        let mut short = String::from("short");
        truncate_text(&mut short, 10);
        assert_eq!(short, "short");
    }

    /// The project a session uploads under is its repository's basename,
    /// cut to the server's project rule — and nothing at all when there is
    /// no repository to name.
    #[test]
    fn the_project_is_the_repository_basename_within_the_server_rule() {
        assert_eq!(project_of("~/code/demo").as_deref(), Some("demo"));
        assert_eq!(
            project_of("/home/alice/code/demo/").as_deref(),
            Some("demo")
        );
        assert_eq!(
            project_of(r"C:\Users\alice\livingbrain").as_deref(),
            Some("livingbrain")
        );
        assert_eq!(
            project_of("~/My Repo (old)").as_deref(),
            Some("My-Repo--old-")
        );
        assert_eq!(project_of("~/.dotfiles").as_deref(), Some("dotfiles"));
        assert_eq!(
            project_of(&format!("~/{}", "a".repeat(80))).map(|p| p.len()),
            Some(64)
        );
        assert_eq!(project_of(""), None);
        assert_eq!(project_of("~"), None);
        assert_eq!(project_of("~/.."), None);
        assert_eq!(project_of("/"), None);
    }

    /// The subagent fixtures: a Claude Code session whose background
    /// `Explore` agent launched a nested `general-purpose` one, and a Codex
    /// rollout whose `spawn_agent` call started a rollout of its own.
    const SUB_MAIN: &str = include_str!(
        "../tests/fixtures/logs-sub/claude/projects/-home-alice-code-demo/9a8b7c6d-1e2f-4a3b-8c9d-0e1f2a3b4c5d.jsonl"
    );
    const SUB_A1: &str = include_str!(
        "../tests/fixtures/logs-sub/claude/projects/-home-alice-code-demo/9a8b7c6d-1e2f-4a3b-8c9d-0e1f2a3b4c5d/subagents/agent-a1111.jsonl"
    );
    const SUB_A1_META: &str = include_str!(
        "../tests/fixtures/logs-sub/claude/projects/-home-alice-code-demo/9a8b7c6d-1e2f-4a3b-8c9d-0e1f2a3b4c5d/subagents/agent-a1111.meta.json"
    );
    const SUB_A2: &str = include_str!(
        "../tests/fixtures/logs-sub/claude/projects/-home-alice-code-demo/9a8b7c6d-1e2f-4a3b-8c9d-0e1f2a3b4c5d/subagents/agent-a2222.jsonl"
    );
    const SUB_A2_META: &str = include_str!(
        "../tests/fixtures/logs-sub/claude/projects/-home-alice-code-demo/9a8b7c6d-1e2f-4a3b-8c9d-0e1f2a3b4c5d/subagents/agent-a2222.meta.json"
    );
    const CODEX_PARENT: &str = include_str!(
        "../tests/fixtures/logs-sub/codex/sessions/2026/10/02/rollout-2026-10-02T11-00-00-01a00000-0000-7000-8000-000000000001.jsonl"
    );
    const CODEX_CHILD: &str = include_str!(
        "../tests/fixtures/logs-sub/codex/sessions/2026/10/02/rollout-2026-10-02T11-00-02-01a00000-0000-7000-8000-000000000002.jsonl"
    );

    /// The Claude subagent session, its two transcripts nested — children
    /// handed over in reverse, so the order of the files does not matter.
    fn claude_with_subagents() -> super::SessionRecord {
        let mut record = collect(&planted(SUB_MAIN), Agent::ClaudeCode).expect("a session");
        let child = |id: &str, text: &str, meta: &str| super::claude::Child {
            agent_id: id.to_owned(),
            meta: serde_json::from_str(meta).expect("meta parses"),
            text: planted(text),
        };
        super::claude::attach(
            &mut record,
            vec![
                child("a2222", SUB_A2, SUB_A2_META),
                child("a1111", SUB_A1, SUB_A1_META),
            ],
        );
        record
    }

    /// A subagent's transcript lands in the call that launched it, with its
    /// name, model, status (the last task notification's), totals and its
    /// own turns — and the subagent it launched lands inside it.
    #[test]
    fn claude_subagents_nest_in_the_calls_that_launched_them() {
        let mut record = claude_with_subagents();
        prepare(&mut record, Detail::Full, &None, false, Path::new("x")).expect("prepares");
        let call = &record.turns[1].tool_calls[0];
        assert_eq!(call.name, "Agent");
        assert_eq!(call.status, Some("ok"));
        // The launch receipt gives way to what the subagent said.
        assert_eq!(
            call.output.as_deref(),
            Some("Two loggers: src/a.rs and src/b.rs.")
        );
        let sub = call.subagent.as_deref().expect("the explorer");
        assert_eq!(sub.id, "a1111");
        assert_eq!(sub.name, "Explore");
        assert_eq!(sub.description.as_deref(), Some("Survey logging"));
        assert_eq!(sub.models, ["claude-haiku-4-5"]);
        assert_eq!(sub.status, Some("completed"));
        assert_eq!(sub.started_at.as_deref(), Some("2026-10-02T10:00:06.500Z"));
        assert_eq!(sub.ended_at.as_deref(), Some("2026-10-02T10:01:30.000Z"));
        assert_eq!(sub.totals.input_tokens, 180);
        assert_eq!(sub.totals.output_tokens, 45);
        assert_eq!(sub.turns.len(), 4);
        assert_eq!(sub.turns[1].model.as_deref(), Some("claude-haiku-4-5"));
        assert_eq!(sub.turns[2].model, None, "the same model is named once");
        let nested = sub.turns[2].tool_calls[0]
            .subagent
            .as_deref()
            .expect("the nested agent");
        assert_eq!(nested.name, "general-purpose");
        assert_eq!(nested.status, Some("completed"));
        assert_eq!(nested.final_answer(), Some("a.rs is fine."));
        // The main agent's model, named where it changes.
        assert_eq!(record.turns[1].model.as_deref(), Some("claude-opus-4-5"));
        assert_eq!(
            record.turns.last().unwrap().model.as_deref(),
            Some("claude-sonnet-4-5")
        );
        // The notification lines are machinery, not turns.
        assert!(!serialise(&record).contains("task-notification"));
        // The session totals are the main agent's own.
        assert_eq!(record.totals.input_tokens, 300);
    }

    /// A secret inside a subagent's tool output is redacted like one in the
    /// session's own, and the home prefix goes from a subagent's command.
    #[test]
    fn subagent_turns_are_redacted_and_home_elided() {
        let mut record = claude_with_subagents();
        let (body, findings) = prepare(
            &mut record,
            Detail::Full,
            &Some("/home/alice".into()),
            true,
            Path::new("x"),
        )
        .expect("prepares")
        .expect("a body");
        assert!(findings >= 1, "{body}");
        assert!(!body.contains(&["AKIA", "IOSFODNN7"].concat()), "{body}");
        assert!(!body.contains("/home/alice"), "{body}");
        let parsed: Value = serde_json::from_str(&body).expect("parses");
        let sub = &parsed["turns"][1]["tool_calls"][0]["subagent"];
        assert_eq!(
            sub["turns"][1]["tool_calls"][0]["input"],
            "rg -n logger ~/code/demo/src"
        );
        assert!(
            sub["turns"][1]["tool_calls"][0]["output"]
                .as_str()
                .unwrap()
                .contains("[REDACTED:")
        );
    }

    /// Detail levels reach inside: under `messages` a subagent keeps its
    /// header and messages, its calls their names and their own subagents.
    #[test]
    fn detail_levels_apply_inside_subagents() {
        let mut record = claude_with_subagents();
        let (body, _) = prepare(&mut record, Detail::Messages, &None, true, Path::new("x"))
            .expect("prepares")
            .expect("a body");
        let parsed: Value = serde_json::from_str(&body).expect("parses");
        let sub = &parsed["turns"][1]["tool_calls"][0]["subagent"];
        assert_eq!(sub["name"], "Explore");
        assert_eq!(
            sub["turns"][1]["tool_calls"][0],
            serde_json::json!({"name": "Bash"})
        );
        assert_eq!(
            sub["turns"][2]["tool_calls"][0]["subagent"]["turns"][1]["text"],
            "a.rs is fine."
        );
        assert!(!body.contains("rg -n logger"), "{body}");
    }

    /// Over the cap, subagents lose their inner turns first — oldest first,
    /// each keeping its header and final answer — and the main conversation
    /// keeps every turn while that is enough.
    #[test]
    fn the_size_cap_trims_subagents_first() {
        let mut record = claude_with_subagents();
        let sub = record.turns[1].tool_calls[0].subagent.as_mut().unwrap();
        let filler = "w".repeat(8000);
        let answer = sub.turns.pop().unwrap();
        for index in 0..150 {
            let mut turn = super::Turn::new(Role::Assistant, "", &format!("step {index}"));
            turn.tool_calls.push(super::ToolCall {
                name: "Bash".to_owned(),
                output: Some(filler.clone()),
                ..super::ToolCall::default()
            });
            sub.turns.push(turn);
        }
        sub.turns.push(answer);
        let main_turns = record.turns.len();
        cap(&mut record);
        let body = serialise(&record);
        assert!(body.len() <= MAX_SESSION_BODY_BYTES);
        assert!(escaped_len(&body) <= MAX_ESCAPED_BODY_BYTES);
        assert!(record.truncated);
        assert_eq!(record.omitted_turns, 0, "the main conversation is whole");
        assert_eq!(record.turns.len(), main_turns);
        let sub = record.turns[1].tool_calls[0].subagent.as_deref().unwrap();
        assert!(sub.truncated);
        assert!(sub.omitted_turns > 0);
        assert_eq!(record.omitted_subagent_turns, sub.omitted_turns);
        assert_eq!(sub.turns[0].role, Role::Event);
        assert!(sub.turns[0].text.contains("of this subagent omitted"));
        assert_eq!(
            sub.final_answer(),
            Some("Two loggers: src/a.rs and src/b.rs.")
        );
        assert_eq!(sub.name, "Explore", "the header stays");
        // The newest steps are what is left besides the answer.
        assert!(sub.turns[sub.turns.len() - 2].text.starts_with("step 149"));
    }

    /// A spawned Codex rollout nests in its `spawn_agent` call, matched by
    /// the path the call's result names; the call's result becomes what the
    /// agent said.
    #[test]
    fn codex_spawned_rollouts_nest_in_their_spawn_call() {
        let first = CODEX_CHILD.lines().next().unwrap();
        let spawn = super::codex::spawned(first).expect("a spawned rollout");
        assert_eq!(spawn.path, "/root/scan");
        assert_eq!(spawn.role, "explorer");
        assert!(super::codex::spawned(CODEX_PARENT.lines().next().unwrap()).is_none());
        let mut record = collect(CODEX_PARENT, Agent::Codex).expect("a session");
        let child = collect(CODEX_CHILD, Agent::Codex).expect("the child");
        super::codex::attach(&mut record, child, &spawn);
        prepare(&mut record, Detail::Full, &None, false, Path::new("x")).expect("prepares");
        let call = &record.turns[1].tool_calls[0];
        assert_eq!(call.name, "spawn_agent");
        assert_eq!(call.output.as_deref(), Some("One TODO, in src/a.rs."));
        let sub = call.subagent.as_deref().expect("the spawned agent");
        assert_eq!(sub.name, "explorer");
        assert_eq!(sub.description.as_deref(), Some("Gibbs: scan"));
        assert_eq!(sub.models, ["gpt-5.1-codex-mini"]);
        assert_eq!(sub.status, Some("completed"));
        assert_eq!(sub.totals.input_tokens, 300);
        assert_eq!(
            sub.turns[1].tool_calls[0].input.as_deref(),
            Some("rg -n TODO")
        );
        assert_eq!(record.turns[1].model.as_deref(), Some("gpt-5.1-codex"));
    }

    /// The first assistant turn that survives the cap names its model, even
    /// when the one that named it first was cut.
    #[test]
    fn the_model_is_named_after_the_cap() {
        let mut record = super::empty_record(Agent::ClaudeCode, 3);
        for (index, model) in ["a", "a", "b", "b", "a"].iter().enumerate() {
            let mut turn = super::Turn::new(Role::Assistant, "", &format!("t{index}"));
            turn.model = Some((*model).to_owned());
            record.turns.push(turn);
        }
        record.turns.remove(0);
        super::mark_model_changes(&mut record.turns);
        let models: Vec<Option<&str>> = record
            .turns
            .iter()
            .map(|turn| turn.model.as_deref())
            .collect();
        assert_eq!(models, [Some("a"), Some("b"), None, Some("a")]);
    }

    /// A version-1 file cannot name routes or workspaces: an older-format reader
    /// would drop them and send everything to the default, so the file is refused.
    #[test]
    fn version_one_refuses_routing_keys() {
        for body in [
            r#"{"version":1,"allowed":["/x"],"routes":[{"folder_glob":"a","workspace":"ws"}]}"#,
            r#"{"version":1,"allowed":["/x"],"workspace":{"/x":"ws"}}"#,
        ] {
            let error = parse_allowlist(body.as_bytes()).expect_err("refused");
            assert!(error.contains("needs version 2"), "{error}");
        }
        assert!(
            parse_allowlist(br#"{"version":1,"allowed":["/x"],"routes":[],"workspace":{}}"#)
                .is_ok()
        );
    }

    /// Two configured ids that normalise to one token variable are refused, and
    /// the check covers route targets as well as explicit choices.
    #[test]
    fn two_ids_that_share_a_variable_are_refused() {
        let mut file = AllowlistFile {
            version: super::ROUTING_VERSION,
            ..AllowlistFile::default()
        };
        file.workspace.insert("/a".to_owned(), "a-b".to_owned());
        file.workspace.insert("/b".to_owned(), "a_b".to_owned());
        let error = file.validate().expect_err("collision");
        assert!(error.contains("LIVINGBRAIN_TOKEN_A_B"), "{error}");

        let mut routed = AllowlistFile {
            version: super::ROUTING_VERSION,
            ..AllowlistFile::default()
        };
        routed
            .workspace
            .insert("/a".to_owned(), "ws-one".to_owned());
        routed.routes = vec![super::Route {
            remote_owner: Some("acme".to_owned()),
            folder_glob: None,
            host: None,
            workspace: "WS-ONE".to_owned(),
        }];
        assert!(routed.validate().is_err(), "case-only difference collides");
        routed.routes[0].workspace = "ws-two".to_owned();
        assert!(routed.validate().is_ok());
        routed
            .workspace
            .insert("/b".to_owned(), "default".to_owned());
        assert!(routed.validate().is_ok(), "`default` has no variable");
    }

    /// A nested checkout follows its parent by lookup, not by copy: changing the
    /// parent's workspace later changes where the nested root's sessions go.
    #[test]
    fn a_nested_root_follows_its_parent_when_the_parent_changes() {
        let mut file = AllowlistFile {
            version: super::ROUTING_VERSION,
            ..AllowlistFile::default()
        };
        file.allowed.insert("/opt/work".to_owned());
        file.allowed.insert("/opt/work/app".to_owned());
        file.workspace
            .insert("/opt/work".to_owned(), "ws-a".to_owned());
        let mut repos = super::Repos::default();
        let mut record = super::empty_record(Agent::Codex, 2);
        record.cwds = vec!["/opt/work/app/src".to_owned()];
        assert_eq!(
            super::destination_for(&record, &file, &mut repos),
            super::route::Destination::Workspace("ws-a".to_owned())
        );
        file.workspace
            .insert("/opt/work".to_owned(), "ws-b".to_owned());
        assert_eq!(
            super::destination_for(&record, &file, &mut repos),
            super::route::Destination::Workspace("ws-b".to_owned()),
            "the nested root moved with its parent"
        );
        assert!(
            file.explicit_workspace("/opt/work/app").is_none(),
            "nothing was copied"
        );
    }

    fn git_ok(dir: &Path, args: &[&str]) -> bool {
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .is_ok_and(|status| status.success())
    }

    /// The reproduced disagreement: a plain folder that holds a CHI checkout and
    /// a foreign one. Status must say the folder is per-repository and list each
    /// checkout's own answer; sync judges each session on its own checkout.
    #[test]
    fn a_plain_folder_is_per_repository_and_sync_routes_each_checkout() {
        let base = std::env::temp_dir().join(format!("lb-logs-folder-{}", std::process::id()));
        let chi = base.join("chi-web");
        let notes = base.join("team").join("notes");
        let mut ok = true;
        for (dir, url) in [
            (&chi, "https://github.com/CHI-Ecosystem/chi-web.git"),
            (&notes, "https://github.com/someone/notes.git"),
        ] {
            ok &= std::fs::create_dir_all(dir).is_ok() && git_ok(dir, &["init", "-q"]);
            ok &= git_ok(dir, &["remote", "add", "origin", url]);
        }
        if !ok {
            let _ = std::fs::remove_dir_all(&base);
            return; // no git on this machine
        }
        let mut file = AllowlistFile {
            version: super::ROUTING_VERSION,
            ..AllowlistFile::default()
        };
        file.allowed.insert(base.to_string_lossy().into_owned());
        file.routes = vec![super::Route {
            remote_owner: Some("CHI-Ecosystem".to_owned()),
            folder_glob: None,
            host: None,
            workspace: "ws-chi".to_owned(),
        }];
        let mut repos = super::Repos::default();
        let status = super::root_status(&base.to_string_lossy(), &file, &mut repos);
        assert_eq!(status.effective, "per-repository");
        assert_eq!(status.reason.as_deref(), Some("per-repository"));
        let judged: Vec<(String, String)> = status
            .nested
            .iter()
            .map(|(path, effective, _)| (path.clone(), effective.clone()))
            .collect();
        let chi_key = chi.to_string_lossy().into_owned();
        let notes_key = notes.to_string_lossy().into_owned();
        assert!(
            judged.contains(&(chi_key.clone(), "ws-chi".to_owned())),
            "{judged:?}"
        );
        assert!(
            judged.contains(&(notes_key.clone(), "default".to_owned())),
            "{judged:?}"
        );

        for (dir, want) in [
            (
                &chi,
                super::route::Destination::Workspace("ws-chi".to_owned()),
            ),
            (&notes, super::route::Destination::Default),
        ] {
            let mut record = super::empty_record(Agent::Codex, 2);
            record.cwds = vec![dir.to_string_lossy().into_owned()];
            assert_eq!(super::destination_for(&record, &file, &mut repos), want);
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A root that is itself a checkout, and holds a nested checkout, lists its
    /// own answer first: a held root shows as held in the folder's list.
    #[test]
    fn a_checkout_root_with_nested_checkouts_lists_its_own_answer() {
        let base = std::env::temp_dir().join(format!("lb-logs-root-{}", std::process::id()));
        let inner = base.join("inner");
        let mut ok = std::fs::create_dir_all(&inner).is_ok();
        for (dir, url) in [
            (&base, "https://gitlab.com/CHI-Ecosystem/root.git"),
            (&inner, "https://github.com/someone/inner.git"),
        ] {
            ok &= git_ok(dir, &["init", "-q"]) && git_ok(dir, &["remote", "add", "origin", url]);
        }
        if !ok {
            let _ = std::fs::remove_dir_all(&base);
            return; // no git on this machine
        }
        let mut file = AllowlistFile {
            version: super::ROUTING_VERSION,
            ..AllowlistFile::default()
        };
        file.allowed.insert(base.to_string_lossy().into_owned());
        file.routes = vec![super::Route {
            remote_owner: Some("CHI-Ecosystem".to_owned()),
            folder_glob: None,
            host: None,
            workspace: "ws-chi".to_owned(),
        }];
        let mut repos = super::Repos::default();
        let status = super::root_status(&base.to_string_lossy(), &file, &mut repos);
        let root_key = base.to_string_lossy().into_owned();
        assert_eq!(
            status.nested.first().map(|(p, _, _)| p.as_str()),
            Some(root_key.as_str())
        );
        let own = &status.nested[0];
        assert_eq!(own.1, "unroutable", "the held root shows as held");
        assert!(
            own.2.as_deref().unwrap_or("").contains("gitlab.com"),
            "{own:?}"
        );
        assert_eq!(status.nested.len(), 2, "{:?}", status.nested);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_default_expectation_is_the_app_id_or_the_file_id_and_a_disagreement_holds() {
        let file = |default: Option<&str>| super::AllowlistFile {
            version: 2,
            default_workspace: default.map(str::to_owned),
            ..Default::default()
        };
        assert_eq!(
            super::default_expectation(None, &file(None)),
            super::DefaultExpectation::Unset
        );
        assert_eq!(
            super::default_expectation(Some("ws-a".into()), &file(None)),
            super::DefaultExpectation::Workspace("ws-a".into())
        );
        assert_eq!(
            super::default_expectation(None, &file(Some("ws-a"))),
            super::DefaultExpectation::Workspace("ws-a".into())
        );
        assert_eq!(
            super::default_expectation(Some("ws-a".into()), &file(Some("ws-a"))),
            super::DefaultExpectation::Workspace("ws-a".into())
        );
        assert_eq!(
            super::default_expectation(Some("ws-a".into()), &file(Some("ws-b"))),
            super::DefaultExpectation::Conflict
        );
    }
}
