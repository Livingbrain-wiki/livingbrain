//! What a page is called, and which project it belongs to, as a person
//! reads it.
//!
//! Two rules live here because every reader needs the same answer:
//!
//! - **Session titles.** A coding-agent session's note (`agent-log-<agent>-
//!   <id>`) arrives titled `claude_code session 3f2a…` — an id, not a name.
//!   [`session_title`] names it from what the note itself says: the project,
//!   the agent, the day and the first thing the person asked; for a Colonizer
//!   colony, the issue it worked on, else its pull request, else its outcome.
//!   It is a pure function of the note, so a re-sync names the same session
//!   the same way and the store's unchanged check still holds.
//! - **Project keys.** A project is spelled `EarthOS.world` by the
//!   repository, `earthos-world` by the compiler's folder and by a local
//!   clone, and the two must be one project. [`project_key`] folds any
//!   spelling onto the compiler's folder name (`livingbrain-compiler`'s
//!   `layout::segment`): lowercase ASCII letters and digits, every other run
//!   one `-`, at most 28 bytes. The key matches; the original spelling is
//!   what a person reads.

use crate::entity::Frontmatter;

/// The slug prefix every session note is filed under.
pub const SESSION_SLUG_PREFIX: &str = "agent-log-";

/// The most a project key takes: the compiler's segment cap.
const MAX_KEY_LEN: usize = 28;

/// The most characters a quoted prompt or issue title adds to a title.
const MAX_SUMMARY_CHARS: usize = 60;

/// Whether `slug` is a coding-agent session's note.
#[must_use]
pub fn is_session_slug(slug: &str) -> bool {
    slug.len() > SESSION_SLUG_PREFIX.len() && slug.starts_with(SESSION_SLUG_PREFIX)
}

/// A project's canonical key: the compiler's folder name for it. `None`
/// when nothing survives the fold.
#[must_use]
pub fn project_key(name: &str) -> Option<String> {
    let mut out = String::new();
    let mut dash = false;
    for ch in name.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_lowercase() || lower.is_ascii_digit() {
            if dash && !out.is_empty() {
                out.push('-');
            }
            dash = false;
            out.push(lower);
        } else {
            dash = true;
        }
        if out.len() >= MAX_KEY_LEN {
            break;
        }
    }
    let out = out.trim_matches('-').to_owned();
    (!out.is_empty()).then_some(out)
}

/// The display spelling to prefer between two names of one project: the
/// one that is not already folded (`EarthOS.world` over `earthos-world`),
/// so a person reads the repository's own name.
#[must_use]
pub fn prefer_display<'a>(have: &'a str, other: &'a str) -> &'a str {
    let folded = |name: &str| project_key(name).as_deref() == Some(name);
    match (folded(have), folded(other)) {
        (true, false) => other,
        _ => have,
    }
}

/// A page's title by the shared rule: `name`, then `title`, then `term` —
/// the key each entity type requires — or `None` when the fence has none.
#[must_use]
pub fn frontmatter_title(frontmatter: &Frontmatter) -> Option<String> {
    ["name", "title", "term"]
        .iter()
        .filter_map(|key| frontmatter.get(key))
        .map(str::trim)
        .find(|value| !value.is_empty())
        .map(one_line)
}

/// Whether a session note's title is the id-only placeholder a client
/// sends (`claude_code session 3f2a…`, `colonizer session a15f…`), the
/// notes route's `Note`, or nothing — a title worth replacing.
#[must_use]
pub fn is_placeholder_title(title: &str, slug: &str) -> bool {
    let title = title.trim();
    if title.is_empty() || title == "Note" || title == slug {
        return true;
    }
    let words: Vec<&str> = title.split_whitespace().collect();
    matches!(words.as_slice(), [_, "session", _])
}

/// What a session note's header says, read back from its markdown: the
/// lines `livingbrain logs sync` and Colonizer both write (`- Agent: …`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Header {
    agent: Option<String>,
    session: Option<String>,
    issue: Option<String>,
    pull_request: Option<String>,
    outcome: Option<String>,
    when: Option<String>,
    first_prompt: Option<String>,
}

