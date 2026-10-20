//! Every read the page module builds from its input stays under D1's
//! 100-bind ceiling.
//!
//! Each test drives one such read past the ceiling over the real SQLite
//! database, through `BindLimited`, which refuses any statement over 100
//! binds the way D1 does. The same call against the unlimited database must
//! give the same answer: a split that dropped, repeated or reordered a row
//! fails here too. Inputs grow with data (a scope per granted space, a prefix
//! per granted folder, a spelling per project name), so each test builds
//! more than the ceiling allows in one statement.

mod common;

use std::sync::Arc;

use cratefield_core::{Database, UlidIdGen};
use cratefield_module_orgs::grants::{Grant, Permission, Selector};
use cratefield_testing::{FixedClock, MemoryBlob, TestHarness};
use livingbrain_d1_fake::{BindLimited, D1_MAX_BINDS};
use livingbrain_pages::access::{AccessPlan, KIND_PAGES, Principal, PrincipalKind, ScopeFilter};
use livingbrain_pages::{
    Author, EntityType, FactStore, PageFilter, PageStore, PageSummary, PageWrite, Pages, SearchHit,
    SourceFilter, SourceKind, SourceStore, SourceSummary, SourceWrite,
};
use time::OffsetDateTime;

const WS: &str = "ws-one";
/// More scopes than one statement may bind.
const SCOPES: usize = 140;

fn scope_names() -> Vec<String> {
    (0..SCOPES).map(|i| format!("scope-{i:03}")).collect()
}

fn refs(names: &[String]) -> Vec<&str> {
    names.iter().map(String::as_str).collect()
}

fn source_write(rel_path: &str, body: &str, project: Option<&str>) -> SourceWrite {
    SourceWrite {
        kind: SourceKind::Import,
        workspace: WS.to_owned(),
        origin_ref: None,
        author: None,
        rel_path: rel_path.to_owned(),
        markdown: body.to_owned(),
        imported_by: "u-a".to_owned(),
        held: false,
        project: project.map(str::to_owned),
    }
}

fn source_store(db: Arc<dyn Database>, clock: &FixedClock) -> SourceStore {
    SourceStore::new(
        db,
        Arc::new(MemoryBlob::new()),
        common::kms(),
        Arc::new(clock.clone()),
        Arc::new(UlidIdGen),
    )
}

fn source_ids(rows: &[SourceSummary]) -> Vec<String> {
    rows.iter().map(|row| row.id.clone()).collect()
}

#[test]
fn a_source_listing_over_many_scopes_stays_within_the_bind_limit() {
    for kit in TestHarness::all_dialects(|| vec![Box::new(Pages::new())]) {
        let clock = kit.clock.clone();
        let raw = source_store(kit.db.clone(), &clock);
        let limited_db = BindLimited::new(kit.db.clone());
        let limited = source_store(limited_db.clone(), &clock);
        let mut names = scope_names();
        names.push("user-a".to_owned());
        pollster::block_on(async {
            raw.put("user-a", source_write("notes/one.md", "# one", None))
                .await
                .expect("a put");
            let filter = SourceFilter::default();
            let want = raw
                .list(WS, &refs(&names), &filter, 50)
                .await
                .expect("the unlimited read");
            let got = limited
                .list(WS, &refs(&names), &filter, 50)
                .await
                .expect("the listing stays within D1's bind ceiling");
            assert_eq!(source_ids(&got), source_ids(&want));
            assert_eq!(want.len(), 1);
        });
        assert!(
            limited_db.widest() <= D1_MAX_BINDS,
            "widest {}",
            limited_db.widest()
        );
    }
}

#[test]
fn a_project_listing_with_many_spellings_and_scopes_stays_within_the_bind_limit() {
    for kit in TestHarness::all_dialects(|| vec![Box::new(Pages::new())]) {
        let clock = kit.clock.clone();
        let raw = source_store(kit.db.clone(), &clock);
        let limited_db = BindLimited::new(kit.db.clone());
        let limited = source_store(limited_db.clone(), &clock);
        let mut names = scope_names();
        names.push("user-a".to_owned());
        pollster::block_on(async {
            // Forty spellings of one project key ("repo"): the filter matches
            // every one of them, so the name list alone is past the ceiling.
            for i in 0..40 {
                let project = format!("repo{}", "-".repeat(i + 1));
                raw.put(
                    "user-a",
                    source_write(&format!("notes/{i}.md"), &format!("# {i}"), Some(&project)),
                )
                .await
                .expect("a put");
            }
            let filter = SourceFilter {
                project: Some("repo".to_owned()),
                ..SourceFilter::default()
            };
            let want = raw
                .list(WS, &refs(&names), &filter, 25)
                .await
                .expect("unlimited");
            let got = limited
                .list(WS, &refs(&names), &filter, 25)
                .await
                .expect("the project listing stays within D1's bind ceiling");
            assert_eq!(source_ids(&got), source_ids(&want));
            assert_eq!(want.len(), 25, "the cap is the same as one statement's");

            let want = raw
                .projects(WS, &refs(&names), None)
                .await
                .expect("unlimited");
            let got = limited
                .projects(WS, &refs(&names), None)
                .await
                .expect("the projects facet stays within D1's bind ceiling");
            assert_eq!(got, want);
        });
        assert!(
            limited_db.widest() <= D1_MAX_BINDS,
            "widest {}",
            limited_db.widest()
        );
    }
}

