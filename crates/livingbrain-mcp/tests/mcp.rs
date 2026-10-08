//! The MCP endpoint, driven through the harness router the Worker mounts.
//!
//! The test that matters is `an_agent_sees_only_its_users_pages`: two users in
//! one workspace and the same user id in a second workspace, each with a
//! private page, plus a shared page both may read. Every read tool has to come
//! back with the caller's own pages and the shared one — and with nothing
//! else, not even a page the caller would be entitled to under a different
//! workspace.

use std::collections::BTreeMap;
use std::sync::Arc;

use cratefield_core::axum::http::{Method, StatusCode, header};
use cratefield_core::{Blob, Clock, Ports, ScopedBlob, UlidIdGen};
use cratefield_kms::{Dek, Kms, LocalFileKms};
use cratefield_testing::{MemoryBlob, TestHarness, TestResponse, request, request_as};
use livingbrain_access::{Scope, UserId};
use livingbrain_mcp::{Asker, AuthError, BearerAuth, page_scope};
use livingbrain_pages::{Author, EntityType, Page, PageStore, PageWrite, Pages};
use serde_json::{Value, json};

/// The endpoint, nested in the pages module: its `Blob` is scoped to `pages`,
/// which is the whole reason the surface lives there.
const ENDPOINT: &str = "/v1/pages/mcp";

/// What a refused call says about where to authenticate when the request
/// named no host at all: the production origin, the same fallback the
/// discovery documents' compiled-in constant covers.
const CHALLENGE: &str = "Bearer resource_metadata=\"https://mcp.livingbrain.wiki/.well-known/oauth-protected-resource\"";

/// A bearer resolver over a fixed table: a known token names an asker, an
/// unknown one names nobody. That is the whole of what #72 replaces.
#[derive(Default)]
struct Tokens(BTreeMap<String, Asker>);

#[async_trait::async_trait]
impl BearerAuth for Tokens {
    async fn authenticate(&self, _ports: &Ports, token: &str) -> Result<Asker, AuthError> {
        self.0.get(token).cloned().ok_or(AuthError::Rejected)
    }
}

/// The default three askers: two people in one workspace, and the *same*
/// person id in a second workspace, which is the case the page scope has to
/// fold the workspace into.
fn world() -> Fixture {
    let askers = [
        ("token-a", "ws-one", "u-a"),
        ("token-b", "ws-one", "u-b"),
        ("token-a2", "ws-two", "u-a"),
    ]
    .into_iter()
    .map(|(token, workspace, user)| {
        (
            token.to_owned(),
            Asker {
                workspace_id: workspace.to_owned(),
                user_id: user.to_owned(),
                token_scopes: None,
            },
        )
    })
    .collect();
    fixture(Arc::new(Tokens(askers)))
}

fn kms() -> Arc<dyn Kms> {
    Arc::new(
        LocalFileKms::from_key(Dek::generate().expect("a key"), "test-kek", "test")
            .expect("a well-formed key"),
    )
}

/// The harness router the surface is mounted into, plus a `PageStore` over
/// the very same database and blob — the ports are read out of the harness's
/// own set, so a page a test seeds is a page the surface can read.
struct Fixture {
    router: cratefield_core::axum::Router,
    store: PageStore,
}

/// A world with the askers given, and nothing else.
fn fixture_with(askers: &[(&str, &str, &str)]) -> Fixture {
    let askers = askers
        .iter()
        .map(|(token, workspace, user)| {
            (
                (*token).to_owned(),
                Asker {
                    workspace_id: (*workspace).to_owned(),
                    user_id: (*user).to_owned(),
                    token_scopes: None,
                },
            )
        })
        .collect();
    fixture(Arc::new(Tokens(askers)))
}

