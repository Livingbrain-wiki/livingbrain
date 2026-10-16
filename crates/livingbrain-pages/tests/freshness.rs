//! Freshness and dreaming on the fact store (docs/temporal-facts.md,
//! "Freshness" and "Dreaming"): a repeat confirms instead of duplicating, a
//! fact can be ended without being replaced, and the dream queue, its claim
//! and its decision log behave as the sweep needs them to.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use cratefield_core::{Clock, Database, UlidIdGen};
use cratefield_testing::{MemoryBlob, TestHarness};
use livingbrain_memory::temporal::{DAY, days_from_civil};
use livingbrain_memory::timeline::{FactStatus, PartialDate};
use livingbrain_pages::{
    DreamDecisionRow, DreamFinish, FactError, FactInput, FactOutcome, FactStore, PageStore, Pages,
    fact_json,
};
use time::OffsetDateTime;

/// A clock the test moves by hand.
#[derive(Clone)]
struct HandClock(Arc<AtomicI64>);

impl HandClock {
    fn set(&self, year: i64, month: u32, day: u32) {
        self.0.store(
            days_from_civil(year, month, day) * DAY + 12 * 3600,
            Ordering::SeqCst,
        );
    }
}

impl Clock for HandClock {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(self.0.load(Ordering::SeqCst)).expect("an epoch")
    }
}

struct Kit {
    facts: FactStore,
    #[allow(dead_code)]
    pages: PageStore,
    #[allow(dead_code)]
    db: Arc<dyn Database>,
    clock: HandClock,
}

fn kit() -> Kit {
    let harness = TestHarness::new(vec![Box::new(Pages::new())]);
    let clock = HandClock(Arc::new(AtomicI64::new(0)));
    clock.set(2025, 3, 2);
    let dyn_clock: Arc<dyn Clock> = Arc::new(clock.clone());
    let kms = common::kms();
    Kit {
        facts: FactStore::new(
            harness.db.clone(),
            kms.clone(),
            dyn_clock.clone(),
            Arc::new(UlidIdGen),
        ),
        pages: PageStore::new(
            harness.db.clone(),
            Arc::new(MemoryBlob::new()),
            kms,
            dyn_clock,
            Arc::new(UlidIdGen),
        ),
        db: harness.db.clone(),
        clock,
    }
}

fn input(attribute: &str, value: &str, from: Option<&str>) -> FactInput {
    FactInput {
        subject: "project/demo".to_owned(),
        attribute: attribute.to_owned(),
        value: value.to_owned(),
        valid_from: from.map(str::to_owned),
        source: Some(format!("source:{attribute}")),
        ..FactInput::default()
    }
}

#[test]
fn a_repeat_confirms_the_fact_instead_of_duplicating_it() {
    pollster::block_on(async {
        let kit = kit();
        let first = kit
            .facts
            .record("user-a", input("database", "D1", Some("2025")), "u-a")
            .await
            .unwrap();
        assert_eq!(first.fact.last_confirmed_at, first.fact.observed_at);

        kit.clock.set(2026, 10, 15);
        let again = kit
            .facts
            .record("user-a", input("database", "D1", Some("2025")), "u-a")
            .await
            .unwrap();
        assert_eq!(again.outcome, FactOutcome::Corroborated);
        assert_eq!(again.fact.id, first.fact.id);

        let timeline = kit
            .facts
            .timelines(&["user-a"], Some("project/demo"), Some("database"))
            .await
            .unwrap();
        assert_eq!(timeline[0].facts.len(), 1, "no second row");
        let stored = &timeline[0].facts[0];
        assert!(stored.last_confirmed_at > stored.observed_at);
        assert_eq!(
            fact_json(stored)["last_confirmed_at"],
            serde_json::json!("2026-10-15T12:00:00Z")
        );
    });
}

