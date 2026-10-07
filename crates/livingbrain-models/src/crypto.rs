//! AES-256-GCM encryption of provider keys at rest, with an `ApiKey`
//! newtype that never leaks the full key.
//!
//! The encryption key is SHA-256 of the `MODEL_KEYS_SECRET` config secret;
//! the AAD is `workspace_id + ":" + role`, so a ciphertext lifted from one
//! row cannot decrypt in another. If the secret is missing, connecting
//! fails with a 500 rather than storing plaintext.
//!
//! The nonce is the *random* part of two ULIDs, not their text. A ULID is
//! big-endian Crockford base32: its first ten characters are a millisecond
//! timestamp and its last sixteen are 80 random bits, so taking the first
//! twelve characters of the string would spend every nonce's entropy on
//! the clock. [`nonce`] decodes the random half of each ULID instead —
//! 80 bits from the first and 16 from the second, 96 in all, which is the
//! uniqueness AES-GCM's 96-bit nonce needs.
//!
//! No `OsRng` / `getrandom` / `wasm-bindgen` is pulled in: the `IdGen`
//! port is the module's only source of randomness, the same port the
//! workspaces module uses for CSRF state and nonces. This keeps the
//! module's wasm graph clean (`assert_wasm_safe_deps`).

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use serde::{Deserialize, Deserializer};
use sha2::{Digest, Sha256};

use cratefield_core::{Config, IdGen};

/// The config key for the encryption secret.
pub(crate) const MODEL_KEYS_SECRET: &str = "MODEL_KEYS_SECRET";

/// The shortest secret [`derive_key`] will hash into an encryption key.
///
/// A bare SHA-256 stretches nothing, so the secret's own entropy is the
/// whole of it: below this the key is brute-forceable from one stored
/// ciphertext. `validate_config` refuses a shorter one at build time.
pub(crate) const MINIMUM_SECRET_BYTES: usize = 32;

/// A provider API key that never shows its full value. `Debug` and `Display`
/// print only `…` followed by the last four characters; it is never
/// `Serialize`, so it cannot reach a JSON response by accident.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ApiKey(String);

impl ApiKey {
    /// Wraps a raw key string.
    pub(crate) fn new(key: String) -> Self {
        Self(key)
    }

    /// The last four characters of the key, for display only.
    pub(crate) fn last4(&self) -> &str {
        let len = self.0.len();
        if len <= 4 {
            &self.0
        } else {
            &self.0[len - 4..]
        }
    }

    /// The raw key, for sending to the provider. Never logged, never
    /// serialized.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "…{}", self.last4())
    }
}

impl std::fmt::Display for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "…{}", self.last4())
    }
}

/// Deserializes straight from the request body's JSON string, so the
/// struct a handler takes is already `Debug`-safe: no request struct has
/// to opt out of `Debug` to avoid printing the key it carries.
impl<'de> Deserialize<'de> for ApiKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self::new)
    }
}

/// The ciphertext blob: a 12-byte nonce followed by the AES-256-GCM
/// ciphertext (plaintext length + 16-byte tag).
pub(crate) struct Ciphertext {
    pub(crate) blob: Vec<u8>,
}

/// Derives the AES-256 key from the config secret. Returns `None` when the
/// secret is absent or empty — the caller fails with a 500 rather than
/// storing plaintext.
pub(crate) fn derive_key(config: &dyn Config) -> Option<[u8; 32]> {
    let secret = config.get(MODEL_KEYS_SECRET)?;
    if secret.is_empty() {
        return None;
    }
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    let result = hasher.finalize();
    let mut key = [0u8; 32];
    key.copy_from_slice(&result);
    Some(key)
}

