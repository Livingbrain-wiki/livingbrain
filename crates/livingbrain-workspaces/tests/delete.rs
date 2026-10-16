//! Deleting a workspace (ADR 0004, "Deleting a workspace").
//!
//! What this file holds the module to: only an owner deletes; the typed name
//! must match exactly; a person keeps a workspace to sign in to; the content
//! and credentials are erased before this module's rows go, and an erasure
//! that fails leaves the workspace whole; afterwards every session that named
//! the workspace is refused, its invitations and sign-in links are dead, and
//! the person's other workspace is untouched and reachable by email.

mod support;

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use cratefield_core::axum::http::{Method, StatusCode};
use cratefield_core::{Config, Database};
use cratefield_testing::TestHarness;
use livingbrain_workspaces::{ErasureFailed, SESSION_COOKIE, WorkspaceErasure, Workspaces};
use serde_json::{Value, json};
use support::*;
use time::OffsetDateTime;

const START: &str = "/v1/workspaces/email/start";
const VERIFY: &str = "/v1/workspaces/email/verify";
const ME: &str = "/v1/workspaces/me";
const WORKSPACES: &str = "/v1/workspaces";
const CURRENT: &str = "/v1/workspaces/current";
const INVITES: &str = "/v1/workspaces/invites";

const ALICE: &str = "alice@example.test";
const BOB: &str = "bob@example.test";
const CAROL: &str = "carol@example.test";
const DAN: &str = "dan@example.test";

/// What the composition was asked to erase: the workspace and its spaces.
/// The erasures asked for: the workspace id and the spaces it was asked for.
type Calls = Arc<Mutex<Vec<(String, Vec<String>)>>>;

#[derive(Clone, Default)]
#[allow(clippy::disallowed_types)]
struct Erasure {
    calls: Calls,
    fail: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl WorkspaceErasure for Erasure {
    async fn erase(
        &self,
        _db: &dyn Database,
        _now: OffsetDateTime,
        workspace_id: &str,
        spaces: &[String],
    ) -> Result<(), ErasureFailed> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(ErasureFailed);
        }
        self.calls
            .lock()
            .expect("not poisoned")
            .push((workspace_id.to_owned(), spaces.to_vec()));
        Ok(())
    }
}

fn kit_with(erasure: &Erasure) -> TestHarness {
    let config: Arc<dyn Config> = Arc::new(config());
    let http = TokenHttp::new();
    let erasure = erasure.clone();
    TestHarness::with_ports(
        vec![Box::new(
            Workspaces::new().on_workspace_erasure(Arc::new(erasure)),
        )],
        move |ports| {
            ports.config = config;
            ports.http = Some(Arc::new(http));
            ports.clock = Some(Arc::new(clock()));
        },
    )
}

async fn call(
    kit: &TestHarness,
    method: Method,
    path: &str,
    session: &str,
    body: Option<&Value>,
) -> Res {
    let mut builder = cratefield_core::axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header(
            cratefield_core::axum::http::header::COOKIE,
            format!("{SESSION_COOKIE}={session}"),
        );
    let body = match body {
        Some(json) => {
            builder = builder.header(
                cratefield_core::axum::http::header::CONTENT_TYPE,
                "application/json",
            );
            cratefield_core::axum::body::Body::from(json.to_string())
        }
        None => cratefield_core::axum::body::Body::empty(),
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

/// Signs `address` in with a fresh link and returns the session it gets.
async fn sign_in(kit: &TestHarness, address: &str) -> String {
    let start = post_json(&kit.router, START, &[], &json!({ "email": address })).await;
    assert_eq!(start.status, StatusCode::ACCEPTED);
    verify(kit, &mailed_token(kit)).await
}

async fn me(kit: &TestHarness, session: &str) -> Res {
    get(&kit.router, ME, &[(SESSION_COOKIE, session)]).await
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
    let id = me(kit, &session).await.json()["workspace"]["id"]
        .as_str()
        .expect("an id")
        .to_owned();
    (session, id)
}

/// `owner` creates a second workspace from its own email identity, and is
/// moved into it: returns that session and the new id.
async fn second_workspace(kit: &TestHarness, owner: &str, name: &str) -> (String, String) {
    let created = call(
        kit,
        Method::POST,
        WORKSPACES,
        owner,
        Some(&json!({ "name": name })),
    )
    .await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&created.body)
    );
    let session = created.cookie(SESSION_COOKIE).expect("the new session");
    let id = created.json()["id"].as_str().expect("an id").to_owned();
    (session, id)
}

