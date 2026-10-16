//! The single-use state of the connect round trip.
//!
//! An admin's browser is sent to Discord's bot-install page with a random
//! token; Discord sends it back on the callback. Only the SHA-256 of the token
//! is stored, and spending it is one compare-and-set that also checks the
//! workspace and the member who minted it, so a state is good for exactly one
//! callback by exactly the person who started it.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use cratefield_core::{Database, Statement};
use sha2::{Digest as _, Sha256};

use crate::common::{StoreError, hex, int, require_admin, text, unix};

/// How long a connect step stays good: ten minutes.
pub const STATE_TTL_SECS: i64 = 10 * 60;

fn state_hash(token: &str) -> String {
    hex(&Sha256::digest(token.as_bytes()))
}

/// Mints a state for `member_id`, who must be an owner or admin of
/// `workspace_id` right now.
///
/// # Errors
///
/// [`StoreError::Forbidden`] for anyone else; a storage or randomness failure.
pub async fn create(
    db: &dyn Database,
    now: time::OffsetDateTime,
    workspace_id: &str,
    member_id: &str,
) -> Result<String, StoreError> {
    require_admin(db, workspace_id, member_id).await?;
    let mut raw = [0_u8; 32];
    getrandom::fill(&mut raw).map_err(|_| StoreError::Random)?;
    let token = URL_SAFE_NO_PAD.encode(raw);
    let now = unix(now);
    db.execute(&Statement::with_values(
        "INSERT INTO discord_states (state_hash, workspace_id, created_by, created_at, expires_at) \
         VALUES (?, ?, ?, ?, ?)",
        vec![
            text(&state_hash(&token)),
            text(workspace_id),
            text(member_id),
            int(now),
            int(now + STATE_TTL_SECS),
        ],
    ))
    .await?;
    Ok(token)
}

/// Spends `token` if it is unexpired, unspent and was minted by `member_id`
/// for `workspace_id`. Answers `false` otherwise and spends nothing on a
/// mismatch, so a wrong member cannot burn someone else's state.
///
/// # Errors
///
/// A storage failure.
pub async fn consume(
    db: &dyn Database,
    now: time::OffsetDateTime,
    token: &str,
    workspace_id: &str,
    member_id: &str,
) -> Result<bool, StoreError> {
    if token.is_empty() || token.len() > 128 {
        return Ok(false);
    }
    let now = unix(now);
    Ok(db
        .execute(&Statement::with_values(
            "UPDATE discord_states SET spent_at = ? \
             WHERE state_hash = ? AND spent_at IS NULL AND expires_at >= ? \
               AND workspace_id = ? AND created_by = ?",
            vec![
                int(now),
                text(&state_hash(token)),
                int(now),
                text(workspace_id),
                text(member_id),
            ],
        ))
        .await?
        == 1)
}

/// Forgets states that expired before `before`.
///
/// # Errors
///
/// A storage failure.
pub async fn prune(db: &dyn Database, before: time::OffsetDateTime) -> Result<u64, StoreError> {
    Ok(db
        .execute(&Statement::with_values(
            "DELETE FROM discord_states WHERE expires_at < ?",
            vec![int(unix(before))],
        ))
        .await?)
}
