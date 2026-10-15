//! Members, roles, invitations and the workspace switcher (ADR 0004,
//! phase a).
//!
//! What this file holds the module to: only owners and admins invite, re-role
//! and remove; only owners touch owners; a workspace always keeps an owner;
//! an invitation is the one way an address that is nobody here becomes a
//! member; a removed member's session, magic links and identities stop
//! working at once; every new cookie write refuses another site; and a
//! person reaches another workspace only through an address they sign in
//! with in both.

mod support;

use std::sync::{Arc, Mutex};

use cratefield_core::axum::body::Body;
use cratefield_core::axum::http::{Method, Request, StatusCode, header};
use cratefield_core::{Config, Database};
use cratefield_testing::TestHarness;
use livingbrain_workspaces::{MemberRemoval, SESSION_COOKIE, Workspaces};
use serde_json::{Value, json};
use support::*;
use time::OffsetDateTime;

const START: &str = "/v1/workspaces/email/start";
const VERIFY: &str = "/v1/workspaces/email/verify";
const ME: &str = "/v1/workspaces/me";
const MEMBERS: &str = "/v1/workspaces/members";
const INVITES: &str = "/v1/workspaces/invites";

const ALICE: &str = "alice@example.test";
const BOB: &str = "bob@example.test";
const CAROL: &str = "carol@example.test";

/// The removals the composition was told about.
#[derive(Clone, Default)]
#[allow(clippy::disallowed_types)]
struct Removed(Arc<Mutex<Vec<(String, String)>>>);

#[async_trait::async_trait]
impl MemberRemoval for Removed {
    async fn removed(
        &self,
        _db: &dyn Database,
        _now: OffsetDateTime,
        workspace_id: &str,
        user_id: &str,
    ) {
        self.0
            .lock()
            .expect("not poisoned")
            .push((workspace_id.to_owned(), user_id.to_owned()));
    }
}

fn kit_with(removed: &Removed) -> TestHarness {
    let config: Arc<dyn Config> = Arc::new(config());
    let http = TokenHttp::new();
    let removed = removed.clone();
    TestHarness::with_ports(
        vec![Box::new(
            Workspaces::new().on_member_removed(Arc::new(removed)),
        )],
        move |ports| {
            ports.config = config;
            ports.http = Some(Arc::new(http));
            ports.clock = Some(Arc::new(clock()));
        },
    )
}

/// A request with a session cookie, an optional JSON body and optional
/// extra headers.
async fn call(
    kit: &TestHarness,
    method: Method,
    path: &str,
    session: &str,
    body: Option<&Value>,
    extra: &[(&str, &str)],
) -> Res {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::COOKIE, format!("{SESSION_COOKIE}={session}"));
    for (name, value) in extra {
        builder = builder.header(*name, *value);
    }
    let body = match body {
        Some(json) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(json.to_string())
        }
        None => Body::empty(),
    };
    respond(&kit.router, builder.body(body).expect("request builds")).await
}

/// Spends `token` and returns the session it sealed.
async fn verify(kit: &TestHarness, token: &str) -> String {
    let response = post_form(&kit.router, VERIFY, &[], &format!("token={token}")).await;
    assert_eq!(
        response.status,
        StatusCode::FOUND,
        "{}",
        String::from_utf8_lossy(&response.body)
    );
    response.cookie(SESSION_COOKIE).expect("a session cookie")
}

/// Creates a workspace from `address` and returns its owner's session and
/// the workspace id.
async fn create_workspace(kit: &TestHarness, address: &str, name: &str) -> (String, String) {
    let start = post_json(
        &kit.router,
        START,
        &[],
        &json!({"email": address, "workspace_name": name}),
    )
    .await;
    assert_eq!(start.status, StatusCode::ACCEPTED);
    let session = verify(kit, &mailed_token(kit)).await;
    let me = get(&kit.router, ME, &[(SESSION_COOKIE, &session)])
        .await
        .json();
    let workspace_id = me["workspace"]["id"].as_str().expect("an id").to_owned();
    (session, workspace_id)
}

