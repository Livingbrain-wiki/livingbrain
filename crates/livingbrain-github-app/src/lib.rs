//! `livingbrain-github-app`: the GitHub connection's storage — the
//! workspace's GitHub App and its sealed credentials, the single-use states
//! of the manifest and install flows, the installation → workspace binding,
//! the repositories an installation reaches with their backfill progress,
//! webhook replay protection, and the work queue the cron drain consumes.
//!
//! Parts B and C of the GitHub connection. Part A (`livingbrain-github`)
//! is the pure crate — webhook signature verification, payload models, the
//! payload → page mapping, the backfill planner. Part B is the store API
//! exported here; Part C is the module's routes (`routes.rs`: manifest
//! flow, install callback, `POST /v1/github/webhook`, the settings reads
//! and writes) and the body of its cron drain (`sync.rs`).
//! `docs/github.md` documents the API, the routes and the threat model.
//!
//! # Why a module crate, not a pure store crate
//!
//! The tables need migrations, migrations belong to a [`Module`], and the
//! harness collects them per module name (`github/0001` →
//! `crates/livingbrain-venture/migrations`). The routes and the drain that
//! Part C adds read and write exactly these tables, so they belong to the
//! same module: one `github` module owns its tables end to end, as `usage`
//! does. Part A stays a dependency-free library this crate can depend on
//! once it lands.
//!
//! # The shared app, and one app per workspace
//!
//! The default is one operator-owned **shared** "Living Brain" app that
//! every workspace installs; its credentials are Worker secrets
//! ([`SharedApp`], `GITHUB_APP_*`), never rows. Because anyone can install
//! a public app, a shared installation binds only after a one-time OAuth
//! hop proves the person finishing the flow is the GitHub user who
//! installed it ([`bind_shared_installation`]). The per-workspace app below
//! stays as the advanced "use your own GitHub App" option; an installation
//! belongs to exactly one app ([`AppKind`]).
//!
//! The manifest flow is driven by a workspace owner or admin and creates an
//! app on *their* GitHub account. Its private key is that workspace's
//! credential, so it is stored per workspace (`github_app.workspace_id` is
//! unique), which keeps the blast radius of one leaked key to one tenant and
//! lets an installation be checked against the one app it may belong to.
//! Webhooks therefore arrive per app (`POST /v1/github/webhook`, the app
//! named by `X-GitHub-Hook-Installation-Target-ID`), are verified with that
//! app's secret ([`webhook_secret`]), and route only to installations held
//! under that app id ([`route_installation`]).
//!
//! # Backfill runs only inside the Worker
//!
//! Backfill progress and queue claims are written only by the scheduled
//! drain; see [`drain`] for how that is enforced.

#![forbid(unsafe_code)]

mod app;
mod common;
mod deliveries;
pub mod drain;
mod github;
mod installations;
mod intake;
mod queue;
mod repos;
mod routes;
mod seal;
mod shared;
mod sync;

pub use app::{
    AppCredentials, AppSummary, DEFAULT_STATE_TTL_SECS, MAX_STATE_TTL_SECS, NewApp, SaveAppOutcome,
    StateProof, StatePurpose, app_credentials, app_summary, consume_state, create_state,
    create_state_about, prune_states, remove_app, save_app, state_purpose, webhook_secret,
};
pub use common::StoreError;
pub use deliveries::{DELIVERY_RETENTION_SECS, prune_deliveries, record_delivery};
pub use drain::{BackfillProgress, Claimed, Drain};
pub use installations::{
    Account, AppKind, BindOutcome, Installation, InstallationStatus, Route, bind_installation,
    bind_shared_installation, list_installations, observe_installation, observe_installation_as,
    route_installation, route_installation_as, set_installation_status, set_installation_status_as,
};
pub use queue::{WorkItem, dead_count, enqueue, queued_count};
pub use repos::{
    BackfillPhase, Repo, RepoRef, RepoSettings, get_repo, list_repos, note_webhook, remove_repo,
    set_repo_settings, upsert_repos,
};
pub use seal::Secret;
pub use shared::{SHARED_APP_KEYS, SharedApp};

use std::sync::Arc;

use cratefield_core::axum;
use cratefield_core::{
    AnyError, BoxFuture, Config, ConfigError, DataKind, Disposition, Migrations, Module,
    ModuleContext, PersonalDataSet, Port, SqlMigration,
};
use cratefield_kms::Kms;

/// The largest webhook body accepted, in bytes. GitHub caps a payload at
/// 25 MB, but every event this connection subscribes to is far smaller (a
/// push lists at most 20 commits); a bigger body is refused unread rather
/// than buffered.
pub const MAX_WEBHOOK_BYTES: usize = 1024 * 1024;

/// What the routes and the drain share for the life of the isolate: the
/// key custodian the app credentials are sealed under, and the in-memory
/// installation-token cache.
pub(crate) struct Shared {
    pub kms: Option<Arc<dyn Kms>>,
    pub tokens: github::TokenCache,
}

/// The GitHub module: its tables, the routes under `/v1/github` (manifest
/// flow, install callback, webhook, settings) and the cron drain that
/// backfills and ingests.
///
/// Without a key custodian ([`GithubApp::with_kms`]) the tables are still
/// migrated and the housekeeping runs, but nothing can seal or open an app
/// credential: the flow routes answer 503 and the webhook 401.
pub struct GithubApp {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for GithubApp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GithubApp")
            .field("kms", &self.shared.kms.is_some())
            .finish_non_exhaustive()
    }
}

