//! The readable side of a synced session: one note per session, filed
//! under the session's project.
//!
//! A source in the ledger is not a page — nothing extracts sources into
//! pages yet — so an uploaded session is invisible to `search` and `ask`.
//! Beside each session's source, `logs sync` therefore writes a note through
//! `POST /v1/notes`: who, where, when, with what, at what cost, and the
//! conversation itself — every message, reasoning summary and event in
//! order, and each tool call as a compact block (its command or patch
//! fenced, its result clipped to head and tail). The notes route sits under
//! the 64 KiB request limit, so a long session's page quotes less of each
//! call, and past that keeps its opening and its most recent turns and links
//! the full transcript in the web app's Agent sessions view. The note carries
//! `project: <repo basename>` in its frontmatter, which is what `livingbrain
//! search --project <repo>` and `livingbrain ask --project <repo>` filter on.
//!
//! The note is keyed: its slug is `agent-log-<agent>-<session-id>`, so a
//! re-sync updates the one page in place, and a re-sync of an unchanged
//! session sends the same markdown and the server writes nothing. It is
//! built from the record the source was built from — at the repository's
//! detail level, home-elided, redacted value by value — and redacted again
//! here, on this machine, before it is sent.

use std::collections::BTreeMap;
use std::path::Path;

use livingbrain_redact::{Policy, redact};
use serde_json::{Value, json};

use super::clean::clip;
use super::{Role, SessionRecord, Turn};
use crate::api::Client;

/// The most the note's markdown may be once escaped into the JSON request
/// that carries it. The notes route sits under the harness's 64 KiB request
/// limit; this leaves room for the title, slug, project and any growth
/// redaction's replacement tokens cause.
const NOTE_BUDGET_BYTES: usize = 48 * 1024;

/// How much of each message, tool input and tool result the note quotes
/// (head and tail), tried in order until the whole conversation fits the
/// page: generous first, then commands with short results, then commands
/// alone. The page is for reading and finding; the transcript view has
/// everything.
const NOTE_LIMITS: [Limits; 5] = [
    Limits::new(6 * 1024, 1536, 1024),
    Limits::new(4 * 1024, 1024, 512),
    Limits::new(3 * 1024, 768, 256),
    Limits::new(2 * 1024, 512, 128),
    Limits::new(2 * 1024, 384, 0),
];

/// One rung of [`NOTE_LIMITS`], in bytes; an `output` of 0 shows a call's
/// outcome without its result.
#[derive(Debug, Clone, Copy)]
struct Limits {
    text: usize,
    input: usize,
    output: usize,
}

impl Limits {
    const fn new(text: usize, input: usize, output: usize) -> Self {
        Self {
            text,
            input,
            output,
        }
    }
}

/// The slug prefix every session note is filed under.
const SLUG_PREFIX: &str = "agent-log";

/// What `logs sync` intends for one session's note, decided locally and
/// before any socket: the page it lands on and the project it is filed
/// under. `None` on [`super::Prepared`] means no note — the session
/// recorded no repository to file it under.
#[derive(Debug)]
pub(super) struct Planned {
    pub(super) slug: String,
    pub(super) project: String,
    pub(super) title: String,
    /// What became of the write; `None` until one was attempted.
    pub(super) outcome: Option<Outcome>,
}

/// One note write's result.
#[derive(Debug)]
pub(super) enum Outcome {
    /// A new page, or a new version of the session's page.
    Written { id: String, url: String },
    /// The page already said exactly this; nothing was written.
    Unchanged { id: String, url: String },
    /// The write was refused (a hand-edited page, say). The session's
    /// source is in the ledger regardless; the note is retried next sync.
    Failed(String),
}

/// The note one session would get: `None` when the record names no
/// repository to file it under.
pub(super) fn plan(record: &SessionRecord) -> Option<Planned> {
    let project = project_of(&record.repo)?;
    Some(Planned {
        slug: slug_for(record.agent, &record.session_id),
        project,
        title: format!("{} session {}", record.agent, record.session_id),
        outcome: None,
    })
}