fn header(markdown: &str) -> Header {
    let mut out = Header::default();
    let mut lines = markdown.lines().peekable();
    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        let field = |name: &str| {
            trimmed
                .strip_prefix("- ")
                .and_then(|rest| rest.strip_prefix(name))
                .and_then(|rest| rest.strip_prefix(':'))
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        };
        if out.agent.is_none()
            && let Some(value) = field("Agent")
        {
            out.agent = Some(value);
        } else if out.session.is_none()
            && let Some(value) = field("Session")
        {
            out.session = Some(value);
        } else if out.issue.is_none()
            && let Some(value) = field("Issue")
        {
            out.issue = Some(value);
        } else if out.pull_request.is_none()
            && let Some(value) = field("Pull request")
        {
            out.pull_request = Some(value);
        } else if out.outcome.is_none()
            && let Some(value) = field("Outcome")
        {
            out.outcome = Some(value);
        } else if out.when.is_none()
            && let Some(value) = field("When")
        {
            out.when = Some(value);
        } else if out.first_prompt.is_none() && trimmed.starts_with("**User**") {
            // The turn's text follows the label after a blank line; the
            // first line of it that reads as prose is the summary.
            let mut inside_fence = false;
            for next in lines.by_ref() {
                let text = next.trim();
                if text.starts_with("**Assistant**") || text.starts_with("**User**") {
                    break;
                }
                if text.starts_with("```") {
                    inside_fence = !inside_fence;
                    continue;
                }
                if inside_fence || text.is_empty() || text.starts_with('<') || text.starts_with('>')
                {
                    continue;
                }
                let prose = text.trim_start_matches(['#', '-', '*', ' ']).trim();
                if !prose.is_empty() {
                    out.first_prompt = Some(prose.to_owned());
                    break;
                }
            }
        }
    }
    out
}

/// What a person calls an agent.
#[must_use]
pub fn agent_label(agent: &str) -> String {
    match agent.trim() {
        "claude_code" | "claude-code" | "claude" => "Claude Code".to_owned(),
        "codex" => "Codex".to_owned(),
        "colonizer" => "Colonizer".to_owned(),
        "" => "Agent".to_owned(),
        other => other.to_owned(),
    }
}

/// The agent a session slug names, when the header did not.
fn agent_of_slug(slug: &str) -> Option<&'static str> {
    let rest = slug.strip_prefix(SESSION_SLUG_PREFIX)?;
    ["claude-code-", "codex-", "colonizer-"]
        .iter()
        .find(|prefix| rest.starts_with(**prefix))
        .map(|prefix| match *prefix {
            "claude-code-" => "claude_code",
            "codex-" => "codex",
            _ => "colonizer",
        })
}

/// The id a session slug ends in, when the header named none.
fn id_of_slug(slug: &str) -> String {
    let rest = slug.strip_prefix(SESSION_SLUG_PREFIX).unwrap_or(slug);
    for prefix in ["claude-code-", "codex-", "colonizer-"] {
        if let Some(id) = rest.strip_prefix(prefix) {
            return id.to_owned();
        }
    }
    rest.to_owned()
}

/// A session note's title, from the note itself.
///
/// - A Colonizer colony: `EarthOS.world · Colony a15fba01 · #123 Fix auth
///   redirect` — the issue it worked on, else `PR #45`, else its outcome.
/// - Any other agent: `EarthOS.world · Claude Code · 2026-10-15 · Fix the
///   login redirect loop` — the day it started and the first thing the
///   person asked, cut to 60 characters.
///
/// A part the note does not carry is left out rather than guessed.
#[must_use]
pub fn session_title(slug: &str, project: Option<&str>, markdown: &str) -> String {
    let head = header(markdown);
    let agent = head
        .agent
        .clone()
        .or_else(|| agent_of_slug(slug).map(str::to_owned))
        .unwrap_or_default();
    let mut parts: Vec<String> = Vec::new();
    if let Some(project) = project.map(str::trim).filter(|p| !p.is_empty()) {
        parts.push(one_line(project));
    }
    if agent == "colonizer" {
        let id = head.session.clone().unwrap_or_else(|| id_of_slug(slug));
        let short: String = id.chars().take(8).collect();
        parts.push(format!("Colony {short}"));
        if let Some(issue) = &head.issue {
            parts.push(clip(issue, MAX_SUMMARY_CHARS + 8));
        } else if let Some(number) = head.pull_request.as_deref().and_then(pull_number) {
            parts.push(format!("PR #{number}"));
        } else if let Some(outcome) = &head.outcome {
            parts.push(clip(outcome, MAX_SUMMARY_CHARS));
        }
    } else {
        parts.push(agent_label(&agent));
        if let Some(day) = head.when.as_deref().and_then(day_of) {
            parts.push(day);
        }
        if let Some(prompt) = &head.first_prompt {
            parts.push(clip(prompt, MAX_SUMMARY_CHARS));
        }
    }
    one_line(&parts.join(" · "))
}

/// `2026-10-15` from the start of a `When:` value.
fn day_of(when: &str) -> Option<String> {
    let day: String = when.trim().chars().take(10).collect();
    let bytes = day.as_bytes();
    (bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && day
            .chars()
            .enumerate()
            .all(|(at, ch)| at == 4 || at == 7 || ch.is_ascii_digit()))
    .then_some(day)
}

/// The number of a pull request URL (`…/pull/45`).
fn pull_number(url: &str) -> Option<String> {
    let after = url.split("/pull/").nth(1)?;
    let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
    (!digits.is_empty()).then_some(digits)
}

