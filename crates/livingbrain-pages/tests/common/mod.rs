//! The one thing every test binary in this module needs and cannot invent:
//! a key custodian. A `MemoryBlob` and a `TestHarness` are a two-line
//! constructor each and live where they are used.
//!
//! The recording fakes `crypto.rs` needs — a blob store that remembers what
//! was written to it, a database that remembers every statement — live in
//! that file, because only those tests want them.

#![allow(dead_code)]

use std::sync::Arc;

use base64::Engine as _;
use cratefield_kms::{Kms, WorkerSecretKms};

/// A key custodian over an in-process key ring: the production
/// `WorkerSecretKms` shape — `HARNESS_KEK_CURRENT` plus `HARNESS_KEK_V<n>`,
/// base64 of 32 bytes — served by a closure, so a test needs no file and no
/// environment variable. `LocalFileKms` would have been the other answer and
/// it wants a path on disk.
pub fn kms() -> Arc<dyn Kms> {
    let key = base64::engine::general_purpose::STANDARD.encode([7_u8; 32]);
    Arc::new(
        WorkerSecretKms::from_lookup(|name| match name {
            "HARNESS_KEK_CURRENT" => Some("1".to_owned()),
            "HARNESS_KEK_V1" => Some(key.clone()),
            _ => None,
        })
        .expect("the key ring is well formed"),
    )
}
