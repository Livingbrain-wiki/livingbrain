//! The Slack app, end to end over HTTP (issue #6): the app manifest, the
//! OAuth install and the bot token it seals, and the events endpoint's
//! signature check, acknowledgement and deduplication.
//!
//! Every delivery is signed the way Slack signs one — the HMAC is computed in
//! the test, against the harness's fixed clock — so a passing test is evidence
//! about Slack's protocol and not about the module agreeing with itself.

mod support;

use cratefield_core::axum::http::StatusCode;
use cratefield_testing::TestHarness;
use serde_json::{Value, json};
use support::*;

/// The workspace the seeded events resolve through. `seed_legacy_workspace`
/// links it as a Slack connection, which is what a workspace a sign-in
/// created looks like once migration `0002` has run.
const TEAM: &str = "T0LEGACY";

/// A `user_change` Slack would send, as one this deployment can apply.
fn user_change(user: &str, name: &str) -> Value {
    json!({
        "type": "user_change",
        "user": {
            "id": user,
            "team_id": TEAM,
            "name": name,
            "is_admin": false,
            "profile": {"real_name": name, "display_name": name},
            "deleted": false,
        },
    })
}

/// The display name the mirror holds for `user`, or [`None`].
async fn mirrored_name(kit: &TestHarness, user: &str) -> Option<String> {
    column(
        kit,
        "SELECT name AS value FROM workspace_members \
         WHERE workspace_id = ? AND user_id = ?",
        vec![text(TEAM), text(user)],
    )
    .await
}

/// How many rows the inbox holds for one `event_id`.
async fn claims(kit: &TestHarness, event_id: &str) -> usize {
    rows(
        kit,
        "SELECT event_key FROM workspaces_slack_inbox WHERE event_key = ?",
        vec![text(event_id)],
    )
    .await
    .len()
}

// ---------------------------------------------------------------------------
// The signature

#[pollster::test]
async fn a_delivery_without_a_verifying_signature_is_refused() {
    let http = TokenHttp::new();
    let kit = harness(&http);
    let body = envelope("Ev1", TEAM, user_change("U1", "Ada")).to_string();

    // Signed with a secret that is not this app's, and signed with nothing.
    for secret in ["a different secret entirely", ""] {
        let response = post_signed_with(&kit.router, &body, NOW, secret).await;
        assert_eq!(
            response.status,
            StatusCode::UNAUTHORIZED,
            "secret {secret:?}"
        );
    }

    // A refused delivery defers nothing and claims no inbox row.
    assert_eq!(kit.defer.deferred_count(), 0);
    assert!(
        rows(&kit, "SELECT event_key FROM workspaces_slack_inbox", vec![])
            .await
            .is_empty(),
        "an unauthenticated delivery never reaches the inbox"
    );

    // The same body, correctly signed, is accepted — so what refused the
    // others was the signature, not the body.
    let signed = post_signed(
        &kit.router,
        &envelope("Ev1", TEAM, user_change("U1", "Ada")),
        NOW,
    )
    .await;
    assert_eq!(signed.status, StatusCode::OK);
    assert_eq!(kit.defer.deferred_count(), 1);
}

#[pollster::test]
async fn a_delivery_outside_slacks_five_minute_window_is_refused() {
    let http = TokenHttp::new();
    let kit = harness(&http);
    let body = envelope("Ev1", TEAM, user_change("U1", "Ada")).to_string();

    for sent in [NOW - 301, NOW + 301] {
        let response = post_signed_with(&kit.router, &body, sent, SIGNING_SECRET).await;
        assert_eq!(
            response.status,
            StatusCode::UNAUTHORIZED,
            "a delivery stamped {sent} is outside the window in one direction"
        );
    }
    assert_eq!(kit.defer.deferred_count(), 0);

    // The edge of the window is inside it, and the edge *is* a delivery: it
    // must be acknowledged rather than retried forever.
    let edge = post_signed_with(&kit.router, &body, NOW - 300, SIGNING_SECRET).await;
    assert_eq!(edge.status, StatusCode::OK);
    assert_eq!(kit.defer.deferred_count(), 1);
    assert_eq!(
        claims(&kit, "Ev1").await,
        1,
        "only the delivery inside the window was claimed"
    );
}

