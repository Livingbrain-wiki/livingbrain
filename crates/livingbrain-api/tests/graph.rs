//! The brain map's data: `GET /v1/graph` answers each asker with the nodes
//! and edges of exactly what they may read — their own personal pages and
//! sources, the workspace's shared ones, nothing of another member's and
//! nothing of another workspace's — with the filters applied before the
//! limit and the limit clamped.

mod common;

use std::collections::BTreeSet;

use common::{
    OTHER_WORKSPACE, TOKEN_A, TOKEN_A_ELSEWHERE, TOKEN_A_NARROW, TOKEN_B, WORKSPACE, body_text,
    fixture, get, get_anon, is_problem, scope_of, seed, status_of, user,
};
use livingbrain_access::Scope;
use livingbrain_pages::{SourceKind, SourceWrite};
use serde_json::Value;

/// One agent session, filed in `scope` of `workspace` under `project`.
fn session(
    fixture: &common::Fixture,
    workspace: &str,
    scope: &str,
    name: &str,
    project: &str,
) -> String {
    pollster::block_on(fixture.sources.put(
        scope,
        SourceWrite {
            kind: SourceKind::AgentLog,
            workspace: workspace.to_owned(),
            origin_ref: None,
            author: None,
            rel_path: format!("claude_code session {name}"),
            markdown: format!("session {name}"),
            imported_by: "user-a".to_owned(),
            held: false,
            project: Some(project.to_owned()),
        },
    ))
    .expect("a seed session")
    .0
    .id
}

/// The world: a's private page, b's private page, a shared hub that links
/// to both of them by slug, sessions for a and for b, and a page and a
/// session in another workspace under the same user id as a.
fn world() -> common::Fixture {
    let fixture = fixture();
    seed(
        &fixture,
        WORKSPACE,
        &user("user-a"),
        "a-secret",
        "---\ntitle: A secret\n---\nA's own page.",
    );
    seed(
        &fixture,
        WORKSPACE,
        &user("user-b"),
        "b-secret",
        "---\ntitle: B secret\n---\nB's own page.",
    );
    seed(
        &fixture,
        WORKSPACE,
        &Scope::Shared,
        "hub",
        "---\ntitle: Hub\n---\nSee [[a-secret]] and [[b-secret]] and [[spoke]].",
    );
    seed(
        &fixture,
        WORKSPACE,
        &Scope::Shared,
        "spoke",
        "---\ntitle: Spoke\n---\nBack to [[hub]].",
    );
    session(
        &fixture,
        WORKSPACE,
        &scope_of(WORKSPACE, "user-a"),
        "a1",
        "alpha",
    );
    session(
        &fixture,
        WORKSPACE,
        &scope_of(WORKSPACE, "user-b"),
        "b1",
        "beta",
    );
    seed(
        &fixture,
        OTHER_WORKSPACE,
        &user("user-a"),
        "elsewhere",
        "---\ntitle: Elsewhere\n---\nOther workspace.",
    );
    session(
        &fixture,
        OTHER_WORKSPACE,
        &scope_of(OTHER_WORKSPACE, "user-a"),
        "x1",
        "gamma",
    );
    fixture
}

fn map(fixture: &common::Fixture, token: &str, query: &str) -> Value {
    let response = get(fixture, token, &format!("/v1/graph{query}"));
    assert_eq!(response.status, 200, "{}", body_text(&response));
    response.json()
}