/// `owner` invites `address` as `role`, and `address` accepts. Returns the
/// new member's session.
async fn invite_and_accept(kit: &TestHarness, owner: &str, address: &str, role: &str) -> String {
    let sent = call(
        kit,
        Method::POST,
        INVITES,
        owner,
        Some(&json!({"email": address, "role": role})),
    )
    .await;
    assert_eq!(sent.status, StatusCode::CREATED);
    verify(kit, &mailed_token(kit)).await
}

async fn delete_current(kit: &TestHarness, session: &str, confirm: &str) -> Res {
    call(
        kit,
        Method::DELETE,
        CURRENT,
        session,
        Some(&json!({ "confirm": confirm })),
    )
    .await
}

fn member_ids(kit_members: &Value) -> Vec<String> {
    kit_members
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row["user_id"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

#[pollster::test]
async fn an_owner_deletes_a_second_workspace_and_lands_in_the_first() {
    let erasure = Erasure::default();
    let kit = kit_with(&erasure);
    let (alice_first, first) = create_workspace(&kit, ALICE, "Acme").await;
    let (alice_side, side) = second_workspace(&kit, &alice_first, "Side").await;
    let side_me = me(&kit, &alice_side).await.json();
    let alice_id = side_me["member"]["user_id"]
        .as_str()
        .expect("an id")
        .to_owned();
    assert_eq!(side_me["workspace"]["id"], side.as_str());

    // Bob joins Side, and Carol is invited to it but has not accepted yet.
    let bob = invite_and_accept(&kit, &alice_side, BOB, "member").await;
    let sent = call(
        &kit,
        Method::POST,
        INVITES,
        &alice_side,
        Some(&json!({"email": CAROL})),
    )
    .await;
    assert_eq!(sent.status, StatusCode::CREATED);
    let carol_link = mailed_token(&kit);
    let bob_id = me(&kit, &bob).await.json()["member"]["user_id"]
        .as_str()
        .expect("an id")
        .to_owned();

    let deleted = delete_current(&kit, &alice_side, "Side").await;
    assert_eq!(
        deleted.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&deleted.body)
    );
    let body = deleted.json();
    assert_eq!(body["deleted"], side.as_str());
    assert_eq!(
        body["next"]["id"],
        first.as_str(),
        "the oldest other workspace"
    );
    assert_eq!(body["next"]["name"], "Acme");

    // The erasure was asked for this workspace's own spaces: shared, each
    // member's personal space, and nothing that names another workspace.
    let calls = erasure.calls.lock().expect("not poisoned").clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, side);
    assert!(calls[0].1.contains(&"shared".to_owned()));
    assert!(calls[0].1.contains(&format!("personal:{alice_id}")));
    assert!(calls[0].1.contains(&format!("personal:{bob_id}")));

    // The new session names the first workspace and works there.
    let next = deleted
        .cookie(SESSION_COOKIE)
        .expect("a session for the next workspace");
    assert_eq!(
        me(&kit, &next).await.json()["workspace"]["id"],
        first.as_str()
    );

    // Every session that named the deleted workspace is refused, the
    // owner's and the member's alike.
    assert_eq!(me(&kit, &alice_side).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(me(&kit, &bob).await.status, StatusCode::UNAUTHORIZED);
    let gone = get(&kit.router, WORKSPACES, &[(SESSION_COOKIE, &alice_side)]).await;
    assert_eq!(gone.status, StatusCode::UNAUTHORIZED);

    // The open invitation's link no longer signs anybody in.
    let spent = post_form(&kit.router, VERIFY, &[], &format!("token={carol_link}")).await;
    assert_ne!(
        spent.status,
        StatusCode::FOUND,
        "an invitation's link is dead"
    );
    assert!(spent.cookie(SESSION_COOKIE).is_none());

    // The other workspace is exactly as it was, members and all.
    let first_members = get(
        &kit.router,
        "/v1/workspaces/members",
        &[(SESSION_COOKIE, &next)],
    )
    .await
    .json();
    assert_eq!(member_ids(&first_members).len(), 1, "{first_members}");

    // Signing in by email lands in the workspace that remains.
    let again = sign_in(&kit, ALICE).await;
    assert_eq!(
        me(&kit, &again).await.json()["workspace"]["id"],
        first.as_str()
    );
    let listed = get(&kit.router, WORKSPACES, &[(SESSION_COOKIE, &again)])
        .await
        .json();
    let ids: Vec<&str> = listed
        .as_array()
        .expect("a list")
        .iter()
        .filter_map(|item| item["id"].as_str())
        .collect();
    assert_eq!(ids, vec![first.as_str()], "{listed}");
}

#[pollster::test]
async fn a_wrong_name_changes_nothing() {
    let erasure = Erasure::default();
    let kit = kit_with(&erasure);
    let (alice_first, _) = create_workspace(&kit, ALICE, "Acme").await;
    let (alice_side, side) = second_workspace(&kit, &alice_first, "Side").await;

    let refused = delete_current(&kit, &alice_side, "side").await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    let refused = delete_current(&kit, &alice_side, "").await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);

    assert!(erasure.calls.lock().expect("not poisoned").is_empty());
    let still = me(&kit, &alice_side).await.json();
    assert_eq!(still["workspace"]["id"], side.as_str());
}

