//! The `tool_connections`, `tool_workspace_settings` and
//! `tool_pending_approvals` tables, behind the `Database` port.
//!
//! Every statement is parameterized and names its workspace, so a row can
//! only be reached through the workspace the caller's session carries. The
//! token columns are read only here and opened only by the gate's call path.

use cratefield_core::{Database, DbError, Row, Statement};
use sea_query::Value as SeaValue;
use serde_json::Value;

use crate::envelope::SealedToken;

/// One connection: who connected it, where it points, the sealed token, and
/// the tool names switched off on this connection alone.
#[derive(Debug, Clone, PartialEq)]
pub struct ConnectionRecord {
    pub workspace_id: String,
    pub member_id: String,
    pub provider: String,
    pub server_url: String,
    pub sealed: SealedToken,
    pub disabled_tools: Vec<String>,
    pub created_at: String,
}

/// One borrowed write waiting for the owner's decision, recorded whole so
/// resolving it replays exactly what was asked.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingCall {
    pub approval_id: String,
    pub workspace_id: String,
    pub owner_member_id: String,
    pub asker_member_id: String,
    pub provider: String,
    pub tool_name: String,
    pub arguments: Value,
    pub created_at: String,
}

/// Stores (or replaces) a connection: one statement, so a re-connect is an
/// overwrite with a fresh envelope rather than a window with no row.
pub async fn put_connection(db: &dyn Database, record: &ConnectionRecord) -> Result<(), DbError> {
    let statement = Statement::with_values(
        "INSERT INTO tool_connections \
            (workspace_id, member_id, provider, server_url, wrapped_dek, nonce, \
             ciphertext, kms_key_ref, disabled_tools, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT (workspace_id, member_id, provider) DO UPDATE SET \
            server_url = excluded.server_url, \
            wrapped_dek = excluded.wrapped_dek, \
            nonce = excluded.nonce, \
            ciphertext = excluded.ciphertext, \
            kms_key_ref = excluded.kms_key_ref, \
            disabled_tools = excluded.disabled_tools, \
            created_at = excluded.created_at",
        vec![
            text(&record.workspace_id),
            text(&record.member_id),
            text(&record.provider),
            text(&record.server_url),
            text(&record.sealed.wrapped_dek),
            text(&record.sealed.nonce),
            text(&record.sealed.ciphertext),
            text(&record.sealed.kms_key_ref),
            json_text(&record.disabled_tools),
            text(&record.created_at),
        ],
    );
    db.execute(&statement).await.map(|_| ())
}

/// One connection, or `None`.
pub async fn get_connection(
    db: &dyn Database,
    workspace_id: &str,
    member_id: &str,
    provider: &str,
) -> Result<Option<ConnectionRecord>, DbError> {
    let rows = db
        .query(&Statement::with_values(
            "SELECT workspace_id, member_id, provider, server_url, wrapped_dek, nonce, \
                    ciphertext, kms_key_ref, disabled_tools, created_at \
             FROM tool_connections WHERE workspace_id = ? AND member_id = ? AND provider = ?",
            vec![text(workspace_id), text(member_id), text(provider)],
        ))
        .await?;
    Ok(rows.first().map(from_row))
}

/// The workspace's disabled tool names, empty when nobody set any.
pub async fn workspace_disabled_tools(
    db: &dyn Database,
    workspace_id: &str,
) -> Result<Vec<String>, DbError> {
    let rows = db
        .query(&Statement::with_values(
            "SELECT disabled_tools FROM tool_workspace_settings WHERE workspace_id = ?",
            vec![text(workspace_id)],
        ))
        .await?;
    Ok(rows
        .first()
        .and_then(|row| row.get::<Option<String>>("disabled_tools"))
        .flatten()
        .map(decode_tools)
        .unwrap_or_default())
}

/// Replaces the workspace's disabled tool list.
pub async fn set_workspace_disabled_tools(
    db: &dyn Database,
    workspace_id: &str,
    disabled: &[String],
) -> Result<(), DbError> {
    let statement = Statement::with_values(
        "INSERT INTO tool_workspace_settings (workspace_id, disabled_tools) VALUES (?, ?) \
         ON CONFLICT (workspace_id) DO UPDATE SET disabled_tools = excluded.disabled_tools",
        vec![text(workspace_id), json_text(disabled)],
    );
    db.execute(&statement).await.map(|_| ())
}

