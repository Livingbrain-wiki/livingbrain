//! Per-scope data keys, the body envelope, and the blind index (issue #43,
//! ADR 0003). [`ScopeKeys`] owns a scope's 32-byte data key: generated once per
//! key version, wrapped by the [`Kms`] port, stored in D1 as base64. The
//! plaintext key never touches the database and never outlives the call that
//! needed it — a crypto-shred has to take effect now, everywhere, or it is not
//! one.

use std::collections::BTreeSet;
use std::sync::Arc;

use base64::Engine as _;
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, Payload},
};
use cratefield_core::{Clock, Database, DbError, Statement};
use cratefield_kms::{Dek, Kms};
use hmac::{Hmac, Mac};
use sea_query::Value as SeaValue;
use sha2::Sha256;
use time::format_description::well_known::Rfc3339;
use zeroize::Zeroizing;

use crate::entity;
use crate::store::{PageError, int, text};

/// The first four bytes of a sealed body: `lbe1`. A body in the blob store
/// that does not open with this is not one of ours.
const MAGIC: &[u8; 4] = b"lbe1";
/// XChaCha20's nonce, in bytes. 24, so a random one never repeats.
const NONCE_LEN: usize = 24;
/// magic + key version + nonce.
const HEADER_LEN: usize = 4 + 4 + NONCE_LEN;
/// The AAD domain. The scope, body key and key version follow it,
/// NUL-separated — see [`body_aad`].
const AAD_DOMAIN: &str = "livingbrain-pages/body/v1";
/// Derives the blind index key from the DEK. Its own domain string, so an
/// index key can never be mistaken for a key that seals anything.
const BLIND_INDEX_DOMAIN: &[u8] = b"livingbrain-pages/blind-index/v1";
/// The R2 content type of a sealed body: it is not Markdown any more.
pub const SEALED_CONTENT_TYPE: &str = "application/octet-stream";
/// The shortest token the blind index stores: one letter is noise.
const MIN_TOKEN_LEN: usize = 2;
/// The most distinct tokens indexed per page, so the index cannot be a
/// denial of service against D1.
const MAX_TOKENS: usize = 2000;
/// How many times a key lookup will re-read before giving up: a scope that
/// cannot settle in a few rounds is a fault worth reporting.
const SETTLE_ROUNDS: usize = 4;

/// A data key with the version it belongs to, unwrapped for this call only.
pub(crate) struct ActiveKey {
    pub(crate) version: u32,
    pub(crate) dek: Dek,
}

/// The AAD a body at `body_key` in `scope`, sealed under `version`, is bound
/// to.
#[must_use]
pub(crate) fn body_aad(scope: &str, body_key: &str, version: u32) -> String {
    format!("{AAD_DOMAIN}\0{scope}\0{body_key}\0{version}")
}

/// Splits a body into the tokens the blind index stores: lowercased, split on
/// every non-alphanumeric character, at least [`MIN_TOKEN_LEN`] characters,
/// deduplicated, and at most [`MAX_TOKENS`] of them in first-seen order.
#[must_use]
pub(crate) fn tokenize(markdown: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for word in markdown.split(|c: char| !c.is_alphanumeric()) {
        let token = word.to_lowercase();
        if token.chars().count() < MIN_TOKEN_LEN {
            continue;
        }
        if seen.insert(token.clone()) {
            tokens.push(token);
            if tokens.len() >= MAX_TOKENS {
                break;
            }
        }
    }
    tokens
}

/// Hex, the shape the blind index stores a MAC in. Hand-rolled rather than
/// pulled in as a dependency for one loop.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    out
}

