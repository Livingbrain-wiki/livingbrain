//! The brain map's data: `GET /v1/graph` answers each asker with the nodes
//! and edges of exactly what they may read — their own personal pages and
//! sources, the workspace's shared ones, nothing of another member's and
//! nothing of another workspace's — with the filters applied before the
//! limit and the limit clamped.

mod common;

use std::collections::BTreeSet;

use common::{
    OTHER_WORKSPACE, TOKEN_A, TOKEN_A_ELSEWHERE, TOKEN_A_NARROW, TOKEN_B, WORKSPACE, body_text,
    fixture, get, get_anon, is_problem, post, scope_of, seed, status_of, user,
};
use livingbrain_access::Scope;
use livingbrain_pages::{SourceKind, SourceWrite};
use serde_json::{Value, json};

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

/// The feed's node ids in an answer, in order.
fn feed_nodes(answer: &Value) -> Vec<String> {
    answer["events"]
        .as_array()
        .expect("events")
        .iter()
        .map(|event| event["node"].as_str().expect("a node").to_owned())
        .collect()
}

#[test]
fn the_live_feed_records_reads_shows_only_what_the_asker_reads_and_moves_its_cursor() {
    let fixture = world();
    let a_scope = scope_of(WORKSPACE, "user-a");
    let b_scope = scope_of(WORKSPACE, "user-b");
    // Each member reads their own page through the API: the read is recorded.
    assert_eq!(get(&fixture, TOKEN_A, "/v1/pages/a-secret").status, 200);
    assert_eq!(get(&fixture, TOKEN_B, "/v1/pages/b-secret").status, 200);
    // A's read of B's page is refused, so nothing of it is recorded either.
    assert_eq!(get(&fixture, TOKEN_A, "/v1/pages/b-secret").status, 404);

    let feed = |token: &str, query: &str| {
        let response = get(&fixture, token, &format!("/v1/graph/activity{query}"));
        assert_eq!(response.status, 200, "{}", body_text(&response));
        response.json()
    };
    let a = feed(TOKEN_A, "");
    assert_eq!(feed_nodes(&a), [format!("page:{a_scope}:a-secret")]);
    assert_eq!(a["events"][0]["kind"], "read");
    assert_eq!(a["events"][0]["actor"], "web");
    assert_eq!(a["events"][0]["label"], "read by web");

    let b = feed(TOKEN_B, "");
    assert_eq!(feed_nodes(&b), [format!("page:{b_scope}:b-secret")]);
    assert!(
        !b.to_string().contains("a-secret"),
        "A's page is not in B's feed"
    );

    // The cursor: A's next answer holds only what came after it.
    let cursor = a["cursor"].as_str().expect("a cursor").to_owned();
    assert!(
        feed(TOKEN_A, &format!("?since={cursor}"))["events"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(get(&fixture, TOKEN_A, "/v1/pages/a-secret").status, 200);
    let after = feed(TOKEN_A, &format!("?since={cursor}"));
    assert_eq!(feed_nodes(&after), [format!("page:{a_scope}:a-secret")]);
    assert!(
        after["cursor"].as_str().unwrap().parse::<i64>().unwrap() > cursor.parse::<i64>().unwrap()
    );

    // A bad cursor is refused, and no credential is refused.
    assert_eq!(
        get(&fixture, TOKEN_A, "/v1/graph/activity?since=soon").status,
        400
    );
    assert_eq!(get_anon(&fixture, "/v1/graph/activity").status, 401);
}

/// Pages in the large brain: each links to the next two, so the link table
/// is as wide as the page table.
const BIG_PAGES: usize = 2_100;
/// Fact subjects in the large brain. Every fifth names a page (and is
/// counted onto it); the rest are fact nodes of their own.
const BIG_SUBJECTS: usize = 400;
/// The fact subjects that name no page of the brain.
const BIG_FACT_NODES: usize = BIG_SUBJECTS - BIG_SUBJECTS / 5;
/// Each page links to two others.
const BIG_LINKS: usize = 2 * BIG_PAGES;
/// What a Worker's buffered response bridge will carry: an ordinary body past
/// this is refused with `length limit exceeded`.
const BRIDGE_BUFFER: usize = 1024 * 1024;

/// A brain past the size every earlier map was tested at: [`BIG_PAGES`] linked
/// pages and [`BIG_SUBJECTS`] fact subjects in user-a's own scope, written
/// through the routes, read through a database that enforces D1's bound
/// ceiling.
fn big_brain() -> common::Fixture {
    let fixture = common::fixture_bind_limited();
    let scope = user("user-a");
    for i in 0..BIG_PAGES {
        let body = format!(
            "---\ntitle: Page {i}\n---\nSee [[p-{:04}]] and [[p-{:04}]].",
            (i + 1) % BIG_PAGES,
            (i + 2) % BIG_PAGES,
        );
        seed(&fixture, WORKSPACE, &scope, &format!("p-{i:04}"), &body);
    }
    let facts: Vec<Value> = (0..BIG_SUBJECTS)
        .map(|i| {
            let subject = if i % 5 == 0 {
                format!("person/p-{i:04}")
            } else {
                format!("customer/c-{i:04}")
            };
            json!({
                "subject": subject,
                "attribute": "tier",
                "value": "gold",
                "observed_at": "2026-10-01T00:00:00Z",
            })
        })
        .collect();
    for batch in facts.chunks(50) {
        let response = post(&fixture, TOKEN_A, "/v1/facts", json!({ "facts": batch }));
        assert_eq!(response.status, 201, "a fact batch is refused");
    }
    fixture
}

/// The map as the Worker bridge reads it: the route's body must be a
/// streamed body (a `ResponseStream`), which the bridge takes as its stream
/// without the ceiling an ordinary body is held to. Returns the parsed answer
/// and how many bytes it streamed.
fn streamed_map(fixture: &common::Fixture, token: &str, query: &str) -> (Value, usize) {
    use cratefield_core::ResponseStream;
    use cratefield_core::axum::body::Body;
    use cratefield_core::axum::http::{Method, Request, header};
    use futures_util::StreamExt;
    use tower::ServiceExt;

    let request = Request::builder()
        .method(Method::GET)
        .uri(format!("/v1/graph{query}"))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("a request");
    let response = pollster::block_on(fixture.router.clone().oneshot(request)).expect("answers");
    assert_eq!(response.status(), 200, "the map answers 200");
    let (parts, _) = response.into_parts();
    assert_eq!(
        parts
            .headers
            .get(header::CONTENT_TYPE)
            .map(|v| v.as_bytes()),
        Some(b"application/json".as_slice())
    );
    let stream = parts
        .extensions
        .get::<ResponseStream>()
        .and_then(ResponseStream::take)
        .expect("the map is a streamed body, not one buffered under the bridge's ceiling");
    let chunks: Vec<_> = pollster::block_on(stream.collect());
    let bytes: Vec<u8> = chunks
        .into_iter()
        .flat_map(|chunk| chunk.expect("a clean chunk").to_vec())
        .collect();
    let len = bytes.len();
    (serde_json::from_slice(&bytes).expect("JSON"), len)
}

#[test]
fn a_large_brain_maps_within_the_d1_ceiling_and_past_the_bridge_buffer() {
    let fixture = big_brain();
    let (answer, len) = streamed_map(&fixture, TOKEN_A, "?limit=2000");

    // Past the ceiling an ordinary body is refused at: the answer is streamed
    // because it has to be. A buffered answer of this size is the 500.
    assert!(
        len > BRIDGE_BUFFER,
        "the answer is {len} bytes, past the bridge's {BRIDGE_BUFFER}"
    );

    let counts = &answer["counts"];
    assert_eq!(counts["nodes"], 2000, "clamped to the asked limit");
    assert_eq!(answer["nodes"].as_array().unwrap().len(), 2000);
    assert_eq!(counts["matched"], BIG_PAGES + BIG_FACT_NODES);
    assert_eq!(counts["nodes_total"], BIG_PAGES + BIG_FACT_NODES);
    assert_eq!(counts["edges_total"], BIG_LINKS);
    assert_eq!(answer["truncated"], true);

    // Every kept edge joins two kept nodes.
    let kept = ids(&answer);
    assert_eq!(kept.len(), 2000);
    for edge in answer["edges"].as_array().unwrap() {
        assert!(kept.contains(edge["source"].as_str().unwrap()), "{edge}");
        assert!(kept.contains(edge["target"].as_str().unwrap()), "{edge}");
    }

    // The bind ceiling held for every statement the map ran.
    let limited = fixture.limited.as_ref().expect("a bind-limited fixture");
    assert!(
        limited.widest() > 0,
        "the map read through the limited view"
    );
    assert!(
        limited.widest() <= livingbrain_d1_fake::D1_MAX_BINDS,
        "widest statement bound {} values",
        limited.widest()
    );

    // A filter still finds its nodes past the cap: the fact nodes are all
    // there, counted, when asked for by kind.
    let (facts, _) = streamed_map(&fixture, TOKEN_A, "?kind=fact&limit=2000");
    assert_eq!(facts["counts"]["matched"], BIG_FACT_NODES);
    assert_eq!(facts["counts"]["nodes"], BIG_FACT_NODES);
    assert_eq!(facts["truncated"], false);
}
