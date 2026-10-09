//! Every provider in the catalog: both wires, both auth styles, variables,
//! and the server-side model list.
//!
//! The catalog is `app/assets/providers.json`, vendored from Colonizer. These
//! tests drive `PUT /v1/models/{role}` and `POST /v1/models/discover`
//! against a fake HTTP client and look at what the probe actually sent: the
//! path for the wire, and the header the key travelled in.

mod support;

use cratefield_core::axum::http::{Method, StatusCode};
use serde_json::{Value, json};
use support::{FakeHttp, KEY, PLAIN, WORKSPACE, put};

fn connect(provider: &str) -> Value {
    json!({"provider": provider, "api_key": KEY, "model": "some-model"})
}

/// The requests the probe made to the model host (DNS lookups left out).
fn model_calls(http: &FakeHttp) -> Vec<support::Sent> {
    http.sent()
}

#[test]
fn an_anthropic_wire_provider_with_x_api_key_is_probed_over_messages() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, cookie) = support::signed_in(&http).await;

        let res = put(&kit, "/v1/models/main", &cookie, connect("anthropic")).await;

        assert_eq!(res.status, StatusCode::OK, "{}", res.text());
        let body = res.json();
        assert_eq!(body["provider"], "anthropic");
        assert_eq!(body["wire"], "anthropic");
        assert_eq!(body["auth"], "x-api-key");
        assert_eq!(body["status"], "works", "{body}");
        assert_eq!(body["base_url"], "https://api.anthropic.com/");

        let calls = model_calls(&http);
        let messages: Vec<_> = calls
            .iter()
            .filter(|c| c.uri == "https://api.anthropic.com/v1/messages")
            .collect();
        assert_eq!(messages.len(), 2, "tool and JSON probes: {calls:#?}");
        for call in &messages {
            assert_eq!(call.x_api_key.as_deref(), Some(KEY));
            assert_eq!(call.authorization, None, "the key goes in one header only");
            assert_eq!(call.anthropic_version.as_deref(), Some("2023-06-01"));
        }
        // The tool probe is in Anthropic's shape, not OpenAI's.
        assert!(
            messages[0].body.contains("\"input_schema\""),
            "{}",
            messages[0].body
        );
        assert!(
            messages[0]
                .body
                .contains("\"tool_choice\":{\"type\":\"any\"}")
        );
        assert!(!calls.iter().any(|c| c.uri.contains("chat/completions")));
        assert!(
            calls
                .iter()
                .any(|c| c.uri == "https://api.anthropic.com/v1/models/some-model")
        );
    });
}

#[test]
fn an_anthropic_wire_provider_with_bearer_auth_sends_authorization() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, cookie) = support::signed_in(&http).await;

        // DeepSeek's Anthropic endpoint, bearer auth, from the vendored catalog.
        let res = put(&kit, "/v1/models/main", &cookie, connect("deepseek")).await;

        assert_eq!(res.status, StatusCode::OK, "{}", res.text());
        assert_eq!(res.json()["wire"], "anthropic");
        assert_eq!(res.json()["auth"], "bearer");
        let calls = model_calls(&http);
        let call = calls
            .iter()
            .find(|c| c.uri == "https://api.deepseek.com/anthropic/v1/messages")
            .expect("the Messages path under the catalog base");
        assert_eq!(
            call.authorization.as_deref(),
            Some(&*format!("Bearer {KEY}"))
        );
        assert_eq!(call.x_api_key, None);
        assert_eq!(call.anthropic_version.as_deref(), Some("2023-06-01"));
    });
}

#[test]
fn an_openai_wire_provider_is_probed_over_chat_completions_without_doubling_v1() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, cookie) = support::signed_in(&http).await;

        // xAI's base URL already ends in /v1.
        let res = put(&kit, "/v1/models/main", &cookie, connect("xai-grok")).await;

        assert_eq!(res.status, StatusCode::OK, "{}", res.text());
        assert_eq!(res.json()["wire"], "openai");
        assert_eq!(res.json()["status"], "works");
        let calls = model_calls(&http);
        let chat: Vec<_> = calls
            .iter()
            .filter(|c| c.uri == "https://api.x.ai/v1/chat/completions")
            .collect();
        assert_eq!(chat.len(), 2, "{calls:#?}");
        assert_eq!(
            chat[0].authorization.as_deref(),
            Some(&*format!("Bearer {KEY}"))
        );
        assert_eq!(
            chat[0].anthropic_version, None,
            "no Anthropic header on the OpenAI wire"
        );
        assert!(chat[0].body.contains("\"tool_choice\":\"required\""));
    });
}

#[test]
fn an_anthropic_wire_model_that_does_not_call_tools_is_answers_only() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        http.calling_tools(false);
        let (kit, cookie) = support::signed_in(&http).await;

        let res = put(&kit, "/v1/models/main", &cookie, connect("kimi")).await;

        assert_eq!(res.status, StatusCode::OK, "{}", res.text());
        assert_eq!(res.json()["status"], "answers only");
        assert_eq!(res.json()["missing"], json!(["tool calling"]));
    });
}

#[test]
fn a_custom_endpoint_names_its_wire_and_auth() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, cookie) = support::signed_in(&http).await;
        let body = json!({
            "provider": "custom",
            "base_url": "https://gateway.example.com",
            "wire": "anthropic",
            "auth": "x-api-key",
            "api_key": KEY,
            "model": "some-model",
        });

        let res = put(&kit, "/v1/models/main", &cookie, body).await;

        assert_eq!(res.status, StatusCode::OK, "{}", res.text());
        assert_eq!(res.json()["provider"], "custom");
        assert_eq!(res.json()["wire"], "anthropic");
        assert_eq!(res.json()["auth"], "x-api-key");
        assert!(
            http.sent()
                .iter()
                .any(|c| c.uri == "https://gateway.example.com/v1/messages"
                    && c.x_api_key.as_deref() == Some(KEY))
        );
    });
}

