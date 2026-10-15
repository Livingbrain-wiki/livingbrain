//! Access grants through every wiki route (ADR 0004).
//!
//! What these hold the routes to: a member's personal space stays theirs; a
//! grant opens exactly its folder (`coding-agents/claude/` and never
//! `coding-agents/codex/` or `coding-agents/claudette`); read is not write; a
//! deny carves a folder out; a credential only narrows; an agent account
//! reaches nothing until granted; and listing, search, ask, backlinks,
//! citations and export never name, count or quote a page outside the grant —
//! whatever the limit, whatever ranks first. Workspaces never mix.

mod common;

use common::*;
use cratefield_module_orgs::grants::Permission;
use livingbrain_access::Scope;
use serde_json::{Value, json};

const CLAUDE: &str = "coding-agents--claude--chi-infra--decisions";
const CODEX: &str = "coding-agents--codex--chi-infra--decisions";
const CLAUDETTE: &str = "coding-agents--claudette";
const DIARY: &str = "diary";
const SECRET: &str = "coding-agents--claude--secret";

/// `user-a`'s personal space with one page in each place a grant could
/// reach, every body sharing the word "orchid" so one search touches all.
fn world() -> Fixture {
    let fixture = fixture();
    let a = user("user-a");
    seed(
        &fixture,
        WORKSPACE,
        &a,
        CLAUDE,
        "---\ntitle: Claude decisions\n---\n\norchid retries go through the queue. See [[diary]] and [[coding-agents--codex--chi-infra--decisions]].",
    );
    seed(
        &fixture,
        WORKSPACE,
        &a,
        CODEX,
        "---\ntitle: Codex decisions\n---\n\norchid codex notes",
    );
    seed(
        &fixture,
        WORKSPACE,
        &a,
        CLAUDETTE,
        "---\ntitle: Claudette\n---\n\norchid lookalike",
    );
    seed(
        &fixture,
        WORKSPACE,
        &a,
        DIARY,
        "---\ntitle: Diary\n---\n\norchid private thoughts, links [[coding-agents--claude--chi-infra--decisions]]",
    );
    seed(
        &fixture,
        WORKSPACE,
        &a,
        SECRET,
        "---\ntitle: Secret\n---\n\norchid secret",
    );
    seed(
        &fixture,
        WORKSPACE,
        &Scope::Shared,
        "team-handbook",
        "---\ntitle: Handbook\n---\n\norchid shared",
    );
    fixture
}

fn slugs(listing: &Value, key: &str, field: &str) -> Vec<String> {
    listing[key]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item[field].as_str())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn listed(fixture: &Fixture, token: &str) -> Vec<String> {
    let response = get(fixture, token, "/v1/pages?limit=200");
    assert_eq!(response.status.as_u16(), 200, "{}", body_text(&response));
    slugs(&response.json(), "pages", "slug")
}

fn found(fixture: &Fixture, token: &str, query: &str, limit: usize) -> String {
    let response = get(
        fixture,
        token,
        &format!("/v1/search?q={query}&limit={limit}"),
    );
    assert_eq!(response.status.as_u16(), 200, "{}", body_text(&response));
    body_text(&response)
}

fn grant_claude_folder(fixture: &Fixture, principal: &str, permission: Permission) {
    grant(
        fixture,
        WORKSPACE,
        principal,
        "personal:user-a",
        "coding-agents/claude",
        "pages",
        permission,
        false,
    );
}

#[test]
fn with_no_grants_a_personal_space_is_its_owners_alone() {
    let fixture = world();
    let mine = listed(&fixture, TOKEN_A);
    assert!(mine.contains(&DIARY.to_owned()) && mine.contains(&"team-handbook".to_owned()));
    let theirs = listed(&fixture, TOKEN_B);
    assert_eq!(
        theirs,
        vec!["team-handbook".to_owned()],
        "only the shared page"
    );
    assert_eq!(
        get(&fixture, TOKEN_B, &format!("/v1/pages/{DIARY}"))
            .status
            .as_u16(),
        404
    );
    let search = found(&fixture, TOKEN_B, "orchid", 25);
    assert!(
        !search.contains("Diary") && !search.contains("Claude decisions"),
        "{search}"
    );
}

