//! Tokens: where they come from, where they go, and how `login` gets one.
//!
//! A token lives in exactly two places: the `LIVINGBRAIN_TOKEN` environment
//! variable (for that one invocation, never persisted), or the OS keychain.
//! It is never written to a dotfile, a log line or stdout; [`Token`] redacts
//! itself in `Debug` so an accident cannot leak it.

use std::fmt;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::api::{Client, Poll};
use crate::{CliError, CliResult, Out, err};

/// The keychain service name; the user is the API base URL.
const SERVICE: &str = "wiki.livingbrain.cli";

/// The longest we will wait for device approval, whatever `expires_in` claims.
const MAX_WAIT: Duration = Duration::from_secs(24 * 60 * 60);

/// A bearer token. `Debug` never reveals it.
#[derive(Clone)]
pub struct Token(String);

impl Token {
    /// The raw token, for the `Authorization` header only.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

/// A place a token can be kept: the OS keychain in real use, an in-memory
/// store in tests. No implementation ever writes a file.
pub trait TokenStore {
    fn get(&self, user: &str) -> CliResult<Option<String>>;
    fn set(&self, user: &str, token: &str) -> CliResult<()>;
    fn delete(&self, user: &str) -> CliResult<()>;
}

/// The OS keychain. Never a file.
pub struct KeyringStore;

impl KeyringStore {
    fn entry(user: &str) -> CliResult<keyring::Entry> {
        keyring::Entry::new(SERVICE, user)
            .map_err(|e| err(format!("the OS keychain is unavailable: {e}")))
    }
}

impl TokenStore for KeyringStore {
    fn get(&self, user: &str) -> CliResult<Option<String>> {
        match Self::entry(user)?.get_password() {
            Ok(token) => Ok(Some(token)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(err(format!("could not read the OS keychain: {e}"))),
        }
    }

    fn set(&self, user: &str, token: &str) -> CliResult<()> {
        Self::entry(user)?
            .set_password(token)
            .map_err(|e| err(format!("could not write the OS keychain: {e}")))
    }

    fn delete(&self, user: &str) -> CliResult<()> {
        match Self::entry(user)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(err(format!("could not update the OS keychain: {e}"))),
        }
    }
}

/// Install keyring's in-memory mock as the backend when the test-only hook is
/// set. The hook is honoured only in a debug build, so a release binary always
/// uses the real OS keychain — and the mock never touches a file.
pub fn init_backend() {
    #[cfg(debug_assertions)]
    if std::env::var("LIVINGBRAIN_TEST_KEYRING").as_deref() == Ok("mock") {
        keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
    }
}

/// The keychain user for an API base URL.
fn user_key(api_url: &str) -> String {
    api_url.trim_end_matches('/').to_owned()
}

/// The token for the normal commands: the environment first, then the keychain
/// ([`CliError::NotLoggedIn`] when neither has one).
pub fn token_for(api_url: &str) -> CliResult<Token> {
    if let Some(token) = std::env::var_os("LIVINGBRAIN_TOKEN") {
        let token = token.to_string_lossy().into_owned();
        if !token.is_empty() {
            return Ok(Token(token));
        }
    }
    match KeyringStore.get(&user_key(api_url))? {
        Some(token) => Ok(Token(token)),
        None => Err(CliError::NotLoggedIn(
            "not logged in — run `livingbrain login`".into(),
        )),
    }
}

/// Seconds between token polls: the server's `interval`, never below the RFC
/// 8628 floor of five. A debug-only override lets tests poll without sleeping;
/// it does not exist in a release build.
fn poll_interval(server: Option<u64>) -> u64 {
    #[cfg(debug_assertions)]
    if let Ok(secs) = std::env::var("LIVINGBRAIN_TEST_POLL_INTERVAL") {
        return secs.parse().unwrap_or(0);
    }
    server.unwrap_or(5).max(5)
}

/// `livingbrain login`: the RFC 8628 device flow, then the keychain.
pub fn login(api_url: &str, out: &Out) -> CliResult<()> {
    let client = Client::new(api_url);
    let authorization = client.device_authorize()?;
    let device_code = authorization
        .get("device_code")
        .and_then(Value::as_str)
        .ok_or_else(|| err("the server's device response had no device_code"))?
        .to_owned();
    // `user_code` and `verification_uri` are required by RFC 8628 §3.2.
    let user_code = authorization
        .get("user_code")
        .and_then(Value::as_str)
        .ok_or_else(|| err("the server's device response had no user_code"))?
        .to_owned();
    let verification_uri = authorization
        .get("verification_uri")
        .and_then(Value::as_str)
        .ok_or_else(|| err("the server's device response had no verification_uri"))?
        .to_owned();
    let complete = authorization
        .get("verification_uri_complete")
        .and_then(Value::as_str);
    let mut interval = poll_interval(authorization.get("interval").and_then(Value::as_u64));
    let expires_in = authorization
        .get("expires_in")
        .and_then(Value::as_u64)
        .unwrap_or(600);

    // The prompt goes to stderr, so `--json` keeps stdout to the one result.
    if out.json {
        eprintln!(
            "{}",
            json!({
                "user_code": user_code,
                "verification_uri": verification_uri,
                "verification_uri_complete": complete,
            })
        );
    } else {
        eprintln!(
            "Open {} and enter the code: {user_code}",
            complete.unwrap_or(&verification_uri)
        );
    }

    // Cap an absurd `expires_in` and saturate the add: no panic, no overflow.
    let wait = Duration::from_secs(expires_in).min(MAX_WAIT);
    let deadline = Instant::now()
        .checked_add(wait)
        .unwrap_or_else(|| Instant::now() + wait);
    let token = loop {
        if Instant::now() >= deadline {
            return Err(err("the device code expired before it was approved"));
        }
        // Check the deadline first, and never sleep past it: `min` caps the last
        // wait to the time remaining.
        let remaining = deadline.saturating_duration_since(Instant::now());
        let sleep = Duration::from_secs(interval).min(remaining);
        if !sleep.is_zero() {
            std::thread::sleep(sleep);
        }
        match client.device_token(&device_code)? {
            Poll::Issued(token) => break token,
            Poll::Pending => {}
            Poll::SlowDown => interval += 5,
        }
    };

    KeyringStore.set(&user_key(api_url), &token)?;
    out.json_or(&json!({ "logged_in": true }), || println!("Logged in."));
    Ok(())
}

/// `livingbrain logout`: forget the keychain token.
pub fn logout(api_url: &str, out: &Out) -> CliResult<()> {
    KeyringStore.delete(&user_key(api_url))?;
    out.json_or(&json!({ "logged_out": true }), || println!("Logged out."));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{KeyringStore, Token, TokenStore};

    #[test]
    fn a_token_never_appears_in_debug() {
        let token = Token("secret-token-value".into());
        assert_eq!(format!("{token:?}"), "Token(<redacted>)");
    }

    /// keyring's mock keeps its state inside each `Entry`, not in a shared
    /// store, so a fresh `get` is the "missing" path. That is the mapping worth
    /// testing: `NoEntry` becomes `None`, and deleting is idempotent.
    #[test]
    fn the_keychain_store_maps_a_missing_entry_to_none() {
        keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
        let store = KeyringStore;
        assert_eq!(store.get("https://api.example").expect("read"), None);
        store.set("https://api.example", "t").expect("write");
        store.delete("https://api.example").expect("delete");
        store.delete("https://api.example").expect("delete again");
    }
}
