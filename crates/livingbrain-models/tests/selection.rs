//! `models`: which connection serves a job.
//!
//! Issue #140's product rule, as the `GET /v1/models/{role}` endpoint
//! answers it: an unset job uses Main, and with nothing connected the
//! managed model answers — a 404 naming `models/managed`, never a
//! connection that is not there.

mod support;

use cratefield_core::axum::http::StatusCode;
use serde_json::{Value, json};
use support::{FakeHttp, KEY, OWNER, PLAIN, WORKSPACE, get, put};

/// A triage or research job with only a Main connection is served by Main:
/// the endpoint answers with the Main row, `role: "main"` — the body names
/// the *serving* connection, not the role that was asked about.
#[test]
fn an_unset_job_uses_the_main_connection() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, cookie) = support::signed_in(&http).await;
        let connected = put(&kit, "/v1/models/main", &cookie, custom("93.184.216.34")).await;
        assert_eq!(connected.status, StatusCode::OK, "{}", connected.text());

        for role in ["triage", "research"] {
            let res = get(&kit, &format!("/v1/models/{role}"), &cookie).await;
            assert_eq!(res.status, StatusCode::OK, "{}", res.text());
            let body = res.json();
            assert_eq!(body["role"], "main", "{}", res.text());
            assert_eq!(body["provider"], "custom", "{}", res.text());
            assert_eq!(body["model"], "some-model", "{}", res.text());
        }
    });
}

/// A role with a connection of its own is served by it, not by Main.
#[test]
fn a_connected_role_is_served_by_its_own_connection() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, cookie) = support::signed_in(&http).await;
        let mut main = custom("93.184.216.34");
        main["model"] = json!("main-model");
        let mut triage = custom("93.184.216.34");
        triage["model"] = json!("triage-model");
        for (role, body) in [("main", main), ("triage", triage)] {
            let connected = put(&kit, &format!("/v1/models/{role}"), &cookie, body).await;
            assert_eq!(connected.status, StatusCode::OK, "{}", connected.text());
        }

        let res = get(&kit, "/v1/models/triage", &cookie).await;

        assert_eq!(res.status, StatusCode::OK, "{}", res.text());
        let body = res.json();
        assert_eq!(body["role"], "triage");
        assert_eq!(body["model"], "triage-model");
    });
}

/// With nothing connected, the managed model answers: a 404 whose type
/// names `models/managed`, while the list still answers, empty.
#[test]
fn with_nothing_connected_the_managed_model_answers() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, cookie) = support::signed_in(&http).await;

        let res = get(&kit, "/v1/models/main", &cookie).await;

        assert_eq!(res.status, StatusCode::NOT_FOUND, "{}", res.text());
        assert!(res.text().contains("models/managed"), "{}", res.text());

        let listed = get(&kit, "/v1/models", &cookie).await;
        assert_eq!(listed.status, StatusCode::OK, "{}", listed.text());
        assert_eq!(listed.json(), json!([]));
    });
}

/// A role that is not one of the three is refused, the same answer a
/// connect or a remove gives for it.
#[test]
fn an_unknown_role_is_refused() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, cookie) = support::signed_in(&http).await;

        let res = get(&kit, "/v1/models/bogus", &cookie).await;

        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.text());
        assert!(res.text().contains("bogus"), "{}", res.text());
    });
}

/// Resolving is not connecting: any signed-in member can ask which
/// connection serves a job, though only an admin could have connected it.
#[test]
fn a_member_who_is_not_an_admin_can_resolve() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let kit = support::harness(&http);
        support::seed_workspace(&kit, WORKSPACE, OWNER).await;
        support::seed_member(&kit, WORKSPACE, OWNER, true).await;
        support::seed_member(&kit, WORKSPACE, PLAIN, false).await;
        let owner = support::session(&kit, WORKSPACE, OWNER);
        let member = support::session(&kit, WORKSPACE, PLAIN);
        let connected = put(&kit, "/v1/models/main", &owner, custom("93.184.216.34")).await;
        assert_eq!(connected.status, StatusCode::OK, "{}", connected.text());

        let res = get(&kit, "/v1/models/triage", &member).await;

        assert_eq!(res.status, StatusCode::OK, "{}", res.text());
        assert_eq!(res.json()["role"], "main");
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
