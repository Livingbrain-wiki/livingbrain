//! The `personal_access_tokens` table: mint, verify, list, revoke.
//!
//! The credential never reaches storage — only the SHA-256 of the whole
//! token, re-derived on every request and compared in constant time. The
//! public `prefix` is the lookup key, so a revoke is one indexed read.
//!
//! A token may carry an expiry and an audience (issue #72). An audience of
//! `mcp` marks a connector token, which only the MCP endpoint accepts, and
//! `actions` an actions token (issue #234), which only four read-only REST
//! routes accept; the refusal lives in `lib.rs`, where each surface picks
//! what it takes.

use std::sync::Arc;

use cratefield_core::{Clock, Database, DbError, Statement, constant_time_eq};
use sea_query::Value;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

pub(crate) const TABLE: &str = "personal_access_tokens";

/// The audience a connector token carries: accepted by the MCP endpoint only.
pub(crate) const MCP_AUDIENCE: &str = "mcp";

/// The public half of a token: `lbp_<16 hex>`.
const PREFIX_TAG: &str = "lbp";
/// Random bytes in the public id — short enough for a person to recognise a
/// token in a settings list, long enough that a collision is not worth
/// guarding.
const ID_BYTES: usize = 8;
/// Random bytes in the secret half: 256 bits.
const SECRET_BYTES: usize = 32;

/// The entropy source core does not carry (ADR 0002) — the same `getrandom`
/// call the page store makes for its nonce.
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
    at.to_offset(time::UtcOffset::UTC)
        .replace_nanosecond(0)
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
    /// `true` for a token that carries an audience — a connector token (the
    /// Claude connector sign-in) or an actions token (a ChatGPT custom GPT,
    /// issue #234) — so the settings list can label either as a connected
    /// app and revoke it with its label.
    pub(crate) connector: bool,
}

/// A verified token's answer to "who is calling?", and the subset it was cut
/// down to.
pub(crate) struct Principal {
    /// The token's public prefix, for "connected as" and the audit trail.
    pub(crate) prefix: String,
    pub(crate) workspace_id: String,
    pub(crate) user_id: String,
    pub(crate) scopes: Option<Vec<String>>,
    /// `Some("mcp")` for a connector token, `Some("actions")` for an actions
    /// token, `None` for an ordinary one.
    pub(crate) audience: Option<String>,
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
    /// stores the empty string, which reads back as "no subset". The token
    /// never expires and carries no audience.
    pub(crate) async fn mint(
        &self,
        workspace_id: &str,
        user_id: &str,
        name: &str,
        scopes: &[String],
    ) -> Result<Minted, DbError> {
        self.insert(workspace_id, user_id, name, scopes, None, None)
            .await
    }

    /// A connector token (issue #72): expiring, and accepted by the MCP
    /// endpoint only. Its `scopes` must name a subset, so it is never an admin.
    pub(crate) async fn mint_connector(
        &self,
        workspace_id: &str,
        user_id: &str,
        name: &str,
        scopes: &[String],
        expires_at: OffsetDateTime,
    ) -> Result<Minted, DbError> {
        self.insert(
            workspace_id,
            user_id,
            name,
            scopes,
            Some(expires_at),
            Some(MCP_AUDIENCE),
        )
        .await
    }

    /// An actions token (issue #234, a ChatGPT custom GPT's Actions):
    /// expiring, and accepted by the four read-only REST routes the venture
    /// names — nothing else. Its `scopes` are forced `read:`-only here, the
    /// access model's own vocabulary, so every write handler refuses the
    /// token no matter which route it reaches; an empty `scopes` is an
    /// error, so the token always names a subset and is never an admin
    /// credential ([`crate::Member::is_admin`] is false for any token cut
    /// down to a subset).
    ///
    /// The OAuth grant mints through this for its one confidential client,
    /// the predefined Actions client, exactly as it mints a connector token
    /// through [`Store::mint_connector`] for every public one.
    pub(crate) async fn mint_actions(
        &self,
        workspace_id: &str,
        user_id: &str,
        name: &str,
        scopes: &[String],
        expires_at: OffsetDateTime,
    ) -> Result<Minted, DbError> {
        let scopes = Self::read_only(scopes)?;
        self.insert(
            workspace_id,
            user_id,
            name,
            &scopes,
            Some(expires_at),
            Some(crate::ACTIONS_AUDIENCE),
        )
        .await
    }

