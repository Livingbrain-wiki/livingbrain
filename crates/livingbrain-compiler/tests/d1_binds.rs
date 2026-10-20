//! The compiler's reads and writes over a scope list stay under D1's
//! 100-bind ceiling.
//!
//! The queue's statements bind one value per scope they name, and a
//! workspace's own list can name more scopes than one statement may bind. Each
//! test seeds jobs and units in 130 scopes, reads them through `BindLimited`
//! over the real SQLite database, and checks the answer against the same call
//! on the unlimited database: the split must not change what is returned.

use std::sync::Arc;

use cratefield_core::{
    Config, ConfigError, Database, Migrations, Module, ModuleContext, Port, SqlMigration, Statement,
};
use cratefield_testing::TestHarness;
use livingbrain_compiler::{ClaimFilter, Queue};
use livingbrain_d1_fake::{BindLimited, D1_MAX_BINDS};
use livingbrain_pages::Pages;
use sea_query::Value as SeaValue;
use time::{Duration, OffsetDateTime};

const WS: &str = "T0WORKSPACE";
const SCOPES: usize = 130;

/// The module that owns the compiler's tables in a test composition (the
/// same composition `tests/compiler.rs` builds).
struct QueueTables;

impl Module for QueueTables {
    fn name(&self) -> &'static str {
        "coding-agents"
    }
    fn version(&self) -> &'static str {
        "0"
    }
    fn requires(&self) -> &'static [Port] {
        &[Port::Db]
    }
    fn tables(&self) -> &'static [&'static str] {
        &livingbrain_compiler::TABLES
    }
    fn migrations(&self) -> Migrations {
        const SET: [SqlMigration; 6] = [
            livingbrain_compiler::MIGRATION_INIT,
            livingbrain_compiler::MIGRATION_WAITING_RUNNER,
            livingbrain_compiler::MIGRATION_TRIAGE,
            livingbrain_compiler::MIGRATION_PARALLEL,
            livingbrain_compiler::MIGRATION_REQUEUE_BAD_OUTPUT,
            livingbrain_compiler::MIGRATION_SOURCE_UNITS,
        ];
        Migrations {
            sqlite: &SET,
            postgres: &[],
        }
    }
    fn validate_config(&self, _cfg: &dyn Config) -> Result<(), ConfigError> {
        Ok(())
    }
    fn router(&self, _ctx: ModuleContext) -> cratefield_core::axum::Router {
        cratefield_core::axum::Router::new()
    }
}

fn text(value: &str) -> SeaValue {
    SeaValue::String(Some(Box::new(value.to_owned())))
}

fn int(value: i64) -> SeaValue {
    SeaValue::BigInt(Some(value))
}

fn stamp(seconds: i64) -> String {
    let at = OffsetDateTime::from_unix_timestamp(1_800_000_000).expect("in range")
        + Duration::seconds(seconds);
    at.format(&time::format_description::well_known::Rfc3339)
        .expect("formats")
}

fn scope(i: usize) -> String {
    format!("scope-{i:03}")
}

fn scopes() -> Vec<String> {
    (0..SCOPES).map(scope).collect()
}

/// One job per scope. Statuses rotate through queued, failed, parked and
/// done; triage rotates through skip, summary, full; every timestamp is
/// distinct, so each ordering the queue asks for is a total one.
fn seed_jobs(db: &Arc<dyn Database>) {
    const STATUSES: [&str; 4] = ["queued", "failed", "skipped-no-model", "done"];
    const TRIAGE: [&str; 3] = ["skip", "summary", "full"];
    for i in 0..SCOPES {
        let values = vec![
            text(&scope(i)),
            text(&format!("body-{i:03}")),
            text(&format!("src-{i:03}")),
            text(WS),
            text("u1"),
            text(&format!("claude-code session {i}")),
            SeaValue::String(None),
            text(&stamp(i as i64)),
            text(STATUSES[i % 4]),
            text(&stamp(i as i64)),
            text(&stamp(i as i64)),
            text(TRIAGE[i % 3]),
            int(10 * i as i64),
            int(5 * i as i64),
        ];
        pollster::block_on(db.execute(&Statement::with_values(
            "INSERT INTO coding_agent_jobs (scope, body_sha256, source_id, workspace, owner, \
             session_key, project, source_created_at, status, queued_at, updated_at, triage, \
             est_tokens_raw, est_tokens_sent) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            values,
        )))
        .expect("a job row");
    }
}

fn seed_units(db: &Arc<dyn Database>) {
    for i in 0..SCOPES {
        let values = vec![
            text(&scope(i)),
            text(&format!("unit-{i:03}")),
            text(WS),
            text("u1"),
            text("imports"),
            text("2026-10-01"),
            text("notes"),
            text("queued"),
            text(&stamp(i as i64)),
            text(&stamp(i as i64)),
        ];
        pollster::block_on(db.execute(&Statement::with_values(
            "INSERT INTO source_compile_units (scope, unit_key, workspace, owner, lane, unit_day, \
             label, status, queued_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            values,
        )))
        .expect("a unit row");
    }
}

