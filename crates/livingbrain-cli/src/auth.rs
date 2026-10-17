//! Tokens: where they come from, where they go, and how `login` gets one.
//!
//! A token lives in exactly two places: the `LIVINGBRAIN_TOKEN` environment
//! variable (for that one invocation, never persisted), or the OS keychain.
//! It is never written to a dotfile, a log line or stdout; [`Token`] redacts
//! itself in `Debug` so an accident cannot leak it.
//!
//! The Mac app runs this binary as a child process and hands it the tokens it
//! already holds, in the environment, with `LIVINGBRAIN_NO_KEYCHAIN=1` set.
//! In that mode nothing here touches the keychain: a token the environment
//! does not carry is simply missing. Environment variables are visible only to
//! the same user's processes, and [`scrub_token_vars`] keeps them away from
//! every other child this binary starts (git, the agent CLIs).

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::api::{Client, Poll};
use crate::{CliError, CliResult, Out, err};

/// The keychain service name; the user is the API base URL.
const SERVICE: &str = "wiki.livingbrain.cli";

/// The Mac app's keychain service (`KeychainTokenStore.appService` in
/// macos/LivingBrainBar). It keeps one token per workspace under the account
/// `<api base URL>#<workspace id>` (`WorkspaceDirectory`), and routed
/// `logs sync` uploads read those same items.
pub const APP_SERVICE: &str = "wiki.livingbrain.mac";

/// Set to `1` by the Mac app on every run it starts: the keychain is not read
/// or written, and tokens come only from the environment.
pub const NO_KEYCHAIN_ENV: &str = "LIVINGBRAIN_NO_KEYCHAIN";

/// The longest we will wait for device approval, whatever `expires_in` claims.
const MAX_WAIT: Duration = Duration::from_secs(24 * 60 * 60);

/// Where environment variables come from: the process in real use, a map in
/// tests, so the token rules can be exercised without touching the real
/// environment.
pub trait EnvSource {
    fn var_os(&self, key: &str) -> Option<OsString>;
}

/// The process environment.
pub struct ProcessEnv;

impl EnvSource for ProcessEnv {
    fn var_os(&self, key: &str) -> Option<OsString> {
        std::env::var_os(key)
    }
}

/// Whether this run may use the keychain at all.
fn keychain_allowed(env: &dyn EnvSource) -> bool {
    env.var_os(NO_KEYCHAIN_ENV).as_deref() != Some(OsStr::new("1"))
}

/// Refuse a keychain access when this run has turned the keychain off. Every
/// read, write and delete of an item goes through here.
pub fn keychain_gate(env: &dyn EnvSource) -> CliResult<()> {
    if keychain_allowed(env) {
        Ok(())
    } else {
        Err(err(format!(
            "the OS keychain is off for this run ({NO_KEYCHAIN_ENV}=1)"
        )))
    }
}

/// [`keychain_gate`] for the process environment.
pub fn keychain_gate_process() -> CliResult<()> {
    keychain_gate(&ProcessEnv)
}

/// Whether an environment variable name carries a token: `LIVINGBRAIN_TOKEN`
/// itself, or one of the per-workspace `LIVINGBRAIN_TOKEN_<ID>` variables.
pub fn is_token_var(name: &str) -> bool {
    name == "LIVINGBRAIN_TOKEN" || name.starts_with("LIVINGBRAIN_TOKEN_")
}

/// Remove every token variable among `names` from a child's environment, so
/// the child never inherits one. Used for every child this binary starts that
/// has no business with a LivingBrain token.
pub fn scrub_token_vars(command: &mut Command, names: impl IntoIterator<Item = OsString>) {
    for name in names {
        if name.to_str().is_some_and(is_token_var) {
            command.env_remove(name);
        }
    }
}

/// [`scrub_token_vars`] over the names in this process's environment, which is
/// what a child would otherwise inherit.
pub fn scrub_inherited_tokens(command: &mut Command) {
    scrub_token_vars(command, std::env::vars_os().map(|(key, _)| key));
}

/// A place a token can be kept: the OS keychain in real use, an in-memory
/// store in tests. No implementation ever writes a file.
pub trait TokenStore {
    fn set(&self, user: &str, token: &str) -> CliResult<()>;
    fn delete(&self, user: &str) -> CliResult<()>;
}

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

/// The OS keychain. Never a file.
pub struct KeyringStore;

impl KeyringStore {
    fn entry(user: &str) -> CliResult<keyring::Entry> {
        keychain_gate_process()?;
        keyring::Entry::new(SERVICE, user)
            .map_err(|e| err(format!("the OS keychain is unavailable: {e}")))
    }
}

