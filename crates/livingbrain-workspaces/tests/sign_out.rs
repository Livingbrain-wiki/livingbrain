//! `POST /v1/workspaces/signout`: the session cookie is dropped with the
//! attributes it was set with, the browser is signed out afterwards, and
//! another site cannot sign a person out.

mod support;

use cratefield_core::axum::body::Body;
use cratefield_core::axum::http::{Method, Request, StatusCode, header};
use livingbrain_workspaces::SESSION_COOKIE;
use serde_json::json;
use support::*;

const ME: &str = "/v1/workspaces/me";
const SIGNOUT: &str = "/v1/workspaces/signout";
const HOST: &str = "brain.example";

/// A sign-out POST with the cookie and whatever browser headers the case
/// needs.
async fn sign_out(
    kit: &cratefield_testing::TestHarness,
    cookie: Option<&str>,
    headers: &[(&str, &str)],
) -> Res {
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(SIGNOUT)
        .header(header::HOST, HOST);
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, format!("{SESSION_COOKIE}={cookie}"));
    }
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    respond(
        &kit.router,
        builder.body(Body::empty()).expect("request builds"),
    )
    .await
}

/// Signs a person in by email, the way the app's own form does.
async fn signed_in(kit: &cratefield_testing::TestHarness) -> String {
    let start = post_json(
        &kit.router,
        "/v1/workspaces/email/start",
        &[],
        &json!({"email": "ada@example.com"}),
    )
    .await;
    assert_eq!(start.status, StatusCode::ACCEPTED);
    let token = mailed_token(kit);
    let verify = post_form(
        &kit.router,
        "/v1/workspaces/email/verify",
        &[],
        &format!("token={token}"),
    )
    .await;
    assert_eq!(verify.status, StatusCode::FOUND);
    verify.cookie(SESSION_COOKIE).expect("a session cookie")
}

#[pollster::test]
async fn signing_out_drops_the_cookie_and_later_calls_are_401() {
    let http = TokenHttp::new();
    let kit = harness(&http);
    let cookie = signed_in(&kit).await;

    let before = get(&kit.router, ME, &[(SESSION_COOKIE, cookie.as_str())]).await;
    assert_eq!(before.status, StatusCode::OK, "signed in first");

    let out = sign_out(
        &kit,
        Some(&cookie),
        &[
            ("sec-fetch-site", "same-origin"),
            ("origin", "https://brain.example"),
        ],
    )
    .await;
    assert_eq!(out.status, StatusCode::NO_CONTENT);
    assert!(out.body.is_empty(), "a 204 has no body");

    // One Set-Cookie, for the session, with the attributes it was set with
    // and an expiry in the past.
    let set = out.set_cookies();
    assert_eq!(set.len(), 1, "{set:?}");
    let cleared = &set[0];
    assert!(cleared.starts_with("__Host-lb_session=;"), "{cleared}");
    for attribute in ["Path=/", "Secure", "HttpOnly", "SameSite=Lax", "Max-Age=0"] {
        assert!(cleared.contains(attribute), "{attribute} in {cleared}");
    }
    assert!(
        !cleared.contains("Domain"),
        "__Host- forbids Domain: {cleared}"
    );

    // The browser applies the Set-Cookie: it has no session cookie left, so
    // the next call carries none and is refused.
    assert_eq!(out.cookie(SESSION_COOKIE).as_deref(), Some(""));
    let after = get(&kit.router, ME, &[]).await;
    assert_eq!(after.status, StatusCode::UNAUTHORIZED);
    let members = get(&kit.router, "/v1/workspaces/members", &[]).await;
    assert_eq!(members.status, StatusCode::UNAUTHORIZED);
}

#[pollster::test]
async fn signing_out_with_no_session_is_still_a_204() {
    let http = TokenHttp::new();
    let kit = harness(&http);
    // A second tab, or a second click: nothing to end is not an error.
    let out = sign_out(&kit, None, &[("sec-fetch-site", "same-origin")]).await;
    assert_eq!(out.status, StatusCode::NO_CONTENT);
    assert_eq!(out.cookie(SESSION_COOKIE).as_deref(), Some(""));
}

#[pollster::test]
async fn a_cross_site_sign_out_is_refused_and_the_session_survives() {
    let http = TokenHttp::new();
    let kit = harness(&http);
    let cookie = signed_in(&kit).await;

    for headers in [
        vec![("sec-fetch-site", "cross-site")],
        // A sibling subdomain is same-site, and SameSite=Lax would send the
        // cookie along, so this is the case the check exists for.
        vec![("sec-fetch-site", "same-site")],
        vec![("origin", "https://evil.example")],
        vec![("origin", "https://app.brain.example")],
    ] {
        let out = sign_out(&kit, Some(&cookie), &headers).await;
        assert_eq!(out.status, StatusCode::FORBIDDEN, "{headers:?}");
        assert!(out.set_cookies().is_empty(), "nothing cleared: {headers:?}");
        let body = out.json();
        assert_eq!(
            body["type"]
                .as_str()
                .map(|t| t.ends_with("workspaces/cross-site-request")),
            Some(true),
            "{body}"
        );
    }

    let still = get(&kit.router, ME, &[(SESSION_COOKIE, cookie.as_str())]).await;
    assert_eq!(still.status, StatusCode::OK, "the session was not touched");
}

#[pollster::test]
async fn sign_out_is_a_post_only() {
    let http = TokenHttp::new();
    let kit = harness(&http);
    let out = get(&kit.router, SIGNOUT, &[]).await;
    assert_eq!(out.status, StatusCode::METHOD_NOT_ALLOWED);
}
