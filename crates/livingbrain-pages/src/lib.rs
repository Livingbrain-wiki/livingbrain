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
//! There is no HTTP surface yet: authentication and workspaces are a sibling
//! issue, and an unauthenticated write route would be a hole. The module ships
//! its tables and its [`PageStore`]; the routes arrive with the auth work.

#![forbid(unsafe_code)]

mod entity;
mod store;

pub use entity::{
    EntityType, Frontmatter, MAX_SLUG_LEN, extract_links, is_slug, parse_frontmatter,
};
pub use store::{Author, AuthorKind, Page, PageError, PageStore, PageWrite, VersionMeta};

use cratefield_core::{
    Config, ConfigError, DataKind, Migrations, Module, ModuleContext, PersonalDataSet, Port,
    SqlMigration,
};

/// The one migration: `pages`, `page_versions` and `page_links`, in the
/// portable SQL subset so Postgres runs the same file.
const MIGRATION_INIT: SqlMigration = SqlMigration::new(
    "0001",
    "init",
    include_str!("../migrations/sqlite/0001_init.sql"),
);

/// The pages module.
#[derive(Debug, Default)]
pub struct Pages;

impl Pages {
    /// A new pages module. There is nothing to configure.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Module for Pages {
    fn name(&self) -> &'static str {
        "pages"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    fn requires(&self) -> &'static [Port] {
        &[Port::Db, Port::Blob, Port::Clock, Port::IdGen]
    }

    fn tables(&self) -> &'static [&'static str] {
        &["pages", "page_versions", "page_links"]
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
        ];
        SETS
    }

    fn migrations(&self) -> Migrations {
        const MIGRATIONS: [SqlMigration; 1] = [MIGRATION_INIT];
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

    /// No routes yet — see the module docs. A `PageStore` is built from
    /// [`ModuleContext::ports`] when the auth surface lands.
    fn router(&self, _ctx: ModuleContext) -> cratefield_core::axum::Router {
        cratefield_core::axum::Router::new()
    }
}
