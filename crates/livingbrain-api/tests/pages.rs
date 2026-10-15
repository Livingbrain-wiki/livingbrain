//! The page routes, end to end through the harness: the shape the web app
//! pins, the optimistic-concurrency contract, redaction before storing, and
//! scope isolation.
//!
//! The shape test is the load-bearing one: every field
//! `app/assets/app.js` reads — `slug`, `title`, `markdown`, `url`,
//! `version`, `entity_type`, `backlinks`, `citations` — must be present
//! under that name, because the app renders from the body and cannot be
//! told that a field moved.

mod common;

use common::{
    TOKEN_A, TOKEN_B, WORKSPACE, fixture, get, get_anon, is_problem, post_bad_token, put, scope_of,
    seed, status_of, user,
};
use livingbrain_access::Scope;
use serde_json::json;

const KESTREL: &str =
    "---\ntitle: Kestrel routes\n---\nThe kestrel routes run at dawn. See [[tamar]].";
const TAMAR: &str = "---\ntitle: Tamar routes\n---\nThe tamar routes run at dusk. See [[kestrel]].";

#[test]
fn a_page_reads_back_in_the_shape_the_web_app_pins() {
    let fixture = fixture();
    seed(&fixture, WORKSPACE, &user("user-a"), "kestrel", KESTREL);
    let tamar = seed(&fixture, WORKSPACE, &user("user-a"), "tamar", TAMAR);
    let scope = scope_of(WORKSPACE, "user-a");

    let response = get(&fixture, TOKEN_A, "/v1/pages/tamar");
    assert_eq!(
        status_of(&response),
        200,
        "{}",
        common::body_text(&response)
    );
    assert!(
        !is_problem(&response),
        "a good read is not a problem: {}",
        common::body_text(&response)
    );
    let page = response.json();
    // Every field the app reads, under the exact name it reads.
    assert_eq!(page["slug"], "tamar", "{page}");
    assert_eq!(page["title"], "Tamar routes", "{page}");
    assert_eq!(page["markdown"], TAMAR, "{page}");
    assert_eq!(
        page["url"],
        format!("https://livingbrain.wiki/brain/{scope}/tamar"),
        "{page}"
    );
    assert_eq!(page["version"], tamar.version, "{page}");
    assert_eq!(page["entity_type"], "decision", "{page}");
    // The kestrel page links here; nothing else does.
    let backlinks = page["backlinks"].as_array().expect("backlinks is a list");
    assert_eq!(backlinks, &["kestrel".to_owned()], "{page}");
    // And tamar links out to kestrel, as a citation with the site's URL.
    let citations = page["citations"].as_array().expect("citations is a list");
    assert_eq!(citations.len(), 1, "{page}");
    assert_eq!(citations[0]["title"], "kestrel", "{page}");
    assert_eq!(
        citations[0]["url"],
        format!("https://livingbrain.wiki/brain/{scope}/kestrel"),
        "{page}"
    );
}

#[test]
fn a_put_answer_is_the_read_of_the_page_the_store_kept() {
    let fixture = fixture();
    let written = put(
        &fixture,
        TOKEN_A,
        "/v1/pages/dawn-run",
        json!({
            "markdown": "The route runs at dawn.",
            "base_version": null,
            "title": "Dawn run",
        }),
    );
    assert_eq!(status_of(&written), 200, "{}", common::body_text(&written));
    let read = get(&fixture, TOKEN_A, "/v1/pages/dawn-run");
    assert_eq!(written.json(), read.json(), "a save renders what is stored");
}

#[test]
fn an_unknown_slug_and_an_unreadable_one_are_the_same_404() {
    let fixture = fixture();
    seed(&fixture, WORKSPACE, &user("user-a"), "kestrel", KESTREL);

    let response = get(&fixture, TOKEN_A, "/v1/pages/never-written");
    assert_eq!(
        status_of(&response),
        404,
        "{}",
        common::body_text(&response)
    );
    assert_eq!(
        response.json()["title"],
        "No such page",
        "{}",
        common::body_text(&response)
    );

    // user-b asking for user-a's page learns only that it is not *theirs*.
    let owned = get(&fixture, TOKEN_B, "/v1/pages/kestrel");
    assert_eq!(status_of(&owned), 404, "{}", common::body_text(&owned));
    assert_eq!(
        owned.json()["title"],
        response.json()["title"],
        "absent and somebody else's say the same thing"
    );
}

