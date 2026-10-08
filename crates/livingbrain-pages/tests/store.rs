//! Behaviour tests for the page store, against a real SQL database (the kit's
//! in-memory SQLite adapter, with the module's migrations applied) and a
//! `MemoryBlob`/`FixedClock`. Bodies are sealed on the way in, so "exactly
//! what was written" is now also a statement about the envelope; the
//! encryption itself is in `crypto.rs`.

mod common;

use std::sync::Arc;

use cratefield_core::{Blob, Clock, Database, EmptyConfig, Module, Statement, UlidIdGen};
use cratefield_testing::{MemoryBlob, TestHarness};
use livingbrain_pages::{
    Author, AuthorKind, EntityType, PageError, PageStore, PageWrite, Pages, extract_links,
    parse_frontmatter,
};
use sea_query::Value as SeaValue;

/// A store over a freshly migrated in-memory database and an empty blob store.
/// The kit also builds and validates the module's harness, so a broken
/// migration or an undeclared table fails here rather than silently passing.
fn store() -> PageStore {
    let kit = TestHarness::new(vec![Box::new(Pages::new())]);
    let blob: Arc<dyn Blob> = Arc::new(MemoryBlob::new());
    let clock: Arc<dyn Clock> = Arc::new(kit.clock.clone());
    PageStore::new(
        kit.db.clone(),
        blob,
        common::kms(),
        clock,
        Arc::new(UlidIdGen),
    )
}

/// A Person page body, so the store tests do not repeat the frontmatter.
fn person(body: &str) -> String {
    format!("---\nname: Ada Lovelace\n---\n{body}")
}

fn human(id: &str, base_version: Option<u32>, markdown: String) -> PageWrite {
    PageWrite {
        entity_type: EntityType::Person,
        markdown,
        author: Author::Human { id: id.to_owned() },
        base_version,
    }
}

/// A brain write; `reconciles` names the pending human version it asserts it
/// has incorporated, or `None` for a write that claims no reconciliation.
fn brain(reconciles: Option<u32>, base_version: u32, markdown: String) -> PageWrite {
    PageWrite {
        entity_type: EntityType::Person,
        markdown,
        author: Author::Brain { reconciles },
        base_version: Some(base_version),
    }
}

/// The stored body keys for `team/ada`, oldest version first.
async fn body_keys(db: &Arc<dyn Database>) -> Vec<String> {
    let rows = db
        .query(&Statement::with_values(
            "SELECT body_key FROM page_versions WHERE scope = ? AND slug = ? ORDER BY version ASC",
            vec![
                SeaValue::String(Some(Box::new("team".to_owned()))),
                SeaValue::String(Some(Box::new("ada".to_owned()))),
            ],
        ))
        .await
        .expect("body keys");
    rows.rows
        .iter()
        .filter_map(|row| row.get::<String>("body_key"))
        .collect()
}

// ---------------------------------------------------------------------------
// Frontmatter and links (pure)

#[test]
fn frontmatter_is_typed_per_entity() {
    // No frontmatter at all: a required key is missing.
    assert!(matches!(
        parse_frontmatter("just a body", EntityType::Person),
        Err(PageError::Frontmatter(_))
    ));
    // An unknown key is refused rather than kept.
    assert!(matches!(
        parse_frontmatter("---\nname: A\ncolour: red\n---\n", EntityType::Person),
        Err(PageError::Frontmatter(_))
    ));
    // An enum key must hold one of its values.
    assert!(matches!(
        parse_frontmatter("---\nname: A\nstatus: sideways\n---\n", EntityType::Project),
        Err(PageError::Frontmatter(_))
    ));
    // A `type:` key must agree with the declared type.
    assert!(matches!(
        parse_frontmatter("---\ntype: project\nname: A\n---\n", EntityType::Person),
        Err(PageError::TypeMismatch { .. })
    ));

    let (frontmatter, body) = parse_frontmatter(
        "---\ntype: project\nname: Atlas\nstatus: active\n---\nBody.",
        EntityType::Project,
    )
    .expect("a valid Project page");
    assert_eq!(frontmatter.get("name"), Some("Atlas"));
    assert_eq!(frontmatter.get("status"), Some("active"));
    assert_eq!(body, "Body.");
}

