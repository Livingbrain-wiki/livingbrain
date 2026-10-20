//! App memories (issue #146) against a real migrated SQLite: sealing at
//! rest, scope isolation, hard deletion, the scope key surviving a "forget
//! all", a shred taking everything, and a key rotation moving the rows.

mod common;

use std::sync::Arc;

use cratefield_core::{Clock, Database, Statement, SystemClock, UlidIdGen};
use cratefield_testing::{MemoryBlob, TestHarness};
use livingbrain_pages::{
    MAX_MEMORIES, MemoryError, MemoryStore, PageError, PageStore, Pages, PublicStore, Publication,
    app_scopes,
};

const WS: &str = "ws-one";
const ALICE: &str = "user-aaaaaaaaaaaaaaaa";
const BOB: &str = "user-bbbbbbbbbbbbbbbb";

struct Kit {
    memories: MemoryStore,
    pages: PageStore,
    db: Arc<dyn Database>,
}

fn kit() -> Kit {
    let harness = TestHarness::new(vec![Box::new(Pages::new())]);
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let kms = common::kms();
    Kit {
        memories: MemoryStore::new(
            harness.db.clone(),
            kms.clone(),
            clock.clone(),
            Arc::new(UlidIdGen),
        ),
        pages: PageStore::new(
            harness.db.clone(),
            Arc::new(MemoryBlob::new()),
            kms,
            clock,
            Arc::new(UlidIdGen),
        ),
        db: harness.db.clone(),
    }
}

fn note(kit: &Kit, scope: &str, text: &str) -> String {
    pollster::block_on(kit.memories.note(WS, "earthos", scope, text, "conv-1")).expect("a note")
}

fn rows(kit: &Kit, scope: &str) -> i64 {
    let result = pollster::block_on(kit.db.query(&Statement::with_values(
        "SELECT COUNT(*) AS n FROM app_memories WHERE scope = ?",
        vec![scope.into()],
    )))
    .expect("a count");
    result
        .first()
        .and_then(|row| row.get::<i64>("n"))
        .unwrap_or(-1)
}

#[test]
fn a_memory_round_trips_and_is_sealed_at_rest() {
    let kit = kit();
    let id = note(&kit, ALICE, "Alice is vegetarian");
    assert!(id.starts_with("mem_"));
    let got = pollster::block_on(kit.memories.recall(ALICE, 20)).unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].text, "Alice is vegetarian");
    assert_eq!(got[0].source, "conv-1");
    assert_eq!(got[0].id, id);

    // The row holds an envelope, not the words.
    let sealed = pollster::block_on(kit.db.query(&Statement::with_values(
        "SELECT sealed FROM app_memories WHERE scope = ?",
        vec![ALICE.into()],
    )))
    .unwrap();
    let sealed = sealed.first().unwrap().get::<String>("sealed").unwrap();
    assert!(!sealed.contains("vegetarian"));
}

#[test]
fn a_scope_reads_only_its_own_memories() {
    let kit = kit();
    note(&kit, ALICE, "alice secret");
    note(&kit, BOB, "bob secret");
    let alice = pollster::block_on(kit.memories.recall(ALICE, 20)).unwrap();
    assert_eq!(alice.len(), 1);
    assert_eq!(alice[0].text, "alice secret");
    // A scope nobody wrote to holds nothing — and has no key made for it.
    assert!(
        pollster::block_on(kit.memories.recall("user-cccccccccccccccc", 20))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn recall_is_newest_first_and_capped() {
    let kit = kit();
    for n in 0..25 {
        note(&kit, ALICE, &format!("memory {n:02}"));
    }
    let got = pollster::block_on(kit.memories.recall(ALICE, 20)).unwrap();
    assert_eq!(got.len(), 20);
    assert_eq!(got[0].text, "memory 24");
}

#[test]
fn forget_deletes_one_memory_for_good_and_only_in_its_scope() {
    let kit = kit();
    let keep = note(&kit, ALICE, "keep me");
    let drop = note(&kit, ALICE, "drop me");
    let bobs = note(&kit, BOB, "bob's");
    // Another scope's id deletes nothing.
    assert_eq!(
        pollster::block_on(kit.memories.forget(ALICE, &bobs)).unwrap(),
        0
    );
    assert_eq!(
        pollster::block_on(kit.memories.forget(ALICE, &drop)).unwrap(),
        1
    );
    assert_eq!(
        pollster::block_on(kit.memories.forget(ALICE, &drop)).unwrap(),
        0
    );
    let got = pollster::block_on(kit.memories.recall(ALICE, 20)).unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].id, keep);
    assert_eq!(rows(&kit, BOB), 1);
}

#[test]
fn a_malformed_id_is_refused_not_matched() {
    let kit = kit();
    assert!(matches!(
        pollster::block_on(kit.memories.forget(ALICE, "mem_%")),
        Err(MemoryError::Invalid(_))
    ));
    assert!(matches!(
        pollster::block_on(kit.memories.forget(ALICE, "x' OR '1'='1")),
        Err(MemoryError::Invalid(_))
    ));
}

