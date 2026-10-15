//! The searchable side of a synced session: one note per session, filed
//! under the session's project.
//!
//! A source in the ledger is not a page — nothing extracts sources into
//! pages yet — so an uploaded session is invisible to `search` and `ask`.
//! Beside each session's source, `logs sync` therefore writes a compact
//! note through `POST /v1/notes`: who, where, when, with what, at what
//! cost, and the conversation excerpted. The note carries `project: <repo
//! basename>` in its frontmatter, which is what `livingbrain search
//! --project <repo>` and `livingbrain ask --project <repo>` filter on.
//!
//! The note is keyed: its slug is `agent-log-<agent>-<session-id>`, so a
//! re-sync updates the one page in place, and a re-sync of an unchanged
//! session sends the same markdown and the server writes nothing. It is
//! built from the record the source was built from — home-elided, capped,
//! tool names only — and redacted here, on this machine, before it is sent.

use std::collections::BTreeMap;
use std::path::Path;

use livingbrain_redact::{Policy, redact};
use serde_json::{Value, json};

use super::{SessionRecord, Turn};
use crate::api::Client;

/// How much of one turn's text the note quotes, in characters.
const NOTE_TURN_CHARS: usize = 400;

/// The budget for the excerpted conversation, in bytes. Far under the
/// notes route's ceiling (redaction's 1 MiB input cap): a note is a
/// summary to find the session by, and the full record is in the source.
const NOTE_EXCERPT_BYTES: usize = 12 * 1024;

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
fn render(record: &SessionRecord, source: Option<&str>) -> String {
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
    let users = record.turns.iter().filter(|t| t.role == "user").count();
    out.push_str(&format!(
        "- Turns: {} ({users} user, {} assistant)\n",
        record.turns.len(),
        record.turns.len() - users,
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
    if let Some(source) = source.filter(|source| !source.is_empty()) {
        out.push_str(&format!("- Source: {source} (agent_log)\n"));
    }
    if record.truncated {
        out.push_str("- Truncated: the record lost trailing turns to the size cap\n");
    }

    out.push_str("\n## Conversation\n\n");
    let spoken: Vec<&Turn> = record
        .turns
        .iter()
        .filter(|turn| !turn.text.trim().is_empty())
        .collect();
    let mut used = 0usize;
    let mut shown = 0usize;
    for turn in &spoken {
        let speaker = if turn.role == "user" {
            "User"
        } else {
            "Assistant"
        };
        let line = format!("**{speaker}:** {}\n\n", excerpt(&turn.text));
        if used + line.len() > NOTE_EXCERPT_BYTES {
            break;
        }
        used += line.len();
        shown += 1;
        out.push_str(&line);
    }
    if spoken.is_empty() {
        out.push_str("_No text turns._\n");
    } else if shown < spoken.len() {
        out.push_str(&format!(
            "_{} more turn(s) not shown; the full record is the source._\n",
            spoken.len() - shown
        ));
    }
    out
}

/// `N (Name ×k, …)`, most-called first, ties by name: the shape of the work
/// without any of its inputs.
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

/// One turn's text as a single line, cut to [`NOTE_TURN_CHARS`] — a
/// single line, so no turn can open a heading or a frontmatter fence.
fn excerpt(text: &str) -> String {
    let joined = one_line(text);
    let mut out: String = joined.chars().take(NOTE_TURN_CHARS).collect();
    if joined.chars().nth(NOTE_TURN_CHARS).is_some() {
        out.push('…');
    }
    out
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
    use super::{defuse_links, excerpt, project_of, slug_for};

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
    fn an_excerpt_is_one_capped_line() {
        assert_eq!(excerpt("if [[ -f x ]];\n\nthen"), "if [[ -f x ]]; then");
        let long = excerpt(&"a".repeat(1000));
        assert_eq!(long.chars().count(), super::NOTE_TURN_CHARS + 1);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn nothing_survives_as_a_wiki_link() {
        assert_eq!(defuse_links("if [[ -f x ]]"), "if [ [ -f x ] ]");
        assert_eq!(defuse_links("[[[a]]]"), "[ [ [a] ] ]");
        assert_eq!(defuse_links("[REDACTED:email]"), "[REDACTED:email]");
    }
}