/// The project a session is filed under: the basename of its repository,
/// redacted like everything else that leaves. A repo of `~` or `/` names no
/// project.
fn project_of(repo: &str) -> Option<String> {
    let base = Path::new(repo.trim()).file_name()?.to_string_lossy();
    let base = base.trim();
    if base.is_empty() || base == "~" {
        return None;
    }
    let clean = redact(base, Policy::Redact).ok()?.0;
    let clean = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    (!clean.is_empty()).then_some(clean)
}

/// `agent-log-<agent>-<session-id>`, folded onto the slug rule (lowercase
/// ascii letters, digits, `-`; at most 128 bytes).
pub(super) fn slug_for(agent: &str, session_id: &str) -> String {
    let raw = format!("{SLUG_PREFIX}-{agent}-{session_id}").to_ascii_lowercase();
    let mut slug = String::with_capacity(raw.len());
    for ch in raw.chars() {
        let ch = if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            ch
        } else {
            '-'
        };
        if ch == '-' && slug.ends_with('-') {
            continue;
        }
        slug.push(ch);
    }
    slug.truncate(128);
    slug.trim_end_matches('-').to_owned()
}

/// The note's body, redacted on this machine. `source` is the ledger id the
/// upload answered with, when it answered with one.
///
/// The wiki-link brackets are defused last, after redaction: a page body's
/// `[[target]]` must name a slug or the store refuses the page, and an
/// agent's `[[ -f x ]]` is shell — as is a bracketed secret that redaction
/// turned into `[[REDACTED:…]]`.
pub(super) fn body(record: &SessionRecord, source: Option<&str>) -> Result<String, String> {
    let text = render(record, source);
    redact(&text, Policy::Redact)
        .map(|(clean, _)| defuse_links(&clean))
        .map_err(|error| error.to_string())
}

/// A space between any two adjacent `[` or `]`, so nothing in the text
/// parses as a wiki-link.
fn defuse_links(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut previous = None;
    for ch in text.chars() {
        if matches!(ch, '[' | ']') && previous == Some(ch) {
            out.push(' ');
        }
        out.push(ch);
        previous = Some(ch);
    }
    out
}

/// The note's markdown, before redaction. Everything here is a pure
/// function of the record, so the same session renders the same bytes and
/// the server's unchanged check holds.
///
/// The whole conversation, in order, when it fits [`NOTE_BUDGET_BYTES`]:
/// messages in full, reasoning summaries quoted, events as a line, and each
/// tool call as a compact block — its input fenced, its result fenced and
/// clipped to head and tail. A conversation past the budget keeps its
/// opening and its most recent stretch, with a line saying how many turns
/// sit between them and where the whole transcript is.
fn render(record: &SessionRecord, source: Option<&str>) -> String {
    let mut out = header(record, source);
    out.push_str("\n## Conversation\n\n");
    let budget = NOTE_BUDGET_BYTES.saturating_sub(escaped_len(&out));
    let mut blocks: Vec<String> = Vec::new();
    let mut sizes: Vec<usize> = Vec::new();
    for limits in NOTE_LIMITS {
        blocks = record
            .turns
            .iter()
            .map(|turn| turn_block(turn, limits))
            .filter(|block| !block.is_empty())
            .collect();
        sizes = blocks.iter().map(|block| escaped_len(block)).collect();
        if sizes.iter().sum::<usize>() <= budget {
            for block in &blocks {
                out.push_str(block);
            }
            if blocks.is_empty() {
                out.push_str("_No text turns._\n");
            }
            return out;
        }
    }
    // The opening third, then as much of the end as the rest holds.
    let marker_room = 512;
    let head_budget = budget / 3;
    let mut used = 0usize;
    let mut head = 0usize;
    while head < blocks.len() && used + sizes[head] <= head_budget {
        used += sizes[head];
        head += 1;
    }
    let mut tail = blocks.len();
    while tail > head && used + sizes[tail - 1] + marker_room <= budget {
        used += sizes[tail - 1];
        tail -= 1;
    }
    for block in &blocks[..head] {
        out.push_str(block);
    }
    let hidden = tail - head;
    if hidden > 0 {
        let link = match source.filter(|source| !source.is_empty()) {
            Some(source) => format!(
                "open the [full transcript](wiki.html?view=sessions&source={source}) in Agent sessions"
            ),
            None => "the full transcript is in the session's source (Agent sessions)".to_owned(),
        };
        out.push_str(&format!(
            "_… {hidden} turn(s) not shown on this page — {link}._\n\n"
        ));
    }
    for block in &blocks[tail..] {
        out.push_str(block);
    }
    out
}

