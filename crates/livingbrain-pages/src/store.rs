//! The page store: versioned entities in D1, sealed Markdown bodies in the
//! blob store. The module rules it enforces are in [`crate`].
//!
//! A body is not stored as Markdown: it is an envelope (see [`keys`]) opened by
//! a per-scope data key held only wrapped in D1, so reading or searching for
//! one is also a KMS round trip — which is what buys [`PageStore::forget_scope`].

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use cratefield_core::{Blob, BlobError, Clock, Database, DbError, IdGen, Row, Statement};
use cratefield_kms::{Kms, KmsError};
use sea_query::Value as SeaValue;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use base64::Engine as _;
use cratefield_kms::Dek;
use livingbrain_d1::{BIND_BUDGET, pack};

use crate::access::ScopeFilter;
use crate::entity::{self, EntityType, Frontmatter};
use crate::keys::{self, ScopeKeys};
use crate::titles;

/// The AAD domain a page title is sealed under (`pages.title_sealed`).
const TITLE_AAD_DOMAIN: &str = "livingbrain-pages/title/v1";

/// The AAD a title of `slug` in `scope`, sealed under `version`, is bound to
/// — so a sealed title cannot be moved to another page or scope.
fn title_aad(scope: &str, slug: &str, version: u32) -> String {
    format!("{TITLE_AAD_DOMAIN}\0{scope}\0{slug}\0{version}")
}

/// A title sealed for its `pages` row, base64.
fn seal_title(
    dek: &Dek,
    version: u32,
    scope: &str,
    slug: &str,
    title: &str,
) -> Result<String, PageError> {
    let envelope = keys::seal(
        dek,
        version,
        &title_aad(scope, slug, version),
        title.as_bytes(),
    )?;
    Ok(base64::engine::general_purpose::STANDARD.encode(envelope))
}

/// A sealed title opened, or `None` when it does not open.
fn open_title(dek: &Dek, version: u32, scope: &str, slug: &str, sealed: &str) -> Option<String> {
    let envelope = base64::engine::general_purpose::STANDARD
        .decode(sealed)
        .ok()?;
    let plain = keys::open(dek, version, &title_aad(scope, slug, version), &envelope).ok()?;
    String::from_utf8(plain)
        .ok()
        .filter(|title| !title.is_empty())
}

/// How long a key version has to have been superseded before a re-encryption
/// pass will destroy it. A Worker request commits its batch in seconds, and the
/// guard inside the retirement statement is what actually makes it safe.
const RETIRE_GRACE: time::Duration = time::Duration::minutes(15);

/// Who authored a version. Stored as a string in `page_versions`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorKind {
    Human,
    Brain,
}

impl AuthorKind {
    /// The stored wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Brain => "brain",
        }
    }

    /// Parses a stored wire name.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "human" => Some(Self::Human),
            "brain" => Some(Self::Brain),
            _ => None,
        }
    }
}

/// Who is writing a page.
///
/// A brain write's `reconciles` is the pending human version the writer
/// **asserts** its body already incorporates. The store accepts it only when
/// that value names the page's current pending human version, so a write that
/// merely claims to have reconciled cannot be mistaken for one that has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Author {
    /// A person, by their id.
    Human { id: String },
    /// The brain; `reconciles` names the pending human version it has seen.
    Brain { reconciles: Option<u32> },
}

impl Author {
    /// The stored kind.
    #[must_use]
    pub fn kind(&self) -> AuthorKind {
        match self {
            Self::Human { .. } => AuthorKind::Human,
            Self::Brain { .. } => AuthorKind::Brain,
        }
    }

    /// The stored author label: the human's id, or `brain`.
    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            Self::Human { id } => id,
            Self::Brain { .. } => "brain",
        }
    }
}

/// One write: the whole new body, typed, by someone, based on a version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageWrite {
    pub entity_type: EntityType,
    pub markdown: String,
    pub author: Author,
    /// `None` creates a new page; `Some` edits an existing one.
    pub base_version: Option<u32>,
}

/// One version's metadata, without its body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionMeta {
    pub version: u32,
    pub author_kind: AuthorKind,
    pub author: String,
    pub base_version: Option<u32>,
    pub created_at: String,
}

/// A page version with its body: the head from [`PageStore::read`], or any
/// version from [`PageStore::read_version`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub scope: String,
    pub slug: String,
    pub entity_type: EntityType,
    pub version: u32,
    pub markdown: String,
    pub frontmatter: Frontmatter,
    pub links: Vec<String>,
}

/// One page a [`PageStore::search`] matched, in a scope the asker named.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SearchHit {
    pub scope: String,
    pub slug: String,
}

/// One scored candidate, before the cross-scope merge.
struct Ranked {
    scope: String,
    slug: String,
    score: i64,
    updated_at: String,
}

/// How many times a word in a page's title or slug outweighs the same word
/// in its body.
const TITLE_BOOST: i64 = 3;

/// A word's weight, in thousandths: BM25's inverse document frequency,
/// `ln(1 + (N - df + 0.5) / (df + 0.5))`, for a word on `df` of `total`
/// pages. Never below one, so a word on every page still counts.
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
fn idf_weight(total: i64, df: i64) -> i64 {
    let (n, df) = (total as f64, df.clamp(0, total) as f64);
    let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
    ((idf * 1000.0).round() as i64).max(1)
}

/// One page's head metadata, without its body: what [`PageStore::list`]
/// hands back for the pages the asker's scopes hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageSummary {
    pub scope: String,
    pub slug: String,
    pub entity_type: EntityType,
    /// The page's head version.
    pub version: u32,
    /// When the head was written, RFC 3339 — the storage format for every
    /// timestamp.
    pub updated_at: String,
    /// The page's title (its frontmatter `name`, `title` or `term`), opened
    /// from the row's sealed copy; `None` for a page with no title, or one
    /// written before titles were kept and not yet backfilled.
    pub title: Option<String>,
}

/// Which pages a [`PageStore::list_filtered`] answers with. The default is
/// every page.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PageFilter {
    /// Only pages of these types; empty is every type.
    pub types: Vec<EntityType>,
    /// Never pages of these types.
    pub exclude: Vec<EntityType>,
    /// Only the compiled coding-agents pages of this project key
    /// ([`crate::project_key`]): its folder under every agent, and its
    /// cross-agent rollup.
    pub project: Option<String>,
}

/// A page whose title the backfill has not set, or a session log still
/// filed as a decision: what [`PageStore::title_candidates`] hands back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TitleCandidate {
    pub scope: String,
    pub slug: String,
    pub entity_type: EntityType,
}

/// What a [`PageStore::reencrypt_scope`] pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReencryptReport {
    /// Bodies re-sealed under the active key.
    pub rewritten: usize,
    /// Key versions retired because nothing refers to them any more.
    pub retired: Vec<u32>,
    /// Key versions still referred to by a body, and therefore not retired
    /// yet. Empty when the rotation is finished.
    pub pending: Vec<u32>,
}

/// Everything a page write can be refused for.
#[derive(Debug)]
pub enum PageError {
    /// A slug outside the slug rule.
    InvalidSlug(String),
    /// A scope outside the slug rule.
    InvalidScope(String),
    /// Frontmatter that does not match the entity type.
    Frontmatter(String),
    /// A `[[link]]` whose target is not a slug.
    InvalidLink(String),
    /// A `type:` frontmatter key that disagrees with the entity type.
    TypeMismatch {
        declared: EntityType,
        found: EntityType,
    },
    /// A page cannot change its entity type.
    EntityTypeChanged { from: EntityType, to: EntityType },
    /// A `base_version` supplied for a page that does not exist yet.
    NewPageWithBase { base: u32 },
    /// An edit of an existing page with no `base_version`.
    MissingBaseVersion,
    /// The base is ahead of `head`, or a write would overwrite a version it
    /// did not see. `base` is `0` when a page was created concurrently.
    Conflict { base: u32, head: u32 },
    /// The brain tried to write without reconciling a pending human edit.
    HumanEditPending { version: u32 },
    /// A stored row that does not parse (an entity type or author kind).
    Corrupt(String),
    /// The scope's key was shredded (`forget_scope`), so the body it sealed
    /// is gone for good. Distinct from an infrastructure failure because the
    /// right answer is to stop asking, not to retry.
    Shredded(String),
    /// The key version an envelope names is not live: never created, or
    /// retired before the body was re-sealed.
    KeyUnavailable { scope: String, version: u32 },
    /// A rotation is still in flight for this scope, so it cannot start
    /// another until `reencrypt_scope` has finished the one it has.
    RotationPending { scope: String, superseded: u32 },
    /// A sealed body is not one of ours, or failed authentication — the wrong
    /// key, the wrong scope, or altered bytes.
    Crypto(String),
    /// The key custodian refused a wrap or an unwrap.
    Kms(KmsError),
    /// The blob store failed.
    Blob(BlobError),
    /// The database failed.
    Store(DbError),
}

