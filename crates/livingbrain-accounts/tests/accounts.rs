//! Agent accounts, grants and the audit log through the routes a deployment
//! mounts (ADR 0004): `workspaces`, `tokens`, `me`, `access`, `agents`,
//! `audit`.
//!
//! What this file holds them to: an agent key is shown once and works only
//! as a bearer; it stops the moment it is revoked, rotated away, its account
//! disabled or its owner removed; it can never reach a session route, mint a
//! token or manage anything; only a space's owner (or an admin of it) can
//! share it — a workspace admin cannot grant themselves into a member's
//! memory; a grantee can give up an allow but never escape a deny; every
//! write is same-origin and audited; and the log is for owners and admins.

#![allow(clippy::disallowed_types)]

use std::sync::{Arc, Mutex};

use cratefield_core::axum::body::Body;
use cratefield_core::axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header};
use cratefield_core::{Kid, Payload, Ports, Signer, Statement};
use cratefield_testing::TestHarness;
use livingbrain_accounts::{Access, Agents, Audit};
use livingbrain_tokens::{Me, Tokens};
use livingbrain_workspaces::Workspaces;
use serde_json::{Value, json};

const WORKSPACE: &str = "ws_acme";
const OWNER: &str = "usr_owner";
const ADMIN: &str = "usr_admin";
const BOB: &str = "usr_bob";
const HOST: &str = "app.example.test";

struct Kit {
    harness: TestHarness,
}

fn text(value: &str) -> sea_query::Value {
    sea_query::Value::String(Some(Box::new(value.to_owned())))
}

async fn kit() -> Kit {
    let captured: Arc<Mutex<Option<Ports>>> = Arc::new(Mutex::new(None));
    let patch = captured.clone();
    let harness = TestHarness::with_ports(
        vec![
            Box::new(Workspaces::new()),
            Box::new(Tokens::new()),
            Box::new(Me::new()),
            Box::new(Access::new()),
            Box::new(Agents::new()),
            Box::new(Audit::new()),
            Box::new(livingbrain_models::Models::new()),
            Box::new(livingbrain_usage::Usage::new()),
        ],
        move |ports| {
            let mut copy = Ports::empty();
            copy.db = ports.db.clone();
            *patch.lock().expect("lock") = Some(copy);
        },
    );
    let db = harness.db.clone();
    db.execute(&Statement::with_values(
        "INSERT INTO workspaces (id, name, owner_id, created_at) VALUES (?, 'Acme', ?, ?)",
        vec![text(WORKSPACE), text(OWNER), text("2026-01-01T00:00:00Z")],
    ))
    .await
    .expect("seeds");
    for (user, role) in [(OWNER, "owner"), (ADMIN, "admin"), (BOB, "member")] {
        db.execute(&Statement::with_values(
            "INSERT INTO workspace_members \
               (workspace_id, user_id, name, timezone, is_admin, updated_at, role) \
             VALUES (?, ?, ?, NULL, 0, ?, ?)",
            vec![
                text(WORKSPACE),
                text(user),
                text(user),
                text("2026-01-01T00:00:00Z"),
                text(role),
            ],
        ))
        .await
        .expect("seeds");
    }
    Kit { harness }
}

impl Kit {
    fn session(&self, user: &str) -> String {
        self.harness.signer.sign(&Payload {
            purpose: livingbrain_workspaces::SESSION_PURPOSE.to_owned(),
            subject: json!({
                "workspace_id": WORKSPACE,
                "user_id": user,
                "exp": self.harness.clock.0.unix_timestamp() + 3600,
            })
            .to_string(),
            exp: None,
            kid: Kid::Cur,
        })
    }
}