/// Seals `plaintext` under `dek` into an envelope: `lbe1`, the key version
/// big-endian, a fresh 24-byte nonce, then the ciphertext and its tag.
pub(crate) fn seal(
    dek: &Dek,
    key_version: u32,
    aad: &str,
    plaintext: &[u8],
) -> Result<Vec<u8>, PageError> {
    let mut nonce = [0_u8; NONCE_LEN];
    getrandom::fill(&mut nonce)
        .map_err(|err| PageError::Crypto(format!("the OS random source failed: {err}")))?;
    let nonce = XNonce::try_from(&nonce[..])
        .map_err(|_| PageError::Crypto("a nonce is not 24 bytes".to_owned()))?;
    let sealed = XChaCha20Poly1305::new_from_slice(dek.expose())
        .expect("a Dek is always DEK_LEN bytes, which is what the cipher takes")
        .encrypt(
            &nonce,
            Payload {
                msg: plaintext,
                aad: aad.as_bytes(),
            },
        )
        .map_err(|_| PageError::Crypto("sealing a body failed".to_owned()))?;

    let mut envelope = Vec::with_capacity(HEADER_LEN + sealed.len());
    envelope.extend_from_slice(MAGIC);
    envelope.extend_from_slice(&key_version.to_be_bytes());
    envelope.extend_from_slice(nonce.as_slice());
    envelope.extend_from_slice(&sealed);
    Ok(envelope)
}

/// The key version an envelope was sealed under, read from its header.
pub fn envelope_version(envelope: &[u8]) -> Result<u32, PageError> {
    let header = envelope
        .get(..HEADER_LEN)
        .ok_or_else(|| PageError::Crypto("a sealed body is truncated".to_owned()))?;
    if &header[..4] != MAGIC {
        return Err(PageError::Crypto(
            "a body in the blob store is not a sealed envelope".to_owned(),
        ));
    }
    let version = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
    if version == 0 {
        return Err(PageError::Crypto(
            "a sealed body claims key version 0, which no scope has".to_owned(),
        ));
    }
    Ok(version)
}

/// Opens an envelope with the key it names. The header's version is inside
/// the AAD, so it cannot be edited to point at another key's body; this check
/// is that the caller unwrapped the DEK it claims to.
pub(crate) fn open(
    dek: &Dek,
    key_version: u32,
    aad: &str,
    envelope: &[u8],
) -> Result<Vec<u8>, PageError> {
    let named = envelope_version(envelope)?;
    if named != key_version {
        return Err(PageError::Crypto(format!(
            "a body claims key version {named} but was opened with version {key_version}"
        )));
    }
    let sealed = &envelope[HEADER_LEN..];
    let nonce = XNonce::try_from(&envelope[8..8 + NONCE_LEN])
        .map_err(|_| PageError::Crypto("a sealed body has no nonce".to_owned()))?;
    XChaCha20Poly1305::new_from_slice(dek.expose())
        .expect("a Dek is always DEK_LEN bytes, which is what the cipher takes")
        .decrypt(
            &nonce,
            Payload {
                msg: sealed,
                aad: aad.as_bytes(),
            },
        )
        .map_err(|_| {
            PageError::Crypto(
                "a sealed body failed authentication: wrong key, wrong scope, or altered"
                    .to_owned(),
            )
        })
}

/// One live scope key version's blind index key. The DEK it came from is not
/// held: this is derived from it and zeroised with it.
pub(crate) struct IndexKey {
    /// `HMAC-SHA256(dek, BLIND_INDEX_DOMAIN)`.
    index_key: Zeroizing<[u8; 32]>,
}

impl IndexKey {
    /// The index key for a DEK already unwrapped for this call, so a write
    /// never needs a second KMS round trip.
    #[must_use]
    pub(crate) fn derive(dek: &Dek) -> Self {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(dek.expose())
            .expect("HMAC accepts a key of any length");
        mac.update(BLIND_INDEX_DOMAIN);
        Self {
            index_key: Zeroizing::new(mac.finalize().into_bytes().into()),
        }
    }

    /// The blind-index MAC of one already-normalized token, as hex.
    #[must_use]
    pub(crate) fn mac(&self, token: &str) -> String {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(self.index_key.as_slice())
            .expect("HMAC accepts a key of any length");
        mac.update(token.as_bytes());
        hex(&mac.finalize().into_bytes())
    }
}

