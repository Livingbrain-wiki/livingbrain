//! Model lists that are not there, keys that are refused, thinking models,
//! and switching a connection's model on the stored key.
//!
//! Many Anthropic-compatible endpoints serve no model list and answer the
//! request with a `404` or `405`; `POST /v1/models/discover` must answer the
//! curated list then, with `listed: false`, never the bare status. Only a
//! refused key (`401`/`403`) is an error.

mod support;

use cratefield_core::axum::http::{Method, StatusCode};
use serde_json::{Value, json};
use support::{FakeHttp, KEY, PLAIN, WORKSPACE, put};

async fn discover(http: &FakeHttp, provider: &str) -> support::Res {
    let (kit, cookie) = support::signed_in(http).await;
    support::send(
        &kit,
        Method::POST,
        "/v1/models/discover",
        &cookie,
        Some(json!({"provider": provider, "api_key": KEY})),
    )
    .await
}

fn ids(body: &Value) -> Vec<String> {
    body["models"]
        .as_array()
        .expect("a models array")
        .iter()
        .map(|m| m["id"].as_str().expect("an id").to_owned())
        .collect()
}

#[test]
fn a_405_from_the_list_is_the_curated_list_not_an_error() {
    pollster::block_on(async {
        for status in [404, 405, 501, 400, 500] {
            let http = FakeHttp::new();
            http.listing_with(status);
            // OpenRouter lists natively; a refusal falls back to its card.
            let res = discover(&http, "openrouter").await;
            assert_eq!(res.status, StatusCode::OK, "{status}: {}", res.text());
            let body = res.json();
            assert_eq!(body["listed"], false, "{status}");
            assert!(ids(&body).contains(&"anthropic/claude-sonnet-5.5".to_owned()));
            assert_eq!(
                body["note"],
                "This provider doesn't publish a model list; showing known models."
            );
            assert!(!res.text().contains(&format!("HTTP {status}")));
            assert!(!res.text().contains(KEY));
        }
    });
}

#[test]
fn a_provider_with_no_list_is_not_asked_for_one() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let res = discover(&http, "zhipu-glm-en").await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.text());
        let body = res.json();
        assert_eq!(body["listed"], false);
        assert_eq!(ids(&body)[..2], ["glm-5.3", "glm-5.3-flash"]);
        assert_eq!(body["models"][1]["context"], 1_000_000);
        assert_eq!(body["models"][1]["price"], "$0.15 / $0.50");
        assert!(http.sent().is_empty(), "{:#?}", http.sent());
    });
}

#[test]
fn a_200_error_envelope_is_no_list() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        http.listing_body(r#"{"code":401,"msg":"token expired or incorrect","success":false}"#);
        let res = discover(&http, "anthropic").await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.text());
        assert_eq!(res.json()["listed"], false);
        assert!(ids(&res.json()).contains(&"claude-opus-5-5".to_owned()));
    });
}

#[test]
fn a_refused_key_is_a_clear_422() {
    pollster::block_on(async {
        for status in [401, 403] {
            let http = FakeHttp::new();
            http.listing_with(status);
            let res = discover(&http, "openai").await;
            assert_eq!(
                res.status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{}",
                res.text()
            );
            assert!(res.text().contains("models/key-refused"), "{}", res.text());
            assert!(
                res.text().contains(&format!("HTTP {status}")),
                "{}",
                res.text()
            );
            assert!(res.text().contains("refused this API key"));
            assert!(!res.text().contains(KEY));
        }
    });
}

#[test]
fn a_listed_200_carries_the_curated_hints_and_the_listed_context() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        http.listing_body(
            &json!({"data": [
                {"id": "claude-opus-5-5"},
                {"id": "claude-new-6", "max_input_tokens": 2_000_000},
            ]})
            .to_string(),
        );
        let res = discover(&http, "anthropic").await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.text());
        let body = res.json();
        assert_eq!(body["listed"], true);
        assert!(body.get("note").is_none());
        assert_eq!(
            body["models"],
            json!([
                {"id": "claude-new-6", "context": 2_000_000},
                {"id": "claude-opus-5-5", "label": "Claude Opus 5.5", "context": 1_000_000, "price": "$4 / $20"},
            ])
        );
    });
}

