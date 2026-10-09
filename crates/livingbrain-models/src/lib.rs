//! `livingbrain-models`: bring your own model, with a capability check.
//!
//! Per workspace and per role, a member with admin or owner rights can
//! connect any provider in the vendored catalog — the providers Colonizer
//! supports, over the Anthropic Messages or the OpenAI chat wire, with a
//! bearer or `x-api-key` key — or a custom endpoint that names its own. On
//! connect, the module probes the model for tool calling, JSON output and
//! context size, and stores the connection with the provider key encrypted
//! (AES-256-GCM, AAD = workspace_id + role). No silent fallback to the
//! managed model: `fallback_to_managed` defaults `false` and is only set
//! when a member explicitly sends it.
//!
//! Keys are never shown in full: the API shows `…` followed by the last
//! four characters, and the ciphertext is never returned. Provider
//! response bodies are never echoed in error responses, because they can
//! echo the key.
//!
//! The SSRF guard rejects non-HTTPS schemes, userinfo, localhost and
//! single-label hosts, every private/loopback/link-local IP range, and
//! hostnames whose DNS answers resolve to a private address. It runs
//! before the probe, on every connect.
//!
//! `MODEL_KEYS_SECRET` is the deployment's key-encryption secret. It is
//! read per request rather than at `Harness::build`, so a deployment
//! without it still installs and serves reads; a *connect* without it is
//! refused with a 500 rather than storing the key in plaintext.

#![forbid(unsafe_code)]

mod catalog;
mod crypto;
mod handlers;
mod probe;
mod ssrf;
mod store;

use cratefield_core::{
    Config, ConfigError, DataKind, Disposition, Migrations, Module, ModuleContext, PersonalDataSet,
    Port, SqlMigration,
};

/// The one migration: `model_connections`.
const MIGRATION_INIT: SqlMigration = SqlMigration::new(
    "0001",
    "init",
    include_str!("../migrations/sqlite/0001_init.sql"),
);

/// The Postgres form of [`MIGRATION_INIT`]. The schema is the portable
/// subset (ADR 0004), so the SQL is the sqlite file's with `BYTEA` for
/// the ciphertext column; the set is shipped anyway so the parity job
/// applies a Postgres migration rather than falling back to the sqlite
/// one.
const MIGRATION_INIT_POSTGRES: SqlMigration = SqlMigration::new(
    "0001",
    "init",
    include_str!("../migrations/postgres/0001_init.sql"),
);

/// The second migration: how each connection talks to its provider.
const MIGRATION_PROVIDER_WIRE: SqlMigration = SqlMigration::new(
    "0002",
    "provider_wire",
    include_str!("../migrations/sqlite/0002_provider_wire.sql"),
);

/// The Postgres form of [`MIGRATION_PROVIDER_WIRE`]; the SQL is the same.
const MIGRATION_PROVIDER_WIRE_POSTGRES: SqlMigration = SqlMigration::new(
    "0002",
    "provider_wire",
    include_str!("../migrations/postgres/0002_provider_wire.sql"),
);

/// Bring your own model, with a capability check.
#[derive(Debug, Default)]
pub struct Models;

impl Models {
    /// A new models module. There is nothing to configure in the builder;
    /// the encryption secret comes from config at router build.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Module for Models {
    fn name(&self) -> &'static str {
        "models"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    /// The ports the handlers reach. `Signer` and `Clock` verify the
    /// session cookie (shared with the workspaces module), `HttpClient`
    /// runs the probe and the DoH resolution, `IdGen` mints the AES-GCM
    /// nonce, and the database holds the `model_connections` table.
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
        &["model_connections"]
    }

    /// The ciphertext column holds a provider API key (a credential).
    fn personal_data(&self) -> &'static [PersonalDataSet] {
        const SETS: &[PersonalDataSet] = &[PersonalDataSet {
            table: "model_connections",
            subject: "updated_by",
            kind: DataKind::Identifier,
            disposition: Disposition::Erase,
            description: "For each model role a workspace connected: the provider, base URL, \
                          model name, the encrypted API key (ciphertext), the key's last four \
                          characters, the capability check's result, and the Slack user id of \
                          the member who last changed it.",
            redacted: &["key_ciphertext", "key_last4"],
            subject_via: None,
        }];
        SETS
    }

    fn migrations(&self) -> Migrations {
        const MIGRATIONS: [SqlMigration; 2] = [MIGRATION_INIT, MIGRATION_PROVIDER_WIRE];
        const _: () = cratefield_core::assert_migration_set(&MIGRATIONS);
        const MIGRATIONS_POSTGRES: [SqlMigration; 2] =
            [MIGRATION_INIT_POSTGRES, MIGRATION_PROVIDER_WIRE_POSTGRES];
        const _: () = cratefield_core::assert_migration_set(&MIGRATIONS_POSTGRES);
        Migrations {
            sqlite: &MIGRATIONS,
            postgres: &MIGRATIONS_POSTGRES,
        }
    }

    /// `MODEL_KEYS_SECRET` is the deployment's key-encryption secret, and
    /// the key is a bare SHA-256 of it with no key-stretching, so its
    /// length *is* the entropy: a short or memorable secret is
    /// brute-forceable from one ciphertext. A secret that is set must
    /// therefore be at least 32 bytes — `openssl rand -base64 32`.
    ///
    /// Like the workspaces module's Slack keys it is optional at build
    /// time, so a deployment that serves no model connection still
    /// installs and is not failed for a secret it does not use; a secret
    /// that *is* set and is short is a mistake worth reporting. The
    /// runtime half is the 500 a connect answers when the secret is
    /// absent, because that is where it actually matters.
    fn validate_config(&self, cfg: &dyn Config) -> Result<(), ConfigError> {
        let Some(secret) = cfg.get(crypto::MODEL_KEYS_SECRET) else {
            return Ok(());
        };
        if secret.trim().is_empty() {
            return Err(config_error("MODEL_KEYS_SECRET is set but blank"));
        }
        if secret.len() < crypto::MINIMUM_SECRET_BYTES {
            return Err(config_error(&format!(
                "MODEL_KEYS_SECRET is {} bytes; the encryption key is a bare SHA-256 \
                 of it, so it must be at least {} bytes of random text — \
                 `openssl rand -base64 32`",
                secret.len(),
                crypto::MINIMUM_SECRET_BYTES
            )));
        }
        Ok(())
    }

    fn router(&self, ctx: ModuleContext) -> cratefield_core::axum::Router {
        handlers::router(ctx)
    }
}

/// One problem to report about the deployment's configuration.
fn config_error(problem: &str) -> ConfigError {
    let mut error = ConfigError::new();
    error.push(problem);
    error
}
