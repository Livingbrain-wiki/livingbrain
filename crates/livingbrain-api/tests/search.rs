//! The search route: the same recall `brain_search` gives an agent, as
//! data — every page in the asker's scopes whose body holds every word of
//! the query, as a title, a citation URL and a snippet, with the optional
//! `project` filter over the frontmatter a note carries.

mod common;

use common::{TOKEN_A, TOKEN_B, WORKSPACE, fixture, get, get_anon, post, seed, status_of, user};
use livingbrain_access::Scope;
use serde_json::{Value, json};

const KESTREL: &str =
    "---\ntitle: Kestrel routes\n---\nThe kestrel routes run at dawn near the harbour.";
const TAMAR: &str =
    "---\ntitle: Tamar routes\n---\nThe tamar routes run at dusk, unlike the kestrel routes.";

fn results(fixture: &common::Fixture, token: &str, query: &str) -> Vec<Value> {
    let response = get(fixture, token, query);
    assert_eq!(response.status, 200, "{}", common::body_text(&response));
    response.json()["results"]
        .as_array()
        .expect("a results list")
        .clone()
}

#[test]
fn a_search_finds_the_pages_whose_body_holds_every_word() {
    let fixture = fixture();
    seed(&fixture, WORKSPACE, &user("user-a"), "kestrel", KESTREL);
    seed(&fixture, WORKSPACE, &user("user-a"), "tamar", TAMAR);

    // Both bodies hold "routes"; only the kestrel page holds "harbour".
    let both = results(&fixture, TOKEN_A, "/v1/search?q=routes");
    assert_eq!(both.len(), 2, "{both:?}");
    let harbour = results(&fixture, TOKEN_A, "/v1/search?q=harbour");
    assert_eq!(harbour.len(), 1, "{harbour:?}");
    assert_eq!(harbour[0]["title"], "Kestrel routes", "{harbour:?}");

    // Two words are an AND, not an OR: "dusk harbour" is on no one page.
    let none = results(&fixture, TOKEN_A, "/v1/search?q=dusk%20harbour");
    assert_eq!(none.len(), 0, "{none:?}");
}

#[test]
fn a_result_carries_the_citation_a_reader_needs() {
    let fixture = fixture();
    let scope = common::scope_of(WORKSPACE, "user-a");
    seed(&fixture, WORKSPACE, &user("user-a"), "kestrel", KESTREL);

    let found = results(&fixture, TOKEN_A, "/v1/search?q=kestrel");
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0]["title"], "Kestrel routes", "{found:?}");
    assert_eq!(
        found[0]["url"],
        format!("https://livingbrain.wiki/brain/{scope}/kestrel"),
        "{found:?}"
    );
    let snippet = found[0]["snippet"].as_str().expect("a snippet");
    assert!(
        snippet.contains("harbour"),
        "the hit is in there: {snippet}"
    );
}

#[test]
fn a_search_reads_only_the_scopes_the_caller_was_granted() {
    let fixture = fixture();
    seed(&fixture, WORKSPACE, &user("user-a"), "kestrel", KESTREL);
    seed(
        &fixture,
        WORKSPACE,
        &user("user-b"),
        "burdock",
        "---\ntitle: Burdock\n---\nA kestrel flew over the burdock patch.",
    );
    seed(
        &fixture,
        WORKSPACE,
        &Scope::Shared,
        "logbook",
        "---\ntitle: Logbook\n---\nThe kestrel is the shared subject today.",
    );

    // user-a sees their page plus the shared one, never user-b's.
    let mine = results(&fixture, TOKEN_A, "/v1/search?q=kestrel");
    let mut slugs: Vec<String> = mine
        .iter()
        .filter_map(|result| result["url"].as_str())
        .filter_map(|url| url.rsplit('/').next())
        .map(str::to_owned)
        .collect();
    slugs.sort();
    assert_eq!(slugs, ["kestrel", "logbook"], "{mine:?}");

    // user-b sees theirs plus the shared one, never user-a's.
    let theirs = results(&fixture, TOKEN_B, "/v1/search?q=kestrel");
    let mut other: Vec<String> = theirs
        .iter()
        .filter_map(|result| result["url"].as_str())
        .filter_map(|url| url.rsplit('/').next())
        .map(str::to_owned)
        .collect();
    other.sort();
    assert_eq!(other, ["burdock", "logbook"], "{theirs:?}");
}