/// The scope keys: `scope_keys` in D1, wrapped by the KMS.
pub(crate) struct ScopeKeys {
    db: Arc<dyn Database>,
    kms: Arc<dyn Kms>,
    clock: Arc<dyn Clock>,
}

impl ScopeKeys {
    /// A key set over the database, the key custodian and the clock.
    #[must_use]
    pub(crate) fn new(db: Arc<dyn Database>, kms: Arc<dyn Kms>, clock: Arc<dyn Clock>) -> Self {
        Self { db, kms, clock }
    }

    /// The scope's live key, created on first use and never reusing a
    /// version. Two writers racing for a scope's first key both compute
    /// version 1 and both try to insert it; the primary key lets exactly one
    /// through and the other re-reads and uses the winner's key.
    pub(crate) async fn active(&self, scope: &str) -> Result<ActiveKey, PageError> {
        self.check_slug(scope)?;
        for _ in 0..SETTLE_ROUNDS {
            let rows = self.versions(scope).await?;
            if let Some((version, wrapped)) = live_of(&rows) {
                return Ok(ActiveKey {
                    version,
                    dek: self.unwrap(scope, version, &wrapped).await?,
                });
            }
            if shredded(&rows) {
                return Err(PageError::Shredded(scope.to_owned()));
            }
            self.insert(scope, rows.last().map_or(1, |row| row.version + 1))
                .await?;
        }
        Err(unsettled(scope))
    }

    /// The data key of one specific version, or why there is not one. A
    /// version that never existed and one a finished rotation retired are both
    /// [`PageError::KeyUnavailable`]; one a `forget_scope` took is
    /// [`PageError::Shredded`], because the right answer to that is to stop.
    pub(crate) async fn dek(&self, scope: &str, version: u32) -> Result<Dek, PageError> {
        self.check_slug(scope)?;
        let rows = self.versions(scope).await?;
        let Some(row) = rows.iter().find(|row| row.version == version) else {
            return Err(PageError::KeyUnavailable {
                scope: scope.to_owned(),
                version,
            });
        };
        match row.wrapped_dek.as_deref() {
            Some(wrapped) => self.unwrap(scope, version, wrapped).await,
            None if row.retired_at.is_some() => Err(PageError::KeyUnavailable {
                scope: scope.to_owned(),
                version,
            }),
            None => Err(PageError::Shredded(scope.to_owned())),
        }
    }

    /// Every live version of a scope, newest first, each with its index key.
    /// Unlike [`active`](Self::active) this never creates one: a search must
    /// not resurrect the key of a scope somebody asked us to forget.
    pub(crate) async fn index_keys(&self, scope: &str) -> Result<Vec<IndexKey>, PageError> {
        self.check_slug(scope)?;
        let mut keys = Vec::new();
        for row in self.versions(scope).await? {
            let Some(wrapped) = row.wrapped_dek.as_deref() else {
                continue;
            };
            let dek = self.unwrap(scope, row.version, wrapped).await?;
            keys.push(IndexKey::derive(&dek));
        }
        keys.reverse();
        Ok(keys)
    }

