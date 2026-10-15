//! The source citation route: `GET /v1/sources/{id}` answers a member whose
//! grant includes the source's scope, and answers everybody else — another
//! member of the same workspace, another workspace, nobody at all — with
//! the same 404 an unknown id gets.
//!
//! The sources a test reads are seeded straight into the ledger over the
//! fixture's own ports; the pipeline test at the bottom puts one through the
//! real ingest to pin that the body the route serves is the redacted body.

mod common;

use std::sync::Arc;

use common::{
    TOKEN_A, TOKEN_B, WORKSPACE, body_text, fixture, get, get_anon, is_problem, scope_of, status_of,
};
use livingbrain_access::Scope;
use livingbrain_pages::{SourceIngest, SourceKind, SourceWrite};

/// Seeds one source into `scope` as `importer` filed it, and returns the row.
fn seed(
    fixture: &common::Fixture,
    scope: &str,
    markdown: &str,
    held: bool,
) -> livingbrain_pages::Source {
    pollster::block_on(fixture.sources.put(
        scope,
        SourceWrite {
            kind: SourceKind::Import,
            workspace: WORKSPACE.to_owned(),
            origin_ref: Some("https://example.com/one".to_owned()),
            author: None,
            rel_path: "notes/one.md".to_owned(),
            markdown: markdown.to_owned(),
            imported_by: "user-a".to_owned(),
            held,
            project: None,
        },
    ))
    .expect("a seed import")
    .0
}

#[test]
fn a_source_reads_for_a_member_whose_scope_includes_it() {
    let fixture = fixture();
    let scope = scope_of(WORKSPACE, "user-a");
    let source = seed(&fixture, &scope, "# One\n\nA note.\n", false);

    let response = get(&fixture, TOKEN_A, &format!("/v1/sources/{}", source.id));
    assert_eq!(response.status, 200, "{}", body_text(&response));
    let answer = response.json();
    assert_eq!(answer["id"], source.id, "{answer}");
    assert_eq!(answer["kind"], "import", "{answer}");
    assert_eq!(answer["scope"], scope, "{answer}");
    assert_eq!(answer["workspace"], WORKSPACE, "{answer}");
    assert_eq!(answer["origin_ref"], "https://example.com/one", "{answer}");
    assert_eq!(answer["held"], false, "{answer}");
    assert_eq!(answer["sha256"], source.body_sha256, "{answer}");
    assert_eq!(answer["body"], "# One\n\nA note.\n", "{answer}");
}

#[test]
fn an_out_of_scope_member_gets_the_same_404_as_an_unknown_id() {
    let fixture = fixture();
    let source = seed(
        &fixture,
        &scope_of(WORKSPACE, "user-a"),
        "# Private.\n",
        false,
    );

    // Another member of the same workspace: the scope is not theirs.
    let theirs = get(&fixture, TOKEN_B, &format!("/v1/sources/{}", source.id));
    assert_eq!(theirs.status, 404, "{}", body_text(&theirs));
    assert!(is_problem(&theirs), "{}", body_text(&theirs));

    // An id nobody has: the same problem, indistinguishably.
    let unknown = get(&fixture, TOKEN_A, "/v1/sources/01J00000000000000000000000");
    assert_eq!(unknown.status, 404, "{}", body_text(&unknown));
    assert!(is_problem(&unknown), "{}", body_text(&unknown));
    assert_eq!(
        body_text(&theirs),
        body_text(&unknown),
        "absent and unreadable are the same answer"
    );
    assert_eq!(
        theirs.json()["type"],
        unknown.json()["type"],
        "one problem slug for the whole 404 family"
    );
}

#[test]
fn a_shared_source_reads_for_every_member_of_the_workspace() {
    let fixture = fixture();
    let shared = livingbrain_pages::page_scope(WORKSPACE, &Scope::Shared);
    let source = seed(&fixture, &shared, "# Shared.\n", false);
    for token in [TOKEN_A, TOKEN_B] {
        let response = get(&fixture, token, &format!("/v1/sources/{}", source.id));
        assert_eq!(response.status, 200, "{token}: {}", body_text(&response));
    }
}