/// `owner` invites `address` as `role`, and `address` accepts. Returns the
/// new member's session and id.
async fn invite_and_accept(
    kit: &TestHarness,
    owner: &str,
    address: &str,
    role: &str,
) -> (String, String) {
    let sent = call(
        kit,
        Method::POST,
        INVITES,
        owner,
        Some(&json!({"email": address, "role": role})),
        &[],
    )
    .await;
    assert_eq!(
        sent.status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&sent.body)
    );
    let session = verify(kit, &mailed_token(kit)).await;
    let me = get(&kit.router, ME, &[(SESSION_COOKIE, &session)])
        .await
        .json();
    (
        session,
        me["member"]["user_id"].as_str().expect("an id").to_owned(),
    )
}

fn role_of(members: &Value, user_id: &str) -> Option<String> {
    members.as_array().and_then(|rows| {
        rows.iter()
            .find(|row| row["user_id"] == user_id)
            .map(|row| row["role"].as_str().unwrap_or_default().to_owned())
    })
}

#[pollster::test]
async fn an_invited_address_joins_with_the_role_it_was_invited_with() {
    let removed = Removed::default();
    let kit = kit_with(&removed);
    let (alice, workspace_id) = create_workspace(&kit, ALICE, "Acme").await;

    let me = get(&kit.router, ME, &[(SESSION_COOKIE, &alice)])
        .await
        .json();
    assert_eq!(me["member"]["role"], "owner", "the creator owns it: {me}");
    assert_eq!(me["is_owner"], true);

    let sent = call(
        &kit,
        Method::POST,
        INVITES,
        &alice,
        Some(&json!({"email": BOB, "role": "admin"})),
        &[],
    )
    .await;
    assert_eq!(sent.status, StatusCode::CREATED);
    let invite = sent.json();
    assert_eq!(invite["email"], BOB);
    assert_eq!(invite["role"], "admin");
    assert!(
        invite.get("token").is_none(),
        "the token is mailed, never returned"
    );

    let message = kit.mailer.last_message().expect("an invitation mail");
    assert_eq!(message.to, BOB);
    assert!(message.text.contains("Acme"), "{}", message.text);
    assert!(message.text.contains("7 days"), "{}", message.text);

    let open = call(&kit, Method::GET, INVITES, &alice, None, &[]).await;
    assert_eq!(open.status, StatusCode::OK);
    assert_eq!(open.json().as_array().map(Vec::len), Some(1));

    let bob = verify(&kit, &mailed_token(&kit)).await;
    let me = get(&kit.router, ME, &[(SESSION_COOKIE, &bob)]).await.json();
    assert_eq!(me["workspace"]["id"], workspace_id, "joined, not created");
    assert_eq!(me["member"]["role"], "admin");
    assert_eq!(me["is_owner"], false);
    let bob_id = me["member"]["user_id"].as_str().expect("an id").to_owned();
    assert!(bob_id.starts_with("usr_"), "{bob_id}");

    let members = get(&kit.router, MEMBERS, &[(SESSION_COOKIE, &alice)])
        .await
        .json();
    assert_eq!(role_of(&members, &bob_id).as_deref(), Some("admin"));
    let open = call(&kit, Method::GET, INVITES, &alice, None, &[]).await;
    assert_eq!(
        open.json().as_array().map(Vec::len),
        Some(0),
        "accepted is no longer open"
    );
}

