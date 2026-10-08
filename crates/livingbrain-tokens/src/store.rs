//! The `personal_access_tokens` table: mint, verify, list, revoke.
//!
//! The credential never reaches storage — only the SHA-256 of the whole
//! token, re-derived on every request and compared in constant time. The
//! public `prefix` is the lookup key, so a revoke is one indexed read.
//!
//! SHA-256 rather than argon2id, for the reason `cratefield_core::ApiKeys`
//! gives: 256 random bits leave a slow search nothing to find, and a
//! stretched KDF per request would burn the Workers CPU budget.

use std::sync::Arc;

use cratefield_core::{Clock, Database, DbError, Statement, constant_time_eq};
use sea_query::Value;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

pub(crate) const TABLE: &str = "personal_access_tokens";

/// The public half of a token: `lbp_<16 hex>`.
const PREFIX_TAG: &str = "lbp";
/// Random bytes in the public id — short enough for a person to recognise a
/// token in a settings list, long enough that a collision is not worth
/// guarding.
const ID_BYTES: usize = 8;
/// Random bytes in the secret half: 256 bits.
const SECRET_BYTES: usize = 32;

/// The entropy source core does not carry (ADR 0002) — the same `getrandom`
/// call the page store makes for its nonce. Not the `RandomBytes` port: a
/// store that cannot mint wants to say so, and `RandomError` has a private
/// field, so nothing outside core can construct one to return.
fn draw(dest: &mut [u8]) -> Result<(), DbError> {
    getrandom::fill(dest).map_err(|err| DbError::Execute(format!("entropy source failed: {err}")))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn sha256_hex(value: &str) -> String {
    hex(&Sha256::digest(value.as_bytes()))
}

fn is_lower_hex(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn text(value: &str) -> Value {
    Value::String(Some(Box::new(value.to_owned())))
}

fn stamp(at: OffsetDateTime) -> String {
    at.replace_nanosecond(0)
        .expect("truncation to whole seconds stays in range")
        .format(&Rfc3339)
        .unwrap_or_default()
}

/// A row the listing shows: everything about a token except the token and
/// its hash, neither of which is ever read back out of storage. `Serialize`
/// is what the route sends, so there is one shape between storage and the
/// response and no second view type to keep in step.
#[derive(serde::Serialize)]
pub(crate) struct Summary {
    pub(crate) prefix: String,
    pub(crate) name: String,
    pub(crate) scopes: Option<Vec<String>>,
    pub(crate) created_at: String,
}

/// A verified token's answer to "who is calling?", and the subset it was cut
/// down to.
pub(crate) struct Principal {
    pub(crate) workspace_id: String,
    pub(crate) user_id: String,
    pub(crate) scopes: Option<Vec<String>>,
}

/// A freshly minted token. [`Minted::token`] is the only time the plaintext
/// exists: it is the response body of `POST /v1/tokens`, never stored, logged
/// or echoed again.
pub(crate) struct Minted {
    pub(crate) token: String,
    pub(crate) prefix: String,
    pub(crate) created_at: String,
}

#[derive(Clone)]
pub(crate) struct Store {
    db: Arc<dyn Database>,
    clock: Arc<dyn Clock>,
}

impl Store {
    pub(crate) fn new(db: Arc<dyn Database>, clock: Arc<dyn Clock>) -> Self {
        Self { db, clock }
    }

    fn generate(&self) -> Result<(String, String), DbError> {
        let mut bytes = [0_u8; ID_BYTES + SECRET_BYTES];
        draw(&mut bytes)?;
        let (id, secret) = bytes.split_at(ID_BYTES);
        Ok((hex(id), hex(secret)))
    }

    /// Mints a token for one member and stores its hash. An empty `scopes`
    /// stores the empty string, which reads back as "no subset".
    pub(crate) async fn mint(
        &self,
        workspace_id: &str,
        user_id: &str,
        name: &str,
        scopes: &[String],
    ) -> Result<Minted, DbError> {
        let (id, secret) = self.generate()?;
        let prefix = format!("{PREFIX_TAG}_{id}");
        let token = format!("{prefix}_{secret}");
        let created_at = stamp(self.clock.now());
        let statement = Statement::with_values(
            "INSERT INTO personal_access_tokens \
                 (prefix, secret_hash, workspace_id, user_id, name, scopes, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            vec![
                text(&prefix),
                text(&sha256_hex(&token)),
                text(workspace_id),
                text(user_id),
                text(name),
                text(&scopes.join(" ")),
                text(&created_at),
            ],
        );
        self.db.execute(&statement).await?;
        Ok(Minted {
            token,
            prefix,
            created_at,
        })
    }

    /// Parse, look the prefix up, re-derive the hash, compare in constant
    /// time. `Ok(None)` is every failure there is — malformed, unknown,
    /// revoked, wrong secret — deliberately indistinguishable, so probing an
    /// endpoint built on this learns nothing.
    pub(crate) async fn verify(&self, token: &str) -> Result<Option<Principal>, DbError> {
        let Some(prefix) = split(token) else {
            return Ok(None);
        };
        let rows = self
            .db
            .query(&Statement::with_values(
                "SELECT secret_hash, workspace_id, user_id, scopes, revoked_at \
                 FROM personal_access_tokens WHERE prefix = ?",
                vec![text(prefix)],
            ))
            .await?;
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        let field = |name: &str| row.get::<Option<String>>(name).flatten();
        // A revoked key is refused by the same path as an unknown one.
        if field("revoked_at").is_some() {
            return Ok(None);
        }
        let Some(expected) = field("secret_hash") else {
            return Ok(None);
        };
        if !constant_time_eq(sha256_hex(token).as_bytes(), expected.as_bytes()) {
            return Ok(None);
        }
        let (Some(workspace_id), Some(user_id)) = (field("workspace_id"), field("user_id")) else {
            return Ok(None);
        };
        Ok(Some(Principal {
            workspace_id,
            user_id,
            scopes: split_list(&field("scopes").unwrap_or_default()),
        }))
    }

    /// One member's own tokens, newest first, revoked ones excluded: a
    /// revoked token is gone from the list because it is gone from use.
    pub(crate) async fn list(
        &self,
        workspace_id: &str,
        user_id: &str,
    ) -> Result<Vec<Summary>, DbError> {
        let rows = self
            .db
            .query(&Statement::with_values(
                "SELECT prefix, name, scopes, created_at FROM personal_access_tokens \
                 WHERE workspace_id = ? AND user_id = ? AND revoked_at IS NULL \
                 ORDER BY created_at DESC, prefix ASC",
                vec![text(workspace_id), text(user_id)],
            ))
            .await?;
        Ok(rows
            .rows
            .iter()
            .map(|row| {
                let field = |name: &str| {
                    row.get::<Option<String>>(name)
                        .flatten()
                        .unwrap_or_default()
                };
                Summary {
                    scopes: split_list(&field("scopes")),
                    prefix: field("prefix"),
                    name: field("name"),
                    created_at: field("created_at"),
                }
            })
            .collect())
    }

    /// Revokes one of a member's own tokens. `Ok(false)` when the prefix
    /// names no token of theirs — deliberately the same answer as a token
    /// that does not exist, so the route cannot probe another member's
    /// prefixes.
    pub(crate) async fn revoke(
        &self,
        workspace_id: &str,
        user_id: &str,
        prefix: &str,
    ) -> Result<bool, DbError> {
        let statement = Statement::with_values(
            "UPDATE personal_access_tokens SET revoked_at = ? \
             WHERE prefix = ? AND workspace_id = ? AND user_id = ?",
            vec![
                text(&stamp(self.clock.now())),
                text(prefix),
                text(workspace_id),
                text(user_id),
            ],
        );
        Ok(self.db.execute(&statement).await? > 0)
    }
}

/// `{prefix}_{secret}` into its public half, refusing a malformed token
/// before it costs a query. The secret half is checked for shape too, though
/// it is never read back: the constant-time compare runs on the whole token.
fn split(token: &str) -> Option<&str> {
    let (prefix, secret) = token.rsplit_once('_')?;
    let id = prefix.strip_prefix(PREFIX_TAG)?.strip_prefix('_')?;
    if id.len() != ID_BYTES * 2
        || !is_lower_hex(id)
        || secret.len() != SECRET_BYTES * 2
        || !is_lower_hex(secret)
    {
        return None;
    }
    Some(prefix)
}

/// A space-separated scope list, or `None` when it is empty — the difference
/// between "no subset" and "an empty subset", which would lock the holder out
/// of everything.
fn split_list(scopes: &str) -> Option<Vec<String>> {
    let list: Vec<String> = scopes
        .split(' ')
        .filter(|scope| !scope.is_empty())
        .map(str::to_owned)
        .collect();
    if list.is_empty() { None } else { Some(list) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A malformed token must be refused on its shape, before it costs a
    /// query: wrong tag, wrong lengths, uppercase hex, trailing junk.
    #[test]
    fn a_malformed_token_never_costs_a_query() {
        let id = "ab".repeat(ID_BYTES);
        let secret = "cd".repeat(SECRET_BYTES);
        assert_eq!(
            split(&format!("lbp_{id}_{secret}")),
            Some(format!("lbp_{id}").as_str())
        );
        for token in [
            String::new(),
            format!("lbp_{id}"),
            format!("lbp_{id}_{secret}_extra"),
            format!("lbp_{}_{secret}", &id[..15]),
            format!("lbp_{id}_{}", &secret[..63]),
            format!("lbp_{id}_{}", "CD".repeat(SECRET_BYTES)),
            format!("lbx_{id}_{secret}"),
            format!("lbp_{id}_{secret} "),
        ] {
            assert!(split(&token).is_none(), "{token:?} parsed");
        }
    }
}
