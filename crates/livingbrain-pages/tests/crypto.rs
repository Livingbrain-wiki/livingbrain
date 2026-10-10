//! Issue #43 and ADR 0003 in tests: a body is sealed at rest, a search runs
//! off a blind index without decrypting anything, a scope can be forgotten,
//! and a key can be rotated without ever making a page unreadable.
//!
//! Every test writes through [`PageStore`] and then looks at what is actually
//! in the blob store and in D1, because a promise about storage is not
//! checkable through the API that made it. The two recording fakes exist for
//! exactly that: `MemoryBlob` has no `list`, so "nothing readable is at rest"
//! needs a store that recorded every put, and "a search binds only the scopes
//! it was given" needs a database that recorded every statement.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use cratefield_core::{
    Blob, BlobError, BlobObject, Clock, Database, DbError, PresignedPut, Rows, Statement, UlidIdGen,
};
use cratefield_testing::{FixedClock, MemoryBlob, TestHarness};
use livingbrain_pages::{
    Author, EntityType, PageError, PageStore, PageWrite, Pages, ReencryptReport, SearchHit,
    SourceKind, SourceStore, SourceWrite, envelope_version,
};
use sea_query::Value as SeaValue;

/// One object the recording blob saw: `(key, content_type, bytes)`.
type Seen = (String, String, Vec<u8>);

/// A `MemoryBlob` that remembers every object written to it, so a test can
/// look at what is at rest rather than only at what the store says it put
/// there.
#[derive(Clone, Default)]
struct RecordingBlob {
    inner: MemoryBlob,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl RecordingBlob {
    async fn object(&self, key: &str) -> Option<(String, Vec<u8>)> {
        self.inner
            .get(key)
            .await
            .ok()
            .flatten()
            .map(|found| (found.content_type, found.bytes))
    }

    /// Latest write per key, because that is the one a reader is handed.
    /// Sorted by key, so a failure names the same object every run.
    fn objects(&self) -> Vec<Seen> {
        let seen = self.seen.lock().unwrap();
        let mut latest: BTreeMap<&str, &Seen> = BTreeMap::new();
        for entry in seen.iter() {
            latest.insert(&entry.0, entry);
        }
        latest.into_values().cloned().collect()
    }
}

#[async_trait]
impl Blob for RecordingBlob {
    async fn put(&self, key: &str, bytes: &[u8], content_type: &str) -> Result<(), BlobError> {
        self.seen
            .lock()
            .unwrap()
            .push((key.to_owned(), content_type.to_owned(), bytes.to_vec()));
        self.inner.put(key, bytes, content_type).await
    }

    async fn get(&self, key: &str) -> Result<Option<BlobObject>, BlobError> {
        self.inner.get(key).await
    }

    async fn delete(&self, key: &str) -> Result<(), BlobError> {
        self.inner.delete(key).await
    }

    async fn signed_url(&self, _key: &str, _ttl: Duration) -> Result<String, BlobError> {
        Err(BlobError::Unsupported("no presigner in a test".to_owned()))
    }

    async fn signed_put_url(
        &self,
        _key: &str,
        _content_type: &str,
        _content_length: Option<u64>,
        _ttl: Duration,
    ) -> Result<PresignedPut, BlobError> {
        Err(BlobError::Unsupported("no presigner in a test".to_owned()))
    }
}

/// A database that records every statement it is asked to run, then answers
/// from the real one, so a test can read `search`'s promise to bind only the
/// asker's scopes back off the wire.
struct RecordingDatabase {
    inner: Arc<dyn Database>,
    seen: Log,
}

/// Every statement the store ran, shared by the fake and the test reading it.
type Log = Arc<Mutex<Vec<Statement>>>;

#[async_trait]
impl Database for RecordingDatabase {
    async fn execute(&self, stmt: &Statement) -> Result<u64, DbError> {
        self.record(stmt);
        self.inner.execute(stmt).await
    }

    async fn query(&self, stmt: &Statement) -> Result<Rows, DbError> {
        self.record(stmt);
        self.inner.query(stmt).await
    }

    async fn batch_atomic(&self, stmts: &[Statement]) -> Result<(), DbError> {
        self.seen.lock().unwrap().extend(stmts.iter().cloned());
        self.inner.batch_atomic(stmts).await
    }
}

impl RecordingDatabase {
    fn record(&self, stmt: &Statement) {
        self.seen.lock().unwrap().push(stmt.clone());
    }

