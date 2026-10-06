//! Sign-in with Slack, and the workspace isolation that rests on it.
//!
//! The acceptance criteria this file answers: sign-in works end to end,
//! a member of one workspace can never read another, and the owner is set
//! exactly once.

mod support;

use cratefield_core::axum::http::StatusCode;
use cratefield_core::{HmacSigner, Kid, MapConfig, Payload, Signer};
use livingbrain_workspaces::{FLOW_COOKIE, SESSION_COOKIE, SESSION_PURPOSE, UserChange};
use serde_json::{Value, json};
use support::*;

const TEAM_A: &str = "T0AAAAAAA";
const TEAM_B: &str = "T0BBBBBBB";
const ALICE: &str = "U0ALICEAA";
const BOB: &str = "U0BOBBBBB";
const CAROL: &str = "U0CAROLCC";
const DAVE: &str = "U0DAVEDDD";

const ME: &str = "/v1/workspaces/me";
const MEMBERS: &str = "/v1/workspaces/members";

// ---------------------------------------------------------------------------
// The flow, end to end

#[pollster::test]
async fn start_seals_a_flow_cookie_and_sends_the_browser_to_slack() {
    let http = TokenHttp::new();
    let kit = harness(&http);

    let start = get(&kit.router, "/v1/workspaces/slack/start", &[]).await;
    assert_eq!(start.status, StatusCode::FOUND);

    let location = start.location();
    assert!(
        location.starts_with("https://slack.com/openid/connect/authorize?"),
        "the browser goes to Slack: {location}"
    );
    assert!(location.contains("scope=openid%20profile%20email"));
    assert!(location.contains("client_id=1234.5678"));
    assert!(location.contains(&format!("redirect_uri={}", callback_url())));

    let headers = start.set_cookies().join(" | ");
    assert!(headers.contains("__Host-lb_flow="), "{headers}");
    assert!(headers.contains("Path=/"), "{headers}");
    assert!(headers.contains("Secure"), "{headers}");
    assert!(headers.contains("HttpOnly"), "{headers}");
    assert!(headers.contains("SameSite=Lax"), "{headers}");

    // The state and nonce in the URL are the ones sealed into the cookie.
    let cookie = start.cookie(FLOW_COOKIE).expect("a flow cookie");
    let payload = flow_payload(&kit, &cookie);
    assert_eq!(
        payload["state"].as_str().expect("state"),
        query_param(&location, "state")
    );
    assert_eq!(
        payload["nonce"].as_str().expect("nonce"),
        query_param(&location, "nonce")
    );
    assert_ne!(payload["state"], payload["nonce"]);
}

#[pollster::test]
async fn signing_in_creates_the_workspace_and_serves_me() {
    let http = TokenHttp::new();
    let kit = harness(&http);
    let alice = sign_in(&kit, &http, TEAM_A, ALICE, "Alice").await;

    let me = get(&kit.router, ME, &alice.cookies()).await;
    assert_eq!(me.status, StatusCode::OK);
    let body = me.json();
    // ADR 0002: the workspace id is ours, not the Slack team id, so a team
    // that has never been seen here gets `ws_` plus a minted id.
    assert!(
        alice.workspace_id.starts_with("ws_"),
        "a new workspace id is ours: {}",
        alice.workspace_id
    );
    assert_eq!(body["workspace"]["id"], alice.workspace_id);
    assert_eq!(body["workspace"]["owner_id"], ALICE);
    assert_eq!(body["workspace"]["name"], "T0AAAAAAA workspace");
    assert_eq!(body["member"]["user_id"], ALICE);
    assert_eq!(body["member"]["name"], "Alice");
    assert_eq!(body["member"]["is_admin"], false);
    assert_eq!(body["member"]["timezone"], Value::Null);
    assert_eq!(body["is_owner"], true);

    let members = get(&kit.router, MEMBERS, &alice.cookies()).await;
    assert_eq!(members.status, StatusCode::OK);
    let list = members.json();
    let rows = list.as_array().expect("an array");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["user_id"], ALICE);

    // The exchange was a form POST to Slack's token endpoint, carrying the
    // code, the client, and the same redirect URI the authorize call used.
    let requests = http.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0, "POST");
    assert_eq!(requests[0].1, "https://slack.com/api/openid.connect.token");
    assert!(requests[0].2.contains("code=code-1"), "{}", requests[0].2);
    assert!(requests[0].2.contains("client_id=1234.5678"));
    assert!(
        requests[0]
            .2
            .contains(&format!("redirect_uri={}", callback_url()))
    );
}

