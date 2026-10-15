//! The shared Cratefield conformance suite for `livingbrain-github-app`:
//! migrations applied (twice) on SQLite always, and on Postgres when
//! `FZ_TEST_POSTGRES_URL` names a server — CI's parity job.

use cratefield_testing::{assert_wasm_safe_deps, conformance};
use livingbrain_github_app::GithubApp;

#[test]
fn github_conforms() {
    conformance(Box::new(GithubApp::new()));
}

#[test]
fn github_deps_are_wasm_safe() {
    assert_wasm_safe_deps(env!("CARGO_PKG_NAME"));
}