#[test]
fn links_are_labelled_deduped_and_normalised() {
    let links =
        extract_links("See [[Alpha]] and [[beta|the beta]], then [[alpha]] again.").unwrap();
    assert_eq!(links, vec!["alpha".to_owned(), "beta".to_owned()]);
    // A target that cannot be a page is an error, not a dropped edge.
    assert!(matches!(
        extract_links("[[Not A Slug]]"),
        Err(PageError::InvalidLink(_))
    ));
    assert!(extract_links("[[unterminated").unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// Write rules

#[test]
fn every_version_is_recoverable() {
    pollster::block_on(async {
        let store = store();
        let v1 = person("First draft.");
        let v2 = person("Second draft with [[analytical-engine]].");
        let v3 = person("Third draft.");

        let m1 = store
            .write("team", "ada", human("u1", None, v1.clone()))
            .await
            .unwrap();
        assert_eq!((m1.version, m1.author_kind), (1, AuthorKind::Human));
        // v1 is a pending human edit, so the brain names it to reconcile.
        let m2 = store
            .write("team", "ada", brain(Some(1), 1, v2.clone()))
            .await
            .unwrap();
        assert_eq!((m2.version, m2.author_kind), (2, AuthorKind::Brain));
        let m3 = store
            .write("team", "ada", brain(None, 2, v3.clone()))
            .await
            .unwrap();
        assert_eq!(m3.version, 3);

        // Every version hands back exactly what was written.
        for (version, expected) in [(1_u32, &v1), (2, &v2), (3, &v3)] {
            let page = store
                .read_version("team", "ada", version)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(&page.markdown, expected, "version {version} must be exact");
        }

        let history = store.history("team", "ada").await.unwrap();
        let kinds: Vec<AuthorKind> = history.iter().map(|meta| meta.author_kind).collect();
        assert_eq!(
            kinds,
            vec![AuthorKind::Human, AuthorKind::Brain, AuthorKind::Brain]
        );
        assert_eq!(history[1].base_version, Some(1));

        // The head reads the latest version.
        let head = store.read("team", "ada").await.unwrap().unwrap();
        assert_eq!(head.markdown, v3);
        assert_eq!(head.version, 3);
    });
}

#[test]
fn a_brain_write_with_a_stale_base_conflicts() {
    pollster::block_on(async {
        let store = store();
        store
            .write("team", "ada", human("u1", None, person("v1")))
            .await
            .unwrap();
        store
            .write("team", "ada", brain(Some(1), 1, person("v2")))
            .await
            .unwrap();

        let error = store
            .write(
                "team",
                "ada",
                brain(None, 1, person("v3 from a stale base")),
            )
            .await
            .expect_err("a brain write must be based on the head");
        assert!(matches!(error, PageError::Conflict { base: 1, head: 2 }));
        assert_eq!(store.read("team", "ada").await.unwrap().unwrap().version, 2);
    });
}

#[test]
fn a_human_write_wins_over_brain_versions() {
    pollster::block_on(async {
        let store = store();
        store
            .write("team", "ada", human("u1", None, person("v1")))
            .await
            .unwrap();
        store
            .write("team", "ada", brain(Some(1), 1, person("v2")))
            .await
            .unwrap();
        store
            .write("team", "ada", brain(None, 2, person("v3")))
            .await
            .unwrap();

        // Base 1 is stale, but every version after it is the brain's, so the
        // human wins and the brain's versions stay in history.
        let meta = store
            .write(
                "team",
                "ada",
                human("u1", Some(1), person("v4, the human's")),
            )
            .await
            .unwrap();
        assert_eq!(meta.version, 4);

        let kinds: Vec<AuthorKind> = store
            .history("team", "ada")
            .await
            .unwrap()
            .iter()
            .map(|m| m.author_kind)
            .collect();
        assert_eq!(
            kinds,
            vec![
                AuthorKind::Human,
                AuthorKind::Brain,
                AuthorKind::Brain,
                AuthorKind::Human
            ]
        );
    });
}

/// The module's second promise, in one place: a human edit is never silently
/// overwritten — by the brain, or by another human on a stale base.
#[test]
fn a_human_edit_is_never_silently_overwritten() {
    pollster::block_on(async {
        let store = store();
        store
            .write("team", "ada", human("u1", None, person("v1")))
            .await
            .unwrap();

        // A brain write that claims no reconciliation is refused.
        let error = store
            .write("team", "ada", brain(None, 1, person("the brain oversteps")))
            .await
            .expect_err("a pending human edit stops an unreconciled brain write");
        assert!(matches!(error, PageError::HumanEditPending { version: 1 }));
        // A bare `Some` is not enough — it must name the pending version.
        let error = store
            .write(
                "team",
                "ada",
                brain(Some(2), 1, person("reconciles the wrong version")),
            )
            .await
            .expect_err("naming the wrong pending version is not reconciliation");
        assert!(matches!(error, PageError::HumanEditPending { version: 1 }));
        // Neither attempt touched the head or appended a version.
        assert_eq!(
            store.read("team", "ada").await.unwrap().unwrap().markdown,
            person("v1")
        );
        assert_eq!(store.history("team", "ada").await.unwrap().len(), 1);

        // A second human editing from the same base is refused too.
        store
            .write("team", "ada", human("u2", Some(1), person("v2")))
            .await
            .unwrap();
        let error = store
            .write(
                "team",
                "ada",
                human("u1", Some(1), person("v3 clobbers u2")),
            )
            .await
            .expect_err("a human write must not overwrite another human's");
        assert!(matches!(error, PageError::Conflict { base: 1, head: 2 }));
        assert_eq!(store.read("team", "ada").await.unwrap().unwrap().version, 2);
    });
}

#[test]
fn base_version_must_match_whether_the_page_exists() {
    pollster::block_on(async {
        let store = store();
        // A new page takes no base.
        let error = store
            .write("team", "ada", human("u1", Some(1), person("new")))
            .await
            .expect_err("a new page takes no base_version");
        assert!(matches!(error, PageError::NewPageWithBase { base: 1 }));

        store
            .write("team", "ada", human("u1", None, person("v1")))
            .await
            .unwrap();
        // An existing page requires one.
        let error = store
            .write("team", "ada", human("u1", None, person("edit")))
            .await
            .expect_err("editing a page requires a base_version");
        assert!(matches!(error, PageError::MissingBaseVersion));

        // The entity type cannot change silently.
        let error = store
            .write(
                "team",
                "ada",
                PageWrite {
                    entity_type: EntityType::Glossary,
                    markdown: "---\nterm: Ada\n---\n".to_owned(),
                    author: Author::Human {
                        id: "u1".to_owned(),
                    },
                    base_version: Some(1),
                },
            )
            .await
            .expect_err("a page's entity type is fixed");
        assert!(matches!(error, PageError::EntityTypeChanged { .. }));
    });
}

/// Each write owns a distinct body key, so a writer that lost the race for a
/// version cannot overwrite the winner's committed body.
#[test]
fn every_write_gets_its_own_body_key() {
    pollster::block_on(async {
        let kit = TestHarness::new(vec![Box::new(Pages::new())]);
        let blob: Arc<dyn Blob> = Arc::new(MemoryBlob::new());
        let clock: Arc<dyn Clock> = Arc::new(kit.clock.clone());
        let store = PageStore::new(
            kit.db.clone(),
            blob,
            common::kms(),
            clock,
            Arc::new(UlidIdGen),
        );

        store
            .write("team", "ada", human("u1", None, person("v1")))
            .await
            .unwrap();
        store
            .write("team", "ada", human("u1", Some(1), person("v2")))
            .await
            .unwrap();

        let keys = body_keys(&kit.db).await;
        assert_eq!(keys.len(), 2);
        assert_ne!(keys[0], keys[1], "each write must own a distinct body key");
        // `{scope}/{slug}/{version:010}-{ulid}.md`.
        assert!(keys[0].starts_with("team/ada/0000000001-") && keys[0].ends_with(".md"));
        assert!(keys[1].starts_with("team/ada/0000000002-"));
    });
}

// ---------------------------------------------------------------------------
// Links and backlinks

#[test]
fn backlinks_follow_a_pages_links() {
    pollster::block_on(async {
        let store = store();
        let links = |body: &str| PageWrite {
            entity_type: EntityType::Glossary,
            markdown: format!("---\nterm: Alpha\n---\n{body}"),
            author: Author::Human {
                id: "u1".to_owned(),
            },
            base_version: None,
        };
        store
            .write("team", "alpha", links("Links to [[beta]] and [[gamma]]."))
            .await
            .unwrap();
        assert_eq!(
            store.backlinks("team", "beta").await.unwrap(),
            vec!["alpha"]
        );
        assert_eq!(
            store.backlinks("team", "gamma").await.unwrap(),
            vec!["alpha"]
        );

        // A rewrite that drops `beta` drops the backlink with it.
        let mut rewrite = links("Only [[gamma]] now.");
        rewrite.base_version = Some(1);
        store.write("team", "alpha", rewrite).await.unwrap();
        assert!(store.backlinks("team", "beta").await.unwrap().is_empty());
        assert_eq!(
            store.backlinks("team", "gamma").await.unwrap(),
            vec!["alpha"]
        );
    });
}

/// The import ceiling a runtime reads before it buffers anything (issue #81).
///
/// The harness caps every module body at 64 KiB, which a vault file is over,
/// so the pages module raises its coarse ceiling to the ledger's own — and
/// declares the very number the import route enforces, so the two cannot drift
/// apart and let the door refuse a body the route would have taken.
#[test]
fn the_import_ceiling_is_raised_and_is_the_routes_own() {
    assert_eq!(
        Pages::new().max_body_bytes(&EmptyConfig),
        livingbrain_pages::MAX_SOURCE_BODY_BYTES
    );
}
