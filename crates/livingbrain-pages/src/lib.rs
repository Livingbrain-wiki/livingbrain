//! `livingbrain-pages`: the page store — entities, Markdown, links, history.
//!
//! A page is an **entity** (a person, project, decision, customer, system,
//! glossary term or radar item) whose Markdown body lives in the blob store and whose
//! metadata lives in D1: a slug, an entity type, a scope, `[[backlinks]]` and
//! a version history. Two properties are the point of the module:
//!
//! - **Every version is recoverable.** A write appends a version row and puts
//!   the body at a new, immutable blob key. `read_version` always hands back
//!   exactly what was written.
//! - **A human edit is never silently overwritten.** A human write marks the
//!   page `human_pending_version`; a brain write that has not seen that edit
//!   is refused. A human edit is first-class.
//!
//! A third property arrived with issue #43: **a body is sealed at rest.** Each
//! scope has a data key, held only wrapped by a key custodian; bodies are
//! XChaCha20-Poly1305 envelopes in R2, search runs off a blind index of keyed
//! HMACs rather than over the Markdown, and forgetting a scope destroys the
//! key so the bodies left in the blob store are unreadable. See
//! `docs/adr/0003-per-scope-encryption.md`.
//!
//! There is no HTTP surface of its own: authentication and workspaces are a
//! sibling issue, and an unauthenticated write route would be a hole. A
//! composition that has one mounts it with [`Pages::nest`] (routes under a
//! path inside the module) or [`Pages::surface`] (routes at the module's own
//! mount root), either of which builds it from this module's own context —
//! the harness scopes `Blob` per module name, so only a surface built here
//! can open a body this module writes.

#![forbid(unsafe_code)]

mod answers;
mod entity;
mod keys;
mod scope;
mod sources;
mod store;

pub use answers::{AnswerError, Answered, Answers, Asker, Citation, PageAnswers};
pub use entity::{
    EntityType, Frontmatter, MAX_SLUG_LEN, extract_links, is_slug, parse_frontmatter,
};
pub use keys::{SEALED_CONTENT_TYPE, envelope_version};
pub use scope::{page_scope, page_scopes_for};
pub use sources::{
    MAX_SOURCE_BODY_BYTES, Source, SourceError, SourceKind, SourceStore, SourceWrite,
    extract_wikilinks,
};
pub use store::{
    Author, AuthorKind, Page, PageError, PageStore, PageSummary, PageWrite, ReencryptReport,
    SearchHit, VersionMeta,
};

use std::sync::Arc;

use cratefield_core::axum::Router;
use cratefield_core::{
    Config, ConfigError, DataKind, Disposition, Migrations, Module, ModuleContext, PersonalDataSet,
    Port, SqlMigration,
};

/// The pages, links and history tables, in the portable SQL subset so
/// Postgres runs the same file.
const MIGRATION_INIT: SqlMigration = SqlMigration::new(
    "0001",
    "init",
    include_str!("../migrations/sqlite/0001_init.sql"),
);

/// The scope keys and the blind index (issue #43), same portable subset.
const MIGRATION_SCOPE_KEYS: SqlMigration = SqlMigration::new(
    "0002",
    "scope_keys",
    include_str!("../migrations/sqlite/0002_scope_keys.sql"),
);

/// The source ledger (issue #81), same portable subset.
const MIGRATION_SOURCES: SqlMigration = SqlMigration::new(
    "0003",
    "sources",
    include_str!("../migrations/sqlite/0003_sources.sql"),
);

/// Routes a composition mounts inside this module.
///
/// The closure is handed the one [`ModuleContext`] the mount got, shared, so
/// the surface it builds reads what this module reads. It is shared through an
/// `Arc` because `ModuleContext` is neither `Clone` nor copyable — [`Ports`]
/// behind it holds a resolved runtime bundle and a second one rebuilt by hand
/// would drift from the first the moment a port is added — while a module may
/// nest more than one surface and each needs the same one.
///
/// [`Ports`]: cratefield_core::Ports
type Nest = Box<dyn Fn(Arc<ModuleContext>) -> Router + Send + Sync>;

/// The pages module.
#[derive(Default)]
pub struct Pages {
    nest: Vec<(&'static str, Nest)>,
    surface: Option<Nest>,
}

// A surface is a closure, which is not `Debug`; the module itself is still
// one, so a composition that prints its modules keeps working.
impl std::fmt::Debug for Pages {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pages")
            .field(
                "nested",
                &self.nest.iter().map(|(path, _)| *path).collect::<Vec<_>>(),
            )
            .field("surface", &self.surface.is_some())
            .finish()
    }
}

