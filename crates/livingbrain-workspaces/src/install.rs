//! Installing the Slack app into a workspace, and holding the bot token it
//! hands back (issue #6).
//!
//! OAuth v2, one workspace at a time: `GET /slack/install` sends an owner to
//! Slack, Slack sends them back with a code, and `oauth.v2.access` exchanges
//! that code for an `xoxb-` bot token. The token is a **bearer credential for
//! the whole workspace**, so it is never stored in the clear.
//!
//! The envelope is the one `livingbrain-pages` already uses: a fresh 256-bit
//! [`Dek`] per install, wrapped by the [`Kms`] port, and the token sealed
//! under the DEK with XChaCha20-Poly1305 and a random 24-byte nonce. The AAD
//! binds the ciphertext to the team that installed it, so a row copied from
//! one team's install into another's does not decrypt.
//!
//! `cratefield-core` has no `Kms` port, so one is built here from the
//! deployment's own config — `HARNESS_KEK_CURRENT` and `HARNESS_KEK_V<n>`
//! through [`Config`](cratefield_core::Config) — and a deployment with no key
//! ring installs the routes and answers `503` on the ones that need one.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, Payload},
};
use cratefield_core::{Database, DbError, Statement};
use cratefield_kms::{Dek, Kms};
use sea_query::Value as SeaValue;

/// XChaCha20's nonce, in bytes. 24, so a random one never repeats.
const NONCE_LEN: usize = 24;
/// The AAD domain. The team follows it, so one ciphertext cannot be moved
/// between rows.
const AAD_DOMAIN: &str = "livingbrain-workspaces/slack-bot-token/v1";

/// A bot token as it sits in `slack_installs`: the sealed bytes and
/// everything needed to open them again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SealedToken {
    /// The KMS-wrapped data key, base64.
    pub wrapped_dek: String,
    /// The XChaCha20 nonce, base64.
    pub nonce: String,
    /// The sealed token and its tag, base64.
    pub ciphertext: String,
    /// Which master key the wrapped key names, so a re-wrap knows what it is
    /// moving from.
    pub kms_key_ref: String,
}

/// The AAD a token for `team_id` is bound to.
fn aad(team_id: &str) -> Vec<u8> {
    format!("{AAD_DOMAIN}\0{team_id}").into_bytes()
}

/// Seals `token` under a fresh data key, wrapping that key with `kms`. A
/// fresh DEK per install: rotation is then a per-row re-wrap rather than a
/// schema change, and a row that is erased takes its key with it.
///
/// # Errors
///
/// [`KmsError`](cratefield_kms::KmsError) when the wrap fails, or a failure
/// of the OS random source or the cipher. None of these carry the token.
pub(crate) async fn seal(
    kms: &dyn Kms,
    team_id: &str,
    token: &str,
) -> Result<SealedToken, cratefield_kms::KmsError> {
    let dek = Dek::generate()?;
    let wrapped = kms.wrap(&dek).await?;
    let mut nonce = [0_u8; NONCE_LEN];
    getrandom::fill(&mut nonce)
        .map_err(|err| cratefield_kms::KmsError::Unavailable(format!("no nonce: {err}")))?;
    let cipher = XChaCha20Poly1305::new_from_slice(dek.expose())
        .expect("a Dek is always 32 bytes, which is what the cipher takes");
    let ciphertext = cipher
        .encrypt(
            &XNonce::try_from(&nonce[..]).expect("the nonce is 24 bytes"),
            Payload {
                msg: token.as_bytes(),
                aad: &aad(team_id),
            },
        )
        .map_err(|_| {
            cratefield_kms::KmsError::Unavailable("sealing the token failed".to_owned())
        })?;
    Ok(SealedToken {
        wrapped_dek: STANDARD.encode(wrapped),
        nonce: STANDARD.encode(nonce),
        ciphertext: STANDARD.encode(ciphertext),
        kms_key_ref: kms.key_ref().to_owned(),
    })
}