#[test]
fn forget_all_erases_the_scope_and_leaves_it_writable() {
    let kit = kit();
    note(&kit, ALICE, "one");
    note(&kit, ALICE, "two");
    note(&kit, BOB, "bobs");
    assert_eq!(
        pollster::block_on(kit.memories.forget_all(ALICE)).unwrap(),
        2
    );
    assert_eq!(rows(&kit, ALICE), 0);
    assert!(
        pollster::block_on(kit.memories.recall(ALICE, 20))
            .unwrap()
            .is_empty()
    );
    assert_eq!(rows(&kit, BOB), 1);
    // Not a shred: the member can start again.
    note(&kit, ALICE, "fresh start");
    assert_eq!(
        pollster::block_on(kit.memories.recall(ALICE, 20))
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn limits_are_enforced() {
    let kit = kit();
    let empty = pollster::block_on(kit.memories.note(WS, "earthos", ALICE, "  ", ""));
    assert!(matches!(empty, Err(MemoryError::Invalid(_))));
    let long = "x".repeat(2_001);
    assert!(matches!(
        pollster::block_on(kit.memories.note(WS, "earthos", ALICE, &long, "")),
        Err(MemoryError::Invalid(_))
    ));
    let source = "s".repeat(201);
    assert!(matches!(
        pollster::block_on(kit.memories.note(WS, "earthos", ALICE, "ok", &source)),
        Err(MemoryError::Invalid(_))
    ));
    for n in 0..MAX_MEMORIES {
        pollster::block_on(kit.memories.note(WS, "earthos", BOB, &format!("m{n}"), "")).unwrap();
    }
    assert!(matches!(
        pollster::block_on(kit.memories.note(WS, "earthos", BOB, "one more", "")),
        Err(MemoryError::Full)
    ));
}

#[test]
fn forgetting_the_scope_shreds_the_key_and_every_row() {
    let kit = kit();
    note(&kit, ALICE, "gone with the key");
    pollster::block_on(kit.pages.forget_scope(ALICE)).unwrap();
    assert_eq!(rows(&kit, ALICE), 0);
    // A shredded scope cannot be written again — the workspace erasure.
    assert!(matches!(
        pollster::block_on(kit.memories.note(WS, "earthos", ALICE, "again", "")),
        Err(MemoryError::Store(PageError::Shredded(_)))
    ));
}

#[test]
fn a_key_rotation_moves_the_memories_and_they_still_open() {
    let kit = kit();
    note(&kit, ALICE, "before the rotation");
    pollster::block_on(kit.pages.rotate_scope_key(ALICE)).unwrap();
    note(&kit, ALICE, "after the rotation");
    let report = pollster::block_on(kit.pages.reencrypt_scope(ALICE)).unwrap();
    assert!(report.rewritten >= 1);
    let got = pollster::block_on(kit.memories.recall(ALICE, 20)).unwrap();
    assert_eq!(got.len(), 2);
    assert!(got.iter().any(|m| m.text == "before the rotation"));
    // And nothing is left on the old key version.
    let old = pollster::block_on(kit.db.query(&Statement::with_values(
        "SELECT COUNT(*) AS n FROM app_memories WHERE scope = ? AND key_version = 1",
        vec![ALICE.into()],
    )))
    .unwrap();
    assert_eq!(old.first().and_then(|row| row.get::<i64>("n")), Some(0));
}

#[test]
fn a_workspace_finds_the_scopes_its_apps_and_publications_hold() {
    let kit = kit();
    note(&kit, ALICE, "a");
    let public = PublicStore::new(kit.db.clone(), Arc::new(SystemClock));
    pollster::block_on(public.publish(
        WS,
        &Publication {
            scope: "public-0123456789abcdef".to_owned(),
            slug: "refunds".to_owned(),
            title: "Refunds".to_owned(),
            url: "https://example.com/refunds".to_owned(),
            source_scope: "shared-x".to_owned(),
            source_version: 1,
            published_by: "owner".to_owned(),
            published_at: String::new(),
        },
    ))
    .unwrap();
    let mut scopes = pollster::block_on(app_scopes(&*kit.db, WS)).unwrap();
    scopes.sort();
    assert_eq!(
        scopes,
        vec!["public-0123456789abcdef".to_owned(), ALICE.to_owned()]
    );
    assert!(
        pollster::block_on(app_scopes(&*kit.db, "ws-two"))
            .unwrap()
            .is_empty()
    );
    // And a shred of the public scope drops its ledger.
    pollster::block_on(kit.pages.forget_scope("public-0123456789abcdef")).unwrap();
    assert!(
        pollster::block_on(public.list("public-0123456789abcdef"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_publication_is_withdrawn_by_deleting_its_row() {
    let kit = kit();
    let public = PublicStore::new(kit.db.clone(), Arc::new(SystemClock));
    let publication = Publication {
        scope: "public-0123456789abcdef".to_owned(),
        slug: "refunds".to_owned(),
        title: "Refunds".to_owned(),
        url: "https://example.com/refunds".to_owned(),
        source_scope: "shared-x".to_owned(),
        source_version: 1,
        published_by: "owner".to_owned(),
        published_at: String::new(),
    };
    pollster::block_on(public.publish(WS, &publication)).unwrap();
    // Publishing again refreshes the row rather than adding one.
    pollster::block_on(public.publish(WS, &publication)).unwrap();
    assert_eq!(
        pollster::block_on(public.list(&publication.scope))
            .unwrap()
            .len(),
        1
    );
    assert!(pollster::block_on(public.unpublish(&publication.scope, "refunds")).unwrap());
    assert!(!pollster::block_on(public.unpublish(&publication.scope, "refunds")).unwrap());
}
