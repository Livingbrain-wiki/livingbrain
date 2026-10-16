//! What every store function shares: the error type, the bind helpers, the
//! admin check and the random source.

use cratefield_core::{Database, DbError, Row};
use sea_query::Value as SeaValue;

/// Why a store call failed. `Display` never carries a credential or content.
#[derive(Debug)]
pub enum StoreError {
    /// A statement failed. Retryable.
    Store(DbError),
    /// The OS random source failed.
    Random,
    /// The acting member is not an owner or admin of the workspace.
    Forbidden,
    /// The page store refused a transcript; the text is the store's own.
    Ingest(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => write!(f, "discord store: {error}"),
            Self::Random => f.write_str("discord store: no randomness"),
            Self::Forbidden => f.write_str("discord store: owner or admin required"),
            Self::Ingest(what) => write!(f, "discord import: {what}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<DbError> for StoreError {
    fn from(error: DbError) -> Self {
        Self::Store(error)
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
/// `workspace_id`. The workspaces module's own read decides, so "admin" means
/// here what it means on every other route.
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
    getrandom::fill(&mut bytes).map_err(|_| StoreError::Random)?;
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

/// Free text bounded in length and stripped of control characters, for the
/// error and label columns. Truncates on a character boundary.
pub(crate) fn bounded(value: &str, max_chars: usize) -> String {
    value
        .chars()
        .filter(|c| !c.is_control())
        .take(max_chars)
        .collect()
}

/// A Discord snowflake (an id): digits only, 1..=20 of them.
pub(crate) fn is_snowflake(value: &str) -> bool {
    !value.is_empty() && value.len() <= 20 && value.bytes().all(|b| b.is_ascii_digit())
}

/// A value that must never be printed: a bot token or an OAuth secret.
#[derive(Clone)]
pub(crate) struct Secret(String);

impl Secret {
    pub(crate) fn new(value: String) -> Self {
        Self(value)
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(..)")
    }
}
