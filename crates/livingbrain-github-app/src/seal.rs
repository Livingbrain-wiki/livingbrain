//! The credential envelope: a fresh 256-bit data key per app row, wrapped by
//! the [`Kms`] port, and each credential sealed under it with
//! XChaCha20-Poly1305 and its own random 24-byte nonce.
//!
//! The AAD names the credential's **purpose**, the **workspace** and the
//! **app id**. So a sealed private key cannot be opened as the webhook
//! secret (column swap), and a row copied into another workspace or onto
//! another app id does not open (row swap). It is the Slack bot token's
//! envelope (`livingbrain-workspaces/src/install.rs`) with a wider binding.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, Payload},
};
use cratefield_kms::{Dek, Kms, KmsError};
use zeroize::Zeroizing;

/// XChaCha20's nonce, in bytes. 24, so a random one never repeats.
const NONCE_LEN: usize = 24;
/// The AAD domain. Bumping the version is how a future envelope change
/// makes every old ciphertext refuse to open under the new rules.
const AAD_DOMAIN: &str = "livingbrain-github-app/app-credential/v1";

/// A credential in memory: wiped on drop, never printed, never serialised,
/// never cloned. The only way to read it is [`Secret::expose`], so every
/// place that touches the plaintext says so in the source.
pub struct Secret(Zeroizing<String>);

impl Secret {
    /// Wraps a credential. The `String` is moved, not copied, so the only
    /// plaintext buffer is the one this value wipes.
    #[must_use]
    pub fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }

    /// The plaintext. Use it, do not keep it: a copy taken from here is not
    /// wiped by anyone.
    #[must_use]
    pub fn expose(&self) -> &str {
        self.0.as_str()
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

/// Which credential a ciphertext holds. Part of the AAD.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Purpose {
    PrivateKey,
    WebhookSecret,
    ClientSecret,
}

impl Purpose {
    const fn label(self) -> &'static str {
        match self {
            Self::PrivateKey => "private-key",
            Self::WebhookSecret => "webhook-secret",
            Self::ClientSecret => "client-secret",
        }
    }
}

/// The AAD one credential is bound to.
fn aad(purpose: Purpose, workspace_id: &str, app_id: i64) -> Vec<u8> {
    format!(
        "{AAD_DOMAIN}\0{}\0{workspace_id}\0{app_id}",
        purpose.label()
    )
    .into_bytes()
}

/// One sealed credential: its nonce and ciphertext, both base64.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Sealed {
    pub nonce: String,
    pub ciphertext: String,
}

/// A data key for one row: the plaintext key (wiped on drop) and its
/// wrapped form for storage.
pub(crate) struct RowKey {
    dek: Dek,
    pub wrapped: String,
    pub kms_key_ref: String,
}

impl RowKey {
    /// A fresh key, wrapped by `kms`.
    pub(crate) async fn generate(kms: &dyn Kms) -> Result<Self, KmsError> {
        let dek = Dek::generate()?;
        let wrapped = kms.wrap(&dek).await?;
        Ok(Self {
            dek,
            wrapped: STANDARD.encode(wrapped),
            kms_key_ref: kms.key_ref().to_owned(),
        })
    }

    /// The key a stored row names, unwrapped by `kms`.
    pub(crate) async fn unwrap(kms: &dyn Kms, wrapped: &str) -> Result<Self, KmsError> {
        let bytes = STANDARD
            .decode(wrapped)
            .map_err(|_| KmsError::Invalid("the wrapped key is not base64".to_owned()))?;
        let dek = kms.unwrap(&bytes).await?;
        Ok(Self {
            dek,
            wrapped: wrapped.to_owned(),
            kms_key_ref: kms.key_ref().to_owned(),
        })
    }

    fn cipher(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new_from_slice(self.dek.expose())
            .expect("a Dek is always 32 bytes, which is what the cipher takes")
    }

    /// Seals one credential.
    pub(crate) fn seal(
        &self,
        purpose: Purpose,
        workspace_id: &str,
        app_id: i64,
        plaintext: &Secret,
    ) -> Result<Sealed, KmsError> {
        let mut nonce = [0_u8; NONCE_LEN];
        getrandom::fill(&mut nonce)
            .map_err(|err| KmsError::Unavailable(format!("no nonce: {err}")))?;
        let ciphertext = self
            .cipher()
            .encrypt(
                &XNonce::try_from(&nonce[..]).expect("the nonce is 24 bytes"),
                Payload {
                    msg: plaintext.expose().as_bytes(),
                    aad: &aad(purpose, workspace_id, app_id),
                },
            )
            .map_err(|_| KmsError::Unavailable("sealing a credential failed".to_owned()))?;
        Ok(Sealed {
            nonce: STANDARD.encode(nonce),
            ciphertext: STANDARD.encode(ciphertext),
        })
    }