#[pollster::test]
async fn a_lost_invitation_mail_is_replaced_by_an_ordinary_link_while_it_is_open() {
    let kit = kit_with(&Removed::default());
    let (alice, workspace_id) = create_workspace(&kit, ALICE, "Acme").await;
    let sent = call(
        &kit,
        Method::POST,
        INVITES,
        &alice,
        Some(&json!({"email": BOB})),
        &[],
    )
    .await;
    assert_eq!(sent.status, StatusCode::CREATED);
    assert_eq!(sent.json()["role"], "member", "member is the default role");
    // Bob never got the first mail; he asks for a link to the workspace.
    post_json(
        &kit.router,
        START,
        &[],
        &json!({"email": BOB, "workspace_id": workspace_id}),
    )
    .await;
    let bob = verify(&kit, &mailed_token(&kit)).await;
    let me = get(&kit.router, ME, &[(SESSION_COOKIE, &bob)]).await.json();
    assert_eq!(me["workspace"]["id"], workspace_id);
    assert_eq!(me["member"]["role"], "member");
}

#[pollster::test]
async fn an_address_without_an_invitation_still_cannot_join() {
    let kit = kit_with(&Removed::default());
    let (_alice, workspace_id) = create_workspace(&kit, ALICE, "Acme").await;
    post_json(
        &kit.router,
        START,
        &[],
        &json!({"email": CAROL, "workspace_id": workspace_id}),
    )
    .await;
    let refused = post_form(
        &kit.router,
        VERIFY,
        &[],
        &format!("token={}", mailed_token(&kit)),
    )
    .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
}

#[pollster::test]
async fn only_owners_and_admins_manage_and_only_owners_touch_owners() {
    let kit = kit_with(&Removed::default());
    let (alice, _) = create_workspace(&kit, ALICE, "Acme").await;
    let me = get(&kit.router, ME, &[(SESSION_COOKIE, &alice)])
        .await
        .json();
    let alice_id = me["member"]["user_id"].as_str().expect("id").to_owned();
    let (bob, bob_id) = invite_and_accept(&kit, &alice, BOB, "admin").await;
    let (carol, carol_id) = invite_and_accept(&kit, &alice, CAROL, "member").await;

    // A member manages nobody.
    for (method, path, body) in [
        (
            Method::POST,
            INVITES.to_owned(),
            Some(json!({"email": "dave@example.test"})),
        ),
        (
            Method::PATCH,
            format!("{MEMBERS}/{bob_id}"),
            Some(json!({"role": "member"})),
        ),
        (Method::DELETE, format!("{MEMBERS}/{bob_id}"), None),
        (Method::GET, INVITES.to_owned(), None),
    ] {
        let refused = call(&kit, method.clone(), &path, &carol, body.as_ref(), &[]).await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN, "{method} {path}");
    }

    // An admin cannot make, unmake or remove an owner, nor invite one.
    for (method, path, body) in [
        (
            Method::PATCH,
            format!("{MEMBERS}/{carol_id}"),
            Some(json!({"role": "owner"})),
        ),
        (
            Method::PATCH,
            format!("{MEMBERS}/{alice_id}"),
            Some(json!({"role": "member"})),
        ),
        (Method::DELETE, format!("{MEMBERS}/{alice_id}"), None),
        (
            Method::POST,
            INVITES.to_owned(),
            Some(json!({"email": "dave@example.test", "role": "owner"})),
        ),
    ] {
        let refused = call(&kit, method.clone(), &path, &bob, body.as_ref(), &[]).await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN, "{method} {path}");
    }

    // An admin may change a member's role; an owner may make an owner.
    let changed = call(
        &kit,
        Method::PATCH,
        &format!("{MEMBERS}/{carol_id}"),
        &bob,
        Some(&json!({"role": "admin"})),
        &[],
    )
    .await;
    assert_eq!(changed.status, StatusCode::OK);
    assert_eq!(changed.json()["role"], "admin");
    let promoted = call(
        &kit,
        Method::PATCH,
        &format!("{MEMBERS}/{bob_id}"),
        &alice,
        Some(&json!({"role": "owner"})),
        &[],
    )
    .await;
    assert_eq!(promoted.status, StatusCode::OK);
    let me = get(&kit.router, ME, &[(SESSION_COOKIE, &bob)]).await.json();
    assert_eq!(me["is_owner"], true);
}

