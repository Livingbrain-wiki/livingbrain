//! The fact store (docs/temporal-facts.md) against a real migrated SQLite:
//! supersession through the store, corroboration, cardinality, retraction,
//! scope isolation, sealing at rest, forgetting and key rotation.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use cratefield_core::{Clock, Database, Statement, UlidIdGen};
use cratefield_testing::{MemoryBlob, TestHarness};
use livingbrain_memory::temporal::{DAY, days_from_civil};
use livingbrain_memory::timeline::{FactStatus, PartialDate, render_line};
use livingbrain_pages::{
    EntityType, FactError, FactInput, FactOutcome, FactStore, PageStore, Pages, person_subject,
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
    pages: PageStore,
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

fn shoe(value: &str, from: Option<&str>) -> FactInput {
    FactInput {
        subject: "person/nick".to_owned(),
        attribute: "Shoe Size".to_owned(),
        value: value.to_owned(),
        unit: Some("EU".to_owned()),
        valid_from: from.map(str::to_owned),
        source: Some(format!("note:user-a/shoe-{value}")),
        quote: Some(format!("my shoe size is {value}")),
        ..FactInput::default()
    }
}

#[test]
fn the_shoe_size_example_end_to_end() {
    pollster::block_on(async {
        let kit = kit();
        let first = kit
            .facts
            .record("user-a", shoe("40", Some("2025")), "u-a")
            .await
            .unwrap();
        assert_eq!(first.outcome, FactOutcome::Created);
        assert_eq!(first.fact.attribute, "shoe_size");
        assert_eq!(first.fact.status, FactStatus::Current);

        kit.clock.set(2026, 10, 15);
        let second = kit
            .facts
            .record("user-a", shoe("41", Some("2026")), "u-a")
            .await
            .unwrap();
        assert_eq!(second.fact.status, FactStatus::Current);
        assert_eq!(
            second.fact.supersedes.as_deref(),
            Some(first.fact.id.as_str())
        );

        let timelines = kit
            .facts
            .timelines(&["user-a"], Some("person/nick"), Some("shoe_size"))
            .await
            .unwrap();
        assert_eq!(timelines.len(), 1);
        let facts = &timelines[0].facts;
        let old = facts.iter().find(|f| f.value == "40").unwrap();
        assert_eq!(old.status, FactStatus::Superseded);
        assert_eq!(old.range(), "2025..2026");
        assert_eq!(old.superseded_by.as_deref(), Some(second.fact.id.as_str()));

        let line = render_line(facts, kit.facts.now(), &|_| Some("F1".to_owned())).unwrap();
        assert_eq!(
            line,
            "shoe_size(person/nick) = 41 EU [valid 2026..now, observed 2026-10-15, source F1] \
             (was 40 EU 2025..2026)"
        );
        let as_of = PartialDate::parse("2025").unwrap().as_of_instant();
        assert!(
            render_line(facts, as_of, &|_| None)
                .unwrap()
                .starts_with("shoe_size(person/nick) = 40 EU [valid 2025..2026")
        );
    });
}

#[test]
fn a_repeat_corroborates_and_stores_nothing() {
    pollster::block_on(async {
        let kit = kit();
        let first = kit
            .facts
            .record("user-a", shoe("40", Some("2025")), "u-a")
            .await
            .unwrap();
        kit.clock.set(2025, 8, 1);
        let again = kit
            .facts
            .record("user-a", shoe("40", Some("2025-08")), "u-a")
            .await
            .unwrap();
        assert_eq!(again.outcome, FactOutcome::Corroborated);
        assert_eq!(again.fact.id, first.fact.id);
        let all = kit.facts.timelines(&["user-a"], None, None).await.unwrap();
        assert_eq!(all[0].facts.len(), 1);
    });
}

#[test]
fn cardinality_is_a_property_of_the_attribute() {
    pollster::block_on(async {
        let kit = kit();
        let tool = |value: &str| FactInput {
            subject: "person/nick".to_owned(),
            attribute: "uses_tool".to_owned(),
            value: value.to_owned(),
            ..FactInput::default()
        };
        kit.facts
            .record("user-a", tool("ripgrep"), "u-a")
            .await
            .unwrap();
        kit.facts.record("user-a", tool("jj"), "u-a").await.unwrap();
        let timeline = &kit
            .facts
            .timelines(&["user-a"], None, Some("uses_tool"))
            .await
            .unwrap()[0];
        assert!(
            timeline
                .facts
                .iter()
                .all(|f| f.status == FactStatus::Current)
        );

        let mut one = tool("vim");
        one.cardinality = Some("one".to_owned());
        assert!(matches!(
            kit.facts.record("user-a", one, "u-a").await,
            Err(FactError::CardinalityMismatch { .. })
        ));
    });
}

#[test]
fn a_dispute_is_kept_and_a_retraction_settles_it() {
    pollster::block_on(async {
        let kit = kit();
        let a = kit
            .facts
            .record("user-a", shoe("40", Some("2025")), "u-a")
            .await
            .unwrap();
        let b = kit
            .facts
            .record("user-a", shoe("43", Some("2025")), "u-a")
            .await
            .unwrap();
        assert_eq!(b.fact.status, FactStatus::Disputed);
        let retracted = kit
            .facts
            .retract(&["user-a"], &b.fact.id)
            .await
            .unwrap()
            .expect("found");
        assert_eq!(retracted.1.status, FactStatus::Retracted);
        let facts = &kit.facts.timelines(&["user-a"], None, None).await.unwrap()[0].facts;
        let first = facts.iter().find(|f| f.id == a.fact.id).unwrap();
        assert_eq!(first.status, FactStatus::Current);
        // Another scope cannot retract it, and cannot tell it exists.
        assert!(
            kit.facts
                .retract(&["user-b"], &a.fact.id)
                .await
                .unwrap()
                .is_none()
        );
    });
}

#[test]
fn scopes_never_see_each_other() {
    pollster::block_on(async {
        let kit = kit();
        kit.facts
            .record("user-a", shoe("40", Some("2025")), "u-a")
            .await
            .unwrap();
        kit.facts
            .record("shared-two", shoe("44", Some("2025")), "u-z")
            .await
            .unwrap();
        let b = kit.facts.timelines(&["user-b"], None, None).await.unwrap();
        assert!(b.is_empty());
        let a = kit.facts.timelines(&["user-a"], None, None).await.unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].facts[0].value, "40");
        assert!(
            kit.facts
                .timelines(&[], None, None)
                .await
                .unwrap()
                .is_empty()
        );
        // The same subject in two scopes is two timelines, never merged.
        let both = kit
            .facts
            .timelines(&["user-a", "shared-two"], Some("person/nick"), None)
            .await
            .unwrap();
        assert_eq!(both.len(), 2);
        assert!(
            both.iter()
                .all(|t| t.facts[0].status == FactStatus::Current)
        );
    });
}

