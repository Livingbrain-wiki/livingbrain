//! Page titles on the head row (migration 0007), the listing's filters, and
//! the in-place move of session logs from `decision` to `session`.

mod common;

use std::sync::Arc;

use cratefield_core::{Blob, Clock, Database, Statement, UlidIdGen};
use cratefield_testing::{FixedClock, MemoryBlob, TestHarness};
use livingbrain_pages::{Author, EntityType, PageFilter, PageStore, PageWrite, Pages, project_key};
use sea_query::Value as SeaValue;

fn bench() -> (TestHarness, Arc<dyn Blob>, PageStore) {
    let kit = TestHarness::new(vec![Box::new(Pages::new())]);
    let blob: Arc<dyn Blob> = Arc::new(MemoryBlob::new());
    let clock: Arc<dyn Clock> = Arc::new(kit.clock.clone());
    let store = PageStore::new(
        kit.db.clone(),
        blob.clone(),
        common::kms(),
        clock,
        Arc::new(UlidIdGen),
    );
    (kit, blob, store)
}

fn write(entity_type: EntityType, markdown: &str) -> PageWrite {
    PageWrite {
        entity_type,
        markdown: markdown.to_owned(),
        author: Author::Brain { reconciles: None },
        base_version: None,
    }
}

fn text(value: &str) -> SeaValue {
    SeaValue::String(Some(Box::new(value.to_owned())))
}

async fn column(db: &Arc<dyn Database>, slug: &str) -> (Option<String>, Option<u32>) {
    let rows = db
        .query(&Statement::with_values(
            "SELECT title_sealed, title_key_version FROM pages WHERE scope = ? AND slug = ?",
            vec![text("team"), text(slug)],
        ))
        .await
        .unwrap();
    let row = rows.first().expect("the row");
    (
        row.get::<String>("title_sealed"),
        row.get::<u32>("title_key_version"),
    )
}

/// The pages a session-heavy scope holds: two sessions, two compiled pages
/// of one project, a person.
async fn seed(store: &PageStore) {
    for (slug, kind, markdown) in [
        (
            "agent-log-colonizer-a15fba01",
            EntityType::Session,
            "---\ntitle: EarthOS.world · Colony a15fba01 · #123 Fix auth redirect\nproject: EarthOS.world\n---\n\nbody\n",
        ),
        (
            "agent-log-codex-77",
            EntityType::Session,
            "---\ntitle: EarthOS.world · Codex · 2026-10-15\nproject: EarthOS.world\n---\n\nbody\n",
        ),
        (
            "coding-agents--codex--earthos-world--learnings",
            EntityType::Topic,
            "---\ntitle: codex / earthos-world: learnings\nproject: earthos-world\n---\n\nbody\n",
        ),
        (
            "coding-agents--projects--earthos-world",
            EntityType::Topic,
            "---\ntitle: Project earthos-world: all agents\nproject: earthos-world\n---\n\nbody\n",
        ),
        (
            "coding-agents--codex--livingbrain--index",
            EntityType::Topic,
            "---\ntitle: codex / livingbrain: index\n---\n\nbody\n",
        ),
        (
            "ada",
            EntityType::Person,
            "---\nname: Ada Lovelace\n---\n\nbody\n",
        ),
    ] {
        store
            .write("team", slug, write(kind, markdown))
            .await
            .unwrap();
    }
}

#[test]
fn a_listing_names_every_page_without_opening_a_body() {
    pollster::block_on(async {
        let (kit, _blob, store) = bench();
        seed(&store).await;
        let listed = store.list(&["team"], 50).await.unwrap();
        let title = |slug: &str| {
            listed
                .iter()
                .find(|page| page.slug == slug)
                .and_then(|page| page.title.clone())
        };
        assert_eq!(
            title("agent-log-colonizer-a15fba01").as_deref(),
            Some("EarthOS.world · Colony a15fba01 · #123 Fix auth redirect")
        );
        assert_eq!(title("ada").as_deref(), Some("Ada Lovelace"));

        // The title is sealed: the column never holds the words.
        let (sealed, version) = column(&kit.db, "ada").await;
        assert_eq!(version, Some(1));
        assert!(!sealed.unwrap().contains("Ada"));
    });
}