#[test]
fn a_slug_outside_the_slug_rule_is_a_missing_page_on_a_read() {
    let fixture = fixture();
    let response = get(&fixture, TOKEN_A, "/v1/pages/Not_A_Slug");
    assert_eq!(
        status_of(&response),
        404,
        "{}",
        common::body_text(&response)
    );
    assert_eq!(
        response.json()["title"],
        "No such page",
        "{}",
        common::body_text(&response)
    );
}

#[test]
fn the_list_carries_the_readable_pages_with_their_citation_urls() {
    let fixture = fixture();
    seed(&fixture, WORKSPACE, &user("user-a"), "kestrel", KESTREL);
    seed(&fixture, WORKSPACE, &user("user-a"), "tamar", TAMAR);
    seed(
        &fixture,
        WORKSPACE,
        &user("user-b"),
        "burdock",
        "---\ntitle: Burdock\n---\nB's page.",
    );

    let response = get(&fixture, TOKEN_A, "/v1/pages");
    assert_eq!(
        status_of(&response),
        200,
        "{}",
        common::body_text(&response)
    );
    let listed = response.json();
    let pages = listed["pages"].as_array().expect("pages list");
    let mut slugs: Vec<&str> = pages
        .iter()
        .map(|page| page["slug"].as_str().expect("a slug"))
        .collect();
    slugs.sort_unstable();
    // The harness clock is frozen, so "newest first" falls back to the
    // store's secondary order; the set is what this test pins.
    assert_eq!(slugs, ["kestrel", "tamar"], "{pages:?}");
    let scope = scope_of(WORKSPACE, "user-a");
    for page in pages {
        assert!(page["scope"].is_string(), "{page}");
        assert_eq!(page["entity_type"], "decision", "{page}");
        assert!(page["version"].is_u64(), "{page}");
        assert!(page["updated_at"].is_string(), "{page}");
        assert_eq!(
            page["url"],
            format!(
                "https://livingbrain.wiki/brain/{scope}/{}",
                page["slug"].as_str().expect("slug")
            ),
            "{page}"
        );
    }
}

#[test]
fn the_list_limit_bounds_the_answer_and_refuses_nonsense() {
    let fixture = fixture();
    for slug in ["kestrel", "tamar", "burdock"] {
        seed(&fixture, WORKSPACE, &user("user-a"), slug, KESTREL);
    }

    let response = get(&fixture, TOKEN_A, "/v1/pages?limit=2");
    let listed = response.json();
    let pages = listed["pages"].as_array().expect("pages list");
    assert_eq!(pages.len(), 2, "{}", common::body_text(&response));

    let refused = get(&fixture, TOKEN_A, "/v1/pages?limit=yesterday");
    // The kit's standard validation problem: a malformed limit is a bad
    // request, not a semantic refusal.
    assert_eq!(status_of(&refused), 400, "{}", common::body_text(&refused));
    assert_eq!(
        refused.json()["detail"],
        "`limit` must be a positive integer, got \"yesterday\"",
        "{}",
        common::body_text(&refused)
    );
}

#[test]
fn a_put_with_a_null_base_version_creates_a_page_in_the_askers_own_scope() {
    let fixture = fixture();
    let scope = scope_of(WORKSPACE, "user-a");
    let response = put(
        &fixture,
        TOKEN_A,
        "/v1/pages/dawn-run",
        json!({
            "markdown": "The route runs at dawn.",
            "base_version": null,
            "title": "Dawn run",
        }),
    );
    assert_eq!(
        status_of(&response),
        200,
        "{}",
        common::body_text(&response)
    );
    let page = response.json();
    assert_eq!(page["version"], 1, "a create is the first version: {page}");
    assert_eq!(page["title"], "Dawn run", "{page}");
    assert_eq!(page["entity_type"], "decision", "{page}");
    assert_eq!(
        page["url"],
        format!("https://livingbrain.wiki/brain/{scope}/dawn-run"),
        "{page}"
    );
    // The title the client chose went into the fence the entity needs.
    assert_eq!(
        page["markdown"], "---\ntitle: Dawn run\n---\n\nThe route runs at dawn.",
        "{page}"
    );

    // And nobody else can read it: it landed in the asker's own scope.
    let stranger = get(&fixture, TOKEN_B, "/v1/pages/dawn-run");
    assert_eq!(
        status_of(&stranger),
        404,
        "{}",
        common::body_text(&stranger)
    );
}

