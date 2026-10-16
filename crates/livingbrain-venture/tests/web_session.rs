//! The web app's session cookie on the wiki routes (`/v1/pages`,
//! `/v1/search`, `/v1/ask`, `/v1/notes`, `/v1/export`, `/v1/sources`).
//!
//! The app never sends an `Authorization` header — its session is the
//! `HttpOnly` cookie the email magic link set — so these routes must answer
//! to that cookie, or a signed-in person opening `/wiki` is told to sign in.
//! The composition here is the venture's own: the credentials come from
//! [`livingbrain_venture::wiki`] (the same `TokenBearer` and `WebSession` the
//! Worker wires), the session is sealed the way the workspaces module seals
//! one, and the personal access token is minted through `POST /v1/tokens`
//! exactly as the settings page mints one. Everything goes through the
//! harness router, so CORS, the security headers and problem+json all stand.

use std::sync::Arc;

use cratefield_core::axum::body::Body;
use cratefield_core::axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header};
use cratefield_core::{Blob, Kid, Module, Payload, Signer, Statement};
use cratefield_kms::{Dek, Kms, LocalFileKms};
use cratefield_testing::{MemoryBlob, TestHarness};
use livingbrain_api::{
    Ask, Export, Facts, Graph, Notes, PagesRebind, Search, Sources, pages_routes,
};
use livingbrain_pages::Pages;
use livingbrain_tokens::Tokens;
use livingbrain_workspaces::{SESSION_COOKIE, SESSION_PURPOSE, Workspaces};
use serde_json::{Value, json};
use tower::ServiceExt;

/// The host the requests reach — the one the web app and the Worker share.
const HOST: &str = "app.livingbrain.wiki";
const ORIGIN: &str = "https://app.livingbrain.wiki";

const WORKSPACE: &str = "T0WEBSPACE";
const ALICE: &str = "U0ALICE";
/// A second workspace with its own member, for the isolation case.
const OTHER_WORKSPACE: &str = "T1ELSEWHERE";
const MALLORY: &str = "U1MALLORY";
const SEEDED_AT: &str = "2026-01-01T00:00:00Z";