#[pollster::test]
async fn the_callback_returns_to_the_root_and_clears_the_flow_cookie() {
    let http = TokenHttp::new();
    let kit = harness(&http);

    let flow = start_flow(&kit).await;
    http.will_return(&id_token(&claims(&flow.nonce, TEAM_A, ALICE, "Alice")));

    let callback = get(&kit.router, &flow.callback("c"), &flow.cookies()).await;
    assert_eq!(callback.status, StatusCode::FOUND);
    assert_eq!(callback.location(), "/", "no open redirect");
    assert_eq!(
        callback.cookie(FLOW_COOKIE).as_deref(),
        Some(""),
        "the flow cookie is cleared"
    );
    assert!(callback.cookie(SESSION_COOKIE).is_some());

    let cleared = callback
        .set_cookies()
        .into_iter()
        .find(|cookie| cookie.starts_with("__Host-lb_flow="))
        .expect("a clearing flow cookie");
    assert!(cleared.contains("Max-Age=0"), "{cleared}");
}

// ---------------------------------------------------------------------------
// Owner exactly once

#[pollster::test]
async fn the_first_signer_owns_the_workspace_and_no_later_one_can_change_it() {
    let http = TokenHttp::new();
    let kit = harness(&http);

    let alice = sign_in(&kit, &http, TEAM_A, ALICE, "Alice").await;
    let alice_me = get(&kit.router, ME, &alice.cookies()).await.json();
    assert_eq!(alice_me["is_owner"], true);
    assert_eq!(alice_me["workspace"]["owner_id"], ALICE);

    let bob = sign_in(&kit, &http, TEAM_A, BOB, "Bob").await;
    // A team that is already linked keeps the workspace it was linked to,
    // so signing a second person into it is not a second workspace.
    assert_eq!(bob.workspace_id, alice.workspace_id);
    let bob_me = get(&kit.router, ME, &bob.cookies()).await.json();
    assert_eq!(bob_me["is_owner"], false);
    assert_eq!(
        bob_me["workspace"]["owner_id"], ALICE,
        "the second signer does not take ownership"
    );

    let carol = sign_in(&kit, &http, TEAM_A, CAROL, "Carol").await;
    assert_eq!(carol.workspace_id, alice.workspace_id);
    let carol_me = get(&kit.router, ME, &carol.cookies()).await.json();
    assert_eq!(carol_me["is_owner"], false);
    assert_eq!(carol_me["workspace"]["owner_id"], ALICE);

    // Alice's own view never stops saying she owns it.
    let alice_again = get(&kit.router, ME, &alice.cookies()).await.json();
    assert_eq!(alice_again["is_owner"], true);

    // Three members, one owner.
    let rows = get(&kit.router, MEMBERS, &alice.cookies()).await.json();
    assert_eq!(rows.as_array().expect("an array").len(), 3);
}

#[pollster::test]
async fn a_second_workspace_has_its_own_owner() {
    let http = TokenHttp::new();
    let kit = harness(&http);
    let alice = sign_in(&kit, &http, TEAM_A, ALICE, "Alice").await;
    let dave = sign_in(&kit, &http, TEAM_B, DAVE, "Dave").await;

    // Two teams, two workspaces, two ids — and neither is a team id.
    assert!(
        dave.workspace_id.starts_with("ws_"),
        "{}",
        dave.workspace_id
    );
    assert_ne!(dave.workspace_id, alice.workspace_id);
    assert_ne!(dave.workspace_id, TEAM_B);

    let dave_me = get(&kit.router, ME, &dave.cookies()).await.json();
    assert_eq!(dave_me["workspace"]["id"], dave.workspace_id);
    assert_eq!(dave_me["workspace"]["owner_id"], DAVE);
    assert_eq!(dave_me["is_owner"], true);
}

