//! Connected apps (issues #51 and #146; ADR 0005) through the venture's own
//! credential resolvers: the owner connects an app with her personal access
//! token (what `livingbrain apps issue` sends), the app calls its five tools
//! with the key it was issued, and nothing else answers to that key.
//!
//! The composition is the Worker's: `workspaces` and `tokens` resolve the
//! credentials, `livingbrain_venture::wiki` is the bearer, and the app routes
//! are the ones `cli_modules` mounts, served at `/v1/app-tools` here (the
//! Worker re-addresses `/v1/tools`, which `rewrite_tools_path` pins).

use std::sync::Arc;

use cratefield_core::axum::body::Body;
use cratefield_core::axum::http::{Method, Request, StatusCode, header};
use cratefield_core::{Blob, Kid, Module, Payload, ScopedBlob, Signer, Statement, UlidIdGen};
use cratefield_kms::{Dek, Kms, LocalFileKms};
use cratefield_testing::{MemoryBlob, TestHarness};
use livingbrain_access::Scope;
use livingbrain_api::{AppTools, Apps, Export, Notes, PagesRebind, PublicPages, Search, Wiki};
use livingbrain_pages::{Author, EntityType, PageStore, PageWrite, Pages};
use livingbrain_tokens::{Me, Tokens};
use livingbrain_workspaces::{SESSION_COOKIE, SESSION_PURPOSE, Workspaces};
use serde_json::{Value, json};
use tower::ServiceExt;

const HOST: &str = "app.livingbrain.wiki";
const ORIGIN: &str = "https://app.livingbrain.wiki";
const WORKSPACE: &str = "T0APPS";
const OWNER: &str = "U0OWNER";
const MEMBER: &str = "U0MEMBER";
const SEEDED_AT: &str = "2026-01-01T00:00:00Z";

struct Kit {
    harness: TestHarness,
    store: PageStore,
}

fn kit() -> Kit {
    let kms: Arc<dyn Kms> = Arc::new(
        LocalFileKms::from_key(Dek::generate().expect("a key"), "test-kek", "test")
            .expect("a well-formed key"),
    );
    let wiki: Wiki = livingbrain_venture::wiki(Arc::clone(&kms));
    let surface = wiki.clone();
    let pages = Pages::new().surface(move |ctx| livingbrain_venture::pages_surface(ctx, &surface));
    let modules: Vec<Box<dyn Module>> = vec![
        Box::new(Workspaces::new()),
        Box::new(Tokens::new()),
        Box::new(Me::new()),
        Box::new(pages),
        Box::new(Notes::new(wiki.clone())),
        Box::new(Search::new(wiki.clone())),
        Box::new(Export::new(wiki.clone())),
        Box::new(Apps::new(wiki.clone())),
        Box::new(PublicPages::new(wiki.clone())),
        Box::new(AppTools::new(wiki)),
        Box::new(livingbrain_accounts::Access::new()),
    ];
    let memory: Arc<dyn Blob> = Arc::new(MemoryBlob::new());
    let for_ports = Arc::clone(&memory);
    let harness = TestHarness::with_ports(modules, move |ports| {
        ports.blob = Some(Arc::new(PagesRebind::new(Arc::clone(&for_ports))) as Arc<dyn Blob>);
    });
    let store = PageStore::new(
        harness.db.clone(),
        Arc::new(ScopedBlob::new(memory, "pages")),
        kms,
        Arc::new(harness.clock.clone()),
        Arc::new(UlidIdGen),
    );
    Kit { harness, store }
}

fn text(value: &str) -> sea_query::Value {
    sea_query::Value::String(Some(Box::new(value.to_owned())))
}

