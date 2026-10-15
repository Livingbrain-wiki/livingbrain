//! The notes route: a note is a page in the asker's own scope, slug-named
//! after its redacted text so the same text lands on the same page, with
//! redaction counted and the project — when one is sent — riding in the
//! frontmatter fence where `GET /v1/search?project=` finds it.

mod common;

use common::{
    TOKEN_A, TOKEN_B, WORKSPACE, fixture, get, post, post_bad_token, scope_of, status_of,
};
use serde_json::{Value, json};

/// Posts one note and returns the answer body.
fn note(fixture: &common::Fixture, token: &str, body: Value) -> Value {
    let response = post(fixture, token, "/v1/notes", body);
    assert_eq!(response.status, 200, "{}", common::body_text(&response));
    response.json()
}

/// The slug half of a note answer's `id` (`{scope}/{slug}`).
fn slug_of(answer: &Value) -> &str {
    answer["id"]
        .as_str()
        .and_then(|id| id.split('/').next_back())
        .expect("an id of {scope}/{slug}")
}

#[test]
fn a_note_is_a_page_in_the_askers_own_scope() {
    let fixture = fixture();
    let answer = note(
        &fixture,
        TOKEN_A,
        json!({ "body": "The reconciliation window opens at 0300." }),
    );

    let scope = scope_of(WORKSPACE, "user-a");
    assert_eq!(
        answer["id"],
        format!("{scope}/{}", slug_of(&answer)),
        "{}",
        answer
    );
    assert_eq!(
        answer["url"],
        format!(
            "https://livingbrain.wiki/brain/{scope}/{}",
            slug_of(&answer)
        ),
        "{}",
        answer
    );
    assert_eq!(answer["version"], 1, "{}", answer);

    // The page is there, under the note's slug, with the note as its body.
    let page = get(
        &fixture,
        TOKEN_A,
        &format!("/v1/pages/{}", slug_of(&answer)),
    );
    assert_eq!(page.status, 200, "{}", common::body_text(&page));
    assert_eq!(
        page.json()["title"],
        "Note",
        "the default title: {}",
        common::body_text(&page)
    );
    assert_eq!(
        page.json()["markdown"],
        "---\ntitle: Note\n---\n\nThe reconciliation window opens at 0300.\n",
        "{}",
        common::body_text(&page)
    );
}

#[test]
fn the_same_text_lands_on_the_same_page_rather_than_a_new_one() {
    let fixture = fixture();
    let body = json!({ "body": "The same observation, sent twice." });
    let first = note(&fixture, TOKEN_A, body.clone());
    let second = note(&fixture, TOKEN_A, body);

    assert_eq!(
        first["id"], second["id"],
        "the slug is the hash of the redacted text"
    );
    assert_eq!(
        second["version"],
        first["version"].as_u64().expect("a version") + 1,
        "the same page, one version on: {second}"
    );

    // And the list holds one page, not two near-duplicates.
    let listed = get(&fixture, TOKEN_A, "/v1/pages");
    let body = listed.json();
    let pages = body["pages"].as_array().expect("pages list");
    assert_eq!(pages.len(), 1, "{}", common::body_text(&listed));
}

#[test]
fn a_note_is_redacted_before_it_is_stored_and_the_count_is_honest() {
    let fixture = fixture();
    let answer = note(
        &fixture,
        TOKEN_A,
        json!({ "body": "The deploy key AKIAIOSFODNN7EXAMPLE goes in the vault." }),
    );

    assert_eq!(answer["redacted"], 1, "one finding, counted: {}", answer);
    let page = get(
        &fixture,
        TOKEN_A,
        &format!("/v1/pages/{}", slug_of(&answer)),
    );
    let stored_body = page.json();
    let stored = stored_body["markdown"].as_str().expect("markdown");
    assert!(
        !stored.contains("AKIAIOSFODNN7EXAMPLE"),
        "the key must not survive: {stored}"
    );
    assert!(
        stored.contains("[REDACTED:"),
        "the span is marked: {stored}"
    );
    // The slug is the hash of the *redacted* text, which is why the answer's
    // id fits the slug rule.
    assert!(slug_of(&answer).starts_with("note-"), "{}", answer);
}