impl std::fmt::Display for PageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidSlug(value) | Self::InvalidScope(value) => {
                write!(
                    f,
                    "invalid name `{value}`: lowercase ascii alnum and `-`, 1..=128 chars"
                )
            }
            Self::Frontmatter(message) => write!(f, "invalid frontmatter: {message}"),
            Self::InvalidLink(target) => write!(f, "invalid link target `{target}`: not a slug"),
            Self::TypeMismatch { declared, found } => write!(
                f,
                "frontmatter type `{}` does not match the declared `{}`",
                found.as_str(),
                declared.as_str()
            ),
            Self::EntityTypeChanged { from, to } => write!(
                f,
                "a page's entity type cannot change ({} -> {})",
                from.as_str(),
                to.as_str()
            ),
            Self::NewPageWithBase { base } => {
                write!(f, "a new page takes no base_version, got {base}")
            }
            Self::MissingBaseVersion => write!(f, "editing a page requires a base_version"),
            Self::Conflict { base, head } => write!(f, "write conflict: base {base}, head {head}"),
            Self::HumanEditPending { version } => {
                write!(
                    f,
                    "a human edit at version {version} is pending reconciliation"
                )
            }
            Self::Corrupt(message) => write!(f, "corrupt page row: {message}"),
            Self::Shredded(scope) => write!(
                f,
                "scope `{scope}` was forgotten: its key is destroyed and its bodies unreadable"
            ),
            Self::KeyUnavailable { scope, version } => {
                write!(f, "scope `{scope}` has no live key at version {version}")
            }
            Self::Crypto(message) => write!(f, "body encryption failed: {message}"),
            Self::RotationPending { scope, superseded } => write!(
                f,
                "scope `{scope}` is still re-encrypting from key version {superseded}; \
                 finish that rotation before starting another"
            ),
            Self::Kms(error) => write!(f, "key custodian error: {error}"),
            Self::Blob(error) => write!(f, "blob error: {error}"),
            Self::Store(error) => write!(f, "store error: {error}"),
        }
    }
}

impl std::error::Error for PageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Kms(error) => Some(error),
            Self::Blob(error) => Some(error),
            Self::Store(error) => Some(error),
            _ => None,
        }
    }
}

/// The `pages` head row, as far as a write needs it.
struct PageRow {
    entity_type: EntityType,
    head_version: u32,
    human_pending_version: Option<u32>,
}

/// One version of a page a re-encryption pass has to re-seal.
struct StaleBody {
    slug: String,
    version: u32,
    body_key: String,
    key_version: u32,
    is_head: bool,
}

/// The versioned page store.
pub struct PageStore {
    db: Arc<dyn Database>,
    blob: Arc<dyn Blob>,
    keys: ScopeKeys,
    clock: Arc<dyn Clock>,
    id_gen: Arc<dyn IdGen>,
}

impl PageStore {
    /// A store over the four ports it needs plus a key custodian.
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

