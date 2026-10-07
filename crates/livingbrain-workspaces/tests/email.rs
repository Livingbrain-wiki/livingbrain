//! Sign-in with an email magic link, and the workspace it creates without
//! Slack ever being involved (ADR 0002).
//!
//! The acceptance criteria this file answers: a person can create a
//! workspace with only an email address; the start route cannot be used to
//! find out who has one; the token is single use, expires, and is never
//! stored; and a workspace-scoped link signs in to the workspace it names
//! rather than minting a new one.

mod support;

use cratefield_core::axum::http::StatusCode;
use cratefield_core::{MapConfig, Row};
use cratefield_testing::{MailerMode, TestHarness};
use livingbrain_workspaces::{SESSION_COOKIE, link_identity};
use serde_json::json;
use sha2::{Digest, Sha256};
use support::*;

const START: &str = "/v1/workspaces/email/start";
const VERIFY: &str = "/v1/workspaces/email/verify";
const ME: &str = "/v1/workspaces/me";
const MEMBERS: &str = "/v1/workspaces/members";

const ALICE: &str = "alice@example.test";
const BOB: &str = "bob@example.test";
const STRANGER: &str = "nobody@example.test";
const TEAM: &str = "T0SCOPEDAA";
const ALICE_SLACK: &str = "U0ALICEAA";
const BOB_SLACK: &str = "U0BOBBBBB";

// ---------------------------------------------------------------------------
// Creating a workspace with only an email address