/// The venture's composition of everything the wiki page touches: the
/// session's owner (`workspaces`), the token minter (`tokens`), the pages
/// module with the page routes merged at its root, and the five CLI modules
/// — all built from the one [`livingbrain_venture::wiki`], over a body
/// store rooted on the pages key space the way `serve_cli` plants it.
fn kit() -> TestHarness {
    let kms: Arc<dyn Kms> = Arc::new(
        LocalFileKms::from_key(Dek::generate().expect("a key"), "test-kek", "test")
            .expect("a well-formed key"),
    );
    let wiki = livingbrain_venture::wiki(kms);
    let surface = wiki.clone();
    let modules: Vec<Box<dyn Module>> = vec![
        Box::new(Workspaces::new()),
        Box::new(Tokens::new()),
        Box::new(Pages::new().surface(move |ctx| pages_routes(ctx, &surface))),
        Box::new(Notes::new(wiki.clone())),
        Box::new(Search::new(wiki.clone())),
        Box::new(Ask::new(wiki.clone())),
        Box::new(Export::new(wiki.clone())),
        Box::new(Facts::new(wiki.clone())),
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

/// Two workspaces, one member each.
async fn seeded() -> TestHarness {
    let kit = kit();
    for (workspace, user) in [(WORKSPACE, ALICE), (OTHER_WORKSPACE, MALLORY)] {
        let row = Statement::with_values(
            "INSERT INTO workspaces (id, name, owner_id, created_at) VALUES (?, ?, ?, ?)",
            vec![
                text(workspace),
                text("A workspace"),
                text(user),
                text(SEEDED_AT),
            ],
        );
        kit.db.execute(&row).await.expect("the workspace row seeds");
        let member = Statement::with_values(
            "INSERT INTO workspace_members (workspace_id, user_id, name, timezone, is_admin, \
             updated_at) VALUES (?, ?, ?, ?, ?, ?)",
            vec![
                text(workspace),
                text(user),
                text("A member"),
                sea_query::Value::String(None),
                sea_query::Value::BigInt(Some(1)),
                text(SEEDED_AT),
            ],
        );
        kit.db.execute(&member).await.expect("the member row seeds");
        // The founder is an owner, as migration workspaces/0005 backfills.
        kit.db
            .execute(&cratefield_core::Statement::new("UPDATE workspace_members SET role = 'owner' WHERE EXISTS (SELECT 1 FROM workspaces w WHERE w.id = workspace_members.workspace_id AND w.owner_id = workspace_members.user_id)"))
            .await
            .expect("the owner role seeds");
    }
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

/// How a request arrives: the credential and the browser's own signals.
#[derive(Clone, Copy)]
enum Via<'a> {
    /// The web app's `fetch` on its own origin: the cookie, `sec-fetch-site:
    /// same-origin`, and — on a write — `Origin` naming this host.
    App(&'a str),
    /// The same cookie on a request another site's page made.
    CrossSite(&'a str),
    /// The same cookie from a sibling subdomain.
    SameSite(&'a str),
    /// The same cookie, from an old browser that sends no `sec-fetch-site`
    /// but does send `Origin` — another one.
    ForeignOrigin(&'a str),
    /// The CLI: a bearer token, no cookie and no browser signals.
    Bearer(&'a str),
    /// A bearer token on a request a browser marked cross-site.
    BearerCrossSite(&'a str),
    /// A cookie plus an `Authorization` header that names nobody.
    BadBearerWithCookie(&'a str),
    Anonymous,
}

fn headers(from: Via<'_>, write: bool) -> HeaderMap {
    let value = |text: &str| HeaderValue::from_str(text).expect("a header value");
    let cookie = |sealed: &str| value(&format!("{SESSION_COOKIE}={sealed}"));
    let mut h = HeaderMap::new();
    h.insert(header::HOST, value(HOST));
    match from {
        Via::App(sealed) => {
            h.insert(header::COOKIE, cookie(sealed));
            h.insert("sec-fetch-site", value("same-origin"));
            if write {
                h.insert(header::ORIGIN, value(ORIGIN));
            }
        }
        Via::CrossSite(sealed) => {
            h.insert(header::COOKIE, cookie(sealed));
            h.insert("sec-fetch-site", value("cross-site"));
            h.insert(header::ORIGIN, value("https://evil.example"));
        }
        Via::SameSite(sealed) => {
            h.insert(header::COOKIE, cookie(sealed));
            h.insert("sec-fetch-site", value("same-site"));
            h.insert(header::ORIGIN, value("https://livingbrain.wiki"));
        }
        Via::ForeignOrigin(sealed) => {
            h.insert(header::COOKIE, cookie(sealed));
            h.insert(header::ORIGIN, value("https://evil.example"));
        }
        Via::Bearer(token) => {
            h.insert(header::AUTHORIZATION, value(&format!("Bearer {token}")));
        }
        Via::BearerCrossSite(token) => {
            h.insert(header::AUTHORIZATION, value(&format!("Bearer {token}")));
            h.insert("sec-fetch-site", value("cross-site"));
            h.insert(header::ORIGIN, value("https://evil.example"));
        }
        Via::BadBearerWithCookie(sealed) => {
            h.insert(header::COOKIE, cookie(sealed));
            h.insert("sec-fetch-site", value("same-origin"));
            h.insert(header::AUTHORIZATION, value("Bearer lb_not_a_token"));
        }
        Via::Anonymous => {}
    }
    h
}

struct Res {
    status: StatusCode,
    headers: HeaderMap,
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
        headers: parts.headers,
        body: bytes.to_vec(),
    }
}

fn page(markdown: &str, base_version: Option<u64>) -> Value {
    json!({ "markdown": markdown, "base_version": base_version, "title": "Launch plan" })
}

fn assert_status(res: &Res, status: StatusCode, what: &str) {
    assert_eq!(res.status, status, "{what}: {}", res.text());
}

fn assert_cross_site(res: &Res, what: &str) {
    assert_status(res, StatusCode::FORBIDDEN, what);
    assert_eq!(
        res.json()["type"]
            .as_str()
            .map(|t| t.ends_with("api/cross-site-request")),
        Some(true),
        "{what}: the cross-site problem, not some other 403: {}",
        res.text()
    );
}

/// The bug: a signed-in browser reading the wiki got 401 on every call.
#[pollster::test]
async fn the_session_cookie_reads_the_wiki_and_writes_from_its_own_origin() {
    let kit = seeded().await;
    let alice = session(&kit, WORKSPACE, ALICE);

    // The home list, before and after a page exists.
    let empty = send(
        &kit,
        Method::GET,
        "/v1/pages?limit=200",
        Via::App(&alice),
        None,
    )
    .await;
    assert_status(&empty, StatusCode::OK, "the list answers the cookie");
    assert_eq!(empty.json()["pages"], json!([]));
    assert_eq!(
        empty
            .headers
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
        Some("no-store"),
        "a cookie-authenticated answer is never cached for somebody else"
    );

    // The editor's save: a new page, then an edit at the version it read.
    let created = send(
        &kit,
        Method::PUT,
        "/v1/pages/launch-plan",
        Via::App(&alice),
        Some(page("# Launch plan\n\nShip on Tuesday.\n", None)),
    )
    .await;
    assert_status(&created, StatusCode::OK, "a same-origin save");
    let version = created.json()["version"].as_u64().expect("a version");

    let read = send(
        &kit,
        Method::GET,
        "/v1/pages/launch-plan",
        Via::App(&alice),
        None,
    )
    .await;
    assert_status(&read, StatusCode::OK, "openPage");
    assert!(
        read.json()["markdown"]
            .as_str()
            .unwrap_or_default()
            .contains("Ship on Tuesday")
    );

    let listed = send(&kit, Method::GET, "/v1/pages", Via::App(&alice), None).await;
    assert_status(&listed, StatusCode::OK, "loadHome");
    assert_eq!(listed.json()["pages"][0]["slug"], "launch-plan");

    let edited = send(
        &kit,
        Method::PUT,
        "/v1/pages/launch-plan",
        Via::App(&alice),
        Some(page("# Launch plan\n\nShip on Wednesday.\n", Some(version))),
    )
    .await;
    assert_status(&edited, StatusCode::OK, "a same-origin edit");

    // The rest of the wiki page: search, ask, the projects filter, the
    // sessions view, and export.
    let found = send(
        &kit,
        Method::GET,
        "/v1/search?q=Wednesday&limit=10",
        Via::App(&alice),
        None,
    )
    .await;
    assert_status(&found, StatusCode::OK, "search");
    assert_eq!(found.json()["results"].as_array().map(Vec::len), Some(1));

    let asked = send(
        &kit,
        Method::POST,
        "/v1/ask",
        Via::App(&alice),
        Some(json!({ "question": "Wednesday" })),
    )
    .await;
    assert_status(&asked, StatusCode::OK, "ask");

    let projects = send(
        &kit,
        Method::GET,
        "/v1/sources/projects",
        Via::App(&alice),
        None,
    )
    .await;
    assert_status(&projects, StatusCode::OK, "the projects dropdown");
    let sessions = send(
        &kit,
        Method::GET,
        "/v1/sources?kind=agent_log&limit=50",
        Via::App(&alice),
        None,
    )
    .await;
    assert_status(&sessions, StatusCode::OK, "the sessions view");

    let noted = send(
        &kit,
        Method::POST,
        "/v1/notes",
        Via::App(&alice),
        Some(json!({ "body": "Remember the launch checklist." })),
    )
    .await;
    assert_status(&noted, StatusCode::OK, "a same-origin note");

    let export = send(
        &kit,
        Method::GET,
        "/v1/export?format=obsidian",
        Via::App(&alice),
        None,
    )
    .await;
    assert_status(&export, StatusCode::OK, "export");
}

/// A cookie is a credential the browser attaches for whoever wrote the page,
/// so a cookie write from anywhere but this origin is refused — and nothing
/// is written.
#[pollster::test]
async fn a_cookie_write_from_another_site_is_refused_and_writes_nothing() {
    let kit = seeded().await;
    let alice = session(&kit, WORKSPACE, ALICE);
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

    for (from, what) in [
        (Via::CrossSite(&alice), "sec-fetch-site: cross-site"),
        (Via::SameSite(&alice), "a sibling subdomain"),
        (Via::ForeignOrigin(&alice), "Origin: https://evil.example"),
    ] {
        let put = send(
            &kit,
            Method::PUT,
            "/v1/pages/launch-plan",
            from,
            Some(page("# Launch plan\n\nPwned.\n", Some(version))),
        )
        .await;
        assert_cross_site(&put, what);

        let note = send(
            &kit,
            Method::POST,
            "/v1/notes",
            from,
            Some(json!({ "body": "Planted." })),
        )
        .await;
        assert_cross_site(&note, what);

        let ask = send(
            &kit,
            Method::POST,
            "/v1/ask",
            from,
            Some(json!({ "question": "launch" })),
        )
        .await;
        assert_cross_site(&ask, what);
    }

    let read = send(
        &kit,
        Method::GET,
        "/v1/pages/launch-plan",
        Via::App(&alice),
        None,
    )
    .await;
    assert_eq!(
        read.json()["version"].as_u64(),
        Some(version),
        "nothing was written"
    );
    assert!(!read.text().contains("Pwned"));
    let listed = send(&kit, Method::GET, "/v1/pages", Via::App(&alice), None).await;
    assert_eq!(
        listed.json()["pages"].as_array().map(Vec::len),
        Some(1),
        "no note page was planted: {}",
        listed.text()
    );
}

/// The CLI's path is unchanged: a bearer reads and writes, and — carrying
/// nothing a browser attaches on its own — is never held to the origin
/// check. A rejected bearer is a 401 even with a good cookie beside it.
#[pollster::test]
async fn a_bearer_still_works_and_a_bad_one_never_falls_back_to_the_cookie() {
    let kit = seeded().await;
    let alice = session(&kit, WORKSPACE, ALICE);

    // Minted the way the settings page mints one.
    let minted = send(
        &kit,
        Method::POST,
        "/v1/tokens",
        Via::App(&alice),
        Some(json!({ "name": "laptop", "scopes": [] })),
    )
    .await;
    assert_status(&minted, StatusCode::CREATED, "a token is minted");
    let token = minted.json()["token"].as_str().expect("a token").to_owned();

    let put = send(
        &kit,
        Method::PUT,
        "/v1/pages/from-cli",
        Via::Bearer(&token),
        Some(page("# From the CLI\n", None)),
    )
    .await;
    assert_status(&put, StatusCode::OK, "a bearer write");
    let version = put.json()["version"].as_u64().expect("a version");

    let read = send(
        &kit,
        Method::GET,
        "/v1/pages/from-cli",
        Via::Bearer(&token),
        None,
    )
    .await;
    assert_status(&read, StatusCode::OK, "a bearer read");

    let exempt = send(
        &kit,
        Method::PUT,
        "/v1/pages/from-cli",
        Via::BearerCrossSite(&token),
        Some(page("# From the CLI\n\nAgain.\n", Some(version))),
    )
    .await;
    assert_status(
        &exempt,
        StatusCode::OK,
        "a bearer is not an ambient credential",
    );

    // The cookie and the bearer name the same member: the CLI's page is on
    // the browser's wiki.
    let in_browser = send(
        &kit,
        Method::GET,
        "/v1/pages/from-cli",
        Via::App(&alice),
        None,
    )
    .await;
    assert_status(&in_browser, StatusCode::OK, "one member, two credentials");

    let bad = send(
        &kit,
        Method::GET,
        "/v1/pages",
        Via::BadBearerWithCookie(&alice),
        None,
    )
    .await;
    assert_status(
        &bad,
        StatusCode::UNAUTHORIZED,
        "a rejected bearer decides the request",
    );
}

/// No credential, or a cookie that does not verify, is the one 401.
#[pollster::test]
async fn no_credential_and_a_forged_cookie_are_401() {
    let kit = seeded().await;
    for path in [
        "/v1/pages",
        "/v1/pages/launch-plan",
        "/v1/search?q=launch",
        "/v1/sources/projects",
        "/v1/sources?kind=agent_log",
        "/v1/graph",
        "/v1/graph?limit=5",
        "/v1/export",
    ] {
        let anon = send(&kit, Method::GET, path, Via::Anonymous, None).await;
        assert_status(&anon, StatusCode::UNAUTHORIZED, path);
        let forged = send(&kit, Method::GET, path, Via::App("forged.cookie"), None).await;
        assert_status(&forged, StatusCode::UNAUTHORIZED, path);
    }
    let anon_put = send(
        &kit,
        Method::PUT,
        "/v1/pages/launch-plan",
        Via::Anonymous,
        Some(page("# Launch plan\n", None)),
    )
    .await;
    assert_status(&anon_put, StatusCode::UNAUTHORIZED, "an anonymous write");
}

/// A cookie from another workspace reads only its own workspace's pages.
#[pollster::test]
async fn a_cookie_from_another_workspace_cannot_read_the_first_ones_pages() {
    let kit = seeded().await;
    let alice = session(&kit, WORKSPACE, ALICE);
    let mallory = session(&kit, OTHER_WORKSPACE, MALLORY);
    let created = send(
        &kit,
        Method::PUT,
        "/v1/pages/launch-plan",
        Via::App(&alice),
        Some(page("# Launch plan\n\nThe secret date.\n", None)),
    )
    .await;
    assert_status(&created, StatusCode::OK, "alice's page");

    let read = send(
        &kit,
        Method::GET,
        "/v1/pages/launch-plan",
        Via::App(&mallory),
        None,
    )
    .await;
    assert_status(&read, StatusCode::NOT_FOUND, "another workspace's page");
    let listed = send(&kit, Method::GET, "/v1/pages", Via::App(&mallory), None).await;
    assert_status(&listed, StatusCode::OK, "mallory's own list");
    assert_eq!(listed.json()["pages"], json!([]));
    let found = send(
        &kit,
        Method::GET,
        "/v1/search?q=secret",
        Via::App(&mallory),
        None,
    )
    .await;
    assert_status(&found, StatusCode::OK, "mallory's search");
    assert_eq!(found.json()["results"], json!([]));
    assert!(!found.text().contains("secret date"));

    // A write under the same slug lands in mallory's own workspace and
    // leaves alice's page untouched.
    let own = send(
        &kit,
        Method::PUT,
        "/v1/pages/launch-plan",
        Via::App(&mallory),
        Some(page("# Launch plan\n\nMallory's own.\n", None)),
    )
    .await;
    assert_status(&own, StatusCode::OK, "mallory's own page");
    let alices = send(
        &kit,
        Method::GET,
        "/v1/pages/launch-plan",
        Via::App(&alice),
        None,
    )
    .await;
    assert!(alices.text().contains("The secret date"));
    assert!(!alices.text().contains("Mallory"));
}

/// Temporal facts (`/v1/facts`, docs/temporal-facts.md) answer the same two
/// credentials as every wiki route: the cookie reads, and writes only from
/// its own origin; a bearer writes from anywhere; another workspace's
/// cookie reads nothing.
#[pollster::test]
async fn facts_answer_the_cookie_from_its_own_origin_and_the_bearer() {
    let kit = seeded().await;
    let alice = session(&kit, WORKSPACE, ALICE);
    let fact = |value: &str, from: &str| {
        json!({ "format": 1, "facts": [{
            "subject": "me", "attribute": "shoe_size", "value": value,
            "unit": "EU", "valid_from": from,
        }]})
    };

    let refused = send(
        &kit,
        Method::POST,
        "/v1/facts",
        Via::CrossSite(&alice),
        Some(fact("40", "2025")),
    )
    .await;
    assert_cross_site(&refused, "a cross-site cookie write");
    let foreign = send(
        &kit,
        Method::POST,
        "/v1/facts",
        Via::ForeignOrigin(&alice),
        Some(fact("40", "2025")),
    )
    .await;
    assert_cross_site(&foreign, "a foreign-origin cookie write");

    let written = send(
        &kit,
        Method::POST,
        "/v1/facts",
        Via::App(&alice),
        Some(fact("40", "2025")),
    )
    .await;
    assert_status(&written, StatusCode::CREATED, "a same-origin fact");
    let first = written.json()["facts"][0]["id"]
        .as_str()
        .expect("an id")
        .to_owned();

    let minted = send(
        &kit,
        Method::POST,
        "/v1/tokens",
        Via::App(&alice),
        Some(json!({ "name": "laptop", "scopes": [] })),
    )
    .await;
    assert_status(&minted, StatusCode::CREATED, "a token is minted");
    let token = minted.json()["token"].as_str().expect("a token").to_owned();
    let bearer = send(
        &kit,
        Method::POST,
        "/v1/facts",
        Via::BearerCrossSite(&token),
        Some(fact("41", "2026")),
    )
    .await;
    assert_status(
        &bearer,
        StatusCode::CREATED,
        "a bearer write needs no origin",
    );

    let read = send(
        &kit,
        Method::GET,
        "/v1/facts?subject=me&attribute=shoe_size",
        Via::App(&alice),
        None,
    )
    .await;
    assert_status(&read, StatusCode::OK, "a cookie read");
    let key = &read.json()["keys"][0];
    assert_eq!(key["current"][0]["value"], "41", "{key}");
    assert_eq!(key["history"][1]["id"], first.as_str(), "{key}");
    assert_eq!(key["history"][1]["status"], "superseded", "{key}");

    let historical = send(
        &kit,
        Method::GET,
        "/v1/facts?subject=me&as_of=2025",
        Via::Bearer(&token),
        None,
    )
    .await;
    assert_status(&historical, StatusCode::OK, "a bearer read as of 2025");
    assert_eq!(historical.json()["keys"][0]["current"][0]["value"], "40");

    let retract = send(
        &kit,
        Method::POST,
        &format!("/v1/facts/{first}/retract"),
        Via::CrossSite(&alice),
        None,
    )
    .await;
    assert_cross_site(&retract, "a cross-site retraction");

    let mallory = session(&kit, OTHER_WORKSPACE, MALLORY);
    let elsewhere = send(&kit, Method::GET, "/v1/facts", Via::App(&mallory), None).await;
    assert_status(&elsewhere, StatusCode::OK, "another workspace reads");
    assert_eq!(
        elsewhere.json()["keys"],
        json!([]),
        "and sees nothing of the first"
    );
    let stolen = send(
        &kit,
        Method::POST,
        &format!("/v1/facts/{first}/retract"),
        Via::App(&mallory),
        None,
    )
    .await;
    assert_status(
        &stolen,
        StatusCode::NOT_FOUND,
        "another workspace cannot retract",
    );

    let anonymous = send(&kit, Method::GET, "/v1/facts", Via::Anonymous, None).await;
    assert_status(&anonymous, StatusCode::UNAUTHORIZED, "no credential");
}

/// H1: a connector token (audience `mcp`, the one the OAuth grant mints) is
/// accepted by the MCP endpoint alone. A page route, which resolves bearers
/// through the general resolver, answers it as an unknown token.
#[pollster::test]
async fn a_connector_token_is_refused_by_the_pages_routes() {
    let kit = seeded().await;
    let alice = session(&kit, WORKSPACE, ALICE);
    let minted = send(
        &kit,
        Method::POST,
        "/v1/tokens",
        Via::App(&alice),
        Some(json!({ "name": "connected app", "scopes": [] })),
    )
    .await;
    assert_status(&minted, StatusCode::CREATED, "a token is minted");
    let token = minted.json()["token"].as_str().expect("a token").to_owned();
    let prefix = token
        .rsplit_once('_')
        .expect("a token has a secret half")
        .0
        .to_owned();

    // The same row the OAuth grant writes for a connector.
    kit.db
        .execute(&Statement::with_values(
            "UPDATE personal_access_tokens SET audience = 'mcp' WHERE prefix = ?",
            vec![text(&prefix)],
        ))
        .await
        .expect("the audience is stamped");

    let read = send(
        &kit,
        Method::GET,
        "/v1/pages/from-cli",
        Via::Bearer(&token),
        None,
    )
    .await;
    assert_status(
        &read,
        StatusCode::UNAUTHORIZED,
        "a connector cannot read pages",
    );
    let write = send(
        &kit,
        Method::PUT,
        "/v1/pages/from-connector",
        Via::Bearer(&token),
        Some(page("# Written by a connected app\n", None)),
    )
    .await;
    assert_status(
        &write,
        StatusCode::UNAUTHORIZED,
        "a connector cannot write pages",
    );
}
