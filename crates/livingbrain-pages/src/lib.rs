//! `livingbrain-pages`: the page store — entities, Markdown, links, history.
//!
//! A page is an **entity** (a person, project, decision, customer, system or
//! glossary term) whose Markdown body lives in the blob store and whose
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
//! composition that has one mounts it with [`Pages::nest`], which builds it
//! from this module's own context — the harness scopes `Blob` per module
//! name, so only a nested surface can open a body this module writes.

#![forbid(unsafe_code)]

mod entity;
mod keys;
mod store;

pub use entity::{
    EntityType, Frontmatter, MAX_SLUG_LEN, extract_links, is_slug, parse_frontmatter,
};
pub use keys::{SEALED_CONTENT_TYPE, envelope_version};
pub use store::{
    Author, AuthorKind, Page, PageError, PageStore, PageWrite, ReencryptReport, SearchHit,
    VersionMeta,
};

use cratefield_core::axum::Router;
use cratefield_core::{
    Config, ConfigError, DataKind, Migrations, Module, ModuleContext, PersonalDataSet, Port,
    SqlMigration,
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

/// Routes a composition mounts inside this module.
///
/// The closure is handed the one `ModuleContext` the mount got, so the
/// surface it builds reads what this module reads.
type Nest = Box<dyn Fn(ModuleContext) -> Router + Send + Sync>;

/// The pages module.
#[derive(Default)]
pub struct Pages {
    nest: Option<(&'static str, Nest)>,
}

// A nested surface is a closure, which is not `Debug`; the module itself is
// still one, so a composition that prints its modules keeps working.
impl std::fmt::Debug for Pages {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pages")
            .field("nested", &self.nest.as_ref().map(|(path, _)| path))
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
    /// One surface per module, and the type says so: `ModuleContext` is not
    /// `Clone` and a mount gets exactly one.
    #[must_use]
    pub fn nest(
        self,
        path: &'static str,
        routes: impl Fn(ModuleContext) -> Router + Send + Sync + 'static,
    ) -> Self {
        Self {
            nest: Some((path, Box::new(routes))),
        }
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
        ];
        SETS
    }

    fn migrations(&self) -> Migrations {
        const MIGRATIONS: [SqlMigration; 2] = [MIGRATION_INIT, MIGRATION_SCOPE_KEYS];
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

    /// Whatever a composition nested, and nothing otherwise — see the module
    /// docs. A `PageStore` is built from [`ModuleContext::ports`].
    fn router(&self, ctx: ModuleContext) -> cratefield_core::axum::Router {
        let mut api = cratefield_core::axum::Router::new();
        if let Some((path, build)) = &self.nest {
            api = api.nest(path, build(ctx));
        }
        api
    }
}
