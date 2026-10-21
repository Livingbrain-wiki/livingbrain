//! Where the compiled wiki lives: the `coding-agents/` folder, spelled in
//! slugs.
//!
//! A page slug is `[a-z0-9-]{1,128}` — no `/` — so the folder is spelled
//! with a **double hyphen** between path segments, and a segment never
//! contains one (every segment is folded through [`segment`], which
//! collapses runs of `-`). That makes the split unambiguous: a reader (the
//! wiki UI, the Mac app) turns `coding-agents--claude--chi-infra--decisions`
//! back into `coding-agents/claude/chi-infra/decisions` by splitting on
//! `--`, with no lookup table.
//!
//! ```text
//! coding-agents--index                      all agents, all projects
//! coding-agents--log                        append-only: what was processed when
//! coding-agents--schema                     the conventions the model writes by (human-owned)
//! coding-agents--<agent>--index             what this agent has been used for
//! coding-agents--<agent>--patterns          how it works for this person
//! coding-agents--<agent>--<project>--<page> index, decisions, learnings, architecture,
//!                                           open-questions, or a topic the model names
//! coding-agents--projects--<project>        the cross-agent rollup for one project
//! ```
//!
//! The segment count decides what a slug is: three segments under the
//! prefix is an agent page or a rollup, four is a project page. `projects`
//! is therefore reserved as an agent name ([`agent_segment`] never answers
//! it).

/// The collection every compiled page belongs to, as its first segment.
pub const PREFIX: &str = "coding-agents";
/// The separator between path segments inside one slug.
pub const SEP: &str = "--";
/// The folder the cross-agent project rollups live in.
pub const ROLLUP: &str = "projects";

/// The fixed pages one agent-in-a-project folder holds. A topic page is
/// anything else the model names inside the folder.
pub const PROJECT_PAGES: [&str; 5] = [
    "index",
    "decisions",
    "learnings",
    "architecture",
    "open-questions",
];

/// The top-level pages.
pub const INDEX: &str = "coding-agents--index";
pub const LOG: &str = "coding-agents--log";
pub const SCHEMA: &str = "coding-agents--schema";
/// The lint report (the cheap, model-free pass).
pub const LINT: &str = "coding-agents--lint";

/// Folds free text onto one slug segment: lowercase ASCII letters and
/// digits, every other run of characters one `-`, no `-` at either end, at
/// most 28 bytes. `None` when nothing survives.
///
/// This is the canonical project key ([`livingbrain_pages::project_key`]),
/// one rule for the folder a session compiles into, the project filter of
/// every listing and the brain map's project node: `EarthOS.world` and a
/// clone's `earthos-world` are one project.
#[must_use]
pub fn segment(value: &str) -> Option<String> {
    livingbrain_pages::project_key(value)
}

/// The folder name for an agent, from the record's `agent` field: Claude
/// Code's `claude_code` is `claude`, every other agent its own folded name
/// — so a new agent gets a folder of its own without a code change.
/// `projects` is the rollup folder and is never an agent's name.
#[must_use]
pub fn agent_segment(agent: &str) -> String {
    let folded = match agent.trim() {
        "claude_code" | "claude-code" | "claude code" | "claudecode" => "claude".to_owned(),
        other => segment(other).unwrap_or_else(|| "unknown-agent".to_owned()),
    };
    if folded == ROLLUP {
        "projects-agent".to_owned()
    } else {
        folded
    }
}

/// The folder name for a project (a repository's basename).
#[must_use]
pub fn project_segment(project: &str) -> String {
    segment(project).unwrap_or_else(|| "no-project".to_owned())
}

/// `coding-agents--<agent>--<page>`.
#[must_use]
pub fn agent_page(agent: &str, page: &str) -> String {
    format!("{PREFIX}{SEP}{agent}{SEP}{page}")
}

