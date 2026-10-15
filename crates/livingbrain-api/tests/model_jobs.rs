//! `/v1/model-jobs`: a member's own runner claims the model calls of their
//! jobs and hands the answers back. A compile owned by a member who runs
//! their jobs on their device goes out as a job, waits as `waiting-runner`,
//! and carries on the moment the answer arrives — and nobody else, in this
//! workspace or another, can claim or read that job.

mod common;

use std::sync::Arc;

use cratefield_core::axum::http::StatusCode;
use cratefield_testing::FakeDefer;
use livingbrain_compiler::layout;
use livingbrain_pages::{SourceIngest, SourceKind};
use serde_json::{Value, json};

use common::{
    TOKEN_A, TOKEN_A_ELSEWHERE, TOKEN_A_NARROW, TOKEN_B, WORKSPACE, fixture, get, post, scope_of,
};

/// Syncs one session for `user` through the real pipeline.
fn sync(fx: &common::Fixture, user: &str, session: &str, text: &str) -> String {
    let body = json!({
        "schema_version": 1, "agent": "codex", "collector_version": 1,
        "session_id": session, "repo": "~/code/demo",
        "started_at": "2026-10-15T01:00:00Z",
        // A request and an edit: real work, which triage compiles in full.
        "turns": [
            {"role": "user", "text": text, "tool_calls": []},
            {"role": "assistant", "text": "Done.",
             "tool_calls": [{"name": "apply_patch", "path": "src/db.rs", "status": "ok"}]}
        ],
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

/// `user` runs their Main jobs on their own device, with Claude Code.
fn runner_on(fx: &common::Fixture, user: &str) {
    pollster::block_on(livingbrain_models::jobs::set_runner(
        &*fx.db(),
        time::OffsetDateTime::UNIX_EPOCH,
        WORKSPACE,
        user,
        "main",
        "claude",
        "sonnet",
        "isolated",
    ))
    .expect("a runner row");
}

fn decisions() -> String {
    layout::project_page("codex", "demo", "decisions")
}

/// The closed edit list a compile answers with.
fn edits(body: &str) -> String {
    json!({
        "format": 1,
        "summary": "worked on the demo",
        "pages": [{"slug": decisions(), "base_version": null, "title": "Decisions", "body": body}],
        "facts": [],
    })
    .to_string()
}

/// Starts the compile of user-a's session: with a runner, it goes out as a
/// job and the compile waits.
fn compile_waits(fx: &common::Fixture) -> Value {
    let response = post(fx, TOKEN_A, "/v1/coding-agents/process", json!({"max": 1}));
    assert_eq!(
        response.status,
        StatusCode::OK,
        "{}",
        common::body_text(&response)
    );
    let body = response.json();
    assert_eq!(body["processed"][0]["status"], "waiting-runner", "{body}");
    body
}

#[test]
fn a_runner_compile_round_trips_and_redacts_the_answer() {
    let fx = fixture();
    runner_on(&fx, "user-a");
    sync(&fx, "user-a", "s-a", "we chose postgres for the demo");
    compile_waits(&fx);

    // The queue names the member's own runner as the model.
    let queue = get(&fx, TOKEN_A, "/v1/coding-agents/queue").json();
    assert_eq!(queue["model"]["runner"], true, "{queue}");
    assert_eq!(queue["model"]["provider"], "local-runner:claude");
    assert_eq!(queue["counts"]["waiting-runner"], 1);

    let claimed = get(&fx, TOKEN_A, "/v1/model-jobs/next");
    assert_eq!(
        claimed.status,
        StatusCode::OK,
        "{}",
        common::body_text(&claimed)
    );
    let job = claimed.json()["job"].clone();
    assert_eq!(job["kind"], "compile");
    assert_eq!(
        (job["agent"].as_str(), job["model"].as_str()),
        (Some("claude"), Some("sonnet"))
    );
    assert_eq!(job["json"], true);
    // The same prompt an API model would get: the system rules and the
    // transcript, redacted.
    assert!(job["system"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(
        job["user"]
            .as_str()
            .expect("a prompt")
            .contains("we chose postgres")
    );
    // Leased: a second poll finds nothing to take.
    assert_eq!(
        get(&fx, TOKEN_A, "/v1/model-jobs/next").status,
        StatusCode::NO_CONTENT
    );

    let id = job["id"].as_str().expect("an id");
    let secret = "AKIAIOSFODNN7EXAMPLE";
    let answer = edits(&format!("- Postgres, not SQLite. Key was {secret}."));
    let done = post(
        &fx,
        TOKEN_A,
        &format!("/v1/model-jobs/{id}/result"),
        json!({"text": answer, "prompt_tokens": 1200, "completion_tokens": 80}),
    );
    assert_eq!(done.status, StatusCode::OK, "{}", common::body_text(&done));
    let done = done.json();
    assert_eq!(done["status"], "done");
    // The compile carried on at once, and finished.
    assert_eq!(done["continued"][0]["status"], "done", "{done}");

    let page = pollster::block_on(fx.store.read(&scope_of(WORKSPACE, "user-a"), &decisions()))
        .expect("a read")
        .expect("the compiled page");
    assert!(page.markdown.contains("Postgres, not SQLite"));
    assert!(!page.markdown.contains(secret), "redacted on arrival");

    // The job is consumed: its tokens are recorded, its prompt and answer gone.
    let row = get(&fx, TOKEN_A, &format!("/v1/model-jobs/{id}")).json();
    assert_eq!(row["status"], "consumed");
    assert_eq!(row["tokens"]["prompt"], 1200);
    assert!(row.get("user").is_none() && row.get("text").is_none());
    let sealed = pollster::block_on(fx.db().query(&cratefield_core::Statement::new(
        "SELECT COUNT(*) AS n FROM model_jobs WHERE payload IS NOT NULL OR result IS NOT NULL",
    )))
    .expect("a count");
    assert_eq!(sealed.first().and_then(|r| r.get::<i64>("n")), Some(0));
    // A runner's tokens cost the workspace's compile budget nothing.
    let queue = get(&fx, TOKEN_A, "/v1/coding-agents/queue").json();
    assert_eq!(queue["budget"]["spent_today"], 0, "{queue}");
    // The listing shows it, and when the runner was last seen.
    let listed = get(&fx, TOKEN_A, "/v1/model-jobs").json();
    assert_eq!(listed["jobs"][0]["id"], id);
    assert!(listed["runner"]["last_seen_at"].is_string(), "{listed}");
}

#[test]
fn nobody_else_can_claim_read_or_answer_a_member_s_job() {
    let fx = fixture();
    runner_on(&fx, "user-a");
    sync(&fx, "user-a", "s-a", "private notes");
    compile_waits(&fx);

    // Another member of the workspace, and the same user id in another
    // workspace, find nothing to claim.
    assert_eq!(
        get(&fx, TOKEN_B, "/v1/model-jobs/next").status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        get(&fx, TOKEN_A_ELSEWHERE, "/v1/model-jobs/next").status,
        StatusCode::NO_CONTENT
    );
    // A token narrowed to fewer scopes may not run jobs at all.
    assert_eq!(
        get(&fx, TOKEN_A_NARROW, "/v1/model-jobs/next").status,
        StatusCode::FORBIDDEN
    );
    // No credential, no jobs.
    assert_eq!(
        common::get_anon(&fx, "/v1/model-jobs/next").status,
        StatusCode::UNAUTHORIZED
    );

    let job = get(&fx, TOKEN_A, "/v1/model-jobs/next").json()["job"].clone();
    let id = job["id"].as_str().expect("an id");
    // Someone else's id is the same 404 as no id at all.
    for token in [TOKEN_B, TOKEN_A_ELSEWHERE] {
        assert_eq!(
            get(&fx, token, &format!("/v1/model-jobs/{id}")).status,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            post(
                &fx,
                token,
                &format!("/v1/model-jobs/{id}/result"),
                json!({"text": edits("- hijack")}),
            )
            .status,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            post(
                &fx,
                token,
                &format!("/v1/model-jobs/{id}/fail"),
                json!({"reason": "timeout"})
            )
            .status,
            StatusCode::NOT_FOUND
        );
        let listed = get(&fx, token, "/v1/model-jobs").json();
        assert_eq!(listed["jobs"].as_array().map(Vec::len), Some(0));
    }
    assert_eq!(
        get(&fx, TOKEN_A, &format!("/v1/model-jobs/{id}")).json()["status"],
        "claimed"
    );
}

#[test]
fn an_answer_that_is_not_the_edit_list_fails_the_job_and_the_compile_retries() {
    let fx = fixture();
    runner_on(&fx, "user-a");
    sync(&fx, "user-a", "s-a", "hello");
    compile_waits(&fx);
    let id = get(&fx, TOKEN_A, "/v1/model-jobs/next").json()["job"]["id"]
        .as_str()
        .expect("an id")
        .to_owned();
    let refused = post(
        &fx,
        TOKEN_A,
        &format!("/v1/model-jobs/{id}/result"),
        json!({"text": "Sure! Here are your pages."}),
    );
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        get(&fx, TOKEN_A, &format!("/v1/model-jobs/{id}")).json()["status"],
        "consumed",
        "the compile read the failure"
    );
    let queue = get(&fx, TOKEN_A, "/v1/coding-agents/queue").json();
    let compile = &queue["jobs"][0];
    assert_eq!(compile["status"], "failed", "{queue}");
    assert_eq!(compile["error"], "model-runner-bad-output");
    assert!(compile["next_attempt_at"].is_string(), "it is retried");
}

#[test]
fn a_runner_failure_is_recorded_from_a_fixed_vocabulary() {
    let fx = fixture();
    runner_on(&fx, "user-a");
    sync(&fx, "user-a", "s-a", "hello");
    compile_waits(&fx);
    let id = get(&fx, TOKEN_A, "/v1/model-jobs/next").json()["job"]["id"]
        .as_str()
        .expect("an id")
        .to_owned();
    let failed = post(
        &fx,
        TOKEN_A,
        &format!("/v1/model-jobs/{id}/fail"),
        json!({"reason": "Error: <stderr of the CLI with secrets>"}),
    );
    assert_eq!(
        failed.status,
        StatusCode::OK,
        "{}",
        common::body_text(&failed)
    );
    assert_eq!(failed.json()["job"]["error"], "cli-error");
    let queue = get(&fx, TOKEN_A, "/v1/coding-agents/queue").json();
    assert_eq!(
        queue["jobs"][0]["error"], "model-runner-cli-error",
        "{queue}"
    );
}

#[test]
fn without_a_runner_nothing_is_queued_for_one() {
    let fx = fixture();
    sync(&fx, "user-a", "s-a", "hello");
    let body = post(&fx, TOKEN_A, "/v1/coding-agents/process", json!({"max": 1})).json();
    assert_eq!(body["processed"][0]["status"], "skipped-no-model", "{body}");
    assert_eq!(
        get(&fx, TOKEN_A, "/v1/model-jobs/next").status,
        StatusCode::NO_CONTENT
    );
    // user-b's runner does not serve user-a's session.
    runner_on(&fx, "user-b");
    let body = post(&fx, TOKEN_A, "/v1/coding-agents/process", json!({"max": 1})).json();
    assert_eq!(body["processed"][0]["status"], "skipped-no-model", "{body}");
    assert_eq!(
        get(&fx, TOKEN_B, "/v1/model-jobs/next").status,
        StatusCode::NO_CONTENT
    );
}

#[test]
fn an_ask_with_a_runner_is_answered_on_the_asker_s_device() {
    let fx = fixture();
    common::seed(
        &fx,
        WORKSPACE,
        &common::user("user-a"),
        "kestrel",
        "---\ntitle: Kestrel\n---\nKestrel stores its state in Postgres.\n",
    );
    runner_on(&fx, "user-a");
    let ask = json!({"question": "kestrel state"});

    let pending = post(&fx, TOKEN_A, "/v1/ask", ask.clone());
    assert_eq!(
        pending.status,
        StatusCode::ACCEPTED,
        "{}",
        common::body_text(&pending)
    );
    let pending = pending.json();
    assert_eq!(pending["status"], "pending");
    let id = pending["job"].as_str().expect("a job id").to_owned();
    assert_eq!(pending["poll"], format!("/v1/model-jobs/{id}"));
    assert!(
        pending["citations"]
            .as_array()
            .is_some_and(|c| !c.is_empty())
    );
    // Asked again before the runner answered: the same job, still pending.
    let again = post(&fx, TOKEN_A, "/v1/ask", ask.clone());
    assert_eq!(again.status, StatusCode::ACCEPTED);
    assert_eq!(again.json()["job"], id.as_str());

    // Another member asking the same has no runner: no job, no 202.
    let theirs = post(&fx, TOKEN_B, "/v1/ask", ask.clone());
    assert_eq!(theirs.status, StatusCode::OK);
    assert_eq!(
        get(&fx, TOKEN_B, "/v1/model-jobs/next").status,
        StatusCode::NO_CONTENT
    );

    let job = get(&fx, TOKEN_A, "/v1/model-jobs/next").json()["job"].clone();
    assert_eq!(job["id"], id.as_str());
    assert_eq!(job["kind"], "ask");
    assert_eq!(job["json"], false);
    assert!(
        job["user"]
            .as_str()
            .expect("a prompt")
            .contains("Kestrel stores its state")
    );
    let done = post(
        &fx,
        TOKEN_A,
        &format!("/v1/model-jobs/{id}/result"),
        json!({"text": "In Postgres [1]. Also see [9].", "prompt_tokens": 300, "completion_tokens": 12}),
    );
    assert_eq!(done.status, StatusCode::OK, "{}", common::body_text(&done));
    assert!(done.json()["continued"].is_null());
    assert_eq!(
        get(&fx, TOKEN_A, &format!("/v1/model-jobs/{id}")).json()["status"],
        "done"
    );

    // The client asks again and gets the runner's answer, citation-checked.
    let answered = post(&fx, TOKEN_A, "/v1/ask", ask);
    assert_eq!(
        answered.status,
        StatusCode::OK,
        "{}",
        common::body_text(&answered)
    );
    let answered = answered.json();
    assert_eq!(answered["mode"], "model", "{answered}");
    assert_eq!(answered["model"]["provider"], "local-runner:claude");
    assert_eq!(answered["model"]["model"], "sonnet");
    let text = answered["answer"].as_str().expect("an answer");
    assert!(text.contains("In Postgres [1]"), "{text}");
    assert!(!text.contains("see [9]"), "{text}");
    assert_eq!(answered["dropped_citations"], json!(["9"]));
    assert_eq!(
        get(&fx, TOKEN_A, &format!("/v1/model-jobs/{id}")).json()["status"],
        "consumed"
    );
}
