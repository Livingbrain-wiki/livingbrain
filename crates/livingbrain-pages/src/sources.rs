//! The source ledger: one row per distinct thing that arrived from outside
//! (issue #81, widened into the ingest ledger by issue #77).
//!
//! A source is anything that came in from outside — an Obsidian vault, a
//! documentation site, a checkout, a chat message, a mail — as opposed to a
//! [`crate::Page`], which this brain wrote. What makes it a ledger rather
//! than a second page store is one decision: **the body is the identity**.
//! The primary key is `(scope, body_sha256)`, so importing the same bytes
//! twice is one source whichever path they arrived under, and the second
//! import reads the first one's row back instead of sealing a second copy of
//! the same text.
//!
//! Everything else follows the page store (issue #43): a source body is sealed
//! under the scope's active data key, in the blob store, at a key carrying its
//! own id, and the row records the key version that opens it — so a source
//! written before a rotation still reads after one, and forgetting a scope
//! makes every body it sealed unreadable.
//!
//! The body is stored as the pipeline hands it over: [`crate::Ingestor`]
//! normalises, screens and redacts (issue #108), and the hash is of the
//! redacted text, so the same document redacted the same way twice is still
//! one row.

use std::collections::BTreeSet;
use std::sync::Arc;

use cratefield_core::{Blob, BlobError, Clock, Database, DbError, IdGen, Statement};
use cratefield_kms::{Kms, KmsError};
use sha2::{Digest, Sha256};
use time::format_description::well_known::Rfc3339;

use sea_query::Value as SeaValue;

use crate::entity;
use crate::keys::{self, ScopeKeys};
use crate::store::{int, text};

/// The largest JSON body a caller may hand the ledger, in bytes.
///
/// Twice `livingbrain_redact::MAX_INPUT_BYTES` (1 MiB), because a body of
/// quotes or control characters grows when it is escaped into the string the
/// request carries, plus the harness-wide 64 KiB for the object wrapped around
/// it. The redaction cap is spelled out rather than imported: the ledger is
/// handed a body that is *already* redacted and does not depend on the policy
/// crate, and the two must agree only in this number.
///
/// This is what a runtime reads as this module's coarse pre-buffer ceiling
/// ([`Module::max_body_bytes`]); the import route keeps its precise
/// `DefaultBodyLimit` on top of it, so raising the ceiling here never widens
/// any other route of this module.
pub const MAX_SOURCE_BODY_BYTES: usize = 2 * 1024 * 1024 + 64 * 1024;

/// The longest path an import may name, in bytes. A path is a name, not a
/// document: well past any real vault's depth, and well short of the body it
/// names.
const MAX_REL_PATH_LEN: usize = 1024;
/// The most `[[wiki-links]]` one imported file may name. A file past this is a
/// graph rather than a note, and the cap is what keeps a pasted page from
/// filling a row.
const MAX_WIKILINKS: usize = 256;
/// The longest a single link target may be, in bytes.
const MAX_LINK_LEN: usize = 512;
/// How much of the body's hash the blob key carries, so two files whose bodies
/// share a prefix still land on different keys.
const BODY_KEY_SHA_PREFIX: usize = 12;

/// How a source arrived. The set is closed: a kind is a schema-level fact
/// about where the source came from, not a string a caller invents — whatever
/// else arrives later is a variant here first, exactly as `import` was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// A message from a conversation the workspace had.
    Chat,
    /// A mail the workspace received.
    Mail,
    /// A log of what an agent did, kept for its own account.
    AgentLog,
    /// A file imported from outside the brain, path and body as it arrived.
    Import,
    /// A commit or diff that reached the brain from a repository.
    Git,
    /// A reading from the monitoring the workspace points at itself.
    Monitoring,
    /// A radar item: something spotted outside, filed before it is a page.
    Radar,
    /// A note the brain or a person wrote about what arrived.
    Note,
}

impl SourceKind {
    /// The stored wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Mail => "mail",
            Self::AgentLog => "agent_log",
            Self::Import => "import",
            Self::Git => "git",
            Self::Monitoring => "monitoring",
            Self::Radar => "radar",
            Self::Note => "note",
        }
    }

    /// Parses a stored wire name; `None` for anything else.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "chat" => Some(Self::Chat),
            "mail" => Some(Self::Mail),
            "agent_log" => Some(Self::AgentLog),
            "import" => Some(Self::Import),
            "git" => Some(Self::Git),
            "monitoring" => Some(Self::Monitoring),
            "radar" => Some(Self::Radar),
            "note" => Some(Self::Note),
            _ => None,
        }
    }
}