/// `coding-agents--<agent>--<project>--<page>`.
#[must_use]
pub fn project_page(agent: &str, project: &str, page: &str) -> String {
    format!("{PREFIX}{SEP}{agent}{SEP}{project}{SEP}{page}")
}

/// `coding-agents--projects--<project>`.
#[must_use]
pub fn rollup_page(project: &str) -> String {
    format!("{PREFIX}{SEP}{ROLLUP}{SEP}{project}")
}

/// What a slug in the collection is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    /// One of the top-level pages: `index`, `log`, `schema`, `lint`.
    Top(String),
    /// `coding-agents--<agent>--<page>`.
    Agent { agent: String, page: String },
    /// `coding-agents--projects--<project>`.
    Rollup { project: String },
    /// `coding-agents--<agent>--<project>--<page>`.
    Project {
        agent: String,
        project: String,
        page: String,
    },
}

/// Parses a slug of the collection, or `None` for any slug outside it —
/// including one that only *starts* like it (`coding-agentsx--…`) or has a
/// malformed segment.
#[must_use]
pub fn place(slug: &str) -> Option<Place> {
    if !livingbrain_pages::is_slug(slug) {
        return None;
    }
    let mut parts = slug.split(SEP);
    if parts.next() != Some(PREFIX) {
        return None;
    }
    let rest: Vec<&str> = parts.collect();
    if rest
        .iter()
        .any(|part| part.is_empty() || part.starts_with('-') || part.ends_with('-'))
    {
        return None;
    }
    match rest.as_slice() {
        [page] => Some(Place::Top((*page).to_owned())),
        [first, second] if *first == ROLLUP => Some(Place::Rollup {
            project: (*second).to_owned(),
        }),
        [agent, page] => Some(Place::Agent {
            agent: (*agent).to_owned(),
            page: (*page).to_owned(),
        }),
        [agent, project, page] if *agent != ROLLUP => Some(Place::Project {
            agent: (*agent).to_owned(),
            project: (*project).to_owned(),
            page: (*page).to_owned(),
        }),
        _ => None,
    }
}

/// Whether `slug` is a page of the collection at all.
#[must_use]
pub fn in_collection(slug: &str) -> bool {
    place(slug).is_some()
}

/// Whether `slug` is a hub page: the top index, an agent's index or a
/// project's rollup. The server writes these from the scope's pages (see
/// [`crate::hubs`]); the model never does.
#[must_use]
pub fn is_hub(slug: &str) -> bool {
    match place(slug) {
        Some(Place::Top(page) | Place::Agent { page, .. }) => page == "index",
        Some(Place::Rollup { .. }) => true,
        _ => false,
    }
}

/// The slugs one session's compile may write, and nothing else: its agent's
/// `patterns` page, its own agent-in-project folder (the fixed pages and any
/// topic page in it). The hub pages (the index, the agent indexes, the
/// rollups) are the server's, written from the pages; the log is written by
/// the server alone, and the schema is the person's — none is ever the
/// model's to edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Allowed {
    pub agent: String,
    pub project: String,
}

impl Allowed {
    /// Whether the model may write `slug` for this session.
    #[must_use]
    pub fn permits(&self, slug: &str) -> bool {
        if is_hub(slug) {
            return false;
        }
        match place(slug) {
            Some(Place::Agent { agent, page }) => agent == self.agent && page == "patterns",
            Some(Place::Project {
                agent,
                project,
                page,
            }) => agent == self.agent && project == self.project && segment(&page) == Some(page),
            Some(Place::Top(_) | Place::Rollup { .. }) | None => false,
        }
    }

    /// The pages this session's folder is seeded with, in reading order.
    #[must_use]
    pub fn context_slugs(&self) -> Vec<String> {
        let mut slugs = vec![INDEX.to_owned()];
        slugs.push(agent_page(&self.agent, "index"));
        slugs.push(agent_page(&self.agent, "patterns"));
        for page in PROJECT_PAGES {
            slugs.push(project_page(&self.agent, &self.project, page));
        }
        slugs.push(rollup_page(&self.project));
        slugs
    }