#[test]
fn a_put_with_the_base_version_edits_and_bumps_the_version() {
    let fixture = fixture();
    let page = seed(&fixture, WORKSPACE, &user("user-a"), "kestrel", KESTREL);

    let response = put(
        &fixture,
        TOKEN_A,
        "/v1/pages/kestrel",
        json!({
            "markdown": "---\ntitle: Kestrel routes\n---\nThe kestrel routes run at noon.",
            "base_version": page.version,
        }),
    );
    assert_eq!(
        status_of(&response),
        200,
        "{}",
        common::body_text(&response)
    );
    assert_eq!(
        response.json()["version"],
        page.version + 1,
        "{}",
        common::body_text(&response)
    );
    assert_eq!(
        response.json()["markdown"],
        "---\ntitle: Kestrel routes\n---\nThe kestrel routes run at noon.",
        "{}",
        common::body_text(&response)
    );
}

#[test]
fn a_stale_base_version_is_a_conflict_that_says_to_reload() {
    let fixture = fixture();
    let page = seed(&fixture, WORKSPACE, &user("user-a"), "kestrel", KESTREL);

    // base_version 0 when the head is at 1: a bet against a change that
    // already happened.
    let stale = put(
        &fixture,
        TOKEN_A,
        "/v1/pages/kestrel",
        json!({
            "markdown": "---\ntitle: Kestrel routes\n---\nStale text.",
            "base_version": page.version - 1,
        }),
    );
    assert_eq!(status_of(&stale), 409, "{}", common::body_text(&stale));
    assert_eq!(
        stale.json()["title"],
        "The page moved",
        "{}",
        common::body_text(&stale)
    );
    assert!(
        stale.json()["detail"]
            .as_str()
            .expect("a detail")
            .contains("Reload"),
        "the advice is to reload: {}",
        common::body_text(&stale)
    );
    // Nothing was written.
    let after = get(&fixture, TOKEN_A, "/v1/pages/kestrel");
    assert_eq!(
        after.json()["markdown"],
        KESTREL,
        "{}",
        common::body_text(&after)
    );
}

#[test]
fn an_existing_page_with_a_null_base_version_is_a_conflict() {
    let fixture = fixture();
    seed(&fixture, WORKSPACE, &user("user-a"), "kestrel", KESTREL);

    let response = put(
        &fixture,
        TOKEN_A,
        "/v1/pages/kestrel",
        json!({
            "markdown": "---\ntitle: Kestrel routes\n---\nBlind overwrite.",
            "base_version": null,
        }),
    );
    assert_eq!(
        status_of(&response),
        409,
        "{}",
        common::body_text(&response)
    );
    assert_eq!(
        response.json()["title"],
        "The page moved",
        "{}",
        common::body_text(&response)
    );
    assert!(
        response.json()["detail"]
            .as_str()
            .expect("a detail")
            .contains("already exists"),
        "the detail says the page is there to reload: {}",
        common::body_text(&response)
    );
}

#[test]
fn a_new_page_cannot_carry_a_base_version() {
    let fixture = fixture();
    let response = put(
        &fixture,
        TOKEN_A,
        "/v1/pages/never-written",
        json!({
            "markdown": "Some text.",
            "base_version": 3,
        }),
    );
    assert_eq!(
        status_of(&response),
        422,
        "{}",
        common::body_text(&response)
    );
    assert_eq!(
        response.json()["detail"],
        "a new page cannot carry a base_version; leave it null to create one",
        "{}",
        common::body_text(&response)
    );
}

#[test]
fn a_slug_outside_the_slug_rule_is_a_refused_body_on_a_write() {
    let fixture = fixture();
    let response = put(
        &fixture,
        TOKEN_A,
        "/v1/pages/Not_A_Slug",
        json!({ "markdown": "Some text.", "base_version": null }),
    );
    assert_eq!(
        status_of(&response),
        422,
        "{}",
        common::body_text(&response)
    );
    assert_eq!(
        response.json()["title"],
        "The page was refused",
        "{}",
        common::body_text(&response)
    );
}

