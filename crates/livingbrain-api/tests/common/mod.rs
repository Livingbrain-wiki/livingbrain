//! The shared fixture for the `livingbrain-api` integration tests.
//!
//! The composition is the one the venture will build: the `pages` module
//! with [`pages_routes`](livingbrain_api::pages_routes) merged at its mount
//! root (so the page routes are served at `/v1/pages`), and the sibling
//! modules beside it, each constructed with the same
//! [`Wiki`](livingbrain_api::Wiki) — the blob pre-scoped to `pages`, one
//! key custodian, one bearer resolver.
//!
//! The bearer resolver is a fixed table — `user-a` and `user-b`, two people
//! in one workspace, unknown tokens refused — which is the whole of what
//! issue #72's OAuth server replaces.

// Each test binary includes this module, and each uses a different slice
// of the helpers.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::sync::Arc;

use cratefield_core::axum::http::{Method, StatusCode, header};
use cratefield_core::{Blob, Clock, Database, Defer, Ports, ScopedBlob, UlidIdGen};
use cratefield_kms::{Dek, Kms, LocalFileKms};
use cratefield_testing::{MemoryBlob, TestHarness, TestResponse, request, request_as};
use livingbrain_access::{Scope, UserId};
use livingbrain_api::{Ask, Export, Notes, PagesRebind, Search, Sources, Wiki, pages_routes};
use livingbrain_mcp::{Asker, AuthError, BearerAuth, page_scope};
use livingbrain_pages::{
    Author, EntityType, Ingestor, Page, PageStore, PageWrite, Pages, SourceStore,
};
use serde_json::{Value, json};

/// The one workspace in the test world.
pub const WORKSPACE: &str = "T0SMOKETEST";

/// The bearer token that speaks for `user-a`.
pub const TOKEN_A: &str = "user-a";
/// The bearer token that speaks for `user-b`.
pub const TOKEN_B: &str = "user-b";

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

fn kms() -> Arc<dyn Kms> {
    Arc::new(
        LocalFileKms::from_key(Dek::generate().expect("a key"), "test-kek", "test")
            .expect("a well-formed key"),
    )
}

/// The harness router with the sibling modules mounted, plus a `PageStore`
/// and a `SourceStore` over the very same database and pages blob — the
/// ports are read out of the harness's own set, so a page or a source a test
/// seeds is one the routes read.
pub struct Fixture {
    pub router: cratefield_core::axum::Router,
    pub store: PageStore,
    pub sources: SourceStore,
    db: Arc<dyn Database>,
    blob: Arc<dyn Blob>,
    key: Arc<dyn Kms>,
    clock: Arc<dyn Clock>,
}

impl Fixture {
    /// An ingest pipeline over this fixture's own ports, so a test can put a
    /// source through the real pipeline and read it back through the route.
    /// `defer` is the caller's — a test asserting on deferred work passes
    /// the harness's [`FakeDefer`](cratefield_testing::FakeDefer).
    #[must_use]
    pub fn ingestor(&self, defer: Arc<dyn Defer>) -> Ingestor {
        Ingestor::new(
            SourceStore::new(
                self.db.clone(),
                // The same pages-rooted view the seeded store and every
                // module's routes read through: a raw handle here would land
                // bodies one prefix away from where the citations open them.
                Arc::new(ScopedBlob::new(self.blob.clone(), "pages")),
                self.key.clone(),
                self.clock.clone(),
                Arc::new(UlidIdGen),
            ),
            defer,
        )
    }
}

