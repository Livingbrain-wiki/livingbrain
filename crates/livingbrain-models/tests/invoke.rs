//! `invoke::chat`: a job runs on the connection a member connected, with
//! the key that connection stored, and its usage lands in the ledger.

mod support;

use std::time::Duration;

use cratefield_core::{Clock, MapConfig};
use cratefield_testing::FixedClock;
use livingbrain_models::invoke::{self, ChatRequest, InvokeError};
use serde_json::json;
use support::{FakeHttp, KEY, WORKSPACE, put};
use time::OffsetDateTime;

fn request() -> ChatRequest<'static> {
    ChatRequest {
        system: "You compile wiki pages.",
        user: "the data",
        max_tokens: 64,
        json: true,
        timeout: Duration::from_secs(5),
    }
}

fn config() -> MapConfig {
    MapConfig::from_pairs([("MODEL_KEYS_SECRET", "a-test-encryption-secret-of-32-bytes")])
}

fn clock() -> FixedClock {
    FixedClock(OffsetDateTime::from_unix_timestamp(support::NOW).expect("an instant"))
}

/// With nothing connected there is no model, and nothing is sent.
#[test]
fn nothing_connected_is_no_model() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, _cookie) = support::signed_in(&http).await;
        let clock = clock();
        let error = invoke::chat(
            &*kit.db,
            &http,
            &config(),
            &clock as &dyn Clock,
            WORKSPACE,
            "main",
            &request(),
        )
        .await
        .expect_err("nothing serves main");
        assert_eq!(error, InvokeError::NoModel);
        assert!(http.sent().is_empty(), "nothing went out");
        assert!(
            invoke::serving(&*kit.db, WORKSPACE, "main")
                .await
                .expect("a read")
                .is_none()
        );
    });
}

/// A connected Main serves a job: the stored key travels in the header the
/// provider expects, the system prompt and the data go out, and the reply's
/// text comes back with the connection that served it.
#[test]
fn a_job_runs_on_the_connected_model_with_its_key() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        http.calling_tools(false);
        let (kit, cookie) = support::signed_in(&http).await;
        let connected = put(
            &kit,
            "/v1/models/main",
            &cookie,
            json!({"provider": "custom", "base_url": "https://llm.example.com/v1",
                   "api_key": KEY, "model": "some-model"}),
        )
        .await;
        assert_eq!(connected.status.as_u16(), 200, "{}", connected.text());
        let before = http.sent().len();

        let clock = clock();
        let reply = invoke::chat(
            &*kit.db,
            &http,
            &config(),
            &clock as &dyn Clock,
            WORKSPACE,
            "research",
            &request(),
        )
        .await
        .expect("the call answers");
        assert_eq!(reply.role, "main", "an unset role falls back to Main");
        assert_eq!(reply.model, "some-model");
        assert_eq!(reply.text, "{\"ok\":true}");

        let sent = http.sent();
        let call = &sent[before];
        assert!(call.uri.ends_with("/v1/chat/completions"), "{}", call.uri);
        assert_eq!(
            call.authorization.as_deref(),
            Some(format!("Bearer {KEY}").as_str())
        );
        assert!(call.body.contains("You compile wiki pages."));
        assert!(call.body.contains("json_object"));
        let serving = invoke::serving(&*kit.db, WORKSPACE, "triage")
            .await
            .expect("a read")
            .expect("Main serves triage");
        assert_eq!(serving.provider, "custom");
    });
}

/// A stored key never reaches an error: a provider failure is a status.
#[test]
fn an_error_names_no_key() {
    let error = InvokeError::Http(Some(401));
    assert!(!error.to_string().contains(KEY));
    assert_eq!(error.variant(), "http");
}

/// Connects Main to Z.AI's GLM-5.3-Flash on the Anthropic wire, as
/// production is, and answers the request count before any job runs.
async fn glm(http: &FakeHttp) -> (cratefield_testing::TestHarness, usize) {
    http.calling_tools(false);
    let (kit, cookie) = support::signed_in(http).await;
    let connected = put(
        &kit,
        "/v1/models/main",
        &cookie,
        json!({"provider": "zhipu-glm-en", "api_key": KEY, "model": "glm-5.3-flash"}),
    )
    .await;
    assert_eq!(connected.status.as_u16(), 200, "{}", connected.text());
    let before = http.sent().len();
    (kit, before)
}

