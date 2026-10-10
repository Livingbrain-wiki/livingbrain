//! `import chatgpt` / `import claude` — the chat vendors' own data exports,
//! read and normalised on this machine.
//!
//! Both vendors sell the same artefact: a zip whose `conversations.json`
//! holds every chat as JSON, one array per format with very different shapes.
//! This module reads that file straight out of the archive — in memory, never
//! extracted to disk — and normalises both shapes into one [`Conversation`],
//! the way `logs` normalises session logs.
//!
//! A branched chat is a fact of these exports, not an edge case: an edited or
//! regenerated answer leaves its abandoned sibling on a dead branch. ChatGPT
//! encodes that as a `mapping` tree whose live branch hangs off
//! `current_node`; Claude as a flat list threaded with
//! `parent_message_uuid`. Both readers keep only the live branch, and both
//! cut a parent chain that loops rather than walk it forever.
//!
//! [`render`] turns a conversation into deterministic Markdown — the same
//! export renders to the same bytes, which is what makes a re-import read
//! rows back instead of sealing copies. Every message becomes a section whose
//! heading is its citation key, `conversation:<id>#<message-id>`: the anchor
//! any page generated from this source must cite.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;

use serde_json::Value;

/// The version of the ChatGPT reader, recorded in every body it renders.
pub(crate) const CHATGPT_PARSER_VERSION: u32 = 1;

/// The version of the Claude reader, recorded in every body it renders.
pub(crate) const CLAUDE_PARSER_VERSION: u32 = 1;

/// The most one rendered part may be, in bytes: three quarters of redaction's
/// input cap — the headroom the replacement tokens need — and under the
/// ledger's own ceiling besides.
pub(crate) const MAX_PART_BYTES: usize = livingbrain_redact::MAX_INPUT_BYTES * 3 / 4;

/// The most one message section may be; a message over it is truncated with a
/// visible marker rather than allowed to own a whole part.
const MAX_SECTION_BYTES: usize = MAX_PART_BYTES / 2;

/// What a truncated message says, so a lost tail is never silent.
const TRUNCATION_MARKER: &str = "\n\n[truncated]";

/// The most decompressed `conversations.json` this build will read. A zip bomb
/// names itself by its ratio; this is the ceiling the ratio cannot pass.
const MAX_EXPORT_BYTES: u64 = 256 * 1024 * 1024;

/// Which export a run reads: the wire's `source` word, the path prefix and
/// half the header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Chatgpt,
    Claude,
}

impl Kind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Chatgpt => "chatgpt",
            Self::Claude => "claude",
        }
    }

    pub(crate) const fn parser_version(self) -> u32 {
        match self {
            Self::Chatgpt => CHATGPT_PARSER_VERSION,
            Self::Claude => CLAUDE_PARSER_VERSION,
        }
    }
}

/// One conversation, normalised out of either export format.
#[derive(Debug)]
pub(crate) struct Conversation {
    pub(crate) id: String,
    pub(crate) title: String,
    /// RFC 3339, when the export recorded a creation time: ChatGPT's Unix
    /// seconds rendered, Claude's own timestamp carried over.
    pub(crate) created: Option<String>,
    /// Distinct model slugs, in first-seen order.
    pub(crate) models: Vec<String>,
    pub(crate) messages: Vec<Message>,
}

/// One turn of one conversation.
#[derive(Debug)]
pub(crate) struct Message {
    pub(crate) id: String,
    pub(crate) role: String,
    pub(crate) created: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) text: String,
    pub(crate) attachments: Vec<String>,
}

/// The anchor every page generated from this source must cite.
pub(crate) fn citation_key(conversation: &Conversation, message: &Message) -> String {
    format!("conversation:{}#{}", conversation.id, message.id)
}

// ---------------------------------------------------------------------------
// Reading the export

