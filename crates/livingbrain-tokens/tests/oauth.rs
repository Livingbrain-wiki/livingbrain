//! The connector sign-in over the wire (issue #72): an MCP client registers,
//! sends its user to the consent, and redeems the code it is handed for a
//! personal access token and a renewal token. Every refusal the grant makes is
//! held to here too: a forged or cross-site approval, a replayed or expired
//! code, a verifier that does not match, a redirect the client did not
//! register, and a revoked connection.

mod support;

use cratefield_core::Statement;
use cratefield_core::axum::http::StatusCode;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use support::{
    API_BASE, Caller, Kit, OWNER, WORKSPACE, get, get_at, post_form, post_json, prefix_of,
};

const REDIRECT: &str = "https://claude.example/api/mcp/auth_callback";
const CLIENT_NAME: &str = "Claude test";
/// RFC 7636 Appendix B: the one published verifier and its S256 challenge.
const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
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
    let shown = get(kit, &authorize_path(client_id, CHALLENGE, "s1"), who).await;
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
    assert!(location.starts_with(REDIRECT), "{location}");
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

    for uri in [
        "http://claude.example/cb",
        "https://claude.example/cb#fragment",
        "javascript:alert(1)",
        "myapp://callback",
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
async fn a_bad_request_is_sent_back_to_the_registered_redirect_with_an_error() {
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
    }
}

#[pollster::test]
async fn authorize_on_another_host_moves_to_the_app_origin_with_the_same_request() {
    let kit = support::signed_in().await;
    let client_id = register(&kit).await;
    let path = authorize_path(&client_id, CHALLENGE, "s1");
    let moved = get_at(&kit, &path, "mcp.livingbrain.wiki", None).await;
    assert_eq!(moved.status, StatusCode::FOUND, "{}", moved.text());
    assert_eq!(
        moved.location().as_deref(),
        Some(format!("{API_BASE}{path}").as_str()),
        "the session cookie lives on the app origin, so the consent does too"
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

    // Somebody else's signed-in browser holding the form: refused, the consent
    // was shown to the owner.
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
    let codes = rows(&kit, "oauth_codes").await;
    assert_eq!(codes, 0, "a refused approval mints no code");
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
    assert!(param(&location, "code").is_none(), "{location}");
}

// ---------------------------------------------------------------------------
// The token endpoint

#[pollster::test]
async fn a_code_redeems_once_for_a_personal_access_token_and_a_refresh_token() {
    let kit = support::signed_in().await;
    let (access, refresh, _) = connected(&kit).await;
    assert!(
        access.starts_with("lbp_"),
        "a personal access token: {access}"
    );
    assert!(refresh.starts_with("lbr_"), "{refresh}");

    // The access token is an ordinary PAT: it resolves to the member the person
    // approved, in the workspace they chose.
    let me = get(&kit, "/v1/me", &Caller::Token(access.clone())).await;
    assert_eq!(me.status, StatusCode::OK, "{}", me.text());
    assert_eq!(me.json()["principal"]["id"], OWNER);
    assert_eq!(me.json()["credential"]["prefix"], json!(prefix_of(&access)));

    // It is listed under the person's API tokens, named for the connector.
    let listed = get(&kit, "/v1/tokens", &kit.owner()).await;
    let names: Vec<String> = listed.json()["tokens"]
        .as_array()
        .unwrap()
        .iter()
        .map(|token| token["name"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(
        names
            .iter()
            .any(|name| name == "Claude connector · Claude test"),
        "{names:?}"
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
async fn a_replayed_code_is_refused_and_revokes_the_token_it_made() {
    let kit = support::signed_in().await;
    let client_id = register(&kit).await;
    let code = approve(&kit, &client_id, &kit.owner()).await;
    let first = redeem(&kit, &code, &client_id, VERIFIER).await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.text());
    let access = first.json()["access_token"].as_str().unwrap().to_owned();

    let replay = redeem(&kit, &code, &client_id, VERIFIER).await;
    assert_eq!(error_of(&replay), "invalid_grant");
    let dead = get(&kit, "/v1/me", &Caller::Token(access)).await;
    assert_eq!(
        dead.status,
        StatusCode::UNAUTHORIZED,
        "the replayed code's token is revoked"
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
// Refresh, rotation and revocation

#[pollster::test]
async fn a_refresh_rotates_and_the_spent_token_is_refused() {
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

    // The new access token works; the one it replaced does not.
    assert_eq!(
        get(&kit, "/v1/me", &Caller::Token(next_access.clone()))
            .await
            .status,
        StatusCode::OK
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
    assert_eq!(
        get(&kit, "/v1/me", &Caller::Token(next_access))
            .await
            .status,
        StatusCode::UNAUTHORIZED,
        "the rotated access token ended with its connection"
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
        get(&kit, "/v1/me", &Caller::Token(access)).await.status,
        StatusCode::UNAUTHORIZED
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
    let _: Value = refused.json();
}

async fn rows(kit: &Kit, table: &str) -> usize {
    kit.harness
        .db
        .query(&Statement::with_values(
            format!("SELECT code_hash FROM {table}"),
            Vec::new(),
        ))
        .await
        .expect("the rows read")
        .rows
        .len()
}
