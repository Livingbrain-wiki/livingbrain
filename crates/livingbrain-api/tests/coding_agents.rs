//! `GET /v1/coding-agents/queue` and `POST /v1/coding-agents/process`: the
//! compiler's queue, answered only for the asker's own sessions.

mod common;

use std::sync::Arc;

use cratefield_core::axum::http::StatusCode;
use cratefield_testing::FakeDefer;
use livingbrain_pages::{SourceIngest, SourceKind};
use serde_json::{Value, json};

use common::{TOKEN_A, TOKEN_A_ELSEWHERE, TOKEN_B, WORKSPACE, fixture, get, post, scope_of};

/// Syncs one session for `user` through the real pipeline.
fn sync(fx: &common::Fixture, user: &str, session: &str) -> String {
    let body = json!({
        "schema_version": 1, "agent": "codex", "collector_version": 1,
        "session_id": session, "repo": "~/code/demo",
        "started_at": "2026-10-15T01:00:00Z",
        "turns": [{"role": "user", "text": "hello", "tool_calls": []}],
        "totals": {}, "truncated": false
    })
    .to_string();
    pollster::block_on(
        fx.ingestor(Arc::new(FakeDefer::default()))
            .ingest(SourceIngest {
                kind: SourceKind::AgentLog,
                workspace: WORKSPACE.to_owned(),
                scope: scope_of(WORKSPACE, user),
                origin_ref: None,
                author: None,
                rel_path: format!("codex session {session}"),
                imported_by: user.to_owned(),
                body,
                project: Some("demo".to_owned()),
            }),
    )
    .expect("an ingest")
    .source
    .id
}

#[test]
fn the_queue_lists_only_the_asker_s_sessions() {
    let fx = fixture();
    let mine = sync(&fx, "user-a", "s-a");
    sync(&fx, "user-b", "s-b");

    let response = get(&fx, TOKEN_A, "/v1/coding-agents/queue");
    assert_eq!(
        response.status,
        StatusCode::OK,
        "{}",
        common::body_text(&response)
    );
    let body = response.json();
    let jobs = body["jobs"].as_array().expect("jobs");
    assert_eq!(jobs.len(), 1, "{body}");
    assert_eq!(jobs[0]["source_id"], mine.as_str());
    assert_eq!(jobs[0]["status"], "queued");
    assert_eq!(body["counts"]["queued"], 1);
    assert_eq!(body["model"]["connected"], false);
    assert_eq!(body["model"]["role"], "main");
    assert!(body["budget"]["daily_tokens"].as_u64().is_some());

    // The same user id in another workspace sees nothing of it.
    let elsewhere = get(&fx, TOKEN_A_ELSEWHERE, "/v1/coding-agents/queue").json();
    assert_eq!(elsewhere["jobs"].as_array().map(Vec::len), Some(0));
    // And the other member, only theirs.
    let theirs = get(&fx, TOKEN_B, "/v1/coding-agents/queue").json();
    assert_eq!(theirs["jobs"].as_array().map(Vec::len), Some(1));
    assert_ne!(theirs["jobs"][0]["source_id"], mine.as_str());
    // Filtered by source id.
    let one = get(
        &fx,
        TOKEN_A,
        &format!("/v1/coding-agents/queue?source={mine}"),
    )
    .json();
    assert_eq!(one["jobs"].as_array().map(Vec::len), Some(1));
    let refused = get(&fx, TOKEN_A, "/v1/coding-agents/queue?source=..%2Fx");
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
}

#[test]
fn process_now_with_no_model_parks_the_job_and_says_why() {
    let fx = fixture();
    let mine = sync(&fx, "user-a", "s-a");
    let response = post(&fx, TOKEN_A, "/v1/coding-agents/process", json!({"max": 3}));
    assert_eq!(
        response.status,
        StatusCode::OK,
        "{}",
        common::body_text(&response)
    );
    let body: Value = response.json();
    assert_eq!(body["processed"][0]["source_id"], mine.as_str());
    assert_eq!(body["processed"][0]["status"], "skipped-no-model");
    assert_eq!(body["counts"]["skipped-no-model"], 1);
    // Another member pressing it touches nothing of user-a's.
    let theirs = post(&fx, TOKEN_B, "/v1/coding-agents/process", json!({})).json();
    assert_eq!(theirs["processed"].as_array().map(Vec::len), Some(0));
}

#[test]
fn both_routes_need_a_credential() {
    let fx = fixture();
    assert_eq!(
        common::get_anon(&fx, "/v1/coding-agents/queue").status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        common::post_anon(&fx, "/v1/coding-agents/process", "{}").status,
        StatusCode::UNAUTHORIZED
    );
}
