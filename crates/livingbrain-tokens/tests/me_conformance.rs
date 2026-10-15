//! The conformance suite for the `me` module (ADR 0004).

use cratefield_testing::conformance;
use livingbrain_tokens::Me;

#[test]
fn me_conforms() {
    conformance(Box::new(Me::new()));
}
