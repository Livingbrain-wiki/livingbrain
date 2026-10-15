//! Temporal facts over HTTP (docs/temporal-facts.md): `POST /v1/facts`,
//! `GET /v1/facts`, retraction, facts riding on a note, and the read path —
//! `ask`'s brief (the prompt a model gets), `search` and the page view.
//! The fixture clock stands at 2027-01-15.

mod common;

use common::{
    TOKEN_A, TOKEN_A_ELSEWHERE, TOKEN_B, WORKSPACE, fixture, get, get_anon, post, post_anon, seed,
    user,
};
use serde_json::{Value, json};

fn shoe(value: &str, from: &str, observed: &str) -> Value {
    json!({
        "subject": "me",
        "attribute": "shoe_size",
        "value": value,
        "unit": "EU",
        "valid_from": from,
        "observed_at": observed,
    })
}

/// The shoe-size example from the spec, through the routes: a note in 2025,
/// a note in 2026, then ask now and as of 2025.
fn shoe_size_history(fixture: &common::Fixture) {
    let first = post(
        fixture,
        TOKEN_A,
        "/v1/notes",
        json!({
            "body": "my shoe size is 40",
            "facts": [shoe("40", "2025", "2025-03-02T10:00:00Z")],
        }),
    );
    assert_eq!(u16::from(first.status), 200, "{}", first.json());
    let recorded = &first.json()["facts"][0];
    assert_eq!(recorded["outcome"], "created", "{recorded}");
    assert!(
        recorded["fact"]["source"]
            .as_str()
            .unwrap()
            .starts_with("note:user-"),
        "a note's facts cite the note: {recorded}"
    );
    let second = post(
        fixture,
        TOKEN_A,
        "/v1/notes",
        json!({
            "body": "my shoe size is now 41",
            "facts": [shoe("41", "2026", "2026-10-15T09:12:00Z")],
        }),
    );
    assert_eq!(u16::from(second.status), 200, "{}", second.json());
}

