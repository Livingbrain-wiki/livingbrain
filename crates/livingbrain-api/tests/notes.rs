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
        common::page_url(&scope, slug_of(&answer)),
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

#[test]
fn a_keyed_note_is_updated_in_place_and_an_identical_resend_writes_nothing() {
    let fixture = fixture();
    let first = note(
        &fixture,
        TOKEN_A,
        json!({
            "body": "Session one, two turns.",
            "title": "claude_code session abc",
            "project": "atlas",
            "slug": "agent-log-claude-code-abc",
        }),
    );
    assert_eq!(slug_of(&first), "agent-log-claude-code-abc", "{first}");
    assert_eq!(first["version"], 1, "{first}");
    assert_eq!(first["unchanged"], false, "{first}");

    // The same note again: no new version, and the answer says so.
    let again = note(
        &fixture,
        TOKEN_A,
        json!({
            "body": "Session one, two turns.",
            "title": "claude_code session abc",
            "project": "atlas",
            "slug": "agent-log-claude-code-abc",
        }),
    );
    assert_eq!(again["id"], first["id"], "{again}");
    assert_eq!(again["version"], 1, "{again}");
    assert_eq!(again["unchanged"], true, "{again}");

    // The session grew: the same page, one version on — not a second page.
    let grown = note(
        &fixture,
        TOKEN_A,
        json!({
            "body": "Session one, four turns now.",
            "title": "claude_code session abc",
            "project": "atlas",
            "slug": "agent-log-claude-code-abc",
        }),
    );
    assert_eq!(grown["id"], first["id"], "{grown}");
    assert_eq!(grown["version"], 2, "{grown}");
    assert_eq!(grown["unchanged"], false, "{grown}");

    let listed = get(&fixture, TOKEN_A, "/v1/pages");
    let body = listed.json();
    let pages = body["pages"].as_array().expect("pages list");
    assert_eq!(pages.len(), 1, "{}", common::body_text(&listed));
    // A session log is a session, not a decision, and the placeholder title
    // the client sent is replaced by one read from the note.
    assert_eq!(pages[0]["entity_type"], "session");
    assert_eq!(pages[0]["title"], "atlas · Claude Code");
    let page = get(&fixture, TOKEN_A, "/v1/pages/agent-log-claude-code-abc");
    assert_eq!(
        page.json()["markdown"],
        "---\ntitle: atlas · Claude Code\nproject: atlas\n---\n\nSession one, four turns now.\n",
        "{}",
        common::body_text(&page)
    );
}

#[test]
fn a_slug_outside_the_slug_rule_is_a_400() {
    let fixture = fixture();
    for slug in [
        "",
        "Upper-Case",
        "has space",
        "under_score",
        &"a".repeat(129),
    ] {
        let response = post(
            &fixture,
            TOKEN_A,
            "/v1/notes",
            json!({ "body": "A note.", "slug": slug }),
        );
        assert_eq!(
            status_of(&response),
            400,
            "slug {slug:?}: {}",
            common::body_text(&response)
        );
    }
}

/// The body `livingbrain logs sync` writes for a session (its header, then
/// the conversation), for `agent`.
fn session_body(agent: &str, extra: &str) -> String {
    format!(
        "Coding agent session: {agent} in EarthOS.world.\n\n- Agent: {agent}\n- Session: a15fba01-77c2\n\
         - Repo: World-360/EarthOS.world\n{extra}- When: 2026-10-15T08:01:02Z to 2026-10-15T09:00:00Z\n\
         - Messages: 1 user, 1 assistant\n\n## Conversation\n\n**User** · 08:01\n\n\
         Fix the login redirect loop\n\n**Assistant** · 08:02\n\nOn it.\n"
    )
}

#[test]
fn every_agents_session_is_named_from_its_note() {
    let fixture = fixture();
    for (agent, slug, extra, want) in [
        (
            "claude_code",
            "agent-log-claude-code-a15fba01-77c2",
            "",
            "EarthOS.world · Claude Code · 2026-10-15 · Fix the login redirect loop",
        ),
        (
            "codex",
            "agent-log-codex-a15fba01-77c2",
            "",
            "EarthOS.world · Codex · 2026-10-15 · Fix the login redirect loop",
        ),
        (
            "colonizer",
            "agent-log-colonizer-a15fba01-77c2",
            "- Issue: #123 Fix auth redirect\n- Outcome: merged\n",
            "EarthOS.world · Colony a15fba01 · #123 Fix auth redirect",
        ),
    ] {
        note(
            &fixture,
            TOKEN_A,
            json!({
                "body": session_body(agent, extra),
                "title": format!("{agent} session a15fba01-77c2"),
                "project": "EarthOS.world",
                "slug": slug,
            }),
        );
        let page = get(&fixture, TOKEN_A, &format!("/v1/pages/{slug}")).json();
        assert_eq!(page["title"], want, "{agent}");
        assert_eq!(page["entity_type"], "session", "{agent}");
    }
    // A client's own title is kept.
    note(
        &fixture,
        TOKEN_A,
        json!({
            "body": session_body("codex", ""),
            "title": "My own name for it",
            "project": "EarthOS.world",
            "slug": "agent-log-codex-mine",
        }),
    );
    let page = get(&fixture, TOKEN_A, "/v1/pages/agent-log-codex-mine").json();
    assert_eq!(page["title"], "My own name for it");
}