/// Encrypts `plaintext` with `key`, binding the ciphertext to
/// `(workspace_id, role)` as AAD. The nonce is 12 bytes from the `IdGen`
/// port; the blob is `nonce || ciphertext`.
pub(crate) fn encrypt(
    key: &[u8; 32],
    plaintext: &[u8],
    workspace_id: &str,
    role: &str,
    id_gen: &dyn IdGen,
) -> Result<Ciphertext, String> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let nonce_bytes = nonce(id_gen)?;
    let nonce = Nonce::from_slice(&nonce_bytes);
    let aad = aad(workspace_id, role);
    let ciphertext = cipher
        .encrypt(
            nonce,
            aes_gcm::aead::Payload {
                msg: plaintext,
                aad: &aad,
            },
        )
        .map_err(|e| e.to_string())?;
    let mut blob = nonce_bytes.to_vec();
    blob.extend_from_slice(&ciphertext);
    Ok(Ciphertext { blob })
}

/// Decrypts a blob produced by [`encrypt`], using the same
/// `(workspace_id, role)` AAD. Fails if the AAD does not match.
///
/// Test-only for now: nothing in this module calls a model yet, so there is
/// no code path that spends the key. It stays because the round trip and
/// the AAD binding are the two claims about the ciphertext worth keeping
/// true, and a round trip needs both halves.
#[cfg(test)]
pub(crate) fn decrypt(
    key: &[u8; 32],
    blob: &[u8],
    workspace_id: &str,
    role: &str,
) -> Result<Vec<u8>, String> {
    if blob.len() < 12 {
        return Err("ciphertext too short".to_owned());
    }
    let (nonce_bytes, ciphertext) = blob.split_at(12);
    let nonce = Nonce::from_slice(nonce_bytes);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let aad = aad(workspace_id, role);
    cipher
        .decrypt(
            nonce,
            aes_gcm::aead::Payload {
                msg: ciphertext,
                aad: &aad,
            },
        )
        .map_err(|e| e.to_string())
}

/// Generates a 12-byte nonce from the `IdGen` port: the 80 random bits of
/// one ULID and the first 16 random bits of a second.
///
/// The port mints identifiers, not bytes, so the random half has to be
/// decoded out of the Crockford base32 text — see [`random_half`]. Two
/// ULIDs give 96 bits; one gives 80, and the extra 16 come from a second
/// rather than from a wider window on the first.
fn nonce(id_gen: &dyn IdGen) -> Result<[u8; 12], String> {
    let first = random_half(&id_gen.ulid())?;
    let second = random_half(&id_gen.ulid())?;
    let mut bytes = [0u8; 12];
    bytes[..10].copy_from_slice(&first);
    bytes[10..].copy_from_slice(&second[..2]);
    Ok(bytes)
}

/// Crockford base32, as `ulid`'s `to_string` writes it: no `I`, `L`, `O`
/// or `U`.
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// The 80 random bits of a 26-character ULID, big-endian, as ten bytes.
///
/// `None` for anything that is not a well-formed ULID — which is a port
/// that does not do what [`IdGen`] says, and an encryption nonce built
/// out of a guess would be worse than a refusal.
fn random_half(ulid: &str) -> Result<[u8; 10], String> {
    let chars = ulid.as_bytes();
    if chars.len() != 26 {
        return Err("the id generator did not mint a 26-character ULID".to_owned());
    }
    let mut out = [0u8; 10];
    let mut accumulator: u16 = 0;
    let mut held = 0_u32;
    let mut written = 0;
    // Skip the first ten characters: that is the timestamp.
    for &character in &chars[10..] {
        let value = CROCKFORD
            .iter()
            .position(|&known| known == character)
            .ok_or_else(|| {
                "the id generator minted a character outside Crockford base32".to_owned()
            })? as u16;
        accumulator = (accumulator << 5) | value;
        held += 5;
        if held >= 8 {
            held -= 8;
            out[written] = (accumulator >> held) as u8;
            written += 1;
        }
    }
    Ok(out)
}

