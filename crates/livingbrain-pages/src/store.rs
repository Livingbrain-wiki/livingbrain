//! The page store: versioned entities in D1, immutable Markdown bodies in the
//! blob store. The module rules it enforces are in [`crate`].

use std::sync::Arc;

use cratefield_core::{Blob, BlobError, Clock, Database, DbError, IdGen, Row, Statement};
use sea_query::Value as SeaValue;
use time::format_description::well_known::Rfc3339;

use crate::entity::{self, EntityType, Frontmatter};

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
            Self::Blob(error) => write!(f, "blob error: {error}"),
            Self::Store(error) => write!(f, "store error: {error}"),
        }
    }
}

impl std::error::Error for PageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
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

/// The versioned page store.
pub struct PageStore {
    db: Arc<dyn Database>,
    blob: Arc<dyn Blob>,
    clock: Arc<dyn Clock>,
    id_gen: Arc<dyn IdGen>,
}

impl PageStore {
    /// A store over the four ports it needs.
    #[must_use]
    pub fn new(
        db: Arc<dyn Database>,
        blob: Arc<dyn Blob>,
        clock: Arc<dyn Clock>,
        id_gen: Arc<dyn IdGen>,
    ) -> Self {
        Self {
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
        self.blob
            .put(&body_key, write.markdown.as_bytes(), "text/markdown")
            .await
            .map_err(PageError::Blob)?;

        let now = self.now();
        let pending = match write.author.kind() {
            AuthorKind::Human => Some(version),
            AuthorKind::Brain => None,
        };

        let mut statements = Vec::with_capacity(6 + links.len());
        statements.push(Statement::with_values(
            "INSERT INTO page_versions \
             (scope, slug, version, author_kind, author, base_version, body_key, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            vec![
                text(scope),
                text(slug),
                int(version),
                text(write.author.kind().as_str()),
                text(write.author.label()),
                opt_int(write.base_version),
                text(&body_key),
                text(&now),
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
                "SELECT body_key FROM page_versions WHERE scope = ? AND slug = ? AND version = ?",
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
        let markdown = String::from_utf8_lossy(&object.bytes).into_owned();
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

    /// The current time as RFC 3339, the storage format for every timestamp.
    /// A real clock reading cannot fail to format, so an empty stamp is
    /// preferred to failing an otherwise valid write.
    fn now(&self) -> String {
        self.clock.now().format(&Rfc3339).unwrap_or_default()
    }
}

fn text(value: &str) -> SeaValue {
    SeaValue::String(Some(Box::new(value.to_owned())))
}

fn int(value: u32) -> SeaValue {
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