/// The stats at the top of the note.
fn header(record: &SessionRecord, source: Option<&str>) -> String {
    let mut out = String::new();
    let project = project_of(&record.repo).unwrap_or_default();
    out.push_str(&format!(
        "Coding agent session: {} in {project}.\n\n",
        record.agent
    ));
    out.push_str(&format!("- Agent: {}\n", record.agent));
    out.push_str(&format!("- Session: {}\n", record.session_id));
    out.push_str(&format!("- Repo: {}\n", record.repo));
    if let Some(branch) = &record.branch {
        out.push_str(&format!("- Branch: {}\n", one_line(branch)));
    }
    match (&record.started_at, &record.ended_at) {
        (Some(start), Some(end)) if start != end => {
            out.push_str(&format!("- When: {start} to {end}\n"));
        }
        (Some(at), _) | (None, Some(at)) => out.push_str(&format!("- When: {at}\n")),
        (None, None) => {}
    }
    if !record.models.is_empty() {
        out.push_str(&format!("- Models: {}\n", record.models.join(", ")));
    }
    let count = |role: Role| record.turns.iter().filter(|t| t.role == role).count();
    out.push_str(&format!(
        "- Messages: {} user, {} assistant\n",
        count(Role::User),
        record
            .turns
            .iter()
            .filter(|t| t.role == Role::Assistant && !t.text.trim().is_empty())
            .count(),
    ));
    out.push_str(&format!("- Tool calls: {}\n", tool_summary(&record.turns)));
    let totals = &record.totals;
    out.push_str(&format!(
        "- Tokens: {} in, {} out, {} cache read, {} cache write, {} reasoning\n",
        totals.input_tokens,
        totals.output_tokens,
        totals.cache_read_tokens,
        totals.cache_creation_tokens,
        totals.reasoning_tokens,
    ));
    if let Some(cost) = totals.cost_usd {
        out.push_str(&format!("- Cost: ${cost:.4}\n"));
    }
    out.push_str(&format!("- Detail: {}\n", record.detail.as_str()));
    if let Some(source) = source.filter(|source| !source.is_empty()) {
        out.push_str(&format!("- Source: {source} (agent_log)\n"));
        out.push_str(&format!(
            "- Transcript: [open in Agent sessions](wiki.html?view=sessions&source={source})\n"
        ));
    }
    if record.truncated {
        out.push_str(&format!(
            "- Truncated: {} turn(s) removed to fit the upload size cap\n",
            record.omitted_turns
        ));
    }
    out
}

/// One turn as markdown, ending in a blank line; `""` for a turn with
/// nothing to show.
fn turn_block(turn: &Turn, limits: Limits) -> String {
    let at = turn.at.as_deref().map(short_time).unwrap_or_default();
    let stamp = if at.is_empty() {
        String::new()
    } else {
        format!(" · {at}")
    };
    let mut out = String::new();
    match turn.role {
        Role::User => {
            if turn.text.trim().is_empty() {
                return String::new();
            }
            out.push_str(&format!("**User**{stamp}\n\n"));
            out.push_str(&message(&turn.text, limits.text));
        }
        Role::Assistant => {
            if !turn.text.trim().is_empty() {
                out.push_str(&format!("**Assistant**{stamp}\n\n"));
                out.push_str(&message(&turn.text, limits.text));
            }
            for call in &turn.tool_calls {
                out.push_str(&call_block(call, limits));
            }
            if out.is_empty() {
                return out;
            }
        }
        Role::Reasoning => {
            let text = clip(turn.text.trim(), limits.text);
            out.push_str(&format!("_Reasoning{stamp}_\n\n"));
            for line in text.lines() {
                out.push_str(&format!("> {line}\n"));
            }
            out.push('\n');
        }
        Role::Event => {
            out.push_str(&format!("_— {}{stamp} —_\n\n", one_line(&turn.text)));
        }
    }
    out
}

