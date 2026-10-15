//! The workspace's GitHub App: the single-use states of the manifest and
//! install round trips, and the app's sealed credentials.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use cratefield_core::{Database, Statement};
use cratefield_kms::Kms;
use sha2::{Digest, Sha256};

use crate::common::{
    StoreError, check_github_id, check_id, get_int, get_opt_text, get_text, hex, int,
    require_admin, text, unix,
};
use crate::seal::{Purpose, RowKey, Sealed, Secret};

/// How long a state token lives when the caller does not say: long enough to
/// fill in GitHub's form, short enough that a leaked one is soon worthless.
pub const DEFAULT_STATE_TTL_SECS: i64 = 15 * 60;
/// The longest a state token may be asked to live.
pub const MAX_STATE_TTL_SECS: i64 = 60 * 60;

/// Which round trip a state token guards. A token minted for one is refused
/// by the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatePurpose {
    /// `POST github.com/settings/apps/new?state=…` → the manifest redirect
    /// with `code` and `state`.
    Manifest,
    /// `github.com/apps/<slug>/installations/new?state=…` → the setup URL
    /// with `installation_id` and `state`.
    Install,
}

impl StatePurpose {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Manifest => "manifest",
            Self::Install => "install",
        }
    }
}

/// Proof that a state token was spent by the member who minted it, in the
/// workspace it was minted for, before it expired, for this purpose. It
/// cannot be built outside this crate, so [`save_app`] and
/// [`bind_installation`](crate::bind_installation) can only be reached
/// through a real round trip. Not `Clone`, and both take it by value: one
/// round trip authorises one write.
#[derive(Debug, PartialEq, Eq)]
pub struct StateProof {
    pub(crate) purpose: StatePurpose,
    pub(crate) workspace_id: String,
    pub(crate) member_id: String,
}

impl StateProof {
    /// The workspace the state was minted for.
    #[must_use]
    pub fn workspace_id(&self) -> &str {
        &self.workspace_id
    }

    /// The member who minted (and spent) it.
    #[must_use]
    pub fn member_id(&self) -> &str {
        &self.member_id
    }

    /// Which round trip it guarded.
    #[must_use]
    pub fn purpose(&self) -> StatePurpose {
        self.purpose
    }
}

fn state_hash(token: &str) -> String {
    hex(&Sha256::digest(token.as_bytes()))
}

/// Mints a single-use state token for `purpose`, bound to `workspace_id` and
/// to `member_id`, who must be an owner or admin of it right now. Answers
/// the token, which is the only copy: the table holds its SHA-256.
///
/// `ttl_secs` is clamped to `1..=`[`MAX_STATE_TTL_SECS`].
///
/// # Errors
///
/// [`StoreError::Forbidden`] for anyone but an owner or admin, and the
/// storage or randomness failure otherwise.
pub async fn create_state(
    db: &dyn Database,
    now: time::OffsetDateTime,
    purpose: StatePurpose,
    workspace_id: &str,
    member_id: &str,
    ttl_secs: i64,
) -> Result<String, StoreError> {
    check_id(workspace_id, "workspace id")?;
    check_id(member_id, "member id")?;
    require_admin(db, workspace_id, member_id).await?;
    let mut raw = [0_u8; 32];
    getrandom::fill(&mut raw).map_err(|err| {
        StoreError::Kms(cratefield_kms::KmsError::Unavailable(format!(
            "no randomness: {err}"
        )))
    })?;
    let token = URL_SAFE_NO_PAD.encode(raw);
    let now = unix(now);
    db.execute(&Statement::with_values(
        "INSERT INTO github_states \
           (state_hash, purpose, workspace_id, created_by, created_at, expires_at) \
         VALUES (?, ?, ?, ?, ?, ?)",
        vec![
            text(&state_hash(&token)),
            text(purpose.as_str()),
            text(workspace_id),
            text(member_id),
            int(now),
            int(now + ttl_secs.clamp(1, MAX_STATE_TTL_SECS)),
        ],
    ))
    .await?;
    Ok(token)
}