/// Read `conversations.json`: a path ending `.json` is the bare file;
/// anything else is a zip, and the file is read straight out of it, in
/// memory. Nothing an export contains is ever extracted to disk.
pub(crate) fn read_conversations_json(path: &Path) -> Result<Vec<u8>, String> {
    if path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("json"))
    {
        return std::fs::read(path).map_err(|e| format!("could not read {}: {e}", path.display()));
    }
    let file =
        std::fs::File::open(path).map_err(|e| format!("could not open {}: {e}", path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| format!("{} is not a readable zip: {e}", path.display()))?;
    // The root entry wins over a top-level-directory one; both exporters have
    // been seen to wrap their files one directory down.
    let mut candidates: Vec<(usize, String)> = archive
        .file_names()
        .filter_map(|name| {
            let name = name.ok()?;
            match name.split('/').count() {
                1 if name == "conversations.json" => Some((1, name.into_owned())),
                2 if name.ends_with("/conversations.json") => Some((2, name.into_owned())),
                _ => None,
            }
        })
        .collect();
    candidates.sort();
    let Some((_, name)) = candidates.first() else {
        return Err(format!(
            "{} has no conversations.json at its root or in one top-level directory — is this a ChatGPT or Claude data export?",
            path.display()
        ));
    };
    let name = name.clone();
    let mut entry = archive
        .by_name(&name)
        .map_err(|e| format!("could not read {name} out of {}: {e}", path.display()))?;
    let mut json = Vec::new();
    // `take` is what defuses the bomb: the read stops here whatever the
    // entry's declared (or actual) size says.
    entry
        .by_ref()
        .take(MAX_EXPORT_BYTES + 1)
        .read_to_end(&mut json)
        .map_err(|e| format!("could not read {name} out of {}: {e}", path.display()))?;
    if json.len() as u64 > MAX_EXPORT_BYTES {
        return Err(format!(
            "{name} is over {} MiB — this build reads no larger an export",
            MAX_EXPORT_BYTES / (1024 * 1024)
        ));
    }
    Ok(json)
}

// ---------------------------------------------------------------------------
// Parsing

/// Parse a `conversations.json` of `kind` into normalised conversations. One
/// that cannot be addressed — no id — is skipped; everything else that fails
/// shape is read forward-compatibly, field by field.
pub(crate) fn parse(kind: Kind, json: &[u8]) -> Result<Vec<Conversation>, String> {
    let value: Value = serde_json::from_slice(json)
        .map_err(|e| format!("conversations.json is not valid JSON: {e}"))?;
    let entries = value
        .as_array()
        .ok_or("conversations.json is not an array of conversations")?;
    Ok(entries
        .iter()
        .filter_map(|entry| match kind {
            Kind::Chatgpt => chatgpt(entry),
            Kind::Claude => claude(entry),
        })
        .collect())
}

/// The string at `key`, or `""` — every field of either format is optional,
/// because the formats are read forward-compatibly.
fn str_field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