#[test]
fn a_folder_grant_opens_that_folder_and_nothing_beside_it() {
    let fixture = world();
    grant_claude_folder(&fixture, "member:user-b", Permission::Read);
    let theirs = listed(&fixture, TOKEN_B);
    assert!(theirs.contains(&CLAUDE.to_owned()), "{theirs:?}");
    assert!(
        theirs.contains(&SECRET.to_owned()),
        "a sub-folder is inside the folder"
    );
    for hidden in [CODEX, CLAUDETTE, DIARY] {
        assert!(
            !theirs.contains(&hidden.to_owned()),
            "{hidden} leaked: {theirs:?}"
        );
        assert_eq!(
            get(&fixture, TOKEN_B, &format!("/v1/pages/{hidden}"))
                .status
                .as_u16(),
            404,
            "{hidden}"
        );
    }
    let page = get(&fixture, TOKEN_B, &format!("/v1/pages/{CLAUDE}"));
    assert_eq!(page.status.as_u16(), 200);
    let view = page.json();
    // The page links to the diary and to the codex page: neither is named.
    let cited: Vec<String> = slugs(&json!({"c": view["citations"]}), "c", "title");
    assert!(cited.is_empty(), "citations outside the grant: {cited:?}");
    // The diary links here, and is outside the grant: no backlink names it.
    assert_eq!(view["backlinks"], json!([]), "{view}");
    // user-a, who can read everything, sees both.
    let full = get(&fixture, TOKEN_A, &format!("/v1/pages/{CLAUDE}")).json();
    assert_eq!(full["backlinks"], json!([DIARY]));
    assert_eq!(full["citations"].as_array().map(Vec::len), Some(2));
}

#[test]
fn search_and_ask_filter_before_they_rank_or_limit() {
    let fixture = world();
    grant_claude_folder(&fixture, "member:user-b", Permission::Read);
    // Every page holds "orchid"; with a limit of one the granted page must
    // still be the one returned, never a slot taken by an ungranted hit.
    for limit in [1, 2, 25] {
        let search = found(&fixture, TOKEN_B, "orchid", limit);
        for leaked in ["Codex decisions", "Claudette", "Diary", "private thoughts"] {
            assert!(
                !search.contains(leaked),
                "limit {limit}: {leaked} in {search}"
            );
        }
    }
    let one = get(&fixture, TOKEN_B, "/v1/search?q=retries&limit=1").json();
    assert_eq!(one["results"].as_array().map(Vec::len), Some(1), "{one}");

    let asked = post(&fixture, TOKEN_B, "/v1/ask", json!({"question": "orchid"}));
    assert_eq!(asked.status.as_u16(), 200, "{}", body_text(&asked));
    let brief = body_text(&asked);
    for leaked in ["codex notes", "lookalike", "private thoughts"] {
        assert!(
            !brief.contains(leaked),
            "{leaked} reached the brief: {brief}"
        );
    }
}

#[test]
fn export_carries_only_what_the_grant_opens() {
    let fixture = world();
    grant_claude_folder(&fixture, "member:user-b", Permission::Read);
    let response = get(&fixture, TOKEN_B, "/v1/export?format=obsidian");
    assert_eq!(response.status.as_u16(), 200, "{}", body_text(&response));
    let names: Vec<String> = zip_entries(response.body().as_ref())
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert!(
        names
            .iter()
            .any(|name| name.ends_with(&format!("/{CLAUDE}.md"))),
        "{names:?}"
    );
    for hidden in [CODEX, CLAUDETTE, DIARY] {
        assert!(
            !names
                .iter()
                .any(|name| name.ends_with(&format!("/{hidden}.md"))),
            "{hidden}: {names:?}"
        );
    }
    assert!(
        audit_actions(&fixture, WORKSPACE)
            .iter()
            .any(|(actor, action, _)| actor == "user-b" && action == "export"),
        "an export is recorded"
    );
}

