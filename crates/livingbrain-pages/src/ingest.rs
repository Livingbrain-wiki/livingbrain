//! The one ingest pipeline (issue #77): normalise -> screen -> redact ->
//! dedupe -> store -> enqueue extraction.
//!
//! Every kind of source goes through this one path, and the path is a struct,
//! [`Ingestor`], so a composition assembles it once out of the store, the
//! defer port and its two hooks:
//!
//! - **[`Screen`]** — the moderation hook issue #50 will plug in. Until then
//!   [`NoScreen`] passes everything through; a screen that answers
//!   [`Verdict::Hold`] lands the source in the ledger with `held` set, and a
//!   held source is never extracted until [`Ingestor::release`] clears it.
//! - **[`Extract`]** — what turns a stored source into entities, links and
//!   pages. No extractor exists yet, so [`NoExtract`] is a no-op and the
//!   enqueue is the contract: the pipeline defers one call per newly stored,
//!   unheld source, on the `Defer` port, the way the workspaces webhook
//!   defers its dispatch. Releasing a held source defers one then.
//!
//! The body is normalised before anything else — CRLF and lone CR to LF, the
//! BOM dropped, trailing whitespace trimmed — so the same message with a
//! different line ending or a trailing newline hashes the same and the ledger
//! keeps one row, not three. Redaction runs **after** screening (a screen
//! sees the text as it arrived) and **before** the hash, on the body and on
//! the free-text metadata, so nothing the pipeline stores carries a secret.
//!
//! A source that dedupes to an existing row changes nothing: the first
//! verdict is the row's verdict, and extraction is enqueued for a row only
//! by the call that created it.

use std::sync::Arc;

use cratefield_core::Defer;
use livingbrain_redact::{Policy, RedactError, redact};

use crate::sources::{Released, Source, SourceError, SourceKind, SourceStore, SourceWrite};

/// What a screen says about a source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Store it and, when it is new, extract it like any other.
    Pass,
    /// Store it, but never extract it until it is released.
    Hold,
}

/// The moderation hook (issue #50's seam): a source goes past one before it
/// is stored, and a `Hold` parks it in the ledger unextracted.
///
/// The source a screen sees is the **normalised, unredacted** text — the
/// pipeline has not redacted yet, because what a screen judges is what
/// arrived. An implementation holds the text it is handed to itself; the
/// store will only ever see the redacted body.
#[async_trait::async_trait]
pub trait Screen: Send + Sync {
    async fn screen(&self, source: &SourceIngest) -> Verdict;
}

/// The screen every pipeline has until #50 plugs a real one in: everything
/// passes.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoScreen;

#[async_trait::async_trait]
impl Screen for NoScreen {
    async fn screen(&self, _source: &SourceIngest) -> Verdict {
        Verdict::Pass
    }
}

/// The extraction hook: one call per source the pipeline stored unheld.
///
/// There is no real extractor yet, so the only implementation is
/// [`NoExtract`]; the contract this trait pins is the *shape* — the stored
/// source's identity (id, scope, hash, sealed body), handed over after the
/// row is committed, once per source.
#[async_trait::async_trait]
pub trait Extract: Send + Sync {
    async fn extract(&self, source: &Source);
}

/// The extractor every pipeline has until a real one exists: nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoExtract;

#[async_trait::async_trait]
impl Extract for NoExtract {
    async fn extract(&self, _source: &Source) {}
}

/// What arrives at the pipeline. The body and the free-text metadata are
/// **raw** — redacting them is the pipeline's job, not the caller's, so a
/// caller that skipped it (issue #108) lands the same ledger as one that
/// did not.
#[derive(Debug, Clone)]
pub struct SourceIngest {
    pub kind: SourceKind,
    /// The workspace the source belongs to, plain — the scope only names it
    /// as a hash.
    pub workspace: String,
    /// The folded page scope the source lands in. The caller's, decided the
    /// way every route decides it: folded from the credential, checked
    /// against the grant.
    pub scope: String,
    /// Where the source came from, when it has one name: a permalink, a
    /// message id. Free text, so the pipeline redacts it.
    pub origin_ref: Option<String>,
    /// The authoring member, when one is known. An id, not free text — it
    /// travels as it arrived.
    pub author: Option<String>,
    pub rel_path: String,
    /// The importer's user id.
    pub imported_by: String,
    /// The raw body.
    pub body: String,
}

