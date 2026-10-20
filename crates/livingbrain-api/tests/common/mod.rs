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
use livingbrain_api::{
    AppTools, Apps, Ask, CodingAgents, Export, Facts, Graph, ModelJobs, Notes, PagesRebind,
    PublicPages, ResumeParkedOnMain, Search, Sources, Wiki, pages_routes,
};
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

/// A second workspace, for the tests that prove nothing crosses one.
pub const OTHER_WORKSPACE: &str = "T1OTHERSPACE";

/// The bearer token that speaks for `user-a` — the *same* user id — in
/// [`OTHER_WORKSPACE`]: the case the workspace clause exists for.
pub const TOKEN_A_ELSEWHERE: &str = "user-a-elsewhere";

/// A token for `user-a` narrowed to the shared scope only.
pub const TOKEN_A_NARROW: &str = "user-a-narrow";

/// The deployment key-encryption secret the models module seals with.
pub const MODEL_KEYS_SECRET: &str = "a-test-encryption-secret-of-32-bytes";

/// An agent account `user-a` owns (ADR 0004).
pub const AGENT: &str = "agt_one";
/// The bearer key that speaks for [`AGENT`], uncut.
pub const TOKEN_AGENT: &str = "agent-key";
/// The same agent, its key cut down to reading `coding-agents/claude/` in
/// `user-a`'s space.
pub const TOKEN_AGENT_NARROW: &str = "agent-key-narrow";
/// user-a, by a token cut to reading their own space (ADR 0004 `read:`).
#[allow(dead_code)]
pub const TOKEN_A_READ: &str = "token-a-read";
/// `user-b` with a token cut down to the shared space.
pub const TOKEN_B_SHARED: &str = "user-b-shared";

/// The owner of [`WORKSPACE`], with their whole credential.
pub const TOKEN_OWNER: &str = "owner-1";
/// The same owner, with a token cut down to the shared space.
pub const TOKEN_OWNER_NARROW: &str = "owner-1-narrow";
/// An admin of [`WORKSPACE`] (not an owner).
pub const TOKEN_ADMIN: &str = "admin-1";
/// The owner of [`OTHER_WORKSPACE`].
pub const TOKEN_OWNER_ELSEWHERE: &str = "owner-elsewhere";

/// A bearer resolver over a fixed table: a known token names an asker, an
/// unknown one names nobody. That is the whole of what #72 replaces.
///
/// An agent account's real key (`lba_…`) is resolved against the service
/// account tables, the way the venture's resolver does it minus the
/// membership check, so a test can issue a credential through
/// `POST /v1/apps` and call with the key it got back.
#[derive(Default)]
struct Tokens(BTreeMap<String, Asker>);

