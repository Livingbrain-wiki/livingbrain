//! The hub pages: the top index, each agent's index and each project's
//! rollup. The server builds them from the scope's own slugs, its log and
//! the current decisions — never the model — so the same inputs give the
//! same bytes (stable) and every entry is listed once (duplicate-free).
//!
//! Nothing here reads a clock: the dates come from the log and the pages.

use std::collections::{BTreeMap, BTreeSet};

use crate::layout::{self, Place};

/// The most decision lines one page may contribute to a rollup.
pub const MAX_DECISIONS: usize = 200;

/// One hub page to write: its slug, title and body (without frontmatter).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hub {
    pub slug: String,
    pub title: String,
    pub body: String,
}

/// What the hubs are built from.
#[derive(Debug, Clone, Copy)]
pub struct Inputs<'a> {
    /// Every page slug in the scope.
    pub slugs: &'a BTreeSet<String>,
    /// The scope's log page body (one line per processed session).
    pub log: &'a str,
    /// The current decision lines of each `decisions` page, by slug.
    pub decisions: &'a BTreeMap<String, Vec<String>>,
}

/// What the log says about one agent-in-project folder.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Activity {
    /// One token per distinct session the log names.
    pub sessions: BTreeSet<String>,
    /// The latest day the log names, `YYYY-MM-DD` ("" when none).
    pub last_day: String,
}

/// Reads the log's session lines (`- **day** · agent/project · session \`id\` …`)
/// into activity per `(agent, project)`. A session logged twice counts once.
#[must_use]
pub fn parse_log(log: &str) -> BTreeMap<(String, String), Activity> {
    let mut out: BTreeMap<(String, String), Activity> = BTreeMap::new();
    for line in log.lines() {
        let Some(rest) = line.strip_prefix("- **") else {
            continue;
        };
        let Some((day, rest)) = rest.split_once("**") else {
            continue;
        };
        if !is_day(day) {
            continue;
        }
        let mut parts = rest.split(" · ").skip(1);
        let Some((agent, project)) = parts.next().and_then(|folder| folder.split_once('/')) else {
            continue;
        };
        let Some(session) = parts.next().and_then(session_token) else {
            continue;
        };
        let entry = out
            .entry((agent.to_owned(), project.to_owned()))
            .or_default();
        entry.sessions.insert(session.to_owned());
        if day > entry.last_day.as_str() {
            entry.last_day = day.to_owned();
        }
    }
    out
}

/// The current decision lines of a decisions page: bullets that are not
/// struck through or superseded, in page order, at most [`MAX_DECISIONS`].
/// The `## Sources` section the compiler appends holds citations, not
/// decisions, and is skipped.
#[must_use]
pub fn decision_lines(body: &str) -> Vec<String> {
    let mut in_sources = false;
    let mut out = Vec::new();
    for line in body.lines().map(str::trim) {
        if let Some(heading) = line.strip_prefix("## ") {
            in_sources = heading.trim().eq_ignore_ascii_case("sources");
            continue;
        }
        if in_sources || !line.starts_with("- ") {
            continue;
        }
        if line.contains("~~") || line.to_ascii_lowercase().contains("superseded") {
            continue;
        }
        out.push(line.to_owned());
        if out.len() == MAX_DECISIONS {
            break;
        }
    }
    out
}

