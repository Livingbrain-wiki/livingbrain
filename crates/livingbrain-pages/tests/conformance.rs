//! The shared Cratefield conformance suite for `livingbrain-pages`.
//!
//! `conformance` runs once per dialect the environment provides: SQLite
//! always, and Postgres too when `FZ_TEST_POSTGRES_URL` names a server — which
//! is exactly the parity job in `.github/workflows/ci.yml`. One test
//! definition, both engines.
//!
//! The source ledger's listing queries (`SourceStore::list` and
//! `SourceStore::projects`, behind `GET /v1/sources`) are SQL this module
//! writes by hand — an `IN` list, a `GROUP BY`, a `COUNT(*)` read back as an
//! integer, a keyset cursor — so they run here too, on every dialect, rather
//! than only on the SQLite the other test binaries use.

mod common;

use std::sync::Arc;

use cratefield_core::UlidIdGen;
use cratefield_testing::{FixedClock, MemoryBlob, TestHarness, assert_wasm_safe_deps, conformance};
use livingbrain_pages::{
    Author, EntityType, PageStore, PageWrite, Pages, ProjectCount, SearchHit, SourceError,
    SourceFilter, SourceKind, SourceStore, SourceWrite,
};

#[test]
fn pages_conforms() {
    conformance(Box::new(Pages::new()));
}

#[test]
fn pages_deps_are_wasm_safe() {
    assert_wasm_safe_deps(env!("CARGO_PKG_NAME"));
}

/// Two workspaces; in the first, two members' personal scopes and the
/// shared one; in the second, a shared scope only.
const WS_ONE: &str = "ws-one";
const WS_TWO: &str = "ws-two";
const A_SCOPE: &str = "user-a";
const B_SCOPE: &str = "user-b";
const SHARED_ONE: &str = "shared-one";
const SHARED_TWO: &str = "shared-two";

fn write(workspace: &str, kind: SourceKind, body: &str, project: Option<&str>) -> SourceWrite {
    SourceWrite {
        kind,
        workspace: workspace.to_owned(),
        origin_ref: None,
        author: None,
        rel_path: if kind == SourceKind::Import {
            "notes/one.md".to_owned()
        } else {
            format!("claude-code session {body}")
        },
        markdown: body.to_owned(),
        imported_by: "u-a".to_owned(),
        held: false,
        project: project.map(str::to_owned),
    }
}