#[test]
fn read_is_not_write() {
    let fixture = world();
    grant_claude_folder(&fixture, "member:user-b", Permission::Read);
    let current = get(&fixture, TOKEN_B, &format!("/v1/pages/{CLAUDE}")).json();
    let refused = put(
        &fixture,
        TOKEN_B,
        &format!("/v1/pages/{CLAUDE}"),
        json!({"markdown": "---\ntitle: Claude decisions\n---\n\nedited", "base_version": current["version"]}),
    );
    assert_eq!(refused.status.as_u16(), 422, "{}", body_text(&refused));

    grant_claude_folder(&fixture, "member:user-b", Permission::Write);
    let edited = put(
        &fixture,
        TOKEN_B,
        &format!("/v1/pages/{CLAUDE}"),
        json!({"markdown": "---\ntitle: Claude decisions\n---\n\nedited", "base_version": current["version"]}),
    );
    assert_eq!(edited.status.as_u16(), 200, "{}", body_text(&edited));
    // The edit stayed in user-a's space: the grant moved nothing.
    let read = pollster::block_on(fixture.store.read(&scope_of(WORKSPACE, "user-a"), CLAUDE))
        .expect("reads")
        .expect("there");
    assert!(read.markdown.contains("edited"));
}

#[test]
fn a_deny_carves_a_folder_out_of_an_allow() {
    let fixture = world();
    grant(
        &fixture,
        WORKSPACE,
        "member:user-b",
        "personal:user-a",
        "coding-agents",
        "pages",
        Permission::Read,
        false,
    );
    grant(
        &fixture,
        WORKSPACE,
        "member:user-b",
        "personal:user-a",
        "coding-agents/claude/secret",
        "pages",
        Permission::Read,
        true,
    );
    let theirs = listed(&fixture, TOKEN_B);
    assert!(theirs.contains(&CLAUDE.to_owned()) && theirs.contains(&CODEX.to_owned()));
    assert!(!theirs.contains(&SECRET.to_owned()), "{theirs:?}");
    assert_eq!(
        get(&fixture, TOKEN_B, &format!("/v1/pages/{SECRET}"))
            .status
            .as_u16(),
        404
    );
    assert!(!found(&fixture, TOKEN_B, "secret", 25).contains("Secret"));
}

#[test]
fn a_narrowed_token_reads_only_the_intersection() {
    let fixture = world();
    grant_claude_folder(&fixture, "member:user-b", Permission::Read);
    // The shared-only token: the granted folder is outside it.
    assert_eq!(
        listed(&fixture, TOKEN_B_SHARED),
        vec!["team-handbook".to_owned()]
    );
    assert_eq!(
        get(&fixture, TOKEN_B_SHARED, &format!("/v1/pages/{CLAUDE}"))
            .status
            .as_u16(),
        404
    );
}

#[test]
fn an_agent_account_reaches_nothing_until_granted() {
    let fixture = world();
    assert!(
        listed(&fixture, TOKEN_AGENT).is_empty(),
        "not even the shared space"
    );
    assert_eq!(
        get(&fixture, TOKEN_AGENT, &format!("/v1/pages/{CLAUDE}"))
            .status
            .as_u16(),
        404
    );
    assert!(!found(&fixture, TOKEN_AGENT, "orchid", 25).contains("orchid"));
    let note = post(
        &fixture,
        TOKEN_AGENT,
        "/v1/notes",
        json!({"body": "agent wrote this"}),
    );
    assert_eq!(note.status.as_u16(), 403, "{}", body_text(&note));
}

