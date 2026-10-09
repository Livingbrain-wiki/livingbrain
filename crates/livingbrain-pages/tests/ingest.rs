//! The ingest pipeline (issue #77): one path for every kind of source, run
//! against a real database (the kit's in-memory SQLite with the module's
//! migrations applied) and a `MemoryBlob`.
//!
//! The properties under test are the four the ledger exists to hold: the
//! same message is one row however many times it arrives; a held source
//! never reaches extraction until it is released; the body is in the blob
//! store, sealed, and in no database column anywhere; and redaction happens
//! inside the pipeline, before anything is hashed or stored.

mod common;

use std::sync::{Arc, Mutex};

use cratefield_core::{Blob, Database, Statement, UlidIdGen};
use cratefield_testing::{FakeDefer, FixedClock, MemoryBlob, TestHarness};
use livingbrain_pages::{
    Extract, IngestOutcome, Ingestor, Pages, Screen, Source, SourceIngest, SourceKind, SourceStore,
    Verdict,
};

/// The marker one test plants in a body, to prove no database column ever
/// carries it.
const MARKER: &str = "ZX-MARKER-QW7F";

/// A migrated database, a blob store, and the parts a pipeline is assembled
/// from. The blob is the raw memory store — no scope prefix — so a test reads
/// bodies at exactly the keys the row names, and the `Arc`s are kept so each
/// test can build `Ingestor`s with its own hooks over the one world.
struct World {
    db: Arc<dyn Database>,
    blob: Arc<dyn Blob>,
    kms: Arc<dyn cratefield_kms::Kms>,
    clock: Arc<FixedClock>,
    defer: FakeDefer,
    extractor: Recording,
}

fn world() -> World {
    let kit = TestHarness::new(vec![Box::new(Pages::new())]);
    World {
        db: kit.db.clone(),
        blob: Arc::new(MemoryBlob::new()),
        kms: common::kms(),
        clock: Arc::new(kit.clock.clone()),
        defer: kit.defer.clone(),
        extractor: Recording::default(),
    }
}

impl World {
    /// The ledger over this world's ports.
    fn store(&self) -> SourceStore {
        SourceStore::new(
            self.db.clone(),
            self.blob.clone(),
            self.kms.clone(),
            self.clock.clone(),
            Arc::new(UlidIdGen),
        )
    }

    /// The pipeline over this world, with the recording extractor wired in.
    fn ingestor(&self) -> Ingestor {
        Ingestor::new(self.store(), Arc::new(self.defer.clone()))
            .with_extractor(Arc::new(self.extractor.clone()))
    }

    /// How many rows the ledger holds.
    async fn rows(&self) -> usize {
        self.db
            .query(&Statement::new("SELECT COUNT(*) AS n FROM sources"))
            .await
            .expect("a count")
            .first()
            .and_then(|row| row.get::<i64>("n"))
            .expect("a count column") as usize
    }
}

/// A chat message from one member of one workspace, the shape every
/// non-import kind arrives in: no path, a message id for an origin.
fn chat(body: &str) -> SourceIngest {
    SourceIngest {
        kind: SourceKind::Chat,
        workspace: "ws-one".to_owned(),
        scope: "team".to_owned(),
        origin_ref: Some("msg_0001".to_owned()),
        author: Some("u1".to_owned()),
        imported_by: "u1".to_owned(),
        rel_path: String::new(),
        body: body.to_owned(),
    }
}

/// The extraction hook a test listens to: one remembered id per call.
#[derive(Clone, Default)]
struct Recording(Arc<Mutex<Vec<String>>>);

impl Recording {
    fn seen(&self) -> Vec<String> {
        self.0.lock().expect("recording lock").clone()
    }
}

#[async_trait::async_trait]
impl Extract for Recording {
    async fn extract(&self, source: &Source) {
        self.0
            .lock()
            .expect("recording lock")
            .push(source.id.clone());
    }
}

/// The screen that holds everything, the way #50's will hold some things.
struct Hold;

#[async_trait::async_trait]
impl Screen for Hold {
    async fn screen(&self, _source: &SourceIngest) -> Verdict {
        Verdict::Hold
    }
}