/// A message's text as markdown: clipped for the page, and with any code
/// fence it opened closed again, so it cannot swallow what follows.
fn message(text: &str, max: usize) -> String {
    let mut text = clip(text.trim(), max);
    let fences = text
        .lines()
        .filter(|line| line.trim_start().starts_with("```"))
        .count();
    if fences % 2 == 1 {
        text.push_str("\n```");
    }
    format!("{text}\n\n")
}

/// One tool call: its name, file and outcome on a line, then the input and
/// the result each in a fence.
fn call_block(call: &super::ToolCall, limits: Limits) -> String {
    let mut line = format!("- `{}`", call.name.replace('`', "'"));
    if let Some(path) = &call.path {
        line.push_str(&format!(" `{}`", one_line(path).replace('`', "'")));
    }
    if let Some(status) = call.status {
        line.push_str(&format!(" · {status}"));
    }
    if let Some(code) = call.exit_code {
        line.push_str(&format!(" · exit {code}"));
    }
    let mut out = format!("{line}\n\n");
    if let Some(input) = &call.input {
        let lang = match call.format {
            Some("shell") => "sh",
            Some(other) => other,
            None => "text",
        };
        out.push_str(&fenced(lang, &clip(input, limits.input)));
    }
    if let Some(output) = call.output.as_ref().filter(|_| limits.output > 0) {
        out.push_str(&fenced("text", &clip(output, limits.output)));
    }
    out
}

/// `body` in a fence, every line of it that could close the fence defused
/// with a zero-width space in front.
fn fenced(lang: &str, body: &str) -> String {
    let mut out = format!("```{lang}\n");
    for line in body.trim_end_matches('\n').lines() {
        if line.trim_start().starts_with("```") {
            out.push('\u{200b}');
        }
        out.push_str(line);
        out.push('\n');
    }
    out.push_str("```\n\n");
    out
}

/// `2026-10-01T09:30:05.123Z` as `2026-10-01 09:30:05`; anything else as is.
fn short_time(at: &str) -> String {
    let at = at.trim_end_matches('Z');
    let at = at.split('.').next().unwrap_or(at);
    at.replacen('T', " ", 1)
}

/// How long `text` is once escaped into the JSON request it travels in.
fn escaped_len(text: &str) -> usize {
    serde_json::to_string(text).map_or(text.len(), |escaped| escaped.len())
}

/// `N (Name ×k, …)`, most-called first, ties by name.
fn tool_summary(turns: &[Turn]) -> String {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for call in turns.iter().flat_map(|turn| &turn.tool_calls) {
        *counts.entry(call.name.as_str()).or_default() += 1;
    }
    let total: usize = counts.values().sum();
    if total == 0 {
        return "0".to_owned();
    }
    let mut ranked: Vec<(&str, usize)> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let names: Vec<String> = ranked
        .iter()
        .map(|(name, count)| format!("{} ×{count}", one_line(name)))
        .collect();
    format!("{total} ({})", names.join(", "))
}

