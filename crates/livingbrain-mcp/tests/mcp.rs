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
/// The challenge for a credential that was presented and refused (RFC 6750).
const CHALLENGE_INVALID: &str = "Bearer error=\"invalid_token\", resource_metadata=\"https://mcp.livingbrain.wiki/.well-known/oauth-protected-resource\"";

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
                agent_owner: None,
                role: None,
                credential: None,
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
    db: Arc<dyn cratefield_core::Database>,
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
                    agent_owner: None,
                    role: None,
                    credential: None,
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
    let kit = TestHarness::with_ports(
        vec![
            Box::new(pages),
            // The grants tables (ADR 0004): every tool reads its plan from them.
            Box::new(livingbrain_accounts::Access::new()),
        ],
        |ports| {
            let store: Arc<dyn Blob> = Arc::new(MemoryBlob::new());
            ports.blob = Some(store.clone());
            blob = Some(store);
            // The deployment's own origin, which every cited page is under.
            ports.config = Arc::new(cratefield_core::MapConfig::from_pairs([(
                livingbrain_pages::APP_BASE_KEY,
                "https://app.brain.test",
            )]));
        },
    );
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
        db: kit.db.clone(),
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
            "brain_note",
            "brain_facts",
            "search",
            "fetch"
        ]
    );
    for tool in listed {
        let name = tool["name"].as_str().unwrap();
        assert_eq!(tool["inputSchema"]["type"], "object", "{tool}");
        assert!(!tool["description"].as_str().unwrap().is_empty(), "{tool}");
        // Every tool but the one writer says it reads only; the writer
        // carries no annotations at all.
        let expected = (name != "brain_note").then_some(true);
        assert_eq!(
            tool["annotations"]["readOnlyHint"].as_bool(),
            expected,
            "{name}"
        );
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
    // An empty or absent query is a listing, not an error (see the test below).
    for arguments in [
        json!({ "query": 7 }),
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
    // The deployment's own web app, never the marketing site.
    assert_eq!(
        cited["url"],
        livingbrain_pages::reference_url("https://app.brain.test", reference)
    );
    assert!(
        cited["url"]
            .as_str()
            .unwrap()
            .starts_with("https://app.brain.test/wiki?slug=kestrel&scope="),
        "{cited}"
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

/// Temporal facts over MCP (docs/temporal-facts.md): `brain_note` states
/// them, `brain_facts` reads them now and as of a date, and
/// `brain_context_for` puts the line ahead of the pages. Nobody else sees
/// them.
#[test]
fn brain_note_facts_keep_their_history_and_brain_facts_reads_it() {
    let fixture = world();
    let shoe = |value: &str, from: &str, observed: &str| {
        json!({ "subject": "me", "attribute": "shoe_size", "value": value, "unit": "EU",
                "valid_from": from, "observed_at": observed })
    };
    let first = result_of(&post(
        &fixture,
        "token-a",
        call(
            "brain_note",
            json!({ "text": "my shoe size is 40",
                    "facts": [shoe("40", "2025", "2025-03-02T10:00:00Z")] }),
        ),
    ));
    assert_eq!(
        first["structuredContent"]["facts"][0]["outcome"], "created",
        "{first}"
    );
    result_of(&post(
        &fixture,
        "token-a",
        call(
            "brain_note",
            json!({ "text": "my shoe size is now 41",
                    "facts": [shoe("41", "2026", "2026-10-15T09:12:00Z")] }),
        ),
    ));

    let now = text_of(&post(
        &fixture,
        "token-a",
        call("brain_facts", json!({ "subject": "me" })),
    ));
    assert!(now.contains("Facts below are temporal."), "{now}");
    assert!(
        now.contains(
            "shoe_size(person/u-a) = 41 EU [valid 2026..now, observed 2026-10-15, source F1] \
             (was 40 EU 2025..2026)"
        ),
        "{now}"
    );
    let then = text_of(&post(
        &fixture,
        "token-a",
        call("brain_facts", json!({ "subject": "me", "as_of": "2025" })),
    ));
    assert!(
        then.contains("shoe_size(person/u-a) = 40 EU [valid 2025..2026"),
        "{then}"
    );

    let brief = text_of(&post(
        &fixture,
        "token-a",
        call(
            "brain_context_for",
            json!({ "repo": "shop", "task": "buy shoes in my shoe size" }),
        ),
    ));
    assert!(brief.contains("shoe_size(person/u-a) = 41 EU"), "{brief}");

    for other in ["token-b", "token-a2"] {
        let theirs = text_of(&post(&fixture, other, call("brain_facts", json!({}))));
        assert!(!theirs.contains("shoe_size"), "{other}: {theirs}");
    }

    // An emitter may not resolve conflicts itself.
    let refused = post(
        &fixture,
        "token-a",
        call(
            "brain_note",
            json!({ "text": "x", "facts": [{ "subject": "me", "attribute": "a",
                                            "value": "b", "status": "current" }] }),
        ),
    );
    assert_eq!(
        refused.json()["result"]["isError"],
        true,
        "{:?}",
        refused.body()
    );
}

// ---------------------------------------------------------------------------
// The credential

#[test]
fn no_credential_or_a_wrong_one_is_a_challenged_401() {
    let fixture = world();
    let message = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }).to_string();
    // No credential is the plain challenge; a presented one that is refused
    // says so in the challenge.
    for (token, challenge) in [(None, CHALLENGE), (Some("not-a-token"), CHALLENGE_INVALID)] {
        let response = post_raw(&fixture, token, &message);
        assert_eq!(response.status, StatusCode::UNAUTHORIZED, "{token:?}");
        assert_eq!(
            header_of(&response, header::CONTENT_TYPE),
            Some("application/problem+json")
        );
        assert_eq!(
            header_of(&response, header::WWW_AUTHENTICATE),
            Some(challenge)
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
    // Every message is authenticated, the handshake included: an open
    // `initialize` would look connected to a client that then never signs in.
    let handshake = post_raw(
        &fixture,
        None,
        &json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" }).to_string(),
    );
    assert_eq!(handshake.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        header_of(&handshake, header::WWW_AUTHENTICATE),
        Some(CHALLENGE)
    );
}

/// A presented token that is refused is a 401 whose challenge says why, so a
/// client that holds a revoked token starts the sign-in again.
#[test]
fn a_refused_token_is_an_invalid_token_challenge() {
    let fixture = world();
    let message = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }).to_string();
    let response = post_raw(&fixture, Some("revoked-token"), &message);
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        header_of(&response, header::WWW_AUTHENTICATE),
        Some(CHALLENGE_INVALID)
    );
}