async fn run(
    kit: &cratefield_testing::TestHarness,
    http: &FakeHttp,
) -> Result<invoke::ChatReply, InvokeError> {
    let clock = clock();
    invoke::chat(
        &*kit.db,
        http,
        &config(),
        &clock as &dyn Clock,
        WORKSPACE,
        "main",
        &request(),
    )
    .await
}

/// The production failure: Z.AI refuses a compile with 429 and code 1302.
/// The error carries the status, the provider's body and its Retry-After —
/// and never the key, even when the provider echoes it back.
#[test]
fn a_provider_refusal_carries_its_status_and_body_but_not_the_key() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, _) = glm(&http).await;
        let body = format!(
            r#"{{"type":"error","error":{{"type":"rate_limit_error","code":"1302","message":"[1302][Rate limit reached for requests]"}},"echo":"{KEY}"}}"#
        );
        http.failing_chat(429, &body, Some("17"));
        let error = run(&kit, &http).await.expect_err("refused");
        assert!(error.rate_limited());
        assert_eq!(error.retry_after_secs(), Some(17));
        assert_eq!(error.variant(), "http");
        let reason = error.reason();
        assert!(reason.starts_with("http 429: {"), "{reason}");
        assert!(
            reason.contains("Rate limit reached for requests"),
            "{reason}"
        );
        assert!(!reason.contains(KEY), "{reason}");
        assert!(!error.to_string().contains(KEY));
        assert!(!format!("{error:?}").contains(KEY));
        // Nothing was answered, so nothing is in the ledger as spent.
    });
}

/// A long body is cut, on one line.
#[test]
fn a_long_error_body_is_cut_to_one_line() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, _) = glm(&http).await;
        let long = format!("{{\"error\":\"{}\n{}\"}}", "x".repeat(400), "y".repeat(400));
        http.failing_chat(400, &long, None);
        let error = run(&kit, &http).await.expect_err("refused");
        assert!(!error.rate_limited());
        let InvokeError::Status(upstream) = &error else {
            panic!("{error:?}");
        };
        assert_eq!(upstream.status, 400);
        assert!(upstream.body.chars().count() <= invoke::MAX_DETAIL_CHARS);
        assert!(!upstream.body.contains('\n'));
        assert!(upstream.body.ends_with('…'));
    });
}

/// A transport failure says what failed, scrubbed of the key.
#[test]
fn a_transport_failure_is_named_without_the_key() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, _) = glm(&http).await;
        http.unreachable_chat(&format!("connection reset; Authorization: Bearer {KEY}"));
        let error = run(&kit, &http).await.expect_err("unreachable");
        let reason = error.reason();
        assert!(
            reason.starts_with("http: transport: connection reset"),
            "{reason}"
        );
        assert!(!reason.contains(KEY), "{reason}");
    });
}

/// A forced-thinking model that spends the whole ceiling thinking is named
/// as such, not as a bad response.
#[test]
fn thinking_through_the_whole_budget_is_exhausted() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, _) = glm(&http).await;
        http.exhausting();
        let error = run(&kit, &http).await.expect_err("no answer");
        assert_eq!(error, InvokeError::Exhausted);
        assert_eq!(error.reason(), "max-tokens");
    });
}

/// The compile request Z.AI gets: the Anthropic Messages shape and nothing
/// a GLM-5.3 endpoint might refuse — no thinking switch, no sampling knobs,
/// no `response_format`, no `tool_choice`, the system prompt as a string.
#[test]
fn the_glm_request_carries_only_what_the_messages_api_takes() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, before) = glm(&http).await;
        http.thinking_first();
        let reply = run(&kit, &http).await.expect("answers");
        assert_eq!(reply.text, "{\"ok\":true}");
        let sent = &http.sent()[before];
        assert!(sent.uri.ends_with("/v1/messages"), "{}", sent.uri);
        let body: serde_json::Value = serde_json::from_str(&sent.body).expect("json");
        let mut keys: Vec<&str> = body
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["max_tokens", "messages", "model", "system"]);
        assert!(body["system"].is_string());
    });
}
