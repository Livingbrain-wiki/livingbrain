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
    /// The connector label the token will carry (see `connector_label`).
    pub(crate) name: String,
    pub(crate) family: String,
    pub(crate) expires_at: String,
    pub(crate) used: bool,
}

/// A refresh token row: the access token it was issued beside, the connection
/// (`family`) it belongs to, and who it is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RefreshRow {
    pub(crate) name: String,
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
        name: field(row, "name").unwrap_or_default(),
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

    /// How many clients were registered at or after `since`. With
    /// `unverified_only`, only those with a redirect host Living Brain does not
    /// recognise as Claude: the ones the daily cap is for.
    pub(crate) async fn clients_since(
        &self,
        since: &str,
        unverified_only: bool,
    ) -> Result<usize, DbError> {
        let sql = if unverified_only {
            "SELECT client_id FROM oauth_clients WHERE created_at >= ? AND unverified = 1"
        } else {
            "SELECT client_id FROM oauth_clients WHERE created_at >= ?"
        };
        let rows = self
            .db
            .query(&Statement::with_values(sql, vec![text(since)]))
            .await?;
        Ok(rows.rows.len())
    }

    /// How many clients one registering address created since `since`, for the
    /// per-address registration limit. Only the address's hash is ever stored.
    pub(crate) async fn registrations_from(
        &self,
        ip_hash: &str,
        since: &str,
    ) -> Result<usize, DbError> {
        let rows = self
            .db
            .query(&Statement::with_values(
                "SELECT client_id FROM oauth_clients WHERE ip_hash = ? AND created_at >= ?",
                vec![text(ip_hash), text(since)],
            ))
            .await?;
        Ok(rows.rows.len())
    }

    /// A client already registered with this exact name and redirect set, if
    /// any. Re-registering the same app returns its existing identity instead
    /// of a new row, so an app that registers again (or a retry) does not use
    /// the day's allowance. `redirect_uris` is the sorted JSON array.
    pub(crate) async fn client_with_registration(
        &self,
        client_name: &str,
        redirect_uris: &str,
    ) -> Result<Option<Client>, DbError> {
        let rows = self
            .db
            .query(&Statement::with_values(
                "SELECT client_id, client_name, redirect_uris FROM oauth_clients \
                 WHERE client_name = ? AND redirect_uris = ?",
                vec![text(client_name), text(redirect_uris)],
            ))
            .await?;
        let Some(row) = rows.rows.first() else {
            return Ok(None);
        };
        Ok(Some(Client {
            client_id: field(row, "client_id").unwrap_or_default(),
            client_name: field(row, "client_name").unwrap_or_default(),
            redirect_uris: field(row, "redirect_uris")
                .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
                .unwrap_or_default(),
        }))
    }

    /// Deletes what can no longer do anything: codes past their expiry (a used
    /// code lives exactly as long as an unused one, so replay detection holds
    /// for its whole window), refresh tokens past their expiry, and clients
    /// with no code or refresh token left that were registered before
    /// `client_cutoff`. Answers the rows removed.
    pub(crate) async fn purge(&self, now: &str, client_cutoff: &str) -> Result<u64, DbError> {
        let mut removed = 0;
        removed += self
            .db
            .execute(&Statement::with_values(
                "DELETE FROM oauth_codes WHERE expires_at < ?",
                vec![text(now)],
            ))
            .await?;
        removed += self
            .db
            .execute(&Statement::with_values(
                "DELETE FROM oauth_refresh_tokens WHERE expires_at < ?",
                vec![text(now)],
            ))
            .await?;
        removed += self
            .db
            .execute(&Statement::with_values(
                "DELETE FROM oauth_clients WHERE created_at < ? \
                   AND client_id NOT IN (SELECT client_id FROM oauth_codes) \
                   AND client_id NOT IN (SELECT client_id FROM oauth_refresh_tokens)",
                vec![text(client_cutoff)],
            ))
            .await?;
        Ok(removed)
    }

    pub(crate) async fn insert_client(
        &self,
        client: &Client,
        created_at: &str,
        ip_hash: &str,
        unverified: bool,
    ) -> Result<(), DbError> {
        let uris = serde_json::to_string(&client.redirect_uris)
            .map_err(|err| DbError::Execute(err.to_string()))?;
        self.db
            .execute(&Statement::with_values(
                "INSERT INTO oauth_clients (client_id, client_name, redirect_uris, created_at, \
                     ip_hash, unverified) \
                 VALUES (?, ?, ?, ?, ?, ?)",
                vec![
                    text(&client.client_id),
                    text(&client.client_name),
                    text(&uris),
                    text(created_at),
                    text(ip_hash),
                    Value::BigInt(Some(i64::from(unverified))),
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
                     workspace_id, user_id, scope, name, family, expires_at, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                vec![
                    text(code_hash),
                    text(&code.client_id),
                    text(&code.redirect_uri),
                    text(&code.code_challenge),
                    text(&code.workspace_id),
                    text(&code.user_id),
                    text(&code.scope),
                    text(&code.name),
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
                        name, family, expires_at, used_at \
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
            name: field(row, "name").unwrap_or_default(),
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
                     workspace_id, user_id, expires_at, name, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                vec![
                    text(token_hash),
                    text(&row.family),
                    text(&row.prefix),
                    text(&row.client_id),
                    text(&row.workspace_id),
                    text(&row.user_id),
                    text(&row.expires_at),
                    text(&row.name),
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
                "SELECT family, prefix, client_id, workspace_id, user_id, expires_at, used_at, name \
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
                "SELECT family, prefix, client_id, workspace_id, user_id, expires_at, used_at, name \
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

#[cfg(test)]
mod tests {
    use cratefield_core::Statement;
    use cratefield_testing::TestHarness;
    use sea_query::Value;

    use super::OAuthStore;

    fn text(value: &str) -> Value {
        Value::String(Some(Box::new(value.to_owned())))
    }

    async fn exec(harness: &TestHarness, sql: &str, values: Vec<Value>) {
        harness
            .db
            .execute(&Statement::with_values(sql, values))
            .await
            .expect("the seed row writes");
    }

    async fn count(harness: &TestHarness, table: &str) -> usize {
        harness
            .db
            .query(&Statement::with_values(
                format!("SELECT 1 FROM {table}"),
                Vec::new(),
            ))
            .await
            .expect("the rows read")
            .rows
            .len()
    }

    /// M2: the purge removes what is finished and keeps what is live. Codes and
    /// renewals past expiry go; a client goes only when it is old and has nothing
    /// live left, so a client with a live renewal survives its retention window.
    #[pollster::test]
    async fn the_purge_removes_finished_rows_and_keeps_live_ones() {
        let harness = TestHarness::with_ports(vec![Box::new(super::super::OAuth::new())], |_| {});
        let old = "2020-01-01T00:00:00Z";
        let now = "2026-06-01T00:00:00Z";
        let cutoff = "2026-05-01T00:00:00Z";
        for client in ["lbc_idle", "lbc_renewing"] {
            exec(
                &harness,
                "INSERT INTO oauth_clients (client_id, client_name, redirect_uris, created_at) \
                 VALUES (?, '', '[]', ?)",
                vec![text(client), text(old)],
            )
            .await;
        }
        // Codes: one expired, one live. Renewals: one expired, one live for the
        // renewing client only.
        for (hash, client, expiry) in [
            ("c-old", "lbc_idle", old),
            ("c-live", "lbc_renewing", "2099-01-01T00:00:00Z"),
        ] {
            exec(
                &harness,
                "INSERT INTO oauth_codes (code_hash, client_id, redirect_uri, code_challenge, \
                     workspace_id, user_id, scope, family, expires_at, created_at) \
                 VALUES (?, ?, 'https://x.example/cb', 'c', 'w', 'u', '', 'f', ?, ?)",
                vec![text(hash), text(client), text(expiry), text(old)],
            )
            .await;
        }
        for (hash, client, expiry) in [
            ("r-old", "lbc_idle", old),
            ("r-live", "lbc_renewing", "2099-01-01T00:00:00Z"),
        ] {
            exec(
                &harness,
                "INSERT INTO oauth_refresh_tokens (token_hash, family, prefix, client_id, \
                     workspace_id, user_id, expires_at, created_at) \
                 VALUES (?, 'f', 'p', ?, 'w', 'u', ?, ?)",
                vec![text(hash), text(client), text(expiry), text(old)],
            )
            .await;
        }

        let store = OAuthStore::new(harness.db.clone());
        store.purge(now, cutoff).await.expect("the purge runs");

        assert_eq!(
            count(&harness, "oauth_codes").await,
            1,
            "the live code stays"
        );
        assert_eq!(
            count(&harness, "oauth_refresh_tokens").await,
            1,
            "the live renewal stays"
        );
        let clients = harness
            .db
            .query(&Statement::with_values(
                "SELECT client_id FROM oauth_clients",
                Vec::new(),
            ))
            .await
            .expect("the clients read");
        assert_eq!(
            clients.rows.len(),
            1,
            "only the client with a live renewal survives"
        );
    }
}
