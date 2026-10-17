//! The `actions` audience over the wire (issue #234): a ChatGPT custom GPT's
//! Actions token, admitted on four read-only REST routes and refused
//! everywhere else.
//!
//! The composition here is the venture's own split: the pages surface and
//! the `search`, `ask` and `facts` modules are built from
//! [`livingbrain_venture::wiki_actions`], the page routes wrapped in
//! [`livingbrain_venture::pages_surface`], and the rest — `notes`, `export`,
//! `graph`, `sources`, `me`, `tokens` — keep the ordinary credentials, as
//! does the MCP endpoint nested beside the page routes. Everything goes
//! through the harness router, so the guard, CORS and problem+json all
//! stand. The token is stamped the way `mint_actions` mints it: the
//! audience, and the scope subset `read:`-prefixed, so the reach under test
//! is the reach a GPT holds.

use std::sync::Arc;

use cratefield_core::axum::body::Body;
use cratefield_core::axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header};
use cratefield_core::{Blob, Kid, Module, Payload, Signer, Statement};
use cratefield_kms::{Dek, Kms, LocalFileKms};
use cratefield_testing::{MemoryBlob, TestHarness};
use livingbrain_api::{Ask, Export, Facts, Graph, Notes, PagesRebind, Search, Sources};
use livingbrain_pages::Pages;
use livingbrain_tokens::{Me, Tokens};
use livingbrain_workspaces::{SESSION_COOKIE, SESSION_PURPOSE, Workspaces};
use serde_json::{Value, json};
use tower::ServiceExt;

const HOST: &str = "app.livingbrain.wiki";
const ORIGIN: &str = "https://app.livingbrain.wiki";

const WORKSPACE: &str = "T0ACTIONS";
const OWNER: &str = "U0GPT";
const SEEDED_AT: &str = "2026-01-01T00:00:00Z";

/// The venture's composition of the four routes a GPT's Actions call: the
/// pages module with [`livingbrain_venture::pages_surface`] at its root and
/// the MCP endpoint nested beside it, `search`, `ask` and `facts` over
/// [`livingbrain_venture::wiki_actions`], and the rest of the CLI surface
/// over [`livingbrain_venture::wiki`] — the same split `serve` mounts, over
/// a body store rooted on the pages key space the way `serve_cli` plants
/// it.
fn kit() -> TestHarness {
    let kms: Arc<dyn Kms> = Arc::new(
        LocalFileKms::from_key(Dek::generate().expect("a key"), "test-kek", "test")
            .expect("a well-formed key"),
    );
    let wiki = livingbrain_venture::wiki(Arc::clone(&kms));
    let actions = livingbrain_venture::wiki_actions(Arc::clone(&kms));
    let surface = actions.clone();
    let mcp_kms = Arc::clone(&kms);
    // The pages mount as the Worker serves it: the page routes at its root,
    // the MCP endpoint nested under `mcp` with the connector-admitting
    // bearer alone.
    let pages = Pages::new()
        .surface(move |ctx| livingbrain_venture::pages_surface(ctx, &surface))
        .nest("/mcp", move |ctx| {
            livingbrain_mcp::router(ctx, Arc::clone(&mcp_kms), livingbrain_venture::mcp_bearer())
        });
    let modules: Vec<Box<dyn Module>> = vec![
        Box::new(Workspaces::new()),
        Box::new(Tokens::new()),
        Box::new(Me::new()),
        Box::new(pages),
        Box::new(Notes::new(wiki.clone())),
        Box::new(Search::new(actions.clone())),
        Box::new(Ask::new(actions.clone())),
        Box::new(Export::new(wiki.clone())),
        Box::new(Facts::new(actions)),
        Box::new(Graph::new(wiki.clone())),
        Box::new(Sources::new(wiki)),
        // The grants tables every wiki route reads its access plan from.
        Box::new(livingbrain_accounts::Access::new()),
    ];
    let memory: Arc<dyn Blob> = Arc::new(MemoryBlob::new());
    TestHarness::with_ports(modules, move |ports| {
        ports.blob = Some(Arc::new(PagesRebind::new(Arc::clone(&memory))) as Arc<dyn Blob>);
    })
}

fn text(value: &str) -> sea_query::Value {
    sea_query::Value::String(Some(Box::new(value.to_owned())))
}