#[test]
fn values_are_sealed_and_redacted() {
    pollster::block_on(async {
        let kit = kit();
        let mut input = shoe("40", None);
        input.quote = Some("mail me at ada@example.com, size 40".to_owned());
        let recorded = kit.facts.record("user-a", input, "u-a").await.unwrap();
        assert!(
            !recorded
                .fact
                .quote
                .as_deref()
                .unwrap()
                .contains("ada@example.com")
        );
        let rows = kit
            .db
            .query(&Statement::new("SELECT * FROM facts"))
            .await
            .unwrap();
        let sealed = rows.rows[0].get::<String>("sealed").unwrap();
        assert!(!sealed.contains("size 40") && !sealed.contains("EU"));
        // Undated: valid from the observation instant.
        assert_eq!(recorded.fact.valid_from.text(), "2025-03-02T12:00:00Z");
    });
}

#[test]
fn bad_input_is_refused() {
    pollster::block_on(async {
        let kit = kit();
        for (input, why) in [
            (
                FactInput {
                    subject: "nick".into(),
                    ..shoe("40", None)
                },
                "subject",
            ),
            (
                FactInput {
                    subject: "planet/earth".into(),
                    ..shoe("40", None)
                },
                "subject",
            ),
            (
                FactInput {
                    attribute: "!!".into(),
                    ..shoe("40", None)
                },
                "attribute",
            ),
            (
                FactInput {
                    value: "  ".into(),
                    ..shoe("40", None)
                },
                "value",
            ),
            (shoe("40", Some("2025-13")), "valid_from"),
            (
                FactInput {
                    valid_to: Some("2024".into()),
                    ..shoe("40", Some("2025"))
                },
                "valid_to",
            ),
            (
                FactInput {
                    observed_at: Some("2030-01-01T00:00:00Z".into()),
                    ..shoe("40", None)
                },
                "observed_at",
            ),
            (
                FactInput {
                    confidence: Some(1.5),
                    ..shoe("40", None)
                },
                "confidence",
            ),
        ] {
            match kit.facts.record("user-a", input, "u-a").await {
                Err(FactError::Invalid(message)) => assert!(message.contains(why), "{message}"),
                other => panic!("{why}: {other:?}"),
            }
        }
        assert_eq!(person_subject("U0NICK").as_deref(), Some("person/u0nick"));
        assert_eq!(person_subject("a.b@c").as_deref(), Some("person/a-b-c"));
        assert_eq!(person_subject("@@"), None);
        assert!(EntityType::parse("person").is_some());
    });
}

#[test]
fn forgetting_a_scope_takes_its_facts() {
    pollster::block_on(async {
        let kit = kit();
        kit.facts
            .record("user-a", shoe("40", None), "u-a")
            .await
            .unwrap();
        kit.pages.forget_scope("user-a").await.unwrap();
        assert!(
            kit.facts
                .timelines(&["user-a"], None, None)
                .await
                .unwrap()
                .is_empty()
        );
    });
}

#[test]
fn a_rotation_reseals_facts() {
    pollster::block_on(async {
        let kit = kit();
        kit.facts
            .record("user-a", shoe("40", Some("2025")), "u-a")
            .await
            .unwrap();
        kit.pages.rotate_scope_key("user-a").await.unwrap();
        let report = kit.pages.reencrypt_scope("user-a").await.unwrap();
        assert_eq!(report.rewritten, 1);
        assert_eq!(report.pending, vec![1]);
        // Past the grace window the old key retires, and the fact still reads.
        kit.clock.set(2025, 3, 3);
        let report = kit.pages.reencrypt_scope("user-a").await.unwrap();
        assert_eq!(report.retired, vec![1]);
        let facts = &kit.facts.timelines(&["user-a"], None, None).await.unwrap()[0].facts;
        assert_eq!(facts[0].value, "40");
    });
}

#[test]
fn the_index_lists_timelines_without_opening_them() {
    pollster::block_on(async {
        let kit = kit();
        kit.facts
            .record("user-a", shoe("40", None), "u-a")
            .await
            .unwrap();
        kit.facts
            .record("user-b", shoe("41", None), "u-b")
            .await
            .unwrap();
        let index = kit.facts.index(&["user-a"]).await.unwrap();
        assert_eq!(
            index,
            vec![(
                "user-a".to_owned(),
                "person/nick".to_owned(),
                "shoe_size".to_owned()
            )]
        );
        let timeline = kit
            .facts
            .timeline("user-a", "person/nick", "shoe_size")
            .await
            .unwrap();
        assert_eq!(timeline.facts[0].value, "40");
    });
}
