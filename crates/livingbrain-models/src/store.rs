//! The `model_connections` table, behind the `Database` port.
//!
//! Every statement is parameterized — no value is ever interpolated into
//! SQL — and every one names its workspace, so a row can only be reached
//! through the workspace the caller's verified session carries.

use cratefield_core::{Database, DbError, Row, Statement};
use sea_query::Value;

/// One model connection row, minus the ciphertext: the key column is
/// never selected into this struct, so there is no path from a stored key
/// to a response body. [`list`] and [`get`] say so in their queries too.
#[derive(Debug, Clone)]
pub(crate) struct ModelConnection {
    pub(crate) role: String,
    pub(crate) provider: String,
    pub(crate) base_url: String,
    pub(crate) auth: String,
    pub(crate) wire: String,
    pub(crate) model: String,
    pub(crate) key_last4: String,
    pub(crate) fallback_to_managed: bool,
    pub(crate) status: String,
    pub(crate) missing: String,
    pub(crate) context_size: Option<String>,
    pub(crate) checked_at: String,
}

/// The fields a connect (upsert) writes.
pub(crate) struct ConnectionFields<'a> {
    pub(crate) provider: &'a str,
    pub(crate) base_url: &'a str,
    pub(crate) auth: &'a str,
    pub(crate) wire: &'a str,
    pub(crate) model: &'a str,
    pub(crate) key_ciphertext: &'a [u8],
    pub(crate) key_last4: &'a str,
    pub(crate) fallback_to_managed: bool,
    pub(crate) status: &'a str,
    pub(crate) missing: &'a str,
    pub(crate) context_size: Option<&'a str>,
    pub(crate) checked_at: &'a str,
    pub(crate) updated_by: &'a str,
}

/// Inserts or updates a model connection.
pub(crate) async fn upsert(
    db: &dyn Database,
    workspace_id: &str,
    role: &str,
    fields: ConnectionFields<'_>,
) -> Result<(), DbError> {
    let statement = Statement::with_values(
        "INSERT INTO model_connections \
            (workspace_id, role, provider, base_url, auth, wire, model, \
             key_ciphertext, key_last4, fallback_to_managed, status, missing, \
             context_size, checked_at, updated_by) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT (workspace_id, role) DO UPDATE SET \
            provider = excluded.provider, \
            base_url = excluded.base_url, \
            auth = excluded.auth, \
            wire = excluded.wire, \
            model = excluded.model, \
            key_ciphertext = excluded.key_ciphertext, \
            key_last4 = excluded.key_last4, \
            fallback_to_managed = excluded.fallback_to_managed, \
            status = excluded.status, \
            missing = excluded.missing, \
            context_size = excluded.context_size, \
            checked_at = excluded.checked_at, \
            updated_by = excluded.updated_by",
        vec![
            text(workspace_id),
            text(role),
            text(fields.provider),
            text(fields.base_url),
            text(fields.auth),
            text(fields.wire),
            text(fields.model),
            blob(fields.key_ciphertext),
            text(fields.key_last4),
            flag(fields.fallback_to_managed),
            text(fields.status),
            text(fields.missing),
            nullable_text(fields.context_size),
            text(fields.checked_at),
            text(fields.updated_by),
        ],
    );
    db.execute(&statement).await.map(|_| ())
}

/// Every model connection in a workspace.
pub(crate) async fn list(
    db: &dyn Database,
    workspace_id: &str,
) -> Result<Vec<ModelConnection>, DbError> {
    let statement = Statement::with_values(
        "SELECT role, provider, base_url, auth, wire, model, key_last4, \
                fallback_to_managed, status, missing, context_size, checked_at \
         FROM model_connections WHERE workspace_id = ? ORDER BY role",
        vec![text(workspace_id)],
    );
    Ok(db
        .query(&statement)
        .await?
        .rows
        .iter()
        .map(from_row)
        .collect())
}

/// One model connection, or `None`.
pub(crate) async fn get(
    db: &dyn Database,
    workspace_id: &str,
    role: &str,
) -> Result<Option<ModelConnection>, DbError> {
    let statement = Statement::with_values(
        "SELECT role, provider, base_url, auth, wire, model, key_last4, \
                fallback_to_managed, status, missing, context_size, checked_at \
         FROM model_connections WHERE workspace_id = ? AND role = ?",
        vec![text(workspace_id), text(role)],
    );
    Ok(db.query(&statement).await?.first().map(from_row))
}

/// Deletes a model connection.
pub(crate) async fn delete(
    db: &dyn Database,
    workspace_id: &str,
    role: &str,
) -> Result<(), DbError> {
    let statement = Statement::with_values(
        "DELETE FROM model_connections WHERE workspace_id = ? AND role = ?",
        vec![text(workspace_id), text(role)],
    );
    db.execute(&statement).await.map(|_| ())
}

fn from_row(row: &Row) -> ModelConnection {
    ModelConnection {
        role: row.get::<String>("role").unwrap_or_default(),
        provider: row.get::<String>("provider").unwrap_or_default(),
        base_url: row.get::<String>("base_url").unwrap_or_default(),
        auth: row.get::<String>("auth").unwrap_or_default(),
        wire: row.get::<String>("wire").unwrap_or_default(),
        model: row.get::<String>("model").unwrap_or_default(),
        key_last4: row.get::<String>("key_last4").unwrap_or_default(),
        fallback_to_managed: row.get::<i64>("fallback_to_managed").unwrap_or_default() != 0,
        status: row.get::<String>("status").unwrap_or_default(),
        missing: row.get::<String>("missing").unwrap_or_default(),
        context_size: row.get::<Option<String>>("context_size").flatten(),
        checked_at: row.get::<String>("checked_at").unwrap_or_default(),
    }
}

/// A non-null text bind.
fn text(value: &str) -> Value {
    Value::String(Some(Box::new(value.to_owned())))
}

/// A text bind that is SQL NULL when there is no value.
fn nullable_text(value: Option<&str>) -> Value {
    value.map_or(Value::String(None), text)
}

/// A boolean stored as the portable 0/1 INTEGER flag (ADR 0004).
fn flag(value: bool) -> Value {
    Value::BigInt(Some(i64::from(value)))
}

/// A blob bind for the ciphertext.
fn blob(value: &[u8]) -> Value {
    Value::Bytes(Some(Box::new(value.to_vec())))
}