/// One workspace, two people, nothing else.
pub fn fixture() -> Fixture {
    let askers = [
        (TOKEN_A, WORKSPACE, "user-a"),
        (TOKEN_B, WORKSPACE, "user-b"),
    ]
    .into_iter()
    .map(|(token, workspace, user)| {
        (
            token.to_owned(),
            Asker {
                workspace_id: workspace.to_owned(),
                user_id: user.to_owned(),
                // The fixed table stands for a full member credential: `None`
                // carries every scope the member holds (issue #72).
                token_scopes: None,
            },
        )
    })
    .collect::<BTreeMap<_, _>>();

    let auth: Arc<dyn BearerAuth> = Arc::new(Tokens(askers));
    let key = kms();
    // One body store for everyone: the composition roots the port layer on
    // the pages key space (the planting the venture makes), the harness
    // scopes each module's view itself, and the seed store scopes the same
    // handle to `pages` — so a page a test seeds is a page every module's
    // routes can open.
    let memory: Arc<dyn Blob> = Arc::new(MemoryBlob::new());
    let wiki = Wiki::new(Arc::clone(&key), Arc::clone(&auth));

    let auth_for_routes = Arc::clone(&auth);
    let key_for_routes = Arc::clone(&key);
    let pages = Pages::new().surface(move |ctx| {
        pages_routes(
            ctx,
            Arc::clone(&key_for_routes),
            Arc::clone(&auth_for_routes),
        )
    });
    let modules: Vec<Box<dyn cratefield_core::Module>> = vec![
        Box::new(pages),
        Box::new(Notes::new(wiki.clone())),
        Box::new(Search::new(wiki.clone())),
        Box::new(Ask::new(wiki.clone())),
        Box::new(Export::new(wiki.clone())),
        Box::new(Sources::new(wiki)),
    ];

    let kit = TestHarness::with_ports(modules, |ports| {
        ports.blob = Some(Arc::new(PagesRebind::new(Arc::clone(&memory))) as Arc<dyn Blob>);
    });
    let clock: Arc<dyn Clock> = Arc::new(kit.clock.clone());
    Fixture {
        router: kit.router.clone(),
        store: PageStore::new(
            kit.db.clone(),
            Arc::new(ScopedBlob::new(Arc::clone(&memory), "pages")),
            Arc::clone(&key),
            clock.clone(),
            Arc::new(UlidIdGen),
        ),
        sources: SourceStore::new(
            kit.db.clone(),
            Arc::new(ScopedBlob::new(Arc::clone(&memory), "pages")),
            key.clone(),
            clock.clone(),
            Arc::new(UlidIdGen),
        ),
        db: kit.db.clone(),
        blob: memory,
        key,
        clock,
    }
}

/// A `GET` with a bearer token. The path is the full mounted path —
/// `/v1/search?q=…`, `/v1/export?format=…`.
pub fn get(fixture: &Fixture, token: &str, path: &str) -> TestResponse {
    pollster::block_on(request_as(&fixture.router, Method::GET, path, token, None))
}

/// A `POST` with a bearer token and a JSON body.
pub fn post(fixture: &Fixture, token: &str, path: &str, body: Value) -> TestResponse {
    pollster::block_on(request_as(
        &fixture.router,
        Method::POST,
        path,
        token,
        Some(&body.to_string()),
    ))
}

/// A `PUT` with a bearer token and a JSON body.
pub fn put(fixture: &Fixture, token: &str, path: &str, body: Value) -> TestResponse {
    pollster::block_on(request_as(
        &fixture.router,
        Method::PUT,
        path,
        token,
        Some(&body.to_string()),
    ))
}

/// A `GET` with no credential at all.
pub fn get_anon(fixture: &Fixture, path: &str) -> TestResponse {
    pollster::block_on(request(&fixture.router, Method::GET, path, None))
}

/// A `POST` with no credential at all.
pub fn post_anon(fixture: &Fixture, path: &str, body: &str) -> TestResponse {
    pollster::block_on(request(&fixture.router, Method::POST, path, Some(body)))
}

/// A `POST` with a credential that names nobody.
pub fn post_bad_token(fixture: &Fixture, path: &str, body: Value) -> TestResponse {
    pollster::block_on(request_as(
        &fixture.router,
        Method::POST,
        path,
        "not-a-token",
        Some(&body.to_string()),
    ))
}