#[test]
fn a_fact_can_be_ended_without_a_successor_and_is_kept() {
    pollster::block_on(async {
        let kit = kit();
        let first = kit
            .facts
            .record(
                "user-a",
                input("deploy_target", "vercel", Some("2024")),
                "u-a",
            )
            .await
            .unwrap();
        let ended = kit
            .facts
            .end("user-a", &first.fact.id, "2025-06")
            .await
            .unwrap()
            .expect("the fact is in the scope");
        assert_eq!(ended.status, FactStatus::Current);
        assert_eq!(
            ended.valid_to.as_ref().map(PartialDate::text),
            Some("2025-06")
        );
        let as_of = PartialDate::parse("2025-07").unwrap().as_of_instant();
        assert!(!ended.valid_at(as_of), "no longer true in July");

        assert!(matches!(
            kit.facts.end("user-a", &first.fact.id, "2024").await,
            Err(FactError::Invalid(_))
        ));
        assert!(
            kit.facts
                .end("user-b", &first.fact.id, "2025-06")
                .await
                .unwrap()
                .is_none(),
            "another scope's fact is not reachable"
        );
    });
}

#[test]
fn a_fact_retracted_cannot_be_ended() {
    pollster::block_on(async {
        let kit = kit();
        let first = kit
            .facts
            .record("user-a", input("hosting", "fly", Some("2024")), "u-a")
            .await
            .unwrap();
        kit.facts
            .retract(&["user-a"], &first.fact.id)
            .await
            .unwrap()
            .expect("retracted");
        assert!(matches!(
            kit.facts.end("user-a", &first.fact.id, "2025-01").await,
            Err(FactError::Invalid(_))
        ));
    });
}

#[test]
fn the_dream_queue_finds_a_subject_claims_it_once_and_keeps_a_record() {
    pollster::block_on(async {
        let kit = kit();
        for n in 0..20 {
            kit.facts
                .record(
                    "user-a",
                    input(&format!("tool_{n}"), "yes", Some("2025")),
                    "u-a",
                )
                .await
                .unwrap();
        }
        let candidates = kit.facts.dream_candidates(20, 10).await.unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].current, 20);
        assert_eq!(candidates[0].fresh, 20, "never tidied: all facts are new");
        assert!(candidates[0].dreamt_at.is_none());

        let now = kit.facts.now();
        kit.facts
            .dream_enqueue("user-a", "project/demo", now)
            .await
            .unwrap();
        assert_eq!(
            kit.facts.dream_queued(5).await.unwrap(),
            vec![("user-a".to_owned(), "project/demo".to_owned())]
        );
        assert!(
            kit.facts
                .dream_claim("user-a", "project/demo", now, now + 600)
                .await
                .unwrap()
        );
        assert!(
            !kit.facts
                .dream_claim("user-a", "project/demo", now, now + 600)
                .await
                .unwrap(),
            "a second claim finds it running"
        );

        let first_fact = kit
            .facts
            .timelines(&["user-a"], Some("project/demo"), Some("tool_0"))
            .await
            .unwrap()[0]
            .facts[0]
            .id
            .clone();
        let at = kit.facts.now();
        let stamp = livingbrain_memory::timeline::rfc3339(at);
        kit.facts
            .dream_log(&DreamDecisionRow {
                scope: "user-a",
                dream_at: &stamp,
                seq: 0,
                subject: "project/demo",
                op: "retract",
                fact_id: &first_fact,
                by_fact_id: None,
                new_valid_to: None,
                new_attribute: None,
                reason: Some("event"),
                applied: true,
                input_ids: "[]",
                model: "dreaming",
                decided_at: &stamp,
            })
            .await
            .unwrap();
        kit.facts
            .dream_finish(
                "user-a",
                "project/demo",
                &DreamFinish {
                    status: "done",
                    dreamt: true,
                    facts_seen: 20,
                    decisions: 1,
                    applied: 1,
                    error: None,
                },
                at,
            )
            .await
            .unwrap();

        let outcomes = kit
            .facts
            .dream_outcomes(&["user-a"], Some("project/demo"))
            .await
            .unwrap();
        assert_eq!(outcomes.passes.len(), 1);
        assert_eq!(outcomes.passes[0].applied, 1);
        assert_eq!(
            outcomes.edits,
            vec![(
                "user-a".to_owned(),
                "project/demo".to_owned(),
                first_fact,
                "retract".to_owned()
            )]
        );
        assert!(kit.facts.dream_queued(5).await.unwrap().is_empty());
        assert!(
            kit.facts.dream_candidates(20, 10).await.unwrap()[0]
                .dreamt_at
                .is_some()
        );
        // Nothing is fresh after a pass: the next one waits for new facts.
        assert_eq!(
            kit.facts.dream_candidates(20, 10).await.unwrap()[0].fresh,
            0
        );
    });
}
