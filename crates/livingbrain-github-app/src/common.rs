//! What every store function shares: the error type, the bind helpers, the
//! membership check and the random source.

use cratefield_core::{Database, DbError, Row};
use cratefield_kms::KmsError;
use sea_query::Value as SeaValue;

/// Why a store call failed. `Display` never carries a credential: a
/// [`DbError`] is scrubbed by the port, and a [`KmsError`] from this crate
/// names what failed, never the plaintext.
#[derive(Debug)]
pub enum StoreError {
    /// A statement failed. Retryable.
    Store(DbError),
    /// A key could not be wrapped or unwrapped, or a ciphertext did not
    /// open. A `Tampered` here is never a reason to fall back to anything.
    Kms(KmsError),
    /// The acting member is not an owner or admin of the workspace (or is
    /// no member at all). Re-read at the moment of the write, so a member
    /// demoted after starting a flow cannot finish it.
    Forbidden,
    /// An argument failed validation; the text says which, never the value.
    Invalid(&'static str),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => write!(f, "github store: {error}"),
            Self::Kms(error) => write!(f, "github store: {error}"),
            Self::Forbidden => f.write_str("github store: owner or admin required"),
            Self::Invalid(what) => write!(f, "github store: invalid {what}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<DbError> for StoreError {
    fn from(error: DbError) -> Self {
        Self::Store(error)
    }
}

impl From<KmsError> for StoreError {
    fn from(error: KmsError) -> Self {
        Self::Kms(error)
    }
}

/// A non-null text bind.
pub(crate) fn text(value: &str) -> SeaValue {
    SeaValue::String(Some(Box::new(value.to_owned())))
}

/// A nullable text bind.
pub(crate) fn opt_text(value: Option<&str>) -> SeaValue {
    SeaValue::String(value.map(|v| Box::new(v.to_owned())))
}

/// A non-null integer bind. BIGINT on Postgres, INTEGER on SQLite.
pub(crate) fn int(value: i64) -> SeaValue {
    SeaValue::BigInt(Some(value))
}

/// A nullable integer bind.
pub(crate) fn opt_int(value: Option<i64>) -> SeaValue {
    SeaValue::BigInt(value)
}

pub(crate) fn get_text(row: &Row, column: &str) -> String {
    row.get::<String>(column).unwrap_or_default()
}

pub(crate) fn get_opt_text(row: &Row, column: &str) -> Option<String> {
    row.get::<Option<String>>(column).flatten()
}

pub(crate) fn get_int(row: &Row, column: &str) -> i64 {
    row.get::<i64>(column).unwrap_or_default()
}

pub(crate) fn get_opt_int(row: &Row, column: &str) -> Option<i64> {
    row.get::<i64>(column)
}

/// `instant` as unix seconds, the unit every table here stores.
pub(crate) fn unix(instant: time::OffsetDateTime) -> i64 {
    instant.unix_timestamp()
}

/// Refuses unless `member_id` is, right now, an owner or admin of
/// `workspace_id` — a removed member, a plain member and a stranger are one
/// answer. The workspaces module's own read decides, so "admin" means here
/// exactly what it means on every other route.
pub(crate) async fn require_admin(
    db: &dyn Database,
    workspace_id: &str,
    member_id: &str,
) -> Result<(), StoreError> {
    match livingbrain_workspaces::caller_for(db, workspace_id, member_id).await? {
        Some(caller) if caller.is_admin => Ok(()),
        _ => Err(StoreError::Forbidden),
    }
}

/// `n` random bytes from the OS source, as lowercase hex.
pub(crate) fn random_hex(n: usize) -> Result<String, StoreError> {
    let mut bytes = vec![0_u8; n];
    getrandom::fill(&mut bytes)
        .map_err(|err| StoreError::Kms(KmsError::Unavailable(format!("no randomness: {err}"))))?;
    Ok(hex(&bytes))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// A workspace or member id as the workspaces module mints them: short,
/// printable, no separators that could smuggle anything into the AAD.
pub(crate) fn check_id(value: &str, what: &'static str) -> Result<(), StoreError> {
    let ok = !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'));
    if ok {
        Ok(())
    } else {
        Err(StoreError::Invalid(what))
    }
}

/// A GitHub numeric id: positive.
pub(crate) fn check_github_id(value: i64, what: &'static str) -> Result<(), StoreError> {
    if value > 0 {
        Ok(())
    } else {
        Err(StoreError::Invalid(what))
    }
}

/// Free text bounded in length and stripped of control characters, for the
/// error and label columns. Truncates on a character boundary.
pub(crate) fn bounded(value: &str, max_chars: usize) -> String {
    value
        .chars()
        .filter(|c| !c.is_control())
        .take(max_chars)
        .collect()
}