    /// A scope list as an actions token carries it: every entry
    /// `read:`-prefixed (an entry that already carries the prefix stays), so
    /// the access model reads the subset as read-only reach and no write
    /// handler can open to it. An empty list is an error: an actions token
    /// must name a subset, or it would be a full credential on four routes.
    pub(crate) fn read_only(scopes: &[String]) -> Result<Vec<String>, DbError> {
        if scopes.is_empty() {
            return Err(DbError::Execute(
                "an actions token must name a non-empty scope subset".to_owned(),
            ));
        }
        Ok(scopes
            .iter()
            .map(|scope| {
                if scope.starts_with("read:") {
                    scope.clone()
                } else {
                    format!("read:{scope}")
                }
            })
            .collect())
    }

    async fn insert(
        &self,
        workspace_id: &str,
        user_id: &str,
        name: &str,
        scopes: &[String],
        expires_at: Option<OffsetDateTime>,
        audience: Option<&str>,
    ) -> Result<Minted, DbError> {
        let (id, secret) = self.generate()?;
        let prefix = format!("{PREFIX_TAG}_{id}");
        let token = format!("{prefix}_{secret}");
        let created_at = stamp(self.clock.now());
        let optional = |value: Option<String>| match value {
            Some(value) => text(&value),
            None => Value::String(None),
        };
        let statement = Statement::with_values(
            "INSERT INTO personal_access_tokens \
                 (prefix, secret_hash, workspace_id, user_id, name, scopes, created_at, \
                  expires_at, audience) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            vec![
                text(&prefix),
                text(&sha256_hex(&token)),
                text(workspace_id),
                text(user_id),
                text(name),
                text(&scopes.join(" ")),
                text(&created_at),
                optional(expires_at.map(stamp)),
                optional(audience.map(str::to_owned)),
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
    /// revoked, expired, wrong secret — deliberately indistinguishable.
    pub(crate) async fn verify(&self, token: &str) -> Result<Option<Principal>, DbError> {
        let Some(prefix) = split(token) else {
            return Ok(None);
        };
        let rows = self
            .db
            .query(&Statement::with_values(
                "SELECT secret_hash, workspace_id, user_id, scopes, revoked_at, expires_at, \
                        audience \
                 FROM personal_access_tokens WHERE prefix = ?",
                vec![text(prefix)],
            ))
            .await?;
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        let field = |name: &str| row.get::<Option<String>>(name).flatten();
        // A revoked key is refused by the same path as an unknown one, and so
        // is one past its expiry.
        if field("revoked_at").is_some() {
            return Ok(None);
        }
        if let Some(expiry) = field("expires_at")
            && !live_at(&expiry, self.clock.now())
        {
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
            prefix: prefix.to_owned(),
            workspace_id,
            user_id,
            scopes: split_list(&field("scopes").unwrap_or_default()),
            audience: field("audience"),
        }))
    }