#[pollster::test]
async fn a_person_creates_a_workspace_with_only_an_email_address() {
    let http = TokenHttp::new();
    let kit = harness(&http);

    // Start. Nothing on this path reads the Slack settings, and the answer
    // is the same 202 every address gets.
    let start = post_json(&kit.router, START, &[], &json!({"email": ALICE})).await;
    assert_eq!(start.status, StatusCode::ACCEPTED);
    assert_eq!(start.json(), json!({"status": "accepted"}));

    // The link went to the address that asked for it.
    let message = kit.mailer.last_message().expect("a sign-in mail");
    assert_eq!(message.to, ALICE);
    assert!(
        message.from.contains("no-reply@"),
        "the module sends from its own address: {}",
        message.from
    );
    let token = mailed_token(&kit);
    assert!(
        token.chars().all(|c| c.is_ascii_hexdigit()),
        "the token is URL-safe text, safe in a query string unescaped: {token}"
    );
    // ADR 0002: the token is 32 random bytes, hex encoded into 64
    // characters — the module's 160 bits of `IdGen` entropy run through
    // SHA-256, which is also the same digest `sign_in_links` stores, so a
    // token can be recognised by its shape but not recovered from the row.
    assert_eq!(token.len(), 64, "32 bytes, hex encoded: {token}");
    assert_eq!(token.len() / 2, 32, "ADR 0002's 32 random bytes");

    // The GET renders the form a person submits, and spends nothing. That
    // is the whole reason it is a GET: mail scanners fetch every link in a
    // message on arrival, and spending here would leave the real recipient
    // a dead link.
    let form = get(&kit.router, &format!("{VERIFY}?token={token}"), &[]).await;
    assert_eq!(form.status, StatusCode::OK);
    let html = String::from_utf8_lossy(&form.body).into_owned();
    assert!(html.contains(r#"<form method="post""#), "{html}");
    assert!(
        html.contains(&format!(r#"action="{REDIRECT_BASE}{VERIFY}""#)),
        "{html}"
    );
    assert!(
        html.contains(&format!(r#"name="token" value="{token}""#)),
        "the form posts the token straight back: {html}"
    );

    let row = link_row(&kit, &token).await;
    assert_eq!(row.spent_at, None, "rendering the form spends nothing");
    assert_eq!(row.email, ALICE);
    assert_eq!(row.workspace_id, None, "no workspace named: it makes one");

    // The POST spends the link and seals the same session cookie Slack
    // seals, from the same signer and for the same reason.
    let session = verify(&kit, &token).await;

    // And that session is the owner of a workspace whose id is ours.
    let me = get(&kit.router, ME, &[(SESSION_COOKIE, &session)]).await;
    assert_eq!(me.status, StatusCode::OK);
    let body = me.json();
    let workspace_id = body["workspace"]["id"].as_str().expect("an id");
    assert!(workspace_id.starts_with("ws_"), "{workspace_id}");
    assert_ne!(workspace_id, ALICE, "an address is not a workspace id");
    assert_eq!(body["is_owner"], true);
    assert_eq!(
        body["workspace"]["owner_id"], body["member"]["user_id"],
        "the address that created the workspace owns it"
    );
    assert!(
        body["member"]["user_id"]
            .as_str()
            .expect("a member id")
            .starts_with("usr_"),
        "a member first seen by email gets a `usr_` id: {body}"
    );
    assert_eq!(
        body["workspace"]["name"], "alice",
        "the local part is the name a person would have typed"
    );
    assert_eq!(count_workspaces(&kit).await, 1);
}

#[pollster::test]
async fn a_second_workspace_from_the_same_address_is_created_and_the_first_keeps_the_connection() {
    let http = TokenHttp::new();
    let kit = harness(&http);

    // Alice creates a workspace with her address, twice. The second link is
    // minted and spent like any other: the start route answers every
    // address the same way, and nothing in it says a workspace already
    // exists.
    let first = verify(&kit, &mailed_after(&kit, ALICE).await).await;
    let second = verify(&kit, &mailed_after(&kit, ALICE).await).await;
    assert_ne!(first, second, "two links, two sessions");

    let one = get(&kit.router, ME, &[(SESSION_COOKIE, &first)])
        .await
        .json();
    let two = get(&kit.router, ME, &[(SESSION_COOKIE, &second)])
        .await
        .json();
    assert_ne!(one["workspace"]["id"], two["workspace"]["id"]);
    assert_ne!(one["member"]["user_id"], two["member"]["user_id"]);
    assert_eq!(one["is_owner"], true);
    assert_eq!(two["is_owner"], true);
    assert_eq!(count_workspaces(&kit).await, 2, "both workspaces exist");

    // `('email', address)` is unique across the whole table, so the first
    // workspace keeps it and the second gets no connection row at all.
    // That is not a failure: the address is written to
    // `member_identities` for *both* workspaces, and that — not the
    // connection — is what signs a person in.
    assert_eq!(
        first_value(
            &kit,
            "SELECT workspace_id AS value FROM workspace_connections \
             WHERE platform = 'email' AND external_id = ?",
            vec![text(ALICE)],
        )
        .await
        .as_deref(),
        Some(one["workspace"]["id"].as_str().expect("an id")),
        "the first workspace still holds the address"
    );
    for me in [&one, &two] {
        let workspace_id = me["workspace"]["id"].as_str().expect("an id");
        let owner_id = me["workspace"]["owner_id"].as_str().expect("an owner");
        assert_eq!(
            first_value(
                &kit,
                "SELECT user_id AS value FROM member_identities \
                 WHERE workspace_id = ? AND platform = 'email' AND external_id = ?",
                vec![text(workspace_id), text(ALICE)],
            )
            .await
            .as_deref(),
            Some(owner_id),
            "the address is an identity of that workspace's owner"
        );

        // And each workspace can still be reached by the address, which is
        // what the missing connection row must not have cost.
        let session = scoped_session(&kit, ALICE, workspace_id).await;
        let again = get(&kit.router, ME, &[(SESSION_COOKIE, &session)])
            .await
            .json();
        assert_eq!(again["workspace"]["id"], workspace_id);
        assert_eq!(again["member"]["user_id"], owner_id);
    }
}

#[pollster::test]
async fn a_workspace_can_be_named_when_the_link_is_minted() {
    let http = TokenHttp::new();
    let kit = harness(&http);

    post_json(
        &kit.router,
        START,
        &[],
        &json!({"email": ALICE, "workspace_name": "Deep Work"}),
    )
    .await;
    let session = verify(&kit, &mailed_token(&kit)).await;
    let me = get(&kit.router, ME, &[(SESSION_COOKIE, &session)])
        .await
        .json();
    assert_eq!(me["workspace"]["name"], "Deep Work");
}

#[pollster::test]
async fn the_address_is_normalized_before_it_is_stored() {
    let http = TokenHttp::new();
    let kit = harness(&http);

    post_json(
        &kit.router,
        START,
        &[],
        &json!({"email": "  Alice@Example.TEST "}),
    )
    .await;
    assert_eq!(kit.mailer.last_message().expect("a mail").to, ALICE);
    assert_eq!(link_row(&kit, &mailed_token(&kit)).await.email, ALICE);
}

#[pollster::test]
async fn a_string_that_is_not_an_address_is_refused() {
    let http = TokenHttp::new();
    let kit = harness(&http);

    // A 400 is a property of the string the caller sent, never of whether
    // the address belongs to anybody here.
    for address in ["not-an-address", "@example.test", "a@b@c.test", ""] {
        let start = post_json(&kit.router, START, &[], &json!({"email": address})).await;
        assert_eq!(start.status, StatusCode::BAD_REQUEST, "{address}");
    }
    assert!(
        kit.mailer.sent().is_empty(),
        "no link is minted for a string that is not an address"
    );
}

/// The address is checked before the deployment is: a typo is a 400 from a
/// deployment that could not have mailed it anyway, which is what the route's
/// comment claims and what the code now does.
#[pollster::test]
async fn a_malformed_address_is_a_400_even_when_this_deployment_cannot_mail() {
    let http = TokenHttp::new();
    // No mailer key and no public origin: every well-formed address is a
    // 503, and every deployment fault is a fact about the deployment, not
    // about the address — so the 400 below leaks nothing either.
    let kit = harness_at(&http, MapConfig::default(), NOW);
    kit.mailer.set_mode(MailerMode::NotConfigured);

    assert_eq!(
        post_json(&kit.router, START, &[], &json!({"email": "not-an-address"}))
            .await
            .status,
        StatusCode::BAD_REQUEST,
        "the string is the answer, and it is answered first"
    );
    assert_eq!(
        post_json(&kit.router, START, &[], &json!({"email": ALICE}))
            .await
            .status,
        StatusCode::SERVICE_UNAVAILABLE,
        "a well-formed address still gets the deployment's own answer"
    );
}

// ---------------------------------------------------------------------------
// The token: hashed, single use, expiring

#[pollster::test]
async fn the_raw_token_is_never_stored() {
    let http = TokenHttp::new();
    let kit = harness(&http);

    post_json(&kit.router, START, &[], &json!({"email": ALICE})).await;
    let token = mailed_token(&kit);

    let row = link_row(&kit, &token).await;
    assert_eq!(
        row.token_hash,
        sha256_hex(&token),
        "what is stored is the SHA-256 of the token"
    );
    assert_ne!(row.token_hash, token, "the link itself is not in the table");
    assert_eq!(row.token_hash.len(), 64);
    assert!(
        row.token_hash
            .chars()
            .all(|c| c.is_ascii_digit() || (c.is_ascii_lowercase() && c.is_ascii_hexdigit())),
        "64 lower-case hex characters: {}",
        row.token_hash
    );
    assert!(
        !row.everything.contains(&token),
        "no column of the row holds the token: {}",
        row.everything
    );
}

#[pollster::test]
async fn a_sign_in_link_works_once_and_only_once() {
    let http = TokenHttp::new();
    let kit = harness(&http);

    post_json(&kit.router, START, &[], &json!({"email": ALICE})).await;
    let token = mailed_token(&kit);
    verify(&kit, &token).await;
    assert!(
        link_row(&kit, &token).await.spent_at.is_some(),
        "the row is marked spent by the caller that spent it"
    );

    // The second POST spends nothing: the UPDATE only matches an unspent
    // row, so a link cannot be walked twice even if two requests race it.
    let again = post_form(&kit.router, VERIFY, &[], &format!("token={token}")).await;
    assert_eq!(again.status, StatusCode::BAD_REQUEST);
    assert!(again.cookie(SESSION_COOKIE).is_none(), "no second session");
    assert_eq!(count_workspaces(&kit).await, 1, "and no second workspace");
}

#[pollster::test]
async fn a_token_nobody_holds_is_refused() {
    let http = TokenHttp::new();
    let kit = harness(&http);

    post_json(&kit.router, START, &[], &json!({"email": ALICE})).await;
    let real = mailed_token(&kit);

    // A token that was never minted, and two that are a character off a
    // real one. The replacement character is chosen to *differ*: a real
    // token whose first or last hex character already is the replacement
    // would leave this loop signing in with the very token it means to
    // be refusing.
    let off = |at: usize| {
        let replacement = if real.as_bytes()[at] == b'0' {
            "1"
        } else {
            "0"
        };
        let mut token = real.clone();
        token.replace_range(at..at + 1, replacement);
        token
    };
    for token in ["0".repeat(64), off(63), off(0), String::new()] {
        let response = post_form(&kit.router, VERIFY, &[], &format!("token={token}")).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{token}");
        assert!(response.cookie(SESSION_COOKIE).is_none(), "{token}");
    }
    // The real one still works, so nothing above spent it.
    assert_eq!(
        post_form(&kit.router, VERIFY, &[], &format!("token={real}"))
            .await
            .status,
        StatusCode::FOUND
    );
}

#[pollster::test]
async fn an_expired_token_is_refused() {
    let http = TokenHttp::new();
    let kit = harness(&http);

    post_json(&kit.router, START, &[], &json!({"email": ALICE})).await;
    let token = mailed_token(&kit);
    assert!(
        link_row(&kit, &token).await.expires_at.as_str() > RFC3339_NOW,
        "a fresh link has its fifteen minutes"
    );

    // The kit's clock is fixed when the harness — and the database with it
    // — is built, so a link is aged by writing the timestamp the handler
    // compares against rather than by waiting for it.
    expire_link(&kit, &sha256_hex(&token)).await;

    let response = post_form(&kit.router, VERIFY, &[], &format!("token={token}")).await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(response.cookie(SESSION_COOKIE).is_none());
    assert!(
        link_row(&kit, &token).await.spent_at.is_some(),
        "an expired link is consumed too: forwarding it must not keep it alive"
    );
    assert_eq!(
        count_workspaces(&kit).await,
        0,
        "an expired link creates nothing"
    );
}

#[pollster::test]
async fn the_verify_form_renders_for_any_token_and_spends_none_of_them() {
    let http = TokenHttp::new();
    let kit = harness(&http);

    post_json(&kit.router, START, &[], &json!({"email": ALICE})).await;
    let real = mailed_token(&kit);

    for token in [real.clone(), "f".repeat(64), "not-a-token".to_owned()] {
        let form = get(&kit.router, &format!("{VERIFY}?token={token}"), &[]).await;
        assert_eq!(form.status, StatusCode::OK, "{token}");
        let html = String::from_utf8_lossy(&form.body).into_owned();
        assert!(html.contains(r#"<form method="post""#), "{token}: {html}");
    }

    // A link no address holds renders the same form, because the GET does
    // not look anything up: what it would learn is whether a hash it was
    // just given is in the table, and the POST is where the token is
    // checked. Only a request with no token at all gets a page with
    // nothing on it.
    let bare = get(&kit.router, VERIFY, &[]).await;
    assert_eq!(bare.status, StatusCode::OK);
    let html = String::from_utf8_lossy(&bare.body).into_owned();
    assert!(html.contains("<html"), "{html}");
    assert!(!html.contains("<form"), "nothing to submit: {html}");

    // The real link is untouched by all of that, and still works.
    assert_eq!(link_row(&kit, &real).await.spent_at, None);
    verify(&kit, &real).await;
}

// ---------------------------------------------------------------------------
// No enumeration, and no mailer

#[pollster::test]
async fn the_start_route_answers_every_address_the_same_way() {
    let http = TokenHttp::new();
    let kit = harness(&http);

    // An address that owns a workspace already.
    post_json(&kit.router, START, &[], &json!({"email": ALICE})).await;
    verify(&kit, &mailed_token(&kit)).await;

    // A different answer for a known address is an enumeration oracle, and
    // this route mails any address that asks for a link.
    let known = post_json(&kit.router, START, &[], &json!({"email": ALICE})).await;
    let unknown = post_json(&kit.router, START, &[], &json!({"email": STRANGER})).await;
    assert_eq!(known.status, StatusCode::ACCEPTED);
    assert_eq!(unknown.status, StatusCode::ACCEPTED);
    assert_eq!(known.body, unknown.body, "byte for byte, not merely alike");

    // Nor does naming a workspace leak: a link for a workspace the address
    // cannot enter is the same 202, and both are refusals at the far end.
    let scoped = post_json(
        &kit.router,
        START,
        &[],
        &json!({"email": STRANGER, "workspace_id": "ws_NOTAMEMBER00"}),
    )
    .await;
    assert_eq!(scoped.status, StatusCode::ACCEPTED);
    assert_eq!(scoped.body, unknown.body);
    assert_eq!(
        post_form(
            &kit.router,
            VERIFY,
            &[],
            &format!("token={}", mailed_token(&kit))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(count_workspaces(&kit).await, 1, "and nothing was claimed");
}

#[pollster::test]
async fn email_sign_in_answers_503_without_a_mailer() {
    let http = TokenHttp::new();
    let kit = harness(&http);
    kit.mailer.set_mode(MailerMode::NotConfigured);

    // A mailer that is wired but has no provider key sends nothing, and a
    // 202 here would be a lie: the caller would wait for a mail that is
    // never coming.
    let start = post_json(&kit.router, START, &[], &json!({"email": ALICE})).await;
    assert_eq!(start.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        start.json()["type"]
            .as_str()
            .expect("a problem type")
            .ends_with("workspaces/mail-not-configured"),
        "{}",
        String::from_utf8_lossy(&start.body)
    );
    assert!(
        kit.mailer.sent().is_empty(),
        "nothing was sent, so nothing was promised"
    );

    // With one, the very same request works.
    kit.mailer.set_mode(MailerMode::SendOk);
    assert_eq!(
        post_json(&kit.router, START, &[], &json!({"email": ALICE}))
            .await
            .status,
        StatusCode::ACCEPTED
    );
}

#[pollster::test]
async fn email_sign_in_answers_503_without_a_public_origin() {
    let http = TokenHttp::new();
    // A deployment that signs people in by email alone configures no Slack
    // app, and still has to say where its links point: a relative link in a
    // mail is a dead link, so it is refused rather than guessed.
    let kit = harness_at(
        &http,
        MapConfig::from_pairs([("WORKSPACES_REDIRECT_BASE", "brain.example")]),
        NOW,
    );

    let start = post_json(&kit.router, START, &[], &json!({"email": ALICE})).await;
    assert_eq!(start.status, StatusCode::SERVICE_UNAVAILABLE);
    let body = String::from_utf8_lossy(&start.body).into_owned();
    assert!(
        body.contains("WORKSPACES_REDIRECT_BASE"),
        "the detail names the key: {body}"
    );
    // Nothing about that deployment can sign in with Slack either, and
    // neither route reached Slack to find out.
    assert!(
        http.requests().is_empty(),
        "the email routes work with no Slack credentials at all"
    );
}

#[pollster::test]
async fn a_mail_provider_that_refuses_is_a_502_and_never_a_202() {
    let http = TokenHttp::new();
    let kit = harness(&http);
    kit.mailer.set_mode(MailerMode::Fail);

    let start = post_json(&kit.router, START, &[], &json!({"email": ALICE})).await;
    assert_eq!(start.status, StatusCode::BAD_GATEWAY);
    let body = String::from_utf8_lossy(&start.body);
    assert!(
        !body.contains(ALICE),
        "the detail must not quote the address"
    );
}

// ---------------------------------------------------------------------------
// A link scoped to a workspace the address is already a member of

#[pollster::test]
async fn a_workspace_scoped_link_signs_in_to_the_existing_workspace() {
    let http = TokenHttp::new();
    let kit = harness(&http);

    // A Slack-created workspace, with the address attached to one of its
    // members the way the linking route attaches a Slack user id.
    let alice = sign_in(&kit, &http, TEAM, ALICE_SLACK, "Alice").await;
    sign_in(&kit, &http, TEAM, BOB_SLACK, "Bob").await;
    link_identity(
        &*kit.db,
        &alice.workspace_id,
        "email",
        BOB,
        BOB_SLACK,
        RFC3339_NOW,
    )
    .await
    .expect("the address is an identity of Bob");

    // The link names the workspace, and signs Bob into it: the same
    // workspace, the same member row, and no second tenant.
    let start = post_json(
        &kit.router,
        START,
        &[],
        &json!({"email": BOB, "workspace_id": alice.workspace_id}),
    )
    .await;
    assert_eq!(start.status, StatusCode::ACCEPTED);
    let session = verify(&kit, &mailed_token(&kit)).await;

    let me = get(&kit.router, ME, &[(SESSION_COOKIE, &session)]).await;
    assert_eq!(me.status, StatusCode::OK);
    let body = me.json();
    assert_eq!(
        body["workspace"]["id"], alice.workspace_id,
        "a scoped link signs in, it does not mint a workspace"
    );
    assert_eq!(body["member"]["user_id"], BOB_SLACK);
    assert_eq!(body["is_owner"], false, "Alice still owns it");
    assert_eq!(count_workspaces(&kit).await, 1, "nothing new was created");

    let members = get(&kit.router, MEMBERS, &[(SESSION_COOKIE, &session)])
        .await
        .json();
    assert_eq!(members.as_array().expect("an array").len(), 2);
}

#[pollster::test]
async fn a_workspace_scoped_link_for_a_stranger_is_refused() {
    let http = TokenHttp::new();
    let kit = harness(&http);

    let alice = sign_in(&kit, &http, TEAM, ALICE_SLACK, "Alice").await;
    post_json(
        &kit.router,
        START,
        &[],
        &json!({"email": STRANGER, "workspace_id": alice.workspace_id}),
    )
    .await;
    let token = mailed_token(&kit);

    // A magic link is a way *into* a workspace you belong to, not a way to
    // claim one you do not. The answer is a 400, the same shape an expired
    // link gets, so somebody who did not receive the mail learns nothing
    // about who has accounts here.
    let refused = post_form(&kit.router, VERIFY, &[], &format!("token={token}")).await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    assert!(refused.cookie(SESSION_COOKIE).is_none());
    assert_eq!(count_workspaces(&kit).await, 1, "no workspace was claimed");

    let body = String::from_utf8_lossy(&refused.body).into_owned();
    assert!(
        !body.contains(STRANGER),
        "the address is not echoed: {body}"
    );
    assert!(!body.contains(&alice.workspace_id), "{body}");

    // The owner of a workspace can sign in to it by email the same way,
    // once her address is an identity of her own member row.
    link_identity(
        &*kit.db,
        &alice.workspace_id,
        "email",
        ALICE,
        ALICE_SLACK,
        RFC3339_NOW,
    )
    .await
    .expect("Alice's address is an identity of her own member row");
    post_json(
        &kit.router,
        START,
        &[],
        &json!({"email": ALICE, "workspace_id": alice.workspace_id}),
    )
    .await;
    let owner = verify(&kit, &mailed_token(&kit)).await;
    let me = get(&kit.router, ME, &[(SESSION_COOKIE, &owner)])
        .await
        .json();
    assert_eq!(me["workspace"]["id"], alice.workspace_id);
    assert_eq!(me["is_owner"], true);
    assert_eq!(me["member"]["user_id"], ALICE_SLACK);
}

// ---------------------------------------------------------------------------
// Helpers

/// Spends `token` and returns the session cookie it sealed.
async fn verify(kit: &TestHarness, token: &str) -> String {
    let response = post_form(&kit.router, VERIFY, &[], &format!("token={token}")).await;
    assert_eq!(
        response.status,
        StatusCode::FOUND,
        "{}",
        String::from_utf8_lossy(&response.body)
    );
    response
        .cookie(SESSION_COOKIE)
        .expect("the verify POST sets a session cookie")
}

/// One column of the first row a statement returns — the shape this file's
/// other queries want. (`support::column` is shadowed here by the
/// [`column`] that reads a [`Row`] the module fetched.)
async fn first_value(
    kit: &TestHarness,
    sql: &str,
    values: Vec<sea_query::Value>,
) -> Option<String> {
    rows(kit, sql, values)
        .await
        .first()
        .map(|row| column(row, "value"))
}

/// Asks for a link for `address` and returns the token it was mailed with.
async fn mailed_after(kit: &TestHarness, address: &str) -> String {
    post_json(&kit.router, START, &[], &json!({"email": address})).await;
    mailed_token(kit)
}

/// A link scoped to a workspace `address` is already an identity of, spent
/// for the session it seals.
async fn scoped_session(kit: &TestHarness, address: &str, workspace_id: &str) -> String {
    let start = post_json(
        &kit.router,
        START,
        &[],
        &json!({"email": address, "workspace_id": workspace_id}),
    )
    .await;
    assert_eq!(start.status, StatusCode::ACCEPTED);
    verify(kit, &mailed_token(kit)).await
}

/// The `sign_in_links` row for `token`, read by the hash of the token.
async fn link_row(kit: &TestHarness, token: &str) -> StoredLink {
    let row = sign_in_link(kit, &sha256_hex(token))
        .await
        .expect("a row for the link that was just minted");
    StoredLink {
        token_hash: column(&row, "token_hash"),
        email: column(&row, "email"),
        workspace_id: row
            .get::<Option<String>>("workspace_id")
            .unwrap_or_default(),
        expires_at: column(&row, "expires_at"),
        spent_at: row.get::<Option<String>>("spent_at").unwrap_or_default(),
        everything: format!("{row:?}"),
    }
}

fn column(row: &Row, name: &str) -> String {
    row.get::<String>(name).unwrap_or_default()
}

/// One `sign_in_links` row, as the columns worth asserting on.
struct StoredLink {
    token_hash: String,
    email: String,
    workspace_id: Option<String>,
    expires_at: String,
    spent_at: Option<String>,
    /// Every value in the row rendered, so a test can assert that no column
    /// of it holds the token.
    everything: String,
}

async fn count_workspaces(kit: &TestHarness) -> u64 {
    u64::try_from(rows(kit, "SELECT id FROM workspaces", vec![]).await.len()).expect("a count")
}

fn sha256_hex(value: &str) -> String {
    Sha256::digest(value.as_bytes())
        .iter()
        .fold(String::new(), |mut out, byte| {
            use std::fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
            out
        })
}