    /// A migrated database, a recording blob and a recording database, the
    /// store over them, and the statement log.
    fn bench() -> (TestHarness, Arc<RecordingBlob>, Log, PageStore) {
        let kit = TestHarness::new(vec![Box::new(Pages::new())]);
        let blob = Arc::new(RecordingBlob::default());
        let log = Log::default();
        let db = Arc::new(Self {
            inner: kit.db.clone(),
            seen: log.clone(),
        });
        let clock: Arc<dyn Clock> = Arc::new(kit.clock.clone());
        let store = PageStore::new(db, blob.clone(), common::kms(), clock, Arc::new(UlidIdGen));
        (kit, blob, log, store)
    }
}

/// The same store with the clock moved `minutes` forward, which is how a test
/// crosses a rotation's grace window without sleeping.
fn after(kit: &TestHarness, blob: &Arc<RecordingBlob>, minutes: i64) -> PageStore {
    PageStore::new(
        kit.db.clone(),
        blob.clone(),
        common::kms(),
        Arc::new(FixedClock(kit.clock.0 + time::Duration::minutes(minutes))),
        Arc::new(UlidIdGen),
    )
}

/// A person page with a distinctive name, so no token of the body is also a
/// slug, an entity type or an author id.
fn page(name: &str, body: &str) -> String {
    format!("---\nname: {name}\n---\n{body}")
}

fn human(base_version: Option<u32>, markdown: String) -> PageWrite {
    PageWrite {
        entity_type: EntityType::Person,
        markdown,
        author: Author::Human {
            id: "u1".to_owned(),
        },
        base_version,
    }
}

/// Writes one page per `(slug, name, body)`.
fn put(store: &PageStore, scope: &str, pages: &[(&str, &str, &str)]) {
    for (slug, name, body) in pages {
        pollster::block_on(store.write(scope, slug, human(None, page(name, body))))
            .expect("a write");
    }
}

fn bind(value: &str) -> SeaValue {
    SeaValue::String(Some(Box::new(value.to_owned())))
}

/// The rows a query returns, for a test reading storage rather than the API.
fn rows(db: &Arc<dyn Database>, sql: &str, values: Vec<SeaValue>) -> Rows {
    pollster::block_on(db.query(&Statement::with_values(sql, values))).expect("a query")
}

/// Runs a statement, for a test writing the row shape the code would not.
fn run(db: &Arc<dyn Database>, sql: &str, values: Vec<SeaValue>) {
    pollster::block_on(db.execute(&Statement::with_values(sql, values))).expect("a statement");
}

/// The Markdown a version reads back as, panicking if it does not.
async fn markdown(store: &PageStore, scope: &str, slug: &str, version: u32) -> String {
    let page = store
        .read_version(scope, slug, version)
        .await
        .expect("a read")
        .expect("a version");
    page.markdown
}

/// The hits a search returns, unwrapped, so an assertion fits on one line.
async fn hits(store: &PageStore, scopes: &[&str], query: &str) -> Vec<SearchHit> {
    store.search(scopes, query, 10).await.expect("a search")
}

/// Every page in `pages` still reads with the word its author put in it, and
/// the two Zephyrmoor pages are both still found: the "nothing went
/// unreadable" assertion of the rotation test, said once per point in the
/// rotation instead of copied into it.
async fn survives(store: &PageStore, scope: &str, pages: &[(&str, &str)]) {
    for (slug, word) in pages {
        let read = markdown(store, scope, slug, 1).await;
        assert!(read.contains(word), "{slug} must survive");
    }
    assert_eq!(
        hits(store, &[scope], "zephyrmoor").await.len(),
        2,
        "searchable"
    );
}

/// A whole re-encryption report in one assertion: a pass always has three
/// numbers, and a test that cares about one reads better seeing all three.
#[track_caller]
fn report(got: &ReencryptReport, rewritten: usize, retired: &[u32], pending: &[u32]) {
    assert_eq!(
        (
            got.rewritten,
            got.retired.as_slice(),
            got.pending.as_slice()
        ),
        (rewritten, retired, pending),
        "{got:?}"
    );
}

/// The words of a body long enough to be worth looking for in a table row:
/// lowercased, alphanumeric runs of four characters or more.
fn words(markdown: &str) -> BTreeSet<String> {
    markdown
        .split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|word| word.chars().count() >= 4)
        .collect()
}