/// Every `(table, column, value)` in the database: the whole surface body
/// text could leak through, walked.
async fn every_column(db: &Arc<dyn Database>) -> Vec<(String, String, String)> {
    let tables = db
        .query(&Statement::new(
            "SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name",
        ))
        .await
        .expect("the table list");
    let mut found = Vec::new();
    for name in tables
        .rows
        .iter()
        .filter_map(|row| row.get::<String>("name"))
    {
        // Table names come from the schema the module's own migrations
        // wrote; no caller input is in them to quote.
        let rows = db
            .query(&Statement::new(format!("SELECT * FROM {name}")))
            .await
            .unwrap_or_else(|error| panic!("{name} reads: {error}"));
        for row in &rows.rows {
            for (column, value) in row.columns() {
                found.push((name.clone(), column.to_owned(), format!("{value:?}")));
            }
        }
    }
    found
}

#[test]
fn the_same_message_twice_is_one_source() {
    let world = world();
    pollster::block_on(async {
        let ingestor = world.ingestor();

        let first = ingestor
            .ingest(chat("The window opens at 0300.\n"))
            .await
            .expect("the first ingest");
        assert_eq!(first.outcome, IngestOutcome::Created, "{:?}", first.outcome);

        let second = ingestor
            .ingest(chat("The window opens at 0300."))
            .await
            .expect("the second ingest");
        assert_eq!(second.outcome, IngestOutcome::Duplicate);
        assert_eq!(
            second.source.id, first.source.id,
            "the duplicate answers with the first row"
        );
        assert_eq!(world.rows().await, 1, "one message, one row");

        // And extraction was enqueued once, by the call that created the row.
        world.defer.drain().await;
        assert_eq!(
            world.extractor.seen(),
            vec![first.source.id],
            "one extraction, for the creating call"
        );
    });
}

#[test]
fn a_line_ending_and_a_trailing_newline_are_not_two_sources() {
    let world = world();
    pollster::block_on(async {
        let ingestor = world.ingestor();

        let first = ingestor
            .ingest(chat("alpha\nbravo"))
            .await
            .expect("the first ingest");
        for variant in [
            "alpha\r\nbravo",
            "alpha\nbravo\n",
            "\u{feff}alpha\r\nbravo\n\n",
        ] {
            let again = ingestor
                .ingest(chat(variant))
                .await
                .expect("a normalised ingest");
            assert_eq!(
                again.outcome,
                IngestOutcome::Duplicate,
                "`{variant}` is the same message normalised"
            );
        }
        assert_eq!(world.rows().await, 1, "normalised bodies share a hash");
        assert_eq!(first.source.body_sha256.len(), 64, "a hex sha256");
    });
}

#[test]
fn a_held_source_is_stored_but_never_extracted_until_released() {
    let world = world();
    pollster::block_on(async {
        let ingestor = Ingestor::new(world.store(), Arc::new(world.defer.clone()))
            .with_screen(Arc::new(Hold))
            .with_extractor(Arc::new(world.extractor.clone()));

        let ingested = ingestor
            .ingest(chat("hold this one back"))
            .await
            .expect("the held ingest");
        assert_eq!(ingested.outcome, IngestOutcome::Held);
        assert!(ingested.source.held, "the row carries the verdict");

        // The verdict is in the ledger, and the deferred work never ran.
        world.defer.drain().await;
        assert!(
            world.extractor.seen().is_empty(),
            "a held source is never extracted: {:?}",
            world.extractor.seen()
        );

        // A duplicate of a held source is a duplicate: the first verdict is
        // the row's verdict, and it enqueues nothing either.
        let again = ingestor
            .ingest(chat("hold this one back"))
            .await
            .expect("the duplicate ingest");
        assert_eq!(again.outcome, IngestOutcome::Duplicate);
        world.defer.drain().await;
        assert!(world.extractor.seen().is_empty(), "still nothing");

        // Release, and exactly one extraction is enqueued — the second
        // release releases nothing, so it must not queue a second.
        let released = ingestor
            .release("team", &ingested.source.id)
            .await
            .expect("a release");
        assert_eq!(
            released.as_ref().map(|source| source.id.as_str()),
            Some(ingested.source.id.as_str()),
            "the released row comes back"
        );
        let re_release = ingestor
            .release("team", &ingested.source.id)
            .await
            .expect("a second release");
        assert!(re_release.is_none(), "a second release releases nothing");

        world.defer.drain().await;
        assert_eq!(
            world.extractor.seen(),
            vec![ingested.source.id],
            "one extraction, after the release"
        );
    });
}