// ---------------------------------------------------------------------------
// Isolation

#[pollster::test]
async fn a_member_of_one_workspace_can_never_read_another() {
    let http = TokenHttp::new();
    let kit = harness(&http);
    let alice = sign_in(&kit, &http, TEAM_A, ALICE, "Alice").await;
    let bob = sign_in(&kit, &http, TEAM_A, BOB, "Bob").await;
    let dave = sign_in(&kit, &http, TEAM_B, DAVE, "Dave").await;
    let _ = &bob;

    let a = get(&kit.router, MEMBERS, &alice.cookies()).await.json();
    assert_eq!(user_ids(&a), vec![ALICE, BOB]);
    let b = get(&kit.router, MEMBERS, &dave.cookies()).await.json();
    assert_eq!(user_ids(&b), vec![DAVE]);

    let a_me = get(&kit.router, ME, &alice.cookies()).await.json();
    assert_eq!(a_me["workspace"]["id"], alice.workspace_id);
    let b_me = get(&kit.router, ME, &dave.cookies()).await.json();
    assert_eq!(b_me["workspace"]["id"], dave.workspace_id);

    // No route takes a workspace id from the request, so pointing one at
    // the other workspace changes nothing. Every spelling the old id went
    // by is in the query string, because every one of them is a value the
    // caller could have edited.
    let forced = get(
        &kit.router,
        &format!(
            "{MEMBERS}?workspace_id={}&team_id={}&id={}",
            dave.workspace_id, TEAM_B, dave.workspace_id
        ),
        &alice.cookies(),
    )
    .await;
    assert_eq!(forced.status, StatusCode::OK);
    assert_eq!(user_ids(&forced.json()), vec![ALICE, BOB]);

    let forced_me = get(
        &kit.router,
        &format!("{ME}?workspace_id={}", dave.workspace_id),
        &alice.cookies(),
    )
    .await
    .json();
    assert_eq!(forced_me["workspace"]["id"], alice.workspace_id);
    assert_eq!(forced_me["is_owner"], true);
}