    /// Writes a new version of a page, returning its metadata. A new page
    /// takes no `base_version`; an edit requires one. Failures are
    /// [`PageError`].
    pub async fn write(
        &self,
        scope: &str,
        slug: &str,
        write: PageWrite,
    ) -> Result<VersionMeta, PageError> {
        if !entity::is_slug(scope) {
            return Err(PageError::InvalidScope(scope.to_owned()));
        }
        if !entity::is_slug(slug) {
            return Err(PageError::InvalidSlug(slug.to_owned()));
        }

        let (frontmatter, body) = entity::parse_frontmatter(&write.markdown, write.entity_type)?;
        let links = entity::extract_links(&body)?;
        let title = titles::frontmatter_title(&frontmatter).unwrap_or_default();

        let head = self.head_row(scope, slug).await?;
        let version = match &head {
            None => {
                if let Some(base) = write.base_version {
                    return Err(PageError::NewPageWithBase { base });
                }
                1
            }
            Some(row) => {
                let base = write.base_version.ok_or(PageError::MissingBaseVersion)?;
                if row.entity_type != write.entity_type {
                    return Err(PageError::EntityTypeChanged {
                        from: row.entity_type,
                        to: write.entity_type,
                    });
                }
                if base > row.head_version {
                    return Err(PageError::Conflict {
                        base,
                        head: row.head_version,
                    });
                }
                match &write.author {
                    // A human write wins over brain versions it did not see,
                    // but never over another human's.
                    Author::Human { .. } => {
                        if base < row.head_version
                            && self
                                .human_between(scope, slug, base, row.head_version)
                                .await?
                        {
                            return Err(PageError::Conflict {
                                base,
                                head: row.head_version,
                            });
                        }
                    }
                    Author::Brain { reconciles } => {
                        if base != row.head_version {
                            return Err(PageError::Conflict {
                                base,
                                head: row.head_version,
                            });
                        }
                        if let Some(pending) = row.human_pending_version
                            && *reconciles != Some(pending)
                        {
                            return Err(PageError::HumanEditPending { version: pending });
                        }
                    }
                }
                row.head_version + 1
            }
        };

        // The body key carries a fresh id, not just the version: two writers
        // that computed the same `version` from a stale head then put to two
        // different keys, so the loser of the `page_versions` insert below
        // cannot overwrite the winner's committed body. The body goes first
        // anyway — an orphan body left by a failed batch is harmless, whereas
        // a version row pointing at a body that is not there is not.
        let body_key = format!("{scope}/{slug}/{version:010}-{}.md", self.id_gen.ulid());
        // Sealed before the put, so a blob is never a plaintext that a reader
        // might catch before the key arrives.
        let active = self.keys.active(scope).await?;
        let envelope = keys::seal(
            &active.dek,
            active.version,
            &keys::body_aad(scope, &body_key, active.version),
            write.markdown.as_bytes(),
        )?;
        let index_key = keys::IndexKey::derive(&active.dek);
        let terms: Vec<String> = index_key.page_terms(slug, &write.markdown);
        let key_version = active.version;
        // The title rides on the head row, sealed like the body, so a
        // listing names the page without opening it. An empty title is kept
        // as "" at version 0: nothing to seal, and nothing to backfill.
        let (title_sealed, title_version) = if title.is_empty() {
            (String::new(), 0)
        } else {
            (
                seal_title(&active.dek, key_version, scope, slug, &title)?,
                key_version,
            )
        };
        drop(active);
        self.blob
            .put(&body_key, &envelope, keys::SEALED_CONTENT_TYPE)
            .await
            .map_err(PageError::Blob)?;

        let now = self.now();
        let pending = match write.author.kind() {
            AuthorKind::Human => Some(version),
            AuthorKind::Brain => None,
        };

        let mut statements = Vec::with_capacity(8 + links.len() + terms.len());
        statements.push(Statement::with_values(
            "INSERT INTO page_versions \
             (scope, slug, version, author_kind, author, base_version, body_key, created_at, key_version) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            vec![
                text(scope),
                text(slug),
                int(version),
                text(write.author.kind().as_str()),
                text(write.author.label()),
                opt_int(write.base_version),
                text(&body_key),
                text(&now),
                int(key_version),
            ],
        ));
        match &head {
            None => statements.push(Statement::with_values(
                "INSERT INTO pages \
                 (scope, slug, entity_type, head_version, human_pending_version, updated_at, \
                  title_sealed, title_key_version) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                vec![
                    text(scope),
                    text(slug),
                    text(write.entity_type.as_str()),
                    int(version),
                    opt_int(pending),
                    text(&now),
                    text(&title_sealed),
                    int(title_version),
                ],
            )),
            // The head guard alone cannot stop a race: `batch_atomic` rolls
            // back on an error, not on a zero-row update. The `page_versions`
            // primary key does — two writers of the same next version collide
            // on the insert above, and that failure is read back as a conflict.
            Some(row) => statements.push(Statement::with_values(
                "UPDATE pages SET head_version = ?, human_pending_version = ?, updated_at = ?, \
                 title_sealed = ?, title_key_version = ? \
                 WHERE scope = ? AND slug = ? AND head_version = ?",
                vec![
                    int(version),
                    opt_int(pending),
                    text(&now),
                    text(&title_sealed),
                    int(title_version),
                    text(scope),
                    text(slug),
                    int(row.head_version),
                ],
            )),
        }
        statements.push(Statement::with_values(
            "DELETE FROM page_links WHERE scope = ? AND from_slug = ?",
            vec![text(scope), text(slug)],
        ));
        for target in &links {
            statements.push(Statement::with_values(
                "INSERT INTO page_links (scope, from_slug, to_slug) VALUES (?, ?, ?)",
                vec![text(scope), text(slug), text(target)],
            ));
        }
        // Replaced with the body, in the same batch: an index row surviving a
        // failed write would make search return a version never committed.
        statements.push(Statement::with_values(
            "DELETE FROM page_terms WHERE scope = ? AND slug = ?",
            vec![text(scope), text(slug)],
        ));
        for term in &terms {
            statements.push(Statement::with_values(
                "INSERT INTO page_terms (scope, term, slug) VALUES (?, ?, ?)",
                vec![text(scope), text(term), text(slug)],
            ));
        }

        if let Err(error) = self.db.batch_atomic(&statements).await {
            // The batch failed. If the head has moved past the version this
            // write was based on, another writer won the race and this is a
            // conflict, not an infrastructure fault; otherwise it is one.
            let expected = head.as_ref().map_or(0, |row| row.head_version);
            let now_head = self
                .head_row(scope, slug)
                .await?
                .map_or(0, |row| row.head_version);
            if now_head != expected {
                return Err(PageError::Conflict {
                    base: write.base_version.unwrap_or(0),
                    head: now_head,
                });
            }
            return Err(PageError::Store(error));
        }

        Ok(VersionMeta {
            version,
            author_kind: write.author.kind(),
            author: write.author.label().to_owned(),
            base_version: write.base_version,
            created_at: now,
        })
    }

    /// The current version of a page, body and all, or `None` if it does not
    /// exist. Failures are [`PageError`].
    pub async fn read(&self, scope: &str, slug: &str) -> Result<Option<Page>, PageError> {
        let Some(row) = self.head_row(scope, slug).await? else {
            return Ok(None);
        };
        self.page_at(scope, slug, row.entity_type, row.head_version)
            .await
    }

    /// One version of a page, body and all, or `None` if the page or version
    /// does not exist. Failures are [`PageError`].
    pub async fn read_version(
        &self,
        scope: &str,
        slug: &str,
        version: u32,
    ) -> Result<Option<Page>, PageError> {
        let Some(row) = self.head_row(scope, slug).await? else {
            return Ok(None);
        };
        self.page_at(scope, slug, row.entity_type, version).await
    }

    /// Every version of a page, oldest first. Empty for an unknown page.
    pub async fn history(&self, scope: &str, slug: &str) -> Result<Vec<VersionMeta>, PageError> {
        let rows = self
            .db
            .query(&Statement::with_values(
                "SELECT version, author_kind, author, base_version, created_at \
                 FROM page_versions WHERE scope = ? AND slug = ? ORDER BY version ASC",
                vec![text(scope), text(slug)],
            ))
            .await
            .map_err(PageError::Store)?;
        rows.rows
            .iter()
            .map(version_meta_from)
            .collect::<Result<Vec<_>, _>>()
    }

    /// The slugs that link to this page, sorted.
    pub async fn backlinks(&self, scope: &str, slug: &str) -> Result<Vec<String>, PageError> {
        let rows = self
            .db
            .query(&Statement::with_values(
                "SELECT from_slug FROM page_links WHERE scope = ? AND to_slug = ? \
                 ORDER BY from_slug ASC",
                vec![text(scope), text(slug)],
            ))
            .await
            .map_err(PageError::Store)?;
        Ok(rows
            .rows
            .iter()
            .filter_map(|row| row.get::<String>("from_slug"))
            .collect())
    }

    /// The heads of the pages in `scopes`, newest first, each without its
    /// body — the inventory behind a "list pages" call.
    ///
    /// The asker supplies the scopes; this never widens them and never runs
    /// a query without a scope predicate. An empty scope list or a zero
    /// limit asks for nothing and gets nothing, without a query.
    ///
    /// # Errors
    ///
    /// [`PageError::Store`] if the database fails.
    pub async fn list(&self, scopes: &[&str], limit: usize) -> Result<Vec<PageSummary>, PageError> {
        self.list_filtered(scopes, &PageFilter::default(), limit)
            .await
    }

    /// [`PageStore::list`], narrowed by `filter` in the query itself — so a
    /// listing that leaves the session logs out is still `limit` pages of
    /// everything else, not `limit` rows of which most were sessions.
    ///
    /// Each row's title is opened from its sealed copy with the scope key it
    /// names, one unwrap per key version however many rows share it. A title
    /// that does not open (a forgotten scope, a retired key) is `None`, never
    /// an error: a listing answers with what it can name.
    ///
    /// # Errors
    ///
    /// [`PageError::Store`] if the database fails.
    pub async fn list_filtered(
        &self,
        scopes: &[&str],
        filter: &PageFilter,
        limit: usize,
    ) -> Result<Vec<PageSummary>, PageError> {
        let filters: Vec<ScopeFilter> = scopes
            .iter()
            .map(|scope| ScopeFilter::whole(*scope, ""))
            .collect();
        self.list_in_filtered(&filters, filter, limit).await
    }

    /// [`PageStore::list`] over grant filters (ADR 0004): only the slugs each
    /// [`ScopeFilter`] opens, filtered **in the query**, so the order and the
    /// limit are over what the asker may read and nothing else.
    ///
    /// # Errors
    ///
    /// [`PageError::Store`] if the database fails.
    pub async fn list_in(
        &self,
        filters: &[ScopeFilter],
        limit: usize,
    ) -> Result<Vec<PageSummary>, PageError> {
        self.list_in_filtered(filters, &PageFilter::default(), limit)
            .await
    }

    /// [`PageStore::list_filtered`] over grant filters: the grant's slug
    /// predicate and `filter` both in the query, before the order and the
    /// limit.
    ///
    /// # Errors
    ///
    /// [`PageError::Store`] if the database fails.
    pub async fn list_in_filtered(
        &self,
        filters: &[ScopeFilter],
        filter: &PageFilter,
        limit: usize,
    ) -> Result<Vec<PageSummary>, PageError> {
        if filters.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        if let Some(project) = &filter.project {
            // A project key is `[a-z0-9-]`, so it carries no LIKE wildcard;
            // anything else matches nothing rather than everything.
            if titles::project_key(project).as_deref() != Some(project.as_str()) {
                return Ok(Vec::new());
            }
        }
        // A type named twice is one type: the lists are de-duplicated, so
        // they bind at most one value per entity type however often a caller
        // repeated one.
        let types: BTreeSet<&str> = filter.types.iter().map(|kind| kind.as_str()).collect();
        let exclude: BTreeSet<&str> = filter.exclude.iter().map(|kind| kind.as_str()).collect();
        // The clauses after the grant's predicate bind a fixed number of
        // values; the grant's pieces get what is left of D1's budget.
        let fixed = types.len() + exclude.len() + if filter.project.is_some() { 2 } else { 0 } + 1;
        let budget = BIND_BUDGET.saturating_sub(fixed);
        let grant_binds = |piece: &ScopeFilter| piece.predicate(None).1.len();
        let pieces: Vec<ScopeFilter> = filters
            .iter()
            .flat_map(|filter| filter.split_to_budget(budget, &grant_binds))
            .collect();
        // One statement per group of pieces, each capped at `limit`; the
        // answers merge below into what one statement would have returned.
        let mut rows = Vec::new();
        for group in pack(pieces, budget, grant_binds) {
            let mut values = Vec::new();
            let predicate = group
                .iter()
                .map(|filter| {
                    let (sql, binds) = filter.predicate(None);
                    values.extend(binds);
                    sql
                })
                .collect::<Vec<_>>()
                .join(" OR ");
            let mut sql = format!(
                "SELECT scope, slug, entity_type, head_version, updated_at, title_sealed, \
                 title_key_version FROM pages WHERE ({predicate})"
            );
            if !types.is_empty() {
                let marks = types
                    .iter()
                    .map(|kind| {
                        values.push(text(kind));
                        "?"
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                sql.push_str(&format!(" AND entity_type IN ({marks})"));
            }
            if !exclude.is_empty() {
                let marks = exclude
                    .iter()
                    .map(|kind| {
                        values.push(text(kind));
                        "?"
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                sql.push_str(&format!(" AND entity_type NOT IN ({marks})"));
            }
            if let Some(project) = &filter.project {
                sql.push_str(" AND (slug LIKE ? OR slug = ?)");
                values.push(text(&format!("coding-agents--%--{project}--%")));
                values.push(text(&format!("coding-agents--projects--{project}")));
            }
            sql.push_str(" ORDER BY updated_at DESC, slug ASC LIMIT ?");
            values.push(SeaValue::BigInt(Some(
                i64::try_from(limit).unwrap_or(i64::MAX),
            )));
            let fetched = self
                .db
                .query(&Statement::with_values(sql, values))
                .await
                .map_err(PageError::Store)?;
            rows.extend(fetched.rows);
        }
        // The order one statement would have used, then one row per page
        // (a page two pieces name is one page), cut to the limit.
        rows.sort_by(|a, b| {
            b.get::<String>("updated_at")
                .cmp(&a.get::<String>("updated_at"))
                .then_with(|| a.get::<String>("slug").cmp(&b.get::<String>("slug")))
                .then_with(|| a.get::<String>("scope").cmp(&b.get::<String>("scope")))
        });
        let mut seen = BTreeSet::new();
        rows.retain(|row| {
            seen.insert((
                row.get::<String>("scope").unwrap_or_default(),
                row.get::<String>("slug").unwrap_or_default(),
            ))
        });
        rows.truncate(limit);
        let mut keys: BTreeMap<(String, u32), Option<Dek>> = BTreeMap::new();
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let raw = row
                .get::<String>("entity_type")
                .ok_or_else(|| PageError::Corrupt("pages.entity_type is not text".to_owned()))?;
            let scope = row.get::<String>("scope").unwrap_or_default();
            let slug = row.get::<String>("slug").unwrap_or_default();
            let sealed = row.get::<String>("title_sealed").unwrap_or_default();
            let key_version = row.get::<u32>("title_key_version").unwrap_or_default();
            let title = if sealed.is_empty() || key_version == 0 {
                None
            } else {
                let key = keys.entry((scope.clone(), key_version)).or_insert(None);
                if key.is_none() {
                    *key = self.keys.dek(&scope, key_version).await.ok();
                }
                key.as_ref()
                    .and_then(|dek| open_title(dek, key_version, &scope, &slug, &sealed))
            };
            out.push(PageSummary {
                entity_type: EntityType::parse(&raw)
                    .ok_or_else(|| PageError::Corrupt(format!("unknown entity type `{raw}`")))?,
                version: row.get::<u32>("head_version").unwrap_or_default(),
                updated_at: row.get::<String>("updated_at").unwrap_or_default(),
                scope,
                slug,
                title,
            });
        }
        Ok(out)
    }

    /// Up to `limit` pages, across every scope, whose title has never been
    /// set: every page written before migration 0007, which includes every
    /// session log still filed as a decision. The notes module's cron works
    /// through them; once each has been handled (its title set, to "" when
    /// there is nothing to show) it is never answered again, so the pass is
    /// idempotent.
    ///
    /// # Errors
    ///
    /// [`PageError::Store`] if the database fails.
    pub async fn title_candidates(&self, limit: usize) -> Result<Vec<TitleCandidate>, PageError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let rows = self
            .db
            .query(&Statement::with_values(
                "SELECT scope, slug, entity_type FROM pages WHERE title_sealed IS NULL \
                 ORDER BY updated_at DESC, scope ASC, slug ASC LIMIT ?",
                vec![SeaValue::BigInt(Some(
                    i64::try_from(limit).unwrap_or(i64::MAX),
                ))],
            ))
            .await
            .map_err(PageError::Store)?;
        rows.rows
            .iter()
            .map(|row| {
                let raw = row.get::<String>("entity_type").ok_or_else(|| {
                    PageError::Corrupt("pages.entity_type is not text".to_owned())
                })?;
                Ok(TitleCandidate {
                    scope: row.get::<String>("scope").unwrap_or_default(),
                    slug: row.get::<String>("slug").unwrap_or_default(),
                    entity_type: EntityType::parse(&raw).ok_or_else(|| {
                        PageError::Corrupt(format!("unknown entity type `{raw}`"))
                    })?,
                })
            })
            .collect()
    }

    /// Sets a page's listed title without writing a version: the backfill's
    /// write for a page whose title is already right in its body. `None`
    /// (a page with no title, or one whose body cannot be opened) marks the
    /// row as handled with nothing to show.
    ///
    /// # Errors
    ///
    /// [`PageError::InvalidScope`], and whatever sealing or the database can
    /// fail with.
    pub async fn set_title(
        &self,
        scope: &str,
        slug: &str,
        title: Option<&str>,
    ) -> Result<(), PageError> {
        if !entity::is_slug(scope) {
            return Err(PageError::InvalidScope(scope.to_owned()));
        }
        let (sealed, version) = match title.map(str::trim).filter(|t| !t.is_empty()) {
            None => (String::new(), 0),
            Some(title) => {
                let active = self.keys.active(scope).await?;
                let sealed = seal_title(&active.dek, active.version, scope, slug, title)?;
                (sealed, active.version)
            }
        };
        self.db
            .execute(&Statement::with_values(
                "UPDATE pages SET title_sealed = ?, title_key_version = ? \
                 WHERE scope = ? AND slug = ?",
                vec![text(&sealed), int(version), text(scope), text(slug)],
            ))
            .await
            .map_err(PageError::Store)?;
        Ok(())
    }

    /// Moves a page from one entity type to another in place, without a new
    /// version — the one sanctioned exception to "a page cannot change its
    /// type", for session logs written as decisions before the `session`
    /// type existed. The head body must already be valid as `to` (every key
    /// a decision may carry, a session may too), so nothing a reader parses
    /// can break. Answers whether the row moved: `false` when it was not of
    /// type `from` (already moved, or never one).
    ///
    /// # Errors
    ///
    /// [`PageError::Frontmatter`] when the head would not parse as `to`, and
    /// whatever reading the head or the database can fail with.
    pub async fn retype(
        &self,
        scope: &str,
        slug: &str,
        from: EntityType,
        to: EntityType,
    ) -> Result<bool, PageError> {
        // The head row first: a page already of another type (every write
        // after the first) costs one indexed read, not a body.
        match self.head_row(scope, slug).await? {
            Some(row) if row.entity_type == from => {}
            _ => return Ok(false),
        }
        let Some(page) = self.read(scope, slug).await? else {
            return Ok(false);
        };
        // The fence may name its own type; a `type: decision` line would
        // make the body invalid as `to`, which is a refusal, not a move.
        entity::parse_frontmatter(&page.markdown, to)?;
        let moved = self
            .db
            .execute(&Statement::with_values(
                "UPDATE pages SET entity_type = ? WHERE scope = ? AND slug = ? AND entity_type = ?",
                vec![
                    text(to.as_str()),
                    text(scope),
                    text(slug),
                    text(from.as_str()),
                ],
            ))
            .await
            .map_err(PageError::Store)?;
        Ok(moved > 0)
    }

    /// The `pages` row for a page, or `None`.
    async fn head_row(&self, scope: &str, slug: &str) -> Result<Option<PageRow>, PageError> {
        let rows = self
            .db
            .query(&Statement::with_values(
                "SELECT entity_type, head_version, human_pending_version \
                 FROM pages WHERE scope = ? AND slug = ?",
                vec![text(scope), text(slug)],
            ))
            .await
            .map_err(PageError::Store)?;
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        let raw = row
            .get::<String>("entity_type")
            .ok_or_else(|| PageError::Corrupt("pages.entity_type is not text".to_owned()))?;
        let entity_type = EntityType::parse(&raw)
            .ok_or_else(|| PageError::Corrupt(format!("unknown entity type `{raw}`")))?;
        Ok(Some(PageRow {
            entity_type,
            head_version: row.get::<u32>("head_version").unwrap_or_default(),
            human_pending_version: row.get::<u32>("human_pending_version"),
        }))
    }

    /// Whether any version in `(base, head]` was authored by a human.
    async fn human_between(
        &self,
        scope: &str,
        slug: &str,
        base: u32,
        head: u32,
    ) -> Result<bool, PageError> {
        let rows = self
            .db
            .query(&Statement::with_values(
                "SELECT 1 AS one FROM page_versions \
                 WHERE scope = ? AND slug = ? AND version > ? AND version <= ? AND author_kind = ?",
                vec![
                    text(scope),
                    text(slug),
                    int(base),
                    int(head),
                    text(AuthorKind::Human.as_str()),
                ],
            ))
            .await
            .map_err(PageError::Store)?;
        Ok(!rows.is_empty())
    }

    /// One version's body, read back from the blob store and re-parsed.
    async fn page_at(
        &self,
        scope: &str,
        slug: &str,
        entity_type: EntityType,
        version: u32,
    ) -> Result<Option<Page>, PageError> {
        let rows = self
            .db
            .query(&Statement::with_values(
                "SELECT body_key, key_version FROM page_versions \
                 WHERE scope = ? AND slug = ? AND version = ?",
                vec![text(scope), text(slug), int(version)],
            ))
            .await
            .map_err(PageError::Store)?;
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        let body_key = row.get::<String>("body_key").unwrap_or_default();
        let Some(object) = self.blob.get(&body_key).await.map_err(PageError::Blob)? else {
            // A version row with no body is corruption, not an absent version.
            return Err(PageError::Corrupt(format!(
                "version {version} of `{slug}` has no body at `{body_key}`"
            )));
        };
        let markdown = self
            .open_body(
                scope,
                &body_key,
                row.get::<u32>("key_version"),
                &object.bytes,
            )
            .await?;
        let (frontmatter, body) = entity::parse_frontmatter(&markdown, entity_type)?;
        let links = entity::extract_links(&body)?;
        Ok(Some(Page {
            scope: scope.to_owned(),
            slug: slug.to_owned(),
            entity_type,
            version,
            markdown,
            frontmatter,
            links,
        }))
    }

    /// Pages in `scopes` whose current body mentions `query`, best first, as
    /// `(scope, slug)` and never anything else.
    ///
    /// The query is cut into words the way a body is indexed, its stopwords
    /// dropped and each word widened to its singular/plural forms
    /// ([`crate::query_terms`]). A page matches when it holds **any** of the
    /// words, and the pages are ranked in SQL by a BM25-style score: the sum,
    /// over the query words a page holds, of the word's inverse document
    /// frequency in the scope (`ln(1 + (N - df + 0.5) / (df + 0.5))`), a word
    /// in the page's title or slug counting [`TITLE_BOOST`] times. The blind
    /// index holds distinct words, not counts, so term frequency is one and
    /// there is no length normalisation — rarer words and title words are
    /// what rank. Ties go to the most recently updated page, then the slug.
    ///
    /// The asker supplies the scopes; this never widens them, never runs a
    /// query without a scope predicate (every statement binds the scope
    /// first), and never decrypts a body. Each query word's MAC is computed
    /// under every live key version of the scope — so a page written before a
    /// rotation is still found after one. The limit is applied after the
    /// scope predicate and the ranking, in the database, and again across
    /// scopes. A scope with no live key (shredded, or never written)
    /// contributes nothing, and a query with no word left in it — empty,
    /// punctuation, or only stopwords — matches nothing: there is no way to
    /// say "any page" through a blind index.
    ///
    /// # Errors
    ///
    /// [`PageError::InvalidScope`] for a scope outside the slug rule, and
    /// [`PageError::Store`] if the database fails.
    pub async fn search(
        &self,
        scopes: &[&str],
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchHit>, PageError> {
        let filters: Vec<ScopeFilter> = scopes
            .iter()
            .map(|scope| ScopeFilter::whole(*scope, ""))
            .collect();
        self.search_in(&filters, query, limit).await
    }

    /// [`PageStore::search`] over grant filters (ADR 0004): each scope's
    /// ranking query carries its filter's slug predicate — the candidates,
    /// the page count and the document frequencies alike — so a slug the
    /// asker may not read is never matched, weighed, counted against the
    /// limit or returned.
    ///
    /// # Errors
    ///
    /// As [`PageStore::search`].
    pub async fn search_in(
        &self,
        filters: &[ScopeFilter],
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchHit>, PageError> {
        for filter in filters {
            if !entity::is_slug(&filter.scope) {
                return Err(PageError::InvalidScope(filter.scope.clone()));
            }
        }
        let terms = crate::query::query_terms(query);
        if terms.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }

        let mut ranked: Vec<Ranked> = Vec::new();
        for filter in filters {
            let index_keys = self.keys.index_keys(&filter.scope).await?;
            if index_keys.is_empty() {
                continue;
            }
            ranked.extend(self.rank_scope(filter, &index_keys, &terms, limit).await?);
        }
        ranked.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| b.updated_at.cmp(&a.updated_at))
                .then_with(|| a.scope.cmp(&b.scope))
                .then_with(|| a.slug.cmp(&b.slug))
        });
        Ok(ranked
            .into_iter()
            .take(limit)
            .map(|hit| SearchHit {
                scope: hit.scope,
                slug: hit.slug,
            })
            .collect())
    }

    /// One scope's best `limit` pages for `terms`, scored in the database.
    ///
    /// Two statements: the scope's page count and each body form's document
    /// frequency, from which this computes every word's weight; then one
    /// ranking query that groups the matching index rows by page and word
    /// (a page holding two forms of a word, or a word in both its title and
    /// body, counts the word once, at its best weight), sums the weights per
    /// page and orders and limits there. The candidate set is the index rows
    /// of the query's MACs in this scope — bounded by the page count times
    /// the handful of MACs — and only `limit` rows come back.
    ///
    /// The MACs and weights are written into the SQL as literals: a MAC is
    /// 64 hex characters this module computed ([`keys::hex`]) and a weight an
    /// integer, so neither can carry SQL, and inlining them keeps the scope
    /// the first and the limit the only other bound value — inside D1's
    /// bound-parameter ceiling however many forms and keys a query has.
    async fn rank_scope(
        &self,
        filter: &ScopeFilter,
        index_keys: &[keys::IndexKey],
        terms: &[crate::QueryTerm],
        limit: usize,
    ) -> Result<Vec<Ranked>, PageError> {
        // (group, MAC) for every body form and every title form.
        let mut body: Vec<(usize, String)> = Vec::new();
        let mut title: Vec<(usize, String)> = Vec::new();
        for (group, term) in terms.iter().enumerate() {
            for form in &term.forms {
                for key in index_keys {
                    body.push((group, key.mac(form)));
                    title.push((group, key.title_mac(form)));
                }
            }
        }
        debug_assert!(
            body.iter()
                .chain(&title)
                .all(|(_, mac)| mac.bytes().all(|b| b.is_ascii_hexdigit()))
        );
        let in_list = |macs: &mut dyn Iterator<Item = &String>| {
            macs.map(|mac| format!("'{mac}'"))
                .collect::<Vec<_>>()
                .join(", ")
        };

        // The grant's predicate binds one value per prefix, so a long allow
        // list is read in pieces (disjoint by construction, see
        // `ScopeFilter::split_to_budget`). The count, the document
        // frequencies and the ranking are each summed or merged over the
        // pieces, so every word still weighs what it would over the whole
        // grant, and the ranking is the one a single statement would give.
        let grant_binds = |piece: &ScopeFilter| piece.predicate(None).1.len();
        let pieces = filter.split_to_budget(BIND_BUDGET - 1, &grant_binds);

        let mut total: i64 = 0;
        let mut frequencies: BTreeMap<String, i64> = BTreeMap::new();
        for piece in &pieces {
            let (predicate, values) = piece.predicate(None);
            let counted = self
                .db
                .query(&Statement::with_values(
                    format!("SELECT COUNT(*) AS n FROM pages WHERE {predicate}"),
                    values,
                ))
                .await
                .map_err(PageError::Store)?;
            total += counted
                .first()
                .and_then(|row| row.get::<i64>("n"))
                .unwrap_or(0)
                .max(0);
            let (predicate, values) = piece.predicate(None);
            let df_rows = self
                .db
                .query(&Statement::with_values(
                    format!(
                        "SELECT term, COUNT(*) AS df FROM page_terms \
                         WHERE {predicate} AND term IN ({}) GROUP BY term",
                        in_list(&mut body.iter().map(|(_, mac)| mac))
                    ),
                    values,
                ))
                .await
                .map_err(PageError::Store)?;
            for row in &df_rows.rows {
                if let (Some(term), Some(df)) = (row.get::<String>("term"), row.get::<i64>("df")) {
                    *frequencies.entry(term).or_insert(0) += df;
                }
            }
        }
        if total == 0 {
            return Ok(Vec::new());
        }
        let df_of = frequencies;
        // A word's document frequency is its most frequent form's: the forms
        // of one word overlap on the pages that use both.
        let mut df = vec![0_i64; terms.len()];
        for (group, mac) in &body {
            df[*group] = df[*group].max(df_of.get(mac).copied().unwrap_or(0));
        }
        // A word no body holds keeps its (high) weight: a page can still be
        // named for it, through its title rows.
        let weight: Vec<i64> = df.iter().map(|count| idf_weight(total, *count)).collect();

        let mut group_case = String::from("CASE term");
        let mut weight_case = String::from("CASE term");
        for (group, mac) in &body {
            group_case.push_str(&format!(" WHEN '{mac}' THEN {group}"));
            weight_case.push_str(&format!(" WHEN '{mac}' THEN {}", weight[*group]));
        }
        for (group, mac) in &title {
            group_case.push_str(&format!(" WHEN '{mac}' THEN {group}"));
            weight_case.push_str(&format!(
                " WHEN '{mac}' THEN {}",
                weight[*group] * TITLE_BOOST
            ));
        }
        group_case.push_str(" END");
        weight_case.push_str(" END");
        let scope = filter.scope.as_str();
        let mut ranked = Vec::new();
        for piece in &pieces {
            let (predicate, mut values) = piece.predicate(None);
            values.push(SeaValue::BigInt(Some(
                i64::try_from(limit).unwrap_or(i64::MAX),
            )));
            let rows = self
                .db
                .query(&Statement::with_values(
                    format!(
                        "SELECT g.slug AS slug, SUM(g.w) AS score, MAX(p.updated_at) AS recency \
                         FROM (SELECT scope, slug, {group_case} AS grp, MAX({weight_case}) AS w \
                               FROM page_terms WHERE {predicate} AND term IN ({}) \
                               GROUP BY scope, slug, grp) g \
                         JOIN pages p ON p.scope = g.scope AND p.slug = g.slug \
                         GROUP BY g.slug \
                         ORDER BY score DESC, recency DESC, slug ASC LIMIT ?",
                        in_list(&mut body.iter().chain(&title).map(|(_, mac)| mac))
                    ),
                    values,
                ))
                .await
                .map_err(PageError::Store)?;
            ranked.extend(rows.rows.iter().filter_map(|row| {
                Some(Ranked {
                    scope: scope.to_owned(),
                    slug: row.get::<String>("slug")?,
                    score: row.get::<i64>("score").unwrap_or(0),
                    updated_at: row.get::<String>("recency").unwrap_or_default(),
                })
            }));
        }
        Ok(ranked)
    }

    /// Forgets a scope: every version of its key is destroyed, and its blind
    /// index rows, its link rows and its source rows go, in one batch. The
    /// `pages` and `page_versions` metadata stays, and the bodies stay in the
    /// blob store — readable by nobody, because the key that opened them is
    /// gone, and destroying it is immediate everywhere because no key is ever
    /// cached.
    ///
    /// The link rows go because a `[[wiki-link]]` target is a word chosen out of
    /// a body. The slugs stay: a page's own name is how the rest of the wiki
    /// refers to it.
    ///
    /// The source rows go for the same reason and more (issue #81): an imported
    /// file's path is somebody's own filing and, unlike a page slug, it names
    /// nothing that outlives the scope. Their sealed bodies need no delete —
    /// the `UPDATE` above nulls the key that opens them in the same batch, so
    /// they are unreadable the moment this commits.
    ///
    /// # Errors
    ///
    /// [`PageError::InvalidScope`] for a scope outside the slug rule, and
    /// [`PageError::Store`] if the database fails.
    pub async fn forget_scope(&self, scope: &str) -> Result<(), PageError> {
        if !entity::is_slug(scope) {
            return Err(PageError::InvalidScope(scope.to_owned()));
        }
        self.db
            .batch_atomic(&[
                Statement::with_values(
                    "UPDATE scope_keys SET wrapped_dek = NULL WHERE scope = ?",
                    vec![text(scope)],
                ),
                Statement::with_values("DELETE FROM page_terms WHERE scope = ?", vec![text(scope)]),
                Statement::with_values("DELETE FROM page_links WHERE scope = ?", vec![text(scope)]),
                Statement::with_values("DELETE FROM sources WHERE scope = ?", vec![text(scope)]),
                Statement::with_values("DELETE FROM facts WHERE scope = ?", vec![text(scope)]),
                Statement::with_values(
                    "DELETE FROM fact_dream_decisions WHERE scope = ?",
                    vec![text(scope)],
                ),
                Statement::with_values(
                    "DELETE FROM fact_dreams WHERE scope = ?",
                    vec![text(scope)],
                ),
                Statement::with_values(
                    "DELETE FROM app_memories WHERE scope = ?",
                    vec![text(scope)],
                ),
                Statement::with_values(
                    "DELETE FROM public_pages WHERE scope = ?",
                    vec![text(scope)],
                ),
            ])
            .await
            .map_err(PageError::Store)
    }

    /// Starts a new key version for a scope. Nothing is re-sealed and nothing
    /// is retired: every existing body keeps reading under the version it was
    /// written with, and new writes use the new one. Refused with
    /// [`PageError::RotationPending`] while a previous rotation is unfinished,
    /// which is what bounds a scope to two live keys and a search to two
    /// unwraps per scope; finish the outstanding one with
    /// [`PageStore::reencrypt_scope`] first.
    ///
    /// # Errors
    ///
    /// [`PageError::InvalidScope`], [`PageError::Shredded`] if the scope
    /// was forgotten, [`PageError::RotationPending`], and
    /// [`PageError::Store`].
    pub async fn rotate_scope_key(&self, scope: &str) -> Result<u32, PageError> {
        if !entity::is_slug(scope) {
            return Err(PageError::InvalidScope(scope.to_owned()));
        }
        self.keys.rotate(scope).await
    }

    /// Re-seals every body in a scope under its active key, re-derives the
    /// blind index under that key, and retires the versions nothing refers to
    /// any more.
    ///
    /// Which bodies to re-seal comes from `page_versions.key_version` in D1,
    /// not from the envelope header, because the header is what this pass
    /// rewrites: a header that claimed to be current would skip the very pass
    /// meant to repair it.
    ///
    /// Retirement is a single guarded statement (see [`ScopeKeys::retire`]),
    /// and a version superseded for less than [`RETIRE_GRACE`] is reported as
    /// `pending` rather than retired, so a write that fetched the old key just
    /// before the rotation is never stranded on a destroyed key. Re-running
    /// later finishes the job, and re-running with nothing to do rewrites
    /// nothing.
    ///
    /// # Errors
    ///
    /// [`PageError::InvalidScope`], [`PageError::Shredded`],
    /// [`PageError::Blob`] or [`PageError::Store`].
    pub async fn reencrypt_scope(&self, scope: &str) -> Result<ReencryptReport, PageError> {
        if !entity::is_slug(scope) {
            return Err(PageError::InvalidScope(scope.to_owned()));
        }
        let active = self.keys.active(scope).await?;
        let index_key = keys::IndexKey::derive(&active.dek);
        let mut report = ReencryptReport::default();

        // `pages` is joined in rather than read separately so a head is known
        // while the pass runs, not after it.
        let rows = self
            .db
            .query(&Statement::with_values(
                "SELECT v.slug AS slug, v.version AS version, v.body_key AS body_key, \
                        v.key_version AS key_version, \
                        CASE WHEN v.version = p.head_version THEN 1 ELSE 0 END AS is_head \
                 FROM page_versions v \
                 JOIN pages p ON p.scope = v.scope AND p.slug = v.slug \
                 WHERE v.scope = ? ORDER BY v.slug ASC, v.version ASC",
                vec![text(scope)],
            ))
            .await
            .map_err(PageError::Store)?;
        let mut stale: Vec<StaleBody> = rows
            .rows
            .iter()
            .map(|row| {
                Ok(StaleBody {
                    slug: row.get::<String>("slug").unwrap_or_default(),
                    version: row.get::<u32>("version").unwrap_or_default(),
                    body_key: row.get::<String>("body_key").unwrap_or_default(),
                    key_version: row.get::<u32>("key_version").unwrap_or_default(),
                    is_head: row.get::<u32>("is_head").unwrap_or_default() == 1,
                })
            })
            .collect::<Result<Vec<_>, PageError>>()?;
        // Grouped by the version each body is on: one unwrap per old version.
        stale.retain(|body| body.key_version != active.version);
        stale.sort_by_key(|body| body.key_version);

        // Kept so the index pass MACs the same bytes rather than reading the
        // head back, which could see a version this pass has not rewritten.
        let mut reindexed: BTreeMap<String, String> = BTreeMap::new();
        let mut old_version: Option<u32> = None;
        let mut old_key = None;
        for body in &stale {
            if old_version != Some(body.key_version) {
                old_key = Some(self.keys.dek(scope, body.key_version).await?);
                old_version = Some(body.key_version);
            }
            let key = old_key.as_ref().expect("the key is set with the version");
            let object = self
                .blob
                .get(&body.body_key)
                .await
                .map_err(PageError::Blob)?
                .ok_or_else(|| {
                    PageError::Corrupt(format!(
                        "version {} of `{}` has no body at `{}`",
                        body.version, body.slug, body.body_key
                    ))
                })?;
            let plaintext = keys::open(
                key,
                body.key_version,
                &keys::body_aad(scope, &body.body_key, body.key_version),
                &object.bytes,
            )?;
            let markdown = String::from_utf8_lossy(&plaintext).into_owned();
            let sealed = keys::seal(
                &active.dek,
                active.version,
                &keys::body_aad(scope, &body.body_key, active.version),
                &plaintext,
            )?;
            self.blob
                .put(&body.body_key, &sealed, keys::SEALED_CONTENT_TYPE)
                .await
                .map_err(PageError::Blob)?;
            // The row moves to the new key only once the body is there. A
            // crash between the two fails to open on the next read, visibly,
            // rather than reading as corruption later.
            self.db
                .execute(&Statement::with_values(
                    "UPDATE page_versions SET key_version = ? \
                     WHERE scope = ? AND slug = ? AND version = ?",
                    vec![
                        int(active.version),
                        text(scope),
                        text(&body.slug),
                        int(body.version),
                    ],
                ))
                .await
                .map_err(PageError::Store)?;
            report.rewritten += 1;
            if body.is_head {
                reindexed.insert(body.slug.clone(), markdown);
            }
        }
        drop(old_key);

        // Temporal facts are sealed in their rows under the same scope key,
        // so they move with the bodies: each re-sealed, then its row
        // repointed, in one statement per fact.
        let facts = self
            .db
            .query(&Statement::with_values(
                "SELECT id, sealed, key_version FROM facts \
                 WHERE scope = ? AND key_version <> ? ORDER BY key_version ASC, id ASC",
                vec![text(scope), int(active.version)],
            ))
            .await
            .map_err(PageError::Store)?;
        let mut old_version: Option<u32> = None;
        let mut old_key = None;
        for row in &facts.rows {
            let id = row.get::<String>("id").unwrap_or_default();
            let version = row.get::<u32>("key_version").unwrap_or_default();
            if old_version != Some(version) {
                old_key = Some(self.keys.dek(scope, version).await?);
                old_version = Some(version);
            }
            let key = old_key.as_ref().expect("the key is set with the version");
            let sealed = crate::facts::reseal(
                key,
                version,
                &active.dek,
                active.version,
                scope,
                &id,
                &row.get::<String>("sealed").unwrap_or_default(),
            )?;
            self.db
                .execute(&Statement::with_values(
                    "UPDATE facts SET sealed = ?, key_version = ? \
                     WHERE scope = ? AND id = ? AND key_version = ?",
                    vec![
                        text(&sealed),
                        int(active.version),
                        text(scope),
                        text(&id),
                        int(version),
                    ],
                ))
                .await
                .map_err(PageError::Store)?;
            report.rewritten += 1;
        }
        drop(old_key);

        // App memories (issue #146) are sealed in their rows the same way.
        let memories = self
            .db
            .query(&Statement::with_values(
                "SELECT id, sealed, key_version FROM app_memories \
                 WHERE scope = ? AND key_version <> ? ORDER BY key_version ASC, id ASC",
                vec![text(scope), int(active.version)],
            ))
            .await
            .map_err(PageError::Store)?;
        let mut old_version: Option<u32> = None;
        let mut old_key = None;
        for row in &memories.rows {
            let id = row.get::<String>("id").unwrap_or_default();
            let version = row.get::<u32>("key_version").unwrap_or_default();
            if old_version != Some(version) {
                old_key = Some(self.keys.dek(scope, version).await?);
                old_version = Some(version);
            }
            let key = old_key.as_ref().expect("the key is set with the version");
            let sealed = crate::memories::reseal(
                key,
                version,
                &active.dek,
                active.version,
                scope,
                &id,
                &row.get::<String>("sealed").unwrap_or_default(),
            )?;
            self.db
                .execute(&Statement::with_values(
                    "UPDATE app_memories SET sealed = ?, key_version = ? \
                     WHERE scope = ? AND id = ? AND key_version = ?",
                    vec![
                        text(&sealed),
                        int(active.version),
                        text(scope),
                        text(&id),
                        int(version),
                    ],
                ))
                .await
                .map_err(PageError::Store)?;
            report.rewritten += 1;
        }
        drop(old_key);

        // Titles are sealed on their head rows under the same scope key, so
        // they move too: each re-sealed and its row repointed. One that no
        // longer opens is cleared (the backfill names it again from its
        // body) rather than left pinning a key the rotation must retire.
        let titled = self
            .db
            .query(&Statement::with_values(
                "SELECT slug, title_sealed, title_key_version FROM pages \
                 WHERE scope = ? AND title_key_version > 0 AND title_key_version <> ? \
                 ORDER BY title_key_version ASC, slug ASC",
                vec![text(scope), int(active.version)],
            ))
            .await
            .map_err(PageError::Store)?;
        let mut old_version: Option<u32> = None;
        let mut old_key = None;
        for row in &titled.rows {
            let slug = row.get::<String>("slug").unwrap_or_default();
            let version = row.get::<u32>("title_key_version").unwrap_or_default();
            if old_version != Some(version) {
                old_key = Some(self.keys.dek(scope, version).await?);
                old_version = Some(version);
            }
            let key = old_key.as_ref().expect("the key is set with the version");
            let sealed = row.get::<String>("title_sealed").unwrap_or_default();
            let (resealed, to) = match open_title(key, version, scope, &slug, &sealed) {
                Some(title) => (
                    Some(seal_title(
                        &active.dek,
                        active.version,
                        scope,
                        &slug,
                        &title,
                    )?),
                    active.version,
                ),
                None => (None, 0),
            };
            self.db
                .execute(&Statement::with_values(
                    "UPDATE pages SET title_sealed = ?, title_key_version = ? \
                     WHERE scope = ? AND slug = ? AND title_key_version = ?",
                    vec![
                        SeaValue::String(resealed.map(Box::new)),
                        int(to),
                        text(scope),
                        text(&slug),
                        int(version),
                    ],
                ))
                .await
                .map_err(PageError::Store)?;
            // Not counted in `rewritten`: a title is a column of the head
            // row, like its index rows, not a body.
        }
        drop(old_key);
        drop(active);

        // The blind index follows the bodies. A head already on the active key
        // is left alone: its rows are MACed under the key a search will ask
        // for.
        for (slug, markdown) in &reindexed {
            let mut statements = vec![Statement::with_values(
                "DELETE FROM page_terms WHERE scope = ? AND slug = ?",
                vec![text(scope), text(slug)],
            )];
            for term in index_key.page_terms(slug, markdown) {
                statements.push(Statement::with_values(
                    "INSERT INTO page_terms (scope, term, slug) VALUES (?, ?, ?)",
                    vec![text(scope), text(&term), text(slug)],
                ));
            }
            self.db
                .batch_atomic(&statements)
                .await
                .map_err(PageError::Store)?;
        }

        // Retire what is finished: one still inside the window, or still named
        // by a body, stays and is reported as pending for a later run.
        for version in self.superseded_versions(scope).await? {
            if self.past_grace(scope, version).await? && self.keys.retire(scope, version).await? > 0
            {
                report.retired.push(version);
            } else {
                report.pending.push(version);
            }
        }
        Ok(report)
    }

    /// The live key versions of a scope that a newer one has superseded,
    /// oldest first.
    async fn superseded_versions(&self, scope: &str) -> Result<Vec<u32>, PageError> {
        let rows = self
            .db
            .query(&Statement::with_values(
                "SELECT key_version FROM scope_keys \
                 WHERE scope = ? AND wrapped_dek IS NOT NULL AND key_version < ( \
                     SELECT MAX(key_version) FROM scope_keys WHERE scope = ? AND wrapped_dek IS NOT NULL) \
                 ORDER BY key_version ASC",
                vec![text(scope), text(scope)],
            ))
            .await
            .map_err(PageError::Store)?;
        Ok(rows
            .rows
            .iter()
            .filter_map(|row| row.get::<u32>("key_version"))
            .collect())
    }

    /// Whether a version was superseded long enough ago that no request can
    /// still be holding it.
    async fn past_grace(&self, scope: &str, version: u32) -> Result<bool, PageError> {
        let Some(since) = self.keys.superseded_at(scope, version).await? else {
            return Ok(false);
        };
        let Ok(since) = OffsetDateTime::parse(&since, &Rfc3339) else {
            return Ok(false);
        };
        Ok(self.clock.now() - since >= RETIRE_GRACE)
    }

    /// The current time as RFC 3339, the storage format for every timestamp.
    /// A real clock reading cannot fail to format, so an empty stamp is
    /// preferred to failing an otherwise valid write.
    fn now(&self) -> String {
        self.clock.now().format(&Rfc3339).unwrap_or_default()
    }

    /// Opens a sealed body with the key version D1 records on its version row,
    /// so an envelope written before a rotation still reads after one. A row
    /// without a `key_version` falls back to the header — the one place it is
    /// trusted, because `keys::open` checks it against the key it unwrapped.
    async fn open_body(
        &self,
        scope: &str,
        body_key: &str,
        key_version: Option<u32>,
        envelope: &[u8],
    ) -> Result<String, PageError> {
        let version = key_version
            .filter(|v| *v > 0)
            .map_or_else(|| keys::envelope_version(envelope), Ok)?;
        let dek = self.keys.dek(scope, version).await?;
        let plaintext = keys::open(
            &dek,
            version,
            &keys::body_aad(scope, body_key, version),
            envelope,
        )?;
        Ok(String::from_utf8_lossy(&plaintext).into_owned())
    }
}