#[test]
fn a_held_source_still_reads_flagged_held() {
    let fixture = fixture();
    // Screening's hold is about extraction, not secrecy: the row is in the
    // ledger, and a member who may read it does — told it is held.
    let source = seed(&fixture, &scope_of(WORKSPACE, "user-a"), "# Held.\n", true);
    let response = get(&fixture, TOKEN_A, &format!("/v1/sources/{}", source.id));
    assert_eq!(response.status, 200, "{}", body_text(&response));
    assert_eq!(response.json()["held"], true, "{}", body_text(&response));
}

#[test]
fn a_source_without_a_credential_is_401() {
    let fixture = fixture();
    let source = seed(&fixture, &scope_of(WORKSPACE, "user-a"), "# One.\n", false);
    let response = get_anon(&fixture, &format!("/v1/sources/{}", source.id));
    assert_eq!(status_of(&response), 401, "{}", body_text(&response));
    assert!(is_problem(&response), "{}", body_text(&response));
}

#[test]
fn the_body_the_route_serves_is_the_body_the_pipeline_stored() {
    let fixture = fixture();
    // Through the real pipeline, not the seed helper: whatever it redacted
    // is what the citation serves.
    let defer = Arc::new(cratefield_testing::FakeDefer::new());
    // Assembled from fragments so push protection does not read the fixture
    // as a leaked key; the whole still matches the redactor's Anthropic shape.
    let secret = concat!("sk-", "ant-", "aaaabbbb", "ccccdddd", "eeee");
    let ingested = pollster::block_on(fixture.ingestor(defer).ingest(SourceIngest {
        kind: SourceKind::Chat,
        workspace: WORKSPACE.to_owned(),
        scope: scope_of(WORKSPACE, "user-a"),
        origin_ref: Some("msg_0182".to_owned()),
        author: Some("user-a".to_owned()),
        imported_by: "user-a".to_owned(),
        rel_path: String::new(),
        body: format!("the key is {secret} in the log"),
        project: None,
    }))
    .expect("an ingest");
    assert!(
        ingested.redactions > 0,
        "the pipeline found the secret: {ingested:?}"
    );

    let response = get(
        &fixture,
        TOKEN_A,
        &format!("/v1/sources/{}", ingested.source.id),
    );
    assert_eq!(response.status, 200, "{}", body_text(&response));
    let answer = response.json();
    assert_eq!(answer["kind"], "chat", "{answer}");
    assert_eq!(answer["origin_ref"], "msg_0182", "{answer}");
    assert_eq!(answer["author"], "user-a", "{answer}");
    let body = answer["body"].as_str().expect("a body");
    assert!(
        !body.contains(secret),
        "the secret never reads back: {body}"
    );
    assert!(
        body.contains("[REDACTED:"),
        "the redaction token stands in the body: {body}"
    );
}

/// Seeds one agent session filed under `project` (or none) into `scope` of
/// `workspace`, and returns its id.
fn seed_session(
    fixture: &common::Fixture,
    workspace: &str,
    scope: &str,
    body: &str,
    project: Option<&str>,
) -> String {
    pollster::block_on(fixture.sources.put(
        scope,
        SourceWrite {
            kind: SourceKind::AgentLog,
            workspace: workspace.to_owned(),
            origin_ref: None,
            author: None,
            rel_path: format!("claude-code session {body}"),
            markdown: body.to_owned(),
            imported_by: "user-a".to_owned(),
            held: false,
            project: project.map(str::to_owned),
        },
    ))
    .expect("a seed session")
    .0
    .id
}

/// The ids a listing answered with, in its order.
fn listed_ids(answer: &serde_json::Value) -> Vec<String> {
    answer["sources"]
        .as_array()
        .expect("a sources array")
        .iter()
        .map(|source| source["id"].as_str().expect("an id").to_owned())
        .collect()
}