#[test]
fn an_agent_reads_and_writes_exactly_its_grant_and_is_attributed() {
    let fixture = world();
    grant_claude_folder(&fixture, &format!("agent:{AGENT}"), Permission::Write);
    let theirs = listed(&fixture, TOKEN_AGENT);
    assert!(theirs.contains(&CLAUDE.to_owned()));
    assert!(!theirs.contains(&DIARY.to_owned()) && !theirs.contains(&"team-handbook".to_owned()));

    // A keyed note in the granted folder lands in the owner's space.
    let note = post(
        &fixture,
        TOKEN_AGENT,
        "/v1/notes",
        json!({"body": "compiled by the agent", "slug": "coding-agents--claude--log"}),
    );
    assert_eq!(note.status.as_u16(), 200, "{}", body_text(&note));
    let stored = pollster::block_on(
        fixture
            .store
            .read(&scope_of(WORKSPACE, "user-a"), "coding-agents--claude--log"),
    )
    .expect("reads");
    assert!(stored.is_some(), "the note is in user-a's space");
    // Outside the folder it is refused.
    let outside = post(
        &fixture,
        TOKEN_AGENT,
        "/v1/notes",
        json!({"body": "elsewhere", "slug": "coding-agents--codex--log"}),
    );
    assert_eq!(outside.status.as_u16(), 403, "{}", body_text(&outside));
    // And the write is the agent's, on user-a's behalf, with its key prefix.
    let log = audit_actions(&fixture, WORKSPACE);
    assert!(
        log.iter().any(|(actor, action, behalf)| actor == AGENT
            && action == "note.write"
            && behalf.as_deref() == Some("user-a")),
        "{log:?}"
    );
}

#[test]
fn an_agent_key_cut_to_read_cannot_write_what_the_account_may() {
    let fixture = world();
    grant_claude_folder(&fixture, &format!("agent:{AGENT}"), Permission::Write);
    assert!(listed(&fixture, TOKEN_AGENT_NARROW).contains(&CLAUDE.to_owned()));
    let note = post(
        &fixture,
        TOKEN_AGENT_NARROW,
        "/v1/notes",
        json!({"body": "x", "slug": "coding-agents--claude--log"}),
    );
    assert_eq!(note.status.as_u16(), 403, "{}", body_text(&note));
}

#[test]
fn a_grant_in_one_workspace_opens_nothing_in_another() {
    let fixture = world();
    // The same principal string and space, but stored for the other
    // workspace: it must not reach the first one's pages.
    grant(
        &fixture,
        OTHER_WORKSPACE,
        "member:user-b",
        "personal:user-a",
        "",
        "*",
        Permission::Admin,
        false,
    );
    assert_eq!(listed(&fixture, TOKEN_B), vec!["team-handbook".to_owned()]);
    // And user-a elsewhere reads none of user-a's pages here.
    assert!(listed(&fixture, TOKEN_A_ELSEWHERE).is_empty());
}

#[test]
fn a_pages_grant_is_not_a_sources_or_facts_grant() {
    let fixture = world();
    grant(
        &fixture,
        WORKSPACE,
        &format!("agent:{AGENT}"),
        "personal:user-a",
        "",
        "pages",
        Permission::Read,
        false,
    );
    let sources = get(&fixture, TOKEN_AGENT, "/v1/sources");
    assert_eq!(sources.status.as_u16(), 200, "{}", body_text(&sources));
    assert_eq!(sources.json()["sources"], json!([]));
    let facts = get(&fixture, TOKEN_AGENT, "/v1/facts");
    assert_eq!(facts.status.as_u16(), 200, "{}", body_text(&facts));
    assert_eq!(facts.json()["keys"], json!([]));
}

/// A source in `scope`: an agent session or a GitHub commit, under `project`.
fn source(
    fixture: &Fixture,
    scope: &str,
    kind: livingbrain_pages::SourceKind,
    path: &str,
    project: &str,
) -> String {
    pollster::block_on(fixture.sources.put(
        scope,
        livingbrain_pages::SourceWrite {
            kind,
            workspace: WORKSPACE.to_owned(),
            origin_ref: None,
            author: None,
            rel_path: path.to_owned(),
            markdown: format!("orchid body of {path}"),
            imported_by: "user-a".to_owned(),
            held: false,
            project: Some(project.to_owned()),
        },
    ))
    .expect("a seeded source")
    .0
    .id
}

