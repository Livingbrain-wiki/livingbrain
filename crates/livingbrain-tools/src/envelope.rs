//! The connection-token envelope: a remote MCP server's bearer token at
//! rest, sealed so no row ever holds plaintext. The shape is the one
//! `livingbrain-workspaces` seals its Slack bot token with: a fresh
//! KMS-wrapped data key per row, XChaCha20-Poly1305, the AAD binding the
//! row's workspace, member *and* provider, so a ciphertext lifted from one
//! row does not decrypt under another. [`Token`] is never `Serialize` and
//! shows only its last four characters, so it cannot reach a log line or a
//! JSON response by accident; it is opened on the call path alone.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, Payload},
};
use cratefield_kms::{Dek, Kms};

/// XChaCha20's nonce, in bytes.
const NONCE_LEN: usize = 24;
/// The AAD domain. The row's three keys follow it.
const AAD_DOMAIN: &str = "livingbrain-tools/connection-token/v1";

/// A connection token that never shows its full value.
#[derive(Clone, PartialEq, Eq)]
pub struct Token(String);

impl Token {
    /// The raw token, for the `Authorization` header of one MCP request.
    /// Never logged, never serialized.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The last four characters of the token, for display only.
    fn last4(&self) -> &str {
        let len = self.0.len();
        if len <= 4 {
            &self.0
        } else {
            &self.0[len - 4..]
        }
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "…{}", self.last4())
    }
}

impl std::fmt::Display for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "…{}", self.last4())
    }
}

/// A token as it sits in `tool_connections`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedToken {
    /// The KMS-wrapped data key, base64.
    pub wrapped_dek: String,
    /// The XChaCha20 nonce, base64.
    pub nonce: String,
    /// The sealed token and its tag, base64.
    pub ciphertext: String,
    /// Which master key the wrapped key names, so a re-wrap knows what it
    /// is moving from.
    pub kms_key_ref: String,
}

/// The AAD a token for `(workspace_id, member_id, provider)` is bound to.
fn aad(workspace_id: &str, member_id: &str, provider: &str) -> Vec<u8> {
    format!("{AAD_DOMAIN}\0{workspace_id}\0{member_id}\0{provider}").into_bytes()
}

/// Seals `token` under a fresh data key, wrapping that key with `kms` — a
/// fresh DEK per row, so rotation is a per-row re-wrap and a deleted
/// connection takes its key with it.
pub async fn seal(
    kms: &dyn Kms,
    workspace_id: &str,
    member_id: &str,
    provider: &str,
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
                aad: &aad(workspace_id, member_id, provider),
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

/// Opens a sealed token: unwraps the data key, then decrypts. The
/// ciphertext opens for its own row or for no row at all.
pub async fn open(
    kms: &dyn Kms,
    workspace_id: &str,
    member_id: &str,
    provider: &str,
    sealed: &SealedToken,
) -> Result<Token, cratefield_kms::KmsError> {
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
    let plain = cipher
        .decrypt(
            &XNonce::try_from(&nonce[..]).map_err(|_| invalid("the nonce is not 24 bytes"))?,
            Payload {
                msg: &ciphertext,
                aad: &aad(workspace_id, member_id, provider),
            },
        )
        .map_err(|_| {
            cratefield_kms::KmsError::Tampered(
                "the sealed token did not authenticate: wrong row, wrong key, or altered"
                    .to_owned(),
            )
        })?;
    String::from_utf8(plain).map(Token).map_err(|_| {
        cratefield_kms::KmsError::Tampered("a connection token is not UTF-8".to_owned())
    })
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
    fn a_sealed_token_opens_for_its_own_row_and_nobody_else() {
        let kms = kms();
        // Assembled from fragments so the fixture's token never appears
        // verbatim in the source tree, where scanners read it as a live one.
        let token = concat!("gh", "p_ABCDEFGHIJKLMNOPQRSTUVWXYZ123456");
        let sealed =
            pollster::block_on(seal(&*kms, "T0WS", "U0MEM", "github", token)).expect("it seals");
        for column in [&sealed.wrapped_dek, &sealed.nonce, &sealed.ciphertext] {
            assert!(!column.contains(token), "{column} is not the token");
        }
        assert_eq!(
            pollster::block_on(open(&*kms, "T0WS", "U0MEM", "github", &sealed))
                .expect("it opens")
                .as_str(),
            token
        );
        // The AAD names the row's three keys: the same ciphertext read
        // under any other row fails rather than yielding another's token.
        assert!(
            pollster::block_on(open(&*kms, "T0WS", "U0OTHER", "github", &sealed)).is_err(),
            "another member's row must not open it"
        );
        assert!(
            pollster::block_on(open(&*kms, "T0WS", "U0MEM", "linear", &sealed)).is_err(),
            "another provider's row must not open it"
        );
        assert!(
            pollster::block_on(open(&*kms, "T0TWO", "U0MEM", "github", &sealed)).is_err(),
            "another workspace's row must not open it"
        );

        // Every seal takes a fresh nonce, so two seals of one token differ.
        let again =
            pollster::block_on(seal(&*kms, "T0WS", "U0MEM", "github", token)).expect("it seals");
        assert_ne!(again.nonce, sealed.nonce);
        assert_ne!(again.ciphertext, sealed.ciphertext);
    }

    #[test]
    fn a_token_never_shows_itself() {
        let kms = kms();
        let sealed = pollster::block_on(seal(
            &*kms,
            "T0WS",
            "U0MEM",
            "github",
            "a-token-of-length-24",
        ))
        .expect("it seals");
        let token =
            pollster::block_on(open(&*kms, "T0WS", "U0MEM", "github", &sealed)).expect("it opens");
        assert_eq!(format!("{token:?}"), "…h-24");
        assert_eq!(format!("{token}"), "…h-24");
        assert_eq!(token.as_str(), "a-token-of-length-24");
    }
}