    /// One member's own tokens, newest first. Revoked tokens are gone. An
    /// expired token is hidden too, except a connector whose renewal token is
    /// still live: that connection is idle, not gone, and Settings must still
    /// show it so the person can revoke it.
    pub(crate) async fn list(
        &self,
        workspace_id: &str,
        user_id: &str,
    ) -> Result<Vec<Summary>, DbError> {
        let now = self.clock.now();
        let rows = self
            .db
            .query(&Statement::with_values(
                "SELECT prefix, name, scopes, created_at, expires_at, audience \
                   FROM personal_access_tokens \
                 WHERE workspace_id = ? AND user_id = ? AND revoked_at IS NULL \
                 ORDER BY created_at DESC, prefix ASC",
                vec![text(workspace_id), text(user_id)],
            ))
            .await?;
        // The member's live renewal tokens, by the access token each keeps
        // alive. Best effort: a deployment without the connector tables lists
        // its ordinary tokens exactly as before.
        let renewable: Vec<String> = self
            .db
            .query(&Statement::with_values(
                "SELECT prefix, expires_at FROM oauth_refresh_tokens \
                 WHERE workspace_id = ? AND user_id = ? AND used_at IS NULL",
                vec![text(workspace_id), text(user_id)],
            ))
            .await
            .map(|found| {
                found
                    .rows
                    .iter()
                    .filter(|row| {
                        row.get::<Option<String>>("expires_at")
                            .flatten()
                            .is_some_and(|expiry| live_at(&expiry, now))
                    })
                    .filter_map(|row| row.get::<Option<String>>("prefix").flatten())
                    .collect()
            })
            .unwrap_or_default();
        Ok(rows
            .rows
            .iter()
            .filter_map(|row| {
                let field = |name: &str| row.get::<Option<String>>(name).flatten();
                let prefix = field("prefix").unwrap_or_default();
                let connector = matches!(
                    field("audience").as_deref(),
                    Some(MCP_AUDIENCE) | Some(crate::ACTIONS_AUDIENCE)
                );
                let live = field("expires_at").is_none_or(|expiry| live_at(&expiry, now));
                let renewing = connector && renewable.contains(&prefix);
                (live || renewing).then(|| Summary {
                    scopes: split_list(&field("scopes").unwrap_or_default()),
                    prefix,
                    name: field("name").unwrap_or_default(),
                    created_at: field("created_at").unwrap_or_default(),
                    connector,
                })
            })
            .collect())
    }

    /// Whether the token named by `prefix` has not been revoked. Expiry is
    /// deliberately not consulted: a refresh exists to renew a token that has
    /// run out, so only revocation ends the chain.
    pub(crate) async fn is_unrevoked(&self, prefix: &str) -> Result<bool, DbError> {
        let rows = self
            .db
            .query(&Statement::with_values(
                "SELECT revoked_at FROM personal_access_tokens WHERE prefix = ?",
                vec![text(prefix)],
            ))
            .await?;
        Ok(rows
            .rows
            .first()
            .is_some_and(|row| row.get::<Option<String>>("revoked_at").flatten().is_none()))
    }

    /// Revokes one of a member's own tokens. `Ok(false)` when the prefix
    /// names no token of theirs — deliberately the same answer as a token
    /// that does not exist, so the route cannot probe another member's
    /// prefixes. A connector token takes its renewal chain with it: every
    /// refresh token of the same connection is spent, so nothing renews it.
    pub(crate) async fn revoke(
        &self,
        workspace_id: &str,
        user_id: &str,
        prefix: &str,
    ) -> Result<bool, DbError> {
        let now = stamp(self.clock.now());
        let revoked = self
            .db
            .execute(&Statement::with_values(
                "UPDATE personal_access_tokens SET revoked_at = ? \
                 WHERE prefix = ? AND workspace_id = ? AND user_id = ?",
                vec![text(&now), text(prefix), text(workspace_id), text(user_id)],
            ))
            .await?;
        // Only the caller's own token can end its connection: the family is
        // spent for this member in this workspace, and only when the token
        // above was theirs. Otherwise another member's prefix would spend
        // that member's renewal while answering 404. Best effort: the refresh
        // is refused anyway once its access token is revoked.
        if revoked > 0 {
            let _ = self
                .db
                .execute(&Statement::with_values(
                    "UPDATE oauth_refresh_tokens SET used_at = ? \
                     WHERE used_at IS NULL AND workspace_id = ? AND user_id = ? \
                       AND family IN (SELECT family FROM oauth_refresh_tokens \
                                       WHERE prefix = ? AND workspace_id = ? AND user_id = ?)",
                    vec![
                        text(&now),
                        text(workspace_id),
                        text(user_id),
                        text(prefix),
                        text(workspace_id),
                        text(user_id),
                    ],
                ))
                .await;
        }
        Ok(revoked > 0)
    }
}

