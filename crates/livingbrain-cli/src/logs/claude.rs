//! The Claude Code reader: `~/.claude/projects/<project>/<session>.jsonl`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::Value;

use super::clean::{UserText, user_text};
use super::tools::{claude_exit_code, claude_result_text, describe, settle};
use super::{
    Agent, Role, SessionRecord, Subagent, Turn, attach_subagent, call_by_id, empty_record,
    note_cwd, note_model, note_once, note_time, num_field, present, push_event, str_field,
};

/// The Claude Code reader's version: bumped when reading the format changes
/// in a way that changes the record it produces. 2: tool inputs and results,
/// thinking, boilerplate filtered, interruptions as events. 3: subagents'
/// sessions nested in the calls that launched them, the model per turn.
pub(super) const COLLECTOR_VERSION: u32 = 3;

/// Read one session file. One assistant message streamed as several lines
/// repeats the same `message.id`, the same usage and the same `costUSD`; all
/// three are counted once per id. A tool's result arrives on a later user
/// line and is paired with its call by `tool_use_id`. Lines this build does
/// not know are skipped, never fatal — a live log is read while its writer
/// may still be running.
pub(super) fn collect(text: &str) -> Option<SessionRecord> {
    let mut record = empty_record(Agent::ClaudeCode, COLLECTOR_VERSION);
    // Message ids whose usage has been counted, the turn each id's blocks
    // belong to, and where each tool call sits (turn, call).
    let mut counted: BTreeSet<String> = BTreeSet::new();
    let mut opened: BTreeMap<String, usize> = BTreeMap::new();
    let mut calls: BTreeMap<String, (usize, usize)> = BTreeMap::new();

    for raw in text.lines() {
        let Ok(line) = serde_json::from_str::<Value>(raw) else {
            continue; // a torn write in a live log is not a failed sync
        };
        // A background agent's end, reported to the session that launched
        // it — on a user line, a queued command or a queue operation alike:
        // read before anything decides the line is not conversation.
        if raw.contains("<task-notification>") {
            note_notifications(&mut record, raw);
        }
        let role = match str_field(&line, "type") {
            "user" => Role::User,
            "assistant" => Role::Assistant,
            _ => continue,
        };
        note_once(&mut record.session_id, str_field(&line, "sessionId"));
        note_cwd(&mut record, str_field(&line, "cwd"));
        if record.branch.is_none() {
            record.branch = present(str_field(&line, "gitBranch"));
        }
        let at = str_field(&line, "timestamp").to_owned();
        note_time(&mut record, &at);
        let Some(message) = line.get("message") else {
            continue;
        };
        note_model(&mut record, str_field(message, "model"));

        if role == Role::User {
            // Lines the agent wrote into the user role for itself — an
            // attached image's path, a skill's body — are not the person.
            if line.get("isMeta").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            if line.get("isCompactSummary").and_then(Value::as_bool) == Some(true) {
                push_event(
                    &mut record,
                    &at,
                    "Context compacted: the session continued from a summary",
                );
                continue;
            }
            user_line(&mut record, &line, message, &at, &calls);
            continue;
        }

        let message_id = str_field(message, "id").to_owned();
        for block in message
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            match str_field(block, "type") {
                // Thinking as the log carries it — an empty block with only
                // a signature is the encrypted kind, and is dropped.
                "thinking" if !str_field(block, "thinking").trim().is_empty() => {
                    record.turns.push(Turn::new(
                        Role::Reasoning,
                        &at,
                        str_field(block, "thinking").trim(),
                    ));
                }
                "text" if !str_field(block, "text").is_empty() => {
                    let index = assistant_turn(&mut record, &mut opened, &message_id, &at);
                    let turn = &mut record.turns[index];
                    note_turn_model(turn, message);
                    if !turn.text.is_empty() {
                        turn.text.push('\n');
                    }
                    turn.text.push_str(str_field(block, "text"));
                }
                "tool_use" if !str_field(block, "name").is_empty() => {
                    let index = assistant_turn(&mut record, &mut opened, &message_id, &at);
                    let turn = &mut record.turns[index];
                    note_turn_model(turn, message);
                    if turn.tool_calls.len() >= super::MAX_TOOL_CALLS {
                        continue;
                    }
                    let id = str_field(block, "id");
                    let mut call = describe(
                        str_field(block, "name"),
                        block.get("input").unwrap_or(&Value::Null),
                    );
                    call.id = present(id);
                    turn.tool_calls.push(call);
                    if !id.is_empty() {
                        calls.insert(id.to_owned(), (index, turn.tool_calls.len() - 1));
                    }
                }
                // `redacted_thinking`, empty thinking, anything newer: dropped.
                _ => {}
            }
        }
        // Usage and cost are per request, and the request is the message:
        // count them on the line that first carries the id. A line with no
        // id cannot be deduplicated and is taken at its word.
        let first_of_its_message = message_id.is_empty() || counted.insert(message_id);
        if first_of_its_message {
            if let Some(usage) = message.get("usage") {
                record.totals.input_tokens += num_field(usage, "input_tokens");
                record.totals.output_tokens += num_field(usage, "output_tokens");
                record.totals.cache_read_tokens += num_field(usage, "cache_read_input_tokens");
                record.totals.cache_creation_tokens +=
                    num_field(usage, "cache_creation_input_tokens");
            }
            if let Some(cost) = line.get("costUSD").and_then(Value::as_f64) {
                record.totals.cost_usd = Some(record.totals.cost_usd.unwrap_or(0.0) + cost);
            }
        }
    }
    (!record.session_id.is_empty()).then_some(record)
}

