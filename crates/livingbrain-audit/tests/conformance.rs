//! The shared Cratefield conformance suite for `livingbrain-audit`.
//!
//! `conformance` runs once per dialect the environment provides: SQLite
//! always, and Postgres too when `FZ_TEST_POSTGRES_URL` names a server — which
//! is exactly the parity job in `.github/workflows/ci.yml`. One test
//! definition, both engines.

use cratefield_testing::{assert_wasm_safe_deps, conformance};
use livingbrain_audit::Audit;

#[test]
fn audit_conforms() {
    conformance(Box::new(Audit::new()));
}

#[test]
fn audit_deps_are_wasm_safe() {
    assert_wasm_safe_deps(env!("CARGO_PKG_NAME"));
}
