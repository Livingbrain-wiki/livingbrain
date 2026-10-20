//! App memories (issue #146): what a connected app keeps for one of its end
//! members — "Aura remembers I am vegetarian" — in the member's own private
//! scope, sealed like every other body and **hard-deletable**.
//!
//! A memory is a row, not a page: a page is versioned and a version is
//! forever, and the whole point of `brain_memory_forget` is that it is not.
//! A row is sealed under the subject's scope key (AAD-bound to the scope, the
//! id and the key version), so it reads like a fact does and moves with a key
//! rotation ([`PageStore::reencrypt_scope`](crate::PageStore::reencrypt_scope)).
//!
//! The scope is the caller's to compute — `page_scope(workspace, User
//! ("<app>:<subject>"))` — and every method takes it as the only thing that
//! selects rows, so a memory in one scope is unreachable through another.
//! Forgetting is a `DELETE`; the scope key is left alone, because a shredded
//! scope can never be written again and a member who clears their memory is
//! entitled to start a new one. The whole-workspace erasure
//! ([`PageStore::forget_scope`](crate::PageStore::forget_scope)) does shred.

use std::collections::HashMap;
use std::sync::Arc;

use base64::Engine as _;
use cratefield_core::{Clock, Database, IdGen, Row, Statement};
use cratefield_kms::{Dek, Kms};
use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;

use crate::entity;
use crate::keys::{self, ScopeKeys};
use crate::store::{PageError, int, text};

/// The AAD domain of a sealed memory; the scope, the id and the key version
/// follow it, NUL-separated.
const MEMORY_AAD_DOMAIN: &str = "livingbrain-pages/memory/v1";

/// The longest memory a note may carry, in characters.
pub const MAX_MEMORY_CHARS: usize = 2_000;
/// The longest source label (a conversation id) a note may carry.
pub const MAX_SOURCE_CHARS: usize = 200;
/// The most memories one scope may hold; a note past it is refused so a
/// runaway client cannot grow a subject without bound.
pub const MAX_MEMORIES: usize = 1_000;

fn memory_aad(scope: &str, id: &str, version: u32) -> String {
    format!("{MEMORY_AAD_DOMAIN}\0{scope}\0{id}\0{version}")
}

/// One memory, opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Memory {
    /// `mem_<ULID>`.
    pub id: String,
    /// What the member chose to keep.
    pub text: String,
    /// The conversation it came from.
    pub source: String,
    /// When it was kept, RFC 3339.
    pub created_at: String,
}

