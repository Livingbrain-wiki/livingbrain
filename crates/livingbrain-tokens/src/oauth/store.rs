//! The OAuth tables: registered clients, one-time authorization codes, and
//! rotating refresh tokens (issue #72, the connector sign-in).
//!
//! Like the personal access token table, nothing here holds a credential in
//! the clear: a code and a refresh token are stored as the SHA-256 of the
//! whole value and looked up by that hash. Each consuming write is a single
//! conditional `UPDATE`, so two requests presenting the same code or refresh
//! token cannot both win: one row changes, and the other sees zero rows.

use std::sync::Arc;

use cratefield_core::{Database, DbError, Statement};
use sea_query::Value;
use sha2::{Digest, Sha256};

pub(crate) const CLIENTS: &str = "oauth_clients";
pub(crate) const CODES: &str = "oauth_codes";
pub(crate) const REFRESH: &str = "oauth_refresh_tokens";

fn text(value: &str) -> Value {
    Value::String(Some(Box::new(value.to_owned())))
}

/// The lowercase-hex SHA-256 of a credential, the only form stored.
pub(crate) fn hash(value: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for byte in Sha256::digest(value.as_bytes()) {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// A registered public client, as the grants read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Client {
    pub(crate) client_id: String,
    pub(crate) client_name: String,
    pub(crate) redirect_uris: Vec<String>,
}

/// An authorization code row. `used` changes after creation; everything else
/// is what the approval bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodeRow {
    pub(crate) client_id: String,
    pub(crate) redirect_uri: String,
    pub(crate) code_challenge: String,
    pub(crate) workspace_id: String,
    pub(crate) user_id: String,
    pub(crate) scope: String,
    pub(crate) family: String,
    pub(crate) expires_at: String,
    pub(crate) used: bool,
}

/// A refresh token row: the access token it was issued beside, the connection
/// (`family`) it belongs to, and who it is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RefreshRow {
    pub(crate) family: String,
    pub(crate) prefix: String,
    pub(crate) client_id: String,
    pub(crate) workspace_id: String,
    pub(crate) user_id: String,
    pub(crate) expires_at: String,
    pub(crate) used: bool,
}

#[derive(Clone)]
pub(crate) struct OAuthStore {
    pub(crate) db: Arc<dyn Database>,
}

fn field(row: &cratefield_core::Row, name: &str) -> Option<String> {
    row.get::<Option<String>>(name).flatten()
}

fn refresh_row(row: &cratefield_core::Row) -> RefreshRow {
    RefreshRow {
        family: field(row, "family").unwrap_or_default(),
        prefix: field(row, "prefix").unwrap_or_default(),
        client_id: field(row, "client_id").unwrap_or_default(),
        workspace_id: field(row, "workspace_id").unwrap_or_default(),
        user_id: field(row, "user_id").unwrap_or_default(),
        expires_at: field(row, "expires_at").unwrap_or_default(),
        used: field(row, "used_at").is_some(),
    }
}

impl OAuthStore {
    pub(crate) fn new(db: Arc<dyn Database>) -> Self {
        Self { db }
    }

    pub(crate) async fn insert_client(
        &self,
        client: &Client,
        created_at: &str,
    ) -> Result<(), DbError> {
        let uris = serde_json::to_string(&client.redirect_uris)
            .map_err(|err| DbError::Execute(err.to_string()))?;
        self.db
            .execute(&Statement::with_values(
                "INSERT INTO oauth_clients (client_id, client_name, redirect_uris, created_at) \
                 VALUES (?, ?, ?, ?)",
                vec![
                    text(&client.client_id),
                    text(&client.client_name),
                    text(&uris),
                    text(created_at),
                ],
            ))
            .await
            .map(|_| ())
    }

    pub(crate) async fn client(&self, client_id: &str) -> Result<Option<Client>, DbError> {
        let rows = self
            .db
            .query(&Statement::with_values(
                "SELECT client_id, client_name, redirect_uris FROM oauth_clients \
                 WHERE client_id = ?",
                vec![text(client_id)],
            ))
            .await?;
        let Some(row) = rows.rows.first() else {
            return Ok(None);
        };
        let redirect_uris = field(row, "redirect_uris")
            .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
            .unwrap_or_default();
        Ok(Some(Client {
            client_id: field(row, "client_id").unwrap_or_default(),
            client_name: field(row, "client_name").unwrap_or_default(),
            redirect_uris,
        }))
    }