#[test]
fn a_card_list_url_is_used_and_signed_like_the_provider() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let res = discover(&http, "deepseek").await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.text());
        assert_eq!(res.json()["listed"], true);
        let sent = http.sent();
        assert_eq!(sent.len(), 1, "{sent:#?}");
        assert_eq!(sent[0].uri, "https://api.deepseek.com/models");
        assert_eq!(
            sent[0].authorization.as_deref(),
            Some(&*format!("Bearer {KEY}"))
        );
    });
}

#[test]
fn a_thinking_model_is_probed_through_its_reasoning() {
    pollster::block_on(async {
        for provider in ["zhipu-glm-en", "xai-grok"] {
            let http = FakeHttp::new();
            http.thinking_first().with_context(None);
            let (kit, cookie) = support::signed_in(&http).await;
            let res = put(
                &kit,
                "/v1/models/main",
                &cookie,
                json!({"provider": provider, "api_key": KEY, "model": "glm-5.3-flash"}),
            )
            .await;
            assert_eq!(res.status, StatusCode::OK, "{}", res.text());
            let body = res.json();
            assert_eq!(body["status"], "works", "{provider}: {body}");
            // The endpoint reported no size; Z.AI documents 1M for this model.
            if provider == "zhipu-glm-en" {
                assert_eq!(body["missing"], json!([]), "{body}");
                assert_eq!(body["context_size"], "1000000");
            } else {
                assert_eq!(body["missing"], json!(["context size"]), "{body}");
            }
            // Room for the reasoning before the answer.
            for sent in http.sent().iter().filter(|s| !s.body.is_empty()) {
                let request: Value = serde_json::from_str(&sent.body).expect("JSON body");
                assert!(request["max_tokens"].as_u64() >= Some(1024), "{request}");
                assert!(request.get("thinking").is_none(), "thinking stays on");
            }
        }
    });
}

#[test]
fn a_thinking_model_that_does_not_call_tools_answers_only() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        http.thinking_first().calling_tools(false);
        let (kit, cookie) = support::signed_in(&http).await;
        let res = put(
            &kit,
            "/v1/models/main",
            &cookie,
            json!({"provider": "zhipu-glm-en", "api_key": KEY, "model": "glm-5.3"}),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.text());
        assert_eq!(res.json()["status"], "answers only");
        assert_eq!(res.json()["missing"], json!(["tool calling"]));
    });
}

#[test]
fn patch_switches_the_model_on_the_stored_key() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, cookie) = support::signed_in(&http).await;
        let res = put(
            &kit,
            "/v1/models/main",
            &cookie,
            json!({"provider": "anthropic", "api_key": KEY, "model": "claude-sonnet-5-5"}),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.text());
        let before = support::ciphertext(&kit, WORKSPACE, "main").await;
        let probes_before = http.sent().len();

        let res = support::send(
            &kit,
            Method::PATCH,
            "/v1/models/main",
            &cookie,
            Some(json!({"model": "claude-opus-5-5"})),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.text());
        let body = res.json();
        assert_eq!(body["model"], "claude-opus-5-5");
        assert_eq!(body["provider"], "anthropic");
        assert_eq!(body["key"], format!("…{}", &KEY[KEY.len() - 4..]));
        assert!(!res.text().contains(KEY));
        assert_eq!(support::ciphertext(&kit, WORKSPACE, "main").await, before);
        // The new model was probed with the stored key.
        let probes: Vec<_> = http.sent().into_iter().skip(probes_before).collect();
        assert!(
            probes
                .iter()
                .any(|s| s.body.contains("claude-opus-5-5") && s.x_api_key.as_deref() == Some(KEY))
        );
    });
}

#[test]
fn patch_needs_an_admin_a_connection_and_a_model() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, cookie) = support::signed_in(&http).await;
        let patch = |cookie: String, body: Value| {
            let kit = &kit;
            async move {
                support::send(kit, Method::PATCH, "/v1/models/main", &cookie, Some(body)).await
            }
        };
        let res = patch(cookie.clone(), json!({"model": "m"})).await;
        assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.text());
        put(
            &kit,
            "/v1/models/main",
            &cookie,
            json!({"provider": "anthropic", "api_key": KEY, "model": "a"}),
        )
        .await;
        let res = patch(cookie.clone(), json!({"model": "  "})).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.text());
        let plain = support::session(&kit, WORKSPACE, PLAIN);
        let res = patch(plain, json!({"model": "b"})).await;
        assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.text());
        let res = support::send(
            &kit,
            Method::PATCH,
            "/v1/models/editor",
            &cookie,
            Some(json!({"model": "b"})),
        )
        .await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.text());
    });
}