/// Every listing clause, on every dialect: the workspace and scope clauses
/// that keep one member's sources from another's, the kind and project
/// filters, the cursor, the projects facet, and the re-upload that files a
/// row under a project without making a second one.
#[test]
fn the_source_listing_holds_on_every_dialect() {
    for kit in TestHarness::all_dialects(|| vec![Box::new(Pages::new())]) {
        let dialect = kit.dialect;
        let store = SourceStore::new(
            kit.db.clone(),
            Arc::new(MemoryBlob::new()),
            common::kms(),
            Arc::new(kit.clock.clone()),
            Arc::new(UlidIdGen),
        );
        pollster::block_on(async {
            let put = |scope: &'static str, write: SourceWrite| {
                let store = &store;
                async move { store.put(scope, write).await.expect("a put").0 }
            };
            let a_demo = put(
                A_SCOPE,
                write(WS_ONE, SourceKind::AgentLog, "a1", Some("demo")),
            )
            .await;
            let a_other = put(
                A_SCOPE,
                write(WS_ONE, SourceKind::AgentLog, "a2", Some("other")),
            )
            .await;
            let a_import = put(A_SCOPE, write(WS_ONE, SourceKind::Import, "a3", None)).await;
            let shared = put(
                SHARED_ONE,
                write(WS_ONE, SourceKind::AgentLog, "s1", Some("demo")),
            )
            .await;
            // Another member's personal source, and another workspace's
            // shared one — under the same project name, which is the point.
            let _b = put(
                B_SCOPE,
                write(WS_ONE, SourceKind::AgentLog, "b1", Some("demo")),
            )
            .await;
            let _b2 = put(
                B_SCOPE,
                write(WS_ONE, SourceKind::AgentLog, "b2", Some("b-only")),
            )
            .await;
            let _two = put(
                SHARED_TWO,
                write(WS_TWO, SourceKind::AgentLog, "w1", Some("demo")),
            )
            .await;

            let mine = [A_SCOPE, SHARED_ONE];
            let all = store
                .list(WS_ONE, &mine, &SourceFilter::default(), 50)
                .await
                .expect("a listing");
            let mut expected = vec![
                a_demo.id.clone(),
                a_other.id.clone(),
                a_import.id.clone(),
                shared.id.clone(),
            ];
            expected.sort();
            expected.reverse();
            let ids: Vec<String> = all.iter().map(|row| row.id.clone()).collect();
            assert_eq!(
                ids, expected,
                "{dialect}: the asker's four, newest (id) first"
            );
            let a_row = all.iter().find(|row| row.id == a_demo.id).expect("a1");
            assert_eq!(a_row.project.as_deref(), Some("demo"), "{dialect}");
            assert_eq!(a_row.kind, SourceKind::AgentLog, "{dialect}");
            assert_eq!(a_row.scope, A_SCOPE, "{dialect}");

            // The workspace clause holds even when a scope name matches.
            let crossed = store
                .list(WS_TWO, &mine, &SourceFilter::default(), 50)
                .await
                .expect("a listing");
            assert!(crossed.is_empty(), "{dialect}: {crossed:?}");
            assert!(
                store
                    .list(WS_ONE, &[], &SourceFilter::default(), 50)
                    .await
                    .expect("a listing")
                    .is_empty(),
                "{dialect}: no scopes, no rows"
            );

            let demo = store
                .list(
                    WS_ONE,
                    &mine,
                    &SourceFilter {
                        project: Some("demo".to_owned()),
                        ..SourceFilter::default()
                    },
                    50,
                )
                .await
                .expect("a listing");
            let mut demo_ids: Vec<String> = demo.iter().map(|row| row.id.clone()).collect();
            demo_ids.sort();
            let mut want = vec![a_demo.id.clone(), shared.id.clone()];
            want.sort();
            assert_eq!(demo_ids, want, "{dialect}: only my and the shared `demo`");

            let imports = store
                .list(
                    WS_ONE,
                    &mine,
                    &SourceFilter {
                        kind: Some(SourceKind::Import),
                        ..SourceFilter::default()
                    },
                    50,
                )
                .await
                .expect("a listing");
            assert_eq!(imports.len(), 1, "{dialect}: {imports:?}");
            assert_eq!(imports[0].id, a_import.id, "{dialect}");

            // Two pages of two, then nothing: every row once, none twice.
            let mut paged = Vec::new();
            let mut before = None;
            loop {
                let page = store
                    .list(
                        WS_ONE,
                        &mine,
                        &SourceFilter {
                            before: before.clone(),
                            ..SourceFilter::default()
                        },
                        2,
                    )
                    .await
                    .expect("a page");
                if page.is_empty() {
                    break;
                }
                before = page.last().map(|row| row.id.clone());
                paged.extend(page.into_iter().map(|row| row.id));
                assert!(paged.len() <= 4, "{dialect}: the cursor never ends");
            }
            assert_eq!(paged, ids, "{dialect}: the pages are the listing, in order");

            let projects = store
                .projects(WS_ONE, &mine, None)
                .await
                .expect("the projects");
            assert_eq!(
                projects,
                vec![
                    ProjectCount {
                        project: "demo".to_owned(),
                        sources: 2,
                    },
                    ProjectCount {
                        project: "other".to_owned(),
                        sources: 1,
                    },
                ],
                "{dialect}: `b-only` is another member's, ws-two's `demo` another workspace's"
            );
            let import_projects = store
                .projects(WS_ONE, &mine, Some(SourceKind::Import))
                .await
                .expect("the projects");
            assert!(import_projects.is_empty(), "{dialect}: {import_projects:?}");

            // A re-upload of the unfiled body names a project: same row, now
            // filed. A later one naming another project changes nothing.
            let (filed, created) = store
                .put(
                    A_SCOPE,
                    write(WS_ONE, SourceKind::Import, "a3", Some("late")),
                )
                .await
                .expect("a re-upload");
            assert!(!created, "{dialect}: the body is the identity");
            assert_eq!(filed.id, a_import.id, "{dialect}");
            assert_eq!(filed.project.as_deref(), Some("late"), "{dialect}");
            let (kept, _) = store
                .put(
                    A_SCOPE,
                    write(WS_ONE, SourceKind::Import, "a3", Some("later")),
                )
                .await
                .expect("a re-upload");
            assert_eq!(kept.project.as_deref(), Some("late"), "{dialect}");
            let (unnamed, _) = store
                .put(A_SCOPE, write(WS_ONE, SourceKind::Import, "a3", None))
                .await
                .expect("a re-upload");
            assert_eq!(unnamed.project.as_deref(), Some("late"), "{dialect}");
            assert_eq!(
                store
                    .list(WS_ONE, &mine, &SourceFilter::default(), 50)
                    .await
                    .expect("a listing")
                    .len(),
                4,
                "{dialect}: still four rows"
            );

            // The project rule is the store's too, not only the route's.
            let refused = store
                .put(
                    A_SCOPE,
                    write(WS_ONE, SourceKind::AgentLog, "a9", Some("../etc")),
                )
                .await;
            assert!(
                matches!(refused, Err(SourceError::InvalidProject(_))),
                "{dialect}: {refused:?}"
            );
        });
    }
}