pub(crate) fn text(value: &str) -> SeaValue {
    SeaValue::String(Some(Box::new(value.to_owned())))
}

pub(crate) fn int(value: u32) -> SeaValue {
    SeaValue::BigInt(Some(i64::from(value)))
}

fn opt_int(value: Option<u32>) -> SeaValue {
    SeaValue::BigInt(value.map(i64::from))
}

fn version_meta_from(row: &Row) -> Result<VersionMeta, PageError> {
    let raw = row
        .get::<String>("author_kind")
        .ok_or_else(|| PageError::Corrupt("page_versions.author_kind is not text".to_owned()))?;
    let author_kind = AuthorKind::parse(&raw)
        .ok_or_else(|| PageError::Corrupt(format!("unknown author kind `{raw}`")))?;
    Ok(VersionMeta {
        version: row.get::<u32>("version").unwrap_or_default(),
        author_kind,
        author: row.get::<String>("author").unwrap_or_default(),
        base_version: row.get::<u32>("base_version"),
        created_at: row.get::<String>("created_at").unwrap_or_default(),
    })
}

/// One row of the brain map's live feed: a page read, searched or written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityRow {
    /// Monotonic per table: the feed's cursor.
    pub id: i64,
    pub scope: String,
    pub slug: String,
    /// `read`, `search` or `write`.
    pub kind: String,
    /// Who did it, in words: `Claude`, `CLI` or `web`.
    pub actor: String,
    /// RFC 3339, as the store stamps it.
    pub at: String,
}