/// One write: where the source came from, what it says, and who sent it.
///
/// The body is **already redacted**. Nothing here re-checks it: the hash is of
/// what was handed over, and a caller that stored an unredacted body put a
/// secret in a blob store, which is the one mistake this table cannot undo.
/// The pipeline that redacts is [`crate::Ingestor`]; this is the shape it
/// hands the store once its own checks have run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceWrite {
    pub kind: SourceKind,
    /// The workspace the scope belongs to, plain — the scope itself only
    /// names it as a hash, and `GET /v1/sources/:id` needs the id back.
    pub workspace: String,
    /// Where the source came from, when it has one name: a permalink, a
    /// message id. Already redacted.
    pub origin_ref: Option<String>,
    /// The authoring member, when one is known; `None` when nobody the
    /// member table names wrote this (an imported file, for one).
    pub author: Option<String>,
    pub rel_path: String,
    /// The redacted Markdown body.
    pub markdown: String,
    /// The importer's user id.
    pub imported_by: String,
    /// Screening's verdict, landed as a flag: a held source is stored but
    /// never extracted until [`SourceStore::release`] clears it.
    pub held: bool,
}

/// One row of the ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub scope: String,
    /// The workspace the scope belongs to.
    pub workspace: String,
    pub id: String,
    pub kind: SourceKind,
    pub rel_path: String,
    /// Where the source came from, when it has one name.
    pub origin_ref: Option<String>,
    /// The authoring member, when one is known.
    pub author: Option<String>,
    /// Hex SHA-256 of the redacted body: the identity, and the key.
    pub body_sha256: String,
    /// The `[[wiki-links]]` the file names, in first-seen order.
    pub wikilinks: Vec<String>,
    /// The blob key the sealed body sits at.
    pub body_key: String,
    /// The scope key version the body is sealed under.
    pub key_version: u32,
    pub imported_by: String,
    /// Whether screening held this source: stored, never extracted, until
    /// [`SourceStore::release`] clears the flag.
    pub held: bool,
    pub created_at: String,
}

/// Everything an import can be refused for.
///
/// The key variants are the page store's own, carried through rather than
/// flattened: a scope whose key was shredded is not an infrastructure fault to
/// retry, it is a scope to stop writing to, and a caller that cannot tell those
/// apart retries the one that must not be retried.
#[derive(Debug)]
pub enum SourceError {
    /// A scope outside the slug rule.
    InvalidScope(String),
    /// A path that is not a relative name inside a vault.
    InvalidPath(String),
    /// A stored row that does not parse (a source kind, or the link JSON).
    Corrupt(String),
    /// The scope's key was shredded (`forget_scope`), so what it sealed is
    /// gone for good.
    Shredded(String),
    /// The key version a sealed body names is not live: never created, or
    /// retired before the body was re-sealed.
    KeyUnavailable { scope: String, version: u32 },
    /// A sealed body is not one of ours, or failed authentication.
    Crypto(String),
    /// The key custodian refused a wrap or an unwrap.
    Kms(KmsError),
    /// The blob store failed.
    Blob(BlobError),
    /// The database failed.
    Store(DbError),
}

impl SourceError {
    /// The variant's name, and nothing else. The one thing a failed route's
    /// operator log may carry: `Display` renders scopes, hashes and paths,
    /// and those belong in no log line a 500 produces.
    #[must_use]
    pub fn variant(&self) -> &'static str {
        match self {
            Self::InvalidScope(_) => "invalid-scope",
            Self::InvalidPath(_) => "invalid-path",
            Self::Corrupt(_) => "corrupt",
            Self::Shredded(_) => "shredded",
            Self::KeyUnavailable { .. } => "key-unavailable",
            Self::Crypto(_) => "crypto",
            Self::Kms(_) => "kms",
            Self::Blob(_) => "blob",
            Self::Store(_) => "store",
        }
    }
}

impl From<KmsError> for SourceError {
    fn from(error: KmsError) -> Self {
        Self::Kms(error)
    }
}

