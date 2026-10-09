//! The export route: one zip of every page the asker can read, one
//! Markdown file per page at `{scope}/{slug}.md`. The zip is read back
//! with a test-side reader, so what is pinned is what a real unzip sees.

mod common;

use common::{
    TOKEN_A, TOKEN_B, WORKSPACE, fixture, get, get_anon, scope_of, seed, status_of, user,
    zip_entries,
};
use cratefield_core::axum::http::{StatusCode, header};
use livingbrain_access::Scope;

const KESTREL: &str = "---\ntitle: Kestrel routes\n---\nThe kestrel routes run at dawn.";
const TAMAR: &str = "---\ntitle: Tamar routes\n---\nThe tamar routes run at dusk.";
const LOGBOOK: &str = "---\ntitle: Logbook\n---\nThe shared logbook, readable by everyone.";

#[test]
fn the_export_is_a_zip_of_readable_markdown_files() {
    let fixture = fixture();
    seed(&fixture, WORKSPACE, &user("user-a"), "kestrel", KESTREL);
    seed(&fixture, WORKSPACE, &user("user-a"), "tamar", TAMAR);
    seed(&fixture, WORKSPACE, &Scope::Shared, "logbook", LOGBOOK);

    let response = get(&fixture, TOKEN_A, "/v1/export");
    assert_eq!(
        status_of(&response),
        StatusCode::OK,
        "{}",
        common::body_text(&response)
    );
    assert_eq!(
        response
            .headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/zip"),
        "{}",
        common::body_text(&response)
    );
    assert_eq!(
        response
            .headers
            .get(header::CONTENT_DISPOSITION)
            .and_then(|value| value.to_str().ok()),
        Some("attachment; filename=\"livingbrain-export.zip\""),
        "{}",
        common::body_text(&response)
    );

    let archive = response.body().as_ref();
    let entries = zip_entries(archive);
    let scope = scope_of(WORKSPACE, "user-a");
    let expected = vec![
        (format!("{scope}/kestrel.md"), KESTREL.as_bytes().to_vec()),
        (format!("{scope}/tamar.md"), TAMAR.as_bytes().to_vec()),
        (
            format!("{}/logbook.md", page_scope_shared()),
            LOGBOOK.as_bytes().to_vec(),
        ),
    ];
    let mut got = entries;
    got.sort_by(|left, right| left.0.cmp(&right.0));
    let mut want = expected;
    want.sort_by(|left, right| left.0.cmp(&right.0));
    assert_eq!(got, want, "name and bytes, exactly");
}

/// The shared page-store scope string, computed the way the store names it.
fn page_scope_shared() -> String {
    livingbrain_mcp::page_scope(WORKSPACE, &Scope::Shared)
}

#[test]
fn the_export_carries_only_the_scopes_the_caller_reads() {
    let fixture = fixture();
    seed(&fixture, WORKSPACE, &user("user-a"), "kestrel", KESTREL);
    seed(
        &fixture,
        WORKSPACE,
        &user("user-b"),
        "burdock",
        "---\ntitle: Burdock\n---\nB's own page.",
    );

    let mine = zip_entries(get(&fixture, TOKEN_A, "/v1/export").body().as_ref());
    let names: Vec<&str> = mine.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        [format!("{}/kestrel.md", scope_of(WORKSPACE, "user-a"))],
        "{names:?}"
    );

    let theirs = zip_entries(get(&fixture, TOKEN_B, "/v1/export").body().as_ref());
    let other: Vec<&str> = theirs.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        other,
        [format!("{}/burdock.md", scope_of(WORKSPACE, "user-b"))],
        "{other:?}"
    );
}

#[test]
fn an_empty_wiki_exports_a_valid_empty_zip() {
    let fixture = fixture();
    let response = get(&fixture, TOKEN_A, "/v1/export");
    assert_eq!(
        status_of(&response),
        StatusCode::OK,
        "{}",
        common::body_text(&response)
    );
    // A valid zip: the 22-byte end-of-central-directory record and nothing else.
    assert_eq!(
        response.body().len(),
        22,
        "{}",
        common::body_text(&response)
    );
    assert!(zip_entries(response.body().as_ref()).is_empty());
}

#[test]
fn an_unknown_format_is_a_422_that_names_the_one_speaks() {
    let fixture = fixture();
    let response = get(&fixture, TOKEN_A, "/v1/export?format=notion");
    assert_eq!(status_of(&response), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        response.json()["title"],
        "Unknown export format",
        "{}",
        common::body_text(&response)
    );
    assert!(
        response.json()["detail"]
            .as_str()
            .expect("a detail")
            .contains("obsidian"),
        "the detail names the supported format: {}",
        common::body_text(&response)
    );

    // The one format it does speak works, `format=` or not.
    for query in ["", "?format=obsidian"] {
        let good = get(&fixture, TOKEN_A, &format!("/v1/export{query}"));
        assert_eq!(
            status_of(&good),
            StatusCode::OK,
            "{}",
            common::body_text(&good)
        );
    }
}

#[test]
fn the_export_answers_401_without_a_credential() {
    let fixture = fixture();
    let response = get_anon(&fixture, "/v1/export");
    assert_eq!(
        status_of(&response),
        StatusCode::UNAUTHORIZED,
        "{}",
        common::body_text(&response)
    );
}