/// Seed one page, as a human of that workspace wrote it.
pub fn seed(fixture: &Fixture, workspace: &str, scope: &Scope, slug: &str, body: &str) -> Page {
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
pub fn user(id: &str) -> Scope {
    Scope::User(UserId::new(id.to_owned()))
}

/// The page-store scope string the fixture's askers resolve to, for
/// assertions that name where a page landed.
pub fn scope_of(workspace: &str, id: &str) -> String {
    page_scope(workspace, &user(id))
}

/// The status of a problem response, as the body says it.
pub fn problem_status(response: &TestResponse) -> u16 {
    response.json()["status"].as_u64().unwrap_or(0) as u16
}

/// Whether a response answers `application/problem+json`.
pub fn is_problem(response: &TestResponse) -> bool {
    response
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|content_type| content_type.starts_with("application/problem+json"))
}

/// The response status, for the assertions that only care about the code.
pub fn status_of(response: &TestResponse) -> StatusCode {
    response.status
}

/// The body as text, for failure messages. A body that is not text still
/// lossily prints, which is all a test failure needs.
pub fn body_text(response: &TestResponse) -> String {
    String::from_utf8_lossy(response.body().as_ref()).into_owned()
}

/// A minimal zip reader, test-side only: walks the end-of-central-directory
/// record, then the central directory, then each local header, and verifies
/// every stored CRC along the way. What the export builds, a real unzip
/// reads — the tests pin it.
pub fn zip_entries(archive: &[u8]) -> Vec<(String, Vec<u8>)> {
    fn u16_at(bytes: &[u8], at: usize) -> u16 {
        u16::from_le_bytes(bytes[at..at + 2].try_into().expect("u16 field"))
    }
    fn u32_at(bytes: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(bytes[at..at + 4].try_into().expect("u32 field"))
    }
    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &byte in bytes {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                let low = crc & 1;
                crc >>= 1;
                if low == 1 {
                    crc ^= 0xEDB8_8320;
                }
            }
        }
        !crc
    }

    // No archive comment, so the record is exactly the last 22 bytes.
    let eocd = archive.len() - 22;
    assert_eq!(u32_at(archive, eocd), 0x0605_4b50, "EOCD is last");
    let entries = u16_at(archive, eocd + 10) as usize;
    let directory_size = u32_at(archive, eocd + 12) as usize;
    let directory_offset = u32_at(archive, eocd + 16) as usize;

    let mut at = directory_offset;
    let mut found = Vec::with_capacity(entries);
    for _ in 0..entries {
        assert_eq!(u32_at(archive, at), 0x0201_4b50, "directory signature");
        let crc = u32_at(archive, at + 16);
        let size = u32_at(archive, at + 24) as usize;
        let name_len = u16_at(archive, at + 28) as usize;
        let extra_len = u16_at(archive, at + 30) as usize;
        let comment_len = u16_at(archive, at + 32) as usize;
        let offset = u32_at(archive, at + 42) as usize;
        let name = &archive[at + 46..at + 46 + name_len];

        assert_eq!(u32_at(archive, offset), 0x0403_4b50, "local signature");
        let local_name_len = u16_at(archive, offset + 26) as usize;
        let local_extra_len = u16_at(archive, offset + 28) as usize;
        let data_start = offset + 30 + local_name_len + local_extra_len;
        let data = &archive[data_start..data_start + size];
        assert_eq!(crc32(data), crc, "stored CRC matches the data");

        found.push((
            String::from_utf8(name.to_vec()).expect("UTF-8 name"),
            data.to_vec(),
        ));
        at += 46 + name_len + extra_len + comment_len;
    }
    assert_eq!(
        at,
        directory_offset + directory_size,
        "the directory is exactly its entries"
    );
    found
}

/// A `POST /v1/notes` body, as the CLI sends it.
pub fn note_body(body: &str) -> Value {
    json!({ "body": body })
}
