//! Upgrading a database that already applied migration `0008` (production
//! has) to the forward migration `0009`. `0008` is never edited: the alias
//! columns `state`, `shared` and `set_by` arrive in `0009`, and the alias
//! operations must work on the upgraded table.

use cratefield_adapter_sqlite::SqliteDatabase;
use cratefield_core::{Database, SqlMigration, Statement};
use livingbrain_compiler::subject_aliases::{self, Refused};
use livingbrain_compiler::{
    MIGRATION_DREAMING_SETTINGS, MIGRATION_INIT, MIGRATION_PARALLEL, MIGRATION_REQUEUE_BAD_OUTPUT,
    MIGRATION_SOURCE_UNITS, MIGRATION_SUBJECT_ALIAS_STATE, MIGRATION_SUBJECT_ALIASES,
    MIGRATION_TRIAGE, MIGRATION_WAITING_RUNNER,
};
use sea_query::Value as SeaValue;
use time::OffsetDateTime;

const MODULE: &str = "coding-agents";
const WORKSPACE: &str = "T0WORKSPACE";

/// The coding-agents module's migrations as production applied them at 0008.
fn released() -> Vec<SqlMigration> {
    vec![
        MIGRATION_INIT,
        MIGRATION_WAITING_RUNNER,
        MIGRATION_TRIAGE,
        MIGRATION_PARALLEL,
        MIGRATION_REQUEUE_BAD_OUTPUT,
        MIGRATION_DREAMING_SETTINGS,
        MIGRATION_SOURCE_UNITS,
        MIGRATION_SUBJECT_ALIASES,
    ]
}

/// The same module after the forward migration.
fn upgraded() -> Vec<SqlMigration> {
    let mut set = released();
    set.push(MIGRATION_SUBJECT_ALIAS_STATE);
    set
}

fn text(value: &str) -> SeaValue {
    SeaValue::String(Some(Box::new(value.to_owned())))
}

fn at() -> OffsetDateTime {
    OffsetDateTime::UNIX_EPOCH
}

/// A database with `0008` applied and one alias row written the way the
/// `0008` table takes it (four columns only), as production holds it.
fn released_database() -> SqliteDatabase {
    let db = SqliteDatabase::in_memory().expect("in-memory sqlite");
    db.apply_migrations(MODULE, &released())
        .expect("0008 applies");
    pollster::block_on(db.execute(&Statement::with_values(
        "INSERT INTO fact_subject_aliases (workspace_id, alias, canonical, updated_at) \
         VALUES (?, ?, ?, ?)",
        vec![
            text(WORKSPACE),
            text("project/world369"),
            text("project/earthos"),
            text("1970-01-01T00:00:00Z"),
        ],
    )))
    .expect("a row in the released table");
    db
}

#[test]
fn an_alias_row_written_under_0008_reads_as_set_after_0009() {
    let db = released_database();
    db.apply_migrations(MODULE, &upgraded())
        .expect("0009 applies over the applied 0008");

    let rows = pollster::block_on(subject_aliases::rows(&db, WORKSPACE)).expect("rows");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].alias, "project/world369");
    assert_eq!(rows[0].canonical, "project/earthos");
    // The defaults 0009 gives a row that existed before it: in force, no
    // evidence, and no recorded setter.
    assert_eq!(rows[0].state, "set");
    assert_eq!(rows[0].shared, 0);
    assert_eq!(rows[0].set_by, "");

    let in_force = pollster::block_on(subject_aliases::load(&db, WORKSPACE)).expect("load");
    assert_eq!(
        in_force,
        vec![("project/world369".to_owned(), "project/earthos".to_owned())]
    );
}

#[test]
fn the_alias_operations_work_on_the_upgraded_table() {
    let db = released_database();
    db.apply_migrations(MODULE, &upgraded())
        .expect("0009 applies over the applied 0008");
    let db: &dyn Database = &db;

    // A proposal is stored as proposed, with its evidence and no setter.
    let stored = pollster::block_on(subject_aliases::propose(
        db,
        WORKSPACE,
        "project/aura",
        "project/earthos",
        4,
        at(),
    ))
    .expect("a proposal");
    assert!(stored);
    let proposed = pollster::block_on(subject_aliases::rows(db, WORKSPACE))
        .expect("rows")
        .into_iter()
        .find(|row| row.alias == "project/aura")
        .expect("the proposal row");
    assert_eq!(proposed.state, "proposed");
    assert_eq!(proposed.shared, 4);
    assert_eq!(proposed.set_by, "");
    // Not in force until an owner accepts it.
    assert!(
        !pollster::block_on(subject_aliases::load(db, WORKSPACE))
            .expect("load")
            .iter()
            .any(|(alias, _)| alias == "project/aura")
    );

    pollster::block_on(subject_aliases::accept(
        db,
        WORKSPACE,
        "project/aura",
        "owner-1",
        at(),
    ))
    .expect("an owner accepts it");
    let accepted = pollster::block_on(subject_aliases::rows(db, WORKSPACE))
        .expect("rows")
        .into_iter()
        .find(|row| row.alias == "project/aura")
        .expect("the accepted row");
    assert_eq!(accepted.state, "set");
    assert_eq!(accepted.set_by, "owner-1");

    // An owner's removal leaves a dismissed row, so the pair is not proposed again.
    assert!(
        pollster::block_on(subject_aliases::remove(
            db,
            WORKSPACE,
            "project/world369",
            "owner-1",
            at()
        ))
        .expect("remove")
    );
    let dismissed = pollster::block_on(subject_aliases::rows(db, WORKSPACE))
        .expect("rows")
        .into_iter()
        .find(|row| row.alias == "project/world369")
        .expect("the dismissed row");
    assert_eq!(dismissed.state, "dismissed");
    assert_eq!(dismissed.set_by, "owner-1");

    // The one-hop rule still holds on the upgraded table.
    assert_eq!(
        pollster::block_on(subject_aliases::set(
            db,
            WORKSPACE,
            "project/other",
            "project/aura",
            "owner-1",
            at()
        )),
        Err(Refused::CanonicalIsAlias)
    );
}
