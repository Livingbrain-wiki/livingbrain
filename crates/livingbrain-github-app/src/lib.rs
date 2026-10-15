//! `livingbrain-github-app`: the GitHub connection's storage — the
//! workspace's GitHub App and its sealed credentials, the single-use states
//! of the manifest and install flows, the installation → workspace binding,
//! the repositories an installation reaches with their backfill progress,
//! webhook replay protection, and the work queue the cron drain consumes.
//!
//! This is Part B of the GitHub connection. Part A (`livingbrain-github`)
//! is the pure crate — webhook signature verification, payload models, the
//! payload → page mapping, the backfill planner — and Part C adds this
//! module's routes (manifest flow, install callback, `POST
//! /v1/github/webhook/{app_id}`) and the body of its cron drain on top of
//! the store API exported here. `docs/github.md` ("Storage & secrets")
//! documents that API and its threat model.
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
//! # One app per workspace
//!
//! The manifest flow is driven by a workspace owner or admin and creates an
//! app on *their* GitHub account. Its private key is that workspace's
//! credential, so it is stored per workspace (`github_app.workspace_id` is
//! unique), which keeps the blast radius of one leaked key to one tenant and
//! lets an installation be checked against the one app it may belong to.
//! Webhooks therefore arrive per app (`/v1/github/webhook/{app_id}`), are
//! verified with that app's secret ([`webhook_secret`]), and route only to
//! installations held under that app id ([`route_installation`]).
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
mod installations;
mod queue;
mod repos;
mod seal;

pub use app::{
    AppCredentials, AppSummary, DEFAULT_STATE_TTL_SECS, MAX_STATE_TTL_SECS, NewApp, SaveAppOutcome,
    StateProof, StatePurpose, app_credentials, app_summary, consume_state, create_state,
    prune_states, remove_app, save_app, webhook_secret,
};
pub use common::StoreError;
pub use deliveries::{DELIVERY_RETENTION_SECS, prune_deliveries, record_delivery};
pub use drain::{BackfillProgress, Claimed, Drain};
pub use installations::{
    Account, BindOutcome, Installation, InstallationStatus, Route, bind_installation,
    list_installations, observe_installation, route_installation, set_installation_status,
};
pub use queue::{WorkItem, enqueue, queued_count};
pub use repos::{
    BackfillPhase, Repo, RepoRef, RepoSettings, get_repo, list_repos, note_webhook, remove_repo,
    set_repo_settings, upsert_repos,
};
pub use seal::Secret;

use cratefield_core::axum;
use cratefield_core::{
    AnyError, BoxFuture, Config, ConfigError, DataKind, Disposition, Migrations, Module,
    ModuleContext, PersonalDataSet, Port, SqlMigration,
};

/// The GitHub module. Today it owns the tables and the scheduled
/// housekeeping; Part C mounts its routes under `/v1/github`.
#[derive(Debug, Default)]
pub struct GithubApp;

impl GithubApp {
    #[must_use]
    pub fn new() -> Self {
        Self
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
    /// scheduled housekeeping.
    fn requires(&self) -> &'static [Port] {
        &[Port::Db, Port::Clock]
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
                              the GitHub account it is installed on, which belongs to the \
                              workspace's connection and stays with it.",
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
        const MIGRATIONS: [SqlMigration; 1] = [MIGRATION_INIT];
        const _: () = cratefield_core::assert_migration_set(&MIGRATIONS);
        const MIGRATIONS_POSTGRES: [SqlMigration; 1] = [MIGRATION_INIT_POSTGRES];
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

    /// No routes yet: Part C mounts them here.
    fn router(&self, _ctx: ModuleContext) -> axum::Router {
        axum::Router::new()
    }

    /// The in-Worker drain, and the only place a [`Drain`] is minted.
    fn scheduled<'a>(
        &'a self,
        ctx: &'a ModuleContext,
        cron: &'a str,
    ) -> BoxFuture<'a, Result<(), AnyError>> {
        Box::pin(async move {
            drain::run(ctx, cron)
                .await
                .map_err(|error| AnyError::from(error.to_string()))
        })
    }
}

#[cfg(test)]
mod tests;