#[pollster::test]
async fn an_admin_or_a_member_cannot_delete_it() {
    let erasure = Erasure::default();
    let kit = kit_with(&erasure);
    let (alice_first, _) = create_workspace(&kit, ALICE, "Acme").await;
    let (alice_side, side) = second_workspace(&kit, &alice_first, "Side").await;
    let admin = invite_and_accept(&kit, &alice_side, CAROL, "admin").await;
    let member = invite_and_accept(&kit, &alice_side, DAN, "member").await;

    for session in [&admin, &member] {
        let refused = delete_current(&kit, session, "Side").await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN);
    }

    assert!(erasure.calls.lock().expect("not poisoned").is_empty());
    assert_eq!(
        me(&kit, &alice_side).await.json()["workspace"]["id"],
        side.as_str()
    );
    assert_eq!(
        me(&kit, &member).await.json()["workspace"]["id"],
        side.as_str()
    );
}

#[pollster::test]
async fn the_only_workspace_cannot_be_deleted() {
    let erasure = Erasure::default();
    let kit = kit_with(&erasure);
    let (alice, workspace) = create_workspace(&kit, ALICE, "Acme").await;

    let refused = delete_current(&kit, &alice, "Acme").await;
    assert_eq!(refused.status, StatusCode::CONFLICT);
    assert_eq!(refused.json()["title"], "This is your only workspace.");

    assert!(erasure.calls.lock().expect("not poisoned").is_empty());
    assert_eq!(
        me(&kit, &alice).await.json()["workspace"]["id"],
        workspace.as_str()
    );
}

#[pollster::test]
async fn a_failed_erasure_leaves_the_workspace_whole() {
    let erasure = Erasure::default();
    let kit = kit_with(&erasure);
    let (alice_first, _) = create_workspace(&kit, ALICE, "Acme").await;
    let (alice_side, side) = second_workspace(&kit, &alice_first, "Side").await;

    erasure.fail.store(true, Ordering::SeqCst);
    let failed = delete_current(&kit, &alice_side, "Side").await;
    assert_eq!(failed.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        failed.cookie(SESSION_COOKIE).is_none(),
        "no session for a half deletion"
    );

    // Nothing was tombstoned: the owner is still in, and can try again.
    assert_eq!(
        me(&kit, &alice_side).await.json()["workspace"]["id"],
        side.as_str()
    );

    erasure.fail.store(false, Ordering::SeqCst);
    let retried = delete_current(&kit, &alice_side, "Side").await;
    assert_eq!(retried.status, StatusCode::OK);
}

#[pollster::test]
async fn an_invitee_of_a_deleted_workspace_is_not_let_in() {
    let erasure = Erasure::default();
    let kit = kit_with(&erasure);
    let (alice_first, _) = create_workspace(&kit, ALICE, "Acme").await;
    let (alice_side, side) = second_workspace(&kit, &alice_first, "Side").await;
    let sent = call(
        &kit,
        Method::POST,
        INVITES,
        &alice_side,
        Some(&json!({"email": DAN})),
    )
    .await;
    assert_eq!(sent.status, StatusCode::CREATED);
    let dan_link = mailed_token(&kit);

    assert_eq!(
        delete_current(&kit, &alice_side, "Side").await.status,
        StatusCode::OK
    );

    // The invitation's link was spent with it: it signs nobody in.
    let spent = post_form(&kit.router, VERIFY, &[], &format!("token={dan_link}")).await;
    assert_ne!(spent.status, StatusCode::FOUND);
    assert!(spent.cookie(SESSION_COOKIE).is_none());

    // A fresh sign-in as Dan does not reach the deleted workspace: he has no
    // membership there, and naming it gets nothing.
    let fresh = post_json(&kit.router, START, &[], &json!({ "email": DAN })).await;
    assert_eq!(fresh.status, StatusCode::ACCEPTED);
    let session = verify(&kit, &mailed_token(&kit)).await;
    let landed = me(&kit, &session).await.json();
    assert_ne!(landed["workspace"]["id"], side.as_str(), "{landed}");
    assert_eq!(
        landed["member"]["role"], "owner",
        "a new workspace of his own: {landed}"
    );
}