impl Default for GithubApp {
    fn default() -> Self {
        Self::new()
    }
}

impl GithubApp {
    #[must_use]
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Shared {
                kms: None,
                tokens: github::TokenCache::default(),
            }),
        }
    }

    /// The key ring the app credentials are sealed under — the venture's
    /// `HARNESS_KEK_*`, the same custodian the page store uses.
    #[must_use]
    pub fn with_kms(self, kms: Arc<dyn Kms>) -> Self {
        Self {
            shared: Arc::new(Shared {
                kms: Some(kms),
                tokens: github::TokenCache::default(),
            }),
        }
    }
}

/// Every table this module owns.
const TABLES: &[&str] = &[
    "github_app",
    "github_states",
    "github_installations",
    "github_repos",
    "github_deliveries",
    "github_queue",
];

impl Module for GithubApp {
    fn name(&self) -> &'static str {
        "github"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    /// The database holds everything; the clock stamps rows and drives the
    /// scheduled housekeeping; the HTTP client reaches GitHub; the signer
    /// verifies the session cookie.
    fn requires(&self) -> &'static [Port] {
        &[Port::Db, Port::Clock, Port::HttpClient, Port::Signer]
    }

    /// What the drain files into the ledger with: the page store's blob
    /// (rebound onto the pages key space by the composition), ids, and the
    /// defer port the pipeline enqueues extraction on. Without them the
    /// drain does its housekeeping and files nothing.
    fn optional(&self) -> &'static [Port] {
        &[Port::Blob, Port::IdGen, Port::Defer]
    }

    /// Webhook bodies can exceed the 64 KiB JSON default (a push lists up
    /// to 20 commits; an installation event lists its repositories).
    fn max_body_bytes(&self, _cfg: &dyn Config) -> usize {
        MAX_WEBHOOK_BYTES
    }

    fn tables(&self) -> &'static [&'static str] {
        TABLES
    }

    fn personal_data(&self) -> &'static [PersonalDataSet] {
        const SETS: &[PersonalDataSet] = &[
            PersonalDataSet {
                table: "github_app",
                subject: "created_by",
                kind: DataKind::Identifier,
                disposition: Disposition::Retain(
                    "the app and its credentials belong to the workspace, not the member; \
                     deleting the row would disconnect every other member's GitHub \
                     integration, and the member id is the record of who connected it",
                ),
                description: "For a GitHub App you connected to a workspace: your member id, \
                              as the person who created it. The app itself belongs to the \
                              workspace and stays with it.",
                redacted: &[],
                subject_via: None,
            },
            PersonalDataSet {
                table: "github_states",
                subject: "created_by",
                kind: DataKind::Identifier,
                disposition: Disposition::Erase,
                description: "Short-lived records of a GitHub connection you started: your \
                              member id, the workspace and when it expires. Never the \
                              token itself, only its hash.",
                redacted: &[],
                subject_via: None,
            },
            PersonalDataSet {
                table: "github_installations",
                subject: "bound_by",
                kind: DataKind::Identifier,
                disposition: Disposition::Retain(
                    "the installation belongs to the workspace, not the member; deleting \
                     the row would disconnect the workspace's repositories, and the member \
                     id is the record of who connected it",
                ),
                description: "For a GitHub installation you connected to a workspace: your \
                              member id, as the person who connected it. The row also names \
                              the GitHub account it is installed on and the numeric GitHub id \
                              of the person who installed it there (as GitHub reported it), \
                              which belong to the workspace's connection and stay with it.",
                redacted: &[],
                subject_via: None,
            },
            PersonalDataSet::none(
                "github_repos",
                "a repository's id, name, project and sync progress; the scope column \
                 may name a member id only as the target scope of that member's own pages",
            ),
            PersonalDataSet::none(
                "github_deliveries",
                "a webhook delivery id, its event name and when it arrived — nothing \
                 about a person",
            ),
            PersonalDataSet::none(
                "github_queue",
                "references to GitHub objects to re-read (a kind and a number or ref), \
                 never their content and never a person",
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
        const MIGRATION_SHARED: SqlMigration = SqlMigration::new(
            "0002",
            "shared_app",
            include_str!("../migrations/sqlite/0002_shared_app.sql"),
        );
        const MIGRATION_SHARED_POSTGRES: SqlMigration = SqlMigration::new(
            "0002",
            "shared_app",
            include_str!("../migrations/postgres/0002_shared_app.sql"),
        );
        const MIGRATIONS: [SqlMigration; 2] = [MIGRATION_INIT, MIGRATION_SHARED];
        const _: () = cratefield_core::assert_migration_set(&MIGRATIONS);
        const MIGRATIONS_POSTGRES: [SqlMigration; 2] =
            [MIGRATION_INIT_POSTGRES, MIGRATION_SHARED_POSTGRES];
        const _: () = cratefield_core::assert_migration_set(&MIGRATIONS_POSTGRES);
        Migrations {
            sqlite: &MIGRATIONS,
            postgres: &MIGRATIONS_POSTGRES,
        }
    }

    /// Nothing to configure yet: the key ring is the venture's
    /// (`HARNESS_KEK_*`), passed to the store per call.
    fn validate_config(&self, _cfg: &dyn Config) -> Result<(), ConfigError> {
        Ok(())
    }

    fn router(&self, ctx: ModuleContext) -> axum::Router {
        routes::router(ctx, Arc::clone(&self.shared))
    }

    /// The in-Worker drain, and the only place a [`Drain`] is minted.
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

#[cfg(test)]
mod flow_tests;