    /// Creates the next version and makes it the active one. The old version
    /// stays live: an envelope sealed under it is still readable until
    /// [`PageStore::reencrypt_scope`](crate::PageStore::reencrypt_scope) has
    /// rewritten it.
    ///
    /// Refused with [`PageError::RotationPending`] while a superseded version
    /// is still live, which is what bounds a scope to two live keys, and so a
    /// search to two unwraps per scope.
    pub(crate) async fn rotate(&self, scope: &str) -> Result<u32, PageError> {
        self.check_slug(scope)?;
        let rows = self.versions(scope).await?;
        if shredded(&rows) {
            return Err(PageError::Shredded(scope.to_owned()));
        }
        if let Some(superseded) = superseded(&rows) {
            return Err(PageError::RotationPending {
                scope: scope.to_owned(),
                superseded,
            });
        }
        let next = rows.last().map_or(1, |row| row.version + 1);
        // The insert is conditional on the scope still having a live key, so
        // a `forget_scope` that lands between the read above and this
        // statement cannot be undone by it: the shred and the insert are one
        // decision, taken by the database rather than by this process.
        let mut values = self.wrapped_values(scope, next).await?;
        values.push(scope.into());
        let affected = self
            .db
            .execute(&Statement::with_values(
                "INSERT INTO scope_keys \
                 (scope, key_version, kms_provider, kms_key_ref, wrapped_dek, created_at, retired_at) \
                 SELECT ?, ?, ?, ?, ?, ?, NULL \
                 WHERE EXISTS (SELECT 1 FROM scope_keys WHERE scope = ? AND wrapped_dek IS NOT NULL) \
                 ON CONFLICT (scope, key_version) DO NOTHING",
                values,
            ))
            .await
            .map_err(PageError::Store)?;
        if affected == 0 {
            return Err(match self.versions(scope).await?.first() {
                Some(row) if row.wrapped_dek.is_none() => PageError::Shredded(scope.to_owned()),
                _ => PageError::RotationPending {
                    scope: scope.to_owned(),
                    superseded: rows.last().map_or(1, |row| row.version),
                },
            });
        }
        Ok(next)
    }

    /// Retires one version, if and only if nothing refers to it. The
    /// `NOT EXISTS` is the whole point and it is in the same statement as the
    /// destroy, so a version a `page_versions` row still names — one a
    /// re-encryption pass has not reached, or a write that fetched the old key
    /// moments before the rotation and is committing right now — cannot be
    /// retired between the check and the write.
    pub(crate) async fn retire(&self, scope: &str, version: u32) -> Result<u64, PageError> {
        self.db
            .execute(&Statement::with_values(
                "UPDATE scope_keys SET wrapped_dek = NULL, retired_at = ? \
                 WHERE scope = ? AND key_version = ? \
                 AND NOT EXISTS (SELECT 1 FROM page_versions \
                                 WHERE scope = ? AND key_version = ?)",
                vec![
                    text(&self.now()),
                    text(scope),
                    int(version),
                    text(scope),
                    int(version),
                ],
            ))
            .await
            .map_err(PageError::Store)
    }

    /// The moment a version stopped being the active one, which is what the
    /// grace window in [`PageStore::reencrypt_scope`](crate::PageStore::reencrypt_scope)
    /// is measured against.
    pub(crate) async fn superseded_at(
        &self,
        scope: &str,
        version: u32,
    ) -> Result<Option<String>, PageError> {
        let rows = self
            .db
            .query(&Statement::with_values(
                "SELECT created_at FROM scope_keys WHERE scope = ? AND key_version = ?",
                vec![text(scope), int(version + 1)],
            ))
            .await
            .map_err(PageError::Store)?;
        Ok(rows.first().and_then(|row| row.get::<String>("created_at")))
    }

    /// Wraps a fresh DEK under the KMS and stores it. Losing the race for this
    /// version is a normal outcome: the caller re-reads.
    async fn insert(&self, scope: &str, version: u32) -> Result<(), PageError> {
        self.db
            .execute(&Statement::with_values(
                "INSERT INTO scope_keys \
                 (scope, key_version, kms_provider, kms_key_ref, wrapped_dek, created_at, retired_at) \
                 VALUES (?, ?, ?, ?, ?, ?, NULL) \
                 ON CONFLICT (scope, key_version) DO NOTHING",
                self.wrapped_values(scope, version).await?,
            ))
            .await
            .map_err(PageError::Store)?;
        Ok(())
    }