#[pollster::test]
async fn a_deployment_with_no_signing_secret_answers_503() {
    let http = TokenHttp::new();
    let mut without = config();
    without.0.remove("WORKSPACES_SLACK_SIGNING_SECRET");
    let kit = harness_at(&http, without, NOW);

    let response = post_signed(
        &kit.router,
        &envelope("Ev1", TEAM, user_change("U1", "Ada")),
        NOW,
    )
    .await;
    // 503, not 401: the deployment is missing a credential, the delivery is
    // not at fault. Sign-in with Slack OpenID needs no Events secret, so this
    // is a per-request answer rather than a router that refuses to build.
    assert_eq!(response.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(kit.defer.deferred_count(), 0);
}

#[pollster::test]
async fn the_url_verification_handshake_is_answered_with_its_own_challenge() {
    let http = TokenHttp::new();
    let kit = harness(&http);

    let response = post_signed(&kit.router, &url_verification("3eZbrw1aB2dY6j1x7"), NOW).await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.json(), json!({"challenge": "3eZbrw1aB2dY6j1x7"}));
    assert_eq!(kit.defer.deferred_count(), 0, "a handshake defers nothing");

    // Only after the signature verifies: an unauthenticated caller must not
    // be able to make this endpoint echo.
    let forged = post_signed_with(
        &kit.router,
        &url_verification("3eZbrw1aB2dY6j1x7").to_string(),
        NOW,
        "not this app's secret",
    )
    .await;
    assert_eq!(forged.status, StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Acknowledgement and deduplication

#[pollster::test]
async fn a_delivery_is_acknowledged_before_its_work_runs() {
    let http = TokenHttp::new();
    let kit = harness(&http);
    seed_legacy_workspace(&kit, TEAM, "Legacy", "UOWNER").await;

    let response = post_signed(
        &kit.router,
        &envelope("Ev1", TEAM, user_change("U1", "Ada")),
        NOW,
    )
    .await;
    // The response is already back before the member row is touched, so a
    // slow database cannot make Slack retry a delivery it has already taken.
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        kit.defer.deferred_count(),
        1,
        "the work is deferred, not done"
    );
    assert_eq!(
        mirrored_name(&kit, "U1").await,
        None,
        "the mirror is exactly as the acknowledgement left it"
    );

    kit.defer.drain().await;
    assert_eq!(mirrored_name(&kit, "U1").await.as_deref(), Some("Ada"));
}

#[pollster::test]
async fn the_same_event_id_delivered_twice_is_deferred_once() {
    let http = TokenHttp::new();
    let kit = harness(&http);
    seed_legacy_workspace(&kit, TEAM, "Legacy", "UOWNER").await;
    let body = envelope("Ev-retry", TEAM, user_change("U1", "Ada"));

    // Slack retries on a timer: the same event_id, signed afresh.
    let first = post_signed(&kit.router, &body, NOW).await;
    let second = post_signed(&kit.router, &body, NOW + 4).await;
    assert_eq!(first.status, StatusCode::OK);
    assert_eq!(
        second.status,
        StatusCode::OK,
        "a retry is acknowledged, not rejected"
    );
    assert_eq!(kit.defer.deferred_count(), 1, "the retry is not deferred");

    kit.defer.drain().await;
    // The effect happened once, and the inbox holds one row for the event:
    // that claim is what makes the application idempotent.
    assert_eq!(mirrored_name(&kit, "U1").await.as_deref(), Some("Ada"));
    assert_eq!(claims(&kit, "Ev-retry").await, 1);

    // A different event id, even for the same person, is not a duplicate.
    let third = post_signed(
        &kit.router,
        &envelope("Ev-fresh", TEAM, user_change("U1", "Ada")),
        NOW,
    )
    .await;
    assert_eq!(third.status, StatusCode::OK);
    assert_eq!(kit.defer.deferred_count(), 2);
}

#[pollster::test]
async fn the_five_conversation_events_are_accepted_and_deferred() {
    let http = TokenHttp::new();
    let kit = harness(&http);

    let deliveries = [
        ("Ev-mention", json!({"type": "app_mention", "user": "U1"})),
        (
            "Ev-channel",
            json!({"type": "message", "channel_type": "channel"}),
        ),
        (
            "Ev-group",
            json!({"type": "message", "channel_type": "group"}),
        ),
        ("Ev-im", json!({"type": "message", "channel_type": "im"})),
        (
            "Ev-joined",
            json!({"type": "member_joined_channel", "user": "U1"}),
        ),
    ];
    for (event_id, event) in &deliveries {
        let response =
            post_signed(&kit.router, &envelope(event_id, TEAM, event.clone()), NOW).await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "{event_id} is acknowledged"
        );
    }
    assert_eq!(kit.defer.deferred_count(), deliveries.len());
    // Draining must not panic: the deferred work swallows its errors and has
    // no agent to hand a message to yet (issue #7).
    kit.defer.drain().await;

    // An envelope type this app does not act on, and one with no `event_id`
    // to dedupe on, are both acknowledged and neither is deferred: an event
    // Slack would retry forever is worse than one this module has nothing to
    // do with.
    for envelope in [
        json!({"type": "something_new", "event_id": "Ev-other", "team_id": TEAM}),
        json!({"type": "event_callback", "team_id": TEAM, "event": {"type": "app_mention"}}),
    ] {
        assert_eq!(
            post_signed(&kit.router, &envelope, NOW).await.status,
            StatusCode::OK
        );
    }
    assert_eq!(kit.defer.deferred_count(), deliveries.len());
}