    pub(crate) async fn insert_code(
        &self,
        code_hash: &str,
        code: &CodeRow,
        created_at: &str,
    ) -> Result<(), DbError> {
        self.db
            .execute(&Statement::with_values(
                "INSERT INTO oauth_codes (code_hash, client_id, redirect_uri, code_challenge, \
                     workspace_id, user_id, scope, family, expires_at, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                vec![
                    text(code_hash),
                    text(&code.client_id),
                    text(&code.redirect_uri),
                    text(&code.code_challenge),
                    text(&code.workspace_id),
                    text(&code.user_id),
                    text(&code.scope),
                    text(&code.family),
                    text(&code.expires_at),
                    text(created_at),
                ],
            ))
            .await
            .map(|_| ())
    }

    pub(crate) async fn code(&self, code_hash: &str) -> Result<Option<CodeRow>, DbError> {
        let rows = self
            .db
            .query(&Statement::with_values(
                "SELECT client_id, redirect_uri, code_challenge, workspace_id, user_id, scope, \
                        family, expires_at, used_at \
                   FROM oauth_codes WHERE code_hash = ?",
                vec![text(code_hash)],
            ))
            .await?;
        Ok(rows.rows.first().map(|row| CodeRow {
            client_id: field(row, "client_id").unwrap_or_default(),
            redirect_uri: field(row, "redirect_uri").unwrap_or_default(),
            code_challenge: field(row, "code_challenge").unwrap_or_default(),
            workspace_id: field(row, "workspace_id").unwrap_or_default(),
            user_id: field(row, "user_id").unwrap_or_default(),
            scope: field(row, "scope").unwrap_or_default(),
            family: field(row, "family").unwrap_or_default(),
            expires_at: field(row, "expires_at").unwrap_or_default(),
            used: field(row, "used_at").is_some(),
        }))
    }

    /// Spends a code. `true` for exactly one caller: the row's `used_at` is
    /// set only where it was still unset.
    pub(crate) async fn spend_code(&self, code_hash: &str, now: &str) -> Result<bool, DbError> {
        let changed = self
            .db
            .execute(&Statement::with_values(
                "UPDATE oauth_codes SET used_at = ? WHERE code_hash = ? AND used_at IS NULL",
                vec![text(now), text(code_hash)],
            ))
            .await?;
        Ok(changed == 1)
    }

    pub(crate) async fn insert_refresh(
        &self,
        token_hash: &str,
        row: &RefreshRow,
        created_at: &str,
    ) -> Result<(), DbError> {
        self.db
            .execute(&Statement::with_values(
                "INSERT INTO oauth_refresh_tokens (token_hash, family, prefix, client_id, \
                     workspace_id, user_id, expires_at, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                vec![
                    text(token_hash),
                    text(&row.family),
                    text(&row.prefix),
                    text(&row.client_id),
                    text(&row.workspace_id),
                    text(&row.user_id),
                    text(&row.expires_at),
                    text(created_at),
                ],
            ))
            .await
            .map(|_| ())
    }

    /// Every refresh token of one connection, spent or not, so a replay can
    /// find each access token the connection was given.
    pub(crate) async fn family(&self, family: &str) -> Result<Vec<RefreshRow>, DbError> {
        let rows = self
            .db
            .query(&Statement::with_values(
                "SELECT family, prefix, client_id, workspace_id, user_id, expires_at, used_at \
                   FROM oauth_refresh_tokens WHERE family = ?",
                vec![text(family)],
            ))
            .await?;
        Ok(rows.rows.iter().map(refresh_row).collect())
    }

    pub(crate) async fn refresh(&self, token_hash: &str) -> Result<Option<RefreshRow>, DbError> {
        let rows = self
            .db
            .query(&Statement::with_values(
                "SELECT family, prefix, client_id, workspace_id, user_id, expires_at, used_at \
                   FROM oauth_refresh_tokens WHERE token_hash = ?",
                vec![text(token_hash)],
            ))
            .await?;
        Ok(rows.rows.first().map(refresh_row))
    }

    /// Spends a refresh token, exactly as [`OAuthStore::spend_code`] does.
    pub(crate) async fn spend_refresh(&self, token_hash: &str, now: &str) -> Result<bool, DbError> {
        let changed = self
            .db
            .execute(&Statement::with_values(
                "UPDATE oauth_refresh_tokens SET used_at = ? \
                 WHERE token_hash = ? AND used_at IS NULL",
                vec![text(now), text(token_hash)],
            ))
            .await?;
        Ok(changed == 1)
    }
}
