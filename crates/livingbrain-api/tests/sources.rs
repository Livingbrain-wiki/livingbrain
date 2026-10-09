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
    let secret = "sk-ant-aaaabbbbccccddddeeee";
    let ingested = pollster::block_on(fixture.ingestor(defer).ingest(SourceIngest {
        kind: SourceKind::Chat,
        workspace: WORKSPACE.to_owned(),
        scope: scope_of(WORKSPACE, "user-a"),
        origin_ref: Some("msg_0182".to_owned()),
        author: Some("user-a".to_owned()),
        imported_by: "user-a".to_owned(),
        rel_path: String::new(),
        body: format!("the key is {secret} in the log"),
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