/// The model an assistant message names, on its turn — `<synthetic>` is the
/// agent's own placeholder for a message no model wrote.
fn note_turn_model(turn: &mut Turn, message: &Value) {
    let model = str_field(message, "model");
    if turn.model.is_none() && !model.is_empty() && model != "<synthetic>" {
        turn.model = Some(model.to_owned());
    }
}

/// Every `<task-notification>` in a line: a background agent that
/// stopped, by the id of the call that launched it, with its status. The
/// last one for a call wins — a resumed agent notifies again.
fn note_notifications(record: &mut SessionRecord, text: &str) {
    // The tags' values are ids and status words: nothing JSON escapes.
    for block in text.split("<task-notification>").skip(1) {
        let block = block.split("</task-notification>").next().unwrap_or(block);
        let tag = |name: &str| {
            let open = format!("<{name}>");
            let close = format!("</{name}>");
            let start = block.find(&open)? + open.len();
            let end = block[start..].find(&close)? + start;
            Some(block[start..end].trim().to_owned())
        };
        let (Some(call), Some(status)) = (tag("tool-use-id"), tag("status")) else {
            continue;
        };
        if let Some(status) = subagent_status(&status) {
            record.subagent_status.insert(call, status);
        }
    }
}

/// A status as the logs spell it, in the record's vocabulary.
fn subagent_status(raw: &str) -> Option<&'static str> {
    match raw {
        "completed" | "complete" | "done" | "success" => Some("completed"),
        "failed" | "error" => Some("failed"),
        "killed" | "stopped" | "cancelled" | "canceled" | "interrupted" => Some("stopped"),
        "async_launched" | "running" | "forked" => Some("running"),
        _ => None,
    }
}

/// One subagent transcript beside a session: Claude Code writes each to
/// `<session>/subagents/agent-<id>.jsonl`, its launch in
/// `agent-<id>.meta.json` beside it (`agentType`, `description`, the
/// launching `toolUseId`, `parentAgentId` for one a subagent launched).
pub(super) struct Child {
    pub(super) agent_id: String,
    pub(super) meta: Value,
    pub(super) text: String,
}

/// The subagent transcripts of the session in `file`, sorted by name; none
/// when it has no `subagents` directory. A file that cannot be read is
/// skipped — the session is still the session.
pub(super) fn children(file: &Path) -> Vec<Child> {
    let dir = file.with_extension("").join("subagents");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with("agent-") && name.ends_with(".jsonl"))
        .collect();
    names.sort();
    names
        .into_iter()
        .filter_map(|name| {
            let stem = name.trim_end_matches(".jsonl");
            let text = std::fs::read_to_string(dir.join(&name)).ok()?;
            let meta = std::fs::read(dir.join(format!("{stem}.meta.json")))
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .unwrap_or(Value::Null);
            Some(Child {
                agent_id: stem.trim_start_matches("agent-").to_owned(),
                meta,
                text,
            })
        })
        .collect()
}