#[test]
fn the_pages_list_can_leave_sessions_out_and_carries_titles() {
    let fixture = fixture();
    note(
        &fixture,
        TOKEN_A,
        json!({
            "body": session_body("colonizer", "- Issue: #9 Ship it\n"),
            "title": "colonizer session a15fba01-77c2",
            "project": "EarthOS.world",
            "slug": "agent-log-colonizer-a15fba01-77c2",
        }),
    );
    note(
        &fixture,
        TOKEN_A,
        json!({ "body": "A plain note.", "title": "Plain" }),
    );
    let all = get(&fixture, TOKEN_A, "/v1/pages").json();
    assert_eq!(all["pages"].as_array().unwrap().len(), 2);
    let wiki = get(&fixture, TOKEN_A, "/v1/pages?exclude_type=session").json();
    let wiki = wiki["pages"].as_array().unwrap();
    assert_eq!(wiki.len(), 1, "{wiki:?}");
    assert_eq!(wiki[0]["title"], "Plain");
    let sessions = get(&fixture, TOKEN_A, "/v1/pages?type=session").json();
    assert_eq!(
        sessions["pages"][0]["title"],
        "EarthOS.world · Colony a15fba01 · #9 Ship it"
    );
    let bad = get(&fixture, TOKEN_A, "/v1/pages?type=nope");
    assert_eq!(bad.status, 400, "{}", common::body_text(&bad));
}

/// The production shape before this change: session logs written as
/// decisions, titled with the placeholder, and no title on the row.
fn legacy_session(fixture: &common::Fixture, slug: &str, body: &str) {
    use livingbrain_pages::{Author, EntityType, PageWrite};
    let scope = scope_of(WORKSPACE, "user-a");
    let markdown = format!(
        "---\ntitle: colonizer session a15fba01-77c2\nproject: EarthOS.world\n---\n\n{body}\n"
    );
    pollster::block_on(fixture.store.write(
        &scope,
        slug,
        PageWrite {
            entity_type: EntityType::Decision,
            markdown,
            author: Author::Brain { reconciles: None },
            base_version: None,
        },
    ))
    .expect("a write");
    pollster::block_on(fixture.db().execute(&cratefield_core::Statement::with_values(
        format!("UPDATE pages SET title_sealed = NULL, title_key_version = NULL WHERE slug = '{slug}'"),
        vec![],
    )))
    .expect("the row predates titles");
}

#[test]
fn a_legacy_decision_filed_session_moves_over_on_its_next_write() {
    let fixture = fixture();
    let slug = "agent-log-colonizer-a15fba01-77c2";
    legacy_session(
        &fixture,
        slug,
        &session_body("colonizer", "- Issue: #1 Old\n"),
    );
    let answer = note(
        &fixture,
        TOKEN_A,
        json!({
            "body": session_body("colonizer", "- Issue: #1 Old\n- Outcome: merged\n"),
            "title": "colonizer session a15fba01-77c2",
            "project": "EarthOS.world",
            "slug": slug,
        }),
    );
    assert_eq!(answer["version"], 2, "{answer}");
    let page = get(&fixture, TOKEN_A, &format!("/v1/pages/{slug}")).json();
    assert_eq!(page["entity_type"], "session");
    assert_eq!(page["title"], "EarthOS.world · Colony a15fba01 · #1 Old");
}

#[test]
fn the_backfill_retypes_and_retitles_legacy_sessions_once() {
    let fixture = fixture();
    let slug = "agent-log-colonizer-a15fba01-77c2";
    legacy_session(
        &fixture,
        slug,
        &session_body("colonizer", "- Issue: #123 Fix auth redirect\n"),
    );
    // An ordinary page from before titles were kept, too.
    common::seed(
        &fixture,
        WORKSPACE,
        &common::user("user-a"),
        "ada",
        "---\ntitle: Ada's notes\n---\n\nx\n",
    );
    pollster::block_on(
        fixture
            .db()
            .execute(&cratefield_core::Statement::with_values(
                "UPDATE pages SET title_sealed = NULL, title_key_version = NULL WHERE slug = 'ada'",
                vec![],
            )),
    )
    .unwrap();

    let budget = cratefield_core::ScheduledBudget::default();
    let clock = fixture.clock();
    let first = pollster::block_on(livingbrain_api::backfill(
        &fixture.store,
        &budget,
        clock.as_ref(),
        25,
    ))
    .expect("a pass");
    assert_eq!(first.retyped, 1, "{first:?}");
    assert_eq!(first.retitled, 1, "{first:?}");
    assert_eq!(first.titled, 1, "{first:?}");

    let listed = get(&fixture, TOKEN_A, "/v1/pages").json();
    let pages = listed["pages"].as_array().unwrap();
    let find = |slug: &str| pages.iter().find(|p| p["slug"] == slug).cloned().unwrap();
    assert_eq!(find(slug)["entity_type"], "session");
    assert_eq!(
        find(slug)["title"],
        "EarthOS.world · Colony a15fba01 · #123 Fix auth redirect"
    );
    assert_eq!(find("ada")["title"], "Ada's notes");

    // Idempotent: a second pass finds nothing, and writes nothing.
    let second = pollster::block_on(livingbrain_api::backfill(
        &fixture.store,
        &budget,
        clock.as_ref(),
        25,
    ))
    .expect("a pass");
    assert_eq!(second, livingbrain_api::Backfilled::default());
    let page = get(&fixture, TOKEN_A, &format!("/v1/pages/{slug}")).json();
    assert_eq!(page["version"], 2);
}

#[test]
fn a_project_search_matches_every_spelling_of_the_project() {
    let fixture = fixture();
    note(
        &fixture,
        TOKEN_A,
        json!({ "body": "The orbital migration is staged.", "project": "EarthOS.world" }),
    );
    for spelling in ["EarthOS.world", "earthos-world", "EARTHOS.WORLD"] {
        let found = get(
            &fixture,
            TOKEN_A,
            &format!("/v1/search?q=migration&project={spelling}"),
        );
        assert_eq!(
            found.json()["results"].as_array().expect("results").len(),
            1,
            "{spelling}: {}",
            common::body_text(&found)
        );
    }
}
