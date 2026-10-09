//! The shared Cratefield conformance suite, for all four modules this
//! crate mounts, and the wasm-dependency rule the venture compiles under.
//!
//! `conformance` runs once per dialect the environment provides: SQLite
//! always, and Postgres too when `FZ_TEST_POSTGRES_URL` names a server.
//! The modules are constructed the way the venture constructs them — one
//! `Wiki` shared by all four, over whatever ports the kit provides.

use std::sync::Arc;

use cratefield_kms::{Dek, LocalFileKms};
use cratefield_testing::{assert_wasm_safe_deps, conformance};
use livingbrain_api::{Ask, Export, Notes, Search, Wiki};

/// One `Wiki` over a fresh custodian and an authenticator that
/// refuses everything — the conformance probes mount modules and probe
/// their routes, and a refusal is as good an answer as any for that.
fn wiki() -> Wiki {
    Wiki::new(
        Arc::new(
            LocalFileKms::from_key(Dek::generate().expect("a key"), "test-kek", "test")
                .expect("a well-formed key"),
        ),
        Arc::new(Refuser),
    )
}

/// A bearer resolver that names nobody: the conformance kit probes route
/// presence, not credentials, and a 401 is this surface's presence probe
/// answer.
#[derive(Default)]
struct Refuser;

#[async_trait::async_trait]
impl livingbrain_mcp::BearerAuth for Refuser {
    async fn authenticate(
        &self,
        _ports: &cratefield_core::Ports,
        _token: &str,
    ) -> Result<livingbrain_mcp::Asker, livingbrain_mcp::AuthError> {
        Err(livingbrain_mcp::AuthError::Rejected)
    }
}

#[test]
fn the_four_modules_conform() {
    conformance(Box::new(Notes::new(wiki())));
    conformance(Box::new(Search::new(wiki())));
    conformance(Box::new(Ask::new(wiki())));
    conformance(Box::new(Export::new(wiki())));
}

#[test]
fn the_crate_deps_are_wasm_safe() {
    assert_wasm_safe_deps(env!("CARGO_PKG_NAME"));
}