async fn member(kit: &TestHarness, user: &str, role: &str) {
    kit.db
        .execute(&Statement::with_values(
            "INSERT INTO workspace_members (workspace_id, user_id, name, timezone, is_admin, \
             updated_at, role) VALUES (?, ?, ?, ?, ?, ?, ?)",
            vec![
                text(WORKSPACE),
                text(user),
                text(user),
                sea_query::Value::String(None),
                sea_query::Value::BigInt(Some(0)),
                text(SEEDED_AT),
                text(role),
            ],
        ))
        .await
        .expect("the member row seeds");
}

async fn seeded() -> Kit {
    let kit = kit();
    kit.harness
        .db
        .execute(&Statement::with_values(
            "INSERT INTO workspaces (id, name, owner_id, created_at) VALUES (?, ?, ?, ?)",
            vec![
                text(WORKSPACE),
                text("A workspace"),
                text(OWNER),
                text(SEEDED_AT),
            ],
        ))
        .await
        .expect("the workspace row seeds");
    member(&kit.harness, OWNER, "owner").await;
    member(&kit.harness, MEMBER, "member").await;
    kit
}

fn session(kit: &TestHarness, user: &str) -> String {
    kit.signer.sign(&Payload {
        purpose: SESSION_PURPOSE.to_owned(),
        subject: json!({
            "team_id": WORKSPACE,
            "user_id": user,
            "exp": kit.clock.0.unix_timestamp() + 3600,
        })
        .to_string(),
        exp: None,
        kid: Kid::Cur,
    })
}

struct Res {
    status: StatusCode,
    body: Vec<u8>,
}

impl Res {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|err| {
            panic!("not JSON ({err}): {}", String::from_utf8_lossy(&self.body))
        })
    }
}

/// A bearer request, as the CLI and the apps send them.
async fn bearer(kit: &Kit, method: Method, path: &str, token: &str, body: Option<Value>) -> Res {
    send(kit, method, path, Some(token), None, body).await
}

async fn send(
    kit: &Kit,
    method: Method,
    path: &str,
    token: Option<&str>,
    cookie: Option<&str>,
    body: Option<Value>,
) -> Res {
    let mut builder = Request::builder()
        .method(method.clone())
        .uri(path)
        .header(header::HOST, HOST);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    if let Some(cookie) = cookie {
        builder = builder
            .header(header::COOKIE, format!("{SESSION_COOKIE}={cookie}"))
            .header("sec-fetch-site", "same-origin");
        if !method.is_safe() {
            builder = builder.header(header::ORIGIN, ORIGIN);
        }
    }
    let body = match body {
        Some(json) => {
            let raw = json.to_string();
            builder = builder
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::CONTENT_LENGTH, raw.len());
            Body::from(raw)
        }
        None => Body::empty(),
    };
    let response = kit
        .harness
        .router
        .clone()
        .oneshot(builder.body(body).expect("request builds"))
        .await
        .expect("router answers");
    let (parts, body) = response.into_parts();
    let bytes = cratefield_core::axum::body::to_bytes(body, 4 * 1024 * 1024)
        .await
        .expect("body reads");
    Res {
        status: parts.status,
        body: bytes.to_vec(),
    }
}

/// A personal access token minted over the wire, as `livingbrain login` gets
/// one (whole, or cut to `scopes`).
async fn pat(kit: &Kit, user: &str, scopes: &[&str]) -> String {
    let cookie = session(&kit.harness, user);
    let created = send(
        kit,
        Method::POST,
        "/v1/tokens",
        None,
        Some(&cookie),
        Some(json!({ "name": "cli", "scopes": scopes })),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.json());
    created.json()["token"]
        .as_str()
        .expect("a token")
        .to_owned()
}

const REFUNDS: &str = "---\ntitle: Refund policy\n---\n\nCustomers can request a refund within 30 days of purchase.\n";

async fn shared_page(kit: &Kit, slug: &str, body: &str) {
    let scope = livingbrain_pages::page_scope(WORKSPACE, &Scope::Shared);
    kit.store
        .write(
            &scope,
            slug,
            PageWrite {
                entity_type: EntityType::Decision,
                markdown: body.to_owned(),
                author: Author::Human {
                    id: OWNER.to_owned(),
                },
                base_version: None,
            },
        )
        .await
        .expect("a page");
}

