//! The Claude Code reader: `~/.claude/projects/<project>/<session>.jsonl`.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use super::clean::{UserText, user_text};
use super::tools::{claude_exit_code, claude_result_text, describe, settle};
use super::{
    Agent, Role, SessionRecord, Turn, empty_record, note_cwd, note_model, note_once, note_time,
    num_field, present, push_event, str_field,
};

/// The Claude Code reader's version: bumped when reading the format changes
/// in a way that changes the record it produces. 2: tool inputs and results,
/// thinking, boilerplate filtered, interruptions as events.
pub(super) const COLLECTOR_VERSION: u32 = 2;

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

    for line in text.lines() {
        let Ok(line) = serde_json::from_str::<Value>(line) else {
            continue; // a torn write in a live log is not a failed sync
        };
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
                    if !turn.text.is_empty() {
                        turn.text.push('\n');
                    }
                    turn.text.push_str(str_field(block, "text"));
                }
                "tool_use" if !str_field(block, "name").is_empty() => {
                    let index = assistant_turn(&mut record, &mut opened, &message_id, &at);
                    let turn = &mut record.turns[index];
                    if turn.tool_calls.len() >= super::MAX_TOOL_CALLS {
                        continue;
                    }
                    let id = str_field(block, "id");
                    let call = describe(
                        str_field(block, "name"),
                        block.get("input").unwrap_or(&Value::Null),
                    );
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