/// How long the live feed keeps a row.
const ACTIVITY_TTL_HOURS: i64 = 24;

impl PageStore {
    /// Records one read, search or write of a page for the live feed.
    /// Best effort: a failure is dropped, because the read or write it
    /// describes must never fail for want of its log line. Also prunes the
    /// rows older than a day.
    pub async fn record_activity(
        &self,
        workspace: &str,
        scope: &str,
        slug: &str,
        kind: &str,
        actor: &str,
    ) {
        let now = self.clock.now();
        let at = now.format(&Rfc3339).unwrap_or_default();
        let candidate = activity_id(now, &self.id_gen.ulid());
        let _ = self
            .db
            .execute(&Statement::with_values(
                // Never below the newest row: the cursor stays monotonic even
                // when two writes share a microsecond (or a test's clock).
                "INSERT INTO graph_activity (id, workspace_id, scope, slug, kind, actor, at) \
                 SELECT CASE WHEN ? > COALESCE(MAX(id), 0) THEN ? ELSE COALESCE(MAX(id), 0) + 1 END, \
                        ?, ?, ?, ?, ?, ? \
                 FROM graph_activity",
                vec![
                    SeaValue::BigInt(Some(candidate)),
                    SeaValue::BigInt(Some(candidate)),
                    text(workspace),
                    text(scope),
                    text(slug),
                    text(kind),
                    text(actor),
                    text(&at),
                ],
            ))
            .await;
        let cutoff = (now - time::Duration::hours(ACTIVITY_TTL_HOURS))
            .format(&Rfc3339)
            .unwrap_or_default();
        let _ = self
            .db
            .execute(&Statement::with_values(
                "DELETE FROM graph_activity WHERE at < ?",
                vec![text(&cutoff)],
            ))
            .await;
    }
}