fn slugs(answer: &Value) -> BTreeSet<String> {
    answer["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .filter_map(|node| node["slug"].as_str().map(str::to_owned))
        .collect()
}

fn labels(answer: &Value, kind: &str) -> BTreeSet<String> {
    answer["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .filter(|node| node["kind"] == kind)
        .filter_map(|node| node["label"].as_str().map(str::to_owned))
        .collect()
}

fn ids(answer: &Value) -> BTreeSet<String> {
    answer["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .map(|node| node["id"].as_str().expect("an id").to_owned())
        .collect()
}

#[test]
fn each_member_sees_their_own_and_the_shared_nodes_only() {
    let fixture = world();

    let a = map(&fixture, TOKEN_A, "");
    assert_eq!(
        slugs(&a),
        BTreeSet::from(["a-secret", "hub", "spoke"].map(str::to_owned))
    );
    assert_eq!(
        labels(&a, "session"),
        BTreeSet::from(["claude_code session a1".to_owned()])
    );
    assert_eq!(labels(&a, "project"), BTreeSet::from(["alpha".to_owned()]));

    let b = map(&fixture, TOKEN_B, "");
    assert_eq!(
        slugs(&b),
        BTreeSet::from(["b-secret", "hub", "spoke"].map(str::to_owned))
    );
    assert_eq!(
        labels(&b, "session"),
        BTreeSet::from(["claude_code session b1".to_owned()])
    );
    assert!(
        !labels(&b, "project").contains("alpha"),
        "a's project is not b's to see"
    );

    // Another workspace, the same user id: only that workspace's nodes.
    let elsewhere = map(&fixture, TOKEN_A_ELSEWHERE, "");
    assert_eq!(slugs(&elsewhere), BTreeSet::from(["elsewhere".to_owned()]));
    assert_eq!(
        labels(&elsewhere, "session"),
        BTreeSet::from(["claude_code session x1".to_owned()])
    );

    // A token narrowed to the shared scope narrows the map.
    let narrow = map(&fixture, TOKEN_A_NARROW, "");
    assert_eq!(
        slugs(&narrow),
        BTreeSet::from(["hub", "spoke"].map(str::to_owned))
    );
    assert!(labels(&narrow, "session").is_empty());
}

#[test]
fn a_link_to_a_page_the_asker_cannot_read_is_not_drawn() {
    let fixture = world();
    let b = map(&fixture, TOKEN_B, "");
    let readable = ids(&b);
    for edge in b["edges"].as_array().expect("edges") {
        assert!(
            readable.contains(edge["source"].as_str().unwrap()),
            "{edge}"
        );
        assert!(
            readable.contains(edge["target"].as_str().unwrap()),
            "{edge}"
        );
    }
    let text = b.to_string();
    assert!(
        !text.contains("a-secret"),
        "not even as an edge end: {text}"
    );
    // The links b can see are drawn: hub ↔ spoke, hub → b-secret.
    let links: BTreeSet<(String, String)> = b["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|edge| edge["kind"] == "link")
        .map(|edge| {
            let name = |id: &str| id.rsplit(':').next().unwrap().to_owned();
            (
                name(edge["source"].as_str().unwrap()),
                name(edge["target"].as_str().unwrap()),
            )
        })
        .collect();
    assert_eq!(
        links,
        BTreeSet::from([
            ("hub".to_owned(), "b-secret".to_owned()),
            ("hub".to_owned(), "spoke".to_owned()),
            ("spoke".to_owned(), "hub".to_owned()),
        ])
    );
}

#[test]
fn the_filters_apply_before_the_limit_and_the_limit_is_clamped() {
    let fixture = world();
    // One node, of the asked kind, however many better-connected pages exist.
    let sessions = map(&fixture, TOKEN_A, "?kind=session&limit=1");
    assert_eq!(sessions["nodes"].as_array().unwrap().len(), 1);
    assert_eq!(sessions["nodes"][0]["kind"], "session");
    assert_eq!(sessions["counts"]["matched"], 1);

    let project = map(&fixture, TOKEN_A, "?project=alpha");
    let kinds: BTreeSet<String> = project["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| node["kind"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        kinds,
        BTreeSet::from(["project".to_owned(), "session".to_owned()])
    );
    assert_eq!(
        project["edges"].as_array().unwrap().len(),
        1,
        "the member edge"
    );

    let one = map(&fixture, TOKEN_A, "?limit=1");
    assert_eq!(one["nodes"].as_array().unwrap().len(), 1);
    assert_eq!(one["truncated"], true);
    assert!(one["counts"]["nodes_total"].as_u64().unwrap() > 1);
    assert!(
        one["edges"].as_array().unwrap().is_empty(),
        "no edge has both ends kept"
    );

    let all = map(&fixture, TOKEN_A, "?limit=100000");
    assert_eq!(all["limit"], 2000, "clamped, not refused");
}

#[test]
fn a_bad_query_is_a_400_and_no_credential_is_a_401() {
    let fixture = world();
    for query in ["?kind=planet", "?limit=many", "?project=..%2Fx"] {
        let response = get(&fixture, TOKEN_A, &format!("/v1/graph{query}"));
        assert_eq!(response.status, 400, "{query}: {}", body_text(&response));
        assert!(is_problem(&response));
    }
    let anon = get_anon(&fixture, "/v1/graph");
    assert_eq!(status_of(&anon), 401, "{}", body_text(&anon));
    assert!(is_problem(&anon));
}