/// Files a borrowed write for the owner to decide.
pub async fn put_pending(db: &dyn Database, call: &PendingCall) -> Result<(), DbError> {
    let statement = Statement::with_values(
        "INSERT INTO tool_pending_approvals \
            (approval_id, workspace_id, owner_member_id, asker_member_id, provider, \
             tool_name, arguments, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        vec![
            text(&call.approval_id),
            text(&call.workspace_id),
            text(&call.owner_member_id),
            text(&call.asker_member_id),
            text(&call.provider),
            text(&call.tool_name),
            text(&call.arguments.to_string()),
            text(&call.created_at),
        ],
    );
    db.execute(&statement).await.map(|_| ())
}

/// Takes a pending call out of the table in one `DELETE … RETURNING`: the
/// row is consumed exactly once, and only by its owner in its own
/// workspace — a click from anyone else, or about a row from another
/// workspace, matches nothing, consumes nothing, and leaves the decision
/// to the owner. `None` for an id that is unknown, spent or not theirs.
pub async fn take_pending(
    db: &dyn Database,
    workspace_id: &str,
    approver_member_id: &str,
    approval_id: &str,
) -> Result<Option<PendingCall>, DbError> {
    let rows = db
        .query(&Statement::with_values(
            "DELETE FROM tool_pending_approvals \
             WHERE approval_id = ? AND workspace_id = ? AND owner_member_id = ? \
             RETURNING approval_id, workspace_id, owner_member_id, asker_member_id, \
                       provider, tool_name, arguments, created_at",
            vec![
                text(approval_id),
                text(workspace_id),
                text(approver_member_id),
            ],
        ))
        .await?;
    Ok(rows.first().map(from_pending_row))
}

fn from_row(row: &Row) -> ConnectionRecord {
    ConnectionRecord {
        workspace_id: row.get::<String>("workspace_id").unwrap_or_default(),
        member_id: row.get::<String>("member_id").unwrap_or_default(),
        provider: row.get::<String>("provider").unwrap_or_default(),
        server_url: row.get::<String>("server_url").unwrap_or_default(),
        sealed: SealedToken {
            wrapped_dek: row.get::<String>("wrapped_dek").unwrap_or_default(),
            nonce: row.get::<String>("nonce").unwrap_or_default(),
            ciphertext: row.get::<String>("ciphertext").unwrap_or_default(),
            kms_key_ref: row.get::<String>("kms_key_ref").unwrap_or_default(),
        },
        disabled_tools: row
            .get::<Option<String>>("disabled_tools")
            .flatten()
            .map(decode_tools)
            .unwrap_or_default(),
        created_at: row.get::<String>("created_at").unwrap_or_default(),
    }
}

fn from_pending_row(row: &Row) -> PendingCall {
    PendingCall {
        approval_id: row.get::<String>("approval_id").unwrap_or_default(),
        workspace_id: row.get::<String>("workspace_id").unwrap_or_default(),
        owner_member_id: row.get::<String>("owner_member_id").unwrap_or_default(),
        asker_member_id: row.get::<String>("asker_member_id").unwrap_or_default(),
        provider: row.get::<String>("provider").unwrap_or_default(),
        tool_name: row.get::<String>("tool_name").unwrap_or_default(),
        arguments: row
            .get::<Option<String>>("arguments")
            .flatten()
            .map(|raw| serde_json::from_str(&raw).unwrap_or(Value::Null))
            .unwrap_or(Value::Null),
        created_at: row.get::<String>("created_at").unwrap_or_default(),
    }
}

/// A non-null text bind.
fn text(value: &str) -> SeaValue {
    SeaValue::String(Some(Box::new(value.to_owned())))
}

/// A list of tool names as its JSON array text.
fn json_text(names: &[String]) -> SeaValue {
    text(&encode_tools(names))
}

/// `["a","b"]` — and `[]` for an empty list, never SQL NULL.
fn encode_tools(names: &[String]) -> String {
    serde_json::to_string(names).unwrap_or_else(|_| "[]".to_owned())
}

/// The list back out of its JSON text. A malformed cell reads as none
/// disabled: the cell is only ever written by [`encode_tools`], so a
/// mismatch here is a storage fault, not an input anyone controls.
fn decode_tools(raw: String) -> Vec<String> {
    serde_json::from_str(&raw).unwrap_or_default()
}