/// Whether `needle` occurs in `haystack` as raw bytes, not as text.
fn windows(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Every value of every row of every table, as text, tagged with its table,
/// so AC1 can name the table a body word turns up in. The table list comes
/// from `sqlite_master`, so the scan cannot miss one.
fn every_row_value(db: &Arc<dyn Database>) -> Vec<(String, String)> {
    let tables = rows(
        db,
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
         ORDER BY name ASC",
        vec![],
    );
    let mut values = Vec::new();
    for table in tables
        .rows
        .iter()
        .filter_map(|row| row.get::<String>("name"))
    {
        for row in rows(db, &format!("SELECT * FROM {table}"), vec![]).rows {
            for (_, value) in row.columns() {
                values.push((table.clone(), format!("{value:?}")));
            }
        }
    }
    values
}

/// The body keys of a scope's versions, oldest first, with the version they
/// hold: what a rotation test inspects and what AC1 must not find plaintext
/// in.
fn body_keys(db: &Arc<dyn Database>, scope: &str) -> Vec<(u32, String)> {
    rows(
        db,
        "SELECT version, body_key FROM page_versions WHERE scope = ? ORDER BY version ASC",
        vec![bind(scope)],
    )
    .rows
    .iter()
    .map(|row| {
        (
            row.get::<u32>("version").unwrap_or_default(),
            row.get::<String>("body_key").unwrap_or_default(),
        )
    })
    .collect()
}

/// `(key_version, is the key still there, retired_at)` per stored scope key.
fn keys(db: &Arc<dyn Database>, scope: &str) -> Vec<(u32, bool, Option<String>)> {
    rows(
        db,
        "SELECT key_version, wrapped_dek, retired_at FROM scope_keys \
         WHERE scope = ? ORDER BY key_version ASC",
        vec![bind(scope)],
    )
    .rows
    .iter()
    .map(|row| {
        (
            row.get::<u32>("key_version").unwrap_or_default(),
            row.get::<String>("wrapped_dek").is_some(),
            row.get::<String>("retired_at"),
        )
    })
    .collect()
}

// ---------------------------------------------------------------------------
// AC1 — nothing readable is at rest

/// AC1: bodies whose words appear in no object and in no table, and which all
/// still read back exactly.
#[test]
fn no_plaintext_is_at_rest() {
    pollster::block_on(async {
        let (kit, blob, _log, store) = RecordingDatabase::bench();
        let db = &kit.db;

        // `(scope, slug, name, body)`, two versions of each so a superseded
        // body is at rest too, and one body carrying a `[[wiki-link]]`, since
        // a link target is a word an author chose out of the body and the
        // store is meant to keep it as a link.
        let cases = [
            (
                "team",
                "alpha",
                "Kestrel Nakamura",
                "The Zephyrmoor consortium met in Reykjavik about the Quicksilver ledger.",
            ),
            (
                "team",
                "beta",
                "Tobias Ferrante",
                "Ferrante reviews the Zephyrmoor charter with counsel. See [[vandenberg-marshal]].",
            ),
            (
                "other",
                "alpha",
                "Ingrid Solheim",
                "Solheim signed the Zephyrmoor charter last autumn.",
            ),
        ];
        let mut written: Vec<(&str, &str, u32, String)> = Vec::new();
        for (scope, slug, name, body) in &cases {
            let versions = [
                (1, page(name, body)),
                (
                    2,
                    page("Nobody In Particular", &format!("{body} (revised)")),
                ),
            ];
            for (version, markdown) in versions {
                let base = (version > 1).then_some(version - 1);
                let meta = store
                    .write(scope, slug, human(base, markdown.clone()))
                    .await
                    .expect("a write");
                assert_eq!(meta.version, version);
                written.push((*scope, *slug, version, markdown));
            }
        }

        // Every object in the blob store, at rest, as bytes and as text.
        let objects = blob.objects();
        assert_eq!(objects.len(), 6, "six versions, six objects");
        for (key, content_type, bytes) in &objects {
            assert_eq!(content_type, "application/octet-stream", "{key}");
            assert_eq!(&bytes[..4], b"lbe1", "{key} must be a sealed envelope");
            assert!(envelope_version(bytes).is_ok(), "{key} names a key version");
            let text = String::from_utf8_lossy(bytes);
            for (scope, slug, _, markdown) in &written {
                let held = text.contains(markdown.as_str()) || windows(bytes, markdown.as_bytes());
                assert!(!held, "{key} holds the plaintext of {scope}/{slug}");
            }
        }

        // Every value of every row of every table, minus the strings the
        // store is *meant* to hold in the clear: the slugs, the scopes, the
        // entity type, "revised" and the parts of the name that a version-2
        // frontmatter carries, and — the honest one — the `[[link]]` target
        // and its hyphen-separated parts, which are body-derived strings kept
        // as identifiers.
        let values = every_row_value(db);
        assert!(values.len() > written.len(), "the scan read nothing");
        let in_the_clear: BTreeSet<String> =
            "alpha beta team other person revised particular vandenberg-marshal vandenberg marshal"
                .split(' ')
                .map(String::from)
                .collect();
        let body_words: BTreeSet<String> = written
            .iter()
            .flat_map(|(_, _, _, markdown)| words(markdown))
            .chain(words(
                "Kestrel Nakamura Nobody In Particular Ferrante Solheim",
            ))
            .filter(|word| !in_the_clear.contains(word))
            .collect();
        assert!(
            body_words.contains("zephyrmoor") && body_words.contains("reykjavik"),
            "the scan needs body words to look for"
        );
        for (table, value) in &values {
            for word in &body_words {
                assert!(
                    !value.to_lowercase().contains(word.as_str()),
                    "a {table} value holds the body word `{word}`: {value}"
                );
            }
        }

        // The link target really is in the clear, in the table that says so:
        // this is the one body-derived string the store does not hide, and
        // AC1 would be dishonest not to say which. The wrapped key is there
        // too, and it is not the key.
        let target = "vandenberg-marshal";
        assert!(
            values.iter().any(
                |(table, value)| table == "page_links" && value.to_lowercase().contains(target)
            ),
            "`{target}` is not in page_links, so the exclusion above describes nothing"
        );
        assert_eq!(
            keys(db, "team"),
            vec![(1, true, None)],
            "the key is wrapped, not absent"
        );

        // And every version still reads back exactly what was written.
        for (scope, slug, version, expected) in &written {
            let read = markdown(&store, scope, slug, *version).await;
            assert_eq!(read, *expected, "{scope}/{slug} v{version}");
        }
    });
}

// ---------------------------------------------------------------------------
// AC2 — forgetting a scope

/// AC2: forgetting a scope destroys the key, so the bodies stay in the blob
/// store and come back as an error, a scope nobody forgot is untouched, and
/// the scope's index and link rows go with it.
#[test]
fn forgetting_a_scope_makes_its_bodies_unreadable() {
    pollster::block_on(async {
        let (kit, blob, _log, store) = RecordingDatabase::bench();
        let db = &kit.db;
        put(
            &store,
            "team",
            &[("alpha", "Kestrel Nakamura", "Zephyrmoor met in Reykjavik.")],
        );
        put(
            &store,
            "other",
            &[(
                "alpha",
                "Ingrid Solheim",
                "Solheim read the Quicksilver ledger.",
            )],
        );

        store.forget_scope("team").await.expect("forget");

        // The body is gone, and the error says why rather than looking like a
        // missing page.
        let error = store
            .read("team", "alpha")
            .await
            .expect_err("a shredded body must not read");
        assert!(matches!(error, PageError::Shredded(ref scope) if scope == "team"));
        assert!(
            !error.to_string().contains("Zephyrmoor"),
            "the error leaks no body"
        );

        // The metadata survives, so the shape of the wiki is still there, and
        // the ciphertext survives: shredding destroys the key, not the object.
        assert_eq!(store.history("team", "alpha").await.unwrap().len(), 1);
        assert!(blob.object(&body_keys(db, "team")[0].1).await.is_some());
        // Every version of the scope's key is gone, and it was not retired:
        // the difference is what stops a later write keying it again.
        let stored = keys(db, "team");
        assert!(stored.iter().all(|(_, live, _)| !live));
        assert!(stored.iter().all(|(_, _, retired)| retired.is_none()));

        // The scope's index rows went with it, and so did its links: a link
        // target is a word out of a body, and leaving it behind would leave
        // the author's choice of words behind.
        let links = rows(
            db,
            "SELECT to_slug FROM page_links WHERE scope = 'team'",
            vec![],
        );
        assert!(links.rows.is_empty(), "a forgotten scope keeps no links");
        assert!(
            hits(&store, &["team"], "zephyrmoor").await.is_empty(),
            "a shredded scope cannot be searched"
        );

        // The other scope never noticed.
        assert!(
            markdown(&store, "other", "alpha", 1)
                .await
                .contains("Quicksilver")
        );
        assert_eq!(
            hits(&store, &["other"], "quicksilver").await,
            vec![SearchHit {
                scope: "other".to_owned(),
                slug: "alpha".to_owned()
            }]
        );

        // A write to a shredded scope is refused rather than quietly keyed
        // again, and so is a rotation, which must not resurrect the scope by
        // creating a fresh key for it.
        let refused = [
            store
                .write("team", "beta", human(None, page("Nobody", "new")))
                .await
                .expect_err("a forgotten scope stays forgotten"),
            store
                .rotate_scope_key("team")
                .await
                .expect_err("a rotation must not resurrect a forgotten scope"),
        ];
        for error in refused {
            assert!(matches!(error, PageError::Shredded(_)), "{error}");
        }
        assert_eq!(keys(db, "team").len(), 1, "the rotation added a version");
    });
}

// ---------------------------------------------------------------------------
// AC3 — a search binds only the scopes it was given

proptest::proptest! {
    #![proptest_config(proptest::test_runner::Config::with_cases(16))]

    // AC3, over random scopes, pages and askers: every hit is in a scope the
    // asker named, every statement the search ran against `page_terms` or
    // `scope_keys` bound one of those scopes and nothing else, and a page the
    // asker owns whose body holds every query word *is* found — without that
    // last one, a search that always returned nothing would satisfy the rest.
    #[test]
    fn search_only_touches_the_askers_scopes(
        pages in proptest::collection::vec(
            (
                proptest::sample::select(vec!["alpha", "beta", "gamma"]),
                proptest::sample::select(vec!["one", "two", "three"]),
                proptest::sample::select(vec!["zephyrmoor", "reykjavik", "ledger", "atlas"]),
            ),
            0..6,
        ),
        askers in proptest::collection::vec(
            proptest::sample::select(vec!["alpha", "beta", "gamma"]),
            1..=3,
        ),
        query in proptest::sample::select(vec![
            "zephyrmoor", "reykjavik", "zephyrmoor ledger", "nothing here", "",
        ]),
    ) {
        let outcome: Result<(), proptest::test_runner::TestCaseError> =
            pollster::block_on(async {
            let (_, _, log, store) = RecordingDatabase::bench();
            // A page can be drawn twice, so every write is based on the head it
            // actually found, and the head's word is what the index holds.
            let mut heads: BTreeMap<(&str, &str), (u32, &str)> = BTreeMap::new();
            for &(scope, slug, word) in &pages {
                let base = heads.get(&(scope, slug)).map(|(version, _)| *version);
                let body = page("Kestrel Nakamura", &format!("The {word} note."));
                store.write(scope, slug, human(base, body)).await.expect("a write");
                let next = base.unwrap_or(0) + 1;
                heads.insert((scope, slug), (next, word));
            }
            let mut asker = askers.clone();
            asker.sort_unstable();
            asker.dedup();

            log.lock().unwrap().clear();
            let binding = hits(&store, &asker, query).await;
            let found: BTreeSet<(&str, &str)> = binding.iter()
                .map(|hit| (hit.scope.as_str(), hit.slug.as_str())).collect();
            let allowed: BTreeSet<&str> = asker.iter().copied().collect();
            for (scope, slug) in &found {
                proptest::prop_assert!(
                    allowed.contains(scope),
                    "hit {slug} is outside the asker's scopes"
                );
            }

            // The positive half, only when the query has an indexable token: a
            // page in a scope the asker owns whose *head* body holds every
            // query word must come back. A wordless query promises nothing.
            let wanted: Vec<String> = query.split(|c: char| !c.is_alphanumeric())
                .map(str::to_lowercase)
                .filter(|word| word.len() >= 2).collect();
            if !wanted.is_empty() {
                for ((scope, slug), (_, word)) in &heads {
                    if allowed.contains(scope) && wanted.iter().all(|w| w == *word) {
                        proptest::prop_assert!(
                            found.contains(&(scope, slug)),
                            "{scope}/{slug} holds every token of `{query}` in a scope the asker \
                             owns, but the search did not return it"
                        );
                    }
                }
            }
            for statement in log.lock().unwrap().iter() {
                if !(statement.sql.contains("page_terms") || statement.sql.contains("scope_keys")) {
                    continue;
                }
                let bound = statement.values.0.first().and_then(|value| match value {
                    SeaValue::String(Some(text)) => Some(text.as_str()),
                    _ => None,
                }).unwrap_or_else(|| {
                    panic!("an index statement bound no scope: {}", statement.sql)
                });
                proptest::prop_assert!(
                    allowed.contains(bound),
                    "a search bound scope `{bound}`, which the asker did not name: {}",
                    statement.sql
                );
            }
            Ok(())
        });
        outcome.unwrap();
    }
}

/// A blind index cannot say "any page": a query with no token in it matches
/// nothing, and neither does a zero limit. A scope nobody wrote contributes
/// nothing rather than failing, and one that is not a slug is refused. AND
/// across tokens, and case and punctuation are the tokenizer's business, not
/// the caller's.
#[test]
fn search_is_a_blind_index_not_a_text_search() {
    pollster::block_on(async {
        let (_, _, _log, store) = RecordingDatabase::bench();
        put(
            &store,
            "team",
            &[
                ("both", "Kestrel Nakamura", "Zephyrmoor and Reykjavik."),
                ("one", "Tobias Ferrante", "Zephyrmoor only."),
                ("neither", "Ingrid Solheim", "Reykjavik only."),
            ],
        );
        let hit = |slug: &str| SearchHit {
            scope: "team".to_owned(),
            slug: slug.to_owned(),
        };
        // A page that has one of two words is not a hit for a query naming both.
        assert_eq!(
            hits(&store, &["team"], "zephyrmoor reykjavik").await,
            vec![hit("both")]
        );
        assert_eq!(
            hits(&store, &["team"], "Zephyrmoor.").await,
            vec![hit("both"), hit("one")]
        );
        for (scopes, query, limit) in [
            (["team"], "  ...  ", 10),
            (["team"], "zephyrmoor", 0),
            (["nowhere"], "zephyrmoor", 10),
        ] {
            let found = store.search(&scopes, query, limit).await.expect("a search");
            assert!(found.is_empty(), "{query:?} in {scopes:?} at {limit}");
        }
        assert!(matches!(
            store.search(&["Team A"], "z", 10).await,
            Err(PageError::InvalidScope(_))
        ));
    });
}

// ---------------------------------------------------------------------------
// AC4 — rotation

/// AC4: a rotation is online — old bodies keep reading, new writes use the
/// new key, a re-encryption pass finishes the job without ever making a page
/// unreadable, and a second run is a no-op.
#[test]
fn rotating_a_scope_key_never_makes_a_page_unreadable() {
    pollster::block_on(async {
        let (kit, blob, _log, store) = RecordingDatabase::bench();
        let db = &kit.db;
        put(
            &store,
            "team",
            &[
                (
                    "alpha",
                    "Kestrel Nakamura",
                    "The Zephyrmoor charter, first draft.",
                ),
                ("beta", "Tobias Ferrante", "The Quicksilver annex."),
            ],
        );
        let pages: [(&str, &str); 3] = [
            ("alpha", "first draft"),
            ("beta", "annex"),
            ("gamma", "addendum"),
        ];

        // The key version every stored body is on, in body-key order.
        let under = || -> Vec<u32> {
            body_keys(db, "team")
                .into_iter()
                .map(|(_, key)| {
                    let (_, bytes) = pollster::block_on(blob.object(&key)).expect("a body");
                    envelope_version(&bytes).expect("an envelope")
                })
                .collect()
        };
        assert_eq!(under(), vec![1, 1]);

        // Rotate. Nothing is re-sealed and nothing is retired.
        assert_eq!(store.rotate_scope_key("team").await.unwrap(), 2);
        assert_eq!(keys(db, "team"), vec![(1, true, None), (2, true, None)]);
        assert_eq!(under(), vec![1, 1], "rotation alone rewrites nothing");

        // Mid-rotation both worlds work: an old body still reads, and a new
        // write is sealed under the new key.
        assert!(
            markdown(&store, "team", "alpha", 1)
                .await
                .contains("first draft")
        );
        put(
            &store,
            "team",
            &[("gamma", "Ingrid Solheim", "The Zephyrmoor addendum.")],
        );
        assert_eq!(under(), vec![1, 1, 2]);
        survives(&store, "team", &pages).await;

        // The first pass re-seals the bodies, but version 1 is inside the
        // grace window, so it stays live and is reported as pending.
        report(&store.reencrypt_scope("team").await.unwrap(), 2, &[], &[1]);
        assert_eq!(under(), vec![2, 2, 2]);
        assert_eq!(
            keys(db, "team"),
            vec![(1, true, None), (2, true, None)],
            "version 1 must survive a pass inside the grace window"
        );
        survives(&store, "team", &pages).await;

        // Past the grace window, a second pass finishes the job: version 1 is
        // retired, and the guard inside the retirement statement is what makes
        // that safe rather than lucky.
        let later = after(&kit, &blob, 20);
        report(&later.reencrypt_scope("team").await.unwrap(), 0, &[1], &[]);
        let stored = keys(db, "team");
        assert_eq!(stored.len(), 2);
        assert!(
            !stored[0].1 && stored[0].2.is_some(),
            "version 1 is gone, retired"
        );
        assert!(stored[1].1 && stored[1].2.is_none(), "version 2 is live");

        // Version 1 of the page still reads: the pass re-sealed the body under
        // version 2 and moved the row with it, so the key it was written under
        // is no longer the one the row names — which is what retiring that key
        // safely means.
        assert!(
            markdown(&later, "team", "alpha", 1)
                .await
                .contains("first draft")
        );
        survives(&later, "team", &pages).await;

        // And re-running does nothing at all.
        report(&later.reencrypt_scope("team").await.unwrap(), 0, &[], &[]);
        assert_eq!(under(), vec![2, 2, 2]);
    });
}

/// A scope never holds more than two live keys, because a rotation refuses
/// while the previous one is unfinished. That cap is what bounds a search to
/// at most two unwraps per scope, so it is worth a test of its own.
#[test]
fn a_rotation_refuses_while_the_previous_one_is_unfinished() {
    pollster::block_on(async {
        let (kit, blob, _log, store) = RecordingDatabase::bench();
        let db = &kit.db;
        put(
            &store,
            "team",
            &[("alpha", "Kestrel Nakamura", "Zephyrmoor.")],
        );

        assert_eq!(store.rotate_scope_key("team").await.unwrap(), 2);
        let error = store
            .rotate_scope_key("team")
            .await
            .expect_err("a second rotation must not start on top of the first");
        assert!(
            matches!(error, PageError::RotationPending { superseded: 1, .. }),
            "{error}"
        );
        assert_eq!(keys(db, "team").len(), 2, "it created a version anyway");

        // Finishing the first rotation makes the scope rotatable again, and
        // the search still finds the page across both versions.
        let later = after(&kit, &blob, 20);
        later.reencrypt_scope("team").await.unwrap();
        assert_eq!(later.rotate_scope_key("team").await.unwrap(), 3);
        assert_eq!(hits(&later, &["team"], "zephyrmoor").await.len(), 1);
    });
}

/// The retirement guard: a version row still on the old key keeps that key
/// alive, so a pass can never destroy a key a row depends on. Simulated with
/// a version row the re-seal's join cannot reach — one whose `pages` row is
/// gone — because that is the shape a write that fetched the old key and
/// committed late leaves behind: the row says version 1, and the pass that
/// would move it never sees it. Nothing can read that version any more, but
/// destroying the key it names is not this pass's decision to make.
#[test]
fn a_key_a_body_still_names_is_not_retired() {
    pollster::block_on(async {
        let (kit, blob, _log, store) = RecordingDatabase::bench();
        let db = &kit.db;
        put(
            &store,
            "team",
            &[("alpha", "Kestrel Nakamura", "Zephyrmoor.")],
        );
        let scoped = vec![bind("team"), bind("alpha")];

        assert_eq!(store.rotate_scope_key("team").await.unwrap(), 2);
        report(&store.reencrypt_scope("team").await.unwrap(), 1, &[], &[1]);

        // A version row still naming version 1, and the `pages` row gone, so
        // the re-seal's join skips it. The retirement statement's `NOT
        // EXISTS` does not skip it — that asymmetry is the guard.
        run(
            db,
            "INSERT INTO page_versions (scope, slug, version, body_key, key_version, \
             author_kind, author, created_at) SELECT scope, slug, 99, body_key, 1, \
             author_kind, author, created_at FROM page_versions WHERE scope = ? AND slug = ?",
            scoped.clone(),
        );
        run(
            db,
            "DELETE FROM pages WHERE scope = ? AND slug = ?",
            scoped.clone(),
        );

        // Past the grace window — the only other thing that would let version
        // 1 retire — the guard refuses.
        let later = after(&kit, &blob, 20);
        report(&later.reencrypt_scope("team").await.unwrap(), 0, &[], &[1]);
        assert!(keys(db, "team")[0].1, "a body still named version 1");

        // Drop the row the guard was protecting, and a pass past the window
        // retires the key. The window runs from when version 2 was created, so
        // 40 minutes is late enough.
        run(
            db,
            "DELETE FROM page_versions WHERE scope = ? AND slug = ? AND version = 99",
            scoped,
        );
        let at40 = after(&kit, &blob, 40);
        report(&at40.reencrypt_scope("team").await.unwrap(), 0, &[1], &[]);
        assert!(!keys(db, "team")[0].1);
        // The page itself is gone — this test deleted its `pages` row — so
        // what the guard bought is that the key outlived the row that named
        // it, not that anything stayed readable.
        assert!(at40.read("team", "alpha").await.unwrap().is_none());
    });
}

/// The same guard for the other body a rotation does not re-seal. A re-encryption
/// pass rewrites page bodies (issue #43); an imported source body (issue #81)
/// stays under the key it arrived with, so without a `sources` check the pass
/// would retire a version a source row still names and leave that body
/// readable by nobody and lost to everybody.
#[test]
fn a_key_a_source_body_still_names_is_not_retired() {
    pollster::block_on(async {
        let (kit, blob, _log, store) = RecordingDatabase::bench();
        let db = &kit.db;
        let sources = SourceStore::new(
            db.clone(),
            blob.clone(),
            common::kms(),
            Arc::new(kit.clock.clone()),
            Arc::new(UlidIdGen),
        );
        let (source, created) = sources
            .put(
                "team",
                SourceWrite {
                    kind: SourceKind::Import,
                    workspace: "ws-one".to_owned(),
                    origin_ref: None,
                    author: None,
                    rel_path: "notes/one.md".to_owned(),
                    markdown: "# One\n\nA note.\n".to_owned(),
                    imported_by: "u1".to_owned(),
                    held: false,
                },
            )
            .await
            .expect("an import");
        assert!(created, "the import created the row");

        assert_eq!(store.rotate_scope_key("team").await.unwrap(), 2);
        // The page pass has no page body to rewrite and still leaves version 1
        // pending, because the source row still names it.
        report(&store.reencrypt_scope("team").await.unwrap(), 0, &[], &[1]);
        assert!(
            keys(db, "team")[0].1,
            "a source body is still sealed under version 1"
        );

        // The source still reads, which is the whole point of keeping the key.
        let opened = sources
            .open("team", &source.body_sha256)
            .await
            .expect("the source body opens");
        assert!(opened.contains("A note."), "{opened}");

        // Once nothing names it, a pass past the grace window retires it. The
        // window runs from when version 2 was created, so 40 minutes is late
        // enough.
        run(
            db,
            "DELETE FROM sources WHERE scope = ? AND body_sha256 = ?",
            vec![bind("team"), bind(&source.body_sha256)],
        );
        let later = after(&kit, &blob, 40);
        report(&later.reencrypt_scope("team").await.unwrap(), 0, &[1], &[]);
        assert!(!keys(db, "team")[0].1);
    });
}

// AC5 lives in `latency.rs`, in its own binary so the property tests above
// cannot contend with it for CPU.
