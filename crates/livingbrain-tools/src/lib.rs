//! `livingbrain-tools`: Livingbrain as an MCP *client* of remote servers.
//!
//! A member connects a remote MCP server per provider; the token is sealed
//! at rest under the workspaces module's envelope and opened only on the
//! call path. Everything runs through the [`gate`]: a tool is offered only
//! when neither the connection nor the workspace disabled it, a read may
//! run through a teammate's connection, and a write runs only on the
//! asker's own connection or after the owner approved the recorded call.
//! An unannotated tool is a write: the classification fails closed.
//!
//! No routes yet — connecting a server and the Slack interactivity
//! endpoint that carries the approve/deny clicks are follow-ups.

#![forbid(unsafe_code)]

mod client;
mod envelope;
mod gate;
mod store;
mod tools;

use cratefield_core::{
    Config, ConfigError, DataKind, Disposition, Migrations, Module, ModuleContext, PersonalDataSet,
    Port, SqlMigration,
};

pub use crate::client::{ClientError, Mcp, ToolResult};
pub use crate::envelope::{SealedToken, Token, open, seal};
pub use crate::gate::{
    ToolError, ToolOutcome, ToolPorts, ToolRequest, offered_for, resolve, run_tool,
};
pub use crate::store::{
    ConnectionRecord, PendingCall, get_connection, put_connection, put_pending,
    set_workspace_disabled_tools, take_pending, workspace_disabled_tools,
};
pub use crate::tools::{Effect, ToolDescriptor, classify, offered_tools};

const MIGRATION_INIT: SqlMigration = SqlMigration::new(
    "0001",
    "init",
    include_str!("../migrations/sqlite/0001_init.sql"),
);

/// The Postgres form of [`MIGRATION_INIT`]: the same statements — the
/// schema is the portable subset (ADR 0004) — so the parity job applies a
/// Postgres migration rather than falling back to the sqlite one.
const MIGRATION_INIT_POSTGRES: SqlMigration = SqlMigration::new(
    "0001",
    "init",
    include_str!("../migrations/postgres/0001_init.sql"),
);

/// Livingbrain as an MCP client of remote servers, behind a write gate.
#[derive(Debug, Default)]
pub struct Tools;

impl Tools {
    /// A new tools module. The token envelope's key custodian is the
    /// composition's, as for the pages module.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Module for Tools {
    fn name(&self) -> &'static str {
        "tools"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    fn requires(&self) -> &'static [Port] {
        &[Port::Db, Port::HttpClient, Port::Clock, Port::IdGen]
    }

    fn tables(&self) -> &'static [&'static str] {
        &[
            "tool_connections",
            "tool_workspace_settings",
            "tool_pending_approvals",
        ]
    }

    /// `tool_connections` holds a sealed credential per member; the other
    /// two tables name nobody.
    fn personal_data(&self) -> &'static [PersonalDataSet] {
        const SETS: &[PersonalDataSet] = &[
            PersonalDataSet {
                table: "tool_connections",
                subject: "member_id",
                kind: DataKind::Identifier,
                disposition: Disposition::Erase,
                description: "For each remote MCP server you connected: the provider, the \
                              server's URL and the tool names switched off on the connection. \
                              The connection token is held only sealed — a fresh KMS-wrapped \
                              key per row, XChaCha20-Poly1305, the workspace, your member id \
                              and the provider as the AAD — so a row cannot be replayed as a \
                              credential.",
                redacted: &["wrapped_dek", "nonce", "ciphertext"],
                subject_via: None,
            },
            PersonalDataSet::none(
                "tool_workspace_settings",
                "one row per workspace: the tool names an admin disabled, and no person \
                 is named",
            ),
            PersonalDataSet {
                table: "tool_pending_approvals",
                subject: "asker_member_id",
                kind: DataKind::Identifier,
                disposition: Disposition::Erase,
                description: "A write you asked to run on a teammate's connection, while it \
                              waits for their approve or deny: whose connection it names, \
                              which tool, the arguments, and when it was filed. The row is \
                              deleted the moment the decision lands.",
                redacted: &[],
                subject_via: None,
            },
        ];
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
        // No module-owned secret: a deployment without a key ring still
        // installs — connecting and running then fail per call.
        Ok(())
    }

    fn router(&self, _ctx: ModuleContext) -> cratefield_core::axum::Router {
        // Connect/callback and the Slack interactivity endpoint are
        // follow-ups (issue #11).
        cratefield_core::axum::Router::new()
    }
}