/// What one ingest did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestOutcome {
    /// The call created the row and it is not held: extraction is enqueued.
    Created,
    /// The body is already in the ledger under this scope: one row, and the
    /// call enqueued nothing.
    Duplicate,
    /// The call created the row and screening held it: stored, never
    /// extracted, until it is released.
    Held,
}

/// One ingest, answered.
#[derive(Debug, Clone)]
pub struct Ingested {
    /// The row: the one this call created, or the one it found.
    pub source: Source,
    pub outcome: IngestOutcome,
    /// How many redactions the pipeline made getting there, across the body
    /// and the free-text metadata.
    pub redactions: usize,
}

/// Why an ingest did not happen.
///
/// [`IngestError::TooLarge`] and [`IngestError::Refused`] are the redaction
/// cap and a text redaction would not handle: nothing was stored, and the
/// caller can act on both. The rest are the store's own, carried through
/// rather than flattened.
#[derive(Debug)]
pub enum IngestError {
    /// A scope outside the slug rule.
    InvalidScope(String),
    /// A path that is not a relative name inside a vault.
    InvalidPath(String),
    /// The text is over what redaction will read.
    TooLarge,
    /// Text redaction refused to handle, and nothing was stored.
    Refused(String),
    /// The store refused or failed.
    Store(SourceError),
}

impl From<RedactError> for IngestError {
    fn from(error: RedactError) -> Self {
        match error {
            RedactError::TooLarge { .. } => Self::TooLarge,
            other => Self::Refused(other.to_string()),
        }
    }
}

impl From<SourceError> for IngestError {
    fn from(error: SourceError) -> Self {
        Self::Store(error)
    }
}

impl std::fmt::Display for IngestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidScope(value) => {
                write!(
                    f,
                    "invalid scope `{value}`: lowercase ascii alnum and `-`, 1..=128 chars"
                )
            }
            Self::InvalidPath(value) => write!(f, "invalid source path `{value}`"),
            Self::TooLarge => write!(f, "the text is over what redaction will read"),
            Self::Refused(message) => write!(f, "redaction refused the text: {message}"),
            Self::Store(error) => write!(f, "store error: {error}"),
        }
    }
}

impl std::error::Error for IngestError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            _ => None,
        }
    }
}

impl IngestError {
    /// The variant's name, and nothing else — [`SourceError::variant`]'s
    /// discipline carried up one layer, since [`Display`](std::fmt::Display)
    /// here renders the refused scope or path. The store's own variant
    /// names stand when the failure is the store's.
    #[must_use]
    pub fn variant(&self) -> &'static str {
        match self {
            Self::InvalidScope(_) => "invalid-scope",
            Self::InvalidPath(_) => "invalid-path",
            Self::TooLarge => "too-large",
            Self::Refused(_) => "not-redactable",
            Self::Store(error) => error.variant(),
        }
    }
}

/// The one pipeline: a [`SourceStore`], the `Defer` port extractions are
/// enqueued on, and the two hooks. Assembled once per composition or per
/// request, out of the ports the module already holds.
pub struct Ingestor {
    store: SourceStore,
    defer: Arc<dyn Defer>,
    screen: Arc<dyn Screen>,
    extract: Arc<dyn Extract>,
}

impl Ingestor {
    /// A pipeline over one store and one defer port, with the default hooks:
    /// everything passes, nothing is extracted.
    #[must_use]
    pub fn new(store: SourceStore, defer: Arc<dyn Defer>) -> Self {
        Self {
            store,
            defer,
            screen: Arc::new(NoScreen),
            extract: Arc::new(NoExtract),
        }
    }

    /// Screens every source through `screen` before it is stored.
    #[must_use]
    pub fn with_screen(mut self, screen: Arc<dyn Screen>) -> Self {
        self.screen = screen;
        self
    }

    /// Hands every newly stored, unheld source to `extract`, deferred.
    #[must_use]
    pub fn with_extractor(mut self, extract: Arc<dyn Extract>) -> Self {
        self.extract = extract;
        self
    }

