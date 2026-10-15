//! Replay protection for webhook deliveries.
//!
//! A GitHub delivery is signed (`X-Hub-Signature-256`) but the signature
//! covers no timestamp, so a captured delivery verifies forever. The
//! delivery id (`X-GitHub-Delivery`) is the only thing that tells a replay
//! from a new event; this table remembers the ones already accepted.

use cratefield_core::{Database, Statement};

use crate::common::{StoreError, bounded, check_github_id, int, text, unix};

/// How long an accepted delivery id is remembered. Work items are
/// references that the drain re-reads from GitHub, so a replay older than
/// this costs a refetch, not a wrong write.
pub const DELIVERY_RETENTION_SECS: i64 = 30 * 24 * 60 * 60;

/// Records a delivery for `app_id` (whose secret verified it) and answers
/// `true` when it is new, `false` when it is a replay. One `INSERT … ON
/// CONFLICT DO NOTHING`, so two racing copies of one delivery cannot both be
/// told "new". Call it **after** the signature verifies — an unverified body
/// must not be able to burn a delivery id.
///
/// # Errors
///
/// [`StoreError::Invalid`] for an id that is not a GitHub delivery GUID
/// (hex and dashes, at most 64 characters); a storage failure.
pub async fn record_delivery(
    db: &dyn Database,
    now: time::OffsetDateTime,
    app_id: i64,
    delivery_id: &str,
    event: &str,
) -> Result<bool, StoreError> {
    check_github_id(app_id, "app id")?;
    let well_formed = !delivery_id.is_empty()
        && delivery_id.len() <= 64
        && delivery_id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() || b == b'-');
    if !well_formed {
        return Err(StoreError::Invalid("delivery id"));
    }
    let inserted = db
        .execute(&Statement::with_values(
            "INSERT INTO github_deliveries (app_id, delivery_id, event, received_at) \
             VALUES (?, ?, ?, ?) ON CONFLICT (app_id, delivery_id) DO NOTHING",
            vec![
                int(app_id),
                text(&delivery_id.to_ascii_lowercase()),
                text(&bounded(event, 64)),
                int(unix(now)),
            ],
        ))
        .await?;
    Ok(inserted == 1)
}

/// Forgets deliveries older than [`DELIVERY_RETENTION_SECS`] before `now`.
/// Housekeeping for the cron drain; answers how many went.
///
/// # Errors
///
/// A storage failure.
pub async fn prune_deliveries(
    db: &dyn Database,
    now: time::OffsetDateTime,
) -> Result<u64, StoreError> {
    Ok(db
        .execute(&Statement::with_values(
            "DELETE FROM github_deliveries WHERE received_at < ?",
            vec![int(unix(now) - DELIVERY_RETENTION_SECS)],
        ))
        .await?)
}