/// Nest each subagent transcript in the call that launched it — the
/// session's own, or (`parentAgentId`) another subagent's — deepest first,
/// so a subagent carries its own subagents when it is nested. The launching
/// call is the meta's `toolUseId`, else the call whose result named the
/// agent's id. A subagent whose call cannot be found is left out: there is
/// nowhere to show it. Its working directories count for the opt-in all
/// the same.
pub(super) fn attach(record: &mut SessionRecord, children: Vec<Child>) {
    struct Node {
        parent: Option<String>,
        call: Option<String>,
        record: SessionRecord,
        name: String,
        description: String,
    }
    let mut nodes: BTreeMap<String, Node> = BTreeMap::new();
    for child in children {
        let Some(sub) = collect(&child.text) else {
            continue;
        };
        for cwd in &sub.cwds {
            super::note_cwd(record, cwd);
        }
        let meta = &child.meta;
        nodes.insert(
            child.agent_id.clone(),
            Node {
                parent: present(str_field(meta, "parentAgentId")),
                call: present(str_field(meta, "toolUseId")),
                record: sub,
                name: str_field(meta, "agentType").to_owned(),
                description: str_field(meta, "description").to_owned(),
            },
        );
    }
    // Deepest first: a node is attached only once nothing still waits to
    // attach into it.
    let depth = |id: &str, nodes: &BTreeMap<String, Node>| {
        let mut depth = 0usize;
        let mut at = nodes.get(id).and_then(|node| node.parent.clone());
        while let Some(parent) = at {
            depth += 1;
            if depth > nodes.len() {
                break; // a cycle: treat it as deep, attach what can be
            }
            at = nodes.get(&parent).and_then(|node| node.parent.clone());
        }
        depth
    };
    let mut order: Vec<(usize, String)> = nodes
        .keys()
        .map(|id| (depth(id, &nodes), id.clone()))
        .collect();
    order.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    for (_, id) in order {
        let Some(node) = nodes.remove(&id) else {
            continue;
        };
        let host: &mut SessionRecord = match node.parent.as_deref() {
            Some(parent) if nodes.contains_key(parent) => {
                &mut nodes.get_mut(parent).expect("checked").record
            }
            _ => &mut *record,
        };
        let Some(call_id) = node
            .call
            .clone()
            .or_else(|| host.agent_links.get(&id).cloned())
        else {
            continue;
        };
        let status = host.subagent_status.get(&call_id).copied();
        let subagent = Subagent::from_record(node.record, &id, &node.name, &node.description);
        if let Some(call) = call_by_id(&mut host.turns, &call_id) {
            attach_subagent(call, subagent, status);
        }
    }
}

/// The assistant turn a message id's blocks land in: the one already opened
/// for that id, else a new one. An id-less line is a turn of its own.
fn assistant_turn(
    record: &mut SessionRecord,
    opened: &mut BTreeMap<String, usize>,
    message_id: &str,
    at: &str,
) -> usize {
    if let Some(index) = opened.get(message_id) {
        return *index;
    }
    record.turns.push(Turn::new(Role::Assistant, at, ""));
    let index = record.turns.len() - 1;
    if !message_id.is_empty() {
        opened.insert(message_id.to_owned(), index);
    }
    index
}

/// One user line: tool results go to their calls; the text, with the
/// boilerplate peeled off, is a turn, an event, or nothing.
fn user_line(
    record: &mut SessionRecord,
    line: &Value,
    message: &Value,
    at: &str,
    calls: &BTreeMap<String, (usize, usize)>,
) {
    let mut texts: Vec<String> = Vec::new();
    match message.get("content") {
        Some(Value::String(text)) => texts.push(text.clone()),
        Some(Value::Array(blocks)) => {
            for block in blocks {
                match str_field(block, "type") {
                    "text" if !str_field(block, "text").is_empty() => {
                        texts.push(str_field(block, "text").to_owned());
                    }
                    "image" => texts.push("[image]".to_owned()),
                    "tool_result" => {
                        let Some(&(turn, call)) = calls.get(str_field(block, "tool_use_id")) else {
                            continue;
                        };
                        let output = claude_result_text(block.get("content"));
                        let failed = block.get("is_error").and_then(Value::as_bool) == Some(true);
                        let interrupted = line
                            .get("toolUseResult")
                            .and_then(|result| result.get("interrupted"))
                            .and_then(Value::as_bool)
                            == Some(true);
                        let exit = if failed {
                            claude_exit_code(&output)
                        } else {
                            None
                        };
                        let status = if interrupted {
                            "interrupted"
                        } else if failed {
                            "error"
                        } else {
                            "ok"
                        };
                        settle(
                            &mut record.turns[turn].tool_calls[call],
                            &output,
                            exit,
                            Some(status),
                        );
                        // A subagent's launch: which agent it started, and
                        // where it stood when the call returned.
                        if let Some(result) = line.get("toolUseResult")
                            && let Some(agent) = result.get("agentId").and_then(Value::as_str)
                        {
                            let id = str_field(block, "tool_use_id").to_owned();
                            record.agent_links.insert(agent.to_owned(), id.clone());
                            let state = if failed {
                                Some("failed")
                            } else {
                                subagent_status(str_field(result, "status"))
                            };
                            if let Some(state) = state {
                                record.subagent_status.entry(id).or_insert(state);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    // An image alone is not a message.
    if texts.iter().all(|text| text == "[image]") {
        return;
    }
    match user_text(&texts.join("\n")) {
        UserText::Said(text) => record.turns.push(Turn::new(Role::User, at, &text)),
        UserText::Event(text) => push_event(record, at, &text),
        UserText::Nothing => {}
    }
}