/// Spends a state token. The spend is a compare-and-set on `spent_at`, so of
/// two racing callbacks exactly one gets past it — and the spend happens
/// **before** the checks, so a token presented with the wrong workspace,
/// member or purpose, or after it expired, is burnt rather than left for a
/// second try.
///
/// `None` is every refusal (unknown, spent, expired, another workspace,
/// another member, another purpose, the member no longer an owner or
/// admin): one answer, so a probe learns nothing about which.
///
/// # Errors
///
/// Only a storage failure.
pub async fn consume_state(
    db: &dyn Database,
    now: time::OffsetDateTime,
    purpose: StatePurpose,
    token: &str,
    workspace_id: &str,
    member_id: &str,
) -> Result<Option<StateProof>, StoreError> {
    if token.is_empty() || token.len() > 128 {
        return Ok(None);
    }
    let hash = state_hash(token);
    let now = unix(now);
    let spent = db
        .execute(&Statement::with_values(
            "UPDATE github_states SET spent_at = ? WHERE state_hash = ? AND spent_at IS NULL",
            vec![int(now), text(&hash)],
        ))
        .await?;
    if spent == 0 {
        return Ok(None);
    }
    let rows = db
        .query(&Statement::with_values(
            "SELECT purpose, workspace_id, created_by, expires_at FROM github_states \
             WHERE state_hash = ?",
            vec![text(&hash)],
        ))
        .await?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let matches = get_text(row, "purpose") == purpose.as_str()
        && get_text(row, "workspace_id") == workspace_id
        && get_text(row, "created_by") == member_id
        && now < get_int(row, "expires_at");
    if !matches {
        return Ok(None);
    }
    match require_admin(db, workspace_id, member_id).await {
        Ok(()) => Ok(Some(StateProof {
            purpose,
            workspace_id: workspace_id.to_owned(),
            member_id: member_id.to_owned(),
        })),
        Err(StoreError::Forbidden) => Ok(None),
        Err(other) => Err(other),
    }
}

/// Deletes state rows that expired before `before`. Housekeeping; safe to
/// call from anywhere.
///
/// # Errors
///
/// A storage failure.
pub async fn prune_states(
    db: &dyn Database,
    before: time::OffsetDateTime,
) -> Result<u64, StoreError> {
    Ok(db
        .execute(&Statement::with_values(
            "DELETE FROM github_states WHERE expires_at < ?",
            vec![int(unix(before))],
        ))
        .await?)
}

/// What the manifest conversion (`POST /app-manifests/{code}/conversions`)
/// hands back, as the store takes it. The credentials are [`Secret`]s from
/// the moment they are parsed.
#[derive(Debug)]
pub struct NewApp {
    pub app_id: i64,
    pub slug: String,
    pub client_id: String,
    pub private_key_pem: Secret,
    pub webhook_secret: Secret,
    pub client_secret: Secret,
}

/// What [`save_app`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveAppOutcome {
    /// The app is stored, its credentials sealed.
    Saved,
    /// The workspace already has an app. Nothing was written; remove the old
    /// one first ([`remove_app`]) — an app is never silently replaced.
    WorkspaceHasApp,
    /// This app id is already stored for another workspace. Nothing was
    /// written.
    AppIdTaken,
}