    /// The prefix every page in this session's own folder starts with.
    #[must_use]
    pub fn folder_prefix(&self) -> String {
        format!("{PREFIX}{SEP}{}{SEP}{}{SEP}", self.agent, self.project)
    }
}

/// What kind of page a slug is, for its frontmatter (`page:`).
#[must_use]
pub fn page_kind(place: &Place) -> String {
    match place {
        Place::Top(page) | Place::Agent { page, .. } | Place::Project { page, .. } => page.clone(),
        Place::Rollup { .. } => "rollup".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_fold_onto_their_folders() {
        assert_eq!(agent_segment("claude_code"), "claude");
        assert_eq!(agent_segment("codex"), "codex");
        assert_eq!(agent_segment("Open Code"), "open-code");
        assert_eq!(agent_segment("projects"), "projects-agent");
        assert_eq!(agent_segment("!!!"), "unknown-agent");
    }

    #[test]
    fn a_segment_never_carries_the_separator() {
        assert_eq!(segment("chi--infra").as_deref(), Some("chi-infra"));
        assert_eq!(segment("My_Repo.v2").as_deref(), Some("my-repo-v2"));
        assert_eq!(segment("---"), None);
        assert!(segment(&"a".repeat(200)).unwrap().len() <= 28);
    }

    #[test]
    fn a_slug_splits_back_into_its_folder() {
        assert_eq!(place(INDEX), Some(Place::Top("index".to_owned())));
        assert_eq!(
            place("coding-agents--claude--patterns"),
            Some(Place::Agent {
                agent: "claude".to_owned(),
                page: "patterns".to_owned()
            })
        );
        assert_eq!(
            place("coding-agents--projects--chi-infra"),
            Some(Place::Rollup {
                project: "chi-infra".to_owned()
            })
        );
        assert_eq!(
            place("coding-agents--codex--chi-infra--decisions"),
            Some(Place::Project {
                agent: "codex".to_owned(),
                project: "chi-infra".to_owned(),
                page: "decisions".to_owned()
            })
        );
        assert_eq!(place("coding-agentsx--index"), None);
        assert_eq!(place("decisions"), None);
        assert_eq!(place("coding-agents---index"), None);
        assert_eq!(place("coding-agents--a--b--c--d"), None);
        assert_eq!(place("coding-agents--projects--x--y"), None);
    }

    #[test]
    fn the_hubs_are_the_servers_and_say_so() {
        for hub in [
            INDEX.to_owned(),
            agent_page("claude", "index"),
            rollup_page("demo"),
        ] {
            assert!(is_hub(&hub), "{hub}");
        }
        assert!(!is_hub(&agent_page("claude", "patterns")));
        assert!(!is_hub(&project_page("claude", "demo", "index")));
        assert!(!is_hub(LOG));
    }

    #[test]
    fn a_session_may_write_only_its_own_folder() {
        let allowed = Allowed {
            agent: "claude".to_owned(),
            project: "demo".to_owned(),
        };
        for ok in [
            agent_page("claude", "patterns"),
            project_page("claude", "demo", "decisions"),
            project_page("claude", "demo", "d1-migration"),
        ] {
            assert!(allowed.permits(&ok), "{ok}");
        }
        for refused in [
            LOG.to_owned(),
            SCHEMA.to_owned(),
            // The hubs are the server's: written from the pages, never the model's.
            INDEX.to_owned(),
            agent_page("claude", "index"),
            rollup_page("demo"),
            agent_page("codex", "index"),
            agent_page("claude", "secrets"),
            project_page("codex", "demo", "decisions"),
            project_page("claude", "other", "decisions"),
            rollup_page("other"),
            "people-nick".to_owned(),
            "coding-agents".to_owned(),
        ] {
            assert!(!allowed.permits(&refused), "{refused}");
        }
    }
}
