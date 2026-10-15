//! The compiler is not a module (the `coding-agents` module in
//! `livingbrain-api` mounts it), so the one rule checked here is the one
//! every crate the Worker links must keep: its dependencies build for wasm.

use cratefield_testing::assert_wasm_safe_deps;

#[test]
fn the_crate_deps_are_wasm_safe() {
    assert_wasm_safe_deps(env!("CARGO_PKG_NAME"));
}