/// Stores a newly created app for the workspace a spent **manifest** state
/// names, sealing its three credentials. The proof is what makes this
/// unreachable without a real round trip, and the membership is re-read so
/// a member demoted mid-flow cannot finish it.
///
/// # Errors
///
/// [`StoreError::Forbidden`] when the proof is for another purpose or its
/// member is no longer an owner or admin; a KMS or storage failure.
pub async fn save_app(
    kms: &dyn Kms,
    db: &dyn Database,
    now: time::OffsetDateTime,
    proof: StateProof,
    app: &NewApp,
) -> Result<SaveAppOutcome, StoreError> {
    if proof.purpose != StatePurpose::Manifest {
        return Err(StoreError::Forbidden);
    }
    check_github_id(app.app_id, "app id")?;
    if app.slug.is_empty() || app.slug.len() > 100 || app.client_id.is_empty() {
        return Err(StoreError::Invalid("app slug or client id"));
    }
    if app.private_key_pem.expose().is_empty() || app.webhook_secret.expose().is_empty() {
        return Err(StoreError::Invalid("app credential"));
    }
    require_admin(db, &proof.workspace_id, &proof.member_id).await?;

    let ws = proof.workspace_id.as_str();
    let key = RowKey::generate(kms).await?;
    let private_key = key.seal(Purpose::PrivateKey, ws, app.app_id, &app.private_key_pem)?;
    let webhook = key.seal(Purpose::WebhookSecret, ws, app.app_id, &app.webhook_secret)?;
    let client = key.seal(Purpose::ClientSecret, ws, app.app_id, &app.client_secret)?;

    // `ON CONFLICT DO NOTHING` with no target covers both the app id key and
    // the one-app-per-workspace key; which one refused is read back below.
    let inserted = db
        .execute(&Statement::with_values(
            "INSERT INTO github_app \
               (app_id, workspace_id, slug, client_id, wrapped_dek, kms_key_ref, \
                private_key_nonce, private_key_sealed, webhook_secret_nonce, \
                webhook_secret_sealed, client_secret_nonce, client_secret_sealed, \
                created_by, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT DO NOTHING",
            vec![
                int(app.app_id),
                text(ws),
                text(&app.slug),
                text(&app.client_id),
                text(&key.wrapped),
                text(&key.kms_key_ref),
                text(&private_key.nonce),
                text(&private_key.ciphertext),
                text(&webhook.nonce),
                text(&webhook.ciphertext),
                text(&client.nonce),
                text(&client.ciphertext),
                text(&proof.member_id),
                int(unix(now)),
            ],
        ))
        .await?;
    if inserted == 1 {
        return Ok(SaveAppOutcome::Saved);
    }
    if app_summary(db, ws).await?.is_some() {
        Ok(SaveAppOutcome::WorkspaceHasApp)
    } else {
        Ok(SaveAppOutcome::AppIdTaken)
    }
}

/// The app as a settings page may show it: no credential, no ciphertext.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppSummary {
    pub app_id: i64,
    pub slug: String,
    pub client_id: String,
    pub created_by: Option<String>,
    pub created_at: i64,
}

/// The workspace's app, without its credentials, or `None`.
///
/// # Errors
///
/// A storage failure.
pub async fn app_summary(
    db: &dyn Database,
    workspace_id: &str,
) -> Result<Option<AppSummary>, StoreError> {
    let rows = db
        .query(&Statement::with_values(
            "SELECT app_id, slug, client_id, created_by, created_at FROM github_app \
             WHERE workspace_id = ?",
            vec![text(workspace_id)],
        ))
        .await?;
    Ok(rows.first().map(|row| AppSummary {
        app_id: get_int(row, "app_id"),
        slug: get_text(row, "slug"),
        client_id: get_text(row, "client_id"),
        created_by: get_opt_text(row, "created_by"),
        created_at: get_int(row, "created_at"),
    }))
}

/// The app's credentials, unsealed. **Internal use only**: minting the app
/// JWT and the installation token, and the OAuth code exchange. Never put
/// any field of this in a response, a log line, an error or a queue row.
#[derive(Debug)]
pub struct AppCredentials {
    pub app_id: i64,
    pub slug: String,
    pub client_id: String,
    pub private_key_pem: Secret,
    pub webhook_secret: Secret,
    pub client_secret: Secret,
}

