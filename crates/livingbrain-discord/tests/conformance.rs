//! The shared Cratefield conformance suite for `livingbrain-discord`:
//! migrations applied (twice) on SQLite always, and on Postgres when
//! `FZ_TEST_POSTGRES_URL` names a server.

use cratefield_testing::{assert_wasm_safe_deps, conformance};
use livingbrain_discord::DiscordModule;

#[test]
fn discord_conforms() {
    conformance(Box::new(DiscordModule::new()));
}

#[test]
fn discord_deps_are_wasm_safe() {
    assert_wasm_safe_deps(env!("CARGO_PKG_NAME"));
}
