//! The ask route: a retrieval brief, not a conversation. The answer is the
//! pages that mention the question — full text, numbered sections, each
//! section also a structured citation — and nothing calls a model anywhere.

mod common;

use common::{TOKEN_A, TOKEN_B, WORKSPACE, fixture, post, seed, status_of, user};
use livingbrain_access::Scope;
use serde_json::{Value, json};

const KESTREL: &str =
    "---\ntitle: Kestrel routes\n---\nThe kestrel routes run at dawn near the harbour.";
const TAMAR: &str =
    "---\ntitle: Tamar routes\n---\nThe tamar routes run at dusk, unlike the kestrel routes.";

fn ask(fixture: &common::Fixture, token: &str, body: Value) -> (u16, Value) {
    let response = post(fixture, token, "/v1/ask", body);
    (u16::from(response.status), response.json())
}

#[test]
fn an_ask_answers_with_the_pages_that_mention_the_question() {
    let fixture = fixture();
    seed(&fixture, WORKSPACE, &user("user-a"), "kestrel", KESTREL);
    seed(&fixture, WORKSPACE, &user("user-a"), "tamar", TAMAR);
    let scope = common::scope_of(WORKSPACE, "user-a");

    let (status, answer) = ask(
        &fixture,
        TOKEN_A,
        // Recall is every-word: the question's words are ones both pages
        // hold, so a brief that missed one would be a real miss.
        json!({ "question": "kestrel routes" }),
    );
    assert_eq!(status, 200, "{answer}");
    let brief = answer["answer"].as_str().expect("an answer");

    // Both pages answer, each as a numbered section naming the page.
    assert!(brief.contains("[1]"), "sections are numbered: {brief}");
    assert!(
        brief.contains(&format!("Tamar routes — {scope}/tamar")),
        "the heading names the page: {brief}"
    );
    assert!(
        brief.contains("The tamar routes run at dusk"),
        "the body rides in full: {brief}"
    );

    // Each section is also a citation, with a quote a reader can match.
    let citations = answer["citations"].as_array().expect("citations");
    assert!(!citations.is_empty(), "{answer}");
    for citation in citations {
        assert!(citation["title"].is_string(), "{citation}");
        assert!(citation["url"].is_string(), "{citation}");
        let quote = citation["quote"].as_str().expect("a quote");
        assert!(
            brief.contains(quote.trim()),
            "the quote comes from the brief: {quote} vs {brief}"
        );
    }
}

#[test]
fn an_empty_wiki_is_answered_honestly_with_no_citations() {
    let fixture = fixture();
    let (status, answer) = ask(
        &fixture,
        TOKEN_A,
        json!({ "question": "what do the pages say about kestrels?" }),
    );
    assert_eq!(status, 200, "{answer}");
    assert_eq!(
        answer["answer"], "Nothing in the pages you can read mentions this.",
        "honest, not inventive: {answer}"
    );
    assert_eq!(
        answer["citations"].as_array().expect("citations").len(),
        0,
        "{answer}"
    );
}

#[test]
fn an_ask_reads_only_the_scopes_the_caller_was_granted() {
    let fixture = fixture();
    seed(&fixture, WORKSPACE, &user("user-a"), "kestrel", KESTREL);

    let (status, answer) = ask(
        &fixture,
        TOKEN_B,
        json!({ "question": "what about the kestrel?" }),
    );
    assert_eq!(status, 200, "{answer}");
    assert_eq!(
        answer["answer"], "Nothing in the pages you can read mentions this.",
        "user-a's page is not in user-b's brief: {answer}"
    );
    assert_eq!(answer["citations"].as_array().expect("citations").len(), 0);
}

#[test]
fn a_project_narrows_the_brief() {
    let fixture = fixture();
    post(
        &fixture,
        TOKEN_A,
        "/v1/notes",
        json!({ "body": "The atlas migration is staged for Friday.", "project": "atlas" }),
    );
    seed(
        &fixture,
        WORKSPACE,
        &user("user-a"),
        "migration-notes",
        "---\ntitle: Migration notes\n---\nEvery migration needs a migration runbook.",
    );

    let (_, narrowed) = ask(
        &fixture,
        TOKEN_A,
        json!({ "question": "migration?", "project": "atlas" }),
    );
    let citations = narrowed["citations"].as_array().expect("citations");
    assert_eq!(citations.len(), 1, "{narrowed}");
    assert_eq!(citations[0]["title"], "Note", "{narrowed}");

    let (_, unfiled) = ask(
        &fixture,
        TOKEN_A,
        json!({ "question": "migration?", "project": "nowhere" }),
    );
    assert_eq!(
        unfiled["citations"].as_array().expect("citations").len(),
        0,
        "{unfiled}"
    );
}

#[test]
fn an_empty_question_is_a_400() {
    let fixture = fixture();
    for question in ["", "   \n"] {
        let (status, answer) = ask(&fixture, TOKEN_A, json!({ "question": question }));
        // The kit's standard validation problem.
        assert_eq!(status, 400, "{question:?}: {answer}");
        assert_eq!(
            answer["detail"], "`question` is required",
            "{question:?}: {answer}"
        );
    }
}

#[test]
fn an_ask_answers_401_without_a_credential() {
    let fixture = fixture();
    let response = common::post_anon(
        &fixture,
        "/v1/ask",
        &json!({ "question": "anything" }).to_string(),
    );
    assert_eq!(
        status_of(&response),
        401,
        "{}",
        common::body_text(&response)
    );
}

#[test]
fn an_ask_of_the_shared_scope_finds_the_shared_page() {
    let fixture = fixture();
    seed(
        &fixture,
        WORKSPACE,
        &common::user("user-b"),
        "logbook",
        "---\ntitle: Logbook\n---\nThe kestrel was seen over the logbook meadow.",
    );

    // user-a cannot see user-b's page...
    let (_, denied) = ask(&fixture, TOKEN_A, json!({ "question": "kestrel" }));
    assert_eq!(
        denied["citations"].as_array().expect("citations").len(),
        0,
        "{denied}"
    );

    // ...but the shared one, both read.
    seed(
        &fixture,
        WORKSPACE,
        &Scope::Shared,
        "noticeboard",
        "---\ntitle: Noticeboard\n---\nA kestrel headline for everyone.",
    );
    for token in [TOKEN_A, TOKEN_B] {
        let (_, answer) = ask(&fixture, token, json!({ "question": "kestrel" }));
        let titles: Vec<&str> = answer["citations"]
            .as_array()
            .expect("citations")
            .iter()
            .map(|citation| citation["title"].as_str().expect("a title"))
            .collect();
        assert!(
            titles.contains(&"Noticeboard"),
            "the shared page is in both briefs: {token}: {answer}"
        );
    }
}
