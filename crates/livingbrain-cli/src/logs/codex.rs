//! The Codex reader: `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`.

use std::collections::BTreeMap;

use serde_json::Value;

use super::clean::{UserText, user_text};
use super::tools::{codex_result, describe, settle};
use super::{
    Agent, Role, SessionRecord, Totals, Turn, empty_record, note_cwd, note_model, note_once,
    note_time, num_field, present, push_event, str_field,
};

/// The Codex reader's version: bumped when reading the format changes in a
/// way that changes the record it produces. 2: tool inputs and results,
/// reasoning summaries, developer/boilerplate messages filtered,
/// interruptions as events.
pub(super) const COLLECTOR_VERSION: u32 = 2;

/// Read one rollout of `{timestamp, type, payload}` lines. `token_count`
/// events carry cumulative `total_token_usage`, so the **last** one is the
/// session's totals — summing would multiply every turn in. There is no
/// cache-creation figure and no cost in this format; none is invented.
pub(super) fn collect(text: &str) -> Option<SessionRecord> {
    let mut record = empty_record(Agent::Codex, COLLECTOR_VERSION);
    // Where each call sits (turn, call), for its output to find it.
    let mut calls: BTreeMap<String, (usize, usize)> = BTreeMap::new();
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
                        .and_then(present);
                }
            }
            "turn_context" => {
                note_time(&mut record, &at);
                note_cwd(&mut record, str_field(payload, "cwd"));
                note_model(&mut record, str_field(payload, "model"));
            }
            "response_item" => response_item(&mut record, &mut calls, payload, &at),
            "event_msg" => match str_field(payload, "type") {
                "token_count" => {
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
                "turn_aborted" => {
                    note_time(&mut record, &at);
                    push_event(&mut record, &at, "Turn interrupted by the user");
                }
                // Agent and user messages repeated as events, task
                // markers, …: the response items already carry them.
                _ => {}
            },
            // Ghost traces, compactions, whatever else a newer Codex
            // writes: skipped, not fatal.
            _ => {}
        }
    }
    (!record.session_id.is_empty()).then_some(record)
}

/// One `response_item`. Only a payload that produced something moves the
/// session's clock.
fn response_item(
    record: &mut SessionRecord,
    calls: &mut BTreeMap<String, (usize, usize)>,
    item: &Value,
    at: &str,
) {
    match str_field(item, "type") {
        "message" => {
            let role = match str_field(item, "role") {
                "assistant" => Role::Assistant,
                "user" => Role::User,
                // `developer` and `system` messages are the harness
                // instructing the model: never conversation.
                _ => return,
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
            if role == Role::Assistant {
                if !text.trim().is_empty() {
                    note_time(record, at);
                    record.turns.push(Turn::new(Role::Assistant, at, &text));
                }
                return;
            }
            match user_text(&text) {
                UserText::Said(text) => {
                    note_time(record, at);
                    record.turns.push(Turn::new(Role::User, at, &text));
                }
                UserText::Event(text) => {
                    note_time(record, at);
                    push_event(record, at, &text);
                }
                UserText::Nothing => {}
            }
        }
        // The summary a reasoning item carries, when it carries one; the
        // encrypted content is never read.
        "reasoning" => {
            let summary = item
                .get("summary")
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .map(|part| str_field(part, "text").trim())
                        .filter(|text| !text.is_empty())
                        .collect::<Vec<_>>()
                        .join("\n\n")
                })
                .unwrap_or_default();
            if !summary.is_empty() {
                note_time(record, at);
                record.turns.push(Turn::new(Role::Reasoning, at, &summary));
            }
        }
        // A call attaches to a trailing tool-only turn — several parallel
        // calls are one action — or starts one. The last message turn is not
        // its author: the call belongs to the action after it.
        "function_call" | "custom_tool_call" | "local_shell_call" => {
            let name = match str_field(item, "type") {
                "local_shell_call" => "local_shell",
                _ => str_field(item, "name"),
            };
            if name.is_empty() {
                return;
            }
            note_time(record, at);
            let input = match str_field(item, "type") {
                // Arguments are a JSON string; one that does not parse is
                // shown as the text it is.
                "function_call" => {
                    let raw = str_field(item, "arguments");
                    serde_json::from_str::<Value>(raw).unwrap_or_else(|_| Value::from(raw))
                }
                "local_shell_call" => item.get("action").cloned().unwrap_or(Value::Null),
                _ => item.get("input").cloned().unwrap_or(Value::Null),
            };
            let id = str_field(item, "call_id");
            let call = describe(name, &input);
            let index = match record.turns.last() {
                Some(turn)
                    if turn.role == Role::Assistant
                        && turn.text.is_empty()
                        && turn.tool_calls.len() < super::MAX_TOOL_CALLS =>
                {
                    record.turns.len() - 1
                }
                _ => {
                    record.turns.push(Turn::new(Role::Assistant, at, ""));
                    record.turns.len() - 1
                }
            };
            let turn = &mut record.turns[index];
            turn.tool_calls.push(call);
            if !id.is_empty() {
                calls.insert(id.to_owned(), (index, turn.tool_calls.len() - 1));
            }
        }
        "function_call_output" | "custom_tool_call_output" | "local_shell_call_output" => {
            let Some(&(turn, call)) = calls.get(str_field(item, "call_id")) else {
                return;
            };
            note_time(record, at);
            let (output, exit, status) = codex_result(item.get("output").unwrap_or(&Value::Null));
            settle(
                &mut record.turns[turn].tool_calls[call],
                &output,
                exit,
                status,
            );
        }
        // Anything else: skipped.
        _ => {}
    }
}