#[pollster::test]
async fn a_validly_signed_session_for_a_workspace_you_are_not_in_is_refused() {
    let http = TokenHttp::new();
    let kit = harness(&http);
    let alice = sign_in(&kit, &http, TEAM_A, ALICE, "Alice").await;
    let dave = sign_in(&kit, &http, TEAM_B, DAVE, "Dave").await;

    // The signature is genuine — this is what a tampered *claim* looks
    // like once it has been re-signed. The workspace exists; the person is
    // simply not a member of it, so the member lookup is what refuses.
    let cross = signed_session(&kit, &dave.workspace_id, ALICE, NOW + 3600);
    assert_eq!(
        get(&kit.router, MEMBERS, &[(SESSION_COOKIE, &cross)])
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        get(&kit.router, ME, &[(SESSION_COOKIE, &cross)])
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );

    // A user who has never signed in anywhere is refused too.
    let ghost = signed_session(&kit, &alice.workspace_id, "U0GHOST00", NOW + 3600);
    assert_eq!(
        get(&kit.router, ME, &[(SESSION_COOKIE, &ghost)])
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );

    // And a workspace that does not exist at all.
    let nowhere = signed_session(&kit, "T0NOWHERE", ALICE, NOW + 3600);
    assert_eq!(
        get(&kit.router, ME, &[(SESSION_COOKIE, &nowhere)])
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
}

/// A session sealed before ADR 0002 named the Slack team id; the rows it
/// names keep that id, so the old name still resolves to the workspace.
#[pollster::test]
async fn a_session_sealed_before_the_rename_still_resolves() {
    let http = TokenHttp::new();
    let kit = harness(&http);
    let alice = sign_in(&kit, &http, TEAM_A, ALICE, "Alice").await;

    let before = kit.signer.sign(&Payload {
        purpose: SESSION_PURPOSE.to_owned(),
        subject: json!({
            "team_id": alice.workspace_id,
            "user_id": ALICE,
            "exp": NOW + 3600,
        })
        .to_string(),
        exp: None,
        kid: Kid::Cur,
    });
    let me = get(&kit.router, ME, &[(SESSION_COOKIE, &before)]).await;
    assert_eq!(me.status, StatusCode::OK, "the alias is honoured");
    assert_eq!(me.json()["workspace"]["id"], alice.workspace_id);
}

#[pollster::test]
async fn a_session_signed_with_another_key_is_refused() {
    let http = TokenHttp::new();
    let kit = harness(&http);
    let alice = sign_in(&kit, &http, TEAM_A, ALICE, "Alice").await;

    let other = HmacSigner::new("a-different-signing-secret-for-the-tests", None)
        .expect("a long enough secret");
    let forged = other.sign(&Payload {
        purpose: SESSION_PURPOSE.to_owned(),
        subject: json!({
            "workspace_id": alice.workspace_id,
            "user_id": ALICE,
            "exp": NOW + 3600,
        })
        .to_string(),
        exp: None,
        kid: Kid::Cur,
    });
    assert_eq!(
        get(&kit.router, ME, &[(SESSION_COOKIE, &forged)])
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
}

#[pollster::test]
async fn reads_without_a_session_at_all_are_refused() {
    let http = TokenHttp::new();
    let kit = harness(&http);
    let _alice = sign_in(&kit, &http, TEAM_A, ALICE, "Alice").await;

    for path in [ME, MEMBERS] {
        let response = get(&kit.router, path, &[]).await;
        assert_eq!(response.status, StatusCode::UNAUTHORIZED, "{path}");
        let response = get(&kit.router, path, &[(SESSION_COOKIE, "not-a-cookie")]).await;
        assert_eq!(response.status, StatusCode::UNAUTHORIZED, "{path}");
        let response = get(&kit.router, path, &[(SESSION_COOKIE, "a.b.c")]).await;
        assert_eq!(response.status, StatusCode::UNAUTHORIZED, "{path}");
    }
}

#[pollster::test]
async fn an_expired_session_is_refused() {
    let http = TokenHttp::new();
    let kit = harness(&http);
    let alice = sign_in(&kit, &http, TEAM_A, ALICE, "Alice").await;

    let stale = signed_session(&kit, &alice.workspace_id, ALICE, NOW - 1);
    assert_eq!(
        get(&kit.router, ME, &[(SESSION_COOKIE, &stale)])
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    // Freshly signed, so only the expiry can be what refuses it.
    let live = signed_session(&kit, &alice.workspace_id, ALICE, NOW + 1);
    assert_eq!(
        get(&kit.router, ME, &[(SESSION_COOKIE, &live)])
            .await
            .status,
        StatusCode::OK
    );
}

// ---------------------------------------------------------------------------
// The callback refuses what it cannot tie to a live attempt

#[pollster::test]
async fn the_callback_refuses_a_state_that_does_not_match() {
    let http = TokenHttp::new();
    let kit = harness(&http);
    let flow = start_flow(&kit).await;
    http.will_return(&id_token(&claims(&flow.nonce, TEAM_A, ALICE, "Alice")));

    let response = get(
        &kit.router,
        "/v1/workspaces/slack/callback?code=c&state=not-the-state",
        &flow.cookies(),
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(
        http.requests().is_empty(),
        "the state is checked before any token call"
    );
}

#[pollster::test]
async fn the_callback_refuses_a_missing_or_garbage_flow_cookie() {
    let http = TokenHttp::new();
    let kit = harness(&http);

    let missing = get(
        &kit.router,
        "/v1/workspaces/slack/callback?code=c&state=s",
        &[],
    )
    .await;
    assert_eq!(missing.status, StatusCode::BAD_REQUEST);

    let garbage = get(
        &kit.router,
        "/v1/workspaces/slack/callback?code=c&state=s",
        &[(FLOW_COOKIE, "not-a-cookie")],
    )
    .await;
    assert_eq!(garbage.status, StatusCode::BAD_REQUEST);
    assert!(http.requests().is_empty(), "no token call for a bad cookie");
}

#[pollster::test]
async fn the_callback_refuses_an_expired_flow_cookie() {
    let http = TokenHttp::new();
    let kit = harness(&http);
    let flow = start_flow(&kit).await;

    // The same cookie, past its ten minutes: the clock moves, not the
    // cookie. The signer secret is the kit's, so the cookie still
    // verifies — it is the expiry inside it that refuses.
    let later = harness_at(&http, config(), NOW + 700);
    let response = get(&later.router, &flow.callback("c"), &flow.cookies()).await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
}

#[pollster::test]
async fn a_flow_cookie_is_not_a_session_cookie() {
    let http = TokenHttp::new();
    let kit = harness(&http);
    let flow = start_flow(&kit).await;

    // Both cookies are signed by the same key, and only the purpose in the
    // payload tells them apart. Presenting the flow cookie as the session
    // — a valid signature, a live expiry, the wrong shape — must not be
    // enough to read a workspace.
    for path in [ME, MEMBERS] {
        let response = get(&kit.router, path, &[(SESSION_COOKIE, &flow.cookie)]).await;
        assert_eq!(response.status, StatusCode::UNAUTHORIZED, "{path}");
    }

    // And the other way round: a session cookie offered at the callback is
    // not a flow cookie.
    let alice = sign_in(&kit, &http, TEAM_A, ALICE, "Alice").await;
    let response = get(
        &kit.router,
        "/v1/workspaces/slack/callback?code=c&state=whatever",
        &[(FLOW_COOKIE, &alice.cookie)],
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
}

/// Drives start→callback with one mutated claim and answers the response.
async fn callback_response(mutate: impl FnOnce(&mut Value)) -> Res {
    let http = TokenHttp::new();
    let kit = harness(&http);
    let flow = start_flow(&kit).await;
    let mut claims = claims(&flow.nonce, TEAM_A, ALICE, "Alice");
    mutate(&mut claims);
    http.will_return(&id_token(&claims));
    get(&kit.router, &flow.callback("c"), &flow.cookies()).await
}

async fn callback_status(mutate: impl FnOnce(&mut Value)) -> StatusCode {
    callback_response(mutate).await.status
}

#[pollster::test]
async fn the_callback_refuses_an_id_token_it_cannot_trust() {
    assert_eq!(
        callback_status(|claims| claims["iss"] = json!("https://evil.example")).await,
        StatusCode::BAD_GATEWAY,
        "wrong issuer"
    );
    assert_eq!(
        callback_status(|claims| claims["aud"] = json!("another-client")).await,
        StatusCode::BAD_GATEWAY,
        "wrong audience"
    );
    assert_eq!(
        callback_status(|claims| claims["exp"] = json!(NOW - 1)).await,
        StatusCode::BAD_GATEWAY,
        "expired token"
    );
    assert_eq!(
        callback_status(|claims| claims["nonce"] = json!("some-other-nonce")).await,
        StatusCode::BAD_GATEWAY,
        "nonce from another attempt"
    );
    assert_eq!(
        callback_status(|claims| {
            claims
                .as_object_mut()
                .expect("an object")
                .remove("https://slack.com/team_id");
        })
        .await,
        StatusCode::BAD_GATEWAY,
        "no workspace claim"
    );
}

#[pollster::test]
async fn the_callback_surfaces_a_slack_refusal_or_a_broken_answer() {
    // A Slack error code is a plain snake_case token, so it is worth
    // repeating back: it is the difference between "try again" and "your
    // app is misconfigured".
    let refusal = callback_with_body(r#"{"ok":false,"error":"invalid_code"}"#).await;
    assert_eq!(refusal.status, StatusCode::BAD_GATEWAY);
    assert!(
        String::from_utf8_lossy(&refusal.body).contains("invalid_code"),
        "{}",
        String::from_utf8_lossy(&refusal.body)
    );

    // Anything else in that field is not ours to publish, and neither is
    // an upstream body that is not JSON at all.
    for body in [
        r#"{"ok":false,"error":"<script>alert(1)</script>"}"#,
        r#"{"ok":false,"error":"Invalid Code"}"#,
        "<html>gateway</html>",
    ] {
        let refusal = callback_with_body(body).await;
        assert_eq!(refusal.status, StatusCode::BAD_GATEWAY);
        let body = String::from_utf8_lossy(&refusal.body).into_owned();
        assert!(!body.contains("script"), "{body}");
        assert!(!body.contains("gateway"), "{body}");
    }
}

async fn callback_with_body(body: &str) -> Res {
    let http = TokenHttp::new();
    let kit = harness(&http);
    let flow = start_flow(&kit).await;
    http.will_answer(body);
    get(&kit.router, &flow.callback("c"), &flow.cookies()).await
}

#[pollster::test]
async fn a_deployment_without_slack_credentials_answers_503_not_a_panic() {
    let http = TokenHttp::new();
    let kit = harness_at(&http, MapConfig::default(), NOW);

    let start = get(&kit.router, "/v1/workspaces/slack/start", &[]).await;
    assert_eq!(start.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        String::from_utf8_lossy(&start.body).contains("WORKSPACES_SLACK_CLIENT_ID"),
        "the detail names the key"
    );

    let callback = get(
        &kit.router,
        "/v1/workspaces/slack/callback?code=c&state=s",
        &[],
    )
    .await;
    assert_eq!(callback.status, StatusCode::SERVICE_UNAVAILABLE);
}

// ---------------------------------------------------------------------------
// The seam the Events webhook (issue #6) will call

#[pollster::test]
async fn a_user_change_refreshes_the_name_timezone_and_admin_flag_only() {
    let http = TokenHttp::new();
    let kit = harness(&http);
    let alice = sign_in(&kit, &http, TEAM_A, ALICE, "Alice").await;

    let event = json!({
        "type": "user_change",
        "user": {
            "id": ALICE,
            "team_id": TEAM_A,
            "name": "alice",
            "real_name": "Alice Anderson",
            "tz": "Europe/Berlin",
            "is_admin": true,
            "deleted": false,
            "profile": {"real_name": "Alice Anderson", "display_name": "alice"},
        }
    });
    assert_eq!(apply_event(&kit, TEAM_A, &event).await, UserChange::Applied);

    let session = signed_session(&kit, &alice.workspace_id, ALICE, NOW + 3600);
    let me = get(&kit.router, ME, &[(SESSION_COOKIE, &session)])
        .await
        .json();
    assert_eq!(me["member"]["name"], "Alice Anderson");
    assert_eq!(me["member"]["timezone"], "Europe/Berlin");
    assert_eq!(me["member"]["is_admin"], true);
    assert_eq!(
        me["workspace"]["owner_id"], ALICE,
        "nothing about a user_change touches the owner"
    );
    assert_eq!(me["is_owner"], true);

    // A later event that drops back to a member and moves timezone again.
    let event = json!({
        "type": "user_change",
        "user": {"id": ALICE, "team_id": TEAM_A, "real_name": "Alice A.", "tz": "UTC", "is_admin": false}
    });
    assert_eq!(apply_event(&kit, TEAM_A, &event).await, UserChange::Applied);
    let me = get(&kit.router, ME, &[(SESSION_COOKIE, &session)])
        .await
        .json();
    assert_eq!(me["member"]["name"], "Alice A.");
    assert_eq!(me["member"]["timezone"], "UTC");
    assert_eq!(me["member"]["is_admin"], false);
}

#[pollster::test]
async fn a_user_change_leaves_fields_slack_omitted_alone() {
    let http = TokenHttp::new();
    let kit = harness(&http);
    let alice = sign_in(&kit, &http, TEAM_A, ALICE, "Alice").await;

    let first = json!({
        "type": "user_change",
        "user": {"id": ALICE, "team_id": TEAM_A, "real_name": "Alice Anderson", "tz": "Europe/Berlin", "is_admin": true}
    });
    apply_event(&kit, TEAM_A, &first).await;

    // No name, no tz, no admin flag: none of the three is clobbered.
    let second = json!({
        "type": "user_change",
        "user": {"id": ALICE, "team_id": TEAM_A}
    });
    assert_eq!(
        apply_event(&kit, TEAM_A, &second).await,
        UserChange::Applied
    );

    let session = signed_session(&kit, &alice.workspace_id, ALICE, NOW + 3600);
    let me = get(&kit.router, ME, &[(SESSION_COOKIE, &session)])
        .await
        .json();
    assert_eq!(me["member"]["name"], "Alice Anderson");
    assert_eq!(me["member"]["timezone"], "Europe/Berlin");
    assert_eq!(
        me["member"]["is_admin"], true,
        "an event that omits is_admin must not demote an admin"
    );
}

#[pollster::test]
async fn a_user_change_for_an_unknown_workspace_is_ignored() {
    let http = TokenHttp::new();
    let kit = harness(&http);
    let _alice = sign_in(&kit, &http, TEAM_A, ALICE, "Alice").await;

    let event = json!({
        "type": "user_change",
        "user": {"id": DAVE, "team_id": TEAM_B, "tz": "UTC", "is_admin": true}
    });
    assert_eq!(apply_event(&kit, TEAM_B, &event).await, UserChange::Ignored);

    // Nothing was conjured: signing in to B makes Dave its first owner,
    // and he is not an admin, so the ignored event left no trace.
    let http2 = TokenHttp::new();
    let kit2 = harness(&http2);
    let dave = sign_in(&kit2, &http2, TEAM_B, DAVE, "Dave").await;
    let me = get(&kit2.router, ME, &dave.cookies()).await.json();
    assert_eq!(me["is_owner"], true);
    assert_eq!(me["member"]["is_admin"], false);
}

#[pollster::test]
async fn a_user_change_naming_another_team_is_ignored() {
    let http = TokenHttp::new();
    let kit = harness(&http);
    let alice = sign_in(&kit, &http, TEAM_A, ALICE, "Alice").await;

    // The envelope names A, but the event body smuggles a B in — the kind
    // of mismatch a forged or replayed payload would carry. The body's
    // claim must lose to the verified envelope, so nothing is written.
    let event = json!({
        "type": "user_change",
        "user": {"id": DAVE, "team_id": TEAM_B, "real_name": "Dave", "tz": "UTC", "is_admin": true}
    });
    assert_eq!(apply_event(&kit, TEAM_A, &event).await, UserChange::Ignored);

    let rows = get(&kit.router, MEMBERS, &alice.cookies()).await.json();
    assert_eq!(user_ids(&rows), vec![ALICE], "Dave was not added to A");
    let me = get(&kit.router, ME, &alice.cookies()).await.json();
    assert_eq!(me["member"]["is_admin"], false, "Alice was not promoted");
}

#[pollster::test]
async fn events_that_are_not_user_changes_are_ignored() {
    let http = TokenHttp::new();
    let kit = harness(&http);

    for event in [
        json!({"type": "message", "user": {"id": ALICE, "team_id": TEAM_A}}),
        json!({"type": "user_change"}),
        json!({"type": "user_change", "user": {"id": ALICE}}),
        json!({"type": "user_change", "user": {"team_id": TEAM_A}}),
        json!({"no": "type"}),
    ] {
        assert_eq!(
            apply_event(&kit, TEAM_B, &event).await,
            UserChange::Ignored,
            "{event}"
        );
    }
}

#[pollster::test]
async fn a_user_change_can_add_a_member_the_workspace_already_has() {
    let http = TokenHttp::new();
    let kit = harness(&http);
    let alice = sign_in(&kit, &http, TEAM_A, ALICE, "Alice").await;

    // Bob has never signed in, but Slack says he is in the workspace.
    let event = json!({
        "type": "user_change",
        "user": {"id": BOB, "team_id": TEAM_A, "real_name": "Bob", "tz": "UTC", "is_admin": false}
    });
    assert_eq!(apply_event(&kit, TEAM_A, &event).await, UserChange::Applied);

    let rows = get(&kit.router, MEMBERS, &alice.cookies()).await.json();
    assert_eq!(user_ids(&rows), vec![ALICE, BOB]);
}

fn user_ids(rows: &Value) -> Vec<&str> {
    rows.as_array()
        .expect("an array")
        .iter()
        .filter_map(|row| row["user_id"].as_str())
        .collect()
}