#[pollster::test]
async fn an_owner_connects_an_app_and_the_app_uses_only_its_five_tools() {
    let kit = seeded().await;
    shared_page(&kit, "refund-policy", REFUNDS).await;
    shared_page(
        &kit,
        "internal-plans",
        "---\ntitle: Internal plans\n---\n\nThe zebrafinch refund plan.\n",
    )
    .await;
    let owner = pat(&kit, OWNER, &[]).await;

    // Publish and connect, the way the CLI does.
    let published = bearer(
        &kit,
        Method::POST,
        "/v1/public/pages",
        &owner,
        Some(json!({ "slug": "refund-policy", "url": "https://example.com/refunds" })),
    )
    .await;
    assert_eq!(
        published.status,
        StatusCode::CREATED,
        "{}",
        published.json()
    );
    let issued = bearer(
        &kit,
        Method::POST,
        "/v1/apps",
        &owner,
        Some(json!({ "app": "earthos" })),
    )
    .await;
    assert_eq!(issued.status, StatusCode::CREATED, "{}", issued.json());
    let key = issued.json()["key"].as_str().expect("a key").to_owned();
    assert!(key.starts_with("lba_"));

    // The five tools, through the real resolver.
    let call = |name: &'static str, body: Value| {
        let (kit, key) = (&kit, key.clone());
        async move {
            bearer(
                kit,
                Method::POST,
                &format!("/v1/app-tools/{name}"),
                &key,
                Some(body),
            )
            .await
        }
    };
    let answered = call(
        "brain_answer_public",
        json!({ "question": "How long to request a refund?" }),
    )
    .await;
    assert_eq!(answered.status, StatusCode::OK, "{}", answered.json());
    assert!(
        answered.json()["answer"]
            .as_str()
            .unwrap()
            .contains("30 days")
    );
    assert_eq!(answered.json()["citations"][0]["scope"], "public");
    let none = call(
        "brain_answer_public",
        json!({ "question": "zebrafinch refund plan" }),
    )
    .await;
    assert!(
        none.json()["answer"].is_null(),
        "an unpublished page is not answered"
    );

    let noted = call(
        "brain_memory_note",
        json!({ "subject": "user:1", "text": "Likes tea", "source": "c1" }),
    )
    .await;
    assert_eq!(noted.status, StatusCode::OK, "{}", noted.json());
    let id = noted.json()["id"].as_str().unwrap().to_owned();
    let recalled = call("brain_memory_recall", json!({ "subject": "user:1" })).await;
    assert_eq!(recalled.json()["memories"][0]["text"], "Likes tea");
    assert_eq!(
        call(
            "brain_memory_forget",
            json!({ "subject": "user:1", "id": id })
        )
        .await
        .json()["forgotten"],
        1
    );
    call(
        "brain_memory_note",
        json!({ "subject": "user:1", "text": "a", "source": "c" }),
    )
    .await;
    assert_eq!(
        call("brain_memory_forget_all", json!({ "subject": "user:1" }))
            .await
            .json()["forgotten"],
        1
    );

    // Nothing else answers to the key.
    for (method, path) in [
        (Method::GET, "/v1/pages"),
        (Method::GET, "/v1/pages/refund-policy"),
        (Method::GET, "/v1/search?q=refund"),
        (Method::GET, "/v1/export"),
        (Method::GET, "/v1/apps"),
        (Method::GET, "/v1/public"),
        (Method::GET, "/v1/me"),
    ] {
        let res = bearer(&kit, method, path, &key, None).await;
        let body = String::from_utf8_lossy(&res.body).to_lowercase();
        assert!(
            !body.contains("refund") && !body.contains("zebrafinch"),
            "{path}: {} leaked into {}",
            res.status,
            body.chars().take(200).collect::<String>()
        );
    }
    let write = bearer(
        &kit,
        Method::POST,
        "/v1/notes",
        &key,
        Some(json!({ "body": "sneaky" })),
    )
    .await;
    assert_eq!(write.status, StatusCode::FORBIDDEN);

    // A member, and the owner's own token, are not an app.
    let member_pat = pat(&kit, MEMBER, &[]).await;
    for token in [&owner, &member_pat] {
        let res = bearer(
            &kit,
            Method::POST,
            "/v1/app-tools/brain_memory_recall",
            token,
            Some(json!({ "subject": "user:1" })),
        )
        .await;
        assert_eq!(res.status, StatusCode::FORBIDDEN);
    }
}