/// The key path is the page store's, so its failures arrive as [`PageError`]
/// and are carried through rather than flattened: a shredded scope and a
/// database fault must not become the same answer, because only one of them is
/// worth retrying. The variants an import cannot produce — frontmatter, entity
/// types, write conflicts — land in [`SourceError::Store`] rather than being
/// dropped, so nothing is ever silently discarded.
impl From<crate::PageError> for SourceError {
    fn from(error: crate::PageError) -> Self {
        match error {
            crate::PageError::InvalidScope(scope) => Self::InvalidScope(scope),
            crate::PageError::Shredded(scope) => Self::Shredded(scope),
            crate::PageError::KeyUnavailable { scope, version } => {
                Self::KeyUnavailable { scope, version }
            }
            crate::PageError::Crypto(message) => Self::Crypto(message),
            crate::PageError::Kms(error) => Self::Kms(error),
            crate::PageError::Blob(error) => Self::Blob(error),
            crate::PageError::Store(error) => Self::Store(error),
            other => Self::Store(DbError::Batch(format!(
                "the source store cannot use the page store's error: {other}"
            ))),
        }
    }
}

impl From<BlobError> for SourceError {
    fn from(error: BlobError) -> Self {
        Self::Blob(error)
    }
}

impl From<DbError> for SourceError {
    fn from(error: DbError) -> Self {
        Self::Store(error)
    }
}

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidScope(value) => {
                write!(
                    f,
                    "invalid scope `{value}`: lowercase ascii alnum and `-`, 1..=128 chars"
                )
            }
            Self::InvalidPath(value) => write!(f, "invalid source path `{value}`"),
            Self::Corrupt(message) => write!(f, "corrupt source row: {message}"),
            Self::Shredded(scope) => write!(
                f,
                "scope `{scope}` was forgotten: its key is destroyed and its bodies unreadable"
            ),
            Self::KeyUnavailable { scope, version } => {
                write!(f, "scope `{scope}` has no live key at version {version}")
            }
            Self::Crypto(message) => write!(f, "body encryption failed: {message}"),
            Self::Kms(error) => write!(f, "key custodian error: {error}"),
            Self::Blob(error) => write!(f, "blob error: {error}"),
            Self::Store(error) => write!(f, "store error: {error}"),
        }
    }
}

impl std::error::Error for SourceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Kms(error) => Some(error),
            Self::Blob(error) => Some(error),
            Self::Store(error) => Some(error),
            _ => None,
        }
    }
}

/// What [`SourceStore::release`] did about a held source.
///
/// Three answers, all of them final: the release that happened, the one that
/// had happened already, and the id that names nothing in that scope. A
/// caller that releases twice and counts two extractions is the mistake this
/// closed set makes unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Released {
    /// The flag was 1 and is now 0: the source is eligible for extraction.
    Now,
    /// The row exists and was already released; nothing changed.
    Already,
    /// No row in that scope carries that id.
    Missing,
}

/// The source ledger, over the page store's own ports.
pub struct SourceStore {
    db: Arc<dyn Database>,
    blob: Arc<dyn Blob>,
    keys: ScopeKeys,
    clock: Arc<dyn Clock>,
    id_gen: Arc<dyn IdGen>,
}

impl SourceStore {
    /// A store over the four ports it needs plus a key custodian — the same
    /// set [`crate::PageStore`] takes, so a composition that has one has both.
    #[must_use]
    pub fn new(
        db: Arc<dyn Database>,
        blob: Arc<dyn Blob>,
        kms: Arc<dyn Kms>,
        clock: Arc<dyn Clock>,
        id_gen: Arc<dyn IdGen>,
    ) -> Self {
        Self {
            keys: ScopeKeys::new(db.clone(), kms, clock.clone()),
            db,
            blob,
            clock,
            id_gen,
        }
    }

