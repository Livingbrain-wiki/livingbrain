//! The kit the `usage` tests share: a harness with the workspaces module
//! (which owns the session and the member rows), the usage module under
//! test, and — mounted only where a test needs it — the tokens module, so
//! a test can mint a personal access token the way a real client would.
//
// Each test file declares `mod support;` and Cargo compiles each one as its
// own binary, so a helper only one of them uses is dead code in the others.
#![allow(dead_code)]

use std::sync::Arc;

use cratefield_core::axum::http::{Method, Request, header};
use cratefield_core::axum::{self, body::Body};
use cratefield_core::{MapConfig, Signer, Statement};
use cratefield_testing::TestHarness;
use livingbrain_models::Models;
use livingbrain_tokens::Tokens;
use livingbrain_usage::Usage;
use livingbrain_workspaces::Workspaces;
use serde_json::{Value, json};
use time::OffsetDateTime;

/// The instant every test runs at — the kit's own fixed clock, spelled out
/// so a test can build `now`-relative expectations from it.
pub const NOW: i64 = 1_800_000_000;
/// The workspace every test signs into.
pub const WORKSPACE: &str = "T0SPACE";
/// The member: the workspace's owner, so an admin everywhere.
pub const OWNER: &str = "U0OWNER";
/// A member who is neither owner nor admin.
pub const PLAIN: &str = "U0PLAIN";

/// The instant the kit's fixed clock answers with.
#[must_use]
pub fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(NOW).expect("a valid instant")
}

/// A harness with `workspaces` (which owns the session) and `usage`
/// mounted. The kit's fixed clock is the instant [`NOW`] names, which is
/// the instant every record and every read below runs at.
#[must_use]
pub fn harness() -> TestHarness {
    harness_with_config(&[])
}

/// A harness with `USAGE_BUDGET_CENTS_MONTHLY` set to `cents` cents.
#[must_use]
pub fn harness_with_budget(cents: &str) -> TestHarness {
    harness_with_config(&[("USAGE_BUDGET_CENTS_MONTHLY", cents)])
}

fn harness_with_config(pairs: &[(&str, &str)]) -> TestHarness {
    let config = Arc::new(MapConfig::from_pairs(pairs.iter().copied()));
    TestHarness::with_ports(
        vec![
            Box::new(Workspaces::new()),
            // Mounted so a test can seed a `model_connections` row: the
            // models migration has to have run for the table to exist,
            // which is also what makes the read below honest.
            Box::new(Models::new()),
            Box::new(Usage::new()),
            Box::new(Tokens::new()),
        ],
        move |ports| {
            ports.config = config;
        },
    )
}

/// A harness whose seed rows and session cookie already exist: the
/// workspace, its owner (an admin), and one plain member.
pub async fn signed_in() -> (TestHarness, String) {
    let kit = harness();
    seed_workspace(&kit, WORKSPACE, OWNER).await;
    seed_member(&kit, WORKSPACE, OWNER, true).await;
    seed_member(&kit, WORKSPACE, PLAIN, false).await;
    let cookie = session(&kit, WORKSPACE, OWNER);
    (kit, cookie)
}

// ---------------------------------------------------------------------------
// Tenancy

/// Writes the workspace and one member, the rows `caller` resolves a
/// session cookie to. Seeded rather than signed in through Slack: what
/// these tests hold the module to is what happens *after* a session
/// verifies, and the sign-in flow has its own tests.
pub async fn seed_workspace(kit: &TestHarness, workspace_id: &str, owner_id: &str) {
    kit.db
        .execute(&Statement::with_values(
            "INSERT INTO workspaces (id, name, owner_id, created_at) VALUES (?, ?, ?, ?)",
            vec![
                text(workspace_id),
                text("A workspace"),
                text(owner_id),
                text("2026-01-01T00:00:00Z"),
            ],
        ))
        .await
        .expect("the workspace row seeds");
}

/// One member of a seeded workspace, admin or not.
pub async fn seed_member(kit: &TestHarness, workspace_id: &str, user_id: &str, is_admin: bool) {
    let now = "2026-01-01T00:00:00Z".to_owned();
    kit.db
        .execute(&Statement::with_values(
            "INSERT INTO workspace_members \
                 (workspace_id, user_id, name, timezone, is_admin, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
            vec![
                text(workspace_id),
                text(user_id),
                text("A member"),
                sea_query::Value::String(None),
                sea_query::Value::BigInt(Some(i64::from(is_admin))),
                text(&now),
            ],
        ))
        .await
        .expect("the member row seeds");
}