#[pollster::test]
async fn unknown_and_not_yet_supported_roles_are_refused() {
    let kit = kit_with(&Removed::default());
    let (alice, _) = create_workspace(&kit, ALICE, "Acme").await;
    for role in ["viewer", "superuser", ""] {
        let refused = call(
            &kit,
            Method::POST,
            INVITES,
            &alice,
            Some(&json!({"email": BOB, "role": role})),
            &[],
        )
        .await;
        assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY, "{role:?}");
    }
    let me = get(&kit.router, ME, &[(SESSION_COOKIE, &alice)])
        .await
        .json();
    let refused = call(
        &kit,
        Method::POST,
        INVITES,
        &alice,
        Some(&json!({"email": ALICE})),
        &[],
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::CONFLICT,
        "inviting a member is a conflict: {me}"
    );
}

#[pollster::test]
async fn a_workspace_always_keeps_an_owner() {
    let kit = kit_with(&Removed::default());
    let (alice, _) = create_workspace(&kit, ALICE, "Acme").await;
    let me = get(&kit.router, ME, &[(SESSION_COOKIE, &alice)])
        .await
        .json();
    let alice_id = me["member"]["user_id"].as_str().expect("id").to_owned();

    let demote = call(
        &kit,
        Method::PATCH,
        &format!("{MEMBERS}/{alice_id}"),
        &alice,
        Some(&json!({"role": "member"})),
        &[],
    )
    .await;
    assert_eq!(demote.status, StatusCode::CONFLICT, "the last owner stays");
    let leave = call(
        &kit,
        Method::DELETE,
        &format!("{MEMBERS}/{alice_id}"),
        &alice,
        None,
        &[],
    )
    .await;
    assert_eq!(
        leave.status,
        StatusCode::CONFLICT,
        "the last owner cannot leave"
    );

    let (_bob, bob_id) = invite_and_accept(&kit, &alice, BOB, "owner").await;
    let leave = call(
        &kit,
        Method::DELETE,
        &format!("{MEMBERS}/{alice_id}"),
        &alice,
        None,
        &[],
    )
    .await;
    assert_eq!(
        leave.status,
        StatusCode::NO_CONTENT,
        "another owner remains"
    );
    assert!(
        leave
            .set_cookies()
            .iter()
            .any(|cookie| cookie.starts_with(&format!("{SESSION_COOKIE}=;"))),
        "leaving clears this browser's session"
    );
    let owners = rows(
        &kit,
        "SELECT user_id FROM workspace_members WHERE role = 'owner' AND removed_at IS NULL",
        vec![],
    )
    .await;
    assert_eq!(owners.len(), 1);
    assert_eq!(
        owners[0].get::<String>("user_id").as_deref(),
        Some(bob_id.as_str())
    );
}

