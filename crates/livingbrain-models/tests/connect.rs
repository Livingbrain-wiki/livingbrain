//! `models`: connecting a model, and what a connect answers.
//!
//! The three acceptance criteria of issue #10 each have a test here: a
//! custom endpoint that resolves into a private range is refused (twice,
//! once per range), a model that cannot call tools is marked "answers
//! only" naming tool calling as missing, and the key never appears in a
//! response body or in the stored row.

mod support;

use cratefield_core::axum::http::StatusCode;
use serde_json::{Value, json};
use support::{FakeHttp, KEY, OWNER, PLAIN, WORKSPACE, ciphertext, delete, get, put};

/// A custom endpoint whose host resolves into `10.0.0.0/8` is refused,
/// and nothing is sent to it: the guard runs before the probe.
#[test]
fn a_custom_url_resolving_into_10_0_0_0_8_is_refused() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        http.resolving_to("10.1.2.3");
        let (kit, cookie) = support::signed_in(&http).await;

        let res = put(&kit, "/v1/models/main", &cookie, custom("10.1.2.3")).await;

        assert_eq!(
            res.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{}",
            res.text()
        );
        assert!(res.text().contains("models/ssrf-refused"), "{}", res.text());
        assert!(
            !http
                .requests()
                .iter()
                .any(|(_, uri)| uri.contains("10.1.2.3")),
            "the endpoint was never called: {:?}",
            http.requests()
        );
        assert!(
            ciphertext(&kit, WORKSPACE, "main").await.is_none(),
            "nothing was stored"
        );
    });
}

/// The same for `169.254.0.0/16`, the link-local range a cloud metadata
/// service sits in.
#[test]
fn a_custom_url_resolving_into_link_local_is_refused() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        http.resolving_to("169.254.169.254");
        let (kit, cookie) = support::signed_in(&http).await;

        let res = put(&kit, "/v1/models/main", &cookie, custom("169.254.169.254")).await;

        assert_eq!(
            res.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{}",
            res.text()
        );
        assert!(
            !http
                .requests()
                .iter()
                .any(|(_, uri)| uri.contains("169.254")),
            "the endpoint was never called"
        );
    });
}

/// A name whose DoH answers carry no address at all — a CNAME chain with
/// nothing behind it — resolves to nowhere, and nowhere is a refusal, not
/// a pass.
#[test]
fn a_host_that_resolves_to_no_address_is_refused() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        http.resolving_to_nothing();
        let (kit, cookie) = support::signed_in(&http).await;

        let res = put(
            &kit,
            "/v1/models/main",
            &cookie,
            custom("models.example.com"),
        )
        .await;

        assert_eq!(
            res.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{}",
            res.text()
        );
        assert!(
            res.text().contains("could not be resolved"),
            "{}",
            res.text()
        );
        assert!(
            !http
                .requests()
                .iter()
                .any(|(_, uri)| uri.contains("chat/completions")),
            "the endpoint was never called"
        );
    });
}

/// The probe talks to the host the guard approved, not to a string the
/// member sent: the row's base URL is the normalized one the probe used.
#[test]
fn the_stored_base_url_is_the_one_that_was_probed() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, cookie) = support::signed_in(&http).await;
        let mut body = custom("models.example.com");
        // A trailing empty segment and a `..` the parser folds away.
        body["base_url"] = json!("https://models.example.com/v1/./");

        let res = put(&kit, "/v1/models/main", &cookie, body).await;

        assert_eq!(res.status, StatusCode::OK, "{}", res.text());
        assert_eq!(res.json()["base_url"], "https://models.example.com/v1/");
        assert!(
            http.requests()
                .iter()
                .any(|(_, uri)| uri == "https://models.example.com/v1/chat/completions"),
            "{:?}",
            http.requests()
        );
    });
}

/// A model name is one path segment, not a path: a name carrying a slash
/// must not walk out of `/models`.
#[test]
fn a_model_name_cannot_walk_out_of_its_path() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, cookie) = support::signed_in(&http).await;
        let mut body = custom("93.184.216.34");
        body["model"] = json!("evil/../../admin");

        let res = put(&kit, "/v1/models/main", &cookie, body).await;

        assert_eq!(res.status, StatusCode::OK, "{}", res.text());
        let model_url = http
            .requests()
            .into_iter()
            .find(|(_, uri)| uri.contains("/models/"))
            .map(|(_, uri)| uri)
            .expect("the context-size probe ran");
        assert!(
            !model_url.contains("admin/"),
            "the model name split the path: {model_url}"
        );
        assert!(
            model_url.contains("%2F"),
            "not percent-encoded: {model_url}"
        );
    });
}

/// A URL that is not HTTPS is refused without a request at all.
#[test]
fn a_plain_http_custom_url_is_refused() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, cookie) = support::signed_in(&http).await;

        let mut body = custom("93.184.216.34");
        body["base_url"] = json!("http://models.example.com/v1");
        let res = put(&kit, "/v1/models/main", &cookie, body).await;

        assert_eq!(
            res.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{}",
            res.text()
        );
    });
}

/// A model that answers but never calls a tool is "answers only", and the
/// missing list names tool calling and nothing else.
#[test]
fn a_model_without_tool_calling_is_answers_only() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        http.calling_tools(false);
        let (kit, cookie) = support::signed_in(&http).await;

        let res = put(&kit, "/v1/models/main", &cookie, custom("93.184.216.34")).await;

        assert_eq!(res.status, StatusCode::OK, "{}", res.text());
        let body = res.json();
        assert_eq!(body["status"], "answers only");
        assert_eq!(body["missing"], json!(["tool calling"]));
        assert_eq!(body["context_size"], "200000");
        // No silent fallback: the flag is off unless the member set it.
        assert_eq!(body["fallback_to_managed"], false);
    });
}