fn fixture(auth: Arc<dyn BearerAuth>) -> Fixture {
    // The kit's default port set has no blob store; the page store needs one,
    // so it goes in and a handle is kept here.
    let mut blob: Option<Arc<dyn Blob>> = None;
    let auth_for_routes = Arc::clone(&auth);
    // One custodian for the surface and for the store a test seeds with: a
    // second key would seal what the first cannot open.
    let key = kms();
    let key_for_routes = Arc::clone(&key);
    let pages = Pages::new().nest("/mcp", move |ctx| {
        livingbrain_mcp::router(
            ctx,
            Arc::clone(&key_for_routes),
            Arc::clone(&auth_for_routes),
        )
    });
    let kit = TestHarness::with_ports(vec![Box::new(pages)], |ports| {
        let store: Arc<dyn Blob> = Arc::new(MemoryBlob::new());
        ports.blob = Some(store.clone());
        blob = Some(store);
    });
    let clock: Arc<dyn Clock> = Arc::new(kit.clock.clone());
    Fixture {
        router: kit.router.clone(),
        store: PageStore::new(
            kit.db.clone(),
            // The production scope: `pages`, the name the module is mounted
            // under, so a seeded body is where the surface looks for it.
            Arc::new(ScopedBlob::new(
                blob.expect("the port set has the blob store it was given"),
                "pages",
            )),
            Arc::clone(&key),
            clock,
            Arc::new(UlidIdGen),
        ),
    }
}

fn post(fixture: &Fixture, token: &str, message: Value) -> TestResponse {
    let body = message.to_string();
    pollster::block_on(request_as(
        &fixture.router,
        Method::POST,
        ENDPOINT,
        token,
        Some(&body),
    ))
}

fn post_raw(fixture: &Fixture, token: Option<&str>, body: &str) -> TestResponse {
    pollster::block_on(async {
        match token {
            Some(token) => {
                request_as(&fixture.router, Method::POST, ENDPOINT, token, Some(body)).await
            }
            None => request(&fixture.router, Method::POST, ENDPOINT, Some(body)).await,
        }
    })
}

/// A `tools/call`, as the JSON a client sends.
fn call(name: &str, arguments: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": name, "arguments": arguments } })
}

/// The text of a tool result, which is what the model reads.
fn text_of(response: &TestResponse) -> String {
    response.json()["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("a tool result carries text: {:?}", response.body()))
        .to_owned()
}

fn result_of(response: &TestResponse) -> Value {
    let body = response.json();
    assert_eq!(body["jsonrpc"], "2.0", "{body}");
    assert!(
        body["id"].is_number(),
        "the envelope carries the request's id: {body}"
    );
    assert!(
        body.get("error").is_none(),
        "the call was refused, not answered: {body}"
    );
    body["result"].clone()
}

/// A JSON-RPC method call, as a client sends one.
fn method(fixture: &Fixture, token: &str, name: &str) -> TestResponse {
    post(
        fixture,
        token,
        json!({ "jsonrpc": "2.0", "id": 1, "method": name }),
    )
}

fn header_of(response: &TestResponse, name: header::HeaderName) -> Option<&str> {
    response
        .headers
        .get(name)
        .and_then(|value| value.to_str().ok())
}

/// A `tools/list` POST addressed to another hostname, the way a proxy
/// forwards one: `Host` names the deployment, `x-forwarded-proto` (when
/// given) the scheme it terminated. The kit's `request` helpers send no
/// `Host`, and the challenge is the one response that has to vary with it.
fn challenged_at(fixture: &Fixture, host: &str, proto: Option<&str>) -> String {
    use cratefield_core::axum::body::Body;
    use cratefield_core::axum::http::Request;
    use tower::ServiceExt;
    let message = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }).to_string();
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(ENDPOINT)
        .header(header::HOST, host)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(proto) = proto {
        builder = builder.header("x-forwarded-proto", proto);
    }
    let response = pollster::block_on(
        fixture
            .router
            .clone()
            .oneshot(builder.body(Body::from(message)).expect("request builds")),
    )
    .expect("router answers");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    response
        .headers()
        .get(header::WWW_AUTHENTICATE)
        .and_then(|value| value.to_str().ok())
        .expect("a 401 carries the challenge")
        .to_owned()
}

fn code_of(response: &TestResponse) -> i64 {
    response.json()["error"]["code"]
        .as_i64()
        .unwrap_or_else(|| panic!("a JSON-RPC error: {:?}", response.body()))
}

