//! `GET /v1/coding-agents/queue` and `POST /v1/coding-agents/process`: the
//! compiler's queue, answered only for the asker's own sessions — and the
//! facts a compile records, through the composition's real sink, read back
//! through `/v1/facts`.

mod common;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use cratefield_core::ScheduledBudget;
use cratefield_core::axum::http::StatusCode;
use cratefield_testing::FakeDefer;
use livingbrain_compiler::{
    Call, ClaimFilter, JobStatus, Model, ModelFailure, Processed, Reply, layout,
};
use livingbrain_pages::{SourceIngest, SourceKind};
use serde_json::{Value, json};

use common::{TOKEN_A, TOKEN_A_ELSEWHERE, TOKEN_B, WORKSPACE, fixture, get, post, scope_of};

/// Syncs one session for `user` through the real pipeline.
fn sync(fx: &common::Fixture, user: &str, session: &str) -> String {
    sync_on(fx, user, session, "2026-10-15T01:00:00Z")
}

/// Syncs one session for `user` that started at `started_at`.
fn sync_on(fx: &common::Fixture, user: &str, session: &str, started_at: &str) -> String {
    let body = json!({
        "schema_version": 1, "agent": "codex", "collector_version": 1,
        "session_id": session, "repo": "~/code/demo",
        "started_at": started_at,
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

/// The workspace's own budgets (Settings → Usage & limits) are what the
/// queue card shows and what "Process now" runs under: a paused workspace
/// parks its job as `over-budget` (`paused`) without asking any model.
#[test]
fn the_queue_and_process_now_run_under_the_workspace_s_own_budgets() {
    let fx = fixture();
    let mine = sync(&fx, "user-a", "s-a");
    let own = livingbrain_usage::Budgets {
        compiler_paused: Some(true),
        compiler_daily_tokens: Some(7_000_000),
        compiler_batch: Some(4),
        ..livingbrain_usage::Budgets::default()
    };
    pollster::block_on(livingbrain_usage::save_budgets(
        &*fx.db(),
        fx.clock().now(),
        WORKSPACE,
        &own,
    ))
    .expect("the budgets save");

    let card = get(&fx, TOKEN_A, "/v1/coding-agents/queue").json();
    assert_eq!(card["budget"]["paused"], true, "{card}");
    assert_eq!(card["budget"]["daily_tokens"], 7_000_000, "{card}");
    assert_eq!(card["budget"]["batch"], 4, "{card}");
    assert_eq!(
        card["budget"]["monthly_tokens"], 4_000_000,
        "the default, unset"
    );

    let body = post(&fx, TOKEN_A, "/v1/coding-agents/process", json!({"max": 3})).json();
    assert_eq!(body["processed"][0]["source_id"], mine.as_str(), "{body}");
    assert_eq!(body["processed"][0]["status"], "over-budget", "{body}");
    assert_eq!(body["processed"][0]["error"], "paused", "{body}");

    // Another workspace's card is its own.
    let elsewhere = get(&fx, TOKEN_A_ELSEWHERE, "/v1/coding-agents/queue").json();
    assert_eq!(elsewhere["budget"]["paused"], false, "{elsewhere}");
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

/// A model whose every call fails the way Z.AI failed the compile in
/// production, before the error carried what the provider said.
struct Refusing;

#[async_trait::async_trait]
impl Model for Refusing {
    async fn available(&self, _workspace: &str, _owner: &str) -> Result<bool, String> {
        Ok(true)
    }
    async fn complete(
        &self,
        _call: &Call<'_>,
        _system: &str,
        _user: &str,
        _max_tokens: u32,
    ) -> Result<Reply, ModelFailure> {
        Err(ModelFailure::Failed(
            r#"http 400: {"error":{"message":"bad request"}}"#.to_owned(),
        ))
    }
}

/// A job that ran out of attempts shows why, and "Retry failed" puts the
/// asker's own failed jobs — and nobody else's — back in the queue.
#[test]
fn retry_requeues_only_the_asker_s_failed_jobs() {
    let fx = fixture();
    let mine = sync(&fx, "user-a", "s-a");
    sync(&fx, "user-b", "s-b");
    let compiler = fx.compiler(Arc::new(Refusing));
    // A drain takes a bounded share per workspace; drain until both ran.
    for _ in 0..2 {
        let processed = pollster::block_on(compiler.drain(
            &ClaimFilter::default(),
            10,
            &ScheduledBudget::unbounded(),
        ))
        .expect("a drain");
        assert!(
            processed.iter().all(|p| p.status == JobStatus::Failed),
            "{processed:?}"
        );
    }

    let queue = get(&fx, TOKEN_A, "/v1/coding-agents/queue").json();
    assert_eq!(queue["counts"]["failed"], 1, "{queue}");
    assert_eq!(
        queue["jobs"][0]["error"],
        r#"model-http 400: {"error":{"message":"bad request"}}"#
    );

    let response = post(&fx, TOKEN_A, "/v1/coding-agents/retry", json!({}));
    assert_eq!(
        response.status,
        StatusCode::OK,
        "{}",
        common::body_text(&response)
    );
    let body = response.json();
    assert_eq!(body["requeued"], 1, "{body}");
    assert_eq!(body["counts"]["queued"], 1);
    assert_eq!(body["counts"]["failed"], 0);
    let queue = get(&fx, TOKEN_A, "/v1/coding-agents/queue").json();
    assert_eq!(queue["jobs"][0]["source_id"], mine.as_str());
    assert_eq!(queue["jobs"][0]["status"], "queued");
    assert_eq!(queue["jobs"][0]["attempts"], 0);
    // user-b's failed job is still failed.
    let theirs = get(&fx, TOKEN_B, "/v1/coding-agents/queue").json();
    assert_eq!(theirs["jobs"][0]["status"], "failed");
    assert_eq!(
        common::post_anon(&fx, "/v1/coding-agents/retry", "{}").status,
        StatusCode::UNAUTHORIZED
    );
}

/// A model that answers each call with the next scripted reply.
#[derive(Default)]
struct Scripted(Mutex<VecDeque<Value>>);

impl Scripted {
    fn then(&self, facts: Value) {
        self.0.lock().expect("script").push_back(json!({
            "format": 1, "summary": "worked on the demo", "pages": [], "facts": facts,
        }));
    }
}

#[async_trait::async_trait]
impl Model for Scripted {
    async fn available(&self, _workspace: &str, _owner: &str) -> Result<bool, String> {
        Ok(true)
    }
    async fn complete(
        &self,
        _call: &Call<'_>,
        _system: &str,
        _user: &str,
        _max_tokens: u32,
    ) -> Result<Reply, ModelFailure> {
        let reply = self.0.lock().expect("script").pop_front();
        let reply = reply.ok_or_else(|| ModelFailure::Failed("unscripted".to_owned()))?;
        Ok(Reply {
            text: reply.to_string(),
            prompt_tokens: 100,
            completion_tokens: 20,
            runner: false,
        })
    }
}

/// Compiles whatever is queued with `model`, through the composition's
/// real facts sink.
fn compile(fx: &common::Fixture, model: &Arc<Scripted>) -> Vec<Processed> {
    let compiler = fx.compiler(model.clone());
    pollster::block_on(compiler.drain(&ClaimFilter::default(), 10, &ScheduledBudget::unbounded()))
        .expect("a drain")
}

fn log_of(fx: &common::Fixture, user: &str) -> String {
    pollster::block_on(fx.store.read(&scope_of(WORKSPACE, user), layout::LOG))
        .expect("a read")
        .expect("the log page")
        .markdown
}

/// The one `database` timeline of `project/demo` the token can read, if any.
fn database(fx: &common::Fixture, token: &str, as_of: Option<&str>) -> Value {
    let mut path = "/v1/facts?subject=project/demo&attribute=database".to_owned();
    if let Some(as_of) = as_of {
        path.push_str(&format!("&as_of={as_of}"));
    }
    let response = get(fx, token, &path);
    assert_eq!(
        response.status,
        StatusCode::OK,
        "{}",
        common::body_text(&response)
    );
    response.json()
}

#[test]
fn compiled_facts_land_in_the_session_s_scope_and_supersede_across_sessions() {
    let fx = fixture();
    let model = Arc::new(Scripted::default());

    // September's session says the project runs on Supabase; the model
    // gives no time, so it is valid from the session's day.
    let first = sync_on(&fx, "user-a", "s-1", "2026-09-01T09:00:00Z");
    model.then(json!([{
        "subject": "project/demo", "attribute": "database", "value": "Supabase",
        "quote": "we use Supabase", "confidence": 0.9,
    }]));
    let processed = compile(&fx, &model);
    assert_eq!(processed.len(), 1, "{processed:?}");
    assert_eq!(processed[0].status, JobStatus::Done, "{processed:?}");

    // October's session moves it to D1.
    sync_on(&fx, "user-a", "s-2", "2026-10-15T09:00:00Z");
    model.then(json!([{
        "subject": "project/demo", "attribute": "database", "value": "D1",
        "quote": "switch the demo to D1",
    }]));
    let processed = compile(&fx, &model);
    assert_eq!(processed.len(), 1, "{processed:?}");
    assert_eq!(processed[0].status, JobStatus::Done, "{processed:?}");
    assert_eq!(processed[0].error, None, "{processed:?}");

    let now = database(&fx, TOKEN_A, None);
    let keys = now["keys"].as_array().expect("keys");
    assert_eq!(keys.len(), 1, "{now}");
    let timeline = &keys[0];
    // The session's own (personal) scope, nowhere else.
    assert_eq!(timeline["scope"], "personal", "{timeline}");
    assert_eq!(timeline["current"][0]["value"], "D1", "{timeline}");
    let history = timeline["history"].as_array().expect("history");
    assert_eq!(history.len(), 2, "{timeline}");
    let old = history
        .iter()
        .find(|fact| fact["value"] == "Supabase")
        .expect("the superseded fact is kept");
    assert_eq!(old["status"], "superseded", "{old}");
    assert_eq!(old["valid_from"], "2026-09-01", "{old}");
    assert_eq!(old["valid_to"], "2026-10-15", "{old}");
    // Cited to the session's source.
    assert_eq!(old["source"], format!("source:{first}"), "{old}");

    // As of September, it was Supabase.
    let then = database(&fx, TOKEN_A, Some("2026-09"));
    assert_eq!(then["keys"][0]["current"][0]["value"], "Supabase", "{then}");

    // Nobody else reads them: not the other member, not the same user id in
    // another workspace.
    for token in [TOKEN_B, TOKEN_A_ELSEWHERE] {
        let theirs = database(&fx, token, None);
        assert_eq!(
            theirs["keys"].as_array().map(Vec::len),
            Some(0),
            "{token}: {theirs}"
        );
    }

    let log = log_of(&fx, "user-a");
    assert!(log.contains("facts recorded: 1"), "{log}");
    assert!(!log.contains("pending"), "{log}");
}

#[test]
fn a_refused_fact_is_skipped_and_logged_not_a_failed_job() {
    let fx = fixture();
    let model = Arc::new(Scripted::default());

    sync_on(&fx, "user-a", "s-1", "2026-09-01T09:00:00Z");
    model.then(json!([{
        "subject": "project/demo", "attribute": "database", "value": "Supabase",
        "cardinality": "one",
    }]));
    assert_eq!(compile(&fx, &model)[0].status, JobStatus::Done);

    sync_on(&fx, "user-a", "s-2", "2026-10-15T09:00:00Z");
    model.then(json!([
        // The store holds `database` as a `one` attribute: a conflict.
        {"subject": "project/demo", "attribute": "database", "value": "D1", "cardinality": "many"},
        // A range that ends before it starts: the store refuses it.
        {"subject": "project/demo", "attribute": "host", "value": "Workers",
         "valid_from": "2026-10-15", "valid_to": "2026-10-01"},
        // And the one good fact in the batch is still recorded.
        {"subject": "system/codex", "attribute": "language", "value": "Rust"},
    ]));
    let processed = compile(&fx, &model);
    assert_eq!(processed.len(), 1, "{processed:?}");
    assert_eq!(processed[0].status, JobStatus::Done, "{processed:?}");
    assert_eq!(processed[0].error, None, "{processed:?}");

    let rust = get(
        &fx,
        TOKEN_A,
        "/v1/facts?subject=system/codex&attribute=language",
    )
    .json();
    assert_eq!(rust["keys"][0]["current"][0]["value"], "Rust", "{rust}");
    let db = database(&fx, TOKEN_A, None);
    assert_eq!(db["keys"][0]["current"][0]["value"], "Supabase", "{db}");
    let host = get(
        &fx,
        TOKEN_A,
        "/v1/facts?subject=project/demo&attribute=host",
    )
    .json();
    assert_eq!(host["keys"].as_array().map(Vec::len), Some(0), "{host}");

    let log = log_of(&fx, "user-a");
    let line = log
        .lines()
        .rev()
        .find(|line| line.contains("session `s-2`"))
        .expect("the second session's log line");
    assert!(line.contains("facts recorded: 1"), "{line}");
    assert!(line.contains("facts skipped: 2"), "{line}");
    assert!(line.contains("cardinality conflict"), "{line}");
    assert!(
        line.contains("`valid_to` must start after `valid_from`"),
        "{line}"
    );
}
