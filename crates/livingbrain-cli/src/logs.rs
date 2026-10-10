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
//! Two rules are absolute. **Only tool names travel** — a call's input and its
//! result are dropped at parse time, not redacted afterwards. **Redaction is
//! local and not optional** — the home prefix becomes `~`, and the serialised
//! body then goes through `livingbrain_redact`, the library the server runs on
//! arrival, so the bytes that cross the socket are the redacted ones. The body
//! is also the identity: a record serialises deterministically, so the
//! ledger's `(scope, sha256)` key makes a re-sync read rows back instead of
//! sealing copies.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use clap::Subcommand;
use ignore::WalkBuilder;
use livingbrain_redact::{Policy, redact};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::api::Client;
use crate::{CliResult, Out, err};

/// The version of the normalised session schema this build writes: adding a
/// field is a bump of this number, not an edit in place.
pub(crate) const LOG_SCHEMA_VERSION: u32 = 1;

/// The most text one turn keeps, in characters.
const MAX_TURN_TEXT_CHARS: usize = 2000;

/// The most tool calls one turn names; the body cap is what bounds a loop.
const MAX_TOOL_CALLS: usize = 128;

/// The largest body this module builds: three quarters of redaction's input
/// cap — the headroom `import` leaves for redaction's replacement tokens — and
/// under the ledger's own ceiling besides. A session over it loses its
/// trailing turns ([`cap`]) and says so.
const MAX_SESSION_BODY_BYTES: usize = livingbrain_redact::MAX_INPUT_BYTES * 3 / 4;

/// The `kind` agent sessions are filed under on the wire.
const SOURCE_KIND: &str = "agent_log";

/// The scope sessions land in: an agent log is the reader's own activity.
const SCOPE: &str = "personal";

/// The opt-in file, under [`config_dir`].
const ALLOWLIST_FILE: &str = "logs-allow.json";

/// The opt-in file's own shape version, so a future format is migrated rather
/// than guessed at.
const ALLOWLIST_VERSION: u32 = 1;

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

/// The `livingbrain logs` subcommands.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Allow syncing sessions whose log records a working directory inside this one
    Allow {
        /// The repository to allow (default: the current directory)
        path: Option<String>,
    },
    /// Revoke a path `logs allow` added
    Deny {
        /// The repository to forget (default: the current directory)
        path: Option<String>,
    },
    /// Print which repositories are opted in, and where that is recorded
    Status,
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
    },
}