    /// Records one source, returning the row and whether this call is
    /// what created it.
    ///
    /// The row is looked up **before** the body is sealed, so the ordinary
    /// re-import writes nothing at all: no second envelope, no second blob
    /// key, no id spent. The `ON CONFLICT` below is the race guard for the
    /// one case the lookup cannot see — two importers that read "no row"
    /// together — and the loser deletes the body it sealed rather than leaving
    /// an orphan nothing will ever read.
    ///
    /// Every way this can fail *after* the blob is written takes the blob with
    /// it. An envelope whose row was never inserted is ciphertext under a key
    /// nobody is going to look it up with: it costs storage forever and can
    /// never be erased by `forget_scope`, which only unlinks the rows it knows
    /// about. The delete is best effort — the error the caller hears is the
    /// one that actually failed, not a cleanup that followed it.
    ///
    /// # Errors
    ///
    /// [`SourceError::InvalidScope`], [`SourceError::InvalidPath`], and
    /// whatever the key, blob and database paths can fail with.
    pub async fn put(
        &self,
        scope: &str,
        write: SourceWrite,
    ) -> Result<(Source, bool), SourceError> {
        if !entity::is_slug(scope) {
            return Err(SourceError::InvalidScope(scope.to_owned()));
        }
        // The path rule is the vault's rule, so it binds the kinds that
        // arrive as files. A chat message has no path; the column stays
        // for the import's filing and the empty string for the rest.
        if write.kind == SourceKind::Import {
            check_rel_path(&write.rel_path)?;
        }

        let digest = keys::hex(&Sha256::digest(write.markdown.as_bytes()));
        if let Some(existing) = self.row(scope, &digest).await? {
            return Ok((existing, false));
        }

        let wikilinks = extract_wikilinks(&write.markdown);
        let id = self.id_gen.ulid();
        // The key carries a fresh id beside the hash, exactly as a page body's
        // does: a racing loser seals to a key of its own, so the body it wrote
        // cannot overwrite the winner's.
        let body_key = format!(
            "{scope}/sources/{}-{id}.md",
            &digest[..BODY_KEY_SHA_PREFIX.min(digest.len())]
        );
        // Sealed before the put, so a blob is never a plaintext a reader might
        // catch before the key arrives.
        let active = self.keys.active(scope).await?;
        let envelope = keys::seal(
            &active.dek,
            active.version,
            &keys::body_aad(scope, &body_key, active.version),
            write.markdown.as_bytes(),
        )?;
        let key_version = active.version;
        drop(active);
        self.blob
            .put(&body_key, &envelope, keys::SEALED_CONTENT_TYPE)
            .await?;

        let now = self.now();
        let inserted = self
            .db
            .execute(&Statement::with_values(
                "INSERT INTO sources \
                 (scope, body_sha256, id, kind, rel_path, wikilinks, body_key, key_version, \
                  imported_by, created_at, workspace, origin_ref, author, held) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT (scope, body_sha256) DO NOTHING",
                vec![
                    text(scope),
                    text(&digest),
                    text(&id),
                    text(write.kind.as_str()),
                    text(&write.rel_path),
                    text(&links_json(&wikilinks)),
                    text(&body_key),
                    int(key_version),
                    text(&write.imported_by),
                    text(&now),
                    text(&write.workspace),
                    opt_text(&write.origin_ref),
                    opt_text(&write.author),
                    int(u32::from(write.held)),
                ],
            ))
            .await;
        if let Err(error) = inserted {
            self.discard(&body_key).await;
            return Err(error.into());
        }

        let row = match self.row(scope, &digest).await? {
            Some(row) => row,
            None => {
                // The insert claimed to succeed and there is still no row. The
                // body is sealed and nothing names it, so it goes with the
                // error rather than after it.
                self.discard(&body_key).await;
                return Err(SourceError::Corrupt(format!(
                    "a source row for `{digest}` was neither there before the insert nor after it"
                )));
            }
        };
        if row.id != id {
            // Somebody else committed the same body between the read above and
            // the insert. Their row is the one that counts; this envelope has
            // no row pointing at it and is not kept.
            self.discard(&body_key).await;
        }
        let created = row.id == id;
        Ok((row, created))
    }

    /// Unlinks a body nothing points at, ignoring whether it succeeds.
    ///
    /// Every caller is already returning the failure that got it here, and a
    /// cleanup that failed must not replace it: the caller has to hear that
    /// the import did not happen, not that the tidy-up did not either.
    async fn discard(&self, body_key: &str) {
        let _ = self.blob.delete(body_key).await;
    }