#[test]
fn a_custom_endpoint_without_a_wire_is_the_openai_wire_as_before() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, cookie) = support::signed_in(&http).await;
        let body = json!({
            "provider": "custom",
            "base_url": "https://gateway.example.com/v1",
            "api_key": KEY,
            "model": "some-model",
        });
        let res = put(&kit, "/v1/models/main", &cookie, body).await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.text());
        assert_eq!(res.json()["wire"], "openai");
        assert_eq!(res.json()["auth"], "bearer");
    });
}

#[test]
fn a_bad_wire_or_auth_on_custom_is_refused() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, cookie) = support::signed_in(&http).await;
        for (field, value) in [("wire", "gemini"), ("auth", "basic")] {
            let mut body = json!({
                "provider": "custom",
                "base_url": "https://gateway.example.com/v1",
                "api_key": KEY,
                "model": "some-model",
            });
            body[field] = json!(value);
            let res = put(&kit, "/v1/models/main", &cookie, body).await;
            assert_eq!(
                res.status,
                StatusCode::BAD_REQUEST,
                "{field}: {}",
                res.text()
            );
        }
        assert!(http.sent().is_empty(), "nothing was called");
    });
}

#[test]
fn an_unknown_provider_is_refused_before_anything_is_called() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, cookie) = support::signed_in(&http).await;
        let res = put(
            &kit,
            "/v1/models/main",
            &cookie,
            connect("openai-compatible"),
        )
        .await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.text());
        assert!(res.text().contains("unknown provider"), "{}", res.text());
        assert!(http.requests().is_empty());
    });
}

#[test]
fn a_provider_with_a_url_variable_needs_it_and_uses_it() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, cookie) = support::signed_in(&http).await;

        let missing = put(&kit, "/v1/models/main", &cookie, connect("kat-coder")).await;
        assert_eq!(
            missing.status,
            StatusCode::BAD_REQUEST,
            "{}",
            missing.text()
        );
        assert!(missing.text().contains("ENDPOINT_ID"), "{}", missing.text());

        let mut hostile = connect("kat-coder");
        hostile["variables"] = json!({"ENDPOINT_ID": "../../elsewhere"});
        let res = put(&kit, "/v1/models/main", &cookie, hostile).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.text());
        assert!(http.sent().is_empty(), "nothing was called");

        let mut body = connect("kat-coder");
        body["variables"] = json!({"ENDPOINT_ID": "ep-abc-123"});
        let res = put(&kit, "/v1/models/main", &cookie, body).await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.text());
        assert_eq!(
            res.json()["base_url"],
            "https://vanchin.streamlake.ai/api/gateway/v1/endpoints/ep-abc-123/claude-code-proxy"
        );
    });
}

#[test]
fn an_edited_base_url_replaces_the_catalog_one_and_is_still_guarded() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        http.resolving_to("10.0.0.7");
        let (kit, cookie) = support::signed_in(&http).await;
        let mut body = connect("minimax");
        body["base_url"] = json!("https://internal.example.com/anthropic");
        let res = put(&kit, "/v1/models/main", &cookie, body).await;
        assert_eq!(
            res.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{}",
            res.text()
        );
        assert!(res.text().contains("models/ssrf-refused"));
        assert!(http.sent().is_empty());
    });
}

#[test]
fn discover_lists_the_providers_models_server_side() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, cookie) = support::signed_in(&http).await;
        let body = json!({"provider": "anthropic", "api_key": KEY});

        let res = support::send(
            &kit,
            Method::POST,
            "/v1/models/discover",
            &cookie,
            Some(body),
        )
        .await;

        assert_eq!(res.status, StatusCode::OK, "{}", res.text());
        assert_eq!(res.json(), json!({"models": ["model-a", "model-b"]}));
        assert!(!res.text().contains(KEY), "the key is never echoed");
        let calls = http.sent();
        assert_eq!(calls.len(), 1, "{calls:#?}");
        assert_eq!(calls[0].uri, "https://api.anthropic.com/v1/models");
        assert_eq!(calls[0].x_api_key.as_deref(), Some(KEY));

        // An OpenAI-wire base that ends in /v1 lists at /v1/models, once.
        let res = support::send(
            &kit,
            Method::POST,
            "/v1/models/discover",
            &cookie,
            Some(json!({"provider": "openai", "api_key": KEY})),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.text());
        assert!(
            http.sent()
                .iter()
                .any(|c| c.uri == "https://api.openai.com/v1/models"
                    && c.authorization.as_deref() == Some(&*format!("Bearer {KEY}")))
        );
    });
}

#[test]
fn discover_names_the_status_of_a_refused_key_and_stores_nothing() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        http.listing_with(401);
        let (kit, cookie) = support::signed_in(&http).await;
        let res = support::send(
            &kit,
            Method::POST,
            "/v1/models/discover",
            &cookie,
            Some(json!({"provider": "zhipu-glm-en", "api_key": KEY})),
        )
        .await;
        assert_eq!(
            res.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{}",
            res.text()
        );
        assert!(res.text().contains("HTTP 401"), "{}", res.text());
        assert!(support::ciphertext(&kit, WORKSPACE, "main").await.is_none());
    });
}

#[test]
fn discover_is_for_admins_only() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, _) = support::signed_in(&http).await;
        let plain = support::session(&kit, WORKSPACE, PLAIN);
        let res = support::send(
            &kit,
            Method::POST,
            "/v1/models/discover",
            &plain,
            Some(json!({"provider": "anthropic", "api_key": KEY})),
        )
        .await;
        assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.text());
        assert!(http.sent().is_empty());
    });
}