/// A model that calls tools and answers in JSON "works".
#[test]
fn a_full_capability_model_works() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, cookie) = support::signed_in(&http).await;

        let res = put(&kit, "/v1/models/main", &cookie, custom("93.184.216.34")).await;

        assert_eq!(res.status, StatusCode::OK, "{}", res.text());
        let body = res.json();
        assert_eq!(body["status"], "works");
        assert_eq!(body["missing"], json!([]));
    });
}

/// `fallback_to_managed` is opt-in: sending it turns it on, and omitting
/// it leaves it off.
#[test]
fn falling_back_to_the_managed_model_is_opt_in() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, cookie) = support::signed_in(&http).await;
        let mut body = custom("93.184.216.34");
        body["fallback_to_managed"] = json!(true);

        let res = put(&kit, "/v1/models/main", &cookie, body).await;

        assert_eq!(res.status, StatusCode::OK, "{}", res.text());
        assert_eq!(res.json()["fallback_to_managed"], true);
    });
}

/// An endpoint that reports no context size is missing exactly that, and
/// still "works" on the two capabilities it does have.
#[test]
fn an_endpoint_that_reports_no_context_size_says_so() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        http.with_context(None);
        let (kit, cookie) = support::signed_in(&http).await;

        let res = put(&kit, "/v1/models/main", &cookie, custom("93.184.216.34")).await;

        assert_eq!(res.status, StatusCode::OK, "{}", res.text());
        let body = res.json();
        assert_eq!(body["status"], "works");
        assert_eq!(body["missing"], json!(["context size"]));
        assert_eq!(body["context_size"], Value::Null);
    });
}

/// The key appears in no response body, in neither the connect's nor the
/// list's, and only its last four characters do.
#[test]
fn the_key_never_appears_in_a_response() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, cookie) = support::signed_in(&http).await;

        let connected = put(&kit, "/v1/models/main", &cookie, custom("93.184.216.34")).await;
        assert_eq!(connected.status, StatusCode::OK, "{}", connected.text());
        let listed = get(&kit, "/v1/models", &cookie).await;
        assert_eq!(listed.status, StatusCode::OK, "{}", listed.text());

        for res in [&connected, &listed] {
            assert!(!res.text().contains(KEY), "leaked key: {}", res.text());
            assert!(
                res.text().contains("…1234"),
                "last four missing: {}",
                res.text()
            );
        }
        assert_eq!(connected.json()["key"], "…1234");
        assert_eq!(listed.json()[0]["key"], "…1234");
    });
}

/// What is stored is the ciphertext, not the key.
#[test]
fn the_stored_key_is_ciphertext() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, cookie) = support::signed_in(&http).await;
        put(&kit, "/v1/models/main", &cookie, custom("93.184.216.34")).await;

        let blob = ciphertext(&kit, WORKSPACE, "main")
            .await
            .expect("the row is stored");
        assert!(!blob.contains(KEY), "the key is in the clear at rest");
    });
}

/// A member who is neither owner nor admin cannot connect.
#[test]
fn a_member_who_is_not_an_admin_cannot_connect() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let kit = support::harness(&http);
        support::seed_workspace(&kit, WORKSPACE, OWNER).await;
        support::seed_member(&kit, WORKSPACE, PLAIN, false).await;
        let cookie = support::session(&kit, WORKSPACE, PLAIN);

        let res = put(&kit, "/v1/models/main", &cookie, custom("93.184.216.34")).await;

        assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.text());
        assert!(
            !http
                .requests()
                .iter()
                .any(|(_, uri)| uri.contains("chat/completions")),
            "the endpoint was never called"
        );
    });
}

/// Removing a connection takes the row away, and only the caller's own
/// workspace's.
#[test]
fn removing_a_connection_deletes_the_row() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, cookie) = support::signed_in(&http).await;
        put(&kit, "/v1/models/main", &cookie, custom("93.184.216.34")).await;

        let res = delete(&kit, "/v1/models/main", &cookie).await;

        assert_eq!(res.status, StatusCode::NO_CONTENT);
        assert!(
            get(&kit, "/v1/models", &cookie)
                .await
                .json()
                .as_array()
                .unwrap()
                .is_empty()
        );
    });
}

/// A role that is not one of the three is refused before anything is
/// resolved or stored.
#[test]
fn an_unknown_role_is_refused() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, cookie) = support::signed_in(&http).await;

        let res = put(&kit, "/v1/models/editor", &cookie, custom("93.184.216.34")).await;

        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.text());
    });
}

/// A request with no session is a 401, and reaches no endpoint.
#[test]
fn a_request_without_a_session_is_refused() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let kit = support::harness(&http);
        support::seed_workspace(&kit, WORKSPACE, OWNER).await;
        support::seed_member(&kit, WORKSPACE, OWNER, true).await;

        let res = get(&kit, "/v1/models", "").await;

        assert_eq!(res.status, StatusCode::UNAUTHORIZED, "{}", res.text());
        assert!(http.requests().is_empty());
    });
}

/// A `custom` connect without a base URL names what is missing.
#[test]
fn a_custom_provider_needs_a_base_url() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, cookie) = support::signed_in(&http).await;
        let mut body = custom("93.184.216.34");
        body["base_url"] = json!(null);

        let res = put(&kit, "/v1/models/main", &cookie, body).await;

        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.text());
        assert!(
            res.text().contains("base_url is required"),
            "{}",
            res.text()
        );
    });
}

/// A custom connect body. The host is public on paper; what it resolves
/// to is the fake's business.
fn custom(host: &str) -> Value {
    json!({
        "provider": "custom",
        "base_url": format!("https://{host}/v1"),
        "api_key": KEY,
        "model": "some-model",
    })
}
