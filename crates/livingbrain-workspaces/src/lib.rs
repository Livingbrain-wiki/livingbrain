//! `livingbrain-workspaces`: one tenant per Slack workspace.
//!
//! The module owns the two tables that make a workspace a tenant and the
//! Slack sign-in that creates them. The first person to sign in owns the
//! workspace, and that is the only moment an owner is ever decided.
//!
//! Isolation is enforced here, not by the deployment: one D1 holds every
//! workspace, every row is scoped by a workspace id, and that id always
//! comes from the verified session cookie or from a Slack `id_token` this
//! server asked for — never from request input. The ADR records why, and
//! when to revisit it.
//!
//! [`apply_user_change`] is the seam issue #6's Events webhook calls to
//! refresh the member mirror, after it has verified the event's signature
//! and read the team id out of the signed envelope.

#![forbid(unsafe_code)]

mod config;
mod flow;
mod handlers;
mod slack;
mod store;

pub use config::Settings;
pub use flow::{FLOW_COOKIE, FLOW_PURPOSE, SESSION_COOKIE, SESSION_PURPOSE};
pub use store::{UserChange, apply_user_change};

use cratefield_core::{
    Config, ConfigError, DataKind, Disposition, Migrations, Module, ModuleContext, PersonalDataSet,
    Port, SqlMigration,
};

/// The one migration: `workspaces` and `workspace_members`.
const MIGRATION_INIT: SqlMigration = SqlMigration::new(
    "0001",
    "init",
    include_str!("../migrations/sqlite/0001_init.sql"),
);

/// The Postgres form of [`MIGRATION_INIT`]. The schema is the portable
/// subset (ADR 0004), so the SQL is the sqlite file's unchanged; the set
/// is shipped anyway so the parity job applies a Postgres migration rather
/// than falling back to the sqlite one.
const MIGRATION_INIT_POSTGRES: SqlMigration = SqlMigration::new(
    "0001",
    "init",
    include_str!("../migrations/postgres/0001_init.sql"),
);

/// Workspaces, Slack sign-in and the member mirror.
#[derive(Debug, Default)]
pub struct Workspaces;

impl Workspaces {
    /// A new workspaces module. There is nothing to configure in the
    /// builder; the Slack app settings come from config at router build.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Module for Workspaces {
    fn name(&self) -> &'static str {
        "workspaces"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    /// The ports the handlers reach. `Signer` seals the flow and session
    /// cookies, `HttpClient` talks to Slack's token endpoint, `Clock`
    /// decides expiry, `IdGen` mints the CSRF state and the nonce, and the
    /// database holds the two tables.
    fn requires(&self) -> &'static [Port] {
        &[
            Port::Db,
            Port::Signer,
            Port::HttpClient,
            Port::Clock,
            Port::IdGen,
        ]
    }

    fn tables(&self) -> &'static [&'static str] {
        &["workspaces", "workspace_members"]
    }

    /// Both tables hold a Slack id for a person. `workspaces` names the
    /// owner, and `workspace_members` names every member with the display
    /// name and timezone Slack reports for them.
    fn personal_data(&self) -> &'static [PersonalDataSet] {
        const SETS: &[PersonalDataSet] = &[
            PersonalDataSet {
                table: "workspaces",
                subject: "owner_id",
                kind: DataKind::Identifier,
                disposition: Disposition::Erase,
                description: "The Slack workspace's id and name, and the Slack user id \
                              of the person who first signed in — the workspace owner.",
                redacted: &[],
                subject_via: None,
            },
            PersonalDataSet {
                table: "workspace_members",
                subject: "user_id",
                kind: DataKind::Contact,
                disposition: Disposition::Erase,
                description: "For each workspace you signed in to: your Slack user id, \
                              the display name Slack reports for you, your timezone and \
                              whether Slack marks you an admin.",
                redacted: &[],
                subject_via: None,
            },
        ];
        SETS
    }

    fn migrations(&self) -> Migrations {
        const MIGRATIONS: [SqlMigration; 1] = [MIGRATION_INIT];
        // The array is the apply order; this refuses a gap, a duplicate or
        // an entry out of order at build time.
        const _: () = cratefield_core::assert_migration_set(&MIGRATIONS);
        const MIGRATIONS_POSTGRES: [SqlMigration; 1] = [MIGRATION_INIT_POSTGRES];
        const _: () = cratefield_core::assert_migration_set(&MIGRATIONS_POSTGRES);
        Migrations {
            sqlite: &MIGRATIONS,
            postgres: &MIGRATIONS_POSTGRES,
        }
    }

    /// Reports the Slack app keys a deployment must set. This is
    /// `fz doctor`'s half; the runtime half is the 503 the handlers answer
    /// when the keys are absent, because the router must still install.
    fn validate_config(&self, cfg: &dyn Config) -> Result<(), ConfigError> {
        Settings::from_config(cfg).map(|_| ())
    }

    fn router(&self, ctx: ModuleContext) -> cratefield_core::axum::Router {
        handlers::router(ctx)
    }
}