#[async_trait::async_trait]
impl BearerAuth for Tokens {
    async fn authenticate(&self, ports: &Ports, token: &str) -> Result<Asker, AuthError> {
        if token.starts_with("lba_") {
            let db = ports.db.clone().ok_or(AuthError::Rejected)?;
            let clock = ports.clock.clone().ok_or(AuthError::Rejected)?;
            let principal =
                cratefield_module_orgs::service_accounts::verify_key(&*db, token, clock.now())
                    .await
                    .map_err(|_| AuthError::Rejected)?
                    .ok_or(AuthError::Rejected)?;
            return Ok(Asker {
                workspace_id: principal.account.org_id,
                user_id: principal.account.id,
                token_scopes: principal.scopes,
                agent_owner: Some(principal.account.owner_sub),
                role: None,
                credential: Some(principal.prefix),
            });
        }
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
    /// The database every module of this fixture reads.
    #[must_use]
    pub fn db(&self) -> Arc<dyn Database> {
        self.db.clone()
    }

    /// The key custodian every store of this fixture seals with.
    #[allow(dead_code)]
    #[must_use]
    pub fn kms(&self) -> Arc<dyn Kms> {
        self.key.clone()
    }

    /// The harness's own clock.
    #[allow(dead_code)]
    #[must_use]
    pub fn clock(&self) -> Arc<dyn Clock> {
        self.clock.clone()
    }

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

    /// The coding-agents compiler over this fixture's own ports, with
    /// `model` scripted by the test and the composition's real facts sink
    /// ([`StoreFacts`](livingbrain_api::StoreFacts)), so what a compile
    /// records is what `/v1/facts` reads.
    #[must_use]
    pub fn compiler(
        &self,
        model: Arc<dyn livingbrain_compiler::Model>,
    ) -> livingbrain_compiler::Compiler {
        let blob: Arc<dyn Blob> = Arc::new(ScopedBlob::new(self.blob.clone(), "pages"));
        livingbrain_compiler::Compiler::new(
            PageStore::new(
                self.db.clone(),
                blob.clone(),
                self.key.clone(),
                self.clock.clone(),
                Arc::new(UlidIdGen),
            ),
            SourceStore::new(
                self.db.clone(),
                blob,
                self.key.clone(),
                self.clock.clone(),
                Arc::new(UlidIdGen),
            ),
            livingbrain_compiler::Queue::new(self.db.clone()),
            model,
            Arc::new(livingbrain_api::StoreFacts::new(
                livingbrain_pages::FactStore::new(
                    self.db.clone(),
                    self.key.clone(),
                    self.clock.clone(),
                    Arc::new(UlidIdGen),
                ),
            )),
            Arc::new(livingbrain_compiler::NoGate),
            self.clock.clone(),
            livingbrain_compiler::Settings::default(),
        )
    }
}

/// The public origin the fixture deploys under (`WORKSPACES_REDIRECT_BASE`):
/// deliberately not production's, so a link built from a constant shows.
pub const APP_BASE: &str = "https://app.brain.test";

/// The web app's address for one page under [`APP_BASE`] — what every
/// surface must cite.
#[allow(dead_code)]
pub fn page_url(scope: &str, slug: &str) -> String {
    format!("{APP_BASE}/wiki?slug={slug}&scope={scope}")
}

/// The `slug` a page URL names (`…/wiki?slug=…&scope=…`).
#[allow(dead_code)]
pub fn slug_in(url: &str) -> Option<&str> {
    url.split_once("?slug=")?.1.split('&').next()
}

/// One workspace with two people, and the first person's id again in a
/// second workspace.
pub fn fixture() -> Fixture {
    fixture_with(None, None, None)
}

/// [`fixture`], with Jev built through `connector` — the workspace's own
/// switch and key decide whether it runs.
#[allow(dead_code)]
pub fn fixture_with_jev(connector: Arc<dyn livingbrain_models::jev::JevConnector>) -> Fixture {
    fixture_with(None, None, Some(connector))
}

/// [`fixture`], with `POST /v1/ask` answering through `model` — a test's
/// scripted stand-in for the workspace's connected model.
#[allow(dead_code)]
pub fn fixture_with_ask_model(model: Arc<dyn livingbrain_api::AskModel>) -> Fixture {
    fixture_with(Some(model), None, None)
}

/// [`fixture_with_ask_model`], also answering to a session cookie through
/// `session` — the web app's way in.
#[allow(dead_code)]
pub fn fixture_with_session(
    model: Arc<dyn livingbrain_api::AskModel>,
    session: Arc<dyn livingbrain_api::SessionAuth>,
) -> Fixture {
    fixture_with(Some(model), Some(session), None)
}

fn fixture_with(
    ask_model: Option<Arc<dyn livingbrain_api::AskModel>>,
    session: Option<Arc<dyn livingbrain_api::SessionAuth>>,
    jev: Option<Arc<dyn livingbrain_models::jev::JevConnector>>,
) -> Fixture {
    let mut askers = [
        (TOKEN_A, WORKSPACE, "user-a"),
        (TOKEN_B, WORKSPACE, "user-b"),
        (TOKEN_A_ELSEWHERE, OTHER_WORKSPACE, "user-a"),
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
                agent_owner: None,
                role: None,
                credential: None,
            },
        )
    })
    .chain(std::iter::once((
        TOKEN_A_NARROW.to_owned(),
        Asker {
            workspace_id: WORKSPACE.to_owned(),
            user_id: "user-a".to_owned(),
            token_scopes: Some(vec!["shared".to_owned()]),
            agent_owner: None,
            role: None,
            credential: None,
        },
    )))
    .collect::<BTreeMap<_, _>>();
    // ADR 0004: an agent account user-a owns, with a whole key and with one
    // cut down to reading one folder; and user-b with a shared-only token.
    for (token, workspace, user, role, scopes) in [
        (TOKEN_OWNER, WORKSPACE, "owner-1", "owner", None),
        (
            TOKEN_OWNER_NARROW,
            WORKSPACE,
            "owner-1",
            "owner",
            Some(vec!["shared".to_owned()]),
        ),
        (TOKEN_ADMIN, WORKSPACE, "admin-1", "admin", None),
        (
            TOKEN_OWNER_ELSEWHERE,
            OTHER_WORKSPACE,
            "owner-2",
            "owner",
            None,
        ),
    ] {
        askers.insert(
            token.to_owned(),
            Asker {
                workspace_id: workspace.to_owned(),
                user_id: user.to_owned(),
                token_scopes: scopes,
                agent_owner: None,
                role: Some(role.to_owned()),
                credential: None,
            },
        );
    }
    askers.insert(
        TOKEN_AGENT.to_owned(),
        Asker {
            workspace_id: WORKSPACE.to_owned(),
            user_id: AGENT.to_owned(),
            token_scopes: None,
            agent_owner: Some("user-a".to_owned()),
            role: None,
            credential: Some("lba_0011223344556677".to_owned()),
        },
    );
    askers.insert(
        TOKEN_AGENT_NARROW.to_owned(),
        Asker {
            token_scopes: Some(vec!["read:personal:user-a/coding-agents/claude".to_owned()]),
            ..askers[TOKEN_AGENT].clone()
        },
    );
    askers.insert(
        TOKEN_A_READ.to_owned(),
        Asker {
            token_scopes: Some(vec!["read:personal:user-a".to_owned()]),
            ..askers[TOKEN_A].clone()
        },
    );
    askers.insert(
        TOKEN_B_SHARED.to_owned(),
        Asker {
            token_scopes: Some(vec!["shared".to_owned()]),
            ..askers[TOKEN_B].clone()
        },
    );

    let auth: Arc<dyn BearerAuth> = Arc::new(Tokens(askers));
    let key = kms();
    // One body store for everyone: the composition roots the port layer on
    // the pages key space (the planting the venture makes), the harness
    // scopes each module's view itself, and the seed store scopes the same
    // handle to `pages` — so a page a test seeds is a page every module's
    // routes can open.
    let memory: Arc<dyn Blob> = Arc::new(MemoryBlob::new());
    let wiki = Wiki::new(Arc::clone(&key), Arc::clone(&auth)).with_jev(jev);
    let wiki = match session {
        Some(session) => wiki.with_session(session),
        None => wiki,
    };

    let wiki_for_routes = wiki.clone();
    let pages = Pages::new().surface(move |ctx| pages_routes(ctx, &wiki_for_routes));
    let modules: Vec<Box<dyn cratefield_core::Module>> = vec![
        Box::new(pages),
        Box::new(Notes::new(wiki.clone())),
        Box::new(Search::new(wiki.clone())),
        Box::new(match ask_model {
            Some(model) => Ask::new(wiki.clone()).with_model(model),
            None => Ask::new(wiki.clone()),
        }),
        Box::new(Export::new(wiki.clone())),
        Box::new(Facts::new(wiki.clone())),
        Box::new(Sources::new(wiki.clone())),
        Box::new(Graph::new(wiki.clone())),
        Box::new(CodingAgents::new(wiki.clone())),
        Box::new(Apps::new(wiki.clone())),
        Box::new(PublicPages::new(wiki.clone())),
        Box::new(AppTools::new(wiki.clone())),
        Box::new(ModelJobs::new(wiki)),
        // The two tables the compiler reads beside its own: which model
        // serves the workspace, and the usage ledger its budget gate reads.
        Box::new(
            livingbrain_models::Models::new().with_main_connected(Arc::new(ResumeParkedOnMain)),
        ),
        Box::new(livingbrain_usage::Usage::new()),
        // The grants tables (ADR 0004): every route reads its access plan
        // from them, and refuses when they cannot be read.
        Box::new(livingbrain_accounts::Access::new()),
    ];

    let kit = TestHarness::with_ports(modules, |ports| {
        ports.blob = Some(Arc::new(PagesRebind::new(Arc::clone(&memory))) as Arc<dyn Blob>);
        // The deployment's own origin, which every page link is built under,
        // and the key-encryption secret the model jobs are sealed with.
        ports.config = Arc::new(cratefield_core::MapConfig::from_pairs([
            (livingbrain_pages::APP_BASE_KEY, APP_BASE),
            ("MODEL_KEYS_SECRET", MODEL_KEYS_SECRET),
        ]));
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

/// A `DELETE` with a bearer token.
#[allow(dead_code)]
pub fn delete(fixture: &Fixture, token: &str, path: &str) -> TestResponse {
    pollster::block_on(request_as(
        &fixture.router,
        Method::DELETE,
        path,
        token,
        None,
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

/// Stores one grant (ADR 0004) in `workspace`.
#[allow(clippy::too_many_arguments)]
pub fn grant(
    fixture: &Fixture,
    workspace: &str,
    principal: &str,
    space: &str,
    path: &str,
    kind: &str,
    permission: cratefield_module_orgs::grants::Permission,
    deny: bool,
) {
    use cratefield_module_orgs::grants::{self, Effect, NewGrant, Selector, split_path};
    pollster::block_on(grants::put_grant(
        &*fixture.db,
        workspace,
        &format!("grt_{}", cratefield_core::IdGen::ulid(&UlidIdGen)),
        &NewGrant {
            principal: principal.to_owned(),
            selector: Selector::new(kind, space, split_path(path)).expect("a valid selector"),
            permission,
            effect: if deny { Effect::Deny } else { Effect::Allow },
            expires_at: None,
            created_by: "test".to_owned(),
        },
        time::OffsetDateTime::from_unix_timestamp(1_800_000_000).expect("in range"),
    ))
    .expect("the grant stores");
}

/// The audit log's actions for `workspace`, newest first.
pub fn audit_actions(fixture: &Fixture, workspace: &str) -> Vec<(String, String, Option<String>)> {
    pollster::block_on(cratefield_module_orgs::audit::list(
        &*fixture.db,
        workspace,
        None,
        200,
    ))
    .expect("the log reads")
    .into_iter()
    .map(|entry| (entry.actor_id, entry.action, entry.on_behalf_of))
    .collect()
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