/// A session cookie the way the workspaces module seals one. The
/// signature is good; the member row above is what the handler must find.
#[must_use]
pub fn session(kit: &TestHarness, workspace_id: &str, user_id: &str) -> String {
    use cratefield_core::{Kid, Payload};
    kit.signer.sign(&Payload {
        purpose: livingbrain_workspaces::SESSION_PURPOSE.to_owned(),
        subject: json!({"team_id": workspace_id, "user_id": user_id, "exp": NOW + 3600})
            .to_string(),
        exp: None,
        kid: Kid::Cur,
    })
}

/// Seeds a main-role model connection directly: the row the price sheet
/// reads. Nothing here goes through the models module's connect route —
/// what the ledger tests hold the cross-module read to is the *read*, not
/// the connect flow, which has its own tests.
pub async fn seed_main_model(kit: &TestHarness, provider: &str, model: &str) {
    kit.db
        .execute(&Statement::with_values(
            "INSERT INTO model_connections \
                 (workspace_id, role, provider, base_url, model, key_ciphertext, key_last4, \
                  fallback_to_managed, status, missing, context_size, checked_at, updated_by) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            vec![
                text(WORKSPACE),
                text("main"),
                text(provider),
                text("https://api.example.com"),
                text(model),
                sea_query::Value::Bytes(Some(Box::new(vec![0u8, 1, 2, 3]))),
                text("last"),
                sea_query::Value::BigInt(Some(0)),
                text("ok"),
                text("[]"),
                sea_query::Value::String(None),
                text("2026-01-01T00:00:00Z"),
                text(OWNER),
            ],
        ))
        .await
        .expect("the model connection row seeds");
}

// ---------------------------------------------------------------------------
// Requests

/// A buffered response, since the kit's own `TestResponse` body is private.
pub struct Res {
    pub status: cratefield_core::axum::http::StatusCode,
    pub body: Vec<u8>,
}

impl Res {
    /// # Panics
    ///
    /// When the body is not JSON.
    #[must_use]
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("body is JSON")
    }

    /// The body as text, for the assertions that read a problem slug.
    #[must_use]
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// `GET path` with a session cookie (or none, for the anonymous case).
pub async fn get(kit: &TestHarness, path: &str, cookie: &str) -> Res {
    send(kit, Method::GET, path, cookie, None).await
}

/// `GET path` with `Authorization: Bearer token`, the way a personal
/// access token travels.
pub async fn get_with_token(kit: &TestHarness, path: &str, token: &str) -> Res {
    use tower::ServiceExt;
    let request = Request::builder()
        .method(Method::GET)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("request builds");
    let response = kit
        .router
        .clone()
        .oneshot(request)
        .await
        .expect("router answers");
    into_res(response).await
}

/// A request carrying the session cookie the way a browser would.
pub async fn send(
    kit: &TestHarness,
    method: Method,
    path: &str,
    cookie: &str,
    body: Option<Value>,
) -> Res {
    use tower::ServiceExt;
    let mut builder = Request::builder().method(method).uri(path);
    if !cookie.is_empty() {
        builder = builder.header(header::COOKIE, format!("__Host-lb_session={cookie}"));
    }
    let request = builder
        .header(header::CONTENT_TYPE, "application/json")
        .body(match body {
            Some(body) => Body::from(body.to_string()),
            None => Body::empty(),
        })
        .expect("request builds");
    let response = kit
        .router
        .clone()
        .oneshot(request)
        .await
        .expect("router answers");
    into_res(response).await
}

async fn into_res(response: axum::response::Response) -> Res {
    let (parts, body) = response.into_parts();
    let body = axum::body::to_bytes(body, 1024 * 1024)
        .await
        .expect("body reads");
    Res {
        status: parts.status,
        body: body.to_vec(),
    }
}

/// Mints a personal access token the way the API does — `POST /v1/tokens`
/// over the owner's session — so the bearer path is exercised end to end.
pub async fn mint_token(kit: &TestHarness, cookie: &str) -> String {
    let created = send(
        kit,
        Method::POST,
        "/v1/tokens",
        cookie,
        Some(json!({"name": "The billing script"})),
    )
    .await;
    assert_eq!(
        created.status,
        cratefield_core::axum::http::StatusCode::CREATED,
        "{}",
        created.text()
    );
    created.json()["token"]
        .as_str()
        .expect("the token, once")
        .to_owned()
}

fn text(value: &str) -> sea_query::Value {
    sea_query::Value::String(Some(Box::new(value.to_owned())))
}