/// Why a memory could not be written.
#[derive(Debug)]
pub enum MemoryError {
    /// The text or source is empty or past its limit, or the id is malformed.
    Invalid(&'static str),
    /// The scope already holds [`MAX_MEMORIES`].
    Full,
    /// The store failed.
    Store(PageError),
}

impl std::fmt::Display for MemoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(why) => f.write_str(why),
            Self::Full => write!(f, "this subject already holds {MAX_MEMORIES} memories"),
            Self::Store(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for MemoryError {}

impl From<PageError> for MemoryError {
    fn from(error: PageError) -> Self {
        Self::Store(error)
    }
}

/// The memory store: over the database, the key custodian, the clock and the
/// id generator.
pub struct MemoryStore {
    db: Arc<dyn Database>,
    keys: ScopeKeys,
    clock: Arc<dyn Clock>,
    id_gen: Arc<dyn IdGen>,
}

/// Whether `id` has the shape of a memory id this store minted.
#[must_use]
pub fn is_memory_id(id: &str) -> bool {
    id.strip_prefix("mem_").is_some_and(|rest| {
        !rest.is_empty() && rest.len() <= 40 && rest.bytes().all(|b| b.is_ascii_alphanumeric())
    })
}

impl MemoryStore {
    /// A store over the ports a [`FactStore`](crate::FactStore) is built from.
    #[must_use]
    pub fn new(
        db: Arc<dyn Database>,
        kms: Arc<dyn Kms>,
        clock: Arc<dyn Clock>,
        id_gen: Arc<dyn IdGen>,
    ) -> Self {
        Self {
            keys: ScopeKeys::new(db.clone(), kms, clock.clone()),
            db,
            clock,
            id_gen,
        }
    }

    fn check_scope(scope: &str) -> Result<(), PageError> {
        if entity::is_slug(scope) {
            Ok(())
        } else {
            Err(PageError::InvalidScope(scope.to_owned()))
        }
    }

    /// Keeps `text` for the subject whose page scope is `scope`, returning
    /// its id. The caller has already screened and redacted the text; this
    /// enforces the limits and seals it.
    ///
    /// # Errors
    ///
    /// [`MemoryError::Invalid`] for an empty or oversized text or source,
    /// [`MemoryError::Full`], and [`MemoryError::Store`] for a database or
    /// key failure (a shredded scope included).
    pub async fn note(
        &self,
        workspace_id: &str,
        app: &str,
        scope: &str,
        memory: &str,
        source: &str,
    ) -> Result<String, MemoryError> {
        Self::check_scope(scope)?;
        if memory.trim().is_empty() {
            return Err(MemoryError::Invalid("`text` is required"));
        }
        if memory.chars().count() > MAX_MEMORY_CHARS {
            return Err(MemoryError::Invalid("`text` is too long"));
        }
        if source.chars().count() > MAX_SOURCE_CHARS {
            return Err(MemoryError::Invalid("`source` is too long"));
        }
        let held = self
            .db
            .query(&Statement::with_values(
                "SELECT COUNT(*) AS n FROM app_memories WHERE scope = ?",
                vec![text(scope)],
            ))
            .await
            .map_err(PageError::Store)?;
        let held = held
            .first()
            .and_then(|row| row.get::<i64>("n"))
            .unwrap_or(0);
        if usize::try_from(held).unwrap_or(usize::MAX) >= MAX_MEMORIES {
            return Err(MemoryError::Full);
        }
        let id = format!("mem_{}", self.id_gen.ulid());
        let active = self.keys.active(scope).await?;
        let payload = json!({ "text": memory, "source": source }).to_string();
        let envelope = keys::seal(
            &active.dek,
            active.version,
            &memory_aad(scope, &id, active.version),
            payload.as_bytes(),
        )?;
        let sealed = base64::engine::general_purpose::STANDARD.encode(envelope);
        let now = self.clock.now().format(&Rfc3339).unwrap_or_default();
        self.db
            .execute(&Statement::with_values(
                "INSERT INTO app_memories \
                   (scope, id, workspace_id, app, sealed, key_version, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
                vec![
                    text(scope),
                    text(&id),
                    text(workspace_id),
                    text(app),
                    text(&sealed),
                    int(active.version),
                    text(&now),
                ],
            ))
            .await
            .map_err(PageError::Store)?;
        Ok(id)
    }

    /// The newest `limit` memories of `scope`. A scope with no live key (never
    /// written, or shredded) holds none.
    ///
    /// # Errors
    ///
    /// [`PageError`] for a database or key failure, or a row that will not
    /// open.
    pub async fn recall(&self, scope: &str, limit: usize) -> Result<Vec<Memory>, PageError> {
        Self::check_scope(scope)?;
        if limit == 0 {
            return Ok(Vec::new());
        }
        let rows = self
            .db
            .query(&Statement::with_values(
                format!(
                    "SELECT id, sealed, key_version, created_at FROM app_memories \
                     WHERE scope = ? ORDER BY created_at DESC, id DESC LIMIT {}",
                    limit.min(MAX_MEMORIES)
                ),
                vec![text(scope)],
            ))
            .await
            .map_err(PageError::Store)?;
        let mut deks: HashMap<u32, Dek> = HashMap::new();
        let mut out = Vec::with_capacity(rows.rows.len());
        for row in &rows.rows {
            let version = row.get::<u32>("key_version").unwrap_or_default();
            if let std::collections::hash_map::Entry::Vacant(slot) = deks.entry(version) {
                slot.insert(self.keys.dek(scope, version).await?);
            }
            out.push(open_row(row, &deks[&version], scope, version)?);
        }
        Ok(out)
    }

    /// Hard-deletes memory `id` from `scope`. How many rows went: `0` for an
    /// id this scope does not hold — the same answer as for one another
    /// subject holds.
    ///
    /// # Errors
    ///
    /// [`MemoryError::Invalid`] for a malformed id, [`MemoryError::Store`]
    /// for a database failure.
    pub async fn forget(&self, scope: &str, id: &str) -> Result<u64, MemoryError> {
        Self::check_scope(scope)?;
        if !is_memory_id(id) {
            return Err(MemoryError::Invalid("`id` is not a memory id"));
        }
        self.db
            .execute(&Statement::with_values(
                "DELETE FROM app_memories WHERE scope = ? AND id = ?",
                vec![text(scope), text(id)],
            ))
            .await
            .map_err(|error| MemoryError::Store(PageError::Store(error)))
    }

    /// Hard-deletes everything `scope` holds: its memories, and anything
    /// derived into the same scope (temporal facts and the tidy passes over
    /// them), in one atomic batch. Answers how many memories went.
    ///
    /// The scope's key is **not** destroyed: a shredded scope can never be
    /// written again, and a member who clears their memory may start a new
    /// one. Nothing sealed under the key is left to read once its rows are
    /// gone.
    ///
    /// # Errors
    ///
    /// [`PageError`] for a database failure.
    pub async fn forget_all(&self, scope: &str) -> Result<u64, PageError> {
        Self::check_scope(scope)?;
        let held = self
            .db
            .query(&Statement::with_values(
                "SELECT COUNT(*) AS n FROM app_memories WHERE scope = ?",
                vec![text(scope)],
            ))
            .await
            .map_err(PageError::Store)?;
        let held = held
            .first()
            .and_then(|row| row.get::<i64>("n"))
            .unwrap_or(0);
        self.db
            .batch_atomic(&[
                Statement::with_values(
                    "DELETE FROM app_memories WHERE scope = ?",
                    vec![text(scope)],
                ),
                Statement::with_values("DELETE FROM facts WHERE scope = ?", vec![text(scope)]),
                Statement::with_values(
                    "DELETE FROM fact_dream_decisions WHERE scope = ?",
                    vec![text(scope)],
                ),
                Statement::with_values(
                    "DELETE FROM fact_dreams WHERE scope = ?",
                    vec![text(scope)],
                ),
            ])
            .await
            .map_err(PageError::Store)?;
        Ok(u64::try_from(held).unwrap_or(0))
    }

    /// Every page scope in `workspace_id` that holds app memories or a
    /// published page — what a workspace erasure shreds beside the member
    /// spaces.
    ///
    /// # Errors
    ///
    /// [`PageError::Store`].
    pub async fn app_scopes(&self, workspace_id: &str) -> Result<Vec<String>, PageError> {
        app_scopes(&*self.db, workspace_id).await
    }
}

/// [`MemoryStore::app_scopes`] over a bare database, for the erasure hook.
///
/// # Errors
///
/// [`PageError::Store`].
pub async fn app_scopes(db: &dyn Database, workspace_id: &str) -> Result<Vec<String>, PageError> {
    let rows = db
        .query(&Statement::with_values(
            "SELECT scope FROM app_memories WHERE workspace_id = ? \
             UNION SELECT scope FROM public_pages WHERE workspace_id = ?",
            vec![text(workspace_id), text(workspace_id)],
        ))
        .await
        .map_err(PageError::Store)?;
    Ok(rows
        .rows
        .iter()
        .filter_map(|row| row.get::<String>("scope"))
        .collect())
}

fn open_row(row: &Row, dek: &Dek, scope: &str, version: u32) -> Result<Memory, PageError> {
    let id = row.get::<String>("id").unwrap_or_default();
    let envelope = base64::engine::general_purpose::STANDARD
        .decode(row.get::<String>("sealed").unwrap_or_default())
        .map_err(|_| PageError::Crypto(format!("memory `{id}` is not base64")))?;
    let plain = keys::open(dek, version, &memory_aad(scope, &id, version), &envelope)?;
    let payload: Value = serde_json::from_slice(&plain)
        .map_err(|_| PageError::Corrupt(format!("memory `{id}` payload is not JSON")))?;
    let field = |key: &str| {
        payload
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    Ok(Memory {
        id,
        text: field("text"),
        source: field("source"),
        created_at: row.get::<String>("created_at").unwrap_or_default(),
    })
}

/// A memory re-sealed under a new key, for a rotation.
pub(crate) fn reseal(
    old: &Dek,
    from: u32,
    new: &Dek,
    to: u32,
    scope: &str,
    id: &str,
    sealed: &str,
) -> Result<String, PageError> {
    let envelope = base64::engine::general_purpose::STANDARD
        .decode(sealed)
        .map_err(|_| PageError::Crypto(format!("memory `{id}` is not base64")))?;
    let plain = keys::open(old, from, &memory_aad(scope, id, from), &envelope)?;
    let envelope = keys::seal(new, to, &memory_aad(scope, id, to), &plain)?;
    Ok(base64::engine::general_purpose::STANDARD.encode(envelope))
}