impl Pages {
    /// A new pages module. There is nothing to configure.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Mount `routes` at `path` inside this module.
    ///
    /// A nested surface is built from *this* module's context, so it reads
    /// the same blob prefix the pages do — the harness scopes `Blob` per
    /// module name, and a sibling module could not open a body this one
    /// writes (issue #24).
    ///
    /// A module may nest more than one surface (issue #81 added `/sources`
    /// beside `/mcp`), so this pushes rather than replacing. Every mount is
    /// handed the same `Arc`, ports and all.
    #[must_use]
    pub fn nest(
        mut self,
        path: &'static str,
        routes: impl Fn(Arc<ModuleContext>) -> Router + Send + Sync + 'static,
    ) -> Self {
        self.nest.push((path, Box::new(routes)));
        self
    }

    /// Merge `routes` at the module's own mount root, beside [`Pages::nest`],
    /// which puts its routes under a path inside the module.
    ///
    /// The routes the closure builds are module-absolute: one registered at
    /// `/` is served wherever a composition mounted this module, so a
    /// `GET /:slug` there is `/v1/pages/:slug` at the harness. Like a nest,
    /// the surface is built from *this* module's context and reads the same
    /// blob prefix the pages do (issue #24).
    ///
    /// One surface of each kind per module, and the type says so:
    /// `ModuleContext` is not `Clone` and a mount gets exactly one.
    #[must_use]
    pub fn surface(
        self,
        routes: impl Fn(Arc<ModuleContext>) -> Router + Send + Sync + 'static,
    ) -> Self {
        Self {
            nest: self.nest,
            surface: Some(Box::new(routes)),
        }
    }
}

/// A second context with this module's exact wiring, for a composition that
/// both nests a surface and merges one at the mount root. A mount gets
/// exactly one `ModuleContext` and it is not `Clone`, so the twin is
/// rebuilt: [`Ports::view_for`](cratefield_core::Ports::view_for)
/// re-derives the ports view the way the harness built the first one —
/// except the blob, which is put back as scoped, because a second
/// `view_for` would wrap it in `pages/` a second time. Everything else is
/// an `Arc`, the cloned bus, or a bool, so the twin shares what the first
/// context shares.
fn twin_context(ctx: &ModuleContext, module: &dyn Module) -> ModuleContext {
    let mut ports = ctx.ports.view_for(module);
    ports.blob.clone_from(&ctx.ports.blob);
    ModuleContext {
        ports,
        config: Arc::clone(&ctx.config),
        events: ctx.events.clone(),
        templates: Arc::clone(&ctx.templates),
        venture: Arc::clone(&ctx.venture),
        unprotected_writes_accepted: ctx.unprotected_writes_accepted,
        personal_data: Arc::clone(&ctx.personal_data),
        ui_mounted: ctx.ui_mounted,
        scheduled: Arc::clone(&ctx.scheduled),
    }
}