#[test]
fn a_listing_can_leave_the_sessions_out_or_ask_for_only_them() {
    pollster::block_on(async {
        let (_kit, _blob, store) = bench();
        seed(&store).await;
        let without = store
            .list_filtered(
                &["team"],
                &PageFilter {
                    exclude: vec![EntityType::Session],
                    ..PageFilter::default()
                },
                50,
            )
            .await
            .unwrap();
        assert_eq!(without.len(), 4);
        assert!(
            without
                .iter()
                .all(|page| page.entity_type != EntityType::Session)
        );

        // The limit is spent on what the filter keeps, not on the sessions.
        let two = store
            .list_filtered(
                &["team"],
                &PageFilter {
                    exclude: vec![EntityType::Session],
                    ..PageFilter::default()
                },
                2,
            )
            .await
            .unwrap();
        assert_eq!(two.len(), 2);

        let only = store
            .list_filtered(
                &["team"],
                &PageFilter {
                    types: vec![EntityType::Session],
                    ..PageFilter::default()
                },
                50,
            )
            .await
            .unwrap();
        assert_eq!(only.len(), 2);
    });
}

#[test]
fn a_project_key_finds_its_compiled_pages_under_every_agent() {
    pollster::block_on(async {
        let (_kit, _blob, store) = bench();
        seed(&store).await;
        let key = project_key("EarthOS.world").unwrap();
        let mut slugs: Vec<String> = store
            .list_filtered(
                &["team"],
                &PageFilter {
                    project: Some(key),
                    ..PageFilter::default()
                },
                50,
            )
            .await
            .unwrap()
            .into_iter()
            .map(|page| page.slug)
            .collect();
        slugs.sort();
        assert_eq!(
            slugs,
            vec![
                "coding-agents--codex--earthos-world--learnings",
                "coding-agents--projects--earthos-world",
            ]
        );
        // A key with a LIKE wildcard in it is not a key: it matches nothing.
        let none = store
            .list_filtered(
                &["team"],
                &PageFilter {
                    project: Some("%".to_owned()),
                    ..PageFilter::default()
                },
                50,
            )
            .await
            .unwrap();
        assert!(none.is_empty());
    });
}

#[test]
fn the_backfill_candidates_are_untitled_rows_and_decision_filed_sessions() {
    pollster::block_on(async {
        let (kit, _blob, store) = bench();
        // A session log written as a decision, the way production has them.
        store
            .write(
                "team",
                "agent-log-colonizer-b2",
                write(
                    EntityType::Decision,
                    "---\ntitle: colonizer session b2\nproject: EarthOS.world\n---\n\nbody\n",
                ),
            )
            .await
            .unwrap();
        store
            .write(
                "team",
                "ada",
                write(EntityType::Person, "---\nname: Ada\n---\n\nx\n"),
            )
            .await
            .unwrap();
        // Both predate the column: no title yet.
        kit.db
            .execute(&Statement::with_values(
                "UPDATE pages SET title_sealed = NULL, title_key_version = NULL",
                vec![],
            ))
            .await
            .unwrap();
        let mut found: Vec<String> = store
            .title_candidates(10)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.slug)
            .collect();
        found.sort();
        assert_eq!(found, vec!["ada", "agent-log-colonizer-b2"]);

        // Retyping moves the row in place, once.
        assert!(
            store
                .retype(
                    "team",
                    "agent-log-colonizer-b2",
                    EntityType::Decision,
                    EntityType::Session
                )
                .await
                .unwrap()
        );
        assert!(
            !store
                .retype(
                    "team",
                    "agent-log-colonizer-b2",
                    EntityType::Decision,
                    EntityType::Session
                )
                .await
                .unwrap()
        );
        let page = store
            .read("team", "agent-log-colonizer-b2")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(page.entity_type, EntityType::Session);
        assert_eq!(page.version, 1, "a retype writes no version");

        store.set_title("team", "ada", Some("Ada")).await.unwrap();
        store
            .set_title(
                "team",
                "agent-log-colonizer-b2",
                Some("colonizer session b2"),
            )
            .await
            .unwrap();
        assert!(store.title_candidates(10).await.unwrap().is_empty());

        // A page with nothing to show is marked handled, not retried.
        store.set_title("team", "ada", None).await.unwrap();
        assert_eq!(column(&kit.db, "ada").await, (Some(String::new()), Some(0)));
        assert!(store.title_candidates(10).await.unwrap().is_empty());
    });
}