    /// The redacted Markdown of the source a body hash names, read back from the
    /// blob store and opened with the key version its row records.
    ///
    /// The hash is the identity, so it is also the name: a caller reads a
    /// source by the same value it wrote it under and does not have to be
    /// handed a [`Source`] row first. A hash no row names is
    /// [`SourceError::Corrupt`] — the ledger's one name for a body it cannot
    /// account for.
    ///
    /// # Errors
    ///
    /// [`SourceError::InvalidScope`] for a scope outside the slug rule,
    /// [`SourceError::Corrupt`] when the hash names no row or the row points
    /// at a body that is not there, [`SourceError::Blob`] if the store fails,
    /// and the key variants from [`SourceStore::put`].
    pub async fn open(&self, scope: &str, body_sha256: &str) -> Result<String, SourceError> {
        if !entity::is_slug(scope) {
            return Err(SourceError::InvalidScope(scope.to_owned()));
        }
        let source = self
            .row(scope, body_sha256)
            .await?
            .ok_or_else(|| SourceError::Corrupt(format!("no source row for `{body_sha256}`")))?;
        let Some(object) = self.blob.get(&source.body_key).await? else {
            return Err(SourceError::Corrupt(format!(
                "source `{}` has no body at `{}`",
                source.id, source.body_key
            )));
        };
        let dek = self.keys.dek(&source.scope, source.key_version).await?;
        let plaintext = keys::open(
            &dek,
            source.key_version,
            &keys::body_aad(&source.scope, &source.body_key, source.key_version),
            &object.bytes,
        )?;
        Ok(String::from_utf8_lossy(&plaintext).into_owned())
    }

    /// The row for one body hash, or `None`.
    async fn row(&self, scope: &str, digest: &str) -> Result<Option<Source>, SourceError> {
        let rows = self
            .db
            .query(&Statement::with_values(
                "SELECT scope, body_sha256, id, kind, rel_path, wikilinks, body_key, \
                        key_version, imported_by, created_at, workspace, origin_ref, author, \
                        held \
                 FROM sources WHERE scope = ? AND body_sha256 = ?",
                vec![text(scope), text(digest)],
            ))
            .await?;
        rows.first().map(Self::map_row).transpose()
    }

    /// The row one id names, whichever scope it landed in, or `None`.
    ///
    /// Finding a row says nothing about who may read it — the caller checks
    /// the workspace and the scope against the asker, and an id outside both
    /// is answered exactly like an id that is not there.
    ///
    /// # Errors
    ///
    /// [`SourceError::Corrupt`] for a row that does not parse, and whatever
    /// the database path can fail with.
    pub async fn by_id(&self, id: &str) -> Result<Option<Source>, SourceError> {
        let rows = self
            .db
            .query(&Statement::with_values(
                "SELECT scope, body_sha256, id, kind, rel_path, wikilinks, body_key, \
                        key_version, imported_by, created_at, workspace, origin_ref, author, \
                        held \
                 FROM sources WHERE id = ? LIMIT 1",
                vec![text(id)],
            ))
            .await?;
        rows.first().map(Self::map_row).transpose()
    }

    /// Clears a held source's flag in `scope`, and says what happened.
    ///
    /// The `WHERE held = 1` is what makes the answer honest under a double
    /// release: exactly one caller sees [`Released::Now`], and the extraction
    /// that follows a release happens once, whoever calls first.
    ///
    /// # Errors
    ///
    /// [`SourceError::InvalidScope`] for a scope outside the slug rule, and
    /// whatever the database path can fail with.
    pub async fn release(&self, scope: &str, id: &str) -> Result<Released, SourceError> {
        if !entity::is_slug(scope) {
            return Err(SourceError::InvalidScope(scope.to_owned()));
        }
        let updated = self
            .db
            .execute(&Statement::with_values(
                "UPDATE sources SET held = 0 WHERE scope = ? AND id = ? AND held = 1",
                vec![text(scope), text(id)],
            ))
            .await?;
        if updated > 0 {
            return Ok(Released::Now);
        }
        // Nothing updated: either the id names nothing here, or it names a
        // source that was never held or was released before this call — the
        // update only passes over rows still at held = 1.
        match self.by_id(id).await? {
            Some(row) if row.scope == scope => Ok(Released::Already),
            _ => Ok(Released::Missing),
        }
    }