/// Seed one page, as a human of that workspace wrote it.
fn seed(fixture: &Fixture, workspace: &str, scope: &Scope, slug: &str, body: &str) -> Page {
    let scope = page_scope(workspace, scope);
    let head = pollster::block_on(fixture.store.read(&scope, slug))
        .expect("a read")
        .map(|page| page.version);
    pollster::block_on(fixture.store.write(
        &scope,
        slug,
        PageWrite {
            entity_type: EntityType::Decision,
            markdown: body.to_owned(),
            author: Author::Human {
                id: "human".to_owned(),
            },
            base_version: head,
        },
    ))
    .expect("a write");
    pollster::block_on(fixture.store.read(&scope, slug))
        .expect("a read")
        .expect("the page is there")
}

/// The user's own scope in a workspace — the `Scope` half of a seed.
fn user(id: &str) -> Scope {
    Scope::User(UserId::new(id.to_owned()))
}

/// The four pages the acceptance test reads. All four hold both query words,
/// so a search that returned every one of them would be a real leak rather
/// than a narrow query.
const KESTREL: &str =
    "---\ntitle: Kestrel reconciliation\n---\nThe ledger reconciliation runs at 0300 in Reykjavik.";
const TAMAR: &str =
    "---\ntitle: Tamar reconciliation\n---\nThe ledger reconciliation runs at 0300 in Bergen.";
const ZEPHYRMOOR: &str = "---\ntitle: Platform reconciliation\n---\nThe ledger reconciliation is owned by the platform team.";
const ATLAS: &str = "---\ntitle: Atlas reconciliation\n---\nThe ledger reconciliation at Atlas is owned by another team.";

// ---------------------------------------------------------------------------
// The handshake and the methods around it

#[test]
fn the_transport_and_the_method_table_behave_as_the_specification_says() {
    let fixture = world();

    for (asked, expected) in [
        ("2025-06-18", "2025-06-18"),
        ("2025-03-26", "2025-03-26"),
        ("2024-11-05", "2025-06-18"),
    ] {
        let result = result_of(&post(
            &fixture,
            "token-a",
            json!({ "jsonrpc": "2.0", "id": 7, "method": "initialize",
                    "params": { "protocolVersion": asked, "capabilities": {},
                                "clientInfo": { "name": "test", "version": "0" } } }),
        ));
        assert_eq!(result["protocolVersion"], expected, "{asked}");
        assert_eq!(result["capabilities"]["tools"], json!({}), "{asked}");
        assert_eq!(result["serverInfo"]["name"], "livingbrain", "{asked}");
        assert!(
            !result["instructions"].as_str().unwrap().is_empty(),
            "{asked}"
        );
    }

    let tools = result_of(&method(&fixture, "token-a", "tools/list"))["tools"].clone();
    let listed = tools.as_array().expect("an array");
    let names: Vec<&str> = listed.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "brain_search",
            "brain_page",
            "brain_context_for",
            "brain_note"
        ]
    );
    for tool in listed {
        assert_eq!(tool["inputSchema"]["type"], "object", "{tool}");
        assert!(!tool["description"].as_str().unwrap().is_empty(), "{tool}");
    }

    // A notification is acknowledged and never answered; the server's own
    // channels are refused with the one method it serves.
    let notification = post_raw(
        &fixture,
        Some("token-a"),
        &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }).to_string(),
    );
    assert_eq!(notification.status, StatusCode::ACCEPTED);
    assert!(notification.body().is_empty(), "a notification has no body");
    for verb in [Method::GET, Method::DELETE] {
        let response = pollster::block_on(request(&fixture.router, verb.clone(), ENDPOINT, None));
        assert_eq!(response.status, StatusCode::METHOD_NOT_ALLOWED, "{verb}");
        assert_eq!(header_of(&response, header::ALLOW), Some("POST"), "{verb}");
    }
}