#[test]
fn a_body_lives_in_the_blob_store_and_in_no_database_column() {
    let world = world();
    pollster::block_on(async {
        let body = format!("the radar saw {MARKER} at dawn");
        let ingested = world
            .ingestor()
            .ingest(chat(&body))
            .await
            .expect("an ingest");

        // No column of any table carries the marker or any of the body.
        for (table, column, value) in every_column(&world.db).await {
            assert!(
                !value.contains(MARKER),
                "{table}.{column} carries the marker: {value}"
            );
            assert!(
                !value.contains("the radar saw"),
                "{table}.{column} carries body text: {value}"
            );
        }

        // The blob store has it, at the key the row names — sealed, so what
        // sits at that key is not the plaintext either.
        let sealed = world
            .blob
            .get(&ingested.source.body_key)
            .await
            .expect("a blob read")
            .expect("the sealed body is in the blob store");
        assert!(
            !sealed
                .bytes
                .windows(body.len())
                .any(|window| window == body.as_bytes()),
            "the blob holds an envelope, not the plaintext"
        );
        let opened = world
            .store()
            .open("team", &ingested.source.body_sha256)
            .await
            .expect("the body opens");
        assert_eq!(opened, body, "the sealed body opens to what arrived");
    });
}

#[test]
fn the_pipeline_redacts_before_anything_is_stored() {
    let world = world();
    pollster::block_on(async {
        let secret = "sk-ant-aaaabbbbccccddddeeee";
        let ingested = world
            .ingestor()
            .ingest(chat(&format!("the key is {secret}, rotate it")))
            .await
            .expect("an ingest");
        assert!(
            ingested.redactions >= 1,
            "the pipeline found the secret: {ingested:?}"
        );

        let opened = world
            .store()
            .open("team", &ingested.source.body_sha256)
            .await
            .expect("the body opens");
        assert!(
            !opened.contains(secret),
            "no secret in the ledger: {opened}"
        );
        assert!(
            opened.contains("[REDACTED:"),
            "the redaction token stands where the secret was: {opened}"
        );
    });
}

#[test]
fn the_pipeline_redacts_the_origin_ref_before_it_is_stored() {
    let world = world();
    pollster::block_on(async {
        // Free text is where secrets end up as readily as in a body: the
        // permalink a client sends goes through the same redaction the body
        // does, before the hash or the row.
        let secret = "sk-ant-aaaabbbbccccddddeeee";
        let mut arrived = chat("the message itself is clean");
        arrived.origin_ref = Some(format!("https://example.com/after?key={secret}"));
        let ingested = world.ingestor().ingest(arrived).await.expect("an ingest");
        assert!(
            ingested.redactions >= 1,
            "the pipeline found the secret: {ingested:?}"
        );

        let origin_ref = ingested.source.origin_ref.expect("an origin ref");
        assert!(
            !origin_ref.contains(secret),
            "no secret in the stored origin ref: {origin_ref}"
        );
        assert!(
            origin_ref.contains("[REDACTED:"),
            "the token stands where the secret was: {origin_ref}"
        );
    });
}

#[test]
fn a_source_the_screen_passed_travels_to_the_row_intact() {
    let world = world();
    pollster::block_on(async {
        let ingested = world
            .ingestor()
            .ingest(chat("plain sailing"))
            .await
            .expect("an ingest");
        assert_eq!(ingested.outcome, IngestOutcome::Created);
        assert!(!ingested.source.held);
        // Kind, origin and author reach the row untouched by the pipeline.
        assert_eq!(ingested.source.kind, SourceKind::Chat);
        assert_eq!(ingested.source.origin_ref.as_deref(), Some("msg_0001"));
        assert_eq!(ingested.source.author.as_deref(), Some("u1"));
        assert_eq!(ingested.source.workspace, "ws-one");
    });
}