/// Opens the workspace's app credentials, or `None` when it has no app.
///
/// # Errors
///
/// A storage failure, or a KMS failure — including `Tampered` when a
/// ciphertext does not open, which is never reported as "no app".
pub async fn app_credentials(
    kms: &dyn Kms,
    db: &dyn Database,
    workspace_id: &str,
) -> Result<Option<AppCredentials>, StoreError> {
    let rows = db
        .query(&Statement::with_values(
            "SELECT app_id, slug, client_id, wrapped_dek, private_key_nonce, \
                    private_key_sealed, webhook_secret_nonce, webhook_secret_sealed, \
                    client_secret_nonce, client_secret_sealed \
             FROM github_app WHERE workspace_id = ?",
            vec![text(workspace_id)],
        ))
        .await?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let app_id = get_int(row, "app_id");
    let key = RowKey::unwrap(kms, &get_text(row, "wrapped_dek")).await?;
    let open = |purpose, prefix: &str| {
        key.open(
            purpose,
            workspace_id,
            app_id,
            &Sealed {
                nonce: get_text(row, &format!("{prefix}_nonce")),
                ciphertext: get_text(row, &format!("{prefix}_sealed")),
            },
        )
    };
    Ok(Some(AppCredentials {
        app_id,
        slug: get_text(row, "slug"),
        client_id: get_text(row, "client_id"),
        private_key_pem: open(Purpose::PrivateKey, "private_key")?,
        webhook_secret: open(Purpose::WebhookSecret, "webhook_secret")?,
        client_secret: open(Purpose::ClientSecret, "client_secret")?,
    }))
}

/// The webhook secret for `app_id` and the workspace that app belongs to —
/// what the webhook endpoint needs **before** it knows the workspace, to
/// verify `X-Hub-Signature-256`. Opens only that one credential. `None` for
/// an app id this deployment does not hold.
///
/// # Errors
///
/// A storage or KMS failure.
pub async fn webhook_secret(
    kms: &dyn Kms,
    db: &dyn Database,
    app_id: i64,
) -> Result<Option<(String, Secret)>, StoreError> {
    let rows = db
        .query(&Statement::with_values(
            "SELECT workspace_id, wrapped_dek, webhook_secret_nonce, webhook_secret_sealed \
             FROM github_app WHERE app_id = ?",
            vec![int(app_id)],
        ))
        .await?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let workspace_id = get_text(row, "workspace_id");
    let key = RowKey::unwrap(kms, &get_text(row, "wrapped_dek")).await?;
    let secret = key.open(
        Purpose::WebhookSecret,
        &workspace_id,
        app_id,
        &Sealed {
            nonce: get_text(row, "webhook_secret_nonce"),
            ciphertext: get_text(row, "webhook_secret_sealed"),
        },
    )?;
    Ok(Some((workspace_id, secret)))
}

/// Removes the workspace's app: the credential row goes, every installation
/// of it is marked deleted, its repositories stop and its queued work is
/// dropped — in one atomic batch. Owner or admin only. Answers whether
/// there was an app to remove.
///
/// The GitHub side (deleting the app on github.com) is the operator's; this
/// forgets the credentials so nothing here can act as the app again.
///
/// # Errors
///
/// [`StoreError::Forbidden`] for anyone but an owner or admin; a storage
/// failure.
pub async fn remove_app(
    db: &dyn Database,
    now: time::OffsetDateTime,
    workspace_id: &str,
    member_id: &str,
) -> Result<bool, StoreError> {
    require_admin(db, workspace_id, member_id).await?;
    let Some(app) = app_summary(db, workspace_id).await? else {
        return Ok(false);
    };
    let now = unix(now);
    db.batch_atomic(&[
        Statement::with_values(
            "UPDATE github_repos SET active = 0, updated_at = ? \
             WHERE workspace_id = ? AND installation_id IN \
               (SELECT installation_id FROM github_installations WHERE app_id = ?)",
            vec![int(now), text(workspace_id), int(app.app_id)],
        ),
        Statement::with_values(
            "DELETE FROM github_queue WHERE workspace_id = ? AND installation_id IN \
               (SELECT installation_id FROM github_installations WHERE app_id = ?)",
            vec![text(workspace_id), int(app.app_id)],
        ),
        Statement::with_values(
            "UPDATE github_installations SET status = 'deleted', updated_at = ? \
             WHERE app_id = ?",
            vec![int(now), int(app.app_id)],
        ),
        Statement::with_values(
            "DELETE FROM github_app WHERE workspace_id = ? AND app_id = ?",
            vec![text(workspace_id), int(app.app_id)],
        ),
    ])
    .await?;
    Ok(true)
}