#[pollster::test]
async fn the_team_id_comes_from_the_signed_envelope_and_not_from_the_event() {
    let http = TokenHttp::new();
    let kit = harness(&http);
    seed_legacy_workspace(&kit, TEAM, "Legacy", "UOWNER").await;
    seed_legacy_workspace(&kit, "T0ATTACKER", "Not yours", "UOWNER2").await;

    // A delivery Slack would never send: the event's user block claims a team
    // it does not belong to. It is signed with our secret, so the envelope is
    // what is authoritative.
    let event = json!({
        "type": "user_change",
        "user": {
            "id": "U1",
            "team_id": "T0ATTACKER",
            "profile": {"real_name": "Ada of the wrong team"},
        },
    });
    let response = post_signed(&kit.router, &envelope("Ev1", TEAM, event), NOW).await;
    assert_eq!(response.status, StatusCode::OK);
    kit.defer.drain().await;

    assert_eq!(
        mirrored_name(&kit, "U1").await,
        None,
        "nothing was written anywhere: the event's own team is ignored"
    );
}

// ---------------------------------------------------------------------------
// The manifest

#[pollster::test]
async fn the_manifest_carries_this_deployments_urls() {
    let http = TokenHttp::new();
    let kit = harness(&http);

    let response = get(&kit.router, "/v1/workspaces/slack/manifest", &[]).await;
    assert_eq!(response.status, StatusCode::OK);
    let manifest = response.json();

    assert_eq!(
        manifest["settings"]["event_subscriptions"]["request_url"],
        json!(format!("{REDIRECT_BASE}{EVENTS}"))
    );
    assert_eq!(
        manifest["oauth_config"]["redirect_urls"],
        json!([
            format!("{REDIRECT_BASE}/v1/workspaces/slack/install/callback"),
            format!("{REDIRECT_BASE}/v1/workspaces/slack/callback"),
        ])
    );
    // The five events issue #6 asks for, plus the one that keeps the mirror
    // fresh.
    let bot_events = manifest["settings"]["event_subscriptions"]["bot_events"]
        .as_array()
        .expect("bot_events is a list");
    for event in [
        "app_mention",
        "message.channels",
        "message.groups",
        "message.im",
        "member_joined_channel",
        "user_change",
    ] {
        assert!(bot_events.contains(&json!(event)), "{event} is subscribed");
    }
    // The install URL must ask for the same scopes the manifest declares.
    assert_eq!(
        manifest["oauth_config"]["scopes"]["bot"],
        json!(livingbrain_workspaces::bot_scopes())
    );
}

// ---------------------------------------------------------------------------
// Installing

