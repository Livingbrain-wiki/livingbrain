//! The connector sign-in over the wire (issue #72): an MCP client registers,
//! sends its user to the consent, and redeems the code it is handed for a
//! personal access token and a renewal token. Every refusal the grant makes is
//! held to here too: a forged or cross-site approval, a replayed or expired
//! code, a verifier that does not match, a redirect the client did not
//! register, and a revoked connection.

mod support;

use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use cratefield_core::axum::body::Bytes;
use cratefield_core::axum::http::{Request, Response, StatusCode, header};
use cratefield_core::{Decision, HttpClient, HttpError, MapConfig, Module, Statement};
use cratefield_testing::FakeRateLimiter;
use livingbrain_tokens::{
    Authenticated, OAuth, authenticate, authenticate_actions, authenticate_mcp,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use support::{
    API_BASE, Caller, Kit, OWNER, WORKSPACE, get, get_at, post_form, post_json, prefix_of,
};

const REDIRECT: &str = "https://claude.example/api/mcp/auth_callback";
/// A redirect on a host Living Brain recognises as Claude.
const VERIFIED_REDIRECT: &str = "https://claude.ai/api/mcp/auth_callback";
const CLIENT_NAME: &str = "Claude test";
/// RFC 7636 Appendix B: the one published verifier and its S256 challenge.
const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
const VERIFIED_VERIFIER: &str = VERIFIER;
const OTHER_VERIFIER: &str = "xBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";

/// RFC 3986 unreserved-only encoding, for the values a test puts in a form.
fn enc(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                char::from(b).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

/// The value of `name` in a query string or a form body, decoded for the
/// characters the grant puts in it.
fn param(query: &str, name: &str) -> Option<String> {
    query.split(['&', '?']).find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then(|| {
            value
                .replace("%20", " ")
                .replace("%2F", "/")
                .replace("%3A", ":")
        })
    })
}