/// The harness (kept alive for the test) and the bind-limited view of its
/// database. The unlimited view is `kit.db`.
fn kit() -> (TestHarness, Arc<BindLimited>) {
    let kit = TestHarness::new(vec![Box::new(Pages::new()), Box::new(QueueTables)]);
    seed_jobs(&kit.db);
    seed_units(&kit.db);
    let limited = BindLimited::new(kit.db.clone());
    (kit, limited)
}

#[test]
fn the_job_reads_over_many_scopes_stay_within_the_bind_limit() {
    let (kit, limited_db) = kit();
    let limited = Queue::new(limited_db.clone());
    let unlimited = Queue::new(kit.db.clone());
    let scopes = scopes();

    let want = pollster::block_on(unlimited.list(WS, &scopes, None, 50)).expect("unlimited list");
    let got = pollster::block_on(limited.list(WS, &scopes, None, 50))
        .expect("the listing stays within D1's bind ceiling");
    assert_eq!(got, want);
    assert_eq!(want.len(), 50, "the cap is the same as one statement's");

    let want = pollster::block_on(unlimited.counts(WS, &scopes)).expect("unlimited counts");
    let got = pollster::block_on(limited.counts(WS, &scopes)).expect("counts");
    assert_eq!(got, want);

    let want = pollster::block_on(unlimited.savings(WS, &scopes)).expect("unlimited savings");
    let got = pollster::block_on(limited.savings(WS, &scopes)).expect("savings");
    assert_eq!(got, want);
    assert!(want.skipped + want.summarised + want.full == SCOPES as u64);

    assert!(
        limited_db.widest() <= D1_MAX_BINDS,
        "widest statement bound {} values",
        limited_db.widest()
    );
}

#[test]
fn the_fair_claim_over_many_scopes_matches_one_statement() {
    let (kit, limited_db) = kit();
    let limited = Queue::new(limited_db.clone());
    let unlimited = Queue::new(kit.db.clone());
    let now =
        OffsetDateTime::from_unix_timestamp(1_800_000_000).expect("in range") + Duration::hours(1);
    let filter = ClaimFilter {
        scopes: Some(scopes()),
        ..ClaimFilter::default()
    };
    for per in [None, Some(3)] {
        let want = pollster::block_on(unlimited.ready_fair(now, &filter, 40, per))
            .expect("unlimited claim");
        let got = pollster::block_on(limited.ready_fair(now, &filter, 40, per))
            .expect("the claim stays within D1's bind ceiling");
        assert_eq!(got, want, "per_workspace {per:?}");
        assert!(!want.is_empty());
    }
    assert!(
        limited_db.widest() <= D1_MAX_BINDS,
        "widest statement bound {} values",
        limited_db.widest()
    );
}

#[test]
fn the_unit_counts_over_many_scopes_match_one_statement() {
    let (kit, limited_db) = kit();
    let limited = Queue::new(limited_db.clone());
    let unlimited = Queue::new(kit.db.clone());
    let scopes = scopes();
    let want = pollster::block_on(unlimited.unit_counts(WS, &scopes)).expect("unlimited counts");
    let got = pollster::block_on(limited.unit_counts(WS, &scopes))
        .expect("the unit counts stay within D1's bind ceiling");
    assert_eq!(got, want);
    assert_eq!(want["imports"].units.values().sum::<u64>(), SCOPES as u64);
    assert!(
        limited_db.widest() <= D1_MAX_BINDS,
        "widest statement bound {} values",
        limited_db.widest()
    );
}

#[test]
fn the_requeues_over_many_scopes_change_every_matching_row() {
    let (_kit, limited_db) = kit();
    let limited = Queue::new(limited_db.clone());
    let now =
        OffsetDateTime::from_unix_timestamp(1_800_000_000).expect("in range") + Duration::hours(2);
    let scopes = scopes();
    // Statuses rotate through four values, so 33 of the 130 jobs are `failed`
    // (i % 4 == 1) and 32 are `skipped-no-model` (i % 4 == 2).
    let failed = pollster::block_on(limited.requeue_failed(WS, &scopes, now))
        .expect("the failed requeue stays within D1's bind ceiling");
    assert_eq!(failed, 33);
    let parked = pollster::block_on(limited.requeue_parked_no_model_scoped(WS, &scopes, now))
        .expect("the parked requeue stays within D1's bind ceiling");
    assert_eq!(parked, 32);
    let units = pollster::block_on(limited.requeue_parked_units_no_model_scoped(WS, &scopes, now))
        .expect("the unit requeue stays within D1's bind ceiling");
    assert_eq!(units, 0);
    assert!(
        limited_db.widest() <= D1_MAX_BINDS,
        "widest statement bound {} values",
        limited_db.widest()
    );
}