/// Whitespace collapsed to single spaces: a title is one line.
fn one_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// At most `max` characters, cut at a word where one is near, with `…`.
fn clip(value: &str, max: usize) -> String {
    let value = one_line(value);
    if value.chars().count() <= max {
        return value;
    }
    let cut: String = value.chars().take(max).collect();
    let cut = match cut.rfind(' ') {
        Some(at) if at > max / 2 => cut[..at].to_owned(),
        _ => cut,
    };
    format!("{}…", cut.trim_end_matches([' ', ',', '.', ':', ';', '-']))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE: &str = "Coding agent session: claude_code in EarthOS.world.\n\n\
        - Agent: claude_code\n- Session: 3f2a9c1e-5b7d\n- Repo: ~/src/EarthOS.world\n\
        - When: 2026-10-15T08:01:02Z to 2026-10-15T09:00:00Z\n- Messages: 2 user, 3 assistant\n\n\
        ## Conversation\n\n**User** · 08:01\n\n<command-name>/clear</command-name>\n\
        Fix the login redirect loop that sends people back to /signin after they authenticate\n\n\
        **Assistant** · 08:02\n\nLooking.\n";

    const COLONY: &str = "Coding agent session: colonizer in EarthOS.world.\n\n\
        - Agent: colonizer\n- Session: a15fba01-77c2-4e0e\n- Repo: World-360/EarthOS.world\n\
        - Issue: #123 Fix auth redirect\n- Pull request: https://github.com/World-360/EarthOS.world/pull/130\n\
        - Outcome: merged\n- When: 2026-10-14T10:00:00Z\n";

    #[test]
    fn a_claude_session_is_named_for_its_project_day_and_first_ask() {
        let title = session_title(
            "agent-log-claude-code-3f2a9c1e-5b7d",
            Some("EarthOS.world"),
            CLAUDE,
        );
        assert_eq!(
            title,
            "EarthOS.world · Claude Code · 2026-10-15 · Fix the login redirect loop that sends people back to…"
        );
    }

    #[test]
    fn a_codex_session_reads_the_same_way() {
        let body = CLAUDE.replace("claude_code", "codex");
        let title = session_title("agent-log-codex-3f2a", Some("livingbrain"), &body);
        assert!(
            title.starts_with("livingbrain · Codex · 2026-10-15 · Fix the login"),
            "{title}"
        );
    }

    #[test]
    fn a_colony_is_named_for_its_issue_then_pr_then_outcome() {
        let slug = "agent-log-colonizer-a15fba01-77c2-4e0e";
        assert_eq!(
            session_title(slug, Some("EarthOS.world"), COLONY),
            "EarthOS.world · Colony a15fba01 · #123 Fix auth redirect"
        );
        let no_issue = COLONY.replace("- Issue: #123 Fix auth redirect\n", "");
        assert_eq!(
            session_title(slug, Some("EarthOS.world"), &no_issue),
            "EarthOS.world · Colony a15fba01 · PR #130"
        );
        let bare = no_issue.replace(
            "- Pull request: https://github.com/World-360/EarthOS.world/pull/130\n",
            "",
        );
        assert_eq!(
            session_title(slug, Some("EarthOS.world"), &bare),
            "EarthOS.world · Colony a15fba01 · merged"
        );
    }

    #[test]
    fn a_note_with_no_header_still_gets_a_name_from_its_slug() {
        assert_eq!(
            session_title("agent-log-colonizer-a15fba01", None, "just text"),
            "Colony a15fba01"
        );
        assert_eq!(session_title("agent-log-codex-x", None, ""), "Codex");
    }

    #[test]
    fn placeholders_are_told_apart_from_real_titles() {
        let slug = "agent-log-colonizer-a15fba01";
        assert!(is_placeholder_title("colonizer session a15fba01", slug));
        assert!(is_placeholder_title("claude_code session 3f2a", slug));
        assert!(is_placeholder_title("Note", slug));
        assert!(is_placeholder_title("", slug));
        assert!(is_placeholder_title(slug, slug));
        assert!(!is_placeholder_title(
            "EarthOS.world · Colony a15fba01",
            slug
        ));
    }

    #[test]
    fn project_keys_fold_every_spelling_onto_one() {
        assert_eq!(
            project_key("EarthOS.world").as_deref(),
            Some("earthos-world")
        );
        assert_eq!(
            project_key("earthos-world").as_deref(),
            Some("earthos-world")
        );
        assert_eq!(project_key("World_360").as_deref(), Some("world-360"));
        assert_eq!(project_key("..."), None);
        assert_eq!(
            prefer_display("earthos-world", "EarthOS.world"),
            "EarthOS.world"
        );
        assert_eq!(
            prefer_display("EarthOS.world", "earthos-world"),
            "EarthOS.world"
        );
    }

    #[test]
    fn session_slugs_are_recognised() {
        assert!(is_session_slug("agent-log-colonizer-a1"));
        assert!(!is_session_slug("agent-log-"));
        assert!(!is_session_slug("coding-agents--index"));
    }
}