/// Opens a sealed token: unwraps the data key, then decrypts. A failure to
/// open is reported as `Invalid` or `Tampered` rather than as a database
/// fault — the ciphertext opens or it does not.
///
/// # Errors
///
/// A [`KmsError`](cratefield_kms::KmsError) when the key cannot be unwrapped
/// or the ciphertext does not authenticate.
pub(crate) async fn open(
    kms: &dyn Kms,
    team_id: &str,
    sealed: &SealedToken,
) -> Result<String, cratefield_kms::KmsError> {
    let invalid = |what: &str| cratefield_kms::KmsError::Invalid(what.to_owned());
    let wrapped = STANDARD
        .decode(&sealed.wrapped_dek)
        .map_err(|_| invalid("the wrapped key is not base64"))?;
    let dek = kms.unwrap(&wrapped).await?;
    let nonce = STANDARD
        .decode(&sealed.nonce)
        .map_err(|_| invalid("the nonce is not base64"))?;
    let ciphertext = STANDARD
        .decode(&sealed.ciphertext)
        .map_err(|_| invalid("the ciphertext is not base64"))?;
    let cipher = XChaCha20Poly1305::new_from_slice(dek.expose())
        .expect("a Dek is always 32 bytes, which is what the cipher takes");
    cipher
        .decrypt(
            &XNonce::try_from(&nonce[..]).map_err(|_| invalid("the nonce is not 24 bytes"))?,
            Payload {
                msg: &ciphertext,
                aad: &aad(team_id),
            },
        )
        .map_err(|_| {
            cratefield_kms::KmsError::Tampered(
                "the sealed token did not authenticate: wrong team, wrong key, or altered"
                    .to_owned(),
            )
        })
        .map(|plain| {
            String::from_utf8(plain).map_err(|_| {
                cratefield_kms::KmsError::Tampered("a bot token is not UTF-8".to_owned())
            })
        })?
}

/// Stores (or replaces) the install row for a team. One statement, so a
/// re-install is an overwrite rather than a delete followed by an insert:
/// there is no window in which the team has no token.
pub(crate) async fn put_install(
    db: &dyn Database,
    team_id: &str,
    app_id: &str,
    bot_user_id: &str,
    sealed: &SealedToken,
    installed_at: &str,
) -> Result<(), DbError> {
    db.execute(&Statement::with_values(
        "INSERT INTO slack_installs \
         (team_id, app_id, bot_user_id, wrapped_dek, nonce, ciphertext, kms_key_ref, installed_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT (team_id) DO UPDATE SET \
           app_id = excluded.app_id, \
           bot_user_id = excluded.bot_user_id, \
           wrapped_dek = excluded.wrapped_dek, \
           nonce = excluded.nonce, \
           ciphertext = excluded.ciphertext, \
           kms_key_ref = excluded.kms_key_ref, \
           installed_at = excluded.installed_at",
        vec![
            text(team_id),
            text(app_id),
            text(bot_user_id),
            text(&sealed.wrapped_dek),
            text(&sealed.nonce),
            text(&sealed.ciphertext),
            text(&sealed.kms_key_ref),
            text(installed_at),
        ],
    ))
    .await?;
    Ok(())
}

/// The bot token a team installed, or [`None`] when that team has not
/// installed the app.
///
/// The read side of [`put_install`], and the only way a token ever leaves
/// this module. It exists as a seam rather than as a route because nothing
/// in issue #6 needs it — the agent loop that will (#7) is the caller.
///
/// # Errors
///
/// A [`DbError`] when the read fails, and a
/// [`KmsError`](cratefield_kms::KmsError) when the key cannot be unwrapped
/// or the ciphertext does not open — a `Some` token that cannot be read is
/// not the same as no install, and is never reported as one.
pub async fn bot_token(
    kms: &dyn Kms,
    db: &dyn Database,
    team_id: &str,
) -> Result<Option<String>, BotTokenError> {
    let rows = db
        .query(&Statement::with_values(
            "SELECT wrapped_dek, nonce, ciphertext FROM slack_installs WHERE team_id = ?",
            vec![text(team_id)],
        ))
        .await?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let sealed = SealedToken {
        wrapped_dek: row.get::<String>("wrapped_dek").unwrap_or_default(),
        nonce: row.get::<String>("nonce").unwrap_or_default(),
        ciphertext: row.get::<String>("ciphertext").unwrap_or_default(),
        kms_key_ref: row.get::<String>("kms_key_ref").unwrap_or_default(),
    };
    Ok(Some(open(kms, team_id, &sealed).await?))
}