enum Who<'a> {
    Browser(&'a str),
    CrossSite(&'a str),
    Bearer(&'a str),
}

fn headers(kit: &Kit, who: &Who<'_>) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert("host", HeaderValue::from_static(HOST));
    match who {
        Who::Browser(user) => {
            h.insert(
                header::COOKIE,
                HeaderValue::from_str(&format!("__Host-lb_session={}", kit.session(user)))
                    .expect("ascii"),
            );
            h.insert("sec-fetch-site", HeaderValue::from_static("same-origin"));
            h.insert(
                header::ORIGIN,
                HeaderValue::from_str(&format!("https://{HOST}")).expect("ascii"),
            );
        }
        Who::CrossSite(user) => {
            h.insert(
                header::COOKIE,
                HeaderValue::from_str(&format!("__Host-lb_session={}", kit.session(user)))
                    .expect("ascii"),
            );
            h.insert("sec-fetch-site", HeaderValue::from_static("cross-site"));
        }
        Who::Bearer(token) => {
            h.insert(
                header::AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {token}")).expect("ascii"),
            );
        }
    }
    h
}

async fn send(
    kit: &Kit,
    method: Method,
    path: &str,
    who: &Who<'_>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    use tower::ServiceExt;
    let mut builder = Request::builder().method(method).uri(path);
    for (name, value) in &headers(kit, who) {
        builder = builder.header(name, value.clone());
    }
    let body = match body {
        Some(json) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(json.to_string())
        }
        None => Body::empty(),
    };
    let response = kit
        .harness
        .router
        .clone()
        .oneshot(builder.body(body).expect("builds"))
        .await
        .expect("answers");
    let status = response.status();
    let bytes = cratefield_core::axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("reads");
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

/// `owner_of` creates an agent for themselves and mints a key for it.
async fn agent_with_key(kit: &Kit, owner_of: &str) -> (String, String, String) {
    let (status, agent) = send(
        kit,
        Method::POST,
        "/v1/agents",
        &Who::Browser(owner_of),
        Some(json!({"name": "CI bot", "description": "pushes sessions"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{agent}");
    let id = agent["id"].as_str().expect("an id").to_owned();
    assert!(id.starts_with("agt_"), "{id}");
    let (status, minted) = send(
        kit,
        Method::POST,
        &format!("/v1/agents/{id}/keys"),
        &Who::Browser(owner_of),
        Some(json!({"name": "laptop"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{minted}");
    let key = minted["key"].as_str().expect("the key, once").to_owned();
    let prefix = minted["prefix"].as_str().expect("a prefix").to_owned();
    assert!(key.starts_with("lba_") && key.starts_with(&prefix));
    (id, key, prefix)
}

#[pollster::test]
async fn an_agent_key_is_shown_once_and_says_who_it_is() {
    let kit = kit().await;
    let (id, key, prefix) = agent_with_key(&kit, BOB).await;
    let (status, keys) = send(
        &kit,
        Method::GET,
        &format!("/v1/agents/{id}/keys"),
        &Who::Browser(BOB),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !keys.to_string().contains(&key),
        "the list never carries the key"
    );
    assert_eq!(keys["keys"][0]["prefix"], prefix);

    let (status, me) = send(&kit, Method::GET, "/v1/me", &Who::Bearer(&key), None).await;
    assert_eq!(status, StatusCode::OK, "{me}");
    assert_eq!(me["principal"]["kind"], "agent");
    assert_eq!(me["principal"]["id"], id);
    assert_eq!(me["principal"]["on_behalf_of"]["id"], BOB);
    assert_eq!(me["credential"]["prefix"], prefix);
    assert!(!me.to_string().contains(&key));
    let (_, keys) = send(
        &kit,
        Method::GET,
        &format!("/v1/agents/{id}/keys"),
        &Who::Browser(BOB),
        None,
    )
    .await;
    assert!(keys["keys"][0]["last_used_at"].is_string(), "{keys}");
}

#[pollster::test]
async fn an_agent_key_stops_when_revoked_rotated_or_disabled() {
    let kit = kit().await;
    let (id, key, prefix) = agent_with_key(&kit, BOB).await;

    let (status, rotated) = send(
        &kit,
        Method::POST,
        &format!("/v1/agents/{id}/keys/{prefix}/rotate"),
        &Who::Browser(BOB),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{rotated}");
    let fresh = rotated["key"].as_str().expect("a new key").to_owned();
    assert_eq!(
        send(&kit, Method::GET, "/v1/me", &Who::Bearer(&key), None)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        send(&kit, Method::GET, "/v1/me", &Who::Bearer(&fresh), None)
            .await
            .0,
        StatusCode::OK
    );

    let (status, _) = send(
        &kit,
        Method::PATCH,
        &format!("/v1/agents/{id}"),
        &Who::Browser(BOB),
        Some(json!({"disabled": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        send(&kit, Method::GET, "/v1/me", &Who::Bearer(&fresh), None)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let (status, _) = send(
        &kit,
        Method::PATCH,
        &format!("/v1/agents/{id}"),
        &Who::Browser(BOB),
        Some(json!({"disabled": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        send(&kit, Method::GET, "/v1/me", &Who::Bearer(&fresh), None)
            .await
            .0,
        StatusCode::OK
    );

    let new_prefix = rotated["prefix"].as_str().expect("prefix").to_owned();
    let (status, _) = send(
        &kit,
        Method::DELETE,
        &format!("/v1/agents/{id}/keys/{new_prefix}"),
        &Who::Browser(BOB),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        send(&kit, Method::GET, "/v1/me", &Who::Bearer(&fresh), None)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
}

#[pollster::test]
async fn an_agent_key_never_reaches_a_session_route() {
    let kit = kit().await;
    let (_, key, _) = agent_with_key(&kit, BOB).await;
    for (method, path, body) in [
        (Method::GET, "/v1/agents", None),
        (Method::POST, "/v1/agents", Some(json!({"name": "spawn"}))),
        (Method::GET, "/v1/access/grants", None),
        (
            Method::POST,
            "/v1/access/grants",
            Some(json!({"principal": "member:usr_bob", "space": "shared", "permission": "admin"})),
        ),
        (Method::GET, "/v1/audit", None),
        (
            Method::POST,
            "/v1/tokens",
            Some(json!({"name": "escalate"})),
        ),
        (Method::GET, "/v1/tokens", None),
        (Method::GET, "/v1/workspaces/me", None),
        (Method::GET, "/v1/workspaces/members", None),
        // Routes added since: profiles (#200), models and Jev (#205), usage.
        (
            Method::PATCH,
            "/v1/me",
            Some(json!({"display_name": "Bot"})),
        ),
        (Method::GET, "/v1/models", None),
        (Method::GET, "/v1/models/mine", None),
        (Method::GET, "/v1/models/jev", None),
        (
            Method::PUT,
            "/v1/models/jev",
            Some(json!({"enabled": true})),
        ),
        (Method::DELETE, "/v1/models/jev/key", None),
        (Method::POST, "/v1/models/jev/test", Some(json!({}))),
        (Method::GET, "/v1/usage/daily", None),
    ] {
        let (status, body) = send(&kit, method.clone(), path, &Who::Bearer(&key), body).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {path}: {body}");
    }
    // `GET /v1/me` is the one session route that answers an agent key.
    let (status, me) = send(&kit, Method::GET, "/v1/me", &Who::Bearer(&key), None).await;
    assert_eq!(status, StatusCode::OK, "{me}");
}

#[pollster::test]
async fn jev_settings_are_read_by_members_and_written_by_owners_and_admins_only() {
    let kit = kit().await;
    let (status, body) = send(
        &kit,
        Method::GET,
        "/v1/models/jev",
        &Who::Browser(BOB),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "a member reads: {body}");
    let (status, body) = send(
        &kit,
        Method::PUT,
        "/v1/models/jev",
        &Who::Browser(BOB),
        Some(json!({"enabled": true})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a member does not write: {body}"
    );
    let (status, body) = send(
        &kit,
        Method::DELETE,
        "/v1/models/jev/key",
        &Who::Browser(BOB),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    for admin in [ADMIN, OWNER] {
        let (status, body) = send(
            &kit,
            Method::PUT,
            "/v1/models/jev",
            &Who::Browser(admin),
            Some(json!({"enabled": true})),
        )
        .await;
        assert_ne!(status, StatusCode::FORBIDDEN, "{admin} writes: {body}");
        assert_ne!(status, StatusCode::UNAUTHORIZED, "{admin} writes: {body}");
    }
}

#[pollster::test]
async fn a_removed_owners_agent_and_tokens_stop_working() {
    let kit = kit().await;
    let (_, key, _) = agent_with_key(&kit, BOB).await;
    let (status, pat) = send(
        &kit,
        Method::POST,
        "/v1/tokens",
        &Who::Browser(BOB),
        Some(json!({"name": "cli"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{pat}");
    let pat = pat["token"].as_str().expect("a token").to_owned();
    assert_eq!(
        send(&kit, Method::GET, "/v1/me", &Who::Bearer(&pat), None)
            .await
            .0,
        StatusCode::OK
    );

    let (status, _) = send(
        &kit,
        Method::DELETE,
        &format!("/v1/workspaces/members/{BOB}"),
        &Who::Browser(OWNER),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        send(&kit, Method::GET, "/v1/me", &Who::Bearer(&key), None)
            .await
            .0,
        StatusCode::UNAUTHORIZED,
        "an agent outlives nobody"
    );
    assert_eq!(
        send(&kit, Method::GET, "/v1/me", &Who::Bearer(&pat), None)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
}

#[pollster::test]
async fn only_an_admin_creates_an_agent_for_somebody_else_or_hands_one_over() {
    let kit = kit().await;
    let (status, _) = send(
        &kit,
        Method::POST,
        "/v1/agents",
        &Who::Browser(BOB),
        Some(json!({"name": "x", "owner_id": ADMIN})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, made) = send(
        &kit,
        Method::POST,
        "/v1/agents",
        &Who::Browser(ADMIN),
        Some(json!({"name": "x", "owner_id": BOB})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    assert_eq!(made["owner_id"], BOB);
    let (status, _) = send(
        &kit,
        Method::POST,
        "/v1/agents",
        &Who::Browser(ADMIN),
        Some(json!({"name": "x", "owner_id": "usr_nobody"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let (bobs, _, _) = agent_with_key(&kit, BOB).await;
    // Another member cannot see, manage or take it.
    let (status, _) = send(
        &kit,
        Method::GET,
        &format!("/v1/agents/{bobs}"),
        &Who::Browser(OWNER),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "an owner is an admin");
    let (status, list) = send(&kit, Method::GET, "/v1/agents", &Who::Browser(BOB), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        list["agents"]
            .as_array()
            .expect("a list")
            .iter()
            .all(|a| a["owner_id"] == BOB)
    );
    let (status, _) = send(
        &kit,
        Method::PATCH,
        &format!("/v1/agents/{bobs}"),
        &Who::Browser(BOB),
        Some(json!({"owner_id": ADMIN})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "handing over is an admin's call"
    );
}

#[pollster::test]
async fn only_a_spaces_owner_can_share_it_and_no_admin_can_grant_themselves_in() {
    let kit = kit().await;
    let grant = |principal: &str, space: &str, permission: &str| json!({"principal": principal, "space": space, "path": "coding-agents/claude", "kind": "pages", "permission": permission});
    // Bob shares his own folder with the admin: fine.
    let (status, made) = send(
        &kit,
        Method::POST,
        "/v1/access/grants",
        &Who::Browser(BOB),
        Some(grant("member:usr_admin", "personal:me", "read")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    // The workspace owner cannot grant anybody — themselves included — into
    // Bob's space.
    for who in [OWNER, ADMIN] {
        let (status, body) = send(
            &kit,
            Method::POST,
            "/v1/access/grants",
            &Who::Browser(who),
            Some(grant(&format!("member:{who}"), "personal:usr_bob", "admin")),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{who}: {body}");
    }
    // A member cannot hand out the shared space; an owner can.
    let (status, _) = send(
        &kit,
        Method::POST,
        "/v1/access/grants",
        &Who::Browser(BOB),
        Some(grant("member:usr_bob", "shared", "admin")),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = send(
        &kit,
        Method::POST,
        "/v1/access/grants",
        &Who::Browser(OWNER),
        Some(grant("member:usr_bob", "shared", "write")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    // Names that are nobody are refused.
    let (status, _) = send(
        &kit,
        Method::POST,
        "/v1/access/grants",
        &Who::Browser(BOB),
        Some(grant("member:usr_ghost", "personal:me", "read")),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = send(&kit, Method::POST, "/v1/access/grants", &Who::Browser(BOB), Some(json!({"principal": "member:usr_admin", "space": "personal:me", "path": "../x", "permission": "read"}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    // And a cross-site write is refused before anything is read.
    let (status, _) = send(
        &kit,
        Method::POST,
        "/v1/access/grants",
        &Who::CrossSite(BOB),
        Some(grant("member:usr_admin", "personal:me", "read")),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[pollster::test]
async fn a_grantee_gives_up_an_allow_but_never_escapes_a_deny() {
    let kit = kit().await;
    let (_, allow) = send(
        &kit,
        Method::POST,
        "/v1/access/grants",
        &Who::Browser(BOB),
        Some(
            json!({"principal": "member:usr_admin", "space": "personal:me", "permission": "read"}),
        ),
    )
    .await;
    let (_, deny) = send(&kit, Method::POST, "/v1/access/grants", &Who::Browser(BOB), Some(json!({"principal": "member:usr_admin", "space": "personal:me", "path": "diary", "permission": "read", "effect": "deny"}))).await;
    let deny_id = deny["id"].as_str().expect("an id");
    let (status, _) = send(
        &kit,
        Method::DELETE,
        &format!("/v1/access/grants/{deny_id}"),
        &Who::Browser(ADMIN),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a deny is not the grantee's to lift"
    );
    let allow_id = allow["id"].as_str().expect("an id");
    let (status, _) = send(
        &kit,
        Method::DELETE,
        &format!("/v1/access/grants/{allow_id}"),
        &Who::Browser(ADMIN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = send(
        &kit,
        Method::DELETE,
        &format!("/v1/access/grants/{deny_id}"),
        &Who::Browser(BOB),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "the space's owner lifts it");
}

#[pollster::test]
async fn the_audit_log_records_and_is_for_owners_and_admins() {
    let kit = kit().await;
    let (id, key, _) = agent_with_key(&kit, BOB).await;
    send(&kit, Method::POST, "/v1/access/grants", &Who::Browser(BOB), Some(json!({"principal": format!("agent:{id}"), "space": "personal:me", "path": "coding-agents", "permission": "write"}))).await;
    let (status, _) = send(&kit, Method::GET, "/v1/audit", &Who::Browser(BOB), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, log) = send(&kit, Method::GET, "/v1/audit", &Who::Browser(ADMIN), None).await;
    assert_eq!(status, StatusCode::OK, "{log}");
    let actions: Vec<&str> = log["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .filter_map(|entry| entry["action"].as_str())
        .collect();
    for wanted in ["agent.create", "agent.key.mint", "grant.create"] {
        assert!(actions.contains(&wanted), "{wanted} missing: {actions:?}");
    }
    assert!(!log.to_string().contains(&key), "no secret in the log");
}

#[pollster::test]
async fn a_key_scope_with_a_space_in_it_is_refused_rather_than_split() {
    let kit = kit().await;
    let (id, _, _) = agent_with_key(&kit, BOB).await;
    for scopes in [json!(["read: personal:usr_bob"]), json!([]), json!([""])] {
        let (status, body) = send(
            &kit,
            Method::POST,
            &format!("/v1/agents/{id}/keys"),
            &Who::Browser(BOB),
            Some(json!({"name": "x", "scopes": scopes})),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{scopes}: {body}");
    }
    let (status, minted) = send(
        &kit,
        Method::POST,
        &format!("/v1/agents/{id}/keys"),
        &Who::Browser(BOB),
        Some(json!({"name": "x", "scopes": ["read:personal:usr_bob/coding-agents"]})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{minted}");
    assert_eq!(
        minted["scopes"],
        json!(["read:personal:usr_bob/coding-agents"])
    );
}