    /// Opens one credential. Anything that does not authenticate — another
    /// purpose, workspace or app id, another key, an altered byte — is
    /// [`KmsError::Tampered`], and the error never carries plaintext.
    pub(crate) fn open(
        &self,
        purpose: Purpose,
        workspace_id: &str,
        app_id: i64,
        sealed: &Sealed,
    ) -> Result<Secret, KmsError> {
        let invalid = |what: &str| KmsError::Invalid(what.to_owned());
        let nonce = STANDARD
            .decode(&sealed.nonce)
            .map_err(|_| invalid("the nonce is not base64"))?;
        let ciphertext = STANDARD
            .decode(&sealed.ciphertext)
            .map_err(|_| invalid("the ciphertext is not base64"))?;
        let plain = Zeroizing::new(
            self.cipher()
                .decrypt(
                    &XNonce::try_from(&nonce[..])
                        .map_err(|_| invalid("the nonce is not 24 bytes"))?,
                    Payload {
                        msg: &ciphertext,
                        aad: &aad(purpose, workspace_id, app_id),
                    },
                )
                .map_err(|_| {
                    KmsError::Tampered(
                        "a sealed GitHub credential did not authenticate: wrong purpose, \
                         workspace, app, key, or altered"
                            .to_owned(),
                    )
                })?,
        );
        let text = std::str::from_utf8(&plain)
            .map_err(|_| KmsError::Tampered("a sealed credential is not UTF-8".to_owned()))?;
        Ok(Secret::new(text.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn kms(byte: u8) -> std::sync::Arc<dyn Kms> {
        let key = STANDARD.encode([byte; 32]);
        std::sync::Arc::new(
            cratefield_kms::WorkerSecretKms::from_lookup(|name| match name {
                "HARNESS_KEK_CURRENT" => Some("1".to_owned()),
                "HARNESS_KEK_V1" => Some(key.clone()),
                _ => None,
            })
            .expect("the key ring is well formed"),
        )
    }

    fn pem() -> Secret {
        // Assembled from fragments so no scanner reads the fixture as a key.
        Secret::new(format!(
            "-----BEGIN {} KEY-----\nMIIfixturefixturefixture\n-----END {} KEY-----\n",
            "RSA PRIVATE", "RSA PRIVATE"
        ))
    }

    #[test]
    fn a_credential_round_trips_and_its_ciphertext_is_not_its_plaintext() {
        pollster::block_on(async {
            let kms = kms(7);
            let key = RowKey::generate(&*kms).await.expect("a key");
            let secret = pem();
            let sealed = key
                .seal(Purpose::PrivateKey, "W1", 42, &secret)
                .expect("it seals");
            for column in [&sealed.nonce, &sealed.ciphertext, &key.wrapped] {
                assert!(!column.contains("MIIfixture"), "{column}");
                assert_ne!(column.as_str(), secret.expose());
            }
            let again = RowKey::unwrap(&*kms, &key.wrapped).await.expect("unwraps");
            let opened = again
                .open(Purpose::PrivateKey, "W1", 42, &sealed)
                .expect("it opens");
            assert_eq!(opened.expose(), secret.expose());

            // A second seal of the same plaintext takes a fresh nonce.
            let twice = key
                .seal(Purpose::PrivateKey, "W1", 42, &secret)
                .expect("it seals");
            assert_ne!(twice.nonce, sealed.nonce);
            assert_ne!(twice.ciphertext, sealed.ciphertext);
        });
    }

    #[test]
    fn an_aad_mismatch_refuses_to_open() {
        pollster::block_on(async {
            let kms = kms(7);
            let key = RowKey::generate(&*kms).await.expect("a key");
            let sealed = key
                .seal(Purpose::PrivateKey, "W1", 42, &pem())
                .expect("it seals");
            // Another purpose (column swap), workspace (row swap) or app id.
            for (purpose, workspace, app) in [
                (Purpose::WebhookSecret, "W1", 42),
                (Purpose::ClientSecret, "W1", 42),
                (Purpose::PrivateKey, "W2", 42),
                (Purpose::PrivateKey, "W1", 43),
            ] {
                let error = key
                    .open(purpose, workspace, app, &sealed)
                    .expect_err("the AAD differs");
                assert!(matches!(error, KmsError::Tampered(_)), "{error:?}");
                assert!(!error.to_string().contains("MIIfixture"));
            }
            // Another data key does not open it either.
            let other = RowKey::generate(&*kms).await.expect("a key");
            assert!(other.open(Purpose::PrivateKey, "W1", 42, &sealed).is_err());
            // Nor does the right key under a different master key.
            assert!(RowKey::unwrap(&*self::kms(9), &key.wrapped).await.is_err());
        });
    }

    #[test]
    fn a_secret_never_prints() {
        let secret = pem();
        assert_eq!(format!("{secret:?}"), "Secret([redacted])");
    }
}