/// Whitespace collapsed to single spaces.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Write one session's planned note. A refusal is recorded, not raised:
/// the session's source is already in the ledger, and the next sync tries
/// the note again.
pub(super) fn write(
    client: &Client,
    record: &SessionRecord,
    planned: &mut Planned,
    source: Option<&str>,
) {
    let outcome = match body(record, source) {
        Err(error) => Outcome::Failed(error),
        Ok(text) => {
            match client.keyed_note(&planned.slug, &planned.title, &text, Some(&planned.project)) {
                Err(error) => Outcome::Failed(error.to_string()),
                Ok(value) => {
                    let id = crate::text(&value, "id").to_owned();
                    let url = crate::text(&value, "url").to_owned();
                    if value.get("unchanged").and_then(Value::as_bool) == Some(true) {
                        Outcome::Unchanged { id, url }
                    } else {
                        Outcome::Written { id, url }
                    }
                }
            }
        }
    };
    planned.outcome = Some(outcome);
}

/// The note's entry in the `--json` report, and its status word:
/// `would_write` (dry run), `written`, `unchanged`, `failed`, `no_repo`, or
/// `not_written` (an upload before it failed).
pub(super) fn report(planned: Option<&Planned>, dry_run: bool) -> (Value, &'static str) {
    let Some(planned) = planned else {
        // An opted-in session that recorded no repository has no project
        // to be filed under, so it gets no note.
        return (json!(null), "no_repo");
    };
    let mut entry = json!({
        "slug": planned.slug,
        "project": planned.project,
    });
    let status = match (&planned.outcome, dry_run) {
        (None, true) => "would_write",
        (None, false) => "not_written",
        (Some(Outcome::Written { id, url }), _) => {
            entry["id"] = json!(id);
            entry["url"] = json!(url);
            "written"
        }
        (Some(Outcome::Unchanged { id, url }), _) => {
            entry["id"] = json!(id);
            entry["url"] = json!(url);
            "unchanged"
        }
        (Some(Outcome::Failed(error)), _) => {
            entry["error"] = json!(error);
            "failed"
        }
    };
    entry["status"] = json!(status);
    (entry, status)
}

/// The human report's line for a note that failed, if this one did.
pub(super) fn failure_line(planned: Option<&Planned>) -> Option<String> {
    match planned?.outcome.as_ref()? {
        Outcome::Failed(error) => Some(format!("note {} not written: {error}", planned?.slug)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{defuse_links, fenced, message, project_of, short_time, slug_for};

    #[test]
    fn the_slug_is_the_agent_and_the_session_folded_onto_the_slug_rule() {
        assert_eq!(
            slug_for("claude_code", "3F2A9C1E-5b7d"),
            "agent-log-claude-code-3f2a9c1e-5b7d"
        );
        assert_eq!(slug_for("codex", "a/b c__d"), "agent-log-codex-a-b-c-d");
        assert!(slug_for("codex", &"x".repeat(300)).len() <= 128);
    }

    #[test]
    fn the_project_is_the_repo_basename() {
        assert_eq!(project_of("~/code/demo").as_deref(), Some("demo"));
        assert_eq!(
            project_of("/srv/livingbrain/").as_deref(),
            Some("livingbrain")
        );
        assert_eq!(project_of("~"), None);
        assert_eq!(project_of(""), None);
        assert_eq!(project_of("/"), None);
    }

    #[test]
    fn fences_cannot_be_broken_out_of() {
        let block = fenced("text", "a\n```\nb");
        assert_eq!(block, "```text\na\n\u{200b}```\nb\n```\n\n");
        assert_eq!(
            message("see\n```rust\nfn x()", 100),
            "see\n```rust\nfn x()\n```\n\n"
        );
        assert_eq!(message("plain", 100), "plain\n\n");
    }

    #[test]
    fn times_read_short() {
        assert_eq!(
            short_time("2026-10-01T09:30:05.123Z"),
            "2026-10-01 09:30:05"
        );
        assert_eq!(short_time("yesterday"), "yesterday");
    }

    #[test]
    fn nothing_survives_as_a_wiki_link() {
        assert_eq!(defuse_links("if [[ -f x ]]"), "if [ [ -f x ] ]");
        assert_eq!(defuse_links("[[[a]]]"), "[ [ [a] ] ]");
        assert_eq!(defuse_links("[REDACTED:email]"), "[REDACTED:email]");
    }
}