/// [`world`], plus a's session and a GitHub commit in a's space, a GitHub
/// commit in the shared space, and two fact subjects in a's space.
fn wider_world() -> Fixture {
    let fixture = world();
    let own = scope_of(WORKSPACE, "user-a");
    let shared = livingbrain_pages::page_scope(WORKSPACE, &Scope::Shared);
    source(
        &fixture,
        &own,
        livingbrain_pages::SourceKind::AgentLog,
        "claude_code session private-session",
        "alpha",
    );
    source(
        &fixture,
        &own,
        livingbrain_pages::SourceKind::GithubCommit,
        "github commit private-sha",
        "alpha",
    );
    source(
        &fixture,
        &shared,
        livingbrain_pages::SourceKind::GithubCommit,
        "github commit shared-sha",
        "beta",
    );
    let written = post(
        &fixture,
        TOKEN_A,
        "/v1/facts",
        json!({ "format": 1, "facts": [
            { "subject": "person/zed", "attribute": "role", "value": "reviewer" },
            { "subject": "person/yan", "attribute": "role", "value": "orchid-keeper" },
        ]}),
    );
    assert_eq!(written.status.as_u16(), 201, "{}", body_text(&written));
    fixture
}

fn graph(fixture: &Fixture, token: &str) -> Value {
    let response = get(fixture, token, "/v1/graph?limit=2000");
    assert_eq!(response.status.as_u16(), 200, "{}", body_text(&response));
    response.json()
}

/// Every string the map answers with, for a "never names it" check.
fn graph_text(answer: &Value) -> String {
    answer.to_string()
}