#[pollster::test]
async fn a_removed_member_is_signed_out_everywhere_and_cannot_sign_back_in() {
    let removed = Removed::default();
    let kit = kit_with(&removed);
    let (alice, workspace_id) = create_workspace(&kit, ALICE, "Acme").await;
    let (bob, bob_id) = invite_and_accept(&kit, &alice, BOB, "member").await;

    let gone = call(
        &kit,
        Method::DELETE,
        &format!("{MEMBERS}/{bob_id}"),
        &alice,
        None,
        &[],
    )
    .await;
    assert_eq!(gone.status, StatusCode::NO_CONTENT);
    assert_eq!(
        removed.0.lock().expect("not poisoned").as_slice(),
        &[(workspace_id.clone(), bob_id.clone())],
        "the composition is told, once"
    );

    // The session Bob already holds stops working on the next request.
    let me = get(&kit.router, ME, &[(SESSION_COOKIE, &bob)]).await;
    assert_eq!(me.status, StatusCode::UNAUTHORIZED);
    // He is gone from the roster.
    let members = get(&kit.router, MEMBERS, &[(SESSION_COOKIE, &alice)])
        .await
        .json();
    assert_eq!(role_of(&members, &bob_id), None);
    // A magic link to the workspace no longer signs him in.
    post_json(
        &kit.router,
        START,
        &[],
        &json!({"email": BOB, "workspace_id": workspace_id}),
    )
    .await;
    let refused = post_form(
        &kit.router,
        VERIFY,
        &[],
        &format!("token={}", mailed_token(&kit)),
    )
    .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    // A cookie sealed for his old id, by anybody holding the signer, is
    // still refused: the row is a tombstone, not a member.
    let forged = signed_session(&kit, &workspace_id, &bob_id, NOW + 3600);
    let me = get(&kit.router, ME, &[(SESSION_COOKIE, &forged)]).await;
    assert_eq!(me.status, StatusCode::UNAUTHORIZED);

    // Re-inviting him is an explicit act, and gives him a fresh member id.
    let (_bob_again, new_id) = invite_and_accept(&kit, &alice, BOB, "member").await;
    assert_ne!(new_id, bob_id);
}

#[pollster::test]
async fn a_withdrawn_invitation_link_stops_working() {
    let kit = kit_with(&Removed::default());
    let (alice, _) = create_workspace(&kit, ALICE, "Acme").await;
    let sent = call(
        &kit,
        Method::POST,
        INVITES,
        &alice,
        Some(&json!({"email": BOB})),
        &[],
    )
    .await;
    let id = sent.json()["id"].as_str().expect("an id").to_owned();
    let token = mailed_token(&kit);
    let withdrawn = call(
        &kit,
        Method::DELETE,
        &format!("{INVITES}/{id}"),
        &alice,
        None,
        &[],
    )
    .await;
    assert_eq!(withdrawn.status, StatusCode::NO_CONTENT);
    let refused = post_form(&kit.router, VERIFY, &[], &format!("token={token}")).await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    let again = call(
        &kit,
        Method::DELETE,
        &format!("{INVITES}/{id}"),
        &alice,
        None,
        &[],
    )
    .await;
    assert_eq!(again.status, StatusCode::NOT_FOUND);
}

#[pollster::test]
async fn every_new_cookie_write_refuses_another_site() {
    let kit = kit_with(&Removed::default());
    let (alice, _) = create_workspace(&kit, ALICE, "Acme").await;
    let (_bob, bob_id) = invite_and_accept(&kit, &alice, BOB, "member").await;
    let sent_before = kit.mailer.sent().len();
    for (method, path, body) in [
        (
            Method::POST,
            INVITES.to_owned(),
            Some(json!({"email": CAROL})),
        ),
        (
            Method::PATCH,
            format!("{MEMBERS}/{bob_id}"),
            Some(json!({"role": "admin"})),
        ),
        (Method::DELETE, format!("{MEMBERS}/{bob_id}"), None),
        (Method::DELETE, format!("{INVITES}/inv_x"), None),
        (
            Method::POST,
            "/v1/workspaces/switch".to_owned(),
            Some(json!({"workspace_id": "ws_x"})),
        ),
    ] {
        for extra in [
            [("sec-fetch-site", "cross-site"), ("host", "brain.example")],
            [
                ("origin", "https://evil.example"),
                ("host", "brain.example"),
            ],
        ] {
            let refused = call(&kit, method.clone(), &path, &alice, body.as_ref(), &extra).await;
            assert_eq!(
                refused.status,
                StatusCode::FORBIDDEN,
                "{method} {path} {extra:?}"
            );
        }
    }
    assert_eq!(kit.mailer.sent().len(), sent_before, "nothing was mailed");
    let members = get(&kit.router, MEMBERS, &[(SESSION_COOKIE, &alice)])
        .await
        .json();
    assert_eq!(role_of(&members, &bob_id).as_deref(), Some("member"));
}