impl Module for Pages {
    fn name(&self) -> &'static str {
        "pages"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    /// The key custodian is not here: `cratefield-core` has no `Kms` port,
    /// and `PageStore::new` is handed one by whoever composes the store. When
    /// the runtime grows a `Port::Kms` this moves beside it.
    fn requires(&self) -> &'static [Port] {
        &[Port::Db, Port::Blob, Port::Clock, Port::IdGen]
    }

    /// `Signer` is **optional**, and only because a nested surface may need
    /// one to verify a caller's credential (issue #24). Optional, so a
    /// composition with no signer is a working pages module rather than one
    /// `Harness::build` refuses; a nested surface that needs it finds it
    /// absent and says so.
    fn optional(&self) -> &'static [Port] {
        &[Port::Signer]
    }

    fn tables(&self) -> &'static [&'static str] {
        &[
            "pages",
            "page_versions",
            "page_links",
            "scope_keys",
            "page_terms",
            "sources",
        ]
    }

    /// A page may describe a person, so all three tables are declared — and
    /// declared **unreachable** rather than absent: the identifying values
    /// live in the Markdown body and the slug, not in a column an equality
    /// predicate can bind. Saying `none` would report a page holding
    /// somebody's name as not personal data (issue #274).
    fn personal_data(&self) -> &'static [PersonalDataSet] {
        /// The shared reason no erasure query reaches these rows.
        const WHY: &str = "the identifying content is in the Markdown body or the slug, not in a \
                           column an erasure query can bind to";
        const SETS: &[PersonalDataSet] = &[
            PersonalDataSet::unreachable(
                "pages",
                DataKind::Content,
                "A page's current slug, type and Markdown body.",
                WHY,
            ),
            PersonalDataSet::unreachable(
                "page_versions",
                DataKind::Content,
                "Every past version's body, and its editor's id or `brain`.",
                WHY,
            ),
            PersonalDataSet::unreachable(
                "page_links",
                DataKind::Identifier,
                "The `[[wiki-link]]` targets a page names, which can be people.",
                WHY,
            ),
            // The two tables issue #43 added. Both hold a body's words only
            // in a keyed form — the blind index stores an HMAC, and the key
            // table stores a key that is itself wrapped — so an equality
            // predicate cannot reach a word in either, and saying `none`
            // would report them as holding nobody's data.
            PersonalDataSet::unreachable(
                "scope_keys",
                DataKind::Identifier,
                "One wrapped data key per scope version, and the key custodian's name for it.",
                "the row holds a KMS-wrapped key and no subject column; a scope's keys are \
                 destroyed wholesale by PageStore::forget_scope, which is the erasure",
            ),
            PersonalDataSet::unreachable(
                "page_terms",
                DataKind::Content,
                "A blind index of the words in each page's current body, one keyed HMAC per \
                 distinct token.",
                "the row holds HMACs of tokens, not tokens, so no erasure query can match a \
                 word; forgetting a scope deletes its rows with its key",
            ),
            // The source ledger (issue #81). An imported file is somebody's
            // document and its path is often their own filing, so the row is
            // declared content with the one column an erasure query can bind
            // to named as the subject — `imported_by` holds the asker's user
            // id, the same value the workspaces module matches on. The path
            // and the link targets are plaintext columns, so erasure reaches
            // them by deleting the row; only the sealed body outlives it, and
            // that one is destroyed with the scope key by
            // `PageStore::forget_scope`.
            PersonalDataSet {
                table: "sources",
                subject: "imported_by",
                kind: DataKind::Content,
                disposition: Disposition::Erase,
                description: "For each file you imported: the path it arrived under, the \
                              `[[wiki-links]]` it names, and the sealed Markdown body. Erasing \
                              you deletes the row; the sealed body stays in the blob store, \
                              unreadable to everybody once the scope's key is destroyed.",
                redacted: &[],
                subject_via: None,
            },
        ];
        SETS
    }

    fn migrations(&self) -> Migrations {
        const MIGRATIONS: [SqlMigration; 3] =
            [MIGRATION_INIT, MIGRATION_SCOPE_KEYS, MIGRATION_SOURCES];
        // The array is the apply order; this refuses a gap, a duplicate or an
        // entry out of order at build time (issue #27).
        const _: () = cratefield_core::assert_migration_set(&MIGRATIONS);
        Migrations {
            sqlite: &MIGRATIONS,
            postgres: &[],
        }
    }

    fn validate_config(&self, _cfg: &dyn Config) -> Result<(), ConfigError> {
        Ok(())
    }

    /// The coarse pre-buffer ceiling, raised for the import route (issue #81).
    ///
    /// The harness-wide 64 KiB is right for a note written by an agent and
    /// wrong for a file that arrived from a vault: an ordinary Markdown file
    /// is over it, and a runtime with a fixed memory ceiling — a Workers
    /// isolate — would refuse the import at the door, before the route that
    /// actually knows the ledger's own limit had a chance to read the body.
    ///
    /// This is the *coarse* number only. The import route carries its own
    /// `DefaultBodyLimit` at exactly this value and answers anything past it
    /// with the `sources/too-large` problem, so a body between 64 KiB and here
    /// is admitted only to be refused or accepted by the route that means it —
    /// `/mcp`, which inherits the harness limit, is unchanged.
    fn max_body_bytes(&self, _cfg: &dyn Config) -> usize {
        MAX_SOURCE_BODY_BYTES
    }

    /// Whatever a composition surfaced or nested, and nothing otherwise — see
    /// the module docs. A `PageStore` is built from [`ModuleContext::ports`].
    fn router(&self, ctx: ModuleContext) -> cratefield_core::axum::Router {
        // One context per surface: with no merged surface, every nest shares
        // the mount's own `Arc` — `ModuleContext` is not `Clone`, and the
        // ports are **not** re-viewed through [`Ports::view_for`], which would
        // scope the blob a second time (`pages/pages/…`) and put every nested
        // surface somewhere the module's own blobs are not.
        //
        // With a merged surface, the surface keeps the mount's own context and
        // each nest gets a twin ([`twin_context`]), which re-derives the view
        // the way the harness built the first one and puts the scoped blob
        // back, so the same no-second-scoping rule holds there too.
        //
        // [`Ports::view_for`]: cratefield_core::Ports::view_for
        let ctx = Arc::new(ctx);
        let mut api = cratefield_core::axum::Router::new();
        if let Some(surface) = &self.surface {
            // The merged surface is applied first, on the mount's context;
            // each nest then gets its own twin.
            api = api.merge(surface(Arc::clone(&ctx)));
            for (path, build) in &self.nest {
                let twin = twin_context(&ctx, self);
                api = api.nest(path, build(Arc::new(twin)));
            }
        } else {
            for (path, build) in &self.nest {
                api = api.nest(path, build(Arc::clone(&ctx)));
            }
        }
        api
    }
}