fn sha256_hex(value: &str) -> String {
    Sha256::digest(value.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The sealed request the consent page carries, read out of its HTML.
fn consent_request(page: &str) -> String {
    let marker = "name=\"request\" value=\"";
    let start = page
        .find(marker)
        .expect("the consent form carries the request")
        + marker.len();
    let end = page[start..].find('"').expect("a closed attribute") + start;
    page[start..end].to_owned()
}

async fn register(kit: &Kit) -> String {
    let made = post_json(
        kit,
        "/v1/oauth/register",
        &json!({ "client_name": CLIENT_NAME, "redirect_uris": [REDIRECT] }),
        &Caller::Anonymous,
    )
    .await;
    assert_eq!(made.status, StatusCode::CREATED, "{}", made.text());
    made.json()["client_id"]
        .as_str()
        .expect("a client id")
        .to_owned()
}

fn authorize_path(client_id: &str, challenge: &str, state: &str) -> String {
    format!(
        "/v1/oauth/authorize?response_type=code&client_id={client_id}&redirect_uri={}\
         &code_challenge={challenge}&code_challenge_method=S256&state={state}",
        enc(REDIRECT)
    )
}

/// The consent: the page a signed-in person is shown, approved with a chosen
/// workspace. Returns the code the redirect carries.
async fn approve(kit: &Kit, client_id: &str, who: &Caller) -> String {
    approve_to(kit, client_id, REDIRECT, who).await
}

/// The same approval, sent back to a chosen redirect URI.
async fn approve_to(kit: &Kit, client_id: &str, redirect: &str, who: &Caller) -> String {
    let path = format!(
        "/v1/oauth/authorize?response_type=code&client_id={client_id}&redirect_uri={}\
         &code_challenge={CHALLENGE}&code_challenge_method=S256&state=s1",
        enc(redirect)
    );
    let shown = get(kit, &path, who).await;
    assert_eq!(shown.status, StatusCode::OK, "{}", shown.text());
    let request = consent_request(&shown.text());
    let form = format!(
        "request={}&decision=approve&workspace_id={WORKSPACE}",
        enc(&request)
    );
    let approved = post_form(kit, "/v1/oauth/authorize", &form, who).await;
    assert_eq!(
        approved.status,
        StatusCode::SEE_OTHER,
        "{}",
        approved.text()
    );
    let location = approved.location().expect("the approval redirects");
    assert!(location.starts_with(redirect), "{location}");
    assert_eq!(
        param(&location, "state").as_deref(),
        Some("s1"),
        "{location}"
    );
    param(&location, "code").expect("the redirect carries the code")
}

fn token_form(code: &str, client_id: &str, verifier: &str) -> String {
    format!(
        "grant_type=authorization_code&code={code}&redirect_uri={}&client_id={client_id}\
         &code_verifier={verifier}",
        enc(REDIRECT)
    )
}

async fn redeem(kit: &Kit, code: &str, client_id: &str, verifier: &str) -> support::Res {
    post_form(
        kit,
        "/v1/oauth/token",
        &token_form(code, client_id, verifier),
        &Caller::Anonymous,
    )
    .await
}

/// Minimal N1/minor-a helper: a token exchange with a chosen redirect.
async fn redeem_at(kit: &Kit, code: &str, client_id: &str, redirect: &str) -> support::Res {
    post_form(
        kit,
        "/v1/oauth/token",
        &format!(
            "grant_type=authorization_code&code={code}&redirect_uri={}&client_id={client_id}&code_verifier={VERIFIED_VERIFIER}",
            enc(redirect)
        ),
        &Caller::Anonymous,
    )
    .await
}

/// A full connection: registered, approved and redeemed. Returns the access
/// token, the refresh token and the client id.
async fn connected(kit: &Kit) -> (String, String, String) {
    let client_id = register(kit).await;
    let code = approve(kit, &client_id, &kit.owner()).await;
    let granted = redeem(kit, &code, &client_id, VERIFIER).await;
    assert_eq!(granted.status, StatusCode::OK, "{}", granted.text());
    let body = granted.json();
    (
        body["access_token"]
            .as_str()
            .expect("an access token")
            .to_owned(),
        body["refresh_token"]
            .as_str()
            .expect("a refresh token")
            .to_owned(),
        client_id,
    )
}

fn error_of(response: &support::Res) -> String {
    response.json()["error"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

// ---------------------------------------------------------------------------
// Registration (RFC 7591)

#[pollster::test]
async fn registration_accepts_https_and_loopback_and_refuses_the_rest() {
    let kit = support::signed_in().await;

    let made = post_json(
        &kit,
        "/v1/oauth/register",
        &json!({ "client_name": CLIENT_NAME, "redirect_uris": [REDIRECT] }),
        &Caller::Anonymous,
    )
    .await;
    assert_eq!(made.status, StatusCode::CREATED, "{}", made.text());
    let body = made.json();
    assert!(body["client_id"].as_str().unwrap().starts_with("lbc_"));
    assert!(body["client_id_issued_at"].as_i64().unwrap() > 0);
    assert_eq!(body["redirect_uris"], json!([REDIRECT]));
    assert_eq!(body["token_endpoint_auth_method"], json!("none"));
    assert_eq!(body["response_types"], json!(["code"]));

    let loopback = post_json(
        &kit,
        "/v1/oauth/register",
        &json!({ "client_name": "Local", "redirect_uris": ["http://localhost:33418/cb"] }),
        &Caller::Anonymous,
    )
    .await;
    assert_eq!(loopback.status, StatusCode::CREATED, "{}", loopback.text());

    // L1: the authority is a plain host, ASCII, and nothing that could read
    // as a second host, a port that is not a port, or a userinfo.
    for uri in [
        "http://claude.example/cb",
        "https://claude.example/cb#fragment",
        "javascript:alert(1)",
        "myapp://callback",
        "https://claude.ai;evil.example/cb",
        "https://claüde.ai/cb",
        "https://claude.ai:99999999/cb",
        "https://user@claude.ai/cb",
        "https://claude.ai\u{0}/cb",
    ] {
        let refused = post_json(
            &kit,
            "/v1/oauth/register",
            &json!({ "redirect_uris": [uri] }),
            &Caller::Anonymous,
        )
        .await;
        assert_eq!(
            refused.status,
            StatusCode::BAD_REQUEST,
            "{uri}: {}",
            refused.text()
        );
        assert_eq!(error_of(&refused), "invalid_redirect_uri", "{uri}");
    }

    let secret = post_json(
        &kit,
        "/v1/oauth/register",
        &json!({ "redirect_uris": [REDIRECT], "token_endpoint_auth_method": "client_secret_basic" }),
        &Caller::Anonymous,
    )
    .await;
    assert_eq!(error_of(&secret), "invalid_client_metadata");

    let grant = post_json(
        &kit,
        "/v1/oauth/register",
        &json!({ "redirect_uris": [REDIRECT], "grant_types": ["client_credentials"] }),
        &Caller::Anonymous,
    )
    .await;
    assert_eq!(error_of(&grant), "invalid_client_metadata");
}

/// M2: without a rate limiter bound, registration refuses rather than running
/// open. Everything else in the grant still works without one.
#[pollster::test]
async fn registration_fails_closed_without_a_rate_limiter() {
    let kit = support::signed_in_without_limiter().await;
    let refused = post_json(
        &kit,
        "/v1/oauth/register",
        &json!({ "redirect_uris": [REDIRECT] }),
        &Caller::Anonymous,
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        refused.text()
    );
    assert_eq!(error_of(&refused), "temporarily_unavailable");
}

/// Distinct addresses for a crowd of registrations, so the per-address limit
/// does not stand in the way of the daily cap under test.
fn address(n: usize) -> String {
    format!("10.1.{}.{}", n / 256, n % 256)
}

/// M2 (cap): the day's allowance counts unverified apps. Once it is spent, a
/// further unverified app is refused, whoever asks.
#[pollster::test]
async fn registration_stops_at_the_daily_cap_for_unverified_apps() {
    let kit = support::signed_in().await;
    for n in 0..200 {
        let made = support::post_json_from(
            &kit,
            "/v1/oauth/register",
            &json!({ "client_name": format!("app {n}"), "redirect_uris": [REDIRECT] }),
            &address(n),
        )
        .await;
        assert_eq!(
            made.status,
            StatusCode::CREATED,
            "registration {n}: {}",
            made.text()
        );
    }
    let over = support::post_json_from(
        &kit,
        "/v1/oauth/register",
        &json!({ "client_name": "one more", "redirect_uris": [REDIRECT] }),
        &address(999),
    )
    .await;
    assert_eq!(
        over.status,
        StatusCode::TOO_MANY_REQUESTS,
        "{}",
        over.text()
    );
}

/// M2 (cap): a Claude host does not consume the unverified allowance, so a
/// crowd of Claude registrations cannot close registration for everyone else.
#[pollster::test]
async fn verified_registrations_do_not_use_the_unverified_allowance() {
    let kit = support::signed_in().await;
    for n in 0..200 {
        let made = support::post_json_from(
            &kit,
            "/v1/oauth/register",
            &json!({ "client_name": format!("claude {n}"), "redirect_uris": [VERIFIED_REDIRECT] }),
            &address(n),
        )
        .await;
        assert_eq!(
            made.status,
            StatusCode::CREATED,
            "verified {n}: {}",
            made.text()
        );
    }
    let other = support::post_json_from(
        &kit,
        "/v1/oauth/register",
        &json!({ "client_name": "someone else", "redirect_uris": [REDIRECT] }),
        &address(999),
    )
    .await;
    assert_eq!(other.status, StatusCode::CREATED, "{}", other.text());
}

/// M2 (per address): one address may create ten apps an hour. Its eleventh is
/// refused, while another address is unaffected.
#[pollster::test]
async fn one_address_cannot_block_the_others() {
    let kit = support::signed_in().await;
    for n in 0..10 {
        let made = support::post_json_from(
            &kit,
            "/v1/oauth/register",
            &json!({ "client_name": format!("mine {n}"), "redirect_uris": [REDIRECT] }),
            "203.0.113.7",
        )
        .await;
        assert_eq!(made.status, StatusCode::CREATED, "{}", made.text());
    }
    let eleventh = support::post_json_from(
        &kit,
        "/v1/oauth/register",
        &json!({ "client_name": "mine 10", "redirect_uris": [REDIRECT] }),
        "203.0.113.7",
    )
    .await;
    assert_eq!(
        eleventh.status,
        StatusCode::TOO_MANY_REQUESTS,
        "{}",
        eleventh.text()
    );

    let elsewhere = support::post_json_from(
        &kit,
        "/v1/oauth/register",
        &json!({ "client_name": "theirs", "redirect_uris": [REDIRECT] }),
        "198.51.100.9",
    )
    .await;
    assert_eq!(
        elsewhere.status,
        StatusCode::CREATED,
        "{}",
        elsewhere.text()
    );
}

/// Re-registering the same app returns its existing identity: the same
/// client_id, no new row, and the redirect set compared as a set.
#[pollster::test]
async fn the_same_app_registered_again_keeps_its_identity() {
    let kit = support::signed_in().await;
    let first = post_json(
        &kit,
        "/v1/oauth/register",
        &json!({ "client_name": CLIENT_NAME, "redirect_uris": [REDIRECT, "http://localhost:4000/cb"] }),
        &Caller::Anonymous,
    )
    .await;
    let again = post_json(
        &kit,
        "/v1/oauth/register",
        &json!({ "client_name": CLIENT_NAME, "redirect_uris": ["http://localhost:4000/cb", REDIRECT] }),
        &Caller::Anonymous,
    )
    .await;
    assert_eq!(again.status, StatusCode::CREATED, "{}", again.text());
    assert_eq!(first.json()["client_id"], again.json()["client_id"]);
    assert_eq!(rows(&kit, "oauth_clients").await, 1, "no second row");

    // A different redirect set is a different app.
    let other = post_json(
        &kit,
        "/v1/oauth/register",
        &json!({ "client_name": CLIENT_NAME, "redirect_uris": [REDIRECT] }),
        &Caller::Anonymous,
    )
    .await;
    assert_ne!(first.json()["client_id"], other.json()["client_id"]);
}

/// M1: control and bidirectional-override characters never reach a stored name.
#[pollster::test]
async fn a_client_name_is_cleaned_of_control_and_override_characters() {
    let kit = support::signed_in().await;
    let made = post_json(
        &kit,
        "/v1/oauth/register",
        &json!({ "client_name": "\u{202E}enilnoM\u{202C}\u{200B}Claude\u{0007}", "redirect_uris": [REDIRECT] }),
        &Caller::Anonymous,
    )
    .await;
    assert_eq!(made.status, StatusCode::CREATED, "{}", made.text());
    assert_eq!(made.json()["client_name"], json!("enilnoMClaude"));
}

// ---------------------------------------------------------------------------
// Authorize

#[pollster::test]
async fn authorize_without_a_session_sends_the_person_to_sign_in_and_back() {
    let kit = support::signed_in().await;
    let client_id = register(&kit).await;
    let path = authorize_path(&client_id, CHALLENGE, "s1");

    let signed_out = get(&kit, &path, &Caller::Anonymous).await;
    assert_eq!(
        signed_out.status,
        StatusCode::FOUND,
        "{}",
        signed_out.text()
    );
    let location = signed_out.location().expect("a sign-in location");
    assert!(location.starts_with("/index.html?return_to="), "{location}");
    let return_to = location.trim_start_matches("/index.html?return_to=");
    assert!(
        return_to.starts_with("%2Fv1%2Foauth%2Fauthorize%3F"),
        "the return target is this exact request: {location}"
    );
}

#[pollster::test]
async fn authorize_shows_the_consent_to_a_signed_in_person() {
    let kit = support::signed_in().await;
    let client_id = register(&kit).await;
    let shown = get(
        &kit,
        &authorize_path(&client_id, CHALLENGE, "s1"),
        &kit.owner(),
    )
    .await;
    assert_eq!(shown.status, StatusCode::OK, "{}", shown.text());
    let page = shown.text();
    assert!(page.contains(CLIENT_NAME), "the client is named");
    assert!(page.contains("Approve") && page.contains("Deny"));
    assert!(page.contains("action=\"/v1/oauth/authorize\""));
    // The grant as it really is: reads, and notes to one's own memory.
    assert!(
        page.contains("add notes to your own private memory"),
        "{page}"
    );
    assert!(page.contains("cannot change settings"), "{page}");
    let csp = shown
        .headers
        .get("content-security-policy")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        csp.contains("default-src 'none'") && csp.contains("frame-ancestors 'none'"),
        "{csp}"
    );
    assert!(
        csp.contains("form-action 'self' https://claude.example"),
        "{csp}"
    );
    assert!(!csp.contains("script-src"), "the consent runs no script");
    // L5: the consent is never sent a referrer.
    assert_eq!(
        shown
            .headers
            .get("referrer-policy")
            .and_then(|v| v.to_str().ok()),
        Some("no-referrer")
    );
    // M1: an unverified host is shown in full, under a warning.
    assert!(page.contains("claude.example"), "the return host is shown");
    assert!(page.contains("Unverified app"), "{page}");
}

#[pollster::test]
async fn a_verified_connector_host_shows_no_unverified_warning() {
    let kit = support::signed_in().await;
    // Both platforms Living Brain vouches for: Claude's connector redirect
    // and ChatGPT's (the platform one and a Custom GPT's Actions callback).
    let verified = [
        VERIFIED_REDIRECT,
        "https://chatgpt.com/connector_platform_oauth_redirect",
        "https://chat.openai.com/aip/g-docapp/oauth/callback",
    ];
    let made = post_json(
        &kit,
        "/v1/oauth/register",
        &json!({ "client_name": "Claude", "redirect_uris": verified }),
        &Caller::Anonymous,
    )
    .await;
    let client_id = made.json()["client_id"].as_str().unwrap().to_owned();
    for redirect in verified {
        let shown = get(
            &kit,
            &format!(
                "/v1/oauth/authorize?response_type=code&client_id={client_id}&redirect_uri={}\
                 &code_challenge={CHALLENGE}&code_challenge_method=S256&state=s1",
                enc(redirect)
            ),
            &kit.owner(),
        )
        .await;
        assert_eq!(shown.status, StatusCode::OK, "{redirect}: {}", shown.text());
        assert!(
            !shown.text().contains("Unverified app"),
            "{redirect}: {}",
            shown.text()
        );
    }
}

#[pollster::test]
async fn an_unregistered_client_or_redirect_gets_a_page_never_a_redirect() {
    let kit = support::signed_in().await;
    let client_id = register(&kit).await;

    let unknown = get(
        &kit,
        &authorize_path("lbc_nobody", CHALLENGE, "s1"),
        &kit.owner(),
    )
    .await;
    assert_eq!(
        unknown.status,
        StatusCode::BAD_REQUEST,
        "{}",
        unknown.text()
    );
    assert!(
        unknown.location().is_none(),
        "an unknown client is never redirected"
    );

    let elsewhere = format!(
        "/v1/oauth/authorize?response_type=code&client_id={client_id}&redirect_uri={}\
         &code_challenge={CHALLENGE}&code_challenge_method=S256",
        enc("https://attacker.example/steal")
    );
    let mismatch = get(&kit, &elsewhere, &kit.owner()).await;
    assert_eq!(
        mismatch.status,
        StatusCode::BAD_REQUEST,
        "{}",
        mismatch.text()
    );
    assert!(
        mismatch.location().is_none(),
        "an unregistered redirect is never followed"
    );
}

#[pollster::test]
async fn a_bad_request_is_sent_back_with_an_error_and_its_issuer() {
    let kit = support::signed_in().await;
    let client_id = register(&kit).await;
    let base = format!(
        "/v1/oauth/authorize?client_id={client_id}&redirect_uri={}",
        enc(REDIRECT)
    );
    let cases = [
        (
            format!(
                "{base}&response_type=token&code_challenge={CHALLENGE}&code_challenge_method=S256&state=s1"
            ),
            "unsupported_response_type",
        ),
        (
            format!("{base}&response_type=code&state=s1"),
            "invalid_request",
        ),
        (
            format!(
                "{base}&response_type=code&code_challenge={CHALLENGE}&code_challenge_method=plain&state=s1"
            ),
            "invalid_request",
        ),
        (
            format!(
                "{base}&response_type=code&code_challenge={CHALLENGE}&code_challenge_method=S256&resource=https%3A%2F%2Fevil.example%2Fx&state=s1"
            ),
            "invalid_target",
        ),
    ];
    for (path, expected) in cases {
        let refused = get(&kit, &path, &kit.owner()).await;
        assert_eq!(
            refused.status,
            StatusCode::FOUND,
            "{expected}: {}",
            refused.text()
        );
        let location = refused.location().expect("a redirect back");
        assert!(location.starts_with(REDIRECT), "{location}");
        assert_eq!(
            param(&location, "error").as_deref(),
            Some(expected),
            "{location}"
        );
        assert_eq!(
            param(&location, "state").as_deref(),
            Some("s1"),
            "{location}"
        );
        assert_eq!(
            param(&location, "iss").as_deref(),
            Some(API_BASE),
            "RFC 9207: {location}"
        );
    }
}

/// L3 across the host hop: a request that arrives on the MCP host is carried to
/// the app origin with a signed issuer, and the response names the MCP host, the
/// issuer that host's metadata advertised, not the app's.
#[pollster::test]
async fn the_issuer_is_the_host_the_request_arrived_on_across_the_sign_in_hop() {
    let kit = support::signed_in().await;
    let client_id = register(&kit).await;
    let path = authorize_path(&client_id, CHALLENGE, "s1");
    let moved = get_at(&kit, &path, "mcp.livingbrain.wiki", None).await;
    assert_eq!(moved.status, StatusCode::FOUND, "{}", moved.text());
    let carried = moved.location().expect("a hop to the app origin");
    assert!(
        carried.starts_with(&format!("{API_BASE}{path}&issuer_ref=")),
        "{carried}"
    );
    let on_app = carried.trim_start_matches(API_BASE);

    let shown = get(&kit, on_app, &kit.owner()).await;
    assert_eq!(shown.status, StatusCode::OK, "{}", shown.text());
    let request = enc(&consent_request(&shown.text()));
    let approved = post_form(
        &kit,
        "/v1/oauth/authorize",
        &format!("request={request}&decision=approve&workspace_id={WORKSPACE}"),
        &kit.owner(),
    )
    .await;
    let location = approved.location().expect("a redirect");
    assert_eq!(
        param(&location, "iss").as_deref(),
        Some("https://mcp.livingbrain.wiki"),
        "{location}"
    );
}

// ---------------------------------------------------------------------------
// The approval: CSRF and the person it was shown to

#[pollster::test]
async fn an_approval_must_be_genuine_same_origin_and_for_the_person_shown() {
    let kit = support::signed_in().await;
    let client_id = register(&kit).await;
    let shown = get(
        &kit,
        &authorize_path(&client_id, CHALLENGE, "s1"),
        &kit.owner(),
    )
    .await;
    let request = enc(&consent_request(&shown.text()));
    let form =
        |request: &str| format!("request={request}&decision=approve&workspace_id={WORKSPACE}");

    // No form token, a forged one, and an edited one are all refused.
    for body in [
        format!("decision=approve&workspace_id={WORKSPACE}"),
        form("forged.token"),
        form(&format!("{request}x")),
    ] {
        let refused = post_form(&kit, "/v1/oauth/authorize", &body, &kit.owner()).await;
        assert_eq!(
            refused.status,
            StatusCode::FORBIDDEN,
            "{body}: {}",
            refused.text()
        );
        assert!(refused.location().is_none());
    }

    // Another site's page posting it: refused by the browser's own signal.
    let cross = post_form(
        &kit,
        "/v1/oauth/authorize",
        &form(&request),
        &kit.owner_elsewhere(),
    )
    .await;
    assert_eq!(cross.status, StatusCode::FORBIDDEN, "{}", cross.text());
    assert!(
        cross.text().contains("oauth/cross-site-request"),
        "{}",
        cross.text()
    );

    // Somebody else's signed-in browser holding the form: refused.
    let other = post_form(&kit, "/v1/oauth/authorize", &form(&request), &kit.other()).await;
    assert_eq!(other.status, StatusCode::FORBIDDEN, "{}", other.text());

    // A workspace the person does not hold.
    let not_held = post_form(
        &kit,
        "/v1/oauth/authorize",
        &format!("request={request}&decision=approve&workspace_id=T0NOTMINE"),
        &kit.owner(),
    )
    .await;
    assert_eq!(
        not_held.status,
        StatusCode::FORBIDDEN,
        "{}",
        not_held.text()
    );
    assert!(not_held.text().contains("oauth/workspace-not-held"));

    // Nothing was approved by any of that: no code was minted.
    assert_eq!(
        rows(&kit, "oauth_codes").await,
        0,
        "a refused approval mints no code"
    );
}

#[pollster::test]
async fn deny_redirects_with_access_denied_and_no_code() {
    let kit = support::signed_in().await;
    let client_id = register(&kit).await;
    let shown = get(
        &kit,
        &authorize_path(&client_id, CHALLENGE, "s1"),
        &kit.owner(),
    )
    .await;
    let request = enc(&consent_request(&shown.text()));
    let denied = post_form(
        &kit,
        "/v1/oauth/authorize",
        &format!("request={request}&decision=deny"),
        &kit.owner(),
    )
    .await;
    assert_eq!(denied.status, StatusCode::SEE_OTHER, "{}", denied.text());
    let location = denied.location().expect("a redirect");
    assert_eq!(
        param(&location, "error").as_deref(),
        Some("access_denied"),
        "{location}"
    );
    assert_eq!(param(&location, "state").as_deref(), Some("s1"));
    assert_eq!(
        param(&location, "iss").as_deref(),
        Some(API_BASE),
        "{location}"
    );
    assert!(param(&location, "code").is_none(), "{location}");
    assert_eq!(
        denied
            .headers
            .get("referrer-policy")
            .and_then(|v| v.to_str().ok()),
        Some("no-referrer")
    );
}

// ---------------------------------------------------------------------------
// The token endpoint

#[pollster::test]
async fn a_code_redeems_once_for_a_connector_token_and_a_refresh_token() {
    let kit = support::signed_in().await;
    let (access, refresh, _) = connected(&kit).await;
    assert!(
        access.starts_with("lbp_"),
        "a personal access token: {access}"
    );
    assert!(refresh.starts_with("lbr_"), "{refresh}");

    // The MCP endpoint's resolver takes it, as the approved member, and it is
    // not an admin: its scope subset is what it reaches.
    let headers = Caller::Token(access.clone()).headers();
    let Ok(Authenticated::Member(member)) = authenticate_mcp(&kit.ports, &headers).await else {
        panic!("the MCP endpoint accepts a connector token");
    };
    assert_eq!(member.user_id, OWNER);
    assert_eq!(member.workspace_id, WORKSPACE);
    assert!(!member.is_admin, "a connector token is never an admin");
    assert_eq!(
        member.scopes,
        Some(vec!["shared".to_owned(), format!("user:{OWNER}")])
    );

    // It is listed under the person's API tokens, labelled as a connector.
    let listed = get(&kit, "/v1/tokens", &kit.owner()).await;
    let tokens = listed.json()["tokens"].as_array().unwrap().clone();
    let ours = tokens
        .iter()
        .find(|token| token["prefix"] == json!(prefix_of(&access)))
        .expect("the connection is listed");
    assert_eq!(ours["connector"], json!(true));
    assert_eq!(
        ours["name"],
        json!(CLIENT_NAME),
        "an unverified app keeps only its own name"
    );
}

/// H1: a connector token is accepted by the MCP endpoint's resolver alone. Every
/// other surface answers it as an unknown token.
#[pollster::test]
async fn a_connector_token_reaches_nothing_but_the_mcp_endpoint() {
    let kit = support::signed_in().await;
    let (access, _, _) = connected(&kit).await;
    let headers = Caller::Token(access.clone()).headers();

    assert!(
        authenticate(&kit.ports, &headers).await.is_err(),
        "the general resolver refuses it"
    );
    assert_eq!(
        get(&kit, "/v1/me", &Caller::Token(access.clone()))
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        post_json(
            &kit,
            "/v1/tokens",
            &json!({ "name": "escalate" }),
            &Caller::Token(access.clone())
        )
        .await
        .status,
        StatusCode::UNAUTHORIZED,
        "a connector cannot mint a token"
    );
    assert_eq!(
        support::delete(
            &kit,
            &format!("/v1/tokens/{}", prefix_of(&access)),
            &Caller::Token(access.clone())
        )
        .await
        .status,
        StatusCode::UNAUTHORIZED,
        "a connector cannot revoke a token"
    );
}

#[pollster::test]
async fn the_token_response_has_the_rfc_6749_fields_and_is_never_cached() {
    let kit = support::signed_in().await;
    let client_id = register(&kit).await;
    let code = approve(&kit, &client_id, &kit.owner()).await;
    let granted = redeem(&kit, &code, &client_id, VERIFIER).await;
    assert_eq!(granted.status, StatusCode::OK);
    assert_eq!(
        granted
            .headers
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("no-store")
    );
    let body = granted.json();
    assert_eq!(body["token_type"], json!("Bearer"));
    assert_eq!(body["expires_in"], json!(3600));
}

/// L2: a requested scope is never echoed or stored; the response names the
/// scopes the grant actually issued.
#[pollster::test]
async fn the_scope_in_the_response_is_the_one_granted_not_the_one_requested() {
    let kit = support::signed_in().await;
    let client_id = register(&kit).await;
    let path = format!(
        "{}&scope=admin%3Aeverything",
        authorize_path(&client_id, CHALLENGE, "s1")
    );
    let shown = get(&kit, &path, &kit.owner()).await;
    let request = enc(&consent_request(&shown.text()));
    let approved = post_form(
        &kit,
        "/v1/oauth/authorize",
        &format!("request={request}&decision=approve&workspace_id={WORKSPACE}"),
        &kit.owner(),
    )
    .await;
    let code = param(&approved.location().unwrap(), "code").unwrap();
    let granted = redeem(&kit, &code, &client_id, VERIFIER).await;
    assert_eq!(granted.status, StatusCode::OK, "{}", granted.text());
    assert_eq!(
        granted.json()["scope"],
        json!(format!("shared user:{OWNER}")),
        "{}",
        granted.text()
    );
}

#[pollster::test]
async fn a_wrong_verifier_a_mismatched_redirect_and_an_unknown_client_all_fail() {
    let kit = support::signed_in().await;
    let client_id = register(&kit).await;

    let code = approve(&kit, &client_id, &kit.owner()).await;
    let wrong = redeem(&kit, &code, &client_id, OTHER_VERIFIER).await;
    assert_eq!(wrong.status, StatusCode::BAD_REQUEST, "{}", wrong.text());
    assert_eq!(error_of(&wrong), "invalid_grant");
    // The code was spent by that guess, so the right verifier now fails too.
    let retry = redeem(&kit, &code, &client_id, VERIFIER).await;
    assert_eq!(
        error_of(&retry),
        "invalid_grant",
        "a spent code stays spent"
    );

    let code = approve(&kit, &client_id, &kit.owner()).await;
    let elsewhere = post_form(
        &kit,
        "/v1/oauth/token",
        &format!(
            "grant_type=authorization_code&code={code}&redirect_uri={}&client_id={client_id}&code_verifier={VERIFIER}",
            enc("https://claude.example/other")
        ),
        &Caller::Anonymous,
    )
    .await;
    assert_eq!(
        error_of(&elsewhere),
        "invalid_grant",
        "{}",
        elsewhere.text()
    );

    let code = approve(&kit, &client_id, &kit.owner()).await;
    let unknown = redeem(&kit, &code, "lbc_unknown", VERIFIER).await;
    assert_eq!(
        unknown.status,
        StatusCode::UNAUTHORIZED,
        "{}",
        unknown.text()
    );
    assert_eq!(error_of(&unknown), "invalid_client");

    let bogus = redeem(&kit, "not-a-code", &client_id, VERIFIER).await;
    assert_eq!(error_of(&bogus), "invalid_grant");
}

#[pollster::test]
async fn a_replayed_code_is_refused_and_ends_the_connection_it_opened() {
    let kit = support::signed_in().await;
    let client_id = register(&kit).await;
    let code = approve(&kit, &client_id, &kit.owner()).await;
    let first = redeem(&kit, &code, &client_id, VERIFIER).await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.text());
    let access = first.json()["access_token"].as_str().unwrap().to_owned();

    let replay = redeem(&kit, &code, &client_id, VERIFIER).await;
    assert_eq!(error_of(&replay), "invalid_grant");
    let dead = get(&kit, "/v1/me", &Caller::Token(access.clone())).await;
    assert_eq!(
        dead.status,
        StatusCode::UNAUTHORIZED,
        "the replayed code's token is revoked"
    );
    assert!(
        authenticate_mcp(&kit.ports, &Caller::Token(access).headers())
            .await
            .is_err(),
        "and the MCP endpoint refuses it too"
    );
}

#[pollster::test]
async fn an_expired_code_is_refused() {
    let kit = support::signed_in().await;
    let client_id = register(&kit).await;
    let code = "expiredcodeexpiredcodeexpiredcodeexpired00";
    seed_code(&kit, code, &client_id, "2026-01-01T00:00:00Z").await;
    let expired = redeem(&kit, code, &client_id, VERIFIER).await;
    assert_eq!(error_of(&expired), "invalid_grant", "{}", expired.text());
}

async fn seed_code(kit: &Kit, code: &str, client_id: &str, expires_at: &str) {
    let statement = Statement::with_values(
        "INSERT INTO oauth_codes (code_hash, client_id, redirect_uri, code_challenge, \
             workspace_id, user_id, scope, family, expires_at, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, '', 'family-seeded', ?, ?)",
        vec![
            support::text(&sha256_hex(code)),
            support::text(client_id),
            support::text(REDIRECT),
            support::text(CHALLENGE),
            support::text(WORKSPACE),
            support::text(OWNER),
            support::text(expires_at),
            support::text("2026-01-01T00:00:00Z"),
        ],
    );
    kit.harness
        .db
        .execute(&statement)
        .await
        .expect("the code row seeds");
}

// ---------------------------------------------------------------------------
// Refresh, rotation, idle connections and revocation

#[pollster::test]
async fn a_refresh_rotates_and_a_replayed_spent_token_ends_the_connection() {
    let kit = support::signed_in().await;
    let (access, refresh, client_id) = connected(&kit).await;

    let form = format!("grant_type=refresh_token&refresh_token={refresh}&client_id={client_id}");
    let rotated = post_form(&kit, "/v1/oauth/token", &form, &Caller::Anonymous).await;
    assert_eq!(rotated.status, StatusCode::OK, "{}", rotated.text());
    let body = rotated.json();
    let next_access = body["access_token"].as_str().unwrap().to_owned();
    let next_refresh = body["refresh_token"].as_str().unwrap().to_owned();
    assert_ne!(next_access, access);
    assert_ne!(next_refresh, refresh);
    assert_eq!(body["scope"], json!(format!("shared user:{OWNER}")));

    // The new access token works; the one it replaced does not.
    assert_eq!(
        get(&kit, "/v1/me", &Caller::Token(next_access.clone()))
            .await
            .status,
        StatusCode::UNAUTHORIZED,
        "connectors are not /v1/me"
    );
    assert!(
        authenticate_mcp(&kit.ports, &Caller::Token(next_access.clone()).headers())
            .await
            .is_ok()
    );
    assert_eq!(
        get(&kit, "/v1/me", &Caller::Token(access)).await.status,
        StatusCode::UNAUTHORIZED
    );

    // The spent refresh token is refused. Presenting it again is a replay: a
    // copy is loose, so the whole connection ends, the rotated token with it.
    let spent = post_form(&kit, "/v1/oauth/token", &form, &Caller::Anonymous).await;
    assert_eq!(error_of(&spent), "invalid_grant");
    let replay_form =
        format!("grant_type=refresh_token&refresh_token={next_refresh}&client_id={client_id}");
    let ended = post_form(&kit, "/v1/oauth/token", &replay_form, &Caller::Anonymous).await;
    assert_eq!(error_of(&ended), "invalid_grant", "the connection ended");
    assert!(
        authenticate_mcp(&kit.ports, &Caller::Token(next_access).headers())
            .await
            .is_err(),
        "the rotated access token ended with its connection"
    );
}

/// H2: an idle connection (its access token past expiry, its renewal live) is
/// still listed, so it can be seen and revoked; revoking it ends the renewal.
#[pollster::test]
async fn an_idle_connection_stays_listed_until_revoked_and_revoking_ends_renewal() {
    let kit = support::signed_in().await;
    let (access, refresh, client_id) = connected(&kit).await;

    // An hour and more later: the access token is past its expiry.
    let expire = Statement::with_values(
        "UPDATE personal_access_tokens SET expires_at = ? WHERE prefix = ?",
        vec![
            support::text("2026-01-01T00:00:00Z"),
            support::text(prefix_of(&access)),
        ],
    );
    kit.harness
        .db
        .execute(&expire)
        .await
        .expect("the expiry seeds");

    let listed = get(&kit, "/v1/tokens", &kit.owner()).await;
    let listed_prefixes: Vec<String> = listed.json()["tokens"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["prefix"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(
        listed_prefixes.contains(&prefix_of(&access).to_owned()),
        "an idle connection with a live renewal is still listed: {listed_prefixes:?}"
    );

    // Renewing it still works, which is why it must be shown.
    let renewed = post_form(
        &kit,
        "/v1/oauth/token",
        &format!("grant_type=refresh_token&refresh_token={refresh}&client_id={client_id}"),
        &Caller::Anonymous,
    )
    .await;
    assert_eq!(renewed.status, StatusCode::OK, "{}", renewed.text());
    let renewed_access = renewed.json()["access_token"].as_str().unwrap().to_owned();

    // Revoke it from Settings: the whole connection goes.
    let revoked = support::delete(
        &kit,
        &format!("/v1/tokens/{}", prefix_of(&renewed_access)),
        &kit.owner(),
    )
    .await;
    assert_eq!(revoked.status, StatusCode::NO_CONTENT, "{}", revoked.text());
    let form = format!(
        "grant_type=refresh_token&refresh_token={}&client_id={client_id}",
        renewed.json()["refresh_token"].as_str().unwrap()
    );
    let refused = post_form(&kit, "/v1/oauth/token", &form, &Caller::Anonymous).await;
    assert_eq!(
        error_of(&refused),
        "invalid_grant",
        "a revoked connection cannot be renewed"
    );
    let after = get(&kit, "/v1/tokens", &kit.owner()).await;
    assert!(
        !after.text().contains(prefix_of(&renewed_access)),
        "and it is gone from the list"
    );
}

/// H2: a connection whose renewal has run out is hidden, as an expired token is.
#[pollster::test]
async fn a_connection_past_its_renewal_is_no_longer_listed() {
    let kit = support::signed_in().await;
    let (access, refresh, _) = connected(&kit).await;
    let expire = Statement::with_values(
        "UPDATE personal_access_tokens SET expires_at = ? WHERE prefix = ?",
        vec![
            support::text("2026-01-01T00:00:00Z"),
            support::text(prefix_of(&access)),
        ],
    );
    kit.harness
        .db
        .execute(&expire)
        .await
        .expect("the expiry seeds");
    let stale = Statement::with_values(
        "UPDATE oauth_refresh_tokens SET expires_at = ? WHERE token_hash = ?",
        vec![
            support::text("2026-01-01T00:00:00Z"),
            support::text(&sha256_hex(&refresh)),
        ],
    );
    kit.harness
        .db
        .execute(&stale)
        .await
        .expect("the renewal expiry seeds");
    let listed = get(&kit, "/v1/tokens", &kit.owner()).await;
    assert!(
        !listed.text().contains(prefix_of(&access)),
        "{}",
        listed.text()
    );
}

#[pollster::test]
async fn revoking_the_connection_in_settings_ends_its_access_and_its_refresh() {
    let kit = support::signed_in().await;
    let (access, refresh, client_id) = connected(&kit).await;

    let revoked = support::delete(
        &kit,
        &format!("/v1/tokens/{}", prefix_of(&access)),
        &kit.owner(),
    )
    .await;
    assert_eq!(revoked.status, StatusCode::NO_CONTENT, "{}", revoked.text());

    assert_eq!(
        get(&kit, "/v1/me", &Caller::Token(access.clone()))
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert!(
        authenticate_mcp(&kit.ports, &Caller::Token(access).headers())
            .await
            .is_err()
    );
    let form = format!("grant_type=refresh_token&refresh_token={refresh}&client_id={client_id}");
    let refused = post_form(&kit, "/v1/oauth/token", &form, &Caller::Anonymous).await;
    assert_eq!(
        error_of(&refused),
        "invalid_grant",
        "a revoked connection cannot be renewed"
    );
}

#[pollster::test]
async fn an_expired_refresh_token_is_refused() {
    let kit = support::signed_in().await;
    let (_, refresh, client_id) = connected(&kit).await;
    let stale = Statement::with_values(
        "UPDATE oauth_refresh_tokens SET expires_at = ? WHERE token_hash = ?",
        vec![
            support::text("2026-01-01T00:00:00Z"),
            support::text(&sha256_hex(&refresh)),
        ],
    );
    kit.harness
        .db
        .execute(&stale)
        .await
        .expect("the expiry seeds");
    let form = format!("grant_type=refresh_token&refresh_token={refresh}&client_id={client_id}");
    let refused = post_form(&kit, "/v1/oauth/token", &form, &Caller::Anonymous).await;
    assert_eq!(error_of(&refused), "invalid_grant");
}

#[pollster::test]
async fn an_unsupported_grant_is_named_as_one() {
    let kit = support::signed_in().await;
    let refused = post_form(
        &kit,
        "/v1/oauth/token",
        "grant_type=password&username=x",
        &Caller::Anonymous,
    )
    .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    assert_eq!(error_of(&refused), "unsupported_grant_type");
    // A device poll reaching this endpoint is named too, with where it belongs.
    let device = post_form(
        &kit,
        "/v1/oauth/token",
        "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code&device_code=x",
        &Caller::Anonymous,
    )
    .await;
    assert_eq!(error_of(&device), "unsupported_grant_type");
    let _: Value = device.json();
}

async fn rows(kit: &Kit, table: &str) -> usize {
    kit.harness
        .db
        .query(&Statement::with_values(
            format!("SELECT 1 FROM {table}"),
            Vec::new(),
        ))
        .await
        .expect("the rows read")
        .rows
        .len()
}

/// N1: another member cannot end this member's connection. The attempt answers
/// 404, and the connection's renewal is untouched.
#[pollster::test]
async fn a_member_cannot_end_another_members_connection() {
    let kit = support::signed_in().await;
    let (access, refresh, client_id) = connected(&kit).await;

    let attempt = support::delete(
        &kit,
        &format!("/v1/tokens/{}", prefix_of(&access)),
        &kit.other(),
    )
    .await;
    assert_eq!(attempt.status, StatusCode::NOT_FOUND, "{}", attempt.text());

    let renewed = post_form(
        &kit,
        "/v1/oauth/token",
        &format!("grant_type=refresh_token&refresh_token={refresh}&client_id={client_id}"),
        &Caller::Anonymous,
    )
    .await;
    assert_eq!(
        renewed.status,
        StatusCode::OK,
        "A's renewal still works: {}",
        renewed.text()
    );
    assert_eq!(
        get(&kit, "/v1/me", &Caller::Token(access)).await.status,
        StatusCode::UNAUTHORIZED
    );
}

/// Minor (a): the label follows the redirect the code was issued for, not the
/// client's other registered redirects.
#[pollster::test]
async fn the_connector_label_follows_the_redirect_the_code_was_issued_for() {
    let kit = support::signed_in().await;
    let made = post_json(
        &kit,
        "/v1/oauth/register",
        &json!({
            "client_name": CLIENT_NAME,
            "redirect_uris": [
                VERIFIED_REDIRECT,
                "https://chatgpt.com/connector_platform_oauth_redirect",
                "https://chat.openai.com/aip/g-docapp/oauth/callback",
                "https://evil.example/cb",
            ],
        }),
        &Caller::Anonymous,
    )
    .await;
    let client_id = made.json()["client_id"].as_str().unwrap().to_owned();

    let via_evil = approve_to(&kit, &client_id, "https://evil.example/cb", &kit.owner()).await;
    let redeemed = redeem_at(&kit, &via_evil, &client_id, "https://evil.example/cb").await;
    assert_eq!(redeemed.status, StatusCode::OK, "{}", redeemed.text());
    let evil_access = redeemed.json()["access_token"].as_str().unwrap().to_owned();
    let listed = get(&kit, "/v1/tokens", &kit.owner()).await;
    let evil_row = listed.json()["tokens"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["prefix"] == json!(prefix_of(&evil_access)))
        .expect("listed")
        .clone();
    assert_eq!(
        evil_row["name"],
        json!(CLIENT_NAME),
        "not labelled a Claude connector"
    );
    assert_eq!(evil_row["connector"], json!(true));

    let via_claude = approve_to(&kit, &client_id, VERIFIED_REDIRECT, &kit.owner()).await;
    let claude = redeem_at(&kit, &via_claude, &client_id, VERIFIED_REDIRECT).await;
    let claude_access = claude.json()["access_token"].as_str().unwrap().to_owned();
    let listed = get(&kit, "/v1/tokens", &kit.owner()).await;
    let claude_row = listed.json()["tokens"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["prefix"] == json!(prefix_of(&claude_access)))
        .expect("listed")
        .clone();
    assert_eq!(
        claude_row["name"],
        json!(format!("Claude connector · {CLIENT_NAME}"))
    );

    // Both ChatGPT hosts — the platform redirect and a Custom GPT's Actions
    // callback — carry their own label, under the same rule.
    for redirect in [
        "https://chatgpt.com/connector_platform_oauth_redirect",
        "https://chat.openai.com/aip/g-docapp/oauth/callback",
    ] {
        let via = approve_to(&kit, &client_id, redirect, &kit.owner()).await;
        let redeemed = redeem_at(&kit, &via, &client_id, redirect).await;
        assert_eq!(
            redeemed.status,
            StatusCode::OK,
            "{redirect}: {}",
            redeemed.text()
        );
        let access = redeemed.json()["access_token"].as_str().unwrap().to_owned();
        let listed = get(&kit, "/v1/tokens", &kit.owner()).await;
        let row = listed.json()["tokens"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["prefix"] == json!(prefix_of(&access)))
            .expect("listed")
            .clone();
        assert_eq!(
            row["name"],
            json!(format!("ChatGPT connector · {CLIENT_NAME}")),
            "{redirect}"
        );
        assert_eq!(row["connector"], json!(true), "{redirect}");
    }
}

// ---------------------------------------------------------------------------
// Client ID Metadata Document clients (issue #234)

/// A client_id that is a Client ID Metadata Document URL: the app never
/// registers, the server fetches and validates the document itself before a
/// consent page is ever shown.
const CIMD_ID: &str = "https://cimd.example/client.json";
/// The one redirect the test document names: ChatGPT's own, so this path
/// exercises the verified-host rules at the same time.
const CIMD_REDIRECT: &str = "https://chatgpt.com/connector_platform_oauth_redirect";
const CIMD_NAME: &str = "A document app";

/// The document a test serves at a client_id URL.
fn cimd_document(client_id: &str, name: &str, redirects: &[&str]) -> String {
    json!({
        "client_id": client_id,
        "client_name": name,
        "redirect_uris": redirects,
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
    })
    .to_string()
}

/// The authorize path naming a document client by its URL, in the shape the
/// grant reads.
fn cimd_path(client_id: &str, redirect: &str) -> String {
    format!(
        "/v1/oauth/authorize?response_type=code&client_id={}&redirect_uri={}\
         &code_challenge={CHALLENGE}&code_challenge_method=S256&state=s1",
        enc(client_id),
        enc(redirect)
    )
}

/// One served document: the URL it answers at, a status, a body, and the
/// Cache-Control the cache expiry is taken from.
type Served = (String, u16, String, Option<String>);

/// The stub `HttpClient` behind every document test: it answers the DNS
/// resolutions the SSRF guard makes with the address a test scripts (a
/// public one by default), serves the documents a test registers by exact
/// URL, and counts the document fetches — DNS aside — so a test can tell a
/// cache hit from a refetch. Nothing else is reachable.
#[derive(Clone)]
struct StubHttp {
    documents: Arc<Mutex<Vec<Served>>>,
    dns: Arc<Mutex<IpAddr>>,
    fetches: Arc<AtomicUsize>,
}

impl StubHttp {
    /// One good document at `url`.
    fn serving(url: &str, body: &str) -> Self {
        Self::default().with_document(url, 200, body, None)
    }

    /// Adds an exact answer at `url`: a status, a body, and the
    /// Cache-Control the expiry is taken from.
    fn with_document(
        self,
        url: &str,
        status: u16,
        body: &str,
        cache_control: Option<&str>,
    ) -> Self {
        self.documents.lock().expect("the documents").push((
            url.to_owned(),
            status,
            body.to_owned(),
            cache_control.map(str::to_owned),
        ));
        self
    }

    /// Replaces the body at `url`, so a refetch sees a new document.
    fn replace(&self, url: &str, body: &str) {
        let mut documents = self.documents.lock().expect("the documents");
        let slot = documents
            .iter_mut()
            .find(|(at, ..)| at == url)
            .expect("a served document");
        slot.2 = body.to_owned();
    }

    /// Makes every document host resolve to `address`.
    fn resolving_to(&self, address: IpAddr) {
        *self.dns.lock().expect("the resolver") = address;
    }

    /// How many document (not DNS) fetches were made.
    fn fetches(&self) -> usize {
        self.fetches.load(Ordering::SeqCst)
    }

    fn answer(&self, status: u16, body: &str, cache_control: Option<&str>) -> Response<Bytes> {
        let mut page = Response::builder().status(status);
        if let Some(raw) = cache_control {
            page = page.header(header::CACHE_CONTROL, raw);
        }
        page.body(Bytes::from(body.to_owned())).expect("a document")
    }
}

impl Default for StubHttp {
    fn default() -> Self {
        Self {
            documents: Arc::new(Mutex::new(Vec::new())),
            // A public address (example.com's), so the guard lets a fetch
            // through unless a test points the name somewhere it must not.
            dns: Arc::new(Mutex::new(IpAddr::from([93, 184, 216, 34]))),
            fetches: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[async_trait]
impl HttpClient for StubHttp {
    async fn send(&self, request: Request<Bytes>) -> Result<Response<Bytes>, HttpError> {
        let uri = request.uri().to_string();
        if uri.starts_with("https://cloudflare-dns.com/dns-query") {
            // One A record for the name asked about, holding the address the
            // test scripted; the AAAA answer comes back empty, which the
            // guard accepts — one public address is enough.
            let body = json!({
                "Status": 0,
                "Answer": [
                    {
                        "name": param(&uri, "name").unwrap_or_default(),
                        "type": 1,
                        "TTL": 60,
                        "data": self.dns.lock().expect("the resolver").to_string(),
                    }
                ],
            })
            .to_string();
            return Ok(self.answer(200, &body, None));
        }
        let served = self
            .documents
            .lock()
            .expect("the documents")
            .iter()
            .find(|(at, ..)| *at == uri)
            .cloned()
            .unwrap_or((uri, 404, String::new(), None));
        self.fetches.fetch_add(1, Ordering::SeqCst);
        Ok(self.answer(served.1, &served.2, served.3.as_deref()))
    }
}

/// A kit that fetches documents through `http`, with the always-allowing
/// limiter every other kit carries.
async fn cimd_kit(http: &StubHttp) -> Kit {
    support::signed_in_with(
        Some(Arc::new(http.clone())),
        Some(Arc::new(FakeRateLimiter::always_allow())),
    )
    .await
}

/// The token exchange naming a document client by its URL client_id.
async fn redeem_cimd(kit: &Kit, code: &str, redirect: &str) -> support::Res {
    post_form(
        kit,
        "/v1/oauth/token",
        &format!(
            "grant_type=authorization_code&code={code}&redirect_uri={}\
             &client_id={}&code_verifier={VERIFIER}",
            enc(redirect),
            enc(CIMD_ID)
        ),
        &Caller::Anonymous,
    )
    .await
}

/// The server metadata says a client_id URL is supported — and keeps saying
/// registration is, so both ways of being a client stand beside each other.
#[pollster::test]
async fn the_metadata_advertises_document_clients_and_the_iss_parameter() {
    let kit = support::signed_in().await;
    let metadata = get(
        &kit,
        "/.well-known/oauth-authorization-server",
        &Caller::Anonymous,
    )
    .await;
    assert_eq!(metadata.status, StatusCode::OK, "{}", metadata.text());
    let body = metadata.json();
    assert_eq!(body["client_id_metadata_document_supported"], json!(true));
    assert_eq!(
        body["authorization_response_iss_parameter_supported"],
        json!(true)
    );
    assert_eq!(
        body["registration_endpoint"],
        json!(format!("{API_BASE}/v1/oauth/register"))
    );
}

/// The whole grant over a document client: the document is fetched and
/// validated, the consent names the app (verified, ChatGPT's redirect), the
/// code is exchanged and renewed with the URL client_id, and the cached row
/// carries the expiry the cache is served from.
#[pollster::test]
async fn a_document_client_connects_without_registering() {
    let stub = StubHttp::serving(
        CIMD_ID,
        &cimd_document(CIMD_ID, CIMD_NAME, &[CIMD_REDIRECT]),
    );
    let kit = cimd_kit(&stub).await;

    let shown = get(&kit, &cimd_path(CIMD_ID, CIMD_REDIRECT), &kit.owner()).await;
    assert_eq!(shown.status, StatusCode::OK, "{}", shown.text());
    let page = shown.text();
    assert!(
        page.contains(CIMD_NAME),
        "the document names the app: {page}"
    );
    assert!(page.contains("chatgpt.com"), "{page}");
    assert!(!page.contains("Unverified app"), "{page}");
    assert_eq!(stub.fetches(), 1, "one authorization, one fetch");

    // The approval resolves the client again from the cached row: no second
    // fetch, and the code is issued to the redirect the document named.
    let form = format!(
        "request={}&decision=approve&workspace_id={WORKSPACE}",
        enc(&consent_request(&page))
    );
    let approved = post_form(&kit, "/v1/oauth/authorize", &form, &kit.owner()).await;
    assert_eq!(
        approved.status,
        StatusCode::SEE_OTHER,
        "{}",
        approved.text()
    );
    let location = approved.location().expect("the approval redirects");
    assert!(location.starts_with(CIMD_REDIRECT), "{location}");
    let code = param(&location, "code").expect("the redirect carries the code");

    let granted = redeem_cimd(&kit, &code, CIMD_REDIRECT).await;
    assert_eq!(granted.status, StatusCode::OK, "{}", granted.text());
    let body = granted.json();
    let access = body["access_token"].as_str().unwrap().to_owned();
    let refresh = body["refresh_token"].as_str().unwrap().to_owned();

    // The connector label follows the redirect the document named.
    let listed = get(&kit, "/v1/tokens", &kit.owner()).await;
    let row = listed.json()["tokens"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["prefix"] == json!(prefix_of(&access)))
        .expect("listed")
        .clone();
    assert_eq!(
        row["name"],
        json!(format!("ChatGPT connector · {CIMD_NAME}"))
    );

    // A renewal names the URL client_id too; the token endpoint reads the
    // stored row and never fetches.
    let rotated = post_form(
        &kit,
        "/v1/oauth/token",
        &format!(
            "grant_type=refresh_token&refresh_token={refresh}&client_id={}",
            enc(CIMD_ID)
        ),
        &Caller::Anonymous,
    )
    .await;
    assert_eq!(rotated.status, StatusCode::OK, "{}", rotated.text());
    assert_eq!(stub.fetches(), 1, "the token endpoint never fetches");

    // The client lives as a row keyed by its URL, with an expiry set — the
    // cache every later authorization is served from.
    let cached = kit
        .harness
        .db
        .query(&Statement::with_values(
            "SELECT 1 FROM oauth_clients WHERE client_id = ? AND metadata_expires_at IS NOT NULL",
            vec![support::text(CIMD_ID)],
        ))
        .await
        .expect("the cached client reads");
    assert_eq!(cached.rows.len(), 1, "the document is cached under its URL");
}

/// A document client that never registered spends no registration: the
/// dynamic registration this crate has always had keeps working beside it.
#[pollster::test]
async fn registration_keeps_working_beside_document_clients() {
    let kit = cimd_kit(&StubHttp::serving(
        CIMD_ID,
        &cimd_document(CIMD_ID, CIMD_NAME, &[CIMD_REDIRECT]),
    ))
    .await;
    let client_id = register(&kit).await;
    let code = approve(&kit, &client_id, &kit.owner()).await;
    let granted = redeem(&kit, &code, &client_id, VERIFIER).await;
    assert_eq!(granted.status, StatusCode::OK, "{}", granted.text());
}

/// A client_id URL whose host the server must not reach — a private or
/// link-local or loopback address, an internal or single-label name, or a
/// name that resolves into a private range — is refused with the
/// unknown-client page, and the document is never fetched.
#[pollster::test]
async fn a_document_host_the_server_must_not_reach_is_never_fetched() {
    let stub = StubHttp::serving(
        CIMD_ID,
        &cimd_document(CIMD_ID, CIMD_NAME, &[CIMD_REDIRECT]),
    );
    let kit = cimd_kit(&stub).await;
    for refused in [
        "https://10.0.0.1/client.json",
        "https://169.254.169.254/client.json",
        "https://127.0.0.1/client.json",
        "https://localhost/client.json",
        "https://docs.internal/client.json",
        "https://intranet/client.json",
        "https://cimd.example/../client.json",
    ] {
        let answer = get(&kit, &cimd_path(refused, CIMD_REDIRECT), &kit.owner()).await;
        assert_eq!(
            answer.status,
            StatusCode::BAD_REQUEST,
            "{refused}: {}",
            answer.text()
        );
        assert!(answer.location().is_none(), "{refused}: never a redirect");
    }
    // A name that resolves into a private range is refused the same way,
    // before any document is fetched — the DNS check is not decorative.
    stub.resolving_to(IpAddr::from([10, 0, 0, 7]));
    let answer = get(&kit, &cimd_path(CIMD_ID, CIMD_REDIRECT), &kit.owner()).await;
    assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{}", answer.text());
    assert_eq!(
        stub.fetches(),
        0,
        "no document was fetched from any of them"
    );
}

/// A document that breaks a rule — unreachable, a redirect, oversized,
/// another client's, or declaring a shared secret — gets the unknown-client
/// page, and nothing of it is ever cached.
#[pollster::test]
async fn a_document_that_breaks_a_rule_is_refused_and_never_cached() {
    let cases: Vec<(&str, StubHttp)> = vec![
        (
            "a 404",
            StubHttp::default().with_document(CIMD_ID, 404, "gone", None),
        ),
        (
            "a redirect",
            StubHttp::default().with_document(CIMD_ID, 302, "{}", None),
        ),
        (
            "an oversize body",
            StubHttp::default().with_document(
                CIMD_ID,
                200,
                &format!(
                    "{{\"client_id\": \"{CIMD_ID}\", \"padding\": \"{}\"}}",
                    "x".repeat(6 * 1024)
                ),
                None,
            ),
        ),
        (
            "another client's document",
            StubHttp::serving(
                CIMD_ID,
                &cimd_document(
                    "https://other.example/client.json",
                    CIMD_NAME,
                    &[CIMD_REDIRECT],
                ),
            ),
        ),
        (
            "a shared-secret client",
            StubHttp::serving(
                CIMD_ID,
                &format!(
                    "{{\"client_id\": \"{CIMD_ID}\", \"client_name\": \"{CIMD_NAME}\", \
                     \"redirect_uris\": [\"{CIMD_REDIRECT}\"], \
                     \"token_endpoint_auth_method\": \"client_secret_basic\"}}"
                ),
            ),
        ),
    ];
    for (why, stub) in cases {
        let kit = cimd_kit(&stub).await;
        let answer = get(&kit, &cimd_path(CIMD_ID, CIMD_REDIRECT), &kit.owner()).await;
        assert_eq!(
            answer.status,
            StatusCode::BAD_REQUEST,
            "{why}: {}",
            answer.text()
        );
        assert!(answer.location().is_none(), "{why}: never a redirect");
        assert_eq!(
            rows(&kit, "oauth_clients").await,
            0,
            "{why}: a refused document is never cached"
        );
    }
}

/// A redirect the document does not name is refused with a page, never a
/// redirect: the fetch and validation vouch for nothing else.
#[pollster::test]
async fn a_redirect_the_document_does_not_name_gets_a_page_never_a_redirect() {
    let kit = cimd_kit(&StubHttp::serving(
        CIMD_ID,
        &cimd_document(CIMD_ID, CIMD_NAME, &[CIMD_REDIRECT]),
    ))
    .await;
    let elsewhere = get(
        &kit,
        &cimd_path(CIMD_ID, "https://chatgpt.com/not-the-registered-one"),
        &kit.owner(),
    )
    .await;
    assert_eq!(
        elsewhere.status,
        StatusCode::BAD_REQUEST,
        "{}",
        elsewhere.text()
    );
    assert!(elsewhere.location().is_none());
}

/// A cached document is served until its expiry passes, then fetched again —
/// and what the new document says is what is served, both the name and the
/// redirect set.
#[pollster::test]
async fn a_cached_document_is_served_until_it_expires_then_fetched_again() {
    let stub = StubHttp::serving(
        CIMD_ID,
        &cimd_document(CIMD_ID, "First name", &[CIMD_REDIRECT]),
    );
    let kit = cimd_kit(&stub).await;

    let first = get(&kit, &cimd_path(CIMD_ID, CIMD_REDIRECT), &kit.owner()).await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.text());
    assert!(first.text().contains("First name"));
    assert_eq!(stub.fetches(), 1);

    // Within the cache window nothing is fetched again.
    let again = get(&kit, &cimd_path(CIMD_ID, CIMD_REDIRECT), &kit.owner()).await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.text());
    assert_eq!(stub.fetches(), 1, "a cache hit costs no fetch");

    // Past the window the document is fetched again, and the new document —
    // a new name, a new redirect — is what the consent is built from.
    kit.harness
        .db
        .execute(&Statement::with_values(
            "UPDATE oauth_clients SET metadata_expires_at = '2020-01-01T00:00:00Z' \
             WHERE client_id = ?",
            vec![support::text(CIMD_ID)],
        ))
        .await
        .expect("the cached client ages");
    let moved = "https://chatgpt.com/connector/oauth/callback-2";
    stub.replace(CIMD_ID, &cimd_document(CIMD_ID, "Second name", &[moved]));
    let refreshed = get(&kit, &cimd_path(CIMD_ID, moved), &kit.owner()).await;
    assert_eq!(refreshed.status, StatusCode::OK, "{}", refreshed.text());
    assert!(
        refreshed.text().contains("Second name"),
        "the refetch is served"
    );
    assert_eq!(stub.fetches(), 2);

    // The old redirect left with the old document: a page, not a redirect.
    let stale = get(&kit, &cimd_path(CIMD_ID, CIMD_REDIRECT), &kit.owner()).await;
    assert_eq!(stale.status, StatusCode::BAD_REQUEST, "{}", stale.text());
    assert!(stale.location().is_none());
}

/// Every document fetch — first or refresh — is rate-limited per calling
/// address, and the limit fails closed: no limiter bound or no `HttpClient`
/// port means the client_id is refused outright, at a cost of no fetch, while
/// everything else (registration included) keeps working.
#[pollster::test]
async fn document_fetches_are_rate_limited_and_fail_closed_without_a_limiter() {
    let stub = StubHttp::serving(
        CIMD_ID,
        &cimd_document(CIMD_ID, CIMD_NAME, &[CIMD_REDIRECT]),
    );

    // No limiter bound: refuse, and never fetch.
    let bare = support::signed_in_with(Some(Arc::new(stub.clone())), None).await;
    let answer = get(&bare, &cimd_path(CIMD_ID, CIMD_REDIRECT), &bare.owner()).await;
    assert_eq!(
        answer.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        answer.text()
    );
    assert!(
        answer.text().contains("cannot be checked"),
        "{}",
        answer.text()
    );
    assert_eq!(stub.fetches(), 0, "nothing was fetched");

    // No `HttpClient` port: the same refusal, in a deployment that has a
    // limiter but no egress — where registration still works.
    let grounded = support::signed_in().await;
    let answer = get(
        &grounded,
        &cimd_path(CIMD_ID, CIMD_REDIRECT),
        &grounded.owner(),
    )
    .await;
    assert_eq!(
        answer.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        answer.text()
    );
    assert!(
        register(&grounded).await.starts_with("lbc_"),
        "registration is unaffected"
    );

    // A limiter that says no: the 429 every other limited route answers.
    let denied = Decision {
        ok: false,
        retry_after: Some(std::time::Duration::from_secs(60)),
        quota: None,
    };
    let throttled = support::signed_in_with(
        Some(Arc::new(stub.clone())),
        Some(Arc::new(FakeRateLimiter::scripted(
            vec![denied.clone()],
            denied,
        ))),
    )
    .await;
    let answer = get(
        &throttled,
        &cimd_path(CIMD_ID, CIMD_REDIRECT),
        &throttled.owner(),
    )
    .await;
    assert_eq!(
        answer.status,
        StatusCode::TOO_MANY_REQUESTS,
        "{}",
        answer.text()
    );
    assert_eq!(stub.fetches(), 0, "a denied fetch is never made");
}

// ---------------------------------------------------------------------------
// The predefined Actions client (issue #234): a confidential client
// configured out of band, whose Custom GPT Actions send no PKCE and
// authenticate by a client secret instead.

/// The id a test configures — deliberately neither an `lbc_` one nor a URL,
/// the two shapes this server generates itself, so it can be no registered
/// client's.
const ACTIONS_ID: &str = "livingbrain-gpt-actions";
const ACTIONS_SECRET: &str = "a secret the operator set";
/// A custom GPT's Actions callback, on a host Living Brain recognises.
const ACTIONS_REDIRECT: &str = "https://chat.openai.com/aip/g-9f8e7d/oauth/callback";
/// A second callback the operator may list beside the first.
const ACTIONS_OTHER_REDIRECT: &str = "https://chatgpt.com/aip/g-9f8e7d/oauth/callback";

/// The kit with the Actions client configured, plus any extra config pairs a
/// test varies.
async fn actions_kit(extra: &[(&str, &str)]) -> Kit {
    let mut pairs: Vec<(&str, String)> = vec![
        ("OAUTH_ACTIONS_CLIENT_ID", ACTIONS_ID.to_owned()),
        (
            "OAUTH_ACTIONS_CLIENT_SECRET_SHA256",
            sha256_hex(ACTIONS_SECRET),
        ),
        (
            "OAUTH_ACTIONS_REDIRECT_URIS",
            format!("{ACTIONS_REDIRECT} {ACTIONS_OTHER_REDIRECT}"),
        ),
    ];
    pairs.extend(extra.iter().map(|(key, value)| (*key, (*value).to_owned())));
    let pairs: Vec<(&str, &str)> = pairs
        .iter()
        .map(|(key, value)| (*key, value.as_str()))
        .collect();
    support::signed_in_with_config(&pairs).await
}

/// The authorize request a GPT's Actions send: no code_challenge, which is
/// the point of the confidential client. A redirect is named by choice.
fn actions_authorize_path(redirect: &str) -> String {
    format!(
        "/v1/oauth/authorize?response_type=code&client_id={ACTIONS_ID}&redirect_uri={}&state=s1",
        enc(redirect)
    )
}

/// The approval of an Actions redirect, which yields the code.
async fn approve_actions(kit: &Kit, redirect: &str) -> String {
    let shown = get(kit, &actions_authorize_path(redirect), &kit.owner()).await;
    assert_eq!(shown.status, StatusCode::OK, "{}", shown.text());
    assert!(!shown.text().contains("Unverified app"), "{}", shown.text());
    let form = format!(
        "request={}&decision=approve&workspace_id={WORKSPACE}",
        enc(&consent_request(&shown.text()))
    );
    let approved = post_form(kit, "/v1/oauth/authorize", &form, &kit.owner()).await;
    assert_eq!(
        approved.status,
        StatusCode::SEE_OTHER,
        "{}",
        approved.text()
    );
    param(
        &approved.location().expect("the approval redirects"),
        "code",
    )
    .expect("the redirect carries the code")
}

/// Standard base64 with padding — the test's only encoder need, for the
/// Basic credentials `client_secret_basic` sends (RFC 6749 §2.3.1).
fn basic_header(id: &str, secret: &str) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let raw = format!("{id}:{secret}");
    let mut out = String::from("Basic ");
    for chunk in raw.as_bytes().chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for shift in [18, 12, 6, 0] {
            out.push(char::from(ALPHABET[(n >> shift) as usize & 63]));
        }
    }
    match raw.len() % 3 {
        1 => {
            out.truncate(out.len() - 2);
            out.push_str("==");
        }
        2 => {
            out.truncate(out.len() - 1);
            out.push('=');
        }
        _ => {}
    }
    out
}

/// A token request naming the Actions client, with whichever client
/// authentication the caller picks: the secret in the form
/// (`client_secret_post`), in the Authorization header
/// (`client_secret_basic`), or neither.
async fn actions_token(
    kit: &Kit,
    form: &str,
    form_secret: Option<&str>,
    header_secret: Option<&str>,
) -> support::Res {
    let mut form = form.to_owned();
    if let Some(secret) = form_secret {
        form.push_str(&format!("&client_secret={}", enc(secret)));
    }
    match header_secret {
        Some(secret) => {
            let header = basic_header(ACTIONS_ID, secret);
            support::post_form_authorized(kit, "/v1/oauth/token", &form, &header).await
        }
        None => post_form(kit, "/v1/oauth/token", &form, &Caller::Anonymous).await,
    }
}

/// One authorization-code exchange for the Actions client.
async fn actions_exchange(
    kit: &Kit,
    code: &str,
    form_secret: Option<&str>,
    header_secret: Option<&str>,
) -> support::Res {
    actions_token(
        kit,
        &format!(
            "grant_type=authorization_code&code={code}&redirect_uri={}&client_id={ACTIONS_ID}",
            enc(ACTIONS_REDIRECT)
        ),
        form_secret,
        header_secret,
    )
    .await
}

/// One renewal for the Actions client.
async fn actions_refresh(
    kit: &Kit,
    refresh: &str,
    form_secret: Option<&str>,
    header_secret: Option<&str>,
) -> support::Res {
    actions_token(
        kit,
        &format!("grant_type=refresh_token&refresh_token={refresh}&client_id={ACTIONS_ID}"),
        form_secret,
        header_secret,
    )
    .await
}

/// The whole grant for the predefined client, which no registration and no
/// PKCE ever touched: the consent, the code, and an actions token minted by
/// `client_secret_post` — read-only, never an admin, refused by the MCP
/// endpoint and the general resolver, listed under the ChatGPT label with
/// the configured default name.
#[pollster::test]
async fn the_actions_client_connects_without_pkce_by_client_secret_post() {
    let kit = actions_kit(&[]).await;
    let code = approve_actions(&kit, ACTIONS_REDIRECT).await;

    let granted = actions_exchange(&kit, &code, Some(ACTIONS_SECRET), None).await;
    assert_eq!(granted.status, StatusCode::OK, "{}", granted.text());
    let body = granted.json();
    let access = body["access_token"].as_str().unwrap().to_owned();
    assert!(access.starts_with("lbp_"), "{access}");
    assert!(
        body["refresh_token"].as_str().unwrap().starts_with("lbr_"),
        "{}",
        granted.text()
    );
    assert_eq!(
        body["scope"],
        json!(format!("read:shared read:user:{OWNER}")),
        "the response names the read-only subset it minted"
    );

    // An actions token, exactly as the four REST routes resolve it — and as
    // every other surface refuses it.
    let headers = Caller::Token(access.clone()).headers();
    let Ok(Authenticated::Member(member)) = authenticate_actions(&kit.ports, &headers).await else {
        panic!("the actions surface accepts the token");
    };
    assert_eq!(member.user_id, OWNER);
    assert_eq!(member.workspace_id, WORKSPACE);
    assert!(!member.is_admin, "an actions token is never an admin");
    assert_eq!(
        member.scopes,
        Some(vec!["read:shared".to_owned(), format!("read:user:{OWNER}")])
    );
    assert!(
        authenticate_mcp(&kit.ports, &headers).await.is_err(),
        "the MCP endpoint refuses it"
    );
    assert!(
        authenticate(&kit.ports, &headers).await.is_err(),
        "and so does every surface but the four routes"
    );

    let listed = get(&kit, "/v1/tokens", &kit.owner()).await;
    let row = listed.json()["tokens"]
        .as_array()
        .unwrap()
        .iter()
        .find(|token| token["prefix"] == json!(prefix_of(&access)))
        .expect("the connection is listed")
        .clone();
    assert_eq!(row["name"], json!("ChatGPT connector · Living Brain GPT"));
    assert_eq!(row["connector"], json!(true));
}

/// `client_secret_basic` is accepted beside the form secret, a wrong or
/// missing secret is the RFC 6749 §5.2 401 (with `WWW-Authenticate` when the
/// attempt came by the header), and neither failed guess spends the code.
#[pollster::test]
async fn the_actions_client_authenticates_by_basic_and_a_wrong_secret_is_a_401() {
    let kit = actions_kit(&[("OAUTH_ACTIONS_CLIENT_NAME", "Team brain")]).await;
    let code = approve_actions(&kit, ACTIONS_REDIRECT).await;

    let missing = actions_exchange(&kit, &code, None, None).await;
    assert_eq!(
        missing.status,
        StatusCode::UNAUTHORIZED,
        "{}",
        missing.text()
    );
    assert_eq!(error_of(&missing), "invalid_client");

    let wrong = actions_exchange(&kit, &code, Some("not the secret"), None).await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED, "{}", wrong.text());
    assert_eq!(error_of(&wrong), "invalid_client");
    assert!(
        wrong.headers.get("www-authenticate").is_none(),
        "no Basic attempt, no Basic challenge"
    );

    let wrong_basic = actions_exchange(&kit, &code, None, Some("not the secret")).await;
    assert_eq!(error_of(&wrong_basic), "invalid_client");
    assert_eq!(
        wrong_basic
            .headers
            .get("www-authenticate")
            .and_then(|value| value.to_str().ok()),
        Some("Basic realm=\"livingbrain\", charset=\"UTF-8\""),
        "{}",
        wrong_basic.text()
    );

    // The code survived every failed guess: authentication precedes it.
    let granted = actions_exchange(&kit, &code, None, Some(ACTIONS_SECRET)).await;
    assert_eq!(granted.status, StatusCode::OK, "{}", granted.text());
    let access = granted.json()["access_token"].as_str().unwrap().to_owned();
    let refresh = granted.json()["refresh_token"].as_str().unwrap().to_owned();
    assert!(
        authenticate_actions(&kit.ports, &Caller::Token(access.clone()).headers())
            .await
            .is_ok()
    );

    // The configured name carries into the label.
    let listed = get(&kit, "/v1/tokens", &kit.owner()).await;
    let row = listed.json()["tokens"]
        .as_array()
        .unwrap()
        .iter()
        .find(|token| token["prefix"] == json!(prefix_of(&access)))
        .expect("listed")
        .clone();
    assert_eq!(row["name"], json!("ChatGPT connector · Team brain"));

    // A renewal may come by Basic as well.
    let rotated = actions_refresh(&kit, &refresh, None, Some(ACTIONS_SECRET)).await;
    assert_eq!(rotated.status, StatusCode::OK, "{}", rotated.text());
}

/// A renewal for the confidential client needs the secret, and rotates
/// within the `actions` audience: the successor is an actions token, and the
/// access token it replaced is refused.
#[pollster::test]
async fn an_actions_renewal_requires_the_secret_and_rotates_within_the_audience() {
    let kit = actions_kit(&[]).await;
    let code = approve_actions(&kit, ACTIONS_REDIRECT).await;
    let granted = actions_exchange(&kit, &code, Some(ACTIONS_SECRET), None).await;
    let body = granted.json();
    let access = body["access_token"].as_str().unwrap().to_owned();
    let refresh = body["refresh_token"].as_str().unwrap().to_owned();

    let bare = actions_refresh(&kit, &refresh, None, None).await;
    assert_eq!(error_of(&bare), "invalid_client", "{}", bare.text());
    let wrong = actions_refresh(&kit, &refresh, Some("nope"), None).await;
    assert_eq!(error_of(&wrong), "invalid_client");

    let rotated = actions_refresh(&kit, &refresh, Some(ACTIONS_SECRET), None).await;
    assert_eq!(rotated.status, StatusCode::OK, "{}", rotated.text());
    let body = rotated.json();
    let next_access = body["access_token"].as_str().unwrap().to_owned();
    let next_refresh = body["refresh_token"].as_str().unwrap().to_owned();
    assert_ne!(next_access, access);
    assert_ne!(next_refresh, refresh);
    assert_eq!(
        body["scope"],
        json!(format!("read:shared read:user:{OWNER}")),
        "the successor keeps the read-only subset"
    );

    assert!(
        authenticate_actions(&kit.ports, &Caller::Token(next_access.clone()).headers())
            .await
            .is_ok(),
        "the successor is an actions token"
    );
    assert!(
        authenticate_mcp(&kit.ports, &Caller::Token(next_access.clone()).headers())
            .await
            .is_err(),
        "within the actions audience alone"
    );
    assert!(
        authenticate_actions(&kit.ports, &Caller::Token(access.clone()).headers())
            .await
            .is_err(),
        "the token rotation retired is refused"
    );

    // The chain still ends as any connection's does: a replayed spent
    // refresh token revokes the family.
    let replay = actions_refresh(&kit, &refresh, Some(ACTIONS_SECRET), None).await;
    assert_eq!(error_of(&replay), "invalid_grant", "{}", replay.text());
    assert!(
        authenticate_actions(&kit.ports, &Caller::Token(next_access).headers())
            .await
            .is_err(),
        "the replay ended the connection"
    );
}

/// A code the Actions client asked for with a challenge is bound by it:
/// redemption without a verifier, or with the wrong one, is refused — the
/// secret alone does not open a code PKCE sealed.
#[pollster::test]
async fn a_challenge_the_actions_client_sends_is_enforced_at_redemption() {
    let kit = actions_kit(&[]).await;
    let path = format!(
        "{}&code_challenge={CHALLENGE}&code_challenge_method=S256",
        actions_authorize_path(ACTIONS_REDIRECT)
    );

    // `plain` is refused exactly as a public client's request is.
    let plain = get(
        &kit,
        &format!(
            "/v1/oauth/authorize?response_type=code&client_id={ACTIONS_ID}&redirect_uri={}\
             &code_challenge={CHALLENGE}&code_challenge_method=plain&state=s1",
            enc(ACTIONS_REDIRECT)
        ),
        &kit.owner(),
    )
    .await;
    assert_eq!(plain.status, StatusCode::FOUND, "{}", plain.text());
    let location = plain.location().expect("redirected back");
    assert_eq!(
        param(&location, "error").as_deref(),
        Some("invalid_request"),
        "{location}"
    );

    let shown = get(&kit, &path, &kit.owner()).await;
    assert_eq!(shown.status, StatusCode::OK, "{}", shown.text());
    let form = format!(
        "request={}&decision=approve&workspace_id={WORKSPACE}",
        enc(&consent_request(&shown.text()))
    );
    let approved = post_form(&kit, "/v1/oauth/authorize", &form, &kit.owner()).await;
    let code = param(&approved.location().unwrap(), "code").unwrap();

    let bare = actions_exchange(&kit, &code, Some(ACTIONS_SECRET), None).await;
    assert_eq!(error_of(&bare), "invalid_grant", "{}", bare.text());
    assert!(
        bare.text().contains("code does not match"),
        "{}",
        bare.text()
    );

    // The right verifier redeems a fresh code.
    let shown = get(&kit, &path, &kit.owner()).await;
    let form = format!(
        "request={}&decision=approve&workspace_id={WORKSPACE}",
        enc(&consent_request(&shown.text()))
    );
    let approved = post_form(&kit, "/v1/oauth/authorize", &form, &kit.owner()).await;
    let code = param(&approved.location().unwrap(), "code").unwrap();
    let granted = actions_token(
        &kit,
        &format!(
            "grant_type=authorization_code&code={code}&redirect_uri={}&client_id={ACTIONS_ID}\
             &code_verifier={VERIFIER}",
            enc(ACTIONS_REDIRECT)
        ),
        Some(ACTIONS_SECRET),
        None,
    )
    .await;
    assert_eq!(granted.status, StatusCode::OK, "{}", granted.text());
}

/// The public clients keep every rule the confidential client is excused
/// from: PKCE stays mandatory, and a secret presented for one is refused —
/// before its code is spent.
#[pollster::test]
async fn a_public_client_still_needs_pkce_and_may_not_present_a_secret() {
    let kit = actions_kit(&[]).await;
    let client_id = register(&kit).await;

    let no_pkce = get(
        &kit,
        &format!(
            "/v1/oauth/authorize?response_type=code&client_id={client_id}&redirect_uri={}\
             &state=s1",
            enc(REDIRECT)
        ),
        &kit.owner(),
    )
    .await;
    assert_eq!(no_pkce.status, StatusCode::FOUND, "{}", no_pkce.text());
    let location = no_pkce.location().expect("redirected back");
    assert_eq!(
        param(&location, "error").as_deref(),
        Some("invalid_request"),
        "{location}"
    );
    assert!(param(&location, "code").is_none(), "{location}");

    // A secret in the form, or by Basic — neither reaches the code.
    let code = approve(&kit, &client_id, &kit.owner()).await;
    let via_form = post_form(
        &kit,
        "/v1/oauth/token",
        &format!(
            "grant_type=authorization_code&code={code}&redirect_uri={}&client_id={client_id}\
             &code_verifier={VERIFIER}&client_secret=whatever",
            enc(REDIRECT)
        ),
        &Caller::Anonymous,
    )
    .await;
    assert_eq!(
        via_form.status,
        StatusCode::UNAUTHORIZED,
        "{}",
        via_form.text()
    );
    assert_eq!(error_of(&via_form), "invalid_client");

    let via_basic = support::post_form_authorized(
        &kit,
        "/v1/oauth/token",
        &format!(
            "grant_type=authorization_code&code={code}&redirect_uri={}&client_id={client_id}\
             &code_verifier={VERIFIER}",
            enc(REDIRECT)
        ),
        &basic_header(&client_id, "whatever"),
    )
    .await;
    assert_eq!(
        error_of(&via_basic),
        "invalid_client",
        "{}",
        via_basic.text()
    );
    assert_eq!(
        via_basic
            .headers
            .get("www-authenticate")
            .and_then(|value| value.to_str().ok()),
        Some("Basic realm=\"livingbrain\", charset=\"UTF-8\"")
    );

    // The refused attempts spent nothing: the plain exchange still works.
    let granted = redeem(&kit, &code, &client_id, VERIFIER).await;
    assert_eq!(granted.status, StatusCode::OK, "{}", granted.text());
}

/// A code belongs to the client it was issued to: the Actions client's code
/// is not a registered app's, and a registered app's code does not open by
/// the Actions secret.
#[pollster::test]
async fn codes_never_cross_between_the_actions_client_and_registered_ones() {
    let kit = actions_kit(&[]).await;
    let client_id = register(&kit).await;

    let code = approve_actions(&kit, ACTIONS_REDIRECT).await;
    let as_public = post_form(
        &kit,
        "/v1/oauth/token",
        &format!(
            "grant_type=authorization_code&code={code}&redirect_uri={}&client_id={client_id}\
             &code_verifier={VERIFIER}",
            enc(ACTIONS_REDIRECT)
        ),
        &Caller::Anonymous,
    )
    .await;
    assert_eq!(
        error_of(&as_public),
        "invalid_grant",
        "{}",
        as_public.text()
    );

    let code = approve(&kit, &client_id, &kit.owner()).await;
    let as_actions = actions_token(
        &kit,
        &format!(
            "grant_type=authorization_code&code={code}&redirect_uri={}&client_id={ACTIONS_ID}\
             &code_verifier={VERIFIER}",
            enc(REDIRECT)
        ),
        Some(ACTIONS_SECRET),
        None,
    )
    .await;
    assert_eq!(
        error_of(&as_actions),
        "invalid_grant",
        "the secret authenticates the client, not the code: {}",
        as_actions.text()
    );
}

/// A redirect the operator did not list gets the same page an unregistered
/// redirect always gets: no code, no redirect, nothing sent.
#[pollster::test]
async fn a_redirect_the_actions_client_does_not_list_gets_a_page_never_a_redirect() {
    let kit = actions_kit(&[]).await;
    let elsewhere = get(
        &kit,
        &actions_authorize_path("https://chatgpt.com/connector_platform_oauth_redirect"),
        &kit.owner(),
    )
    .await;
    assert_eq!(
        elsewhere.status,
        StatusCode::BAD_REQUEST,
        "{}",
        elsewhere.text()
    );
    assert!(elsewhere.location().is_none());
}

/// None of the three keys set leaves the feature off: the id is simply
/// another unknown client, at both endpoints.
#[pollster::test]
async fn the_actions_client_is_off_when_unconfigured() {
    let kit = support::signed_in().await;
    let answer = get(
        &kit,
        &actions_authorize_path(ACTIONS_REDIRECT),
        &kit.owner(),
    )
    .await;
    assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{}", answer.text());
    assert!(
        answer.text().contains("not registered"),
        "{}",
        answer.text()
    );
    assert!(answer.location().is_none());

    let token = actions_exchange(&kit, "any-code", Some(ACTIONS_SECRET), None).await;
    assert_eq!(error_of(&token), "invalid_client", "{}", token.text());
}

/// A partial or malformed configuration fails closed: `validate_config`
/// names what is wrong, and the routes that meet the configured id refuse
/// with a clear page rather than approving under half the checks.
#[pollster::test]
async fn a_partial_or_malformed_actions_config_fails_closed() {
    let working = |pairs: &[(&str, &str)]| MapConfig::from_pairs(pairs.to_vec());
    let hash = sha256_hex(ACTIONS_SECRET);
    let upper_hash = hash.to_uppercase();
    let (id, hash, redirects) = (
        ("OAUTH_ACTIONS_CLIENT_ID", ACTIONS_ID),
        ("OAUTH_ACTIONS_CLIENT_SECRET_SHA256", hash.as_str()),
        ("OAUTH_ACTIONS_REDIRECT_URIS", ACTIONS_REDIRECT),
    );
    OAuth::new()
        .validate_config(&working(&[]))
        .expect("unset is off, which is fine");
    OAuth::new()
        .validate_config(&working(&[id, hash, redirects]))
        .expect("the three keys together make a client");

    let broken = [
        vec![id],
        vec![hash],
        vec![id, hash],
        vec![("OAUTH_ACTIONS_CLIENT_ID", "lbc_someones"), hash, redirects],
        vec![
            (
                "OAUTH_ACTIONS_CLIENT_ID",
                "https://chat.example/client.json",
            ),
            hash,
            redirects,
        ],
        vec![
            id,
            ("OAUTH_ACTIONS_CLIENT_SECRET_SHA256", "not-hex"),
            redirects,
        ],
        vec![
            id,
            ("OAUTH_ACTIONS_CLIENT_SECRET_SHA256", upper_hash.as_str()),
            redirects,
        ],
        vec![
            id,
            hash,
            (
                "OAUTH_ACTIONS_REDIRECT_URIS",
                "https://ok.example/cb http://evil.example/cb",
            ),
        ],
    ];
    for pairs in &broken {
        let error = OAuth::new()
            .validate_config(&working(pairs))
            .expect_err("a broken configuration is refused");
        assert!(
            error.to_string().contains("OAUTH_ACTIONS"),
            "{pairs:?}: {error}"
        );
    }

    // Over the wire, the misconfigured id answers a clear page, and the
    // token endpoint a clear error — never a code, never a token.
    let kit = support::signed_in_with_config(&[("OAUTH_ACTIONS_CLIENT_ID", ACTIONS_ID)]).await;
    let answer = get(
        &kit,
        &actions_authorize_path(ACTIONS_REDIRECT),
        &kit.owner(),
    )
    .await;
    assert_eq!(answer.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(answer.text().contains("misconfigured"), "{}", answer.text());
    assert!(answer.location().is_none());
    let token = actions_exchange(&kit, "any-code", Some(ACTIONS_SECRET), None).await;
    assert_eq!(token.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(token.text().contains("misconfigured"), "{}", token.text());
}