#[test]
fn the_graph_draws_only_what_the_grant_opens() {
    let fixture = wider_world();
    // Nothing granted: an empty map, not even the shared space.
    let none = graph(&fixture, TOKEN_AGENT);
    assert_eq!(none["nodes"], json!([]), "{none}");
    assert_eq!(none["edges"], json!([]), "{none}");

    grant_claude_folder(&fixture, &format!("agent:{AGENT}"), Permission::Read);
    let answer = graph(&fixture, TOKEN_AGENT);
    let text = graph_text(&answer);
    let drawn: Vec<String> = answer["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .filter_map(|node| node["slug"].as_str().map(str::to_owned))
        .collect();
    let mut sorted = drawn.clone();
    sorted.sort();
    assert_eq!(
        sorted,
        vec![CLAUDE.to_owned(), SECRET.to_owned()],
        "{answer}"
    );
    // The links out of the granted page to the diary and to codex, the
    // sessions, the GitHub commits (personal and shared) and the facts are
    // all outside a pages grant on one folder: none is drawn or named.
    for absent in [
        DIARY,
        CODEX,
        CLAUDETTE,
        "team-handbook",
        "private-session",
        "private-sha",
        "shared-sha",
        "person/zed",
        "person/yan",
        "zed",
        "yan",
    ] {
        assert!(!text.contains(absent), "{absent} leaked: {text}");
    }

    // The other member sees the shared GitHub commit, never a's.
    let theirs = graph_text(&graph(&fixture, TOKEN_B));
    assert!(!theirs.contains("private-sha") && !theirs.contains("private-session"));
    assert!(
        !theirs.contains(DIARY) && !theirs.contains("zed"),
        "{theirs}"
    );
}

#[test]
fn github_and_session_sources_list_only_where_granted() {
    let fixture = wider_world();
    let listed_paths = |token: &str| -> String {
        let response = get(&fixture, token, "/v1/sources?limit=100");
        assert_eq!(response.status.as_u16(), 200, "{}", body_text(&response));
        body_text(&response)
    };
    let mine = listed_paths(TOKEN_A);
    assert!(mine.contains("private-sha") && mine.contains("shared-sha"));
    let b = listed_paths(TOKEN_B);
    assert!(
        b.contains("shared-sha") && !b.contains("private-sha") && !b.contains("private-session")
    );
    assert!(!listed_paths(TOKEN_AGENT).contains("sha"));

    // A sources grant on one project of a's space opens that project only.
    grant(
        &fixture,
        WORKSPACE,
        &format!("agent:{AGENT}"),
        "personal:user-a",
        "project/alpha",
        "sources",
        Permission::Read,
        false,
    );
    let agent = listed_paths(TOKEN_AGENT);
    assert!(
        agent.contains("private-sha") && agent.contains("private-session"),
        "{agent}"
    );
    assert!(!agent.contains("shared-sha"), "{agent}");
    let projects = body_text(&get(&fixture, TOKEN_AGENT, "/v1/sources/projects"));
    assert!(
        projects.contains("alpha") && !projects.contains("beta"),
        "{projects}"
    );
    let map = graph_text(&graph(&fixture, TOKEN_AGENT));
    assert!(
        map.contains("private-sha") && !map.contains("shared-sha"),
        "{map}"
    );
    assert!(
        !map.contains(DIARY),
        "a sources grant is not a pages grant: {map}"
    );
}

#[test]
fn a_fact_subject_grant_opens_that_subject_everywhere_and_nothing_else() {
    let fixture = wider_world();
    grant(
        &fixture,
        WORKSPACE,
        &format!("agent:{AGENT}"),
        "personal:user-a",
        "person/zed",
        "facts",
        Permission::Read,
        false,
    );
    let facts = body_text(&get(&fixture, TOKEN_AGENT, "/v1/facts"));
    assert!(
        facts.contains("reviewer") && !facts.contains("orchid-keeper"),
        "{facts}"
    );
    let map = graph_text(&graph(&fixture, TOKEN_AGENT));
    assert!(map.contains("zed") && !map.contains("yan"), "{map}");
    let searched = found(&fixture, TOKEN_AGENT, "orchid", 25);
    assert!(!searched.contains("orchid-keeper"), "{searched}");
}

#[test]
fn an_agent_key_cannot_drive_a_members_queue_jobs_or_runner() {
    let fixture = wider_world();
    grant(
        &fixture,
        WORKSPACE,
        &format!("agent:{AGENT}"),
        "personal:user-a",
        "",
        "*",
        Permission::Write,
        false,
    );
    for (path, body) in [
        ("/v1/coding-agents/process", Some(json!({}))),
        ("/v1/coding-agents/retry", Some(json!({}))),
        (
            "/v1/coding-agents/compile-fully",
            Some(json!({"source_id": "01ABC"})),
        ),
    ] {
        let response = post(&fixture, TOKEN_AGENT, path, body.unwrap_or(json!({})));
        assert_eq!(
            response.status.as_u16(),
            403,
            "{path}: {}",
            body_text(&response)
        );
    }
    for path in [
        "/v1/coding-agents/queue",
        "/v1/model-jobs",
        "/v1/model-jobs/next",
    ] {
        let response = get(&fixture, TOKEN_AGENT, path);
        assert_eq!(
            response.status.as_u16(),
            403,
            "{path}: {}",
            body_text(&response)
        );
    }
    // The page tree of compiled sessions is a read, through the grant.
    let tree = get(&fixture, TOKEN_AGENT, "/v1/coding-agents/tree");
    assert_eq!(tree.status.as_u16(), 200, "{}", body_text(&tree));
    // A member's own queue still answers.
    let queue = get(&fixture, TOKEN_A, "/v1/coding-agents/queue");
    assert_eq!(queue.status.as_u16(), 200, "{}", body_text(&queue));
}

#[test]
fn a_read_only_token_cannot_start_a_compile() {
    let fixture = wider_world();
    // A member token cut to reading its own space lists the queue but its
    // say-so compiles nothing (a compile writes pages).
    let response = post(
        &fixture,
        TOKEN_A_READ,
        "/v1/coding-agents/process",
        json!({}),
    );
    assert_eq!(response.status.as_u16(), 200, "{}", body_text(&response));
    assert_eq!(
        response.json()["processed"],
        json!([]),
        "{}",
        body_text(&response)
    );
}
