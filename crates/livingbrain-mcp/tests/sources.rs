//! The source import endpoint, driven through the harness router the Worker
//! mounts (issue #81).
//!
//! The tests that matter are the two properties the endpoint exists to hold:
//! **the body is the identity** (a vault imported twice is one ledger, not
//! two), and **the server redacts** (a client that skipped it stores nothing
//! secret anyway).

use std::collections::BTreeMap;
use std::sync::Arc;

use cratefield_core::axum::http::{Method, StatusCode, header};
use cratefield_core::{Blob, Clock, Database, Ports, ScopedBlob, Statement, UlidIdGen};
use cratefield_kms::{Dek, Kms, LocalFileKms};
use cratefield_testing::{MemoryBlob, TestHarness, TestResponse, request, request_as};
use livingbrain_access::{Scope, UserId};
use livingbrain_mcp::{Asker, AuthError, BearerAuth, page_scope};
use livingbrain_pages::{Pages, SourceStore};
use serde_json::{Value, json};

/// The endpoint, nested in the pages module beside the MCP server: its `Blob`
/// is scoped to `pages`, which is the whole reason the surface lives there.
const ENDPOINT: &str = "/v1/pages/sources";

/// What a refused call says about where to authenticate.
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

/// Two people in one workspace and the *same* person id in a second one, which
/// is the case the page scope has to fold the workspace into.
fn world() -> Fixture {
    fixture(Arc::new(Tokens(
        [
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
        .collect(),
    )))
}

fn kms() -> Arc<dyn Kms> {
    Arc::new(
        LocalFileKms::from_key(Dek::generate().expect("a key"), "test-kek", "test")
            .expect("a well-formed key"),
    )
}

/// The harness router the surface is mounted into, plus a `SourceStore` over
/// the very same database and blob — the ports are read out of the harness's
/// own set, so a body the surface wrote is a body this can open.
struct Fixture {
    router: cratefield_core::axum::Router,
    db: Arc<dyn Database>,
    store: SourceStore,
}

fn fixture(auth: Arc<dyn BearerAuth>) -> Fixture {
    // The kit's default port set has no blob store; the source store needs
    // one, so it goes in and a handle is kept here.
    let mut blob: Option<Arc<dyn Blob>> = None;
    let auth_for_mcp = Arc::clone(&auth);
    let auth_for_sources = Arc::clone(&auth);
    // One custodian for both surfaces and for the store a test reads with: a
    // second key would seal what the first cannot open.
    let key = kms();
    let key_for_mcp = Arc::clone(&key);
    let key_for_sources = Arc::clone(&key);
    let pages = Pages::new()
        .nest("/mcp", move |ctx| {
            livingbrain_mcp::router(ctx, Arc::clone(&key_for_mcp), Arc::clone(&auth_for_mcp))
        })
        .nest("/sources", move |ctx| {
            livingbrain_mcp::sources_router(
                ctx,
                Arc::clone(&key_for_sources),
                Arc::clone(&auth_for_sources),
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
        db: kit.db.clone(),
        store: SourceStore::new(
            kit.db.clone(),
            // The production scope: `pages`, the name the module is mounted
            // under, so a sealed body is where the surface put it.
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

fn post(fixture: &Fixture, token: &str, body: Value) -> TestResponse {
    let body = body.to_string();
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

/// The response body as text, so a failed assertion says what the endpoint
/// answered rather than a byte count.
fn body_of(response: &TestResponse) -> String {
    String::from_utf8_lossy(response.body()).into_owned()
}

/// The problem type, which the runtime serves under its own base URI: the
/// tests below match the slug the crate defines, not the deployment's prefix.
fn problem_of(response: &TestResponse) -> String {
    response.json()["type"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

fn header_of(response: &TestResponse, name: header::HeaderName) -> Option<&str> {
    response
        .headers
        .get(name)
        .and_then(|value| value.to_str().ok())
}

/// How many rows the ledger holds, as the database itself says.
fn row_count(fixture: &Fixture) -> i64 {
    let rows = pollster::block_on(
        fixture
            .db
            .query(&Statement::new("SELECT COUNT(*) AS n FROM sources")),
    )
    .expect("the ledger counts");
    rows.rows
        .first()
        .and_then(|row| row.get::<i64>("n"))
        .unwrap_or_default()
}

/// Every row the ledger holds, as the database itself reads it: what an
/// operator with a query tool would see, not what the store remembers.
fn rows(fixture: &Fixture, sql: &str) -> Vec<cratefield_core::Row> {
    pollster::block_on(fixture.db.query(&Statement::new(sql)))
        .expect("a read")
        .rows
}

// ---------------------------------------------------------------------------
// The body is the identity

/// A three-file vault, imported twice. The first pass creates three rows and
/// three bodies; the second creates nothing and reads the same three back, so
/// a CLI that re-imports a changed vault does not accumulate a copy of every
/// note it has ever seen.
#[test]
fn a_vault_imported_twice_is_one_ledger() {
    let fixture = world();
    let vault = [
        (
            "notes/kestrel.md",
            "# Kestrel\n\nThe ledger runs at 0300.\n",
        ),
        ("notes/tamar.md", "# Tamar\n\nThe ledger runs at 0400.\n"),
        ("index.md", "# Index\n\nSee [[kestrel]] and [[tamar]].\n"),
    ];

    for (path, body) in vault {
        let response = post(&fixture, "token-a", json!({ "path": path, "body": body }));
        assert_eq!(
            response.status,
            StatusCode::CREATED,
            "{path}: {}",
            body_of(&response)
        );
        let answer = response.json();
        assert_eq!(answer["created"], true, "{path}");
        assert_eq!(answer["kind"], "import", "{path}");
        assert_eq!(answer["scope"], "personal", "{path}");
        assert_eq!(answer["path"], path, "{path}");
        assert!(
            !answer["id"].as_str().unwrap_or_default().is_empty(),
            "{path} has an id"
        );
        assert!(
            !answer["sha256"].as_str().unwrap_or_default().is_empty(),
            "{path} has a hash"
        );
    }
    assert_eq!(row_count(&fixture), 3, "one row per file");

    // The second pass is the same three files and nothing new.
    for (path, body) in vault {
        let response = post(&fixture, "token-a", json!({ "path": path, "body": body }));
        assert_eq!(
            response.status,
            StatusCode::OK,
            "{path}: {}",
            body_of(&response)
        );
        let answer = response.json();
        assert_eq!(answer["created"], false, "{path} was not created again");
    }
    assert_eq!(row_count(&fixture), 3, "the second pass added nothing");
}

/// The same body under two paths is one source: the body is the identity, and
/// a file that moved is still the file. The answer names the path the row
/// holds — the one it was first imported under — because that is where the
/// ledger says this body lives, and a client mirroring a vault needs to know
/// that rather than where it asked to put it.
#[test]
fn the_same_body_at_two_paths_is_one_source() {
    let fixture = world();
    let body = "# Kestrel\n\nOnly once.\n";
    let first = post(
        &fixture,
        "token-a",
        json!({ "path": "a/one.md", "body": body }),
    );
    let second = post(
        &fixture,
        "token-a",
        json!({ "path": "b/two.md", "body": body }),
    );
    assert_eq!(first.status, StatusCode::CREATED);
    assert_eq!(second.status, StatusCode::OK);
    assert_eq!(
        first.json()["id"],
        second.json()["id"],
        "the row kept its id: {}",
        body_of(&second)
    );
    assert_eq!(
        second.json()["path"],
        "a/one.md",
        "the answer names the stored path: {}",
        body_of(&second)
    );
    assert_eq!(row_count(&fixture), 1);
}

// ---------------------------------------------------------------------------
// Redaction

/// A body carrying a secret, sent **unredacted**, as a client that skipped
/// redaction would send it. The server redacts it anyway (issue #108): the
/// count comes back, the sealed body holds the replacement and not the secret,
/// and no column of the row does either.
#[test]
fn a_secret_is_redacted_before_it_is_sealed() {
    let fixture = world();
    // Assembled at run time so no credential-shaped literal is written into
    // this file; what fires is the `sk-ant-` prefix, not the shape of the
    // tail.
    let secret = format!(
        "sk{}ant-api03-{}-{}",
        "-", "AAAAAAAAAAAAAAAAAAAAAAAA", "BBBBBBBBBBBBBBBBBBBBBBBB"
    );
    let body = format!("# Deploy keys\n\nThe staging key is {secret}.\n");

    let response = post(
        &fixture,
        "token-a",
        json!({ "path": "ops/deploy.md", "body": body }),
    );
    assert_eq!(
        response.status,
        StatusCode::CREATED,
        "{}",
        body_of(&response)
    );
    let answer = response.json();
    assert!(
        answer["redacted"].as_u64().unwrap_or_default() >= 1,
        "the planted key was a finding: {answer}"
    );

    let sha = answer["sha256"].as_str().expect("a hash").to_owned();
    let scope = page_scope("ws-one", &user("u-a"));

    // The stored body is the redacted one: the marker is in it, the secret is
    // not, and the rest of the note survived.
    let opened = pollster::block_on(fixture.store.open(&scope, &sha)).expect("the body opens");
    assert!(
        opened.contains("[REDACTED:"),
        "the replacement is not there: {opened}"
    );
    assert!(!opened.contains(&secret), "the secret survived: {opened}");
    assert!(
        opened.contains("The staging key is"),
        "the note lost more than the secret: {opened}"
    );

    // And nothing in the row names it either — a `SELECT *` over the ledger
    // must not turn a secret up in a column a reader did not have to unseal.
    let stored = rows(
        &fixture,
        "SELECT scope, body_sha256, id, kind, rel_path, wikilinks, body_key, imported_by, \
                created_at FROM sources",
    );
    assert_eq!(stored.len(), 1, "one row");
    for row in &stored {
        for name in [
            "scope",
            "body_sha256",
            "id",
            "kind",
            "rel_path",
            "wikilinks",
            "body_key",
            "imported_by",
            "created_at",
        ] {
            let value = row.get::<String>(name).unwrap_or_default();
            assert!(
                !value.contains(&secret),
                "the secret is in `{name}`: {value}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Wikilinks

/// The links come back as the vault wrote them: the alias and the heading are
/// presentation, the target is what the file names, and it is kept verbatim
/// rather than normalised into a slug — a vault is not a wiki this brain
/// wrote, and `[[My Note]]` is a perfectly good note name.
#[test]
fn wikilinks_come_back_as_the_file_named_them() {
    let fixture = world();
    let body = "See [[My Note|an alias]], [[folder/Other#a heading]] and ![[an embed]]. \
                Again [[My Note]], and an empty [[]].\n";
    let response = post(
        &fixture,
        "token-a",
        json!({ "path": "index.md", "body": body }),
    );
    assert_eq!(
        response.status,
        StatusCode::CREATED,
        "{}",
        body_of(&response)
    );
    assert_eq!(
        response.json()["wikilinks"],
        json!(["My Note", "folder/Other", "an embed"]),
        "{}",
        body_of(&response)
    );
}

// ---------------------------------------------------------------------------
// Scopes

/// `personal` and `shared` fold into different scopes, and neither is the
/// other's. The answer names the scope the caller asked for; what it became
/// is the workspace-folded one, which is what keeps two workspaces apart.
#[test]
fn personal_and_shared_fold_into_different_scopes() {
    let fixture = world();
    let body = "# Shared\n\nOne body, two scopes.\n";

    let mine = post(
        &fixture,
        "token-a",
        json!({ "path": "note.md", "body": body }),
    );
    let shared = post(
        &fixture,
        "token-a",
        json!({ "path": "note.md", "body": body, "scope": "shared" }),
    );
    assert_eq!(mine.status, StatusCode::CREATED);
    assert_eq!(shared.status, StatusCode::CREATED);
    assert_eq!(mine.json()["scope"], "personal");
    assert_eq!(shared.json()["scope"], "shared");
    assert_eq!(
        mine.json()["sha256"],
        shared.json()["sha256"],
        "the same body hashes the same in both"
    );
    assert_eq!(
        row_count(&fixture),
        2,
        "the body is the identity *within* a scope, and these are two"
    );

    // B's own scope is neither A's nor the shared one, and the same user id
    // in another workspace is a third.
    let theirs = post(
        &fixture,
        "token-b",
        json!({ "path": "note.md", "body": body }),
    );
    let elsewhere = post(
        &fixture,
        "token-a2",
        json!({ "path": "note.md", "body": body }),
    );
    assert_eq!(theirs.status, StatusCode::CREATED);
    assert_eq!(elsewhere.status, StatusCode::CREATED);
    assert_eq!(row_count(&fixture), 4);

    // The paths agree; the scopes under them do not.
    let found: Vec<String> = rows(
        &fixture,
        "SELECT scope FROM sources WHERE rel_path = 'note.md'",
    )
    .iter()
    .filter_map(|row| row.get::<String>("scope"))
    .collect();
    assert_eq!(found.len(), 4, "four scopes, four rows: {found:?}");
    assert!(
        found.contains(&page_scope("ws-one", &user("u-a"))),
        "{found:?}"
    );
    assert!(
        found.contains(&page_scope("ws-one", &Scope::Shared)),
        "{found:?}"
    );
    assert!(
        found.contains(&page_scope("ws-two", &user("u-a"))),
        "{found:?}"
    );
}

// ---------------------------------------------------------------------------
// What is refused

#[test]
fn no_credential_is_a_challenged_401() {
    let fixture = world();
    let body = json!({ "path": "one.md", "body": "# One\n" }).to_string();
    for token in [None, Some("not-a-token")] {
        let response = post_raw(&fixture, token, &body);
        assert_eq!(response.status, StatusCode::UNAUTHORIZED, "{token:?}");
        assert_eq!(
            header_of(&response, header::CONTENT_TYPE),
            Some("application/problem+json")
        );
        assert_eq!(
            header_of(&response, header::WWW_AUTHENTICATE),
            Some(CHALLENGE),
            "{token:?}"
        );
    }
    assert_eq!(row_count(&fixture), 0, "nothing was stored");
}

#[test]
fn a_body_the_endpoint_cannot_read_is_a_400() {
    let fixture = world();
    // Not JSON at all.
    let not_json = post_raw(&fixture, Some("token-a"), "{not json");
    assert_eq!(not_json.status, StatusCode::BAD_REQUEST);
    assert!(problem_of(&not_json).ends_with("sources/bad-body"));

    // JSON, but missing what an import cannot do without.
    for body in [
        json!({ "body": "# One\n" }),
        json!({ "path": "one.md" }),
        json!({ "path": 7, "body": "# One\n" }),
        json!({ "path": "one.md", "body": null }),
        json!({ "path": "one.md", "body": "# One\n", "scope": 3 }),
    ] {
        let response = post(&fixture, "token-a", body.clone());
        assert_eq!(
            response.status,
            StatusCode::BAD_REQUEST,
            "{body} was not refused: {}",
            body_of(&response)
        );
        assert!(
            problem_of(&response).ends_with("sources/bad-body"),
            "{body}: {}",
            body_of(&response)
        );
    }
}

#[test]
fn an_unknown_scope_or_kind_is_a_400_naming_itself() {
    let fixture = world();
    for (body, slug) in [
        (
            json!({ "path": "one.md", "body": "# One\n", "scope": "public" }),
            "sources/unknown-scope",
        ),
        (
            json!({ "path": "one.md", "body": "# One\n", "kind": "scraped" }),
            "sources/unknown-kind",
        ),
    ] {
        let response = post(&fixture, "token-a", body.clone());
        assert_eq!(
            response.status,
            StatusCode::BAD_REQUEST,
            "{body}: {}",
            body_of(&response)
        );
        assert!(problem_of(&response).ends_with(slug), "{body}");
    }
    assert_eq!(row_count(&fixture), 0, "nothing was stored");
}

/// An agent session files in under its own kind: `agent_log`, one normalised
/// session record per body. The CLI's `logs sync` sends exactly this — kind,
/// a `<agent> session <id>` filing name instead of a vault path, and the
/// record itself — and re-syncing the same session reads back as unchanged,
/// because the body is the identity whatever kind it is.
#[test]
fn an_agent_log_is_a_kind_of_its_own_and_dedupes_on_its_body() {
    let fixture = world();
    let record = concat!(
        r#"{"schema_version":1,"agent":"codex","collector_version":1,"#,
        r#""session_id":"a1b2c3d4","repo":"~/work/demo","branch":null,"models":[],"#,
        r#""started_at":null,"ended_at":null,"turns":[],"totals":{"input_tokens":0,"#,
        r#""output_tokens":0,"cache_read_tokens":0,"cache_creation_tokens":0,"#,
        r#""reasoning_tokens":0,"cost_usd":null},"outcome":null,"truncated":false}"#,
    );
    let filing = "codex session a1b2c3d4";

    let first = post(
        &fixture,
        "token-a",
        json!({ "kind": "agent_log", "path": filing, "body": record }),
    );
    assert_eq!(first.status, StatusCode::CREATED, "{}", body_of(&first));
    let answer = first.json();
    assert_eq!(answer["kind"], "agent_log");
    assert_eq!(answer["scope"], "personal");
    assert_eq!(
        answer["path"], filing,
        "the filing name is what the row holds"
    );
    assert_eq!(answer["created"], true);

    // The same session, re-synced: the body is the identity, so the ledger
    // reads the row back instead of sealing a copy.
    let second = post(
        &fixture,
        "token-a",
        json!({ "kind": "agent_log", "path": filing, "body": record }),
    );
    assert_eq!(second.status, StatusCode::OK, "{}", body_of(&second));
    assert_eq!(second.json()["created"], false);
    assert_eq!(row_count(&fixture), 1);
}

#[test]
fn a_path_outside_the_vault_is_a_400() {
    let fixture = world();
    for path in [
        "/etc/passwd",
        "../outside.md",
        "notes/../../outside.md",
        "",
        "notes\\one.md",
    ] {
        let response = post(
            &fixture,
            "token-a",
            json!({ "path": path, "body": "# One\n" }),
        );
        assert_eq!(
            response.status,
            StatusCode::BAD_REQUEST,
            "`{path}` was not refused: {}",
            body_of(&response)
        );
        assert!(
            problem_of(&response).ends_with("sources/invalid-path"),
            "`{path}`: {}",
            body_of(&response)
        );
    }
    assert_eq!(row_count(&fixture), 0, "nothing was stored");
}

/// The user's own scope in a workspace — the `Scope` half of an expectation.
fn user(id: &str) -> Scope {
    Scope::User(UserId::new(id.to_owned()))
}

// ---------------------------------------------------------------------------
// Bodies

/// A 200 KiB file imports. The harness caps every module body at 64 KiB, and a
/// vault note is routinely past that; without a route-local ceiling on this
/// endpoint the promise of `livingbrain_redact::MAX_INPUT_BYTES` would be a
/// promise the transport breaks first.
#[test]
fn a_large_vault_file_is_taken() {
    let fixture = world();
    let body = format!("# Ledger\n\n{}", "a note line\n\n".repeat(20_000));
    assert!(
        body.len() > 200 * 1024,
        "the fixture is over 200 KiB, not {}",
        body.len()
    );
    let response = post(
        &fixture,
        "token-a",
        json!({ "path": "notes/long.md", "body": body }),
    );
    assert_eq!(
        response.status,
        StatusCode::CREATED,
        "{}",
        body_of(&response)
    );
    let sha = response.json()["sha256"]
        .as_str()
        .expect("a hash")
        .to_owned();
    let scope = page_scope("ws-one", &user("u-a"));
    let opened = pollster::block_on(fixture.store.open(&scope, &sha)).expect("the body opens");
    assert!(opened.len() > 200 * 1024, "the whole note was stored");
}

/// A body whose *Markdown* is over redaction's 1 MiB cap is refused as a 413
/// `sources/too-large` — the same problem an oversized transport body gets, so
/// a client has one answer and one instruction for "split it and send again".
#[test]
fn a_body_over_the_redaction_cap_is_a_413() {
    let fixture = world();
    let body = "x".repeat(livingbrain_redact::MAX_INPUT_BYTES + 1);
    let response = post(
        &fixture,
        "token-a",
        json!({ "path": "notes/huge.md", "body": body }),
    );
    assert_eq!(
        response.status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "{}",
        body_of(&response)
    );
    assert!(
        problem_of(&response).ends_with("sources/too-large"),
        "{}",
        body_of(&response)
    );
    assert_eq!(
        header_of(&response, header::CONTENT_TYPE),
        Some("application/problem+json")
    );
    assert_eq!(row_count(&fixture), 0, "nothing was stored");
}

/// A body past the route's own ceiling is still a problem+json 413 rather than
/// a bare transport error, because the caller can act on this one: split the
/// document. The harness ceiling itself is 64 KiB, so this also proves the
/// route's ceiling replaced it rather than sitting under it.
#[test]
fn a_body_past_the_route_ceiling_is_a_problem_not_a_crash() {
    let fixture = world();
    // Past twice redaction's cap, which is where this route stops reading.
    let body = "x".repeat(2 * livingbrain_redact::MAX_INPUT_BYTES + 128 * 1024);
    let response = post(
        &fixture,
        "token-a",
        json!({ "path": "notes/enormous.md", "body": body }),
    );
    assert_eq!(
        response.status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "{}",
        body_of(&response)
    );
    assert!(
        problem_of(&response).ends_with("sources/too-large"),
        "{}",
        body_of(&response)
    );
    assert_eq!(row_count(&fixture), 0, "nothing was stored");
}