/// Read one generic-password item by service and account. `None` when there is
/// no such item; a keychain that cannot be read is an error, never "no token".
fn keychain_get(env: &dyn EnvSource, service: &str, user: &str) -> CliResult<Option<String>> {
    keychain_gate(env)?;
    let entry = keyring::Entry::new(service, user)
        .map_err(|e| err(format!("the OS keychain is unavailable: {e}")))?;
    match entry.get_password() {
        Ok(token) => Ok(Some(token)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(err(format!("could not read the OS keychain: {e}"))),
    }
}

impl TokenStore for KeyringStore {
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
    token_for_in(&ProcessEnv, api_url)
}

fn token_for_in(env: &dyn EnvSource, api_url: &str) -> CliResult<Token> {
    if let Some(token) = env.var_os("LIVINGBRAIN_TOKEN") {
        let token = token.to_string_lossy().into_owned();
        if !token.is_empty() {
            return Ok(Token(token));
        }
    }
    if !keychain_allowed(env) {
        return Err(CliError::NotLoggedIn(format!(
            "not logged in — no token was passed to this run (LIVINGBRAIN_TOKEN is unset and {NO_KEYCHAIN_ENV}=1)"
        )));
    }
    match keychain_get(env, SERVICE, &user_key(api_url))? {
        Some(token) => Ok(Token(token)),
        None => Err(CliError::NotLoggedIn(
            "not logged in — run `livingbrain login`".into(),
        )),
    }
}

/// The keychain account the Mac app keeps a workspace's token under.
pub fn workspace_account(api_url: &str, workspace: &str) -> String {
    format!("{}#{workspace}", user_key(api_url))
}

/// The environment variable that overrides one workspace's token, for tests
/// and CI: `LIVINGBRAIN_TOKEN_<WORKSPACE>`, the id upper-cased with every
/// character that is not a letter or digit made `_`.
pub fn workspace_env_var(workspace: &str) -> String {
    let suffix: String = workspace
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("LIVINGBRAIN_TOKEN_{suffix}")
}

/// The token for one workspace, for a session routed there: its
/// [`workspace_env_var`] first, else the Mac app's keychain item (not read when
/// `LIVINGBRAIN_NO_KEYCHAIN=1`). The opt-in file refuses two workspace ids that
/// share a variable name, so the variable always names this workspace alone.
/// `None` when neither holds one. The caller must not fall back to another
/// workspace's token: a routed session stays unsynced rather than land in the
/// wrong one.
pub fn workspace_token(api_url: &str, workspace: &str) -> CliResult<Option<Token>> {
    workspace_token_in(&ProcessEnv, api_url, workspace)
}

fn workspace_token_in(
    env: &dyn EnvSource,
    api_url: &str,
    workspace: &str,
) -> CliResult<Option<Token>> {
    let from_env = env
        .var_os(&workspace_env_var(workspace))
        .map(|token| token.to_string_lossy().into_owned())
        .filter(|token| !token.is_empty());
    if let Some(token) = from_env {
        return Ok(Some(Token(token)));
    }
    if !keychain_allowed(env) {
        return Ok(None);
    }
    Ok(keychain_get(env, APP_SERVICE, &workspace_account(api_url, workspace))?.map(Token))
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
    use std::any::Any;
    use std::collections::HashMap;
    use std::ffi::{OsStr, OsString};
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::{
        APP_SERVICE, EnvSource, KeyringStore, SERVICE, Token, TokenStore, is_token_var,
        keychain_get, scrub_token_vars, token_for_in, workspace_token_in,
    };
    use crate::CliError;

    const API: &str = "https://api.example";

    /// A fixed environment for a test: no value is read from the process.
    struct MapEnv(HashMap<String, OsString>);

    impl MapEnv {
        fn new(pairs: &[(&str, &str)]) -> Self {
            Self(
                pairs
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), OsString::from(v)))
                    .collect(),
            )
        }
    }

    impl EnvSource for MapEnv {
        fn var_os(&self, key: &str) -> Option<OsString> {
            self.0.get(key).cloned()
        }
    }

    /// Serialises the tests that install a keyring backend: the default
    /// builder is process-wide.
    static KEYRING_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Wraps keyring's in-memory mock and counts every credential it builds.
    /// Each `Entry::new` builds one, so a zero count means the keychain was
    /// never asked for anything.
    struct Counting {
        inner: Box<keyring::CredentialBuilder>,
        builds: Arc<AtomicUsize>,
    }

    impl keyring::credential::CredentialBuilderApi for Counting {
        fn build(
            &self,
            target: Option<&str>,
            service: &str,
            user: &str,
        ) -> keyring::Result<Box<keyring::Credential>> {
            self.builds.fetch_add(1, Ordering::SeqCst);
            self.inner.build(target, service, user)
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    fn install_counting_keyring() -> Arc<AtomicUsize> {
        let builds = Arc::new(AtomicUsize::new(0));
        keyring::set_default_credential_builder(Box::new(Counting {
            inner: keyring::mock::default_credential_builder(),
            builds: Arc::clone(&builds),
        }));
        builds
    }

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
        let _lock = KEYRING_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
        let store = KeyringStore;
        assert_eq!(
            keychain_get(&MapEnv::new(&[]), SERVICE, "https://api.example").expect("read"),
            None
        );
        store.set("https://api.example", "t").expect("write");
        store.delete("https://api.example").expect("delete");
        store.delete("https://api.example").expect("delete again");
    }

    /// `LIVINGBRAIN_NO_KEYCHAIN=1` means the keyring is never called: not for
    /// the default token, not for a workspace's, not for a read. The control
    /// half proves the counter does count when the keychain is allowed.
    #[test]
    fn no_keychain_mode_never_calls_the_keyring() {
        let _lock = KEYRING_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let builds = install_counting_keyring();
        let off = MapEnv::new(&[("LIVINGBRAIN_NO_KEYCHAIN", "1")]);

        assert!(matches!(
            token_for_in(&off, API),
            Err(CliError::NotLoggedIn(_))
        ));
        assert!(
            workspace_token_in(&off, API, "ws-a")
                .expect("no error")
                .is_none()
        );
        assert!(keychain_get(&off, APP_SERVICE, "x").is_err());
        assert!(super::keychain_gate(&off).is_err());
        assert_eq!(builds.load(Ordering::SeqCst), 0, "the keyring was called");

        // Control: with the keychain allowed the same lookup does reach it.
        let on = MapEnv::new(&[]);
        assert_eq!(keychain_get(&on, APP_SERVICE, "x").expect("read"), None);
        assert!(builds.load(Ordering::SeqCst) > 0);
    }

    /// The handed-over tokens are used with the keychain off, and the default
    /// token never stands in for a workspace's.
    #[test]
    fn no_keychain_mode_uses_only_the_environment() {
        let env = MapEnv::new(&[
            ("LIVINGBRAIN_NO_KEYCHAIN", "1"),
            ("LIVINGBRAIN_TOKEN_WS_CHI", "ws-chi-token"),
        ]);
        let routed = workspace_token_in(&env, API, "ws-chi")
            .expect("ok")
            .expect("token");
        assert_eq!(routed.expose(), "ws-chi-token");
        // A workspace without a handed token is missing, not the default's.
        assert!(
            workspace_token_in(&env, API, "ws-other")
                .expect("ok")
                .is_none()
        );

        let with_default = MapEnv::new(&[
            ("LIVINGBRAIN_NO_KEYCHAIN", "1"),
            ("LIVINGBRAIN_TOKEN", "default-token"),
        ]);
        assert_eq!(
            token_for_in(&with_default, API).expect("token").expose(),
            "default-token"
        );
        assert!(
            workspace_token_in(&with_default, API, "ws-chi")
                .expect("ok")
                .is_none()
        );
    }

    /// A flag other than exactly `1` does not switch the keychain off.
    #[test]
    fn only_the_value_one_turns_the_keychain_off() {
        for value in ["0", "", "true", "yes"] {
            let env = MapEnv::new(&[("LIVINGBRAIN_NO_KEYCHAIN", value)]);
            assert!(super::keychain_gate(&env).is_ok(), "{value:?}");
        }
    }

    #[test]
    fn token_variables_are_recognised_by_name() {
        assert!(is_token_var("LIVINGBRAIN_TOKEN"));
        assert!(is_token_var("LIVINGBRAIN_TOKEN_WS_CHI"));
        assert!(!is_token_var("LIVINGBRAIN_TOKENS"));
        assert!(!is_token_var("LIVINGBRAIN_NO_KEYCHAIN"));
        assert!(!is_token_var("HOME"));
    }

    /// A child started with the scrub has no token variable in its explicit
    /// environment changes, and nothing else is touched.
    #[test]
    fn scrubbing_removes_every_token_variable_from_a_child() {
        let mut command = Command::new("true");
        let names = [
            "LIVINGBRAIN_TOKEN",
            "LIVINGBRAIN_TOKEN_WS_CHI",
            "LIVINGBRAIN_API_URL",
            "HOME",
        ]
        .map(OsString::from);
        scrub_token_vars(&mut command, names);
        let removed: Vec<&OsStr> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key)
            .collect();
        assert!(removed.contains(&OsStr::new("LIVINGBRAIN_TOKEN")));
        assert!(removed.contains(&OsStr::new("LIVINGBRAIN_TOKEN_WS_CHI")));
        assert!(!removed.contains(&OsStr::new("LIVINGBRAIN_API_URL")));
        assert!(!removed.contains(&OsStr::new("HOME")));
    }
}