/// The AAD that binds a ciphertext to its row: `workspace_id:role`.
fn aad(workspace_id: &str, role: &str) -> Vec<u8> {
    format!("{workspace_id}:{role}").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cratefield_core::{MapConfig, UlidIdGen};

    fn test_key() -> [u8; 32] {
        let mut key = [0u8; 32];
        key.copy_from_slice(b"test-secret-0123456789abcdef-012");
        key
    }

    #[test]
    fn api_key_debug_shows_only_last_four() {
        let key = ApiKey::new("sk-abcdef1234".to_owned());
        assert_eq!(format!("{key:?}"), "…1234");
        assert_eq!(format!("{key}"), "…1234");
    }

    #[test]
    fn api_key_short_key_shows_all() {
        let key = ApiKey::new("abc".to_owned());
        assert_eq!(format!("{key}"), "…abc");
    }

    /// The request body a handler takes must be `Debug`-safe by
    /// construction, not by a module remembering not to derive it.
    #[test]
    fn api_key_deserializes_and_stays_redacted() {
        #[derive(Debug, Deserialize)]
        struct Body {
            api_key: ApiKey,
        }
        let body: Body = serde_json::from_str(r#"{"api_key":"sk-secret-abcd"}"#).unwrap();
        assert_eq!(format!("{body:?}"), "Body { api_key: …abcd }");
        assert_eq!(body.api_key.as_str(), "sk-secret-abcd");
    }

    /// The nonce must not be the ULID's leading characters: those are the
    /// millisecond timestamp, so a nonce built from them repeats for every
    /// connect inside one millisecond and carries no entropy at all.
    #[test]
    fn the_nonce_is_the_random_half_and_does_not_repeat() {
        let key = test_key();
        let id_gen = UlidIdGen;
        let mut seen = std::collections::HashSet::new();
        for _ in 0..64 {
            let ulid = id_gen.ulid();
            let ciphertext = encrypt(&key, b"a-key", "ws-1", "main", &id_gen).unwrap();
            let nonce = &ciphertext.blob[..12];
            assert_ne!(
                nonce,
                &ulid.as_bytes()[..12],
                "the nonce is the ULID's timestamp prefix"
            );
            assert!(seen.insert(nonce.to_vec()), "a nonce repeated");
        }
    }

    #[test]
    fn a_malformed_id_is_refused_rather_than_encoded() {
        assert!(random_half("too-short").is_err());
        assert!(random_half("0123456789ABCDEFGHJKMNPQRU").is_err()); // 26 chars, 'U'
    }

    #[test]
    fn encrypt_decrypt_round_trips() {
        let key = test_key();
        let id_gen = UlidIdGen;
        let plaintext = b"sk-provider-key-12345";
        let ct = encrypt(&key, plaintext, "ws-1", "main", &id_gen).unwrap();
        let pt = decrypt(&key, &ct.blob, "ws-1", "main").unwrap();
        assert_eq!(pt, plaintext);
    }

    #[test]
    fn decrypt_fails_with_wrong_aad() {
        let key = test_key();
        let id_gen = UlidIdGen;
        let plaintext = b"sk-provider-key-12345";
        let ct = encrypt(&key, plaintext, "ws-1", "main", &id_gen).unwrap();
        assert!(decrypt(&key, &ct.blob, "ws-1", "triage").is_err());
        assert!(decrypt(&key, &ct.blob, "ws-2", "main").is_err());
    }

    #[test]
    fn derive_key_returns_none_when_missing() {
        let config = MapConfig::default();
        assert!(derive_key(&config).is_none());
    }

    #[test]
    fn derive_key_returns_some_when_set() {
        let config = MapConfig::from_pairs([("MODEL_KEYS_SECRET", "a-secret")]);
        assert!(derive_key(&config).is_some());
    }

    #[test]
    fn derive_key_returns_none_when_empty() {
        let config = MapConfig::from_pairs([("MODEL_KEYS_SECRET", "")]);
        assert!(derive_key(&config).is_none());
    }
}