#[test]
fn the_fact_reads_over_many_scopes_stay_within_the_bind_limit() {
    for kit in TestHarness::all_dialects(|| vec![Box::new(Pages::new())]) {
        let clock = kit.clock.clone();
        let limited_db = BindLimited::new(kit.db.clone());
        let facts = FactStore::new(
            limited_db.clone(),
            common::kms(),
            Arc::new(clock.clone()),
            Arc::new(UlidIdGen),
        );
        let names = scope_names();
        let scopes = refs(&names);
        pollster::block_on(async {
            assert_eq!(
                facts
                    .retract(&scopes, "no-such-fact")
                    .await
                    .expect("retract"),
                None
            );
            assert_eq!(
                facts
                    .subject_of(&scopes, "no-such-fact")
                    .await
                    .expect("subject_of"),
                None
            );
            assert!(
                facts
                    .timelines(&scopes, None, None)
                    .await
                    .expect("timelines")
                    .is_empty()
            );
            assert!(
                facts
                    .timelines(&scopes, Some("s"), Some("a"))
                    .await
                    .expect("timelines")
                    .is_empty()
            );
            assert!(facts.index(&scopes).await.expect("index").is_empty());
            assert!(
                facts
                    .dream_decisions(&scopes, None, 50)
                    .await
                    .expect("decisions")
                    .is_empty()
            );
            assert!(
                facts
                    .dream_decisions(&scopes, Some(("2026-01-01", "scope-000", 1)), 50)
                    .await
                    .expect("decisions")
                    .is_empty()
            );
            let outcomes = facts
                .dream_outcomes(&scopes, Some("s"))
                .await
                .expect("outcomes");
            assert!(outcomes.passes.is_empty() && outcomes.edits.is_empty());
        });
        assert!(
            limited_db.widest() <= D1_MAX_BINDS,
            "widest {}",
            limited_db.widest()
        );
    }
}

/// A member whose only reach into the shared space is 70 granted folders:
/// one page filter whose predicate alone binds past the ceiling. The role is
/// one the implicit grants do not open the shared space to (a member or a
/// viewer would be granted all of it, and the filter would be whole).
fn many_folder_filters() -> Vec<ScopeFilter> {
    let principal = Principal {
        workspace_id: WS.to_owned(),
        id: "alice".to_owned(),
        kind: PrincipalKind::Member {
            role: "guest".to_owned(),
        },
        credential_scopes: None,
    };
    let grants: Vec<Grant> = (0..70)
        .map(|i| Grant {
            id: format!("g{i}"),
            ..Grant::implicit(
                "member:alice",
                Selector::new(KIND_PAGES, "shared", vec![format!("f{i:02}")]).expect("valid"),
                Permission::Read,
            )
        })
        .collect();
    let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).expect("in range");
    AccessPlan::from_grants(&principal, grants, now).filters(KIND_PAGES, Permission::Read)
}

fn page_summary_keys(rows: &[PageSummary]) -> Vec<(String, String)> {
    rows.iter()
        .map(|row| (row.scope.clone(), row.slug.clone()))
        .collect()
}

fn hit_keys(rows: &[SearchHit]) -> Vec<(String, String)> {
    rows.iter()
        .map(|row| (row.scope.clone(), row.slug.clone()))
        .collect()
}

#[test]
fn a_page_read_under_a_grant_with_many_folders_stays_within_the_bind_limit() {
    for kit in TestHarness::all_dialects(|| vec![Box::new(Pages::new())]) {
        let clock = kit.clock.clone();
        let limited_db = BindLimited::new(kit.db.clone());
        let page_store = |db: Arc<dyn Database>| {
            PageStore::new(
                db,
                Arc::new(MemoryBlob::new()),
                common::kms(),
                Arc::new(clock.clone()),
                Arc::new(UlidIdGen),
            )
        };
        let raw = page_store(kit.db.clone());
        let limited = page_store(limited_db.clone());
        let filters = many_folder_filters();
        let shared = filters
            .iter()
            .find(|filter| !filter.is_whole())
            .map(|filter| filter.scope.clone())
            .expect("the shared space is granted");
        pollster::block_on(async {
            for folder in ["f05", "f42", "f69"] {
                raw.write(
                    &shared,
                    &format!("{folder}--widget"),
                    PageWrite {
                        entity_type: EntityType::Project,
                        markdown: format!("---\nname: Widget {folder}\n---\nA widget in {folder}."),
                        author: Author::Human {
                            id: "alice".to_owned(),
                        },
                        base_version: None,
                    },
                )
                .await
                .expect("a write");
            }
            let want = raw.list_in(&filters, 20).await.expect("unlimited list");
            let got = limited
                .list_in(&filters, 20)
                .await
                .expect("the listing stays within D1's bind ceiling");
            assert_eq!(page_summary_keys(&got), page_summary_keys(&want));
            assert_eq!(want.len(), 3);

            // A type a caller named many times is one type to the query.
            let repeated = PageFilter {
                types: vec![EntityType::Project; 150],
                ..PageFilter::default()
            };
            let want = raw
                .list_in_filtered(&filters, &repeated, 20)
                .await
                .expect("unlimited list");
            let got = limited
                .list_in_filtered(&filters, &repeated, 20)
                .await
                .expect("repeated types stay within D1's bind ceiling");
            assert_eq!(page_summary_keys(&got), page_summary_keys(&want));
            assert_eq!(want.len(), 3);

            let want: Vec<SearchHit> = raw
                .search_in(&filters, "widget", 20)
                .await
                .expect("unlimited search");
            let got: Vec<SearchHit> = limited
                .search_in(&filters, "widget", 20)
                .await
                .expect("the search stays within D1's bind ceiling");
            assert_eq!(hit_keys(&got), hit_keys(&want));
            assert_eq!(want.len(), 3);
        });
        assert!(
            limited_db.widest() <= D1_MAX_BINDS,
            "widest {}",
            limited_db.widest()
        );
    }
}