/// The non-empty string at `key`, as an owned value.
fn str_opt(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// A Unix-seconds timestamp (ChatGPT's shape) as RFC 3339 in UTC.
fn epoch(value: Option<&Value>) -> Option<String> {
    let secs = value?.as_f64()?;
    if secs <= 0.0 {
        return None;
    }
    let at = time::OffsetDateTime::from_unix_timestamp(secs as i64).ok()?;
    at.format(&time::format_description::well_known::Rfc3339)
        .ok()
}

/// Read one ChatGPT conversation: a `mapping` of nodes threaded with `parent`
/// and `children`, whose live branch ends at `current_node`.
fn chatgpt(entry: &Value) -> Option<Conversation> {
    let id = match str_field(entry, "conversation_id") {
        "" => str_field(entry, "id"),
        id => id,
    };
    if id.is_empty() {
        return None;
    }
    let mapping = entry.get("mapping").and_then(Value::as_object)?;
    // The branch starts at `current_node`; an export without one falls back to
    // the first leaf in key order — some branch's end, if not the one the
    // reader last saw.
    let start = match str_field(entry, "current_node") {
        current if mapping.contains_key(current) => current,
        _ => mapping
            .iter()
            .find(|(_, node)| {
                node.get("children")
                    .and_then(Value::as_array)
                    .is_none_or(|children| children.is_empty())
            })
            .map(|(key, _)| key.as_str())
            .unwrap_or(""),
    };
    // Walk the parents up from the branch's end, then reverse into
    // chronological order. A cycle — which an edited export can produce — is
    // cut the moment a node repeats.
    let mut branch: Vec<&str> = Vec::new();
    let mut seen = BTreeSet::new();
    let mut node = if start.is_empty() { None } else { Some(start) };
    while let Some(key) = node {
        if !seen.insert(key) {
            break;
        }
        branch.push(key);
        node = match mapping
            .get(key)
            .and_then(|n| n.get("parent"))
            .and_then(Value::as_str)
        {
            Some(parent) if !parent.is_empty() && mapping.contains_key(parent) => Some(parent),
            _ => None,
        };
    }
    branch.reverse();

    let mut conversation = Conversation {
        id: id.to_owned(),
        title: str_field(entry, "title").to_owned(),
        created: epoch(entry.get("create_time")),
        models: Vec::new(),
        messages: Vec::new(),
    };
    for key in branch {
        let Some(node) = mapping.get(key) else {
            continue;
        };
        let Some(message) = node.get("message") else {
            continue;
        };
        if message.is_null() {
            continue;
        }
        let role = str_field(message.get("author").unwrap_or(&Value::Null), "role");
        // The system prompt travels in the export but was never part of the
        // conversation; a hidden message is one the reader deleted from view.
        if role == "system" {
            continue;
        }
        let metadata = message.get("metadata");
        if metadata
            .and_then(|m| m.get("is_visually_hidden_from_conversation"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        let (text, mut attachments) = chatgpt_content(message.get("content"));
        for attachment in metadata
            .and_then(|m| m.get("attachments"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let name = str_field(attachment, "name");
            if !name.is_empty() {
                attachments.push(name.to_owned());
            }
        }
        if text.is_empty() && attachments.is_empty() {
            continue;
        }
        let model = metadata
            .and_then(|m| m.get("model_slug"))
            .and_then(Value::as_str);
        if let Some(model) = model
            && !conversation.models.iter().any(|seen| seen == model)
        {
            conversation.models.push(model.to_owned());
        }
        conversation.messages.push(Message {
            id: str_field(message, "id").to_owned(),
            role: if role.is_empty() {
                "unknown".to_owned()
            } else {
                role.to_owned()
            },
            created: epoch(message.get("create_time")),
            model: model.map(str::to_owned),
            text,
            attachments,
        });
    }
    Some(conversation)
}

/// The text and attachment references of one ChatGPT message. String `parts`
/// are the text; an object part — an image asset pointer, say — has no text to
/// keep, so it becomes an attachment reference. A `content` that is only a
/// `text` field (code, execution output) is taken whole.
fn chatgpt_content(content: Option<&Value>) -> (String, Vec<String>) {
    let Some(content) = content else {
        return (String::new(), Vec::new());
    };
    let mut parts: Vec<&str> = Vec::new();
    let mut attachments = Vec::new();
    if let Some(list) = content.get("parts").and_then(Value::as_array) {
        for part in list {
            match part {
                Value::String(text) if !text.is_empty() => parts.push(text),
                Value::String(_) => {}
                other => match str_field(other, "asset_pointer") {
                    "" => {
                        let kind = str_field(other, "content_type");
                        if !kind.is_empty() {
                            attachments.push(kind.to_owned());
                        }
                    }
                    pointer => attachments.push(pointer.to_owned()),
                },
            }
        }
    } else if let Some(text) = content.get("text").and_then(Value::as_str)
        && !text.is_empty()
    {
        parts.push(text);
    }
    (parts.join("\n"), attachments)
}

/// Read one Claude conversation: a flat `chat_messages` list, threaded with
/// `parent_message_uuid` when it has been branched or edited.
fn claude(entry: &Value) -> Option<Conversation> {
    let id = str_field(entry, "uuid");
    if id.is_empty() {
        return None;
    }
    let messages = entry.get("chat_messages").and_then(Value::as_array)?;
    let mut conversation = Conversation {
        id: id.to_owned(),
        title: str_field(entry, "name").to_owned(),
        created: str_opt(entry.get("created_at")),
        models: Vec::new(),
        messages: Vec::new(),
    };
    for message in claude_branch(messages) {
        let id = str_field(message, "uuid");
        // No id, no citation key: a message cannot be cited without one.
        if id.is_empty() {
            continue;
        }
        let (text, attachments) = claude_content(message);
        if text.is_empty() && attachments.is_empty() {
            continue;
        }
        let sender = str_field(message, "sender");
        conversation.messages.push(Message {
            id: id.to_owned(),
            role: match sender {
                "human" => "user".to_owned(),
                other if !other.is_empty() => other.to_owned(),
                _ => "unknown".to_owned(),
            },
            created: str_opt(message.get("created_at")),
            // This export names no model per message; none is invented.
            model: None,
            text,
            attachments,
        });
    }
    Some(conversation)
}

/// The messages in conversation order. A chat nothing ever branched keeps its
/// array order; a branched one keeps only the branch whose leaf is the latest
/// message — ties to the later position in the array — from that leaf back to
/// the root.
fn claude_branch(messages: &[Value]) -> Vec<&Value> {
    if !messages
        .iter()
        .any(|message| !str_field(message, "parent_message_uuid").is_empty())
    {
        return messages.iter().collect();
    }
    let by_id: BTreeMap<&str, &Value> = messages
        .iter()
        .filter_map(|message| {
            let id = str_field(message, "uuid");
            (!id.is_empty()).then_some((id, message))
        })
        .collect();
    // A leaf is a message no other message names as its parent.
    let parents: BTreeSet<&str> = messages
        .iter()
        .map(|message| str_field(message, "parent_message_uuid"))
        .filter(|parent| !parent.is_empty())
        .collect();
    // Latest `created_at` wins; RFC 3339 sorts, and an equal timestamp loses
    // to the later position, which this fold keeps.
    let mut leaf: Option<(&str, &Value)> = None;
    for message in messages {
        let id = str_field(message, "uuid");
        if id.is_empty() || parents.contains(id) {
            continue;
        }
        let take = match leaf {
            None => true,
            Some((_, best)) => str_field(message, "created_at") >= str_field(best, "created_at"),
        };
        if take {
            leaf = Some((id, message));
        }
    }
    let Some((leaf_id, _)) = leaf else {
        return messages.iter().collect();
    };
    // Walk the parents up from the leaf, cutting a chain that loops.
    let mut branch: Vec<&Value> = Vec::new();
    let mut seen = BTreeSet::new();
    let mut current = Some(leaf_id);
    while let Some(id) = current {
        if !seen.insert(id) {
            break;
        }
        let Some(message) = by_id.get(id) else {
            break;
        };
        branch.push(*message);
        current = match str_field(message, "parent_message_uuid") {
            "" => None,
            parent => Some(parent),
        };
    }
    branch.reverse();
    branch
}

/// The text and attachment names of one Claude message: the `content` text
/// blocks when the format wrote them, the bare `text` field when it did not
/// (the older format's shape). `attachments` and `files` are either generation's
/// name for the same thing.
fn claude_content(message: &Value) -> (String, Vec<String>) {
    let mut text = String::new();
    for block in message
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if str_field(block, "type") == "text" {
            let block_text = str_field(block, "text");
            if !block_text.is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(block_text);
            }
        }
    }
    if text.is_empty() {
        text = str_field(message, "text").to_owned();
    }
    let mut attachments = Vec::new();
    for key in ["attachments", "files"] {
        for attachment in message
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let name = str_field(attachment, "file_name");
            if !name.is_empty() && !attachments.iter().any(|seen| seen == name) {
                attachments.push(name.to_owned());
            }
        }
    }
    (text, attachments)
}

// ---------------------------------------------------------------------------
// Selection

/// The local, pre-flight narrowing of an export: everything, by default.
pub(crate) struct Selection {
    pub(crate) since: Option<String>,
    pub(crate) until: Option<String>,
    pub(crate) keywords: Vec<String>,
    pub(crate) ids: Vec<String>,
}

impl Selection {
    /// Whether `conversation` is in. A conversation with no recorded date
    /// fails a `--since`/`--until` filter rather than pass it: an undated
    /// conversation cannot be shown to be in the range. Dates compare as the
    /// `YYYY-MM-DD` prefix they share, so one comparison serves both formats.
    pub(crate) fn keeps(&self, conversation: &Conversation) -> bool {
        if !self.ids.is_empty() && !self.ids.iter().any(|id| id == &conversation.id) {
            return false;
        }
        let day = conversation
            .created
            .as_deref()
            .and_then(|created| created.get(..10));
        if let Some(since) = &self.since
            && day.is_none_or(|day| day < since.as_str())
        {
            return false;
        }
        if let Some(until) = &self.until
            && day.is_none_or(|day| day > until.as_str())
        {
            return false;
        }
        if !self.keywords.is_empty()
            && !self
                .keywords
                .iter()
                .any(|keyword| conversation.contains(keyword))
        {
            return false;
        }
        true
    }
}

impl Conversation {
    /// Whether the title or any message's text contains `keyword`,
    /// case-insensitively.
    fn contains(&self, keyword: &str) -> bool {
        let keyword = keyword.to_lowercase();
        self.title.to_lowercase().contains(&keyword)
            || self
                .messages
                .iter()
                .any(|message| message.text.to_lowercase().contains(&keyword))
    }
}

// ---------------------------------------------------------------------------
// Rendering

/// One rendered part of one conversation: Markdown, at most
/// [`MAX_PART_BYTES`] bytes, byte-identical for the same export.
pub(crate) struct Part {
    /// 1-based; equal to `total` when there is only one.
    pub(crate) number: usize,
    pub(crate) total: usize,
    /// Unredacted — redaction is the caller's, after [`MAX_PART_BYTES`] has
    /// already bounded what it will be asked to scan.
    pub(crate) body: String,
}

/// The path a rendered part is filed under: `<format>/<conversation-id>`,
/// and `<format>/<conversation-id>/part-<n>` when one conversation split.
pub(crate) fn part_path(kind: Kind, conversation: &Conversation, part: &Part) -> String {
    let base = format!("{}/{}", kind.as_str(), conversation.id);
    if part.total > 1 {
        format!("{base}/part-{}", part.number)
    } else {
        base
    }
}

/// Render a conversation into one or more parts. Message sections pack in
/// order, splitting at a section boundary when the next would not fit; the
/// header repeats on every part, so each one reads on its own.
pub(crate) fn render(kind: Kind, conversation: &Conversation) -> Vec<Part> {
    let header = base_header(kind, conversation);
    let sections: Vec<String> = conversation
        .messages
        .iter()
        .map(|message| section(conversation, message))
        .collect();
    // The `- part:` line a split adds is what the slack covers.
    let budget = MAX_PART_BYTES.saturating_sub(header.len() + 32);
    let mut packs: Vec<Vec<&str>> = vec![Vec::new()];
    let mut used = 0usize;
    for text in sections.iter().map(String::as_str) {
        // A pack always takes its first section: sections are pre-truncated
        // below the budget, so this never spins.
        if used > 0 && used + text.len() > budget {
            packs.push(Vec::new());
            used = 0;
        }
        used += text.len();
        packs.last_mut().expect("at least one pack").push(text);
    }
    let total = packs.len();
    packs
        .into_iter()
        .enumerate()
        .map(|(index, pack)| {
            let mut body = header.clone();
            if total > 1 {
                body.push_str(&format!("- part: {} of {total}\n", index + 1));
            }
            body.push('\n');
            for text in pack {
                body.push_str(text);
            }
            Part {
                number: index + 1,
                total,
                body,
            }
        })
        .collect()
}

/// The header every part of a conversation carries: what it is, where it came
/// from, and which build of the reader produced it.
fn base_header(kind: Kind, conversation: &Conversation) -> String {
    let mut header = format!(
        "# {}\n\n- source: {}\n- parser-version: {}\n- conversation: {}\n",
        conversation.title.trim(),
        kind.as_str(),
        kind.parser_version(),
        conversation.id,
    );
    if let Some(created) = &conversation.created {
        header.push_str(&format!("- created: {created}\n"));
    }
    if !conversation.models.is_empty() {
        header.push_str(&format!("- models: {}\n", conversation.models.join(", ")));
    }
    header
}

/// One message as a Markdown section whose heading is its citation key.
fn section(conversation: &Conversation, message: &Message) -> String {
    let mut meta: Vec<&str> = vec![&message.role];
    if let Some(created) = &message.created {
        meta.push(created);
    }
    if let Some(model) = &message.model {
        meta.push(model);
    }
    let heading = format!(
        "## {} ({})\n\n",
        citation_key(conversation, message),
        meta.join(" · ")
    );
    let attachments = if message.attachments.is_empty() {
        String::new()
    } else {
        format!("\nAttachments: {}\n", message.attachments.join(", "))
    };
    let mut text = message.text.as_str();
    let mut truncated = false;
    let room = MAX_SECTION_BYTES
        .saturating_sub(heading.len() + attachments.len() + TRUNCATION_MARKER.len());
    if text.len() > room {
        text = &text[..floor_char_boundary(text, room)];
        truncated = true;
    }
    let mut out = String::with_capacity(heading.len() + text.len() + attachments.len() + 2);
    out.push_str(&heading);
    out.push_str(text);
    out.push('\n');
    out.push_str(&attachments);
    if truncated {
        out.push_str(TRUNCATION_MARKER);
    }
    out.push('\n');
    out
}

/// The largest index `<= max` that is a `char` boundary, for cutting text.
fn floor_char_boundary(text: &str, mut max: usize) -> usize {
    if max >= text.len() {
        return text.len();
    }
    while max > 0 && !text.is_char_boundary(max) {
        max -= 1;
    }
    max
}

#[cfg(test)]
mod tests {
    use super::{Conversation, Kind, Selection, citation_key, floor_char_boundary, parse, render};

    /// `parse` takes the array a real `conversations.json` is; the tests each
    /// build one-conversation exports.
    fn chatgpt_array(export: &serde_json::Value) -> Vec<Conversation> {
        parse(
            Kind::Chatgpt,
            serde_json::json!([export]).to_string().as_bytes(),
        )
        .expect("parses")
    }

    fn claude_array(export: &serde_json::Value) -> Vec<Conversation> {
        parse(
            Kind::Claude,
            serde_json::json!([export]).to_string().as_bytes(),
        )
        .expect("parses")
    }

    /// A parent chain that loops — n3's parent is n2, whose parent is n3 — is
    /// cut at the first repeated node, and both messages still render, in
    /// branch order.
    #[test]
    fn a_parent_cycle_is_cut_not_walked_forever() {
        let export = serde_json::json!({
            "conversation_id": "loop", "title": "loop", "current_node": "n3",
            "mapping": {
                "n3": {"id": "n3", "parent": "n2", "children": [],
                    "message": {"id": "n3", "author": {"role": "assistant"},
                        "create_time": 1776246060.0,
                        "content": {"content_type": "text", "parts": ["three"]}}},
                "n2": {"id": "n2", "parent": "n3", "children": ["n3"],
                    "message": {"id": "n2", "author": {"role": "user"},
                        "create_time": 1776246000.0,
                        "content": {"content_type": "text", "parts": ["two"]}}}
            }
        });
        let parsed = chatgpt_array(&export);
        let messages = &parsed[0].messages;
        assert_eq!(messages.len(), 2, "the walk stops, the branch stays");
        assert_eq!(messages[0].id, "n2");
        assert_eq!(messages[1].id, "n3");
        assert_eq!(messages[0].created.as_deref(), Some("2026-04-15T09:40:00Z"));
    }

    /// An object part becomes an attachment reference; the text around it
    /// stays text.
    #[test]
    fn a_non_string_part_becomes_an_attachment_reference() {
        let export = serde_json::json!({
            "conversation_id": "pic", "title": "picture", "current_node": "n2",
            "mapping": {
                "n1": {"id": "n1", "parent": null, "children": ["n2"],
                    "message": {"id": "n1", "author": {"role": "user"},
                        "content": {"content_type": "text", "parts": [
                            "Look at this",
                            {"content_type": "image_asset_pointer",
                             "asset_pointer": "file-service://file-demo", "size_bytes": 1024}
                        ]}}},
                "n2": {"id": "n2", "parent": "n1", "children": [],
                    "message": {"id": "n2", "author": {"role": "assistant"},
                        "metadata": {"attachments": [{"name": "chart.png"}]},
                        "content": {"content_type": "text", "parts": ["A chart."]}}}
            }
        });
        let parsed = chatgpt_array(&export);
        let messages = &parsed[0].messages;
        assert_eq!(messages[0].text, "Look at this");
        assert_eq!(messages[0].attachments, ["file-service://file-demo"]);
        assert_eq!(messages[1].attachments, ["chart.png"]);
    }

    /// The older Claude format wrote a bare `text` and no `content` blocks;
    /// both shapes read, and `content` wins where it exists.
    #[test]
    fn claude_text_falls_back_to_the_bare_field_of_the_older_format() {
        let export = serde_json::json!({
            "uuid": "old", "name": "Old format", "created_at": "2026-04-20T08:00:00Z",
            "chat_messages": [
                {"uuid": "m1", "sender": "human", "created_at": "2026-04-20T08:00:00Z",
                    "text": "Hello there"},
                {"uuid": "m2", "sender": "assistant", "created_at": "2026-04-20T08:01:00Z",
                    "text": "Hi! How can I help?",
                    "content": [{"type": "text", "text": "Hi! How can I help?"}]}
            ]
        });
        let parsed = claude_array(&export);
        let messages = &parsed[0].messages;
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].text, "Hello there");
        assert_eq!(messages[1].text, "Hi! How can I help?");
        assert_eq!(
            citation_key(&parsed[0], &messages[0]),
            "conversation:old#m1"
        );
    }

    /// A Claude chat threaded with `parent_message_uuid` keeps the latest
    /// leaf's branch; a chat without parents keeps its array order.
    #[test]
    fn a_branched_claude_chat_keeps_the_latest_leaf() {
        let export = serde_json::json!({
            "uuid": "branched", "name": "branched", "created_at": "2026-05-01T10:00:00Z",
            "chat_messages": [
                {"uuid": "m1", "sender": "human", "created_at": "2026-05-01T10:00:00Z",
                    "text": "root"},
                {"uuid": "m2", "sender": "assistant", "created_at": "2026-05-01T10:01:00Z",
                    "text": "shared answer", "parent_message_uuid": "m1"},
                {"uuid": "m3", "sender": "human", "created_at": "2026-05-01T10:02:00Z",
                    "text": "dead end", "parent_message_uuid": "m2"},
                {"uuid": "m4", "sender": "assistant", "created_at": "2026-05-01T10:03:00Z",
                    "text": "earlier leaf", "parent_message_uuid": "m3"},
                {"uuid": "m5", "sender": "human", "created_at": "2026-05-01T10:05:00Z",
                    "text": "live leaf", "parent_message_uuid": "m2"}
            ]
        });
        let parsed = claude_array(&export);
        let ids: Vec<&str> = parsed[0].messages.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["m1", "m2", "m5"], "the later leaf's branch wins");
    }

    /// Every flag narrows on its own terms; the date comparison is inclusive
    /// on the day named, and undated conversations fail a date filter.
    #[test]
    fn a_selection_narrows_by_date_keyword_and_id() {
        let conversation = super::Conversation {
            id: "conv".to_owned(),
            title: "Rate limits".to_owned(),
            created: Some("2026-04-15T09:41:00Z".to_owned()),
            models: Vec::new(),
            messages: vec![super::Message {
                id: "m1".to_owned(),
                role: "user".to_owned(),
                created: None,
                model: None,
                text: "What about the rate limits?".to_owned(),
                attachments: Vec::new(),
            }],
        };
        let keeps = |since: Option<&str>, until: Option<&str>, keywords: &[&str], ids: &[&str]| {
            Selection {
                since: since.map(str::to_owned),
                until: until.map(str::to_owned),
                keywords: keywords.iter().map(|k| (*k).to_owned()).collect(),
                ids: ids.iter().map(|i| (*i).to_owned()).collect(),
            }
            .keeps(&conversation)
        };
        assert!(keeps(None, None, &[], &[]));
        assert!(
            keeps(Some("2026-04-15"), None, &[], &[]),
            "the day is inclusive"
        );
        assert!(!keeps(Some("2026-04-16"), None, &[], &[]));
        assert!(!keeps(None, Some("2026-04-14"), &[], &[]));
        assert!(keeps(None, None, &["RATE LIMITS"], &[]), "case-insensitive");
        assert!(!keeps(None, None, &["weekend"], &[]));
        assert!(
            keeps(None, None, &["weekend", "limits"], &[]),
            "any match keeps"
        );
        assert!(keeps(None, None, &[], &["conv"]));
        assert!(!keeps(None, None, &[], &["other"]));
    }

    /// A message too big for half a part is cut on a char boundary, with the
    /// marker where its tail went; small text is untouched.
    #[test]
    fn an_oversize_message_is_truncated_with_a_marker() {
        let mut long = "é".repeat(400_000);
        long.push_str(" the very end");
        let conversation = super::Conversation {
            id: "big".to_owned(),
            title: "big".to_owned(),
            created: None,
            models: Vec::new(),
            messages: vec![
                super::Message {
                    id: "m1".to_owned(),
                    role: "user".to_owned(),
                    created: None,
                    model: None,
                    text: long,
                    attachments: Vec::new(),
                },
                super::Message {
                    id: "m2".to_owned(),
                    role: "assistant".to_owned(),
                    created: None,
                    model: None,
                    text: "short".to_owned(),
                    attachments: Vec::new(),
                },
            ],
        };
        let parts = render(Kind::Claude, &conversation);
        assert_eq!(parts.len(), 1);
        let body = &parts[0].body;
        assert!(body.contains("[truncated]"), "the loss is visible");
        assert!(!body.contains("the very end"), "the tail is gone");
        assert!(body.contains("conversation:big#m2"));
        assert!(body.len() <= super::MAX_PART_BYTES);
        // Both messages' sections are present, so nothing but the tail left.
        assert_eq!(floor_char_boundary("héllo", 1), 1);
        assert_eq!(floor_char_boundary("héllo", 2), 1);
        assert_eq!(floor_char_boundary("héllo", 99), 6);
    }
}