/// Run one `logs` subcommand.
pub fn run(command: &Command, api_url: &str, out: &Out) -> CliResult<()> {
    match command {
        Command::Allow { path } => set_allowlist(path.as_deref(), true, out),
        Command::Deny { path } => set_allowlist(path.as_deref(), false, out),
        Command::Status => status(out),
        Command::Sync {
            claude_dir,
            codex_dir,
            dry_run,
        } => sync(
            claude_dir.as_deref(),
            codex_dir.as_deref(),
            *dry_run,
            api_url,
            out,
        ),
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
    turns: Vec<Turn>,
    totals: Totals,
    /// How the session ended, when its log says. No format this build reads
    /// records one; the field is here from version 1 so a collector that
    /// learns it does not move every field after it.
    outcome: Option<String>,
    truncated: bool,
}

/// One turn of the conversation. Tool inputs and tool results live only in
/// the fields the parsers never copy; there is no field here to hide them in.
#[derive(Debug, Serialize)]
struct Turn {
    role: String,
    at: Option<String>,
    /// Truncated to [`MAX_TURN_TEXT_CHARS`] characters.
    text: String,
    tool_calls: Vec<ToolCall>,
}

/// A tool the agent called, by name and nothing else.
#[derive(Debug, Serialize)]
struct ToolCall {
    name: String,
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

mod claude {
    use std::collections::{BTreeMap, BTreeSet};

    use serde_json::Value;

    use super::{
        Agent, SessionRecord, ToolCall, Turn, empty_record, note_cwd, note_model, note_once,
        note_time, num_field, present, str_field,
    };

    /// The Claude Code reader's version: bumped when reading the format
    /// changes in a way that changes the record it produces.
    pub(super) const COLLECTOR_VERSION: u32 = 1;

    /// Read one `~/.claude/projects/<project>/<session>.jsonl`. One assistant
    /// message streamed as several lines repeats the same `message.id`, the
    /// same usage and the same `costUSD`; all three are counted once per id.
    /// Lines this build does not know are skipped, never fatal — a live log
    /// is read while its writer may still be running.
    pub(super) fn collect(text: &str) -> Option<SessionRecord> {
        let mut record = empty_record(Agent::ClaudeCode, COLLECTOR_VERSION);
        // Message ids whose usage has been counted, and the turn each id's
        // blocks belong to.
        let mut counted: BTreeSet<String> = BTreeSet::new();
        let mut opened: BTreeMap<String, usize> = BTreeMap::new();

        for line in text.lines() {
            let Ok(line) = serde_json::from_str::<Value>(line) else {
                continue; // a torn write in a live log is not a failed sync
            };
            let role = match str_field(&line, "type") {
                "user" => "user",
                "assistant" => "assistant",
                _ => continue,
            };
            note_once(&mut record.session_id, str_field(&line, "sessionId"));
            note_cwd(&mut record, str_field(&line, "cwd"));
            if record.branch.is_none() {
                record.branch = match str_field(&line, "gitBranch") {
                    "" => None,
                    branch => Some(branch.to_owned()),
                };
            }
            let at = str_field(&line, "timestamp").to_owned();
            note_time(&mut record, &at);
            let Some(message) = line.get("message") else {
                continue;
            };
            note_model(&mut record, str_field(message, "model"));
            let message_id = str_field(message, "id").to_owned();

            match role {
                "user" => {
                    let text = user_text(message);
                    // A user line whose only content is a tool result is the
                    // tool's output coming back — no turn, by design.
                    if !text.is_empty() {
                        record.turns.push(Turn {
                            role: "user".to_owned(),
                            at: present(&at),
                            text,
                            tool_calls: Vec::new(),
                        });
                    }
                }
                "assistant" => {
                    let mut text_parts: Vec<String> = Vec::new();
                    let mut tool_names: Vec<String> = Vec::new();
                    for block in message
                        .get("content")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        match str_field(block, "type") {
                            "text" if !str_field(block, "text").is_empty() => {
                                text_parts.push(str_field(block, "text").to_owned());
                            }
                            // The input object stays on the machine; the name
                            // is all that travels.
                            "tool_use" if !str_field(block, "name").is_empty() => {
                                tool_names.push(str_field(block, "name").to_owned());
                            }
                            // `thinking` blocks and anything newer: dropped.
                            _ => {}
                        }
                    }
                    // One message streamed as several lines is one turn; the
                    // id says which. An id-less line is a turn of its own.
                    let index = match opened.get(&message_id) {
                        Some(index) => *index,
                        None => {
                            record.turns.push(Turn {
                                role: "assistant".to_owned(),
                                at: present(&at),
                                text: String::new(),
                                tool_calls: Vec::new(),
                            });
                            let index = record.turns.len() - 1;
                            if !message_id.is_empty() {
                                opened.insert(message_id.clone(), index);
                            }
                            index
                        }
                    };
                    {
                        let turn = &mut record.turns[index];
                        for part in text_parts {
                            if !turn.text.is_empty() {
                                turn.text.push('\n');
                            }
                            turn.text.push_str(&part);
                        }
                        for name in tool_names {
                            if turn.tool_calls.len() >= super::MAX_TOOL_CALLS {
                                break;
                            }
                            turn.tool_calls.push(ToolCall { name });
                        }
                    }
                    // Usage and cost are per request, and the request is the
                    // message: count them on the line that first carries the
                    // id. A line with no id cannot be deduplicated and is
                    // taken at its word.
                    let first_of_its_message = message_id.is_empty() || counted.insert(message_id);
                    if first_of_its_message {
                        if let Some(usage) = message.get("usage") {
                            record.totals.input_tokens += num_field(usage, "input_tokens");
                            record.totals.output_tokens += num_field(usage, "output_tokens");
                            record.totals.cache_read_tokens +=
                                num_field(usage, "cache_read_input_tokens");
                            record.totals.cache_creation_tokens +=
                                num_field(usage, "cache_creation_input_tokens");
                        }
                        if let Some(cost) = line.get("costUSD").and_then(Value::as_f64) {
                            record.totals.cost_usd =
                                Some(record.totals.cost_usd.unwrap_or(0.0) + cost);
                        }
                    }
                }
                _ => unreachable!("the match above only yields user and assistant"),
            }
        }
        (!record.session_id.is_empty()).then_some(record)
    }

    /// The text of a user line: a bare string, or the `text` blocks of an
    /// array. A `tool_result` block is dropped here, on purpose, before it
    /// can reach any body.
    fn user_text(message: &Value) -> String {
        match message.get("content") {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Array(blocks)) => blocks
                .iter()
                .filter(|block| str_field(block, "type") == "text")
                .map(|block| str_field(block, "text"))
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        }
    }
}