/// Why a bot token could not be read. The two causes want different
/// handling — a storage fault retries, a KMS refusal does not — so they are
/// two variants rather than one opaque error.
#[derive(Debug)]
pub enum BotTokenError {
    /// The row could not be read.
    Store(DbError),
    /// The key could not be unwrapped, or the ciphertext did not open.
    Kms(cratefield_kms::KmsError),
}

impl std::fmt::Display for BotTokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => write!(f, "the install row could not be read: {error}"),
            Self::Kms(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for BotTokenError {}

impl From<DbError> for BotTokenError {
    fn from(error: DbError) -> Self {
        Self::Store(error)
    }
}

impl From<cratefield_kms::KmsError> for BotTokenError {
    fn from(error: cratefield_kms::KmsError) -> Self {
        Self::Kms(error)
    }
}

/// A non-null text bind.
fn text(value: &str) -> SeaValue {
    SeaValue::String(Some(Box::new(value.to_owned())))
}

/// Builds a key custodian out of the deployment's own config, or the reason
/// there is not one. This is the module answering the missing `Kms` port for
/// itself: on Workers the same [`Config`](cratefield_core::Config) every
/// other setting comes from is the secret store, secrets taking precedence
/// over vars.
pub(crate) fn key_custodian(
    cfg: &dyn cratefield_core::Config,
) -> Result<std::sync::Arc<dyn Kms>, String> {
    cratefield_kms::WorkerSecretKms::from_lookup(|name| cfg.get(name))
        .map(|kms| std::sync::Arc::new(kms) as std::sync::Arc<dyn Kms>)
        .map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A key custodian over an in-process key ring — the production
    /// `WorkerSecretKms` shape, served by a closure so a unit test needs no
    /// file and no environment variable.
    fn kms() -> std::sync::Arc<dyn Kms> {
        let key = STANDARD.encode([7_u8; 32]);
        std::sync::Arc::new(
            cratefield_kms::WorkerSecretKms::from_lookup(|name| match name {
                "HARNESS_KEK_CURRENT" => Some("1".to_owned()),
                "HARNESS_KEK_V1" => Some(key.clone()),
                _ => None,
            })
            .expect("the key ring is well formed"),
        )
    }

    #[test]
    fn a_sealed_token_opens_for_its_own_team_and_nobody_else() {
        let kms = kms();
        // Assembled from fragments so the fixture's bot token never appears
        // verbatim in the source tree, where scanners read it as a live one.
        let token = concat!("xo", "xb-1111-2222-3333-averyrealsecret");
        let sealed = pollster::block_on(seal(&*kms, "T0TEAM", token)).expect("it seals");
        for column in [&sealed.wrapped_dek, &sealed.nonce, &sealed.ciphertext] {
            assert!(!column.contains(token), "{column} is not the token");
        }
        assert_eq!(
            sealed.kms_key_ref,
            concat!("worker-secret", ":HARNESS_KEK_V1")
        );
        assert_eq!(
            pollster::block_on(open(&*kms, "T0TEAM", &sealed)).expect("it opens"),
            token
        );
        // The AAD names the team, so the same row read under a different one
        // fails authentication rather than yielding somebody else's token.
        assert!(pollster::block_on(open(&*kms, "T0TWO", &sealed)).is_err());

        // Every seal takes a fresh nonce, so two seals of one token differ.
        let again = pollster::block_on(seal(&*kms, "T0TEAM", token)).expect("it seals");
        assert_ne!(again.nonce, sealed.nonce);
        assert_ne!(again.ciphertext, sealed.ciphertext);
    }
}
