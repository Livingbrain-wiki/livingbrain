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

use crate::entity::{self, EntityType, Frontmatter};
use crate::keys::{self, ScopeKeys};

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

        let (_, body) = entity::parse_frontmatter(&write.markdown, write.entity_type)?;
        let links = entity::extract_links(&body)?;

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
        let terms: Vec<String> = keys::tokenize(&write.markdown)
            .iter()
            .map(|token| index_key.mac(token))
            .collect();
        let key_version = active.version;
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
                 (scope, slug, entity_type, head_version, human_pending_version, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?)",
                vec![
                    text(scope),
                    text(slug),
                    text(write.entity_type.as_str()),
                    int(version),
                    opt_int(pending),
                    text(&now),
                ],
            )),
            // The head guard alone cannot stop a race: `batch_atomic` rolls
            // back on an error, not on a zero-row update. The `page_versions`
            // primary key does — two writers of the same next version collide
            // on the insert above, and that failure is read back as a conflict.
            Some(row) => statements.push(Statement::with_values(
                "UPDATE pages SET head_version = ?, human_pending_version = ?, updated_at = ? \
                 WHERE scope = ? AND slug = ? AND head_version = ?",
                vec![
                    int(version),
                    opt_int(pending),
                    text(&now),
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

    /// Pages in `scopes` whose current body contains **every** token of
    /// `query`, as `(scope, slug)` and never anything else.
    ///
    /// The asker supplies the scopes; this never widens them, never runs a
    /// query without a scope predicate, and never decrypts a body. Each query
    /// token's blind-index MAC is computed under every live key version of the
    /// scope — so a page written before a rotation is still found after one —
    /// and the slugs the tokens match are intersected. A scope with no live key
    /// (shredded, or never written) has no MACs to compute and contributes
    /// nothing, and a query with no token in it matches nothing: there is no
    /// way to say "any page" through a blind index.
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
        for scope in scopes {
            if !entity::is_slug(scope) {
                return Err(PageError::InvalidScope((*scope).to_owned()));
            }
        }
        let tokens = keys::tokenize(query);
        if tokens.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }

        let mut hits: BTreeSet<SearchHit> = BTreeSet::new();
        for scope in scopes {
            let index_keys = self.keys.index_keys(scope).await?;
            if index_keys.is_empty() {
                continue;
            }
            let mut slugs: Option<BTreeSet<String>> = None;
            for token in &tokens {
                // Every live version of this scope MACs the same token; a
                // page indexed under any of them is a hit.
                let mut values = vec![text(scope)];
                let placeholders = index_keys
                    .iter()
                    .map(|key| {
                        values.push(text(&key.mac(token)));
                        "?"
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let rows = self
                    .db
                    .query(&Statement::with_values(
                        format!(
                            "SELECT DISTINCT slug FROM page_terms \
                             WHERE scope = ? AND term IN ({placeholders}) LIMIT ?"
                        ),
                        {
                            values.push(SeaValue::BigInt(Some(
                                i64::try_from(limit).unwrap_or(i64::MAX),
                            )));
                            values
                        },
                    ))
                    .await
                    .map_err(PageError::Store)?;
                let found: BTreeSet<String> = rows
                    .rows
                    .iter()
                    .filter_map(|row| row.get::<String>("slug"))
                    .collect();
                slugs = Some(match slugs {
                    None => found,
                    Some(seen) => seen.intersection(&found).cloned().collect(),
                });
            }
            if let Some(slugs) = slugs {
                hits.extend(
                    slugs
                        .into_iter()
                        .map(|slug| SearchHit {
                            scope: (*scope).to_owned(),
                            slug,
                        })
                        .collect::<Vec<_>>(),
                );
            }
        }
        Ok(hits.into_iter().take(limit).collect())
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
        drop(active);

        // The blind index follows the bodies. A head already on the active key
        // is left alone: its rows are MACed under the key a search will ask
        // for.
        for (slug, markdown) in &reindexed {
            let mut statements = vec![Statement::with_values(
                "DELETE FROM page_terms WHERE scope = ? AND slug = ?",
                vec![text(scope), text(slug)],
            )];
            for token in keys::tokenize(markdown) {
                statements.push(Statement::with_values(
                    "INSERT INTO page_terms (scope, term, slug) VALUES (?, ?, ?)",
                    vec![text(scope), text(&index_key.mac(&token)), text(slug)],
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
