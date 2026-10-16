//! `livingbrain-discord`: a Discord server connected to a workspace through
//! its bot, and the messages of its channels filed as sources.
//!
//! The shape mirrors `livingbrain-github-app` (the GitHub connection):
//!
//! - **Connect** (`routes.rs`): an admin starts a signed, single-use state
//!   and follows Discord's bot-install page; the callback spends the state,
//!   confirms with Discord that the bot is in the server, and records it.
//! - **Import** (`drain.rs`, `sync.rs`): the Worker's cron drain lists the
//!   server's channels and threads, walks each included channel backwards
//!   (`before=`) until history is exhausted, then polls forward (`after=`).
//!   Messages only mark their channel-day dirty; a dirty day is re-read in
//!   full and filed as one transcript through `livingbrain-pages`' ingest
//!   pipeline (screen, redact, dedupe, seal, queue for compilation).
//!
//! Live Discord calls go through the harness `HttpClient` port with the bot
//! token (`api.rs`). The bot token and the OAuth client credentials are
//! Worker secrets and are never stored in a table (`config.rs`).
//!
//! **Backfill runs only inside the Worker's cron.** No route, CLI command or
//! agent makes a `Drain`, and the only constructor is private to `drain.rs`.
//! `docs/discord.md` documents the setup, the permissions and the scope.

#![forbid(unsafe_code)]

pub mod api;
mod common;
mod config;
pub mod drain;
mod render;
mod routes;
mod state;
mod store;
mod sync;

pub use config::{DiscordApp, PERMISSIONS, is_available};
pub use drain::Drain;

use std::sync::Arc;

use cratefield_core::axum;
use cratefield_core::{
    AnyError, BoxFuture, Config, ConfigError, DataKind, Disposition, Migrations, Module,
    ModuleContext, PersonalDataSet, Port, SqlMigration,
};
use cratefield_kms::Kms;

/// What the routes and the drain share for the life of the isolate.
pub(crate) struct Shared {
    /// The key ring the page store seals sources under.
    pub kms: Option<Arc<dyn Kms>>,
}

/// The Discord module: its tables, the routes under `/v1/discord`, and the
/// cron drain that imports.
///
/// Without a key ring ([`DiscordModule::with_kms`]) the tables are migrated
/// but nothing can be filed, and the routes answer 503.
pub struct DiscordModule {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for DiscordModule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscordModule")
            .field("kms", &self.shared.kms.is_some())
            .finish_non_exhaustive()
    }
}

impl Default for DiscordModule {
    fn default() -> Self {
        Self::new()
    }
}

impl DiscordModule {
    #[must_use]
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Shared { kms: None }),
        }
    }

    /// The key ring the page store seals the imported transcripts under: the
    /// venture's `HARNESS_KEK_*`, the same custodian as GitHub's.
    #[must_use]
    pub fn with_kms(self, kms: Arc<dyn Kms>) -> Self {
        Self {
            shared: Arc::new(Shared { kms: Some(kms) }),
        }
    }
}

/// Every table this module owns.
const TABLES: &[&str] = &[
    "discord_states",
    "discord_connections",
    "discord_channels",
    "discord_days",
];

impl Module for DiscordModule {
    fn name(&self) -> &'static str {
        "discord"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    fn requires(&self) -> &'static [Port] {
        &[Port::Db, Port::Clock, Port::HttpClient, Port::Signer]
    }

    /// The transcripts are filed through the page store (blob), with ids and
    /// the defer port the pipeline queues compilation on.
    fn optional(&self) -> &'static [Port] {
        &[Port::Blob, Port::IdGen, Port::Defer]
    }

    fn tables(&self) -> &'static [&'static str] {
        TABLES
    }

    fn personal_data(&self) -> &'static [PersonalDataSet] {
        const SETS: &[PersonalDataSet] = &[
            PersonalDataSet {
                table: "discord_states",
                subject: "created_by",
                kind: DataKind::Identifier,
                disposition: Disposition::Erase,
                description: "A short-lived connect step you started: your member id, the \
                              workspace and when it expires. Only a hash of the token is kept.",
                redacted: &[],
                subject_via: None,
            },
            PersonalDataSet {
                table: "discord_connections",
                subject: "connected_by",
                kind: DataKind::Identifier,
                disposition: Disposition::Retain(
                    "the connection belongs to the workspace, not the member; deleting the \
                     row would silently stop the workspace's Discord import, and the member \
                     id is the record of who connected it",
                ),
                description: "For a Discord server you connected to a workspace: your member \
                              id, as the person who connected it, and the server's name.",
                redacted: &[],
                subject_via: None,
            },
            PersonalDataSet::none(
                "discord_channels",
                "a channel's id, name, kind and sync progress; no message content and no person",
            ),
            PersonalDataSet::none(
                "discord_days",
                "a channel-day's date and whether it was filed; no message content and no person",
            ),
        ];
        SETS
    }

    fn migrations(&self) -> Migrations {
        const MIGRATION_INIT: SqlMigration = SqlMigration::new(
            "0001",
            "init",
            include_str!("../migrations/sqlite/0001_init.sql"),
        );
        const MIGRATION_INIT_POSTGRES: SqlMigration = SqlMigration::new(
            "0001",
            "init",
            include_str!("../migrations/postgres/0001_init.sql"),
        );
        const MIGRATIONS: [SqlMigration; 1] = [MIGRATION_INIT];
        const _: () = cratefield_core::assert_migration_set(&MIGRATIONS);
        const MIGRATIONS_POSTGRES: [SqlMigration; 1] = [MIGRATION_INIT_POSTGRES];
        const _: () = cratefield_core::assert_migration_set(&MIGRATIONS_POSTGRES);
        Migrations {
            sqlite: &MIGRATIONS,
            postgres: &MIGRATIONS_POSTGRES,
        }
    }

    /// Nothing is required: without the Discord secrets the module reports
    /// itself unavailable (`GET /v1/discord` says so) rather than refusing to
    /// boot, exactly as GitHub does without its app.
    fn validate_config(&self, _cfg: &dyn Config) -> Result<(), ConfigError> {
        Ok(())
    }

    fn router(&self, ctx: ModuleContext) -> axum::Router {
        routes::router(ctx)
    }

    /// The in-Worker drain, the only place a [`Drain`] is minted.
    fn scheduled<'a>(
        &'a self,
        ctx: &'a ModuleContext,
        cron: &'a str,
    ) -> BoxFuture<'a, Result<(), AnyError>> {
        Box::pin(async move {
            drain::run(ctx, cron, &self.shared)
                .await
                .map_err(|error| AnyError::from(error.to_string()))
        })
    }
}

#[cfg(test)]
mod tests;