#[test]
fn what_the_server_will_not_serve_is_a_json_rpc_error() {
    let fixture = world();
    assert_eq!(
        code_of(&method(&fixture, "token-a", "resources/list")),
        -32601
    );
    assert_eq!(
        code_of(&post_raw(&fixture, Some("token-a"), "{not json")),
        -32700
    );
    for arguments in [
        json!({}),
        json!({ "query": 7 }),
        json!({ "query": "   " }),
        json!({ "slug": null }),
        json!({ "limit": "many", "query": "x" }),
    ] {
        let response = post(&fixture, "token-a", call("brain_search", arguments.clone()));
        assert_eq!(code_of(&response), -32602, "{arguments} was not refused");
    }
    let unknown = call("brain_delete_everything", json!({}));
    assert_eq!(code_of(&post(&fixture, "token-a", unknown)), -32602);
    let nameless = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call" });
    assert_eq!(code_of(&post(&fixture, "token-a", nameless)), -32602);
}

// ---------------------------------------------------------------------------
// The acceptance test: an agent sees only its user's pages

#[test]
fn an_agent_sees_only_its_users_pages() {
    let fixture = world();
    seed(&fixture, "ws-one", &user("u-a"), "kestrel", KESTREL);
    seed(&fixture, "ws-one", &user("u-b"), "tamar", TAMAR);
    seed(&fixture, "ws-one", &Scope::Shared, "zephyrmoor", ZEPHYRMOOR);
    // The same user id in a second workspace, with a page of their own: only
    // the workspace folded into the page scope can keep these two apart.
    seed(&fixture, "ws-two", &user("u-a"), "atlas", ATLAS);

    let search = post(
        &fixture,
        "token-a",
        call("brain_search", json!({ "query": "ledger reconciliation" })),
    );
    let found = text_of(&search);
    assert!(
        found.contains("Reykjavik"),
        "A's own page is missing: {found}"
    );
    assert!(
        found.contains("Platform"),
        "the shared page is missing: {found}"
    );
    assert!(!found.contains("Bergen"), "B's page leaked: {found}");
    assert!(
        !found.contains("Atlas"),
        "another workspace leaked: {found}"
    );

    // And every page it quotes comes back as a citation the model can resolve.
    let results = result_of(&search)["structuredContent"]["results"].clone();
    let cited = results
        .as_array()
        .expect("an array")
        .iter()
        .find(|hit| hit["title"] == "Kestrel reconciliation")
        .unwrap_or_else(|| panic!("the caller's own page is cited: {results}"));
    let reference = cited["ref"].as_str().expect("a ref");
    assert_eq!(
        reference,
        format!("{}/kestrel", page_scope("ws-one", &user("u-a")))
    );
    assert_eq!(
        cited["url"],
        format!("https://livingbrain.wiki/brain/{reference}")
    );
    assert!(found.contains(cited["url"].as_str().unwrap()), "{found}");

    let mine = post(
        &fixture,
        "token-a",
        call("brain_page", json!({ "slug": "kestrel" })),
    );
    assert!(text_of(&mine).contains("Reykjavik"), "A reads her own page");

    // B's page does not exist for A: not an error, a page that is not found.
    for slug in ["tamar", "nothing"] {
        let refused = post(
            &fixture,
            "token-a",
            call("brain_page", json!({ "slug": slug })),
        );
        assert_eq!(refused.status, StatusCode::OK, "a tool error is not a 500");
        assert_eq!(refused.json()["result"]["isError"], true, "{slug}");
        assert!(!text_of(&refused).contains("Bergen"), "{slug}");
    }

    // The brief, for both workspaces, from the same query.
    let brief = |token: &str| {
        let args = json!({ "repo": "platform", "task": "ledger reconciliation" });
        text_of(&post(&fixture, token, call("brain_context_for", args)))
    };
    let mine = brief("token-a");
    assert!(mine.contains("Reykjavik"), "{mine}");
    assert!(!mine.contains("Bergen"), "B's page leaked: {mine}");
    assert!(!mine.contains("Atlas"), "another workspace leaked: {mine}");
    let other = brief("token-a2");
    assert!(other.contains("Atlas"), "{other}");
    assert!(
        !other.contains("Reykjavik"),
        "the other workspace leaked: {other}"
    );
}

// ---------------------------------------------------------------------------
// brain_note