#[test]
fn the_project_filter_keeps_only_pages_filed_under_it() {
    let fixture = fixture();
    // Two notes with projects and one page without any, all on the subject.
    post(
        &fixture,
        TOKEN_A,
        "/v1/notes",
        json!({ "body": "The atlas migration is staged for Friday.", "project": "atlas" }),
    );
    post(
        &fixture,
        TOKEN_A,
        "/v1/notes",
        json!({ "body": "The zurich migration waits for the atlas one.", "project": "zurich" }),
    );
    seed(
        &fixture,
        WORKSPACE,
        &user("user-a"),
        "migration-notes",
        "---\ntitle: Migration notes\n---\nEvery migration needs a migration runbook.",
    );

    let all = results(&fixture, TOKEN_A, "/v1/search?q=migration");
    assert_eq!(all.len(), 3, "the filter is what narrows: {all:?}");

    let atlas = results(&fixture, TOKEN_A, "/v1/search?q=migration&project=atlas");
    assert_eq!(atlas.len(), 1, "{atlas:?}");
    assert!(
        atlas[0]["snippet"]
            .as_str()
            .expect("a snippet")
            .contains("atlas migration"),
        "{atlas:?}"
    );

    let unfiled = results(&fixture, TOKEN_A, "/v1/search?q=migration&project=nowhere");
    assert_eq!(unfiled.len(), 0, "{unfiled:?}");
}

#[test]
fn the_limit_clamps_instead_of_refusing() {
    let fixture = fixture();
    for slug in ["kestrel", "tamar", "burdock", "dahlia"] {
        seed(
            &fixture,
            WORKSPACE,
            &user("user-a"),
            slug,
            &format!("---\ntitle: {slug}\n---\nThe reconciliation touches {slug}."),
        );
    }

    let two = results(&fixture, TOKEN_A, "/v1/search?q=reconciliation&limit=2");
    assert_eq!(two.len(), 2, "{two:?}");

    // A thousand wanted hits is a wish for hits, not an error: the clamp
    // answers with everything the ceiling allows.
    let clamped = results(&fixture, TOKEN_A, "/v1/search?q=reconciliation&limit=1000");
    assert_eq!(clamped.len(), 4, "{clamped:?}");

    // And zero clamps up to one rather than answering with nothing at all.
    let one = results(&fixture, TOKEN_A, "/v1/search?q=reconciliation&limit=0");
    assert_eq!(one.len(), 1, "{one:?}");
}

#[test]
fn a_missing_or_empty_q_is_a_400() {
    let fixture = fixture();
    for query in ["/v1/search", "/v1/search?q=", "/v1/search?q=%20%20"] {
        let response = get(&fixture, TOKEN_A, query);
        // The kit's standard validation problem.
        assert_eq!(
            status_of(&response),
            400,
            "{query}: {}",
            common::body_text(&response)
        );
        assert_eq!(
            response.json()["detail"],
            "a non-empty `q` is required",
            "{query}: {}",
            common::body_text(&response)
        );
    }
}

#[test]
fn an_unparseable_limit_is_a_400() {
    let fixture = fixture();
    let response = get(&fixture, TOKEN_A, "/v1/search?q=kestrel&limit=yesterday");
    assert_eq!(
        status_of(&response),
        400,
        "{}",
        common::body_text(&response)
    );
    assert_eq!(
        response.json()["detail"],
        "`limit` must be a positive integer, got \"yesterday\"",
        "{}",
        common::body_text(&response)
    );
}

#[test]
fn a_search_answers_401_without_a_credential() {
    let fixture = fixture();
    let response = get_anon(&fixture, "/v1/search?q=kestrel");
    assert_eq!(
        status_of(&response),
        401,
        "{}",
        common::body_text(&response)
    );
}