#[test]
fn a_title_survives_a_key_rotation_and_moves_to_the_new_key() {
    pollster::block_on(async {
        let (kit, blob, store) = bench();
        seed(&store).await;
        assert_eq!(store.rotate_scope_key("team").await.unwrap(), 2);
        store.reencrypt_scope("team").await.unwrap();
        assert_eq!(column(&kit.db, "ada").await.1, Some(2));
        // Past the grace window the old key is retired: no title still
        // names it.
        let later = PageStore::new(
            kit.db.clone(),
            blob,
            common::kms(),
            Arc::new(FixedClock(kit.clock.0 + time::Duration::minutes(30))),
            Arc::new(UlidIdGen),
        );
        let report = later.reencrypt_scope("team").await.unwrap();
        assert_eq!(report.retired, vec![1]);
        let listed = later.list(&["team"], 50).await.unwrap();
        let ada = listed.iter().find(|page| page.slug == "ada").unwrap();
        assert_eq!(ada.title.as_deref(), Some("Ada Lovelace"));
    });
}

#[test]
fn a_forgotten_scope_lists_no_titles() {
    pollster::block_on(async {
        let (_kit, _blob, store) = bench();
        seed(&store).await;
        store.forget_scope("team").await.unwrap();
        let listed = store.list(&["team"], 50).await.unwrap();
        assert!(!listed.is_empty());
        assert!(listed.iter().all(|page| page.title.is_none()));
    });
}

/// One project, two spellings: a repository's `EarthOS.world` and a local
/// clone's `earthos-world`. The filter finds both under either spelling,
/// and the facet lists the project once, by the repository's name.
#[test]
fn a_project_filter_matches_every_spelling_of_the_project() {
    for kit in TestHarness::all_dialects(|| vec![Box::new(Pages::new())]) {
        let dialect = kit.dialect;
        let store = livingbrain_pages::SourceStore::new(
            kit.db.clone(),
            Arc::new(MemoryBlob::new()),
            common::kms(),
            Arc::new(kit.clock.clone()),
            Arc::new(UlidIdGen),
        );
        pollster::block_on(async {
            for (body, project) in [
                ("one", "EarthOS.world"),
                ("two", "earthos-world"),
                ("three", "livingbrain"),
            ] {
                store
                    .put(
                        "user-a",
                        livingbrain_pages::SourceWrite {
                            kind: livingbrain_pages::SourceKind::AgentLog,
                            workspace: "ws".to_owned(),
                            origin_ref: None,
                            author: None,
                            rel_path: format!("codex session {body}"),
                            markdown: body.to_owned(),
                            imported_by: "u-a".to_owned(),
                            held: false,
                            project: Some(project.to_owned()),
                        },
                    )
                    .await
                    .unwrap();
            }
            for spelling in ["earthos-world", "EarthOS.world", "EARTHOS.WORLD"] {
                let found = store
                    .list(
                        "ws",
                        &["user-a"],
                        &livingbrain_pages::SourceFilter {
                            project: Some(spelling.to_owned()),
                            ..livingbrain_pages::SourceFilter::default()
                        },
                        10,
                    )
                    .await
                    .unwrap();
                assert_eq!(found.len(), 2, "{dialect}: {spelling}");
            }
            let projects = store.projects("ws", &["user-a"], None).await.unwrap();
            assert_eq!(
                projects
                    .iter()
                    .map(|p| (p.project.as_str(), p.key.as_str(), p.sources))
                    .collect::<Vec<_>>(),
                vec![
                    ("EarthOS.world", "earthos-world", 2),
                    ("livingbrain", "livingbrain", 1)
                ],
                "{dialect}"
            );
        });
    }
}
