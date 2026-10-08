//! `livingbrain-audit`: the settings audit log — who changed which setting,
//! when, in which workspace.
//!
//! One of the web app's acceptance criteria (issue #30) is **"All settings
//! changes are audited"**. This module is the seam that makes that true: a
//! `settings_audit` table, and an [`AuditStore`] that appends one row per
//! change.
//!
//! Two properties are the point:
//!
//! - **A value never reaches the table raw.** A BYOK API key is a setting,
//!   so a settings write is a credential write in a settings costume. Every
//!   value goes through [`livingbrain_redact`] — the pass every other ingest
//!   path already uses — before it is stored, and a value under a key that
//!   names a credential is never handed to this crate at all
//!   ([`SettingChange::secret`]). A change is recorded whether or not the
//!   value survives it: "you set the key and we did not keep it" is still an
//!   audit entry.
//! - **A clear is a fact worth keeping.** Clearing a setting writes an
//!   explicit `cleared` row rather than a `NULL`, because "this key was
//!   removed on Tuesday by the brain" is exactly the sentence an audit log
//!   exists to be able to say.
//!
//! The log is append-only and scoped: [`AuditStore::list_for_scope`] always
//! carries a `workspace_id` predicate, so one workspace's history is never
//! visible from another's settings screen.
//!
//! There is no HTTP surface yet. The routes belong to the web app's auth
//! work, and an unauthenticated settings route would be a hole; the module
//! ships its table and its store, and `router()` stays empty until then.

#![forbid(unsafe_code)]

mod entity;
mod store;

pub use entity::{Actor, MAX_KEY_LEN, Section, actor_kind, is_setting_key, is_workspace_id};
pub use store::{AuditEntry, AuditError, AuditStore, SettingChange, redact_value};

use cratefield_core::{
    Config, ConfigError, DataKind, Disposition, Migrations, Module, ModuleContext, PersonalDataSet,
    Port, SqlMigration,
};

/// The settings audit table, in the portable SQL subset so Postgres runs the
/// same file.
const MIGRATION_INIT: SqlMigration = SqlMigration::new(
    "0001",
    "init",
    include_str!("../migrations/sqlite/0001_init.sql"),
);

/// The same schema for Postgres. Byte-identical apart from the header: the
/// schema has no dialect-specific SQL in it. It ships anyway so the parity
/// job applies a postgres migration rather than falling back to the sqlite
/// one, and so a later dialect-specific change has a file to land in
/// without renumbering.
const MIGRATION_INIT_POSTGRES: SqlMigration = SqlMigration::new(
    "0001",
    "init",
    include_str!("../migrations/postgres/0001_init.sql"),
);

/// The settings audit module.
#[derive(Debug, Default)]
pub struct Audit;

impl Audit {
    /// A new audit module. There is nothing to configure.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Module for Audit {
    fn name(&self) -> &'static str {
        "audit"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    /// No blob port: an audit row is text and nothing else. `Db` holds the
    /// log, `Clock` stamps it and `IdGen` gives each row a unique id. The
    /// ordering key is the row's own per-workspace `seq`, not the id:
    /// `UlidIdGen` fills a ULID's random component randomly, so two changes
    /// in the same millisecond do not sort by write order.
    fn requires(&self) -> &'static [Port] {
        &[Port::Db, Port::Clock, Port::IdGen]
    }

    fn tables(&self) -> &'static [&'static str] {
        &["settings_audit"]
    }

    /// A row names the person who changed the setting, so the table is
    /// declared with `actor` as its subject column and
    /// [`Disposition::Erase`]: when a member is erased, the entries they
    /// changed go with them.
    ///
    /// `value` is listed in `redacted` because it holds the post-redaction
    /// representation of whatever the setting was, and a value under a key
    /// that names a credential is `""` — there is nothing to redact at
    /// query time that the write path did not already do.
    fn personal_data(&self) -> &'static [PersonalDataSet] {
        const SETS: &[PersonalDataSet] = &[PersonalDataSet {
            table: "settings_audit",
            subject: "actor",
            kind: DataKind::Usage,
            disposition: Disposition::Erase,
            description: "For each workspace: every settings change ever made — the section \
                          and key, whether the value was set, withheld or cleared, the value \
                          after redaction, the member id or `brain` that made it, and when.",
            redacted: &["value"],
            subject_via: None,
        }];
        SETS
    }

    fn migrations(&self) -> Migrations {
        const MIGRATIONS: [SqlMigration; 1] = [MIGRATION_INIT];
        const _: () = cratefield_core::assert_migration_set(&MIGRATIONS);
        const MIGRATIONS_POSTGRES: [SqlMigration; 1] = [MIGRATION_INIT_POSTGRES];
        const _: () = cratefield_core::assert_migration_set(&MIGRATIONS_POSTGRES);
        Migrations {
            sqlite: &MIGRATIONS,
            postgres: &MIGRATIONS_POSTGRES,
        }
    }

    fn validate_config(&self, _cfg: &dyn Config) -> Result<(), ConfigError> {
        Ok(())
    }

    /// No routes yet — see the module docs. An [`AuditStore`] is built from
    /// [`ModuleContext::ports`] when the settings screen lands.
    fn router(&self, _ctx: ModuleContext) -> cratefield_core::axum::Router {
        cratefield_core::axum::Router::new()
    }
}

#[cfg(test)]
mod tests {
    /// Both migrations are in the portable subset (ADR 0004), so the same
    /// file shape runs on both dialects. The conformance suite applies each
    /// one, but it applies them and does not check them against the lint —
    /// and a banned construct in the *postgres* file would otherwise only
    /// surface as a runtime failure on Postgres.
    #[test]
    fn both_migrations_are_portable_sql() {
        const SQLITE: &str = include_str!("../migrations/sqlite/0001_init.sql");
        const POSTGRES: &str = include_str!("../migrations/postgres/0001_init.sql");
        for (dialect, sql) in [("sqlite", SQLITE), ("postgres", POSTGRES)] {
            let violations = cratefield_core::lint_portable_sql(sql);
            assert!(
                violations.is_empty(),
                "the {dialect} migration is not portable SQL: {violations:?}"
            );
        }
    }
}