    /// Runs one source through the pipeline.
    ///
    /// The order is the ledger's contract: **normalise**, so the same text
    /// with different line endings or a trailing newline is one source;
    /// **screen**, so a verdict lands before anything is stored; **redact**
    /// the body and the free-text metadata, so nothing secret reaches a
    /// hash, a row or a blob; **store** under `(scope, hash)`, which is the
    /// dedupe; and, for a source that was just created and not held,
    /// **enqueue** one extraction on the defer port. A duplicate changes
    /// nothing — not the row's verdict, not the extraction count.
    ///
    /// # Errors
    ///
    /// [`IngestError::InvalidScope`], [`IngestError::InvalidPath`], the
    /// redaction pair, and whatever the store can fail with.
    pub async fn ingest(&self, mut source: SourceIngest) -> Result<Ingested, IngestError> {
        source.body = normalise(&source.body);
        let held = self.screen.screen(&source).await == Verdict::Hold;

        let (markdown, mut redactions) = redacted(&source.body)?;
        let origin_ref = match &source.origin_ref {
            Some(raw) => {
                let (clean, found) = redacted(raw)?;
                redactions += found;
                Some(clean)
            }
            None => None,
        };
        let (rel_path, found) = redacted(&source.rel_path)?;
        redactions += found;

        let (stored, created) = self
            .store
            .put(
                &source.scope,
                SourceWrite {
                    kind: source.kind,
                    workspace: source.workspace,
                    origin_ref,
                    author: source.author,
                    rel_path,
                    markdown,
                    imported_by: source.imported_by,
                    held,
                },
            )
            .await
            // The vault's rule lives in the store, but a caller of the
            // pipeline reads one vocabulary: its scope and path refusals are
            // the pipeline's own two variants, not a store fault.
            .map_err(|error| match error {
                SourceError::InvalidScope(scope) => IngestError::InvalidScope(scope),
                SourceError::InvalidPath(path) => IngestError::InvalidPath(path),
                other => IngestError::Store(other),
            })?;

        let outcome = match (created, held) {
            (false, _) => IngestOutcome::Duplicate,
            (true, true) => IngestOutcome::Held,
            (true, false) => IngestOutcome::Created,
        };
        if created && !held {
            self.enqueue(&stored);
        }
        Ok(Ingested {
            source: stored,
            outcome,
            redactions,
        })
    }

    /// Releases a held source in `scope`, and enqueues its extraction.
    ///
    /// `None` when nothing was released: no row in that scope carries that
    /// id, or it was not held (or not any more). Only [`Released::Now`]
    /// enqueues — a second release must not become a second extraction.
    ///
    /// # Errors
    ///
    /// Whatever the store can fail with.
    pub async fn release(&self, scope: &str, id: &str) -> Result<Option<Source>, IngestError> {
        if !matches!(
            self.store
                .release(scope, id)
                .await
                .map_err(|error| match error {
                    SourceError::InvalidScope(scope) => IngestError::InvalidScope(scope),
                    other => IngestError::Store(other),
                })?,
            Released::Now
        ) {
            return Ok(None);
        }
        let source = self.store.by_id(id).await?.ok_or_else(|| {
            IngestError::Store(SourceError::Corrupt(format!(
                "source `{id}` was released and is now gone"
            )))
        })?;
        self.enqueue(&source);
        Ok(Some(source))
    }

    /// Defers one extraction: the hook runs after the response, on a future
    /// that owns its own clone of the row and the hook.
    fn enqueue(&self, source: &Source) {
        let extract = Arc::clone(&self.extract);
        let source = source.clone();
        self.defer
            .wait_until(Box::pin(async move { extract.extract(&source).await }));
    }
}

/// Redacts one piece of free text, dropping the findings — the pipeline's
/// caller learns only how many there were.
///
/// # Errors
///
/// [`IngestError::TooLarge`] past redaction's cap, [`IngestError::Refused`]
/// for a text redaction will not handle.
fn redacted(raw: &str) -> Result<(String, usize), IngestError> {
    let (clean, findings) = redact(raw, Policy::Redact)?;
    Ok((clean, findings.len()))
}

/// The pipeline's normal form: the BOM gone, every CRLF and lone CR a plain
/// LF, trailing whitespace trimmed off the end.
///
/// Deliberately small and total: the same message must arrive at the same
/// bytes — and so at the same hash, the same row — whichever client sent it
/// or how its transport mangled the line endings, and nothing here may be
/// clever enough to make two different messages collide.
fn normalise(body: &str) -> String {
    let body = body.strip_prefix('\u{feff}').unwrap_or(body);
    let body = body.replace("\r\n", "\n").replace('\r', "\n");
    body.trim_end().to_owned()
}
