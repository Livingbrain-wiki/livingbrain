//! `livingbrain-canary`: the empty module — no ports, no tables, no routes.
//!
//! Its whole job is to be the first `crates/livingbrain-*` crate, so the
//! workspace, the Cratefield conformance suite (SQLite and Postgres) and the
//! wasm build are green on a module that cannot blame its own logic when they
//! are not. Real modules are added beside it, not in place of it.

#![forbid(unsafe_code)]

use cratefield_core::{Config, ConfigError, Migrations, Module, ModuleContext, Port};

/// A module that does nothing, on purpose.
#[derive(Debug, Default)]
pub struct Canary;

impl Canary {
    /// A new canary. There is nothing to configure.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Module for Canary {
    fn name(&self) -> &'static str {
        "canary"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    fn requires(&self) -> &'static [Port] {
        &[]
    }

    fn migrations(&self) -> Migrations {
        Migrations::EMPTY
    }

    fn validate_config(&self, _cfg: &dyn Config) -> Result<(), ConfigError> {
        Ok(())
    }

    fn router(&self, _ctx: ModuleContext) -> cratefield_core::axum::Router {
        cratefield_core::axum::Router::new()
    }
}
