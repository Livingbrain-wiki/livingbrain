//! The kit the audit tests share: a harness with the module mounted, and the
//! two raw-query helpers that let a test read a row back as the *database*
//! stored it rather than as the store's typed shape round-tripped it.
//!
//! Reading the raw row is the point of half of these tests: "the value is
//! redacted" and "the clear is not a NULL" are claims about what is on disk.

#![allow(dead_code)]

use std::sync::Arc;

use cratefield_core::{Clock, Row, Statement, UlidIdGen};
use cratefield_testing::TestHarness;
use livingbrain_audit::{Audit, AuditStore};
use sea_query::Value as SeaValue;

/// A store over a freshly migrated in-memory database, plus the harness it
/// came from so a test can read the raw rows. The kit also builds and
/// validates the module's harness, so a broken migration or an undeclared
/// table fails here rather than silently passing.
pub fn store() -> (TestHarness, AuditStore) {
    let kit = TestHarness::new(vec![Box::new(Audit::new())]);
    let clock: Arc<dyn Clock> = Arc::new(kit.clock.clone());
    let store = AuditStore::new(kit.db.clone(), clock, Arc::new(UlidIdGen));
    (kit, store)
}

/// The rows one statement returns, for reading a row as the database stored
/// it. `# Panics` when the read fails, which is what a failing assertion
/// setup should say.
pub async fn rows(kit: &TestHarness, sql: &str, values: Vec<SeaValue>) -> Vec<Row> {
    kit.db
        .query(&Statement::with_values(sql, values))
        .await
        .unwrap_or_else(|error| panic!("{sql} reads: {error}"))
        .rows
}

/// Every `settings_audit` row, rendered as one string per row, for an
/// assertion that reads "the raw token is not anywhere in this table".
pub async fn rendered(kit: &TestHarness) -> String {
    rows(kit, "SELECT * FROM settings_audit", Vec::new())
        .await
        .iter()
        .map(|row| format!("{row:?}"))
        .collect::<Vec<_>>()
        .join("\n")
}