    /// The bound parameters of one `scope_keys` insert, shared by the plain
    /// and the conditional form so the two cannot drift.
    async fn wrapped_values(&self, scope: &str, version: u32) -> Result<InsertValues, PageError> {
        let dek = Dek::generate().map_err(PageError::Kms)?;
        let wrapped = self.kms.wrap(&dek).await.map_err(PageError::Kms)?;
        Ok(vec![
            text(scope),
            int(version),
            text(self.kms.provider()),
            text(self.kms.key_ref()),
            text(&b64(&wrapped)),
            text(&self.now()),
        ])
    }

    /// Unwraps one version's key material.
    async fn unwrap(&self, scope: &str, version: u32, wrapped: &str) -> Result<Dek, PageError> {
        let bytes = b64_decode(wrapped).ok_or_else(|| {
            PageError::Crypto(format!("scope `{scope}` key {version} is not base64"))
        })?;
        self.kms.unwrap(&bytes).await.map_err(PageError::Kms)
    }

    /// Every stored version of a scope, oldest first.
    async fn versions(&self, scope: &str) -> Result<Vec<StoredKey>, PageError> {
        let rows = self
            .db
            .query(&Statement::with_values(
                "SELECT key_version, wrapped_dek, retired_at FROM scope_keys \
                 WHERE scope = ? ORDER BY key_version ASC",
                vec![text(scope)],
            ))
            .await
            .map_err(PageError::Store)?;
        rows.rows
            .iter()
            .map(|row| {
                Ok(StoredKey {
                    version: row.get::<u32>("key_version").unwrap_or_default(),
                    wrapped_dek: row.get::<String>("wrapped_dek"),
                    retired_at: row.get::<String>("retired_at"),
                })
            })
            .collect()
    }

    /// A scope outside the slug rule, refused before any SQL is built.
    fn check_slug(&self, scope: &str) -> Result<(), PageError> {
        if entity::is_slug(scope) {
            Ok(())
        } else {
            Err(PageError::InvalidScope(scope.to_owned()))
        }
    }

    /// The current time as RFC 3339, as everywhere else in the module.
    fn now(&self) -> String {
        self.clock.now().format(&Rfc3339).unwrap_or_default()
    }
}

/// The column values of a `scope_keys` row, before the SQL that inserts it.
type InsertValues = Vec<SeaValue>;

/// A scope whose key did not settle, which is a fault rather than a retry.
fn unsettled(scope: &str) -> PageError {
    PageError::Store(DbError::Batch(format!(
        "scope `{scope}` would not settle on an active key"
    )))
}

/// One row of `scope_keys`, as far as key selection needs it.
struct StoredKey {
    version: u32,
    /// `None` is the row's whole meaning: the key is gone.
    wrapped_dek: Option<String>,
    /// Set only by a rotation that finished; `None` on a shredded key.
    retired_at: Option<String>,
}

/// The newest version whose key still exists.
fn live_of(rows: &[StoredKey]) -> Option<(u32, String)> {
    rows.iter().rev().find_map(|row| {
        row.wrapped_dek
            .clone()
            .map(|wrapped| (row.version, wrapped))
    })
}

/// Whether a scope has rows and every one of them was **shredded** rather
/// than retired: `forget_scope` does not stamp `retired_at`, so that
/// difference is the record that the scope must not be keyed again.
fn shredded(rows: &[StoredKey]) -> bool {
    !rows.is_empty()
        && rows
            .iter()
            .all(|row| row.wrapped_dek.is_none() && row.retired_at.is_none())
}

/// A live version that a newer one has superseded, if the caller is already
/// carrying an unfinished rotation. A scope with more than one live version
/// is exactly that, and the oldest live one is the one to report.
fn superseded(rows: &[StoredKey]) -> Option<u32> {
    let live: Vec<u32> = rows
        .iter()
        .filter(|row| row.wrapped_dek.is_some())
        .map(|row| row.version)
        .collect();
    (live.len() > 1).then(|| live[0])
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn b64_decode(text: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD.decode(text).ok()
}