/// The fact store's hand-written SQL — `SELECT *` decoded by column name, an
/// `IN` list, optional filters, `LIMIT ?`, and the settle pass's batched
/// updates — on every dialect: a supersession, a retraction and the scope
/// clause.
#[test]
fn the_fact_store_holds_on_every_dialect() {
    use livingbrain_memory::timeline::FactStatus;
    use livingbrain_pages::{FactInput, FactStore};

    for kit in TestHarness::all_dialects(|| vec![Box::new(Pages::new())]) {
        let dialect = kit.dialect;
        let store = FactStore::new(
            kit.db.clone(),
            common::kms(),
            Arc::new(kit.clock.clone()),
            Arc::new(UlidIdGen),
        );
        pollster::block_on(async {
            let shoe = |value: &str, from: &str| FactInput {
                subject: "person/nick".to_owned(),
                attribute: "shoe_size".to_owned(),
                value: value.to_owned(),
                unit: Some("EU".to_owned()),
                valid_from: Some(from.to_owned()),
                ..FactInput::default()
            };
            let old = store
                .record(A_SCOPE, shoe("40", "2001"), "u-a")
                .await
                .expect("40");
            let new = store
                .record(A_SCOPE, shoe("41", "2002"), "u-a")
                .await
                .expect("41");
            store
                .record(B_SCOPE, shoe("45", "2001"), "u-b")
                .await
                .expect("45");
            let timelines = store
                .timelines(&[A_SCOPE], Some("person/nick"), Some("shoe_size"))
                .await
                .expect("a read");
            assert_eq!(timelines.len(), 1, "{dialect}: B's fact is not A's");
            let facts = &timelines[0].facts;
            assert_eq!(facts.len(), 2, "{dialect}");
            assert_eq!(facts[0].id, old.fact.id, "{dialect}: oldest first");
            assert_eq!(facts[0].status, FactStatus::Superseded, "{dialect}");
            assert_eq!(facts[0].range(), "2001..2002", "{dialect}");
            assert_eq!(facts[1].status, FactStatus::Current, "{dialect}");
            assert_eq!(facts[1].value, "41", "{dialect}");

            store
                .retract(&[A_SCOPE], &new.fact.id)
                .await
                .expect("a retraction")
                .expect("found");
            let facts = &store
                .timelines(&[A_SCOPE], None, None)
                .await
                .expect("a read")[0]
                .facts;
            assert_eq!(
                facts[0].status,
                FactStatus::Current,
                "{dialect}: 40 is back"
            );
            assert_eq!(facts[0].range(), "2001..now", "{dialect}");
        });
    }
}