    /// One ledger row out of one database row.
    fn map_row(row: &cratefield_core::Row) -> Result<Source, SourceError> {
        let raw = row
            .get::<String>("kind")
            .ok_or_else(|| SourceError::Corrupt("sources.kind is not text".to_owned()))?;
        let kind = SourceKind::parse(&raw)
            .ok_or_else(|| SourceError::Corrupt(format!("unknown source kind `{raw}`")))?;
        let links = row
            .get::<String>("wikilinks")
            .unwrap_or_else(|| "[]".to_owned());
        let wikilinks: Vec<String> = serde_json::from_str(&links).map_err(|_| {
            SourceError::Corrupt("sources.wikilinks is not a JSON array".to_owned())
        })?;
        Ok(Source {
            scope: row.get::<String>("scope").unwrap_or_default(),
            workspace: row.get::<String>("workspace").unwrap_or_default(),
            id: row.get::<String>("id").unwrap_or_default(),
            kind,
            rel_path: row.get::<String>("rel_path").unwrap_or_default(),
            origin_ref: row.get::<String>("origin_ref"),
            author: row.get::<String>("author"),
            body_sha256: row.get::<String>("body_sha256").unwrap_or_default(),
            wikilinks,
            body_key: row.get::<String>("body_key").unwrap_or_default(),
            key_version: row.get::<u32>("key_version").unwrap_or_default(),
            imported_by: row.get::<String>("imported_by").unwrap_or_default(),
            held: row.get::<u32>("held").unwrap_or_default() != 0,
            created_at: row.get::<String>("created_at").unwrap_or_default(),
        })
    }

    /// The current time as RFC 3339, as everywhere else in the module.
    fn now(&self) -> String {
        self.clock.now().format(&Rfc3339).unwrap_or_default()
    }
}

/// The path rule: a relative name inside a vault. It is checked before any
/// blob work, and it is deliberately stricter than "not absolute" — a path
/// that walks out of the vault is not a path in it, whichever platform would
/// have resolved it.
fn check_rel_path(path: &str) -> Result<(), SourceError> {
    let refused = || SourceError::InvalidPath(path.to_owned());
    if path.is_empty() || path.len() > MAX_REL_PATH_LEN {
        return Err(refused());
    }
    if path.contains('\0') || path.contains('\\') || path.starts_with('/') {
        return Err(refused());
    }
    if path
        .split('/')
        .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(refused());
    }
    Ok(())
}

/// The `[[wiki-links]]` a body names, Obsidian style and **leniently**.
///
/// Unlike [`crate::extract_links`], a target here is whatever the file wrote:
/// `[[My Note]]`, `[[My Note|an alias]]`, `[[folder/Other#a heading]]` and
/// `![[an embed]]` all name something, and none of them has to be a slug a
/// page could carry. A vault is not a wiki this brain wrote, and refusing an
/// import over one link to a note with a space in its name would refuse the
/// file.
///
/// A target is kept as written and trimmed, with the alias and the heading
/// stripped; empties are skipped, duplicates dropped in first-seen order, and
/// both counts capped so a pasted page cannot make a row grow without bound.
#[must_use]
pub fn extract_wikilinks(markdown: &str) -> Vec<String> {
    let mut links: Vec<String> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut rest = markdown;
    while let Some(start) = rest.find("[[") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("]]") else {
            break;
        };
        let inner = &after[..end];
        // The alias is whatever follows a `|`; a heading may sit before it, so
        // the alias comes off first and the heading off what is left.
        let target = inner.split('|').next().unwrap_or(inner);
        let target = target.split('#').next().unwrap_or(target).trim();
        rest = &after[end + 2..];
        if target.is_empty() || target.len() > MAX_LINK_LEN {
            continue;
        }
        if seen.insert(target.to_owned()) {
            links.push(target.to_owned());
            if links.len() >= MAX_WIKILINKS {
                break;
            }
        }
    }
    links
}

/// The `wikilinks` column: a JSON array, the shape a reader parses without a
/// schema migration. The fallback is the empty array rather than a lossy
/// string — a row whose links did not serialise says nothing, which is a
/// better record than a half-written one.
fn links_json(links: &[String]) -> String {
    serde_json::to_string(links).unwrap_or_else(|_| "[]".to_owned())
}

/// A nullable text column: `NULL` for `None`, the text otherwise.
fn opt_text(value: &Option<String>) -> SeaValue {
    SeaValue::String(value.as_ref().map(|value| Box::new(value.clone())))
}