mod codex {
    use serde_json::Value;

    use super::{
        Agent, SessionRecord, ToolCall, Totals, Turn, empty_record, note_cwd, note_model,
        note_once, note_time, num_field, present, str_field,
    };

    /// The Codex reader's version: bumped when reading the format changes in
    /// a way that changes the record it produces.
    pub(super) const COLLECTOR_VERSION: u32 = 1;

    /// Read one `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` of
    /// `{timestamp, type, payload}` lines. `token_count` events carry
    /// cumulative `total_token_usage`, so the **last** one is the session's
    /// totals — summing would multiply every turn in. There is no
    /// cache-creation figure and no cost in this format; none is invented.
    pub(super) fn collect(text: &str) -> Option<SessionRecord> {
        let mut record = empty_record(Agent::Codex, COLLECTOR_VERSION);
        for line in text.lines() {
            let Ok(line) = serde_json::from_str::<Value>(line) else {
                continue; // a torn write in a live log is not a failed sync
            };
            let Some(payload) = line.get("payload") else {
                continue;
            };
            let at = str_field(&line, "timestamp").to_owned();
            match str_field(&line, "type") {
                "session_meta" => {
                    note_time(&mut record, &at);
                    note_once(&mut record.session_id, str_field(payload, "id"));
                    note_cwd(&mut record, str_field(payload, "cwd"));
                    if record.branch.is_none() {
                        record.branch = payload
                            .get("git")
                            .and_then(|git| git.get("branch"))
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                    }
                }
                "turn_context" => {
                    note_time(&mut record, &at);
                    note_cwd(&mut record, str_field(payload, "cwd"));
                    note_model(&mut record, str_field(payload, "model"));
                }
                "response_item" => response_item(&mut record, payload, &at),
                "event_msg" if str_field(payload, "type") == "token_count" => {
                    if let Some(usage) = payload
                        .get("info")
                        .and_then(|info| info.get("total_token_usage"))
                    {
                        note_time(&mut record, &at);
                        // Cumulative, so replace rather than add: the last
                        // count is the session's.
                        record.totals = Totals {
                            input_tokens: num_field(usage, "input_tokens"),
                            output_tokens: num_field(usage, "output_tokens"),
                            cache_read_tokens: num_field(usage, "cached_input_tokens"),
                            cache_creation_tokens: 0,
                            reasoning_tokens: num_field(usage, "reasoning_output_tokens"),
                            cost_usd: None,
                        };
                    }
                }
                // Ghost traces, compactions, whatever else a newer Codex
                // writes: skipped, not fatal.
                _ => {}
            }
        }
        (!record.session_id.is_empty()).then_some(record)
    }