/// The ranked search (`PageStore::search`) is SQL this module writes by hand
/// — a `GROUP BY` over a derived table, `CASE` weights, a join, an ordered
/// limit — so it runs on every dialect: a natural-language question finds the
/// page that answers it, any query word is enough, a title match outranks a
/// body mention, a rarer word outranks a common one, recency breaks a tie,
/// the limit applies after the ranking, and another member's scope never
/// contributes a candidate.
#[test]
fn the_ranked_search_holds_on_every_dialect() {
    for kit in TestHarness::all_dialects(|| vec![Box::new(Pages::new())]) {
        let dialect = kit.dialect;
        let blob = Arc::new(MemoryBlob::new());
        // A store per minute, so `updated_at` orders the writes.
        let at = |minutes: i64| {
            PageStore::new(
                kit.db.clone(),
                blob.clone(),
                common::kms(),
                Arc::new(FixedClock(kit.clock.0 + time::Duration::minutes(minutes))),
                Arc::new(UlidIdGen),
            )
        };
        let note = |title: &str, body: &str| PageWrite {
            entity_type: EntityType::Project,
            markdown: format!("---\nname: {title}\n---\n{body}"),
            author: Author::Human {
                id: "u-a".to_owned(),
            },
            base_version: None,
        };
        let hit = |scope: &str, slug: &str| SearchHit {
            scope: scope.to_owned(),
            slug: slug.to_owned(),
        };
        pollster::block_on(async {
            let writes = [
                (
                    A_SCOPE,
                    "agent-log-earthos",
                    "claude_code session 1",
                    "Coding agent session in earthos. Reviewed the codebase and listed the issues: \
                  a stale lockfile, a flaky test.",
                ),
                (
                    A_SCOPE,
                    "ledger",
                    "Ledger",
                    "Quarterly numbers. Nothing about the code.",
                ),
                (
                    A_SCOPE,
                    "issue-tracker",
                    "Issue tracker",
                    "Where issues go.",
                ),
                (
                    A_SCOPE,
                    "mentions-tracker",
                    "Weekly notes",
                    "The issue tracker moved.",
                ),
                (A_SCOPE, "older", "Older", "kestrel kestrel"),
                (A_SCOPE, "newer", "Newer", "kestrel"),
                // Another member's page names every query word, in its title.
                (
                    B_SCOPE,
                    "earthos-issues",
                    "EarthOS issues codebase",
                    "earthos issues codebase earthos",
                ),
            ];
            for (minute, (scope, slug, title, body)) in writes.iter().enumerate() {
                at(i64::try_from(minute).expect("a small index"))
                    .write(scope, slug, note(title, body))
                    .await
                    .unwrap_or_else(|error| panic!("{dialect}: write {slug}: {error}"));
            }
            let store = at(100);
            let search = |query: &'static str, limit: usize| {
                let store = &store;
                async move {
                    store
                        .search(&[A_SCOPE, SHARED_ONE], query, limit)
                        .await
                        .unwrap_or_else(|error| panic!("{dialect}: {query}: {error}"))
                }
            };

            // The production failure: a question full of stopwords.
            let found = search("What issues were found in the EarthOS codebase?", 10).await;
            assert_eq!(
                found.first(),
                Some(&hit(A_SCOPE, "agent-log-earthos")),
                "{dialect}: {found:?}"
            );
            assert!(
                found.iter().all(|hit| hit.scope == A_SCOPE),
                "{dialect}: another member's page entered the candidates: {found:?}"
            );

            // Only stopwords: nothing, and no error.
            assert!(
                search("what were the ones that it was?", 10)
                    .await
                    .is_empty(),
                "{dialect}"
            );

            // A title match outranks a body mention of the same word; the
            // plural in the query finds the singular in the page.
            let found = search("tracker issues", 10).await;
            assert_eq!(
                found.first(),
                Some(&hit(A_SCOPE, "issue-tracker")),
                "{dialect}: {found:?}"
            );
            assert!(
                found.contains(&hit(A_SCOPE, "mentions-tracker")),
                "{dialect}"
            );

            // Equal scores: the most recently updated page first.
            assert_eq!(
                search("kestrel", 10).await,
                vec![hit(A_SCOPE, "newer"), hit(A_SCOPE, "older")],
                "{dialect}"
            );

            // The limit comes after the ranking, not before it.
            assert_eq!(
                search("issues earthos lockfile", 1).await,
                vec![hit(A_SCOPE, "agent-log-earthos")],
                "{dialect}"
            );
        });
    }
}