#[test]
fn ask_renders_current_and_history_for_the_model() {
    let fixture = fixture();
    shoe_size_history(&fixture);

    let now = post(
        &fixture,
        TOKEN_A,
        "/v1/ask",
        json!({ "question": "what's my shoe size?" }),
    );
    assert_eq!(u16::from(now.status), 200);
    let answer = now.json();
    let brief = answer["answer"].as_str().unwrap();
    // The convention a model is told, then the one line.
    assert!(brief.contains("Facts below are temporal."), "{brief}");
    assert!(brief.contains("Prefer the value valid now"), "{brief}");
    assert!(
        brief.contains(
            "shoe_size(person/user-a) = 41 EU [valid 2026..now, observed 2026-10-15, \
             source F1] (was 40 EU 2025..2026)"
        ),
        "{brief}"
    );
    assert!(!brief.contains("Nothing in the pages"), "{brief}");
    let citation = answer["citations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["title"] == "F1")
        .expect("F1 is cited");
    assert!(citation["source"].as_str().unwrap().starts_with("note:"));

    // As of 2025: the field, and the question's own words.
    for body in [
        json!({ "question": "what's my shoe size?", "as_of": "2025" }),
        json!({ "question": "what was my shoe size in 2025?" }),
    ] {
        let then = post(&fixture, TOKEN_A, "/v1/ask", body.clone()).json();
        let brief = then["answer"].as_str().unwrap();
        assert!(
            brief.contains(
                "shoe_size(person/user-a) = 40 EU [valid 2025..2026, observed 2025-03-02, \
                 source F1] (later 41 EU 2026..now)"
            ),
            "{body}: {brief}"
        );
        assert_eq!(then["as_of"], "2025-12-31T23:59:59Z");
    }
}

#[test]
fn another_person_or_workspace_never_sees_the_facts() {
    let fixture = fixture();
    shoe_size_history(&fixture);
    for token in [TOKEN_B, TOKEN_A_ELSEWHERE] {
        let read = get(&fixture, token, "/v1/facts").json();
        assert_eq!(read["keys"], json!([]), "{token}");
        let asked = post(
            &fixture,
            token,
            "/v1/ask",
            json!({ "question": "what is user-a shoe size" }),
        )
        .json();
        assert!(
            !asked["answer"].as_str().unwrap().contains("shoe_size"),
            "{token}: {asked}"
        );
        let searched = get(&fixture, token, "/v1/search?q=shoe%20size").json();
        assert_eq!(searched["facts"], json!([]), "{token}");
    }
    // B's `me` is B, not A.
    let mine = get(&fixture, TOKEN_B, "/v1/facts?subject=me").json();
    assert_eq!(mine["keys"], json!([]));
}

#[test]
fn the_facts_routes_read_write_and_retract() {
    let fixture = fixture();
    let written = post(
        &fixture,
        TOKEN_A,
        "/v1/facts",
        json!({ "format": 1, "facts": [
            shoe("40", "2025", "2025-03-02T10:00:00Z"),
            shoe("41", "2026", "2026-10-15T09:12:00Z"),
            { "subject": "project/checkout", "attribute": "uses_tool", "value": "stripe" },
            { "subject": "project/checkout", "attribute": "uses_tool", "value": "redis" },
        ]}),
    );
    assert_eq!(u16::from(written.status), 201, "{}", written.json());
    let ids: Vec<String> = written.json()["facts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["id"].as_str().unwrap().to_owned())
        .collect();

    let all = get(&fixture, TOKEN_A, "/v1/facts").json();
    let keys = all["keys"].as_array().unwrap();
    assert_eq!(keys.len(), 2, "{all}");
    let tools = keys.iter().find(|k| k["attribute"] == "uses_tool").unwrap();
    assert_eq!(tools["cardinality"], "many");
    assert_eq!(tools["current"].as_array().unwrap().len(), 2, "{tools}");
    let shoes = keys.iter().find(|k| k["attribute"] == "shoe_size").unwrap();
    assert_eq!(shoes["current"][0]["value"], "41");
    assert_eq!(shoes["history"][0]["value"], "41", "newest first");
    assert_eq!(shoes["history"][1]["status"], "superseded");
    assert_eq!(shoes["history"][1]["superseded_by"], ids[1].as_str());

    let as_of = get(
        &fixture,
        TOKEN_A,
        "/v1/facts?subject=person/user-a&attribute=Shoe%20Size&as_of=2025-06",
    )
    .json();
    assert_eq!(as_of["as_of"], "2025-06");
    assert_eq!(as_of["keys"][0]["current"][0]["value"], "40");

    // Retract the 41: the 40 is current again.
    let retracted = post(
        &fixture,
        TOKEN_A,
        &format!("/v1/facts/{}/retract", ids[1]),
        json!({}),
    );
    assert_eq!(u16::from(retracted.status), 200, "{}", retracted.json());
    assert_eq!(retracted.json()["fact"]["status"], "retracted");
    let after = get(&fixture, TOKEN_A, "/v1/facts?attribute=shoe_size").json();
    assert_eq!(after["keys"][0]["current"][0]["value"], "40");
    assert_eq!(after["keys"][0]["current"][0]["status"], "current");

    // Somebody else's id is a 404, the same as no id at all.
    let theirs = post(
        &fixture,
        TOKEN_B,
        &format!("/v1/facts/{}/retract", ids[0]),
        json!({}),
    );
    assert_eq!(u16::from(theirs.status), 404);
    let none = post(&fixture, TOKEN_A, "/v1/facts/fact-nope/retract", json!({}));
    assert_eq!(u16::from(none.status), 404);
}

#[test]
fn shared_facts_are_read_by_the_whole_workspace() {
    let fixture = fixture();
    let written = post(
        &fixture,
        TOKEN_A,
        "/v1/facts",
        json!({ "shared": true, "facts": [{
            "subject": "system/postgres", "attribute": "version", "value": 16,
            "valid_from": "2026-04",
        }]}),
    );
    assert_eq!(u16::from(written.status), 201, "{}", written.json());
    assert_eq!(written.json()["facts"][0]["scope"], "shared");
    let b = get(&fixture, TOKEN_B, "/v1/facts?subject=system/postgres").json();
    assert_eq!(b["keys"][0]["current"][0]["value"], "16", "{b}");
    let elsewhere = get(&fixture, TOKEN_A_ELSEWHERE, "/v1/facts").json();
    assert_eq!(elsewhere["keys"], json!([]));
}

#[test]
fn a_fact_outside_the_format_is_a_422() {
    let fixture = fixture();
    for (body, why) in [
        (json!({ "facts": [] }), "empty"),
        (
            json!({ "format": 2, "facts": [shoe("40", "2025", "2025-01-01T00:00:00Z")] }),
            "format",
        ),
        (
            json!({ "facts": [{ "subject": "nick", "attribute": "a", "value": "b" }] }),
            "subject",
        ),
        (
            json!({ "facts": [{ "subject": "me", "attribute": "a", "value": "b",
                                "valid_from": "last year" }] }),
            "valid_from",
        ),
        (
            json!({ "facts": [{ "subject": "me", "attribute": "a", "value": ["b"] }] }),
            "value",
        ),
        (
            json!({ "facts": [{ "subject": "me", "attribute": "a", "value": "b",
                                "status": "current" }] }),
            "unknown field",
        ),
    ] {
        let response = post(&fixture, TOKEN_A, "/v1/facts", body.clone());
        let status = u16::from(response.status);
        assert!(status == 422 || status == 400, "{why}: {status} {body}");
    }
    let bad_as_of = get(&fixture, TOKEN_A, "/v1/facts?as_of=soon");
    assert_eq!(u16::from(bad_as_of.status), 422);
}

#[test]
fn the_facts_routes_answer_401_without_a_credential() {
    let fixture = fixture();
    assert_eq!(u16::from(get_anon(&fixture, "/v1/facts").status), 401);
    assert_eq!(
        u16::from(post_anon(&fixture, "/v1/facts", r#"{"facts":[]}"#).status),
        401
    );
}

#[test]
fn search_and_the_page_view_carry_the_timeline() {
    let fixture = fixture();
    shoe_size_history(&fixture);
    let searched = get(&fixture, TOKEN_A, "/v1/search?q=my%20shoe%20size").json();
    let line = searched["facts"][0]["line"].as_str().unwrap();
    assert!(
        line.starts_with("shoe_size(person/user-a) = 41 EU"),
        "{line}"
    );

    // A decision page with facts about it shows them, rendered.
    seed(
        &fixture,
        WORKSPACE,
        &user("user-a"),
        "db-choice",
        "---\ntitle: Database choice\ndecided_at: 2026-04\nsupersedes: db-choice-2024\n---\nPostgres.",
    );
    let fact = post(
        &fixture,
        TOKEN_A,
        "/v1/facts",
        json!({ "facts": [{ "subject": "decision/db-choice", "attribute": "chosen",
                            "value": "postgres", "valid_from": "2026-04" }] }),
    );
    assert_eq!(u16::from(fact.status), 201);
    let page = get(&fixture, TOKEN_A, "/v1/pages/db-choice").json();
    assert_eq!(page["subject"], "decision/db-choice");
    assert_eq!(page["facts"][0]["current"][0]["value"], "postgres");
    let markdown = page["facts_markdown"].as_str().unwrap();
    assert!(
        markdown.starts_with("## Facts\n\n### Current\n"),
        "{markdown}"
    );
    assert!(
        markdown.contains("| chosen | postgres | 2026-04 – now |"),
        "{markdown}"
    );
    assert!(markdown.contains("### History"), "{markdown}");
}