/// One workspace, one founder.
async fn seeded() -> TestHarness {
    let kit = kit();
    let row = Statement::with_values(
        "INSERT INTO workspaces (id, name, owner_id, created_at) VALUES (?, ?, ?, ?)",
        vec![
            text(WORKSPACE),
            text("A workspace"),
            text(OWNER),
            text(SEEDED_AT),
        ],
    );
    kit.db.execute(&row).await.expect("the workspace row seeds");
    let member = Statement::with_values(
        "INSERT INTO workspace_members (workspace_id, user_id, name, timezone, is_admin, \
         updated_at) VALUES (?, ?, ?, ?, ?, ?)",
        vec![
            text(WORKSPACE),
            text(OWNER),
            text("A member"),
            sea_query::Value::String(None),
            sea_query::Value::BigInt(Some(1)),
            text(SEEDED_AT),
        ],
    );
    kit.db.execute(&member).await.expect("the member row seeds");
    // The founder is an owner, as migration workspaces/0005 backfills.
    kit.db
        .execute(&Statement::new("UPDATE workspace_members SET role = 'owner' WHERE EXISTS (SELECT 1 FROM workspaces w WHERE w.id = workspace_members.workspace_id AND w.owner_id = workspace_members.user_id)"))
        .await
        .expect("the owner role seeds");
    kit
}

/// A session cookie value sealed exactly as the workspaces module seals the
/// one the magic link sets.
fn session(kit: &TestHarness, workspace: &str, user: &str) -> String {
    kit.signer.sign(&Payload {
        purpose: SESSION_PURPOSE.to_owned(),
        subject: json!({
            "team_id": workspace,
            "user_id": user,
            "exp": kit.clock.0.unix_timestamp() + 3600,
        })
        .to_string(),
        exp: None,
        kid: Kid::Cur,
    })
}