#[pollster::test]
async fn a_person_switches_only_to_workspaces_their_address_belongs_to() {
    let kit = kit_with(&Removed::default());
    let (alice, home) = create_workspace(&kit, ALICE, "Home").await;
    let (bob, other) = create_workspace(&kit, BOB, "Other").await;
    let (_carol, unrelated) = create_workspace(&kit, CAROL, "Unrelated").await;
    // Bob invites Alice's address to his workspace, and she accepts there.
    invite_and_accept(&kit, &bob, ALICE, "member").await;

    let listed = call(&kit, Method::GET, "/v1/workspaces", &alice, None, &[]).await;
    assert_eq!(
        listed.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&listed.body)
    );
    let listed = listed.json();
    let ids: Vec<&str> = listed
        .as_array()
        .expect("a list")
        .iter()
        .filter_map(|item| item["id"].as_str())
        .collect();
    assert!(
        ids.contains(&home.as_str()) && ids.contains(&other.as_str()),
        "{listed}"
    );
    assert!(!ids.contains(&unrelated.as_str()), "{listed}");

    let refused = call(
        &kit,
        Method::POST,
        "/v1/workspaces/switch",
        &alice,
        Some(&json!({"workspace_id": unrelated})),
        &[],
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::NOT_FOUND,
        "naming a workspace is not enough"
    );

    let switched = call(
        &kit,
        Method::POST,
        "/v1/workspaces/switch",
        &alice,
        Some(&json!({"workspace_id": other})),
        &[],
    )
    .await;
    assert_eq!(switched.status, StatusCode::OK);
    let session = switched.cookie(SESSION_COOKIE).expect("a new session");
    let me = get(&kit.router, ME, &[(SESSION_COOKIE, &session)])
        .await
        .json();
    assert_eq!(me["workspace"]["id"], other);
    assert_eq!(me["member"]["role"], "member");
}

#[pollster::test]
async fn a_removed_slack_member_is_not_brought_back_by_slack() {
    let http = TokenHttp::new();
    let kit = harness(&http);
    let alice = sign_in(&kit, &http, "T0REMOVE", "U0ALICE", "Alice").await;
    let bob = sign_in(&kit, &http, "T0REMOVE", "U0BOB", "Bob").await;
    let me = get(&kit.router, ME, &bob.cookies()).await.json();
    assert_eq!(
        me["member"]["role"], "member",
        "a later Slack signer is a member"
    );
    let me = get(&kit.router, ME, &alice.cookies()).await.json();
    assert_eq!(
        me["member"]["role"], "owner",
        "the first Slack signer owns it"
    );

    let gone = call(
        &kit,
        Method::DELETE,
        &format!("{MEMBERS}/U0BOB"),
        &alice.cookie,
        None,
        &[],
    )
    .await;
    assert_eq!(gone.status, StatusCode::NO_CONTENT);

    // Signing in with Slack again lands on the tombstone, which no route
    // accepts.
    let again = sign_in(&kit, &http, "T0REMOVE", "U0BOB", "Bob").await;
    let me = get(&kit.router, ME, &again.cookies()).await;
    assert_eq!(me.status, StatusCode::UNAUTHORIZED);
    // Nor does the Slack member mirror revive him.
    let change = apply_event(
        &kit,
        "T0REMOVE",
        &json!({"type": "user_change", "user": {"id": "U0BOB", "name": "bob", "is_admin": true}}),
    )
    .await;
    assert_eq!(change, livingbrain_workspaces::UserChange::Applied);
    let me = get(&kit.router, ME, &again.cookies()).await;
    assert_eq!(me.status, StatusCode::UNAUTHORIZED);
    let members = get(&kit.router, MEMBERS, &alice.cookies()).await.json();
    assert_eq!(role_of(&members, "U0BOB"), None);
}