/// A workspace's live-feed rows, oldest first. With `since`, the rows after
/// that id (the first `cap` of them); without it, the newest `cap`.
///
/// # Errors
///
/// [`DbError`] when the query fails.
/// A feed row id that grows with time: microseconds since the epoch times
/// 4096, plus 12 bits taken from a fresh ULID so two writes in the same
/// microsecond rarely collide (a collision only drops that one best-effort
/// row). Fits an `i64` until the year 2040+ by a wide margin.
fn activity_id(now: time::OffsetDateTime, ulid: &str) -> i64 {
    let micros = i64::try_from(now.unix_timestamp_nanos() / 1_000).unwrap_or(0);
    let low = ulid
        .bytes()
        .rev()
        .take(3)
        .fold(0_i64, |acc, b| (acc << 5) ^ i64::from(b))
        & 0xFFF;
    micros.saturating_mul(4096).saturating_add(low)
}

pub async fn activity_rows(
    db: &dyn Database,
    workspace: &str,
    since: Option<i64>,
    cap: usize,
) -> Result<Vec<ActivityRow>, DbError> {
    let cap = SeaValue::BigInt(Some(i64::try_from(cap).unwrap_or(i64::MAX)));
    let columns =
        "SELECT id, scope, slug, kind, actor, at FROM graph_activity WHERE workspace_id = ?";
    let rows = match since {
        Some(after) => {
            db.query(&Statement::with_values(
                format!("{columns} AND id > ? ORDER BY id ASC LIMIT ?"),
                vec![text(workspace), SeaValue::BigInt(Some(after)), cap],
            ))
            .await?
            .rows
        }
        None => {
            let mut newest = db
                .query(&Statement::with_values(
                    format!("{columns} ORDER BY id DESC LIMIT ?"),
                    vec![text(workspace), cap],
                ))
                .await?
                .rows;
            newest.reverse();
            newest
        }
    };
    Ok(rows
        .iter()
        .map(|row| ActivityRow {
            id: row.get::<i64>("id").unwrap_or_default(),
            scope: row.get::<String>("scope").unwrap_or_default(),
            slug: row.get::<String>("slug").unwrap_or_default(),
            kind: row.get::<String>("kind").unwrap_or_default(),
            actor: row.get::<String>("actor").unwrap_or_default(),
            at: row.get::<String>("at").unwrap_or_default(),
        })
        .collect())
}