#[test]
fn a_project_lands_in_the_fence_search_filters_on() {
    let fixture = fixture();
    let answer = note(
        &fixture,
        TOKEN_A,
        json!({ "body": "The atlas migration is staged for Friday.", "project": "atlas" }),
    );

    let page = get(
        &fixture,
        TOKEN_A,
        &format!("/v1/pages/{}", slug_of(&answer)),
    );
    assert_eq!(
        page.json()["markdown"],
        "---\ntitle: Note\nproject: atlas\n---\n\nThe atlas migration is staged for Friday.\n",
        "the project rides in the fence: {}",
        common::body_text(&page)
    );

    // The round trip the CLI pins: `note --project atlas`, then
    // `search --project atlas` finds it — and another project does not.
    let found = get(&fixture, TOKEN_A, "/v1/search?q=migration&project=atlas");
    let found_body = found.json();
    let results = found_body["results"].as_array().expect("results");
    assert_eq!(results.len(), 1, "{}", common::body_text(&found));
    let elsewhere = get(&fixture, TOKEN_A, "/v1/search?q=migration&project=zurich");
    assert_eq!(
        elsewhere.json()["results"]
            .as_array()
            .expect("results")
            .len(),
        0,
        "{}",
        common::body_text(&elsewhere)
    );
}

#[test]
fn a_note_with_a_title_uses_it() {
    let fixture = fixture();
    let answer = note(
        &fixture,
        TOKEN_A,
        json!({ "body": "The kettle is descaled.", "title": "Kettle log" }),
    );
    let page = get(
        &fixture,
        TOKEN_A,
        &format!("/v1/pages/{}", slug_of(&answer)),
    );
    assert_eq!(
        page.json()["title"],
        "Kettle log",
        "{}",
        common::body_text(&page)
    );
}

#[test]
fn an_empty_body_is_a_400() {
    let fixture = fixture();
    for body in ["", "   \n\t"] {
        let response = post(&fixture, TOKEN_A, "/v1/notes", json!({ "body": body }));
        // The kit's standard validation problem.
        assert_eq!(
            status_of(&response),
            400,
            "{body:?}: {}",
            common::body_text(&response)
        );
        assert_eq!(
            response.json()["detail"],
            "`body` is required",
            "{body:?}: {}",
            common::body_text(&response)
        );
    }
}

#[test]
fn nobody_else_can_read_the_note() {
    let fixture = fixture();
    let answer = note(
        &fixture,
        TOKEN_A,
        json!({ "body": "A private observation about the kestrel." }),
    );
    let slug = slug_of(&answer).to_owned();

    let stranger = get(&fixture, TOKEN_B, &format!("/v1/pages/{slug}"));
    assert_eq!(stranger.status, 404, "{}", common::body_text(&stranger));

    let search = get(&fixture, TOKEN_B, "/v1/search?q=kestrel");
    assert_eq!(
        search.json()["results"].as_array().expect("results").len(),
        0,
        "{}",
        common::body_text(&search)
    );
}

#[test]
fn a_note_answers_401_without_and_with_a_rejected_credential() {
    let fixture = fixture();
    let anonymous = common::post_anon(&fixture, "/v1/notes", &json!({ "body": "x" }).to_string());
    assert_eq!(anonymous.status, 401, "{}", common::body_text(&anonymous));

    let rejected = post_bad_token(&fixture, "/v1/notes", json!({ "body": "x" }));
    assert_eq!(rejected.status, 401, "{}", common::body_text(&rejected));
    assert_eq!(
        anonymous.json()["title"],
        rejected.json()["title"],
        "one body for both cases"
    );
}