/// A line's identity for de-duplication: its text without markdown links
/// (a citation differs per session), emphasis or tildes, lowercased, with
/// runs of spaces folded to one.
#[must_use]
pub fn line_key(line: &str) -> String {
    strip_links(line)
        .replace(['*', '~'], "")
        .to_ascii_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Builds the hub pages for one scope, in slug order.
#[must_use]
pub fn render(inputs: Inputs<'_>) -> Vec<Hub> {
    // Every agent-in-project folder the scope holds: from its pages, and
    // from the log (a folder whose pages were all refused still logs).
    let mut folders = parse_log(inputs.log);
    let mut agent_pages: BTreeSet<String> = BTreeSet::new();
    for slug in inputs.slugs {
        match layout::place(slug) {
            Some(Place::Project { agent, project, .. }) => {
                folders.entry((agent, project)).or_default();
            }
            Some(Place::Agent { agent, .. }) => {
                agent_pages.insert(agent);
            }
            _ => {}
        }
    }
    let agents: BTreeSet<String> = folders
        .keys()
        .map(|(agent, _)| agent.clone())
        .chain(agent_pages)
        .collect();
    let projects: BTreeSet<String> = folders.keys().map(|(_, project)| project.clone()).collect();
    let exists = |slug: &str| inputs.slugs.contains(slug);

    let mut hubs = Vec::new();

    // The top index: every agent and every project, one line each.
    let mut index = String::from(
        "# Coding agents: index\n\nOne line per agent and per project. The server writes this page from the compiled pages; the model does not edit it.\n\n## Agents\n\n",
    );
    if agents.is_empty() {
        index.push_str("No agents compiled yet.\n");
    }
    for agent in &agents {
        let total = totals(&folders, Some(agent), None);
        index.push_str(&format!(
            "- {} · {} · {}\n",
            link(&layout::agent_page(agent, "index"), agent),
            sessions_in(total.sessions, "project", total.others.len()),
            last_active(&total.last_day),
        ));
    }
    index.push_str("\n## Projects\n\n");
    if projects.is_empty() {
        index.push_str("No projects compiled yet.\n");
    }
    for project in &projects {
        let total = totals(&folders, None, Some(project));
        index.push_str(&format!(
            "- {} · {} · agents: {} · {}\n",
            link(&layout::rollup_page(project), project),
            sessions_in(total.sessions, "agent", total.others.len()),
            total.others.iter().cloned().collect::<Vec<_>>().join(", "),
            last_active(&total.last_day),
        ));
    }
    hubs.push(Hub {
        slug: layout::INDEX.to_owned(),
        title: "Coding agents: index".to_owned(),
        body: index,
    });

    // One index per agent: what it has been used for, project by project.
    for agent in &agents {
        let total = totals(&folders, Some(agent), None);
        let mut body = format!(
            "# {agent}: index\n\nWritten by the server from the compiled pages: what this agent has been used for.\n\n- {} · {}\n\n## Projects\n\n",
            sessions_in(total.sessions, "project", total.others.len()),
            last_active(&total.last_day),
        );
        if total.others.is_empty() {
            body.push_str("No projects compiled yet.\n");
        }
        for ((_, project), found) in folders.iter().filter(|((a, _), _)| a == agent) {
            let index_slug = layout::project_page(agent, project, "index");
            let label = if exists(&index_slug) {
                link(&index_slug, project)
            } else {
                project.clone()
            };
            body.push_str(&format!(
                "- {label} · {} · {}\n",
                plural(found.sessions.len(), "session"),
                last_active(&found.last_day),
            ));
        }
        hubs.push(Hub {
            slug: layout::agent_page(agent, "index"),
            title: format!("{agent}: index"),
            body,
        });
    }

    // One rollup per project: each agent's pages for it, and the decisions
    // in force across all of them, once each.
    for project in &projects {
        let mut body = format!(
            "# Project {project}: all agents\n\nWritten by the server from each agent's pages for this project.\n\n## Agents\n\n"
        );
        let folders_here: Vec<_> = folders.iter().filter(|((_, p), _)| p == project).collect();
        if folders_here.is_empty() {
            body.push_str("No agents compiled yet.\n");
        }
        for ((agent, _), found) in &folders_here {
            let pages: Vec<String> = layout::PROJECT_PAGES
                .iter()
                .map(|page| (*page, layout::project_page(agent, project, page)))
                .filter(|(_, slug)| exists(slug))
                .map(|(page, slug)| link(&slug, &page.replace('-', " ")))
                .collect();
            let listed = if pages.is_empty() {
                "no pages yet".to_owned()
            } else {
                pages.join(" · ")
            };
            body.push_str(&format!(
                "- {agent}: {} · {} · {listed}\n",
                plural(found.sessions.len(), "session"),
                last_active(&found.last_day),
            ));
        }

        body.push_str("\n## Decisions\n\n");
        // Newest first: the dated lines sort by their date.
        let mut lines: Vec<&String> = inputs
            .decisions
            .iter()
            .filter(|(slug, _)| is_decisions_of(slug, project))
            .flat_map(|(_, lines)| lines.iter())
            .collect();
        lines.sort_by(|a, b| b.cmp(a));
        let mut seen = BTreeSet::new();
        let unique: Vec<&String> = lines
            .into_iter()
            .filter(|line| seen.insert(line_key(line)))
            .collect();
        if unique.is_empty() {
            body.push_str("No decisions recorded yet.\n");
        }
        for line in unique {
            body.push_str(line);
            body.push('\n');
        }
        hubs.push(Hub {
            slug: layout::rollup_page(project),
            title: format!("Project {project}: all agents"),
            body,
        });
    }

    hubs.sort_by(|a, b| a.slug.cmp(&b.slug));
    hubs
}

/// The folders one agent or one project holds, summed.
struct Total {
    sessions: usize,
    last_day: String,
    /// The other side of the pair: the projects of an agent, the agents of
    /// a project.
    others: BTreeSet<String>,
}

fn totals(
    folders: &BTreeMap<(String, String), Activity>,
    agent: Option<&String>,
    project: Option<&String>,
) -> Total {
    let mut total = Total {
        sessions: 0,
        last_day: String::new(),
        others: BTreeSet::new(),
    };
    for ((a, p), found) in folders {
        if agent.is_some_and(|wanted| wanted != a) || project.is_some_and(|wanted| wanted != p) {
            continue;
        }
        total.sessions += found.sessions.len();
        if found.last_day > total.last_day {
            total.last_day = found.last_day.clone();
        }
        total.others.insert(if agent.is_some() {
            p.clone()
        } else {
            a.clone()
        });
    }
    total
}

/// Whether `slug` is a project's `decisions` page, under any agent.
#[must_use]
pub fn is_decisions_slug(slug: &str) -> bool {
    matches!(layout::place(slug), Some(Place::Project { ref page, .. }) if page == "decisions")
}

/// Whether `slug` is the decisions page of `project`, under any agent.
fn is_decisions_of(slug: &str, project: &str) -> bool {
    matches!(
        layout::place(slug),
        Some(Place::Project { project: ref p, ref page, .. }) if p == project && page == "decisions"
    )
}

fn link(slug: &str, label: &str) -> String {
    format!("[[{slug}|{label}]]")
}

fn plural(count: usize, word: &str) -> String {
    format!("{count} {word}{}", if count == 1 { "" } else { "s" })
}

fn sessions_in(sessions: usize, part: &str, parts: usize) -> String {
    format!("{} in {}", plural(sessions, "session"), plural(parts, part))
}

fn last_active(day: &str) -> String {
    if day.is_empty() {
        "no sessions logged".to_owned()
    } else {
        format!("last active {day}")
    }
}

/// `YYYY-MM-DD`, nothing else.
fn is_day(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 10
        && bytes.iter().enumerate().all(|(i, byte)| match i {
            4 | 7 => *byte == b'-',
            _ => byte.is_ascii_digit(),
        })
}

/// The short session id between the backticks of a log segment
/// (`session \`abcd1234\` of 2026-10-15`).
fn session_token(segment: &str) -> Option<&str> {
    let rest = segment.strip_prefix("session `")?;
    let (token, _) = rest.split_once('`')?;
    (!token.is_empty()).then_some(token)
}

/// `text` without its `[label](target)` links.
fn strip_links(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..open]);
        let tail = &rest[open..];
        let linked = tail.find(']').and_then(|close| {
            let after = &tail[close + 1..];
            after
                .strip_prefix('(')
                .and_then(|target| target.find(')'))
                .map(|end| close + 1 + 1 + end + 1)
        });
        match linked {
            Some(end) => rest = &tail[end..],
            None => {
                out.push('[');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout;

    #[test]
    fn a_citation_is_not_part_of_a_decisions_identity() {
        assert_eq!(
            line_key("- **2026-10-15** · Use D1 · [session 2026-10-15 · claude](wiki.html?x)"),
            line_key("- **2026-10-15** · Use D1 · [session 2026-10-16 · codex](wiki.html?y)"),
        );
        assert_eq!(strip_links("a [b](c) d [e"), "a  d [e");
    }

    #[test]
    fn decisions_are_the_current_bullets_not_the_citations_or_the_struck_ones() {
        let body = "# Decisions\n\n- **2026-10-15** · Use D1\n- ~~**2026-09-01** · Use Supabase~~ · superseded 2026-10-15\n\n## Sources\n\n- [session 2026-10-15 · claude](wiki.html?source=a)\n";
        assert_eq!(
            decision_lines(body),
            vec!["- **2026-10-15** · Use D1".to_owned()]
        );
    }

    #[test]
    fn a_log_session_counts_once_per_folder() {
        let log = "# log\n- **2026-10-15** · claude/demo · session `abc12345` of 2026-10-15 · x\n- **2026-10-16** · claude/demo · session `abc12345` of 2026-10-15 · again\n- **2026-10-14** · codex/demo · session `def67890` of 2026-10-14 · y\n";
        let activity = parse_log(log);
        let claude = &activity[&("claude".to_owned(), "demo".to_owned())];
        assert_eq!(claude.sessions.len(), 1);
        assert_eq!(claude.last_day, "2026-10-16");
        assert_eq!(activity.len(), 2);
    }

    fn two_agents() -> (BTreeSet<String>, String, BTreeMap<String, Vec<String>>) {
        let slugs: BTreeSet<String> = [
            layout::project_page("claude", "demo", "decisions"),
            layout::project_page("codex", "demo", "decisions"),
            layout::project_page("codex", "demo", "learnings"),
            layout::agent_page("claude", "patterns"),
        ]
        .into_iter()
        .collect();
        let log = "# log\n- **2026-10-15** · claude/demo · session `aaaa1111` of 2026-10-15 · did a thing\n- **2026-10-16** · codex/demo · session `bbbb2222` of 2026-10-16 · did another\n".to_owned();
        let mut decisions = BTreeMap::new();
        decisions.insert(
            layout::project_page("claude", "demo", "decisions"),
            decision_lines(
                "- **2026-10-15** · Use D1 · [session 2026-10-15 · claude](wiki.html?source=a)\n",
            ),
        );
        decisions.insert(
            layout::project_page("codex", "demo", "decisions"),
            decision_lines("- **2026-10-15** · Use D1 · [session 2026-10-16 · codex](wiki.html?source=b)\n- **2026-10-16** · Ship on Friday\n"),
        );
        (slugs, log, decisions)
    }

    #[test]
    fn hub_pages_are_stable_and_list_each_entry_once() {
        let (slugs, log, decisions) = two_agents();
        let inputs = Inputs {
            slugs: &slugs,
            log: &log,
            decisions: &decisions,
        };
        let first = render(inputs);
        assert_eq!(first, render(inputs), "the same inputs give the same bytes");
        let slugs_out: Vec<&str> = first.iter().map(|hub| hub.slug.as_str()).collect();
        assert_eq!(
            slugs_out,
            [
                "coding-agents--claude--index",
                "coding-agents--codex--index",
                "coding-agents--index",
                "coding-agents--projects--demo",
            ]
        );
        let rollup = first
            .iter()
            .find(|hub| hub.slug == "coding-agents--projects--demo")
            .expect("rollup");
        assert_eq!(rollup.body.matches("Use D1").count(), 1, "{}", rollup.body);
        assert_eq!(
            rollup.body.matches("Ship on Friday").count(),
            1,
            "{}",
            rollup.body
        );
        let top = first
            .iter()
            .find(|hub| hub.slug == layout::INDEX)
            .expect("index");
        assert_eq!(
            top.body
                .matches("[[coding-agents--claude--index|claude]]")
                .count(),
            1
        );
        assert!(
            top.body
                .contains("[[coding-agents--claude--index|claude]] · 1 session in 1 project"),
            "{}",
            top.body
        );
        assert!(
            top.body
                .contains("[[coding-agents--projects--demo|demo]] · 2 sessions in 2 agents"),
            "{}",
            top.body
        );
    }

    #[test]
    fn a_hub_names_only_pages_that_exist_and_no_clock() {
        let (slugs, log, decisions) = two_agents();
        let hubs = render(Inputs {
            slugs: &slugs,
            log: &log,
            decisions: &decisions,
        });
        let codex = hubs
            .iter()
            .find(|hub| hub.slug == "coding-agents--codex--index")
            .expect("codex");
        // The codex folder has no index page, so its project is named as text.
        assert!(
            !codex.body.contains("[[coding-agents--codex--demo--index"),
            "{}",
            codex.body
        );
        assert!(codex.body.contains("- demo · 1 session"), "{}", codex.body);
        let rollup = hubs
            .iter()
            .find(|hub| hub.slug == "coding-agents--projects--demo")
            .expect("rollup");
        assert!(
            rollup
                .body
                .contains("[[coding-agents--codex--demo--learnings|learnings]]"),
            "{}",
            rollup.body
        );
        assert!(!rollup.body.contains("architecture"), "{}", rollup.body);
    }
}
