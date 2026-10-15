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