    /// One `response_item`: a message becomes a turn; a tool call names
    /// itself and nothing else; outputs and reasoning are dropped where the
    /// parser finds them. Only a payload that produced something moves the
    /// session's clock.
    fn response_item(record: &mut SessionRecord, item: &Value, at: &str) {
        match str_field(item, "type") {
            "message" => {
                let role = match str_field(item, "role") {
                    "assistant" => "assistant",
                    _ => "user",
                };
                let text = item
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|blocks| {
                        blocks
                            .iter()
                            .filter(|block| {
                                matches!(str_field(block, "type"), "input_text" | "output_text")
                            })
                            .map(|block| str_field(block, "text"))
                            .filter(|text| !text.is_empty())
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                // Every session opens with wrapped bootstrap context — the
                // project's instructions, the environment summary. Boilerplate,
                // not conversation: skipped.
                if text.starts_with("<user_instructions>")
                    || text.starts_with("<environment_context>")
                {
                    return;
                }
                if !text.is_empty() {
                    note_time(record, at);
                    record.turns.push(Turn {
                        role: role.to_owned(),
                        at: present(at),
                        text,
                        tool_calls: Vec::new(),
                    });
                }
            }
            // A call attaches to a trailing tool-only turn — several parallel
            // calls are one action — or starts one. The last message turn is
            // not its author: the call belongs to the action after it.
            "function_call" | "custom_tool_call" => {
                let name = str_field(item, "name");
                if name.is_empty() {
                    return;
                }
                note_time(record, at);
                match record.turns.last_mut() {
                    Some(turn)
                        if turn.role == "assistant"
                            && turn.text.is_empty()
                            && turn.tool_calls.len() < super::MAX_TOOL_CALLS =>
                    {
                        turn.tool_calls.push(ToolCall {
                            name: name.to_owned(),
                        });
                    }
                    _ => record.turns.push(Turn {
                        role: "assistant".to_owned(),
                        at: present(at),
                        text: String::new(),
                        tool_calls: vec![ToolCall {
                            name: name.to_owned(),
                        }],
                    }),
                }
            }
            // `function_call_output`, `custom_tool_call_output`, `reasoning`
            // and anything else: the tool's output stays on the machine.
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Bounding and redacting the body

/// The record as bytes, ready for redaction. Fields go out in declaration
/// order and nothing in a record is read from the clock or the environment,
/// so one session is one byte string wherever it is parsed — the property the
/// ledger's `(scope, sha256)` idempotency rests on.
fn serialise(record: &SessionRecord) -> String {
    serde_json::to_string(record).expect("a session record always serialises")
}

/// Bound a record for the wire: every turn's text cut to
/// [`MAX_TURN_TEXT_CHARS`] first, then whole trailing turns off until the
/// body fits [`MAX_SESSION_BODY_BYTES`] — measured once, subtracted
/// arithmetically, not one serialisation per pop. The tail goes, the head
/// stays, and a record that lost anything says so: `truncated` travels with
/// it.
fn cap(record: &mut SessionRecord) {
    for turn in &mut record.turns {
        truncate_text(&mut turn.text, MAX_TURN_TEXT_CHARS);
    }
    let full = serialise(record);
    if full.len() <= MAX_SESSION_BODY_BYTES {
        return;
    }
    // Removing a trailing turn takes its own JSON plus the comma before it.
    let mut excess = (full.len() - MAX_SESSION_BODY_BYTES) as isize;
    while excess > 0 {
        let Some(turn) = record.turns.pop() else {
            break; // metadata alone is over the cap; nothing more to shed
        };
        excess -= turn_bytes(&turn) as isize + 1;
        record.truncated = true;
    }
}

/// The bytes one turn occupies in the serialised body.
fn turn_bytes(turn: &Turn) -> usize {
    serde_json::to_string(turn)
        .expect("a turn always serialises")
        .len()
}

/// `text` cut to at most `max` characters, on a char boundary.
fn truncate_text(text: &mut String, max: usize) {
    if let Some((cut, _)) = text.char_indices().nth(max) {
        text.truncate(cut);
    }
}

/// Rewrite the places this machine could leak from a record, before redaction
/// runs: the home prefix becomes `~`, in the repo and every turn's text, and
/// so does Claude Code's dash-encoded project-dir form of any path **under
/// `$HOME`** — that encoding shows up precisely in text that never wrote the
/// home out. Encoded forms of directories outside `$HOME` are prose as far as
/// the reader is concerned, and no home means nothing to rewrite.
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
    for text in
        std::iter::once(&mut record.repo).chain(record.turns.iter_mut().map(|turn| &mut turn.text))
    {
        for needle in &needles {
            if text.contains(needle.as_str()) {
                *text = text.replace(needle.as_str(), "~");
            }
        }
    }
}

/// Redact the serialised body, on this machine: what redaction produces is
/// what the server is sent.
fn build_body(record: &SessionRecord, file: &Path) -> CliResult<(String, usize)> {
    let body = serialise(record);
    match redact(&body, Policy::Redact) {
        Ok((clean, findings)) => Ok((clean, findings.len())),
        Err(error) => Err(err(format!("{}: {error}", file.display()))),
    }
}

// ---------------------------------------------------------------------------
// The opt-in file

/// The opt-in file's shape on disk; the allowed paths as a set, so the file
/// reads back the same however many times `logs allow` ran.
#[derive(Debug, Serialize, Deserialize)]
struct AllowlistFile {
    version: u32,
    allowed: BTreeSet<String>,
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
fn read_allowlist() -> CliResult<BTreeSet<String>> {
    let path = allowlist_path()?;
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(error) => return Err(err(format!("could not read {}: {error}", path.display()))),
    };
    parse_allowlist(&bytes)
        .map_err(|error| err(format!("{}: {error}", path.display())))
        .map(|file| file.allowed)
}

/// Parse an allowlist file's bytes, refusing a version this build does not
/// read: guessing at a future format could read a list its writer meant
/// differently.
fn parse_allowlist(bytes: &[u8]) -> Result<AllowlistFile, String> {
    let file: AllowlistFile = serde_json::from_slice(bytes)
        .map_err(|error| format!("not a valid opt-in file ({error}); fix or remove it"))?;
    if file.version != ALLOWLIST_VERSION {
        return Err(format!(
            "opt-in file version {} is not {} (this build's); upgrade the CLI or fix the file",
            file.version, ALLOWLIST_VERSION
        ));
    }
    Ok(file)
}

/// Write the allowlist back, creating the config directory if this is the
/// first thing the CLI has ever persisted there. Returns the path, for the
/// report.
fn write_allowlist(allowed: &BTreeSet<String>) -> CliResult<PathBuf> {
    let path = allowlist_path()?;
    let parent = path
        .parent()
        .ok_or_else(|| err("the config path has no parent"))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| err(format!("could not create {}: {error}", parent.display())))?;
    let body = serde_json::to_vec_pretty(&AllowlistFile {
        version: ALLOWLIST_VERSION,
        allowed: allowed.clone(),
    })
    .expect("an allowlist always serialises");
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
    child == ancestor
        || (child.len() > ancestor.len()
            && child.starts_with(ancestor)
            && child.as_bytes()[ancestor.len()] == b'/')
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

/// `logs allow` and `logs deny`: insert or remove one path, then print the
/// list. Both are idempotent, and both write the file even when nothing
/// changed, so the reader sees where the decision lives.
fn set_allowlist(path: Option<&str>, add: bool, out: &Out) -> CliResult<()> {
    let target = normalise_path(path.unwrap_or("."))?;
    let mut allowed = read_allowlist()?;
    let changed = if add {
        allowed.insert(target.clone())
    } else {
        allowed.remove(&target)
    };
    let file = write_allowlist(&allowed)?;
    let list: Vec<&String> = allowed.iter().collect();
    out.json_or(
        &json!({
            "allowed": list,
            "changed": changed,
            "config": file.display().to_string(),
        }),
        || {
            match (add, changed) {
                (true, true) => println!("Allowed {target}."),
                (true, false) => println!("Already allowed: {target}"),
                (false, true) => println!("No longer allowed: {target}"),
                (false, false) => println!("Was not opted in: {target}"),
            }
            println!("Opt-in file: {}", file.display());
        },
    );
    Ok(())
}

fn status(out: &Out) -> CliResult<()> {
    let allowed = read_allowlist()?;
    let file = allowlist_path()?;
    let list: Vec<&String> = allowed.iter().collect();
    out.json_or(
        &json!({
            "allowed": list,
            "config": file.display().to_string(),
        }),
        || {
            if list.is_empty() {
                println!("No repositories are opted in. Run `livingbrain logs allow` inside one.");
            } else {
                println!("Opted in ({}):", list.len());
                for path in &list {
                    println!("  {path}");
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
    opted_in: bool,
    body: Option<String>,
    /// Secrets redacted out of `body`.
    redacted: usize,
    /// The tool-call count, computed once for both reports.
    tool_calls: usize,
    uploaded: Option<String>,
    /// Whether the upload created the row (201) or read one back (200).
    created: bool,
}

/// `livingbrain logs sync`: everything before [`upload`] is local — walk,
/// parse, decide, elide, redact — so a machine with nothing opted in runs the
/// whole command without a single packet, the auth request included.
/// `--dry-run` stops after the report for the same reason.
fn sync(
    claude_dir: Option<&Path>,
    codex_dir: Option<&Path>,
    dry_run: bool,
    api_url: &str,
    out: &Out,
) -> CliResult<()> {
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
    let allowed = read_allowlist()?;
    let (mut sessions, unreadable, unparsed) =
        collect_sessions(&claude_dir, &codex_dir, &allowed, &home)?;
    upload(&mut sessions, dry_run, api_url)?;
    report(
        &sessions,
        unreadable,
        unparsed,
        dry_run,
        (&claude_dir, &codex_dir),
        out,
    );
    Ok(())
}

/// Walk both agents' directories, parse every session file, and decide the
/// opt-in — in that order: a session that is not opted in never produces
/// bytes. Returns the sessions plus two kinds of failure: `unreadable` files
/// could not be read at all, `unparsed` ones were read but yielded no session
/// (empty or foreign `.jsonl`).
fn collect_sessions(
    claude_dir: &Path,
    codex_dir: &Path,
    allowed: &BTreeSet<String>,
    home: &Option<PathBuf>,
) -> CliResult<(Vec<Prepared>, usize, usize)> {
    let mut sessions = Vec::new();
    let (mut unreadable, mut unparsed) = (0usize, 0usize);
    for (agent, dir, levels) in [
        // Claude Code lays sessions out one directory per project — walk into
        // exactly that one level; Codex nests them by date, so its walk goes
        // all the way down.
        (Agent::ClaudeCode, claude_dir, Some(1usize)),
        (Agent::Codex, codex_dir, None),
    ] {
        for file in session_files(dir, levels) {
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
            // Opt-in is decided on the cwds as the log recorded them, before
            // anything is rewritten for the wire.
            let opted = opted_in(&record, allowed);
            elide_home(&mut record, home);
            cap(&mut record);
            let (body, redacted) = if opted {
                let (body, redacted) = build_body(&record, &file)?;
                (Some(body), redacted)
            } else {
                (None, 0)
            };
            let tool_calls = record.turns.iter().map(|turn| turn.tool_calls.len()).sum();
            sessions.push(Prepared {
                record,
                opted_in: opted,
                body,
                redacted,
                tool_calls,
                uploaded: None,
                created: false,
            });
        }
    }
    Ok((sessions, unreadable, unparsed))
}

/// Upload the opted-in sessions, one POST each. This is the command's first
/// socket and its first read of the token; both sit after all the local work
/// on purpose, and `--dry-run` never gets here at all.
fn upload(sessions: &mut [Prepared], dry_run: bool, api_url: &str) -> CliResult<()> {
    let sendable = sessions.iter().filter(|session| session.opted_in).count();
    if dry_run || sendable == 0 {
        return Ok(());
    }
    let client = Client::new(api_url).with_token(crate::auth::token_for(api_url)?);
    for session in sessions.iter_mut().filter(|session| session.opted_in) {
        let body = session
            .body
            .as_deref()
            .expect("an opted-in session has a redacted body");
        let title = format!(
            "{} session {}",
            session.record.agent, session.record.session_id
        );
        let value = client
            .post_source(SOURCE_KIND, &title, body, SCOPE)
            .map_err(|error| err(format!("{title}: {error}")))?;
        session.uploaded = Some(crate::text(&value, "id").to_owned());
        session.created = value
            .get("created")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    }
    Ok(())
}

/// The report, JSON or human: one line per session, then the counts.
fn report(
    sessions: &[Prepared],
    unreadable: usize,
    unparsed: usize,
    dry_run: bool,
    dirs: (&Path, &Path),
    out: &Out,
) {
    let (mut uploaded, mut unchanged, mut skipped) = (0usize, 0usize, 0usize);
    let mut would_upload = 0usize;
    let mut redacted_total = 0usize;
    let mut entries = Vec::new();
    let mut lines = Vec::new();
    for session in sessions {
        let status = match (session.opted_in, dry_run, session.created) {
            (false, _, _) => {
                skipped += 1;
                "skipped"
            }
            (true, true, _) => {
                would_upload += 1;
                "would_upload"
            }
            (true, false, true) => {
                uploaded += 1;
                "uploaded"
            }
            (true, false, false) => {
                unchanged += 1;
                "unchanged"
            }
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
            "status": status,
        });
        if status == "skipped" {
            entry["reason"] = json!("repo not opted in");
        }
        if let Some(id) = &session.uploaded {
            entry["id"] = json!(id);
        }
        entries.push(entry);
        lines.push(human_line(session, status));
    }

    let report = json!({
        "scanned": sessions.len(),
        "uploaded": uploaded,
        "unchanged": unchanged,
        "skipped": skipped,
        "unreadable": unreadable,
        "unparsed": unparsed,
        "redacted": redacted_total,
        "dry_run": dry_run,
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
    use super::{
        Agent, MAX_SESSION_BODY_BYTES, MAX_TURN_TEXT_CHARS, cap, collect, elide_home, is_inside,
        parse_allowlist, serialise, truncate_text,
    };

    /// The fixtures, embedded at compile time: the same bytes the integration
    /// tests drive the real binary over.
    const CLAUDE_SESSION: &str = include_str!(
        "../tests/fixtures/logs/claude/projects/-home-alice-code-demo/3f2a9c1e-5b7d-4e8a-9c1f-2a4b6d8e0f1a.jsonl"
    );
    const CODEX_SESSION: &str = include_str!(
        "../tests/fixtures/logs/codex/sessions/2026/10/01/rollout-2026-10-01T09-31-00-a1b2c3d4-e5f6-4a9b-8c7d-3e5f7a9b1c2d.jsonl"
    );

    /// The Claude fixture's numbers, hand-computed: the one assistant message
    /// is streamed as two lines repeating the same `message.id`, usage and
    /// `costUSD`, so all three are counted once; the tool-result user line,
    /// the summary, the progress mark and the torn last line contribute
    /// nothing.
    #[test]
    fn claude_totals_count_a_split_message_once() {
        let record = collect(CLAUDE_SESSION, Agent::ClaudeCode).expect("a session");
        assert_eq!(record.schema_version, 1);
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
        // usage: 120 + 45 + 30 + 15, once; cost 0.012, once.
        assert_eq!(record.totals.input_tokens, 120);
        assert_eq!(record.totals.output_tokens, 45);
        assert_eq!(record.totals.cache_read_tokens, 30);
        assert_eq!(record.totals.cache_creation_tokens, 15);
        assert_eq!(record.totals.reasoning_tokens, 0);
        assert_eq!(record.totals.cost_usd, Some(0.012));
        // user prompt + the one merged assistant message; the tool-result
        // line, the summary, the progress mark and the torn write all
        // contributed nothing.
        assert_eq!(record.turns.len(), 2);
        assert_eq!(record.turns[0].role, "user");
        assert_eq!(record.turns[1].role, "assistant");
        assert_eq!(
            record.turns[1]
                .tool_calls
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            ["Bash"]
        );
        // Both halves of the split message's text landed in the one turn.
        assert!(record.turns[1].text.contains("They live under"));
        assert!(record.turns[1].text.contains("livingbrain logs sync"));
        assert!(!record.truncated);
    }

    /// The Codex fixture's numbers, hand-computed: two cumulative
    /// `token_count` events, the last one (800/120/96/40) is the session's.
    /// The wrapped `<user_instructions>` opener makes no turn and moves no
    /// clock; the tool call's arguments and its output are nowhere.
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
        // The unknown `ghost_trace` line carries a later timestamp and still
        // did not move `ended_at`: skipped lines contribute nothing.
        assert_eq!(record.ended_at.as_deref(), Some("2026-10-01T09:31:30.000Z"));
        assert_eq!(record.totals.input_tokens, 800);
        assert_eq!(record.totals.output_tokens, 120);
        assert_eq!(record.totals.cache_read_tokens, 96);
        assert_eq!(record.totals.cache_creation_tokens, 0);
        assert_eq!(record.totals.reasoning_tokens, 40);
        assert_eq!(record.totals.cost_usd, None);
        // user prompt, the tool-only turn carrying the call, the answer.
        assert_eq!(record.turns.len(), 3);
        assert_eq!(record.turns[0].role, "user");
        assert_eq!(record.turns[1].role, "assistant");
        assert_eq!(record.turns[1].text, "");
        assert_eq!(
            record.turns[1]
                .tool_calls
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            ["shell"]
        );
        let everywhere: String = serialise(&record);
        assert!(
            !everywhere.contains("user_instructions"),
            "bootstrap context leaked: {everywhere}"
        );
        assert!(
            !everywhere.contains("README.md"),
            "tool output leaked: {everywhere}"
        );
        assert!(
            !everywhere.contains("cmd"),
            "tool input leaked: {everywhere}"
        );
    }

    /// One session, one byte string, however often it is parsed — the whole
    /// of the ledger's re-sync idempotency.
    #[test]
    fn a_session_serialises_to_the_same_bytes_every_time() {
        for (text, agent) in [
            (CLAUDE_SESSION, Agent::ClaudeCode),
            (CODEX_SESSION, Agent::Codex),
        ] {
            let first = serialise(&collect(text, agent).expect("a session"));
            let second = serialise(&collect(text, agent).expect("a session"));
            assert_eq!(first, second);
            // And the declared field order is the wire order.
            assert!(
                first.starts_with("{\"schema_version\":1,"),
                "schema_version must lead the body: {first}"
            );
        }
    }

    /// A long turn is cut to the cap, on a character boundary.
    #[test]
    fn long_turn_text_is_truncated() {
        let long = "x".repeat(3000);
        let line = format!(
            "{{\"type\":\"user\",\"sessionId\":\"s\",\"cwd\":\"/opt/work\",\"message\":{{\"role\":\"user\",\"content\":\"{long}\"}}}}\n"
        );
        let mut record = collect(&line, Agent::ClaudeCode).expect("a session");
        assert_eq!(record.turns[0].text.chars().count(), 3000);
        cap(&mut record);
        assert_eq!(record.turns[0].text.chars().count(), MAX_TURN_TEXT_CHARS);
        assert!(!record.truncated, "text truncation is not body truncation");

        let mut accented = "é".repeat(50);
        truncate_text(&mut accented, 10);
        assert_eq!(accented.chars().count(), 10);
        let mut short = String::from("short");
        truncate_text(&mut short, 10);
        assert_eq!(short, "short");
    }

    /// A session too big for the wire loses trailing turns, keeps its head,
    /// and says so.
    #[test]
    fn an_oversized_session_drops_trailing_turns_and_says_so() {
        let filler = "y".repeat(MAX_TURN_TEXT_CHARS);
        let mut text = String::new();
        for index in 0..600 {
            text.push_str(&format!(
                "{{\"type\":\"user\",\"sessionId\":\"s\",\"cwd\":\"/opt/work\",\"timestamp\":\"2026-10-01T09:00:{:02}.000Z\",\"message\":{{\"role\":\"user\",\"content\":\"turn {index} {filler}\"}}}}\n",
                index % 60
            ));
        }
        let mut record = collect(&text, Agent::ClaudeCode).expect("a session");
        let turns_before = record.turns.len();
        cap(&mut record);
        assert!(record.truncated);
        assert!(record.turns.len() < turns_before);
        assert!(
            serialise(&record).len() <= MAX_SESSION_BODY_BYTES,
            "the body is still over the cap"
        );
        assert_eq!(
            record.turns[0].role, "user",
            "the head of the session stays"
        );
    }

    /// Home elision rewrites the real prefix and the dash-encoded project-dir
    /// form of directories under `$HOME`; with no home known, nothing is
    /// rewritten.
    #[test]
    fn home_elision_rewrites_the_real_and_the_encoded_forms() {
        let mut record = collect(CLAUDE_SESSION, Agent::ClaudeCode).expect("a session");
        elide_home(&mut record, &Some("/home/alice".into()));
        assert_eq!(record.repo, "~/code/demo");
        let prompt = &record.turns[0].text;
        assert!(!prompt.contains("/home/alice"), "{prompt}");
        assert!(!prompt.contains("-home-alice"), "{prompt}");
        // The encoded project directory itself is a needle, so it folds
        // wholly to `~`, and the longest needle wins over its prefixes.
        assert!(prompt.contains("folder ~ under"), "{prompt}");

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

    /// The opt-in file round-trips; a corrupt file and a future version are
    /// refused, not read as empty.
    #[test]
    fn the_allowlist_body_round_trips() {
        let mut allowed = std::collections::BTreeSet::new();
        allowed.insert("/home/alice/code/demo".to_owned());
        allowed.insert("/opt/work".to_owned());
        let body = serde_json::to_vec_pretty(&super::AllowlistFile {
            version: super::ALLOWLIST_VERSION,
            allowed: allowed.clone(),
        })
        .expect("serialises");
        let parsed = parse_allowlist(&body).expect("parses");
        assert_eq!(parsed.version, super::ALLOWLIST_VERSION);
        assert_eq!(parsed.allowed, allowed);

        assert!(parse_allowlist(b"{not json").is_err());
        assert!(parse_allowlist(b"{}").is_err(), "no version, no allowed");
        let error = parse_allowlist(br#"{"version":99,"allowed":[]}"#)
            .expect_err("an unknown version is refused");
        assert!(error.contains("version 99 is not 1"), "{error}");
    }

    /// An empty record serialises with every field the schema promises, in
    /// order, optionals as nulls — the shape a version-1 reader relies on.
    #[test]
    fn the_schema_shape_is_stable() {
        let record = super::empty_record(Agent::ClaudeCode, 1);
        assert_eq!(
            serialise(&record),
            concat!(
                r#"{"schema_version":1,"agent":"claude_code","collector_version":1,"#,
                r#""session_id":"","repo":"","branch":null,"models":[],"started_at":null,"#,
                r#""ended_at":null,"turns":[],"totals":{"input_tokens":0,"output_tokens":0,"#,
                r#""cache_read_tokens":0,"cache_creation_tokens":0,"reasoning_tokens":0,"#,
                r#""cost_usd":null},"outcome":null,"truncated":false}"#
            )
        );
    }
}
