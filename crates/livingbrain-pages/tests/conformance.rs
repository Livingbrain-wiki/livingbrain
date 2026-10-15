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
use cratefield_testing::{MemoryBlob, TestHarness, assert_wasm_safe_deps, conformance};
use livingbrain_pages::{
    Pages, ProjectCount, SourceError, SourceFilter, SourceKind, SourceStore, SourceWrite,
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