/// The OAuth gate covers every method, and a good token works as before.
#[test]
fn every_method_is_challenged_without_a_token_and_served_with_one() {
    let fixture = world();
    let messages = [
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
        call("brain_search", json!({ "query": "anything" })),
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    ];
    for message in &messages {
        let response = post_raw(&fixture, None, &message.to_string());
        assert_eq!(response.status, StatusCode::UNAUTHORIZED, "{message}");
        assert_eq!(
            header_of(&response, header::WWW_AUTHENTICATE),
            Some(CHALLENGE),
            "{message}"
        );
        let refused = post_raw(&fixture, Some("revoked-token"), &message.to_string());
        assert_eq!(refused.status, StatusCode::UNAUTHORIZED, "{message}");
    }
    for message in &messages {
        let response = post_raw(&fixture, Some("token-a"), &message.to_string());
        assert_ne!(response.status, StatusCode::UNAUTHORIZED, "{message}");
    }
}

/// `brain_search` with no query lists the pages the asker can read, and never
/// a page in someone else's scope.
#[test]
fn an_empty_search_lists_the_pages_the_asker_can_read() {
    let fixture = world();
    seed(
        &fixture,
        "ws-one",
        &user("u-a"),
        "first-note",
        "---\ntitle: First note\n---\nAn early note.",
    );
    seed(
        &fixture,
        "ws-one",
        &user("u-a"),
        "second-note",
        "---\ntitle: Second note\n---\nA later note.",
    );
    seed(
        &fixture,
        "ws-one",
        &user("u-b"),
        "theirs",
        "---\ntitle: Theirs\n---\nSomebody else's.",
    );

    for arguments in [json!({}), json!({ "query": "" }), json!({ "query": "   " })] {
        let result = result_of(&post(
            &fixture,
            "token-a",
            call("brain_search", arguments.clone()),
        ));
        let titles: Vec<&str> = result["structuredContent"]["results"]
            .as_array()
            .unwrap_or_else(|| panic!("{arguments}: a results array: {result}"))
            .iter()
            .map(|hit| hit["title"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(titles.len(), 2, "{arguments}: {titles:?}");
        assert!(
            titles.contains(&"First note") && titles.contains(&"Second note"),
            "{titles:?}"
        );
        assert!(
            !titles.contains(&"Theirs"),
            "{arguments} listed another member's page"
        );
    }
}

// ---------------------------------------------------------------------------
// The ChatGPT-shaped pair: search and fetch (deep research connectors)

/// `search` answers in the shape deep research's contract names — exactly
/// `id`/`title`/`url` rows, the one text item the JSON of that same object —
/// and through the same access plan as `brain_search`.
#[test]
fn search_answers_in_the_shape_chatgpt_deep_research_reads() {
    let fixture = world();
    seed(&fixture, "ws-one", &user("u-a"), "kestrel", KESTREL);
    seed(&fixture, "ws-one", &user("u-b"), "tamar", TAMAR);
    seed(&fixture, "ws-one", &Scope::Shared, "zephyrmoor", ZEPHYRMOOR);
    seed(&fixture, "ws-two", &user("u-a"), "atlas", ATLAS);

    let response = post(
        &fixture,
        "token-a",
        call("search", json!({ "query": "ledger reconciliation" })),
    );
    let result = result_of(&response);
    let rows = result["structuredContent"]["results"]
        .as_array()
        .unwrap_or_else(|| panic!("a results array: {result}"))
        .clone();
    let all = Value::Array(rows.clone()).to_string();
    for row in &rows {
        let keys: Vec<&str> = row
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["id", "title", "url"], "{row}");
    }
    let scope = page_scope("ws-one", &user("u-a"));
    let mine = rows
        .iter()
        .find(|row| row["title"] == "Kestrel reconciliation")
        .unwrap_or_else(|| panic!("the caller's own page is a result: {all}"));
    assert_eq!(mine["id"], format!("{scope}:kestrel"));
    assert_eq!(
        mine["url"],
        livingbrain_pages::page_url("https://app.brain.test", &scope, "kestrel")
    );
    assert!(
        mine["url"]
            .as_str()
            .unwrap()
            .starts_with("https://app.brain.test/wiki?slug=kestrel&scope="),
        "{mine}"
    );
    // One text item, and it is that same object as a JSON string.
    let content = result["content"].as_array().expect("one content item");
    assert_eq!(content.len(), 1, "{result}");
    assert_eq!(content[0]["type"], "text", "{result}");
    assert_eq!(text_of(&response), result["structuredContent"].to_string());
    // The same plan as brain_search: B's page and the other workspace's are
    // nowhere in it, not even as an id.
    assert!(!all.contains("Bergen") && !all.contains("Atlas"), "{all}");
}

/// `fetch` reads by the id a `search` result named, in the shape deep
/// research reads. A page outside the caller's access — or an id that is not
/// one — never says a page exists.
#[test]
fn fetch_reads_by_id_and_says_nothing_about_pages_it_may_not_read() {
    let fixture = world();
    seed(&fixture, "ws-one", &user("u-a"), "kestrel", KESTREL);
    seed(&fixture, "ws-one", &user("u-b"), "tamar", TAMAR);
    seed(&fixture, "ws-one", &Scope::Shared, "zephyrmoor", ZEPHYRMOOR);

    let scope = page_scope("ws-one", &Scope::Shared);
    let id = format!("{scope}:zephyrmoor");
    let response = post(&fixture, "token-a", call("fetch", json!({ "id": id })));
    let result = result_of(&response);
    let structured = &result["structuredContent"];
    let keys: Vec<&str> = structured
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        ["id", "title", "text", "url", "metadata"],
        "{structured}"
    );
    let meta_keys: Vec<&str> = structured["metadata"]
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(meta_keys, ["scope", "slug", "version"], "{structured}");
    assert_eq!(structured["id"], id);
    assert_eq!(structured["title"], "Platform reconciliation");
    assert!(
        structured["text"]
            .as_str()
            .unwrap()
            .contains("owned by the platform team"),
        "{structured}"
    );
    assert_eq!(structured["metadata"]["scope"], scope);
    assert_eq!(structured["metadata"]["slug"], "zephyrmoor");
    assert_eq!(structured["metadata"]["version"], 1);
    assert_eq!(
        structured["url"],
        livingbrain_pages::page_url("https://app.brain.test", &scope, "zephyrmoor")
    );
    let content = result["content"].as_array().expect("one content item");
    assert_eq!(content.len(), 1, "{result}");
    assert_eq!(text_of(&response), structured.to_string());

    // B's page, by the very id that names it: the same not-found answer
    // brain_page gives, never "it exists" and never "it is somebody else's".
    let theirs = format!("{}:tamar", page_scope("ws-one", &user("u-b")));
    let refused = post(&fixture, "token-a", call("fetch", json!({ "id": theirs })));
    assert_eq!(refused.status, StatusCode::OK, "a tool error is not a 500");
    assert_eq!(refused.json()["result"]["isError"], true);
    let said = text_of(&refused);
    assert!(said.starts_with("no page"), "{said}");
    assert!(!said.contains("Bergen"), "{said}");

    // An id that is not `<scope>:<slug>` is the caller's syntax, not the
    // tool's answer.
    for id in [
        "zephyrmoor",
        "shared-abc:",
        ":zephyrmoor",
        "shared-abc:a:b",
        "shared-abc:Not A Slug",
        "",
    ] {
        let response = post(&fixture, "token-a", call("fetch", json!({ "id": id })));
        assert_eq!(code_of(&response), -32602, "{id}");
    }
    let nameless = post(&fixture, "token-a", call("fetch", json!({})));
    assert_eq!(code_of(&nameless), -32602);
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

// ---------------------------------------------------------------------------
// ADR 0004: an agent account through the MCP tools

/// One member and an agent account they own, in one workspace, plus pages in
/// two folders of the member's space.
fn agent_world() -> Fixture {
    let mut askers: BTreeMap<String, Asker> = BTreeMap::new();
    askers.insert(
        "member".to_owned(),
        Asker {
            workspace_id: "ws".to_owned(),
            user_id: "u-a".to_owned(),
            token_scopes: None,
            agent_owner: None,
            role: None,
            credential: None,
        },
    );
    askers.insert(
        "agent".to_owned(),
        Asker {
            workspace_id: "ws".to_owned(),
            user_id: "agt_x".to_owned(),
            token_scopes: None,
            agent_owner: Some("u-a".to_owned()),
            role: None,
            credential: Some("lba_00112233aabbccdd".to_owned()),
        },
    );
    let fixture = fixture(Arc::new(Tokens(askers)));
    seed(
        &fixture,
        "ws",
        &user("u-a"),
        "coding-agents--claude--x",
        "---\ntitle: Claude page\n---\n\nmarigold claude",
    );
    seed(
        &fixture,
        "ws",
        &user("u-a"),
        "coding-agents--codex--y",
        "---\ntitle: Codex page\n---\n\nmarigold codex",
    );
    seed(
        &fixture,
        "ws",
        &user("u-a"),
        "diary",
        "---\ntitle: Diary\n---\n\nmarigold diary",
    );
    seed(
        &fixture,
        "ws",
        &Scope::Shared,
        "handbook",
        "---\ntitle: Handbook\n---\n\nmarigold shared",
    );
    fixture
}

fn grant_agent(
    fixture: &Fixture,
    path: &str,
    permission: cratefield_module_orgs::grants::Permission,
) {
    use cratefield_module_orgs::grants::{self, Effect, NewGrant, Selector, split_path};
    pollster::block_on(grants::put_grant(
        &*fixture.db,
        "ws",
        &format!("grt_{path}"),
        &NewGrant {
            principal: "agent:agt_x".to_owned(),
            selector: Selector::new("pages", "personal:u-a", split_path(path)).expect("valid"),
            permission,
            effect: Effect::Allow,
            expires_at: None,
            created_by: "u-a".to_owned(),
        },
        time::OffsetDateTime::from_unix_timestamp(1_800_000_000).expect("in range"),
    ))
    .expect("stores");
}

#[test]
fn an_agent_account_sees_nothing_through_the_tools_until_granted() {
    let fixture = agent_world();
    let found = text_of(&post(
        &fixture,
        "agent",
        call("brain_search", json!({"query": "marigold"})),
    ));
    assert!(!found.contains("marigold"), "{found}");
    let page = post(
        &fixture,
        "agent",
        call("brain_page", json!({"slug": "diary"})),
    );
    assert!(text_of(&page).starts_with("no page"), "{}", text_of(&page));
    let context = text_of(&post(
        &fixture,
        "agent",
        call(
            "brain_context_for",
            json!({"repo": "chi-infra", "task": "marigold"}),
        ),
    ));
    assert!(!context.contains("marigold"), "{context}");
    let note = post(
        &fixture,
        "agent",
        call("brain_note", json!({"text": "hello"})),
    );
    assert_eq!(note.json()["result"]["isError"], true, "{}", note.json());
}

#[test]
fn an_agent_account_reads_exactly_its_folder_through_the_tools() {
    let fixture = agent_world();
    grant_agent(
        &fixture,
        "coding-agents/claude",
        cratefield_module_orgs::grants::Permission::Read,
    );
    let found = text_of(&post(
        &fixture,
        "agent",
        call("brain_search", json!({"query": "marigold"})),
    ));
    assert!(found.contains("Claude page"), "{found}");
    for leaked in ["Codex page", "Diary", "Handbook"] {
        assert!(!found.contains(leaked), "{leaked}: {found}");
    }
    let page = post(
        &fixture,
        "agent",
        call("brain_page", json!({"slug": "coding-agents--claude--x"})),
    );
    assert!(text_of(&page).contains("Claude page"));
    let refused = post(
        &fixture,
        "agent",
        call("brain_page", json!({"slug": "coding-agents--codex--y"})),
    );
    assert!(text_of(&refused).starts_with("no page"));
    // The ChatGPT-shaped pair answers through the same grants: the folder's
    // pages, and nothing beside them.
    let rows = text_of(&post(
        &fixture,
        "agent",
        call("search", json!({"query": "marigold"})),
    ));
    assert!(rows.contains("Claude page"), "{rows}");
    for leaked in ["Codex page", "Diary", "Handbook"] {
        assert!(!rows.contains(leaked), "{leaked}: {rows}");
    }
    let inside = format!(
        "{}:coding-agents--claude--x",
        page_scope("ws", &user("u-a"))
    );
    let fetched = text_of(&post(
        &fixture,
        "agent",
        call("fetch", json!({"id": inside})),
    ));
    assert!(fetched.contains("Claude page"), "{fetched}");
    let outside = format!("{}:coding-agents--codex--y", page_scope("ws", &user("u-a")));
    let refused = post(&fixture, "agent", call("fetch", json!({"id": outside})));
    assert!(text_of(&refused).starts_with("no page"));
    // Read is not write: a note is refused.
    let note = post(
        &fixture,
        "agent",
        call("brain_note", json!({"text": "hello"})),
    );
    assert_eq!(note.json()["result"]["isError"], true, "{}", note.json());
    // The member still reads all of it.
    let theirs = text_of(&post(
        &fixture,
        "member",
        call("brain_search", json!({"query": "marigold"})),
    ));
    assert!(
        theirs.contains("Diary") && theirs.contains("Codex page"),
        "{theirs}"
    );
}

#[test]
fn a_tool_read_lands_on_the_live_feed_of_its_own_workspace_only() {
    let fixture = world();
    seed(&fixture, "ws-one", &user("u-a"), "kestrel", KESTREL);
    seed(&fixture, "ws-two", &user("u-a"), "atlas", ATLAS);

    let read = post(
        &fixture,
        "token-a",
        call("brain_page", json!({ "slug": "kestrel" })),
    );
    assert!(
        text_of(&read).contains("Reykjavik"),
        "the read itself still answers"
    );
    let search = post(
        &fixture,
        "token-a",
        call("brain_search", json!({ "query": "reykjavik" })),
    );
    assert_eq!(search.status, StatusCode::OK);
    // The ChatGPT-shaped tools land on the same feed, their `scope:slug` ids
    // read back as scope and slug.
    let chat_read = post(
        &fixture,
        "token-a",
        call(
            "fetch",
            json!({ "id": format!("{}:kestrel", page_scope("ws-one", &user("u-a"))) }),
        ),
    );
    assert_eq!(chat_read.status, StatusCode::OK);
    let chat_search = post(
        &fixture,
        "token-a",
        call("search", json!({ "query": "reykjavik" })),
    );
    assert_eq!(chat_search.status, StatusCode::OK);

    let feed = |workspace: &str| {
        pollster::block_on(livingbrain_pages::activity_rows(
            fixture.db.as_ref(),
            workspace,
            None,
            50,
        ))
        .expect("the feed reads")
    };
    let mine: Vec<(String, String, String)> = feed("ws-one")
        .into_iter()
        .map(|row| (row.kind, row.slug, row.actor))
        .collect();
    assert!(
        mine.contains(&("read".to_owned(), "kestrel".to_owned(), "Claude".to_owned())),
        "{mine:?}"
    );
    assert!(
        mine.iter()
            .any(|(kind, slug, _)| kind == "search" && slug == "kestrel"),
        "a search that finds the page is on the feed: {mine:?}"
    );
    assert_eq!(
        mine.iter()
            .filter(|(kind, slug, _)| kind == "read" && slug == "kestrel")
            .count(),
        2,
        "a fetch is a read on the feed, its id read as scope and slug: {mine:?}"
    );
    assert_eq!(
        mine.iter()
            .filter(|(kind, slug, _)| kind == "search" && slug == "kestrel")
            .count(),
        2,
        "the ChatGPT-shaped search records like brain_search: {mine:?}"
    );
    assert!(
        feed("ws-two").is_empty(),
        "another workspace's feed is its own"
    );
}
