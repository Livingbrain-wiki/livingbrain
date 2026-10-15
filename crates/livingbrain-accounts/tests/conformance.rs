//! The shared Cratefield conformance suite for the three modules, on every
//! dialect the environment provides, and the wasm dependency boundary.

use cratefield_testing::{assert_wasm_safe_deps, conformance};
use livingbrain_accounts::{Access, Agents, Audit};

#[test]
fn access_conforms() {
    conformance(Box::new(Access::new()));
}

#[test]
fn agents_conform() {
    conformance(Box::new(Agents::new()));
}

#[test]
fn audit_conforms() {
    conformance(Box::new(Audit::new()));
}

#[test]
fn accounts_deps_are_wasm_safe() {
    assert_wasm_safe_deps(env!("CARGO_PKG_NAME"));
}