/// The world every listing test reads: `user-a`'s two personal sessions,
/// one shared, `user-b`'s personal one, and one in another workspace whose
/// member has `user-a`'s very id — three of them under the same project.
struct Seeded {
    a_demo: String,
    a_other: String,
    shared_demo: String,
    b_demo: String,
    elsewhere_demo: String,
}

fn seeded(fixture: &common::Fixture) -> Seeded {
    let shared = livingbrain_pages::page_scope(WORKSPACE, &Scope::Shared);
    Seeded {
        a_demo: seed_session(
            fixture,
            WORKSPACE,
            &scope_of(WORKSPACE, "user-a"),
            "a-demo",
            Some("demo"),
        ),
        a_other: seed_session(
            fixture,
            WORKSPACE,
            &scope_of(WORKSPACE, "user-a"),
            "a-other",
            Some("other"),
        ),
        shared_demo: seed_session(fixture, WORKSPACE, &shared, "shared-demo", Some("demo")),
        b_demo: seed_session(
            fixture,
            WORKSPACE,
            &scope_of(WORKSPACE, "user-b"),
            "b-demo",
            Some("b-private"),
        ),
        elsewhere_demo: seed_session(
            fixture,
            common::OTHER_WORKSPACE,
            &scope_of(common::OTHER_WORKSPACE, "user-a"),
            "elsewhere-demo",
            Some("elsewhere"),
        ),
    }
}

#[test]
fn the_listing_is_the_askers_own_sources_and_the_shared_ones() {
    let fixture = fixture();
    let world = seeded(&fixture);

    let response = get(&fixture, TOKEN_A, "/v1/sources");
    assert_eq!(response.status, 200, "{}", body_text(&response));
    let answer = response.json();
    let mut ids = listed_ids(&answer);
    // Newest first is id order, descending.
    let mut sorted = ids.clone();
    sorted.sort();
    sorted.reverse();
    assert_eq!(ids, sorted, "{answer}");
    ids.sort();
    let mut want = vec![
        world.a_demo.clone(),
        world.a_other.clone(),
        world.shared_demo.clone(),
    ];
    want.sort();
    assert_eq!(ids, want, "user-a's two and the shared one: {answer}");
    assert_eq!(answer["next_cursor"], serde_json::Value::Null, "{answer}");

    // No bodies, ever; the scope is named as the kind of memory it is.
    for source in answer["sources"].as_array().expect("sources") {
        assert!(source.get("body").is_none(), "{source}");
        assert_eq!(source["kind"], "agent_log", "{source}");
        let expected = if source["id"] == world.shared_demo.as_str() {
            "shared"
        } else {
            "personal"
        };
        assert_eq!(source["scope"], expected, "{source}");
    }

    // user-b sees their own and the shared one — never user-a's.
    let theirs = listed_ids(&get(&fixture, TOKEN_B, "/v1/sources").json());
    assert!(theirs.contains(&world.b_demo), "{theirs:?}");
    assert!(theirs.contains(&world.shared_demo), "{theirs:?}");
    assert!(!theirs.contains(&world.a_demo), "{theirs:?}");
    assert!(!theirs.contains(&world.a_other), "{theirs:?}");

    // The same user id in another workspace sees only that workspace.
    let elsewhere = listed_ids(&get(&fixture, common::TOKEN_A_ELSEWHERE, "/v1/sources").json());
    assert_eq!(
        elsewhere,
        vec![world.elsewhere_demo.clone()],
        "{elsewhere:?}"
    );
}

#[test]
fn the_listing_filters_by_project_and_kind() {
    let fixture = fixture();
    let world = seeded(&fixture);

    let mut demo = listed_ids(&get(&fixture, TOKEN_A, "/v1/sources?project=demo").json());
    demo.sort();
    let mut want = vec![world.a_demo.clone(), world.shared_demo.clone()];
    want.sort();
    assert_eq!(demo, want);

    // Another member's project name finds nothing for this asker — the
    // filter narrows the asker's rows, it never widens them.
    let private = get(&fixture, TOKEN_A, "/v1/sources?project=b-private").json();
    assert!(listed_ids(&private).is_empty(), "{private}");

    let imports = get(&fixture, TOKEN_A, "/v1/sources?kind=import").json();
    assert!(listed_ids(&imports).is_empty(), "{imports}");
    let logs = get(
        &fixture,
        TOKEN_A,
        "/v1/sources?kind=agent_log&project=other",
    )
    .json();
    assert_eq!(listed_ids(&logs), vec![world.a_other.clone()], "{logs}");
}