/// Whether an expiry stamp is still in the future. An unreadable stamp is not:
/// a token whose expiry cannot be read is refused rather than kept forever.
fn live_at(expiry: &str, now: OffsetDateTime) -> bool {
    OffsetDateTime::parse(expiry, &Rfc3339).is_ok_and(|at| at > now)
}

/// `{prefix}_{secret}` into its public half, refusing a malformed token
/// before it costs a query.
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

    /// An actions token's scopes are minted read-only: every entry gains the
    /// access model's `read:` prefix (an entry that already carries it
    /// stays), and an empty list is refused — no actions token without a
    /// subset, which would be a full credential on its four routes.
    #[test]
    fn an_actions_token_is_minted_read_only() {
        let scopes = Store::read_only(&["shared".to_owned(), "read:user:U0ALICE".to_owned()])
            .expect("a subset mints");
        assert_eq!(scopes, ["read:shared", "read:user:U0ALICE"]);
        assert!(
            Store::read_only(&[]).is_err(),
            "an empty subset is a full credential, not a narrow one"
        );
    }

    /// The mint itself: the row a GPT's Actions hold, exactly as the OAuth
    /// grant will write it, verified through the entry point the four
    /// read-only routes resolve with — its member, never an admin, carrying
    /// the `read:`-only subset — and refused everywhere else.
    #[pollster::test]
    async fn a_minted_actions_token_carries_its_audience_and_read_only_scopes() {
        use cratefield_core::Ports;
        use cratefield_testing::TestHarness;
        use std::sync::{Arc, Mutex};

        const WORKSPACE: &str = "T0ACTIONS";
        const OWNER: &str = "U0GPT";

        let captured: Arc<Mutex<Option<Ports>>> = Arc::new(Mutex::new(None));
        let patch = captured.clone();
        let harness = TestHarness::with_ports(
            vec![
                Box::new(livingbrain_workspaces::Workspaces::new()),
                Box::new(crate::Tokens::new()),
            ],
            move |ports| {
                let mut copy = Ports::empty();
                copy.db = ports.db.clone();
                copy.clock = ports.clock.clone();
                *patch.lock().expect("lock") = Some(copy);
            },
        );
        for row in [
            Statement::with_values(
                "INSERT INTO workspaces (id, name, owner_id, created_at) \
                 VALUES (?, 'A workspace', ?, '2026-01-01T00:00:00Z')",
                vec![text(WORKSPACE), text(OWNER)],
            ),
            Statement::with_values(
                "INSERT INTO workspace_members (workspace_id, user_id, name, timezone, \
                 is_admin, updated_at) VALUES (?, ?, 'A member', NULL, 1, \
                 '2026-01-01T00:00:00Z')",
                vec![text(WORKSPACE), text(OWNER)],
            ),
        ] {
            harness.db.execute(&row).await.expect("the row seeds");
        }
        let ports = captured.lock().expect("lock").take().expect("ports");
        let (db, clock) = (
            ports.db.clone().expect("db"),
            ports.clock.clone().expect("clock"),
        );

        let minted = Store::new(db, clock.clone())
            .mint_actions(
                WORKSPACE,
                OWNER,
                "ChatGPT",
                &["shared".to_owned()],
                clock.now() + time::Duration::days(30),
            )
            .await
            .expect("the actions token mints");
        let value =
            cratefield_core::axum::http::HeaderValue::from_str(&format!("Bearer {}", minted.token))
                .expect("a header value");
        let mut headers = cratefield_core::axum::http::HeaderMap::new();
        headers.insert(cratefield_core::axum::http::header::AUTHORIZATION, value);

        let Ok(crate::Authenticated::Member(member)) =
            crate::authenticate_actions(&ports, &headers).await
        else {
            panic!("the actions surface resolves the minted token");
        };
        assert_eq!(member.user_id, OWNER);
        assert!(!member.is_admin, "an actions token is never an admin");
        assert_eq!(
            member.scopes.as_deref(),
            Some(&["read:shared".to_owned()][..]),
            "the subset is stored read-only"
        );
        assert!(
            crate::authenticate_mcp(&ports, &headers).await.is_err(),
            "the MCP endpoint refuses it, as every surface but its four routes"
        );
    }
}