/// How a request arrives: the browser's session, or a bearer token.
#[derive(Clone, Copy)]
enum Via<'a> {
    /// The web app's `fetch` on its own origin: the cookie, `sec-fetch-site:
    /// same-origin`, and — on a write — `Origin` naming this host.
    App(&'a str),
    /// The CLI or a GPT: a bearer token, no cookie and no browser signals.
    Bearer(&'a str),
}

fn headers(from: Via<'_>, write: bool) -> HeaderMap {
    let value = |text: &str| HeaderValue::from_str(text).expect("a header value");
    let mut h = HeaderMap::new();
    h.insert(header::HOST, value(HOST));
    match from {
        Via::App(sealed) => {
            h.insert(header::COOKIE, value(&format!("{SESSION_COOKIE}={sealed}")));
            h.insert("sec-fetch-site", value("same-origin"));
            if write {
                h.insert(header::ORIGIN, value(ORIGIN));
            }
        }
        Via::Bearer(token) => {
            h.insert(header::AUTHORIZATION, value(&format!("Bearer {token}")));
        }
    }
    h
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

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

async fn send(
    kit: &TestHarness,
    method: Method,
    path: &str,
    from: Via<'_>,
    body: Option<Value>,
) -> Res {
    let write = !method.is_safe();
    let mut builder = Request::builder().method(method).uri(path);
    for (name, value) in &headers(from, write) {
        builder = builder.header(name, value.clone());
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

fn assert_status(res: &Res, status: StatusCode, what: &str) {
    assert_eq!(res.status, status, "{what}: {}", res.text());
}

/// A token minted over the wire, exactly as the settings page mints one.
async fn mint(kit: &TestHarness, sealed: &str, name: &str, scopes: &[&str]) -> String {
    let created = send(
        kit,
        Method::POST,
        "/v1/tokens",
        Via::App(sealed),
        Some(json!({ "name": name, "scopes": scopes })),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.text());
    created.json()["token"]
        .as_str()
        .expect("the token, once")
        .to_owned()
}

/// The row an audience-carrying mint writes, stamped straight onto a token
/// minted over the wire: the audience, and the scope subset exactly as
/// `mint_actions` forces it for `actions` (each entry `read:`-prefixed),
/// as granted for `mcp`.
async fn stamp(kit: &TestHarness, token: &str, audience: &str, scopes: &str) {
    let prefix = token.rsplit_once('_').expect("a prefix").0;
    kit.db
        .execute(&Statement::with_values(
            "UPDATE personal_access_tokens SET audience = ?, scopes = ? WHERE prefix = ?",
            vec![text(audience), text(scopes), text(prefix)],
        ))
        .await
        .expect("the audience is stamped");
}

fn page(markdown: &str, base_version: Option<u64>) -> Value {
    json!({ "markdown": markdown, "base_version": base_version, "title": "Launch plan" })
}

fn initialize() -> Value {
    json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2025-06-18", "capabilities": {},
                        "clientInfo": { "name": "test", "version": "0" } } })
}

/// A GPT's scope subset, stamped as `mint_actions` stamps it.
fn gpt_scopes() -> String {
    format!("read:shared read:user:{OWNER}")
}

/// The four routes a GPT's Actions are pointed at answer it — one page,
/// search, facts and ask — and nothing else on the same surfaces does.
#[pollster::test]
async fn an_actions_token_reads_its_four_routes_and_nothing_else() {
    let kit = seeded().await;
    let alice = session(&kit, WORKSPACE, OWNER);
    let created = send(
        &kit,
        Method::PUT,
        "/v1/pages/launch-plan",
        Via::App(&alice),
        Some(page("# Launch plan\n\nShip on Tuesday.\n", None)),
    )
    .await;
    assert_status(&created, StatusCode::OK, "the page exists");
    let gpt = mint(
        &kit,
        &alice,
        "ChatGPT",
        &["shared", &format!("user:{OWNER}")],
    )
    .await;
    stamp(&kit, &gpt, "actions", &gpt_scopes()).await;

    // The four allowed routes.
    let read = send(
        &kit,
        Method::GET,
        "/v1/pages/launch-plan",
        Via::Bearer(&gpt),
        None,
    )
    .await;
    assert_status(&read, StatusCode::OK, "one page");
    assert!(
        read.json()["markdown"]
            .as_str()
            .unwrap_or_default()
            .contains("Tuesday")
    );

    let found = send(
        &kit,
        Method::GET,
        "/v1/search?q=Tuesday&limit=10",
        Via::Bearer(&gpt),
        None,
    )
    .await;
    assert_status(&found, StatusCode::OK, "search");
    assert!(
        found.json()["results"]
            .as_array()
            .is_some_and(|results| !results.is_empty()),
        "the search sees the page: {}",
        found.text()
    );

    let facts = send(&kit, Method::GET, "/v1/facts", Via::Bearer(&gpt), None).await;
    assert_status(&facts, StatusCode::OK, "the facts list");

    let asked = send(
        &kit,
        Method::POST,
        "/v1/ask",
        Via::Bearer(&gpt),
        Some(json!({ "question": "When do we ship?" })),
    )
    .await;
    assert_status(&asked, StatusCode::OK, "ask");

    // The pages surface refuses it everywhere but the one page: the list, a
    // write on the page, a write on a page that does not exist, and the MCP
    // endpoint nested under the same mount — which answers POST alone, so a
    // GPT probing it with GET does not even reach an auth check.
    for (method, path, body) in [
        (Method::GET, "/v1/pages", None),
        (Method::GET, "/v1/pages?limit=200", None),
        (
            Method::PUT,
            "/v1/pages/launch-plan",
            Some(page("# Overwritten\n", None)),
        ),
        (
            Method::PUT,
            "/v1/pages/a-second-page",
            Some(page("# Overwritten\n", None)),
        ),
        (
            Method::POST,
            "/v1/pages/mcp",
            Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" })),
        ),
    ] {
        let refused = send(&kit, method.clone(), path, Via::Bearer(&gpt), body).await;
        assert_eq!(
            refused.status,
            StatusCode::UNAUTHORIZED,
            "{method} {path}: {}",
            refused.text()
        );
    }
    let not_a_route = send(&kit, Method::GET, "/v1/pages/mcp", Via::Bearer(&gpt), None).await;
    assert_status(
        &not_a_route,
        StatusCode::METHOD_NOT_ALLOWED,
        "the MCP endpoint serves POST alone",
    );
    let absent = send(
        &kit,
        Method::GET,
        "/v1/pages/a-second-page",
        Via::App(&alice),
        None,
    )
    .await;
    assert_status(&absent, StatusCode::NOT_FOUND, "nothing was created");
    let still = send(
        &kit,
        Method::GET,
        "/v1/pages/launch-plan",
        Via::App(&alice),
        None,
    )
    .await;
    assert!(
        !still.json()["markdown"]
            .as_str()
            .unwrap_or_default()
            .contains("Overwritten"),
        "nothing was written"
    );

    // Every other module: the rest of the CLI surface, the member's own
    // `me`, and the token routes a GPT must never reach.
    for (method, path, body) in [
        (Method::POST, "/v1/notes", Some(json!({ "body": "A note" }))),
        (Method::GET, "/v1/export?format=obsidian", None),
        (Method::GET, "/v1/graph", None),
        (Method::GET, "/v1/sources", None),
        (Method::GET, "/v1/me", None),
        (Method::GET, "/v1/tokens", None),
        (
            Method::POST,
            "/v1/tokens",
            Some(json!({ "name": "A machine" })),
        ),
    ] {
        let refused = send(&kit, method.clone(), path, Via::Bearer(&gpt), body).await;
        assert_eq!(
            refused.status,
            StatusCode::UNAUTHORIZED,
            "{method} {path}: {}",
            refused.text()
        );
    }
}

/// The one write-shaped route among the four, `POST /v1/facts`, still writes
/// nothing: the token's scopes are `read:`-only, the access model's own
/// vocabulary, so the write is refused by the grants plan itself.
#[pollster::test]
async fn an_actions_token_writes_no_facts() {
    let kit = seeded().await;
    let alice = session(&kit, WORKSPACE, OWNER);
    let gpt = mint(&kit, &alice, "ChatGPT", &["shared"]).await;
    stamp(&kit, &gpt, "actions", "read:shared").await;

    let fact = json!({ "format": 1, "facts": [{
        "subject": "me", "attribute": "shoe_size", "value": "40", "unit": "EU",
    }]});
    let refused = send(
        &kit,
        Method::POST,
        "/v1/facts",
        Via::Bearer(&gpt),
        Some(fact),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "a read-only subset writes no fact: {}",
        refused.text()
    );
    let facts = send(&kit, Method::GET, "/v1/facts", Via::App(&alice), None).await;
    assert_status(&facts, StatusCode::OK, "the facts list");
    assert!(
        facts.json()["keys"]
            .as_array()
            .is_some_and(|keys: &Vec<Value>| keys.is_empty()),
        "nothing was written: {}",
        facts.text()
    );
}

/// The credentials that already worked keep their reach under the guard and
/// the actions bearer: the session and an ordinary token still list and
/// write pages, and a connector token is still refused by every REST route
/// while the MCP endpoint keeps answering it.
#[pollster::test]
async fn ordinary_credentials_and_the_connector_keep_their_reach() {
    let kit = seeded().await;
    let alice = session(&kit, WORKSPACE, OWNER);
    let created = send(
        &kit,
        Method::PUT,
        "/v1/pages/launch-plan",
        Via::App(&alice),
        Some(page("# Launch plan\n\nShip on Tuesday.\n", None)),
    )
    .await;
    assert_status(&created, StatusCode::OK, "the page exists");
    let version = created.json()["version"].as_u64().expect("a version");

    // The session still lists, reads and writes pages through the guard.
    let listed = send(
        &kit,
        Method::GET,
        "/v1/pages?limit=200",
        Via::App(&alice),
        None,
    )
    .await;
    assert_status(&listed, StatusCode::OK, "the session lists");
    let edited = send(
        &kit,
        Method::PUT,
        "/v1/pages/launch-plan",
        Via::App(&alice),
        Some(page("# Launch plan\n\nShip on Wednesday.\n", Some(version))),
    )
    .await;
    assert_status(&edited, StatusCode::OK, "the session edits");

    // An ordinary token too: it is not the guard's business — it carries no
    // audience — and the actions bearer answers it exactly as `TokenBearer`
    // does.
    let pat = mint(&kit, &alice, "A full key", &[]).await;
    let listed = send(
        &kit,
        Method::GET,
        "/v1/pages?limit=200",
        Via::Bearer(&pat),
        None,
    )
    .await;
    assert_status(&listed, StatusCode::OK, "an ordinary token lists");
    let written = send(
        &kit,
        Method::PUT,
        "/v1/pages/launch-plan",
        Via::Bearer(&pat),
        Some(page(
            "# Launch plan\n\nShip on Thursday.\n",
            Some(version + 1),
        )),
    )
    .await;
    assert_status(&written, StatusCode::OK, "an ordinary token edits");

    // A connector token: still REST-refused — the list, the one page an
    // actions token may read, ask — and still answered by the MCP endpoint.
    let connector = mint(&kit, &alice, "Claude", &["shared"]).await;
    stamp(&kit, &connector, "mcp", "shared").await;
    for (method, path) in [
        (Method::GET, "/v1/pages?limit=200"),
        (Method::GET, "/v1/pages/launch-plan"),
        (Method::POST, "/v1/ask"),
    ] {
        let refused = send(
            &kit,
            method.clone(),
            path,
            Via::Bearer(&connector),
            Some(json!({ "question": "Tuesday" })),
        )
        .await;
        assert_eq!(
            refused.status,
            StatusCode::UNAUTHORIZED,
            "{method} {path}: {}",
            refused.text()
        );
    }
    let handshake = send(
        &kit,
        Method::POST,
        "/v1/pages/mcp",
        Via::Bearer(&connector),
        Some(initialize()),
    )
    .await;
    assert_status(
        &handshake,
        StatusCode::OK,
        "the MCP endpoint answers a connector",
    );
}