#[test]
fn a_pasted_secret_is_redacted_before_anything_is_stored() {
    let fixture = fixture();
    let response = put(
        &fixture,
        TOKEN_A,
        "/v1/pages/leaked-key",
        json!({
            "markdown": "The deploy key is AKIAIOSFODNN7EXAMPLE and it must never land.",
            "base_version": null,
            "title": "Leaked key",
        }),
    );
    assert_eq!(
        status_of(&response),
        200,
        "{}",
        common::body_text(&response)
    );
    let page = response.json();
    let stored = page["markdown"].as_str().expect("markdown");
    assert!(
        !stored.contains("AKIAIOSFODNN7EXAMPLE"),
        "the key must not survive: {stored}"
    );
    assert!(
        stored.contains("[REDACTED:"),
        "the span says it was redacted: {stored}"
    );
}

#[test]
fn a_client_title_merges_into_a_body_without_a_fence() {
    let fixture = fixture();
    // A decision page must carry a titled fence to exist at all, so the
    // page starts fenced and the *edit* arrives fence-less — the shape a
    // client that strips frontmatter on the way out produces.
    let page = seed(&fixture, WORKSPACE, &user("user-a"), "kestrel", KESTREL);

    let response = put(
        &fixture,
        TOKEN_A,
        "/v1/pages/kestrel",
        json!({
            "markdown": "A body with no fence at all, edited.",
            "base_version": page.version,
            "title": "Kestrel routes",
        }),
    );
    assert_eq!(
        status_of(&response),
        200,
        "{}",
        common::body_text(&response)
    );
    assert_eq!(
        response.json()["markdown"],
        "---\ntitle: Kestrel routes\n---\n\nA body with no fence at all, edited.",
        "{}",
        common::body_text(&response)
    );
}

#[test]
fn a_fence_that_already_carries_a_title_wins_over_the_request_title() {
    let fixture = fixture();
    let page = seed(&fixture, WORKSPACE, &user("user-a"), "kestrel", KESTREL);

    let response = put(
        &fixture,
        TOKEN_A,
        "/v1/pages/kestrel",
        json!({
            // The markdown is the newer text, and its fence names the page.
            "markdown": KESTREL,
            "base_version": page.version,
            "title": "A stale title from the editor",
        }),
    );
    assert_eq!(
        status_of(&response),
        200,
        "{}",
        common::body_text(&response)
    );
    assert_eq!(
        response.json()["title"],
        "Kestrel routes",
        "{}",
        common::body_text(&response)
    );
    assert_eq!(
        response.json()["markdown"],
        KESTREL,
        "the fence survived untouched"
    );
}

#[test]
fn frontmatter_that_fits_no_entity_type_is_refused_before_storing() {
    let fixture = fixture();
    let response = put(
        &fixture,
        TOKEN_A,
        "/v1/pages/odd-page",
        json!({
            "markdown": "---\nvestibule: yes\n---\nA body.",
            "base_version": null,
        }),
    );
    assert_eq!(
        status_of(&response),
        422,
        "{}",
        common::body_text(&response)
    );
    assert_eq!(
        response.json()["title"],
        "The page was refused",
        "{}",
        common::body_text(&response)
    );
}

#[test]
fn a_shared_page_is_readable_by_both_users() {
    let fixture = fixture();
    let shared = seed(
        &fixture,
        WORKSPACE,
        &Scope::Shared,
        "team-handbook",
        "---\ntitle: Team handbook\n---\nEveryone reads this.",
    );

    for token in [TOKEN_A, TOKEN_B] {
        let response = get(&fixture, token, "/v1/pages/team-handbook");
        assert_eq!(
            status_of(&response),
            200,
            "{token}: {}",
            common::body_text(&response)
        );
        assert_eq!(response.json()["version"], shared.version, "{token}");
    }
}

#[test]
fn a_read_answers_401_the_same_without_and_with_a_rejected_credential() {
    let fixture = fixture();

    let anonymous = get_anon(&fixture, "/v1/pages");
    assert_eq!(
        status_of(&anonymous),
        401,
        "{}",
        common::body_text(&anonymous)
    );
    assert!(is_problem(&anonymous), "{}", common::body_text(&anonymous));

    let rejected = post_bad_token(&fixture, "/v1/notes", json!({ "body": "x" }));
    assert_eq!(
        status_of(&rejected),
        401,
        "{}",
        common::body_text(&rejected)
    );

    // One problem body for both: a stranger learns nothing about tokens.
    assert_eq!(
        anonymous.json()["title"],
        rejected.json()["title"],
        "the body says the same thing either way"
    );
    assert_eq!(
        anonymous.json()["status"],
        rejected.json()["status"],
        "{}",
        common::body_text(&rejected)
    );
}
