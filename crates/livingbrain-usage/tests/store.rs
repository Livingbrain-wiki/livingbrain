//! Behaviour tests for the usage store, against a real SQL database (the
//! kit's in-memory SQLite adapter, with the module's migrations applied).
//!
//! The tests are about the two things that could silently be wrong: two
//! records losing each other's counts (the upsert must add, never
//! overwrite), and one workspace's ledger bleeding into another's read.

mod support;

use livingbrain_usage::{DayRow, UsageError, days, record_d1, record_email, record_model, utc_day};
use support::WORKSPACE;

/// The instant today, from the kit's fixed clock.
fn now() -> time::OffsetDateTime {
    support::now()
}

/// The day `offset` days before today (offset 0 is today).
fn day(offset: i64) -> String {
    utc_day(support::now() - time::Duration::days(offset))
}

/// Two records in one day must add up: the upsert's `DO UPDATE` adds the
/// incoming counts to the row, so a concurrent-looking pair merges instead
/// of one overwriting the other.
#[test]
fn two_records_on_the_same_day_merge_instead_of_overwriting() {
    pollster::block_on(async {
        let kit = support::harness();
        record_model(&*kit.db, now(), WORKSPACE, 100, 40)
            .await
            .expect("the first record lands");
        record_model(&*kit.db, now(), WORKSPACE, 250, 60)
            .await
            .expect("the second record lands");

        let rows = days(&*kit.db, WORKSPACE, &day(0), &day(0))
            .await
            .expect("the day reads back");
        assert_eq!(rows.len(), 1, "one day, one row: {rows:?}");
        assert_eq!(rows[0].day, day(0));
        assert_eq!(rows[0].model_requests, 2);
        assert_eq!(rows[0].prompt_tokens, 350);
        assert_eq!(rows[0].completion_tokens, 100);
    });
}

/// A record lands on the day its `now` names, so a caller can backfill a
/// past day after an outage — and two days stay two rows.
#[test]
fn different_days_are_separate_rows() {
    pollster::block_on(async {
        let kit = support::harness();
        record_model(&*kit.db, now(), WORKSPACE, 10, 1)
            .await
            .expect("today lands");
        record_model(&*kit.db, now() - time::Duration::days(1), WORKSPACE, 20, 2)
            .await
            .expect("yesterday lands");

        let rows = days(&*kit.db, WORKSPACE, &day(1), &day(0))
            .await
            .expect("the window reads back");
        assert_eq!(
            rows,
            vec![
                DayRow {
                    day: day(1),
                    model_requests: 1,
                    prompt_tokens: 20,
                    completion_tokens: 2,
                    d1_queries: 0,
                    emails_sent: 0,
                },
                DayRow {
                    day: day(0),
                    model_requests: 1,
                    prompt_tokens: 10,
                    completion_tokens: 1,
                    d1_queries: 0,
                    emails_sent: 0,
                },
            ],
            "oldest first, each day its own counts"
        );
    });
}

/// Email and D1 records write their own columns and nothing else: a day
/// row built by three different record paths holds all three facts.
#[test]
fn email_and_d1_records_land_in_their_own_columns() {
    pollster::block_on(async {
        let kit = support::harness();
        record_email(&*kit.db, now(), WORKSPACE)
            .await
            .expect("the send lands");
        record_email(&*kit.db, now(), WORKSPACE)
            .await
            .expect("the send lands");
        record_d1(&*kit.db, now(), WORKSPACE, 7)
            .await
            .expect("the queries land");

        let rows = days(&*kit.db, WORKSPACE, &day(0), &day(0))
            .await
            .expect("the day reads back");
        assert_eq!(rows[0].emails_sent, 2);
        assert_eq!(rows[0].d1_queries, 7);
        assert_eq!(rows[0].model_requests, 0, "the untouched columns stay zero");
        assert_eq!(rows[0].prompt_tokens, 0);
        assert_eq!(rows[0].completion_tokens, 0);
    });
}

/// The window is inclusive of both bounds and leaks nothing outside it.
#[test]
fn the_window_filters_by_both_bounds() {
    pollster::block_on(async {
        let kit = support::harness();
        for offset in [0, 1, 2, 3] {
            record_model(
                &*kit.db,
                now() - time::Duration::days(offset),
                WORKSPACE,
                (offset + 1) as u64,
                0,
            )
            .await
            .expect("the day lands");
        }

        let middle = days(&*kit.db, WORKSPACE, &day(2), &day(1))
            .await
            .expect("the window reads back");
        assert_eq!(
            middle
                .iter()
                .map(|row| row.day.as_str())
                .collect::<Vec<_>>(),
            vec![day(2), day(1)],
            "the bounds are inclusive, nothing outside leaks"
        );

        let all = days(&*kit.db, WORKSPACE, &day(3), &day(0))
            .await
            .expect("the window reads back");
        assert_eq!(all.len(), 4);
    });
}

/// Every read names its workspace; another workspace's day is empty, not
/// somebody else's numbers.
#[test]
fn one_workspaces_ledger_is_invisible_from_another() {
    pollster::block_on(async {
        let kit = support::harness();
        record_model(&*kit.db, now(), WORKSPACE, 500, 100)
            .await
            .expect("the record lands");

        let other = days(&*kit.db, "T0OTHER", &day(0), &day(0))
            .await
            .expect("the other workspace reads");
        assert!(
            other.is_empty(),
            "no row leaks across workspaces: {other:?}"
        );
    });
}

/// A workspace id outside the shared rule is refused before the database
/// sees a statement.
#[test]
fn a_workspace_id_outside_the_rule_is_refused() {
    pollster::block_on(async {
        let kit = support::harness();
        let error = record_model(&*kit.db, now(), "", 1, 1)
            .await
            .expect_err("an empty workspace id is refused");
        assert!(matches!(error, UsageError::InvalidWorkspace(_)), "{error}");

        let error = days(&*kit.db, "with space", &day(0), &day(0))
            .await
            .expect_err("a workspace id with whitespace is refused");
        assert!(matches!(error, UsageError::InvalidWorkspace(_)), "{error}");
    });
}

/// A day bound that is not a `YYYY-MM-DD` calendar date is refused by
/// shape, before it can be compared as a string.
#[test]
fn a_day_outside_the_format_is_refused() {
    pollster::block_on(async {
        let kit = support::harness();
        record_model(&*kit.db, now(), WORKSPACE, 1, 1)
            .await
            .expect("today lands");

        for bound in ["2026-13-01", "20261010", "2026-10-10T00:00:00Z", ""] {
            let error = days(&*kit.db, WORKSPACE, bound, &day(0))
                .await
                .expect_err("a malformed day bound is refused");
            assert!(
                matches!(&error, UsageError::InvalidDay(value) if value == bound),
                "{bound}: {error}"
            );
        }
    });
}