#[test]
fn brain_note_redacts_before_it_stores_and_lands_in_the_callers_own_scope() {
    let fixture = world();
    // The title is body text the model chose, so it is redacted too.
    let note = json!({
        "title": "Deploy keys for AKIAIOSFODNN7EXAMPLE",
        "text": "The staging password is correct-horse-battery and the AWS key is \
                 AKIAIOSFODNN7EXAMPLE.",
    });
    let result = result_of(&post(&fixture, "token-a", call("brain_note", note.clone())));
    assert!(
        result["structuredContent"]["redacted"].as_u64().unwrap() >= 1,
        "the planted key was a finding: {result}"
    );
    let reference = result["structuredContent"]["ref"].as_str().expect("a ref");
    let (scope, slug) = reference.split_once('/').expect("a scope/slug ref");
    assert_eq!(scope, page_scope("ws-one", &user("u-a")));

    let stored = pollster::block_on(fixture.store.read(scope, slug))
        .expect("a read")
        .expect("the note is stored");
    assert!(
        !stored.markdown.contains("AKIAIOSFODNN7EXAMPLE"),
        "the key survived: {}",
        stored.markdown
    );
    assert!(
        stored.markdown.contains("correct-horse-battery"),
        "the note lost more than the secret"
    );
    assert!(
        !stored.markdown.contains("AKIAIOSFODNN7EXAMPLE"),
        "the title kept the key: {}",
        stored.markdown
    );

    // The same note is a new version of the same page, not a near-duplicate.
    let again = result_of(&post(&fixture, "token-a", call("brain_note", note)));
    assert_eq!(again["structuredContent"]["ref"], reference);
    assert_eq!(again["structuredContent"]["version"], 2);

    // A finds it; B does not.
    let mine = text_of(&post(
        &fixture,
        "token-a",
        call("brain_search", json!({ "query": "staging password" })),
    ));
    assert!(
        mine.contains("Deploy keys"),
        "A cannot find their note: {mine}"
    );
    let theirs = text_of(&post(
        &fixture,
        "token-b",
        call("brain_search", json!({ "query": "staging password" })),
    ));
    assert!(!theirs.contains("Deploy keys"), "B read A's note: {theirs}");
}

// ---------------------------------------------------------------------------
// The credential

#[test]
fn no_credential_or_a_wrong_one_is_a_challenged_401() {
    let fixture = world();
    let message = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }).to_string();
    for token in [None, Some("not-a-token")] {
        let response = post_raw(&fixture, token, &message);
        assert_eq!(response.status, StatusCode::UNAUTHORIZED, "{token:?}");
        assert_eq!(
            header_of(&response, header::CONTENT_TYPE),
            Some("application/problem+json")
        );
        assert_eq!(
            header_of(&response, header::WWW_AUTHENTICATE),
            Some(CHALLENGE)
        );
    }
    // A credential that resolves to an asker with no workspace or no user
    // names nobody: 401, not a panic and not a page scope of empty strings.
    for empty in [
        fixture_with(&[("token-e", "ws-one", "")]),
        fixture_with(&[("token-e", "", "u-a")]),
    ] {
        let response = method(&empty, "token-e", "tools/list");
        assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    }
    // The handshake does not need one: a client has to be able to learn the
    // server exists before it holds a credential.
    let handshake = post(
        &fixture,
        "not-a-token",
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" }),
    );
    assert_eq!(handshake.status, StatusCode::OK);
}

/// The challenge names the metadata on the host the request itself reached,
/// not a compiled-in one: one Worker fronts staging, production and `mcp.`,
/// and a client that followed a challenge across to another host would end
/// up authenticating against a different deployment (issue #72).
#[test]
fn the_challenge_names_the_host_the_request_reached() {
    let fixture = world();

    assert_eq!(
        challenged_at(&fixture, "staging-api.livingbrain.wiki", None),
        "Bearer resource_metadata=\"https://staging-api.livingbrain.wiki/.well-known/oauth-protected-resource\"",
    );

    // `wrangler dev`: the http scheme only `x-forwarded-proto` reveals.
    assert_eq!(
        challenged_at(&fixture, "localhost:8787", Some("http")),
        "Bearer resource_metadata=\"http://localhost:8787/.well-known/oauth-protected-resource\"",
    );
}