#[pollster::test]
async fn only_an_owner_with_a_whole_token_issues_or_publishes() {
    let kit = seeded().await;
    shared_page(&kit, "refund-policy", REFUNDS).await;
    let member_pat = pat(&kit, MEMBER, &[]).await;
    let narrowed = pat(&kit, OWNER, &["shared"]).await;
    for token in [&member_pat, &narrowed] {
        let issued = bearer(
            &kit,
            Method::POST,
            "/v1/apps",
            token,
            Some(json!({ "app": "earthos" })),
        )
        .await;
        assert_eq!(issued.status, StatusCode::FORBIDDEN, "{}", issued.json());
        let published = bearer(
            &kit,
            Method::POST,
            "/v1/public/pages",
            token,
            Some(json!({ "slug": "refund-policy" })),
        )
        .await;
        assert_eq!(
            published.status,
            StatusCode::FORBIDDEN,
            "{}",
            published.json()
        );
    }
    // The audience-carrying tokens are not an owner either.
    let anonymous = send(
        &kit,
        Method::POST,
        "/v1/apps",
        None,
        None,
        Some(json!({ "app": "earthos" })),
    )
    .await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
}

#[pollster::test]
async fn revoking_or_removing_the_owner_ends_the_credential_on_the_next_request() {
    let kit = seeded().await;
    let owner = pat(&kit, OWNER, &[]).await;
    let first = bearer(
        &kit,
        Method::POST,
        "/v1/apps",
        &owner,
        Some(json!({ "app": "earthos" })),
    )
    .await
    .json()["key"]
        .as_str()
        .unwrap()
        .to_owned();
    let second = bearer(
        &kit,
        Method::POST,
        "/v1/apps",
        &owner,
        Some(json!({ "app": "supportgenius" })),
    )
    .await
    .json()["key"]
        .as_str()
        .unwrap()
        .to_owned();
    let recall = |key: String| {
        let kit = &kit;
        async move {
            bearer(
                kit,
                Method::POST,
                "/v1/app-tools/brain_memory_recall",
                &key,
                Some(json!({ "subject": "s" })),
            )
            .await
            .status
        }
    };
    assert_eq!(recall(first.clone()).await, StatusCode::OK);
    assert_eq!(recall(second.clone()).await, StatusCode::OK);

    // Revoke one app: its key is refused at once, the other still works.
    let revoked = bearer(&kit, Method::DELETE, "/v1/apps/earthos", &owner, None).await;
    assert_eq!(revoked.status, StatusCode::OK, "{}", revoked.json());
    assert_eq!(recall(first).await, StatusCode::UNAUTHORIZED);
    assert_eq!(recall(second.clone()).await, StatusCode::OK);

    // Removing the owner who issued it fails the rest closed.
    kit.harness
        .db
        .execute(&Statement::new("UPDATE workspace_members SET removed_at = '2026-01-02T00:00:00Z' WHERE user_id = 'U0OWNER'"))
        .await
        .expect("the owner is removed");
    assert_eq!(recall(second).await, StatusCode::UNAUTHORIZED);
}