#[test]
fn the_listing_pages_with_its_cursor() {
    let fixture = fixture();
    let _ = seeded(&fixture);
    let all = listed_ids(&get(&fixture, TOKEN_A, "/v1/sources").json());
    assert_eq!(all.len(), 3);

    let first = get(&fixture, TOKEN_A, "/v1/sources?limit=2").json();
    assert_eq!(listed_ids(&first), all[..2].to_vec(), "{first}");
    let cursor = first["next_cursor"]
        .as_str()
        .expect("a full page has a cursor");
    assert_eq!(cursor, all[1], "the cursor is the page's last id");

    let second = get(
        &fixture,
        TOKEN_A,
        &format!("/v1/sources?limit=2&cursor={cursor}"),
    )
    .json();
    assert_eq!(listed_ids(&second), all[2..].to_vec(), "{second}");
    assert_eq!(second["next_cursor"], serde_json::Value::Null, "{second}");
}

#[test]
fn a_query_parameter_outside_its_rule_is_a_400() {
    let fixture = fixture();
    for path in [
        "/v1/sources?kind=telepathy",
        "/v1/sources?project=../etc",
        "/v1/sources?project=has%20space",
        "/v1/sources?limit=lots",
        "/v1/sources?cursor=not%2Fan%2Fid",
        "/v1/sources/projects?kind=telepathy",
    ] {
        let response = get(&fixture, TOKEN_A, path);
        assert_eq!(response.status, 400, "{path}: {}", body_text(&response));
        assert!(is_problem(&response), "{path}: {}", body_text(&response));
    }
}

#[test]
fn the_listings_without_a_credential_are_401() {
    let fixture = fixture();
    for path in ["/v1/sources", "/v1/sources/projects"] {
        let response = get_anon(&fixture, path);
        assert_eq!(
            status_of(&response),
            401,
            "{path}: {}",
            body_text(&response)
        );
        assert!(is_problem(&response), "{path}: {}", body_text(&response));
    }
}

#[test]
fn the_projects_are_the_ones_the_asker_can_list() {
    let fixture = fixture();
    let _ = seeded(&fixture);

    let response = get(&fixture, TOKEN_A, "/v1/sources/projects");
    assert_eq!(response.status, 200, "{}", body_text(&response));
    assert_eq!(
        response.json(),
        serde_json::json!({ "projects": [
            { "project": "demo", "sources": 2 },
            { "project": "other", "sources": 1 },
        ]}),
        "neither user-b's `b-private` nor the other workspace's `elsewhere`"
    );

    let theirs = get(&fixture, TOKEN_B, "/v1/sources/projects").json();
    assert_eq!(
        theirs,
        serde_json::json!({ "projects": [
            { "project": "b-private", "sources": 1 },
            { "project": "demo", "sources": 1 },
        ]})
    );
    let elsewhere = get(&fixture, common::TOKEN_A_ELSEWHERE, "/v1/sources/projects").json();
    assert_eq!(
        elsewhere,
        serde_json::json!({ "projects": [{ "project": "elsewhere", "sources": 1 }] })
    );
    let imports = get(&fixture, TOKEN_A, "/v1/sources/projects?kind=import").json();
    assert_eq!(imports, serde_json::json!({ "projects": [] }));
}

#[test]
fn a_citation_names_the_project_it_is_filed_under() {
    let fixture = fixture();
    let world = seeded(&fixture);
    let answer = get(&fixture, TOKEN_A, &format!("/v1/sources/{}", world.a_demo)).json();
    assert_eq!(answer["project"], "demo", "{answer}");
}
