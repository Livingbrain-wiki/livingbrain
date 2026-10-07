//! AC5: a budget, not a benchmark (issue #43). Its own binary so the
//! property tests in `crypto.rs` cannot contend with it for CPU.
//!
//! Measured on this module's `TestHarness` (in-memory SQLite, in-process
//! `WorkerSecretKms`), 200 pages of ~2 KB, after a warm-up, p95 over 200
//! samples: debug 0.6 ms search / 0.5 ms read, release 0.16 ms / 0.015 ms.
//! The budgets below are ~13x the debug p95, which is the build CI runs, and
//! were stable to within 30% over five consecutive runs.
//!
//! What this does *not* measure, honestly: the KMS is in-process, so an
//! unwrap is a function call rather than a network round trip. The budget
//! covers the crypto, the index and the SQL — the parts this change owns —
//! and says nothing about a real custodian's latency.

mod common;

use std::sync::Arc;
use std::time::Instant;

use cratefield_core::{Clock, UlidIdGen};
use cratefield_testing::TestHarness;
use livingbrain_pages::{Author, EntityType, PageStore, PageWrite, Pages};

#[test]
fn search_and_read_stay_inside_their_budget() {
    /// Generous enough for a loaded shared debug runner, tight enough that an
    /// extra round trip per page or per token shows up.
    const SEARCH_BUDGET_US: u128 = 8_000;
    const READ_BUDGET_US: u128 = 6_000;

    pollster::block_on(async {
        let kit = TestHarness::new(vec![Box::new(Pages::new())]);
        let clock: Arc<dyn Clock> = Arc::new(kit.clock.clone());
        let store = PageStore::new(
            kit.db.clone(),
            Arc::new(cratefield_testing::MemoryBlob::new()),
            common::kms(),
            clock,
            Arc::new(UlidIdGen),
        );
        let filler = "lorem ipsum dolor sit amet consectetur adipiscing elit ".repeat(32);
        for index in 0..200 {
            let slug = format!("page-{index:04}");
            let body = format!(
                "---\nname: Kestrel Nakamura\n---\nZephyrmoor ledger entry {index}. {filler}"
            );
            store
                .write(
                    "team",
                    &slug,
                    PageWrite {
                        entity_type: EntityType::Person,
                        markdown: body,
                        author: Author::Human {
                            id: "u1".to_owned(),
                        },
                        base_version: None,
                    },
                )
                .await
                .expect("a seeded page");
        }
        let slugs: Vec<String> = (0..200).map(|index| format!("page-{index:04}")).collect();

        // Warm-up, so the first call's page faults are not what is measured.
        for _ in 0..5 {
            let _ = store
                .search(&["team"], "zephyrmoor ledger", 20)
                .await
                .unwrap();
            let _ = store.read("team", &slugs[0]).await.unwrap();
        }

        let p95 = |mut samples: Vec<u128>| {
            samples.sort_unstable();
            samples[samples.len() * 95 / 100]
        };

        let mut searches = Vec::with_capacity(200);
        for _ in 0..200 {
            let started = Instant::now();
            let hits = store
                .search(&["team"], "zephyrmoor ledger", 20)
                .await
                .unwrap();
            searches.push(started.elapsed().as_micros());
            assert!(!hits.is_empty(), "the seeded corpus must be findable");
        }
        let mut reads = Vec::with_capacity(200);
        for slug in &slugs {
            let started = Instant::now();
            let read = store.read("team", slug).await.unwrap().unwrap();
            reads.push(started.elapsed().as_micros());
            assert!(read.markdown.contains("Zephyrmoor"));
        }
        let search_p95 = p95(searches);
        let read_p95 = p95(reads);
        println!("p95: search {search_p95}us, read {read_p95}us");
        assert!(
            search_p95 < SEARCH_BUDGET_US,
            "search p95 {search_p95}us over the {SEARCH_BUDGET_US}us budget"
        );
        assert!(
            read_p95 < READ_BUDGET_US,
            "read p95 {read_p95}us over the {READ_BUDGET_US}us budget"
        );
    });
}