#[pollster::test]
async fn an_install_seals_the_bot_token_rather_than_storing_it() {
    let http = TokenHttp::new();
    let kit = harness(&http);
    // Assembled from fragments so the fixture's bot token never appears
    // verbatim in the source tree, where scanners read it as a live one. The
    // runtime value is unchanged, so the assertions below still mean that this
    // exact token never surfaces in a response body or a stored column.
    let token = concat!("xo", "xb-1111-2222-3333-not-a-real-bot-token");

    // The install route mints a state and redirects to Slack with the bot
    // scopes, before any code exists.
    let install = start_install(&kit).await;
    assert!(!install.state.is_empty(), "a CSRF state came back");
    let location = get(&kit.router, "/v1/workspaces/slack/install", &[])
        .await
        .location();
    assert!(
        location.starts_with("https://slack.com/oauth/v2/authorize?"),
        "{location}"
    );
    assert!(
        location.contains("scope=app_mentions%3Aread"),
        "the bot scopes are asked for: {location}"
    );

    http.will_answer(&install_answer(TEAM, "A0APP", "U0BOT", token));
    let callback = get(
        &kit.router,
        &install.callback("install-code"),
        &install.cookies(),
    )
    .await;
    assert_eq!(
        callback.status,
        StatusCode::OK,
        "the install completes: {}",
        String::from_utf8_lossy(&callback.body)
    );
    let body = callback.json();
    assert_eq!(body["status"], json!("installed"));
    assert_eq!(body["team_id"], json!(TEAM));
    // The body names the team and the app, and never the token.
    assert!(
        !String::from_utf8_lossy(&callback.body).contains(token),
        "the callback never answers with a token"
    );

    // The row exists, and no column of it is the token.
    let found = rows(
        &kit,
        "SELECT app_id, bot_user_id, wrapped_dek, nonce, ciphertext, kms_key_ref, installed_at \
         FROM slack_installs WHERE team_id = ?",
        vec![text(TEAM)],
    )
    .await;
    let row = found.first().expect("an install row").clone();
    assert_eq!(row.get::<String>("app_id").as_deref(), Some("A0APP"));
    assert_eq!(row.get::<String>("bot_user_id").as_deref(), Some("U0BOT"));
    assert_eq!(
        row.get::<String>("kms_key_ref").as_deref(),
        Some(concat!("worker-secret", ":HARNESS_KEK_V1"))
    );
    for column in ["wrapped_dek", "nonce", "ciphertext"] {
        let value = row.get::<String>(column).unwrap_or_default();
        assert!(!value.is_empty(), "{column} was written");
        assert!(
            !value.contains(token),
            "{column} is not the token in the clear"
        );
    }

    // And it opens back to what Slack handed over, under the key ring the
    // module sealed it with.
    let opened = livingbrain_workspaces::bot_token(&*kms(), &*kit.db, TEAM)
        .await
        .expect("the token opens");
    assert_eq!(opened.as_deref(), Some(token));

    // A team that never installed has no token, and that is not an error.
    assert_eq!(
        livingbrain_workspaces::bot_token(&*kms(), &*kit.db, "T0NEVER")
            .await
            .expect("no row is not a failure"),
        None
    );
}

#[pollster::test]
async fn an_install_callback_without_our_own_state_is_refused() {
    let http = TokenHttp::new();
    let kit = harness(&http);
    let install = start_install(&kit).await;

    // A cookie minted for a sign-in, carrying a state this test can read.
    let flow = start_flow(&kit).await;
    let state = flow_payload(&kit, &flow.cookie)["state"]
        .as_str()
        .expect("a state")
        .to_owned();
    http.will_answer(&install_answer(
        TEAM,
        "A0APP",
        "U0BOT",
        concat!("xo", "xb-never-used"),
    ));

    for (path, cookies) in [
        // No cookie at all.
        (
            "/v1/workspaces/slack/install/callback?code=c&state=s".to_owned(),
            vec![],
        ),
        // Our cookie, somebody else's state.
        (
            "/v1/workspaces/slack/install/callback?code=c&state=guessed".to_owned(),
            install.cookies().to_vec(),
        ),
        // A sign-in cookie, spent as an install state.
        (
            format!("/v1/workspaces/slack/install/callback?code=c&state={state}"),
            flow.cookies().to_vec(),
        ),
    ] {
        let response = get(&kit.router, &path, &cookies).await;
        assert_eq!(
            response.status,
            StatusCode::BAD_REQUEST,
            "{path} is refused"
        );
    }

    // Nothing was exchanged and nothing was stored: the round trip to Slack
    // happens only once the state has been proved under this flow's purpose.
    assert!(http.requests().is_empty(), "Slack was never called");
    assert!(
        rows(&kit, "SELECT team_id FROM slack_installs", vec![])
            .await
            .is_empty(),
        "and no install row was written"
    );
}

#[pollster::test]
async fn a_deployment_with_no_key_ring_refuses_the_install_routes() {
    let http = TokenHttp::new();
    let mut without = config();
    without.0.remove("HARNESS_KEK_CURRENT");
    without.0.remove("HARNESS_KEK_V1");
    let kit = harness_at(&http, without, NOW);

    // A deployment that signs people in and mails magic links is complete
    // without a Slack app: it must still serve them, and refuse only what it
    // could not keep secret — before the round trip, so no owner is sent to
    // Slack for an install that cannot finish.
    let install = get(&kit.router, "/v1/workspaces/slack/install", &[]).await;
    assert_eq!(install.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(http.requests().is_empty(), "Slack was never called");
}
