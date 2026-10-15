//! `livingbrain telemetry on|off|status`: the switch, and the local view.
//!
//! # What is stored, and where
//!
//! Two values, `telemetry.usage` and `telemetry.live-map`, both in the OS
//! keychain under the same service name `auth` uses. The keychain rather than
//! a file, and the reason is not that these values are secret — they are not,
//! they are two words — but that they are nothing a reader would ever edit or
//! back up, and the keychain store is wiped with the token, so deauthing
//! leaves no preference behind. The CLI does keep one file, the `logs`
//! opt-in (`logs-allow.json`, see `logs.rs`) — a list of paths that exists to
//! be inspected and hand-edited — but a two-word on/off preference is not
//! that, and a dotfile here would be a second thing to remember to wipe. The
//! keychain gives us a per-user, per-machine store that honours the same
//! `LIVINGBRAIN_TEST_KEYRING=mock` test seam, and — because
//! `auth::init_backend` installs the mock builder before any command runs —
//! needs no new test hook at all.
//!
//! # Where the send happens
//!
//! **Nowhere in this file.** `on`, `off` and `status` are the switch and the
//! view; the transport belongs to whoever sends. A batch goes to
//! `USAGE_ENDPOINT` and a beat to `LIVE_MAP_ENDPOINT`, and both go through
//! the `ureq` client in `api.rs`, at the end of an on-period, after
//! `Telemetry::resolve(stored, env).is_on()` has been checked. This PR wires
//! the switch and the local view only, so there is no client, no socket and no
//! request here — which `tests/telemetry.rs` proves at the TCP level.
//!
//! # The ids
//!
//! `status` shows a batch, and a batch needs a `UsageId`. Nothing in this file
//! sends it, so it is minted fresh for the display and never stored; see
//! [`random_id`] for where the randomness comes from and why that is enough.

use std::time::{SystemTime, UNIX_EPOCH};

use clap::Subcommand;
use serde_json::{Value, json};

use livingbrain_telemetry::{
    LIVE_MAP_ENDPOINT, LiveMap, Platform, Telemetry, USAGE_ENDPOINT, UsageBatch, UsageId, Version,
};

use crate::{CliResult, Out, err};

/// The keychain service: the same one `auth` writes tokens under, so every
/// value this CLI persists is one thing to look at and one thing to wipe.
/// Repeated here rather than imported because `auth::SERVICE` is private to
/// that module and this one is a constant, not state.
const SERVICE: &str = "wiki.livingbrain.cli";

/// The stored anonymous-usage preference. Machine-wide, not per-API-URL: the
/// question "may this install send counts" does not change when you point the
/// CLI at a different server.
const USAGE_KEY: &str = "telemetry.usage";

/// The stored live-map preference. Off until someone switches it on.
///
/// Read-only from this command. Issue #44 asks `status` to show the live-map
/// setting, and the write side of that opt-in belongs to whoever mints the
/// install id and sends the beat: an id created here would have to be kept,
/// and a kept id is a kept position on a map. `on`/`off` deliberately do not
/// write this key, so there is no flag surface here that could imply one.
const LIVE_MAP_KEY: &str = "telemetry.live-map";

/// Anonymous usage with nothing stored: on. This is [`Telemetry::default`] —
/// the crate's promise that Living Brain sends counted usage out of the box —
/// asserted in the test below, which fails if the default ever changes without
/// this line changing with it.
const USAGE_DEFAULT: Telemetry = Telemetry::On;

/// The live map with nothing stored: off, because it is opt-in and the crate
/// guarantees no id exists before the opt-in.
const LIVE_MAP_DEFAULT: Telemetry = Telemetry::Off;

/// The environment variable that beats the stored preference for one run.
const ENV_VAR: &str = "LIVINGBRAIN_TELEMETRY";

/// Seconds in a day, for rounding the period end up to the next UTC midnight.
const DAY: u64 = 24 * 60 * 60;

/// `livingbrain telemetry on|off|status`.
#[derive(Subcommand, Clone, Copy, Debug)]
pub enum Action {
    /// Turn anonymous usage data on
    On,
    /// Turn anonymous usage data off
    Off,
    /// Show what telemetry would send, and where it would go
    Status,
}

/// The environment, as this command reads it.
///
/// Passed in rather than read from the process at each use so that every test
/// of the resolution rule names the value it means. `std::env::set_var` is
/// `unsafe` in edition 2024 and `unsafe_code` is forbidden workspace-wide, so
/// an injectable value is the only way to test the environment override
/// honestly rather than by mutating global process state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Env {
    /// `LIVINGBRAIN_TELEMETRY`, verbatim: not trimmed and not lowercased,
    /// because [`Telemetry::resolve`] is where that happens and duplicating
    /// it here would be a second definition of the rule.
    pub telemetry: Option<String>,
}

impl Env {
    /// The environment this process was started with.
    pub fn from_process() -> Self {
        Self {
            telemetry: std::env::var(ENV_VAR).ok(),
        }
    }
}

/// A place the two stored preferences live.
///
/// A trait so the resolution and rendering rules can be tested against a
/// store that remembers; the OS keychain does not, under
/// `LIVINGBRAIN_TEST_KEYRING=mock`, because keyring's mock keeps the value
/// inside the `Entry` it was written to and every `Entry::new` starts empty.
/// That is a property of the mock, not of the command.
pub trait Preference {
    fn get(&self, name: &str) -> CliResult<Option<String>>;
    fn set(&self, name: &str, value: &str) -> CliResult<()>;
}

/// The OS keychain. Never a file, for the reason in the module docs.
pub struct KeychainPreference;

impl Preference for KeychainPreference {
    fn get(&self, name: &str) -> CliResult<Option<String>> {
        let entry = keyring::Entry::new(SERVICE, name)
            .map_err(|e| err(format!("the OS keychain is unavailable: {e}")))?;
        match entry.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(err(format!("could not read the OS keychain: {e}"))),
        }
    }

    fn set(&self, name: &str, value: &str) -> CliResult<()> {
        keyring::Entry::new(SERVICE, name)
            .map_err(|e| err(format!("the OS keychain is unavailable: {e}")))?
            .set_password(value)
            .map_err(|e| err(format!("could not write the OS keychain: {e}")))
    }
}

/// `livingbrain telemetry …`: the whole command.
///
/// `store` and `env` are parameters so the rule under test — the environment
/// beats the stored preference, a typo beats neither — can be driven from a
/// test without touching the keychain or the process environment.
pub fn run(action: Action, out: &Out, store: &dyn Preference, env: &Env) -> CliResult<()> {
    match action {
        Action::On => set(Telemetry::On, out, store, env),
        Action::Off => set(Telemetry::Off, out, store, env),
        Action::Status => status(out, store, env),
    }
}

/// `on` / `off`: store the preference, then report what it resolves to.
///
/// Reporting the *resolved* value rather than the stored one is the whole
/// point: a run with `LIVINGBRAIN_TELEMETRY=0` that ends with `telemetry on`
/// must not look like it worked.
fn set(want: Telemetry, out: &Out, store: &dyn Preference, env: &Env) -> CliResult<()> {
    // The write is the point of `on`/`off`, and its error already propagated
    // before this read existed — unchanged. If it succeeded the keychain is
    // answering, so the live-map read below failing means the backend has gone
    // strange mid-command; that is reported rather than smoothed over, and the
    // write itself did land.
    store.set(USAGE_KEY, on_off(want))?;
    let resolved = Telemetry::resolve(want, env.telemetry.as_deref());
    let map = live_map_stored(store)?;
    let value = json!({
        "usage": {
            "stored": on_off(want),
            "resolved": on_off(resolved),
            "env": env.telemetry,
            "env_var": ENV_VAR,
            "endpoint": USAGE_ENDPOINT,
        },
        "live_map": live_map_value(map)?,
    });
    out.json_or(&value, || render_set(want, resolved, env));
    Ok(())
}

/// `status`: the resolved state, and the next batch, verbatim.
fn status(out: &Out, store: &dyn Preference, env: &Env) -> CliResult<()> {
    let stored = read(store, USAGE_KEY, USAGE_DEFAULT)?;
    // Read once and passed down, where it used to be read here and again inside
    // `render_status`. Same answer, one keychain round-trip, and one place a
    // read error can come from rather than two.
    let map = live_map_stored(store)?;
    let resolved = Telemetry::resolve(stored, env.telemetry.as_deref());
    let batch = next_batch();
    let value = json!({
        "usage": {
            "stored": on_off(stored),
            "resolved": on_off(resolved),
            "env": env.telemetry,
            "env_var": ENV_VAR,
            "endpoint": USAGE_ENDPOINT,
            // Every key the batch carries, serialised by the same `Serialize`
            // impl the sender would use — not a rendering of one.
            "next_batch": serde_json::to_value(&batch).unwrap_or(Value::Null),
        },
        "live_map": live_map_value(map)?,
    });
    out.json_or(&value, || render_status(map, stored, resolved, env, &batch));
    Ok(())
}

/// The stored live-map preference, on its own, so [`status`] and [`set`] each
/// read it once.
fn live_map_stored(store: &dyn Preference) -> CliResult<Telemetry> {
    read(store, LIVE_MAP_KEY, LIVE_MAP_DEFAULT)
}

/// The live-map half of the answer: what is stored, whether a beat would be
/// produced, and where it would go.
///
/// The beat itself is not shown, and cannot be: a beat carries a page count
/// and a `LiveMap` only produces one once `enable` has been called with an id
/// this command has no way to mint — the id belongs to whoever sends, and
/// keeping one here for display would be keeping a position on a map.
///
/// What is shown instead is the guarantee that matters, which is that with no
/// id there is no beat. That is [`LiveMap::heartbeat`] returning `None`, and
/// it is asked for here rather than assumed, so the answer is the crate's.
fn live_map_value(stored: Telemetry) -> CliResult<Value> {
    // Always `disabled()`: this command holds no install id, so a stored `On`
    // is somebody else's opt-in and not a beat this process could produce.
    let beat = LiveMap::disabled().heartbeat(version()?, platform(), 0);
    Ok(json!({
        "stored": on_off(stored),
        // `false` while there is no id: the opt-in guarantee in one value.
        "sends_heartbeat": beat.is_some(),
        "endpoint": LIVE_MAP_ENDPOINT,
    }))
}

/// The stored preference for `name`, or `default` when nothing is stored.
///
/// `default` is a parameter rather than [`Telemetry::default`] because the two
/// switches do not share one: anonymous usage is on by default
/// ([`Telemetry::default`] is [`Telemetry::On`], and that is the crate's
/// promise), while the live map is off until someone opts in. Reading both
/// through one default would print `live map: on` on a machine that has never
/// sent a heartbeat, which is the exact thing the live map exists to prevent.
///
/// A stored value this file does not recognise is treated as absent, for the
/// same reason a typo in the environment is: an unreadable word in the
/// keychain must not be a way to turn telemetry off by accident, or on.
///
/// # Why a keychain error is an error and not the default
///
/// This used to be `store.get(name).ok().flatten()`, which threw the error
/// away and fell through to `default` — and for [`USAGE_KEY`] that default is
/// **on**. So a locked keyring, a machine with no secret service, or a backend
/// the platform does not have were all silently reported as "anonymous usage
/// data: on": a store this CLI could not read was indistinguishable from an
/// empty one, and the answer it gave in that case was the one that collects.
///
/// Neither invented answer is acceptable. Reporting `off` would be a lie in
/// the other direction — it is the answer a self-hoster ran this command to
/// check, and telling them it is off when the keychain merely would not answer
/// is worse than the bug. So the error propagates and `main` prints it, plain
/// as `error: …` and under `--json` as `{"error": …}`, exiting 1.
///
/// That is also the shape that leaves the reader a way forward: the message
/// names `LIVINGBRAIN_TELEMETRY`, which still decides the current run without
/// ever touching the keychain, so a broken keychain costs a reader their
/// stored preference, not their ability to turn telemetry off right now.
///
/// [`set`] already propagated its error and never reached this function, so
/// nothing about the write side changes.
fn read(store: &dyn Preference, name: &str, default: Telemetry) -> CliResult<Telemetry> {
    let stored = store.get(name).map_err(|e| {
        err(format!(
            "could not read the stored preference \"{name}\" from the OS keychain, \
             so it is unknown whether telemetry is on: {e}. \
             {ENV_VAR} still decides this run — `export {ENV_VAR}=0` turns it off \
             without reading the keychain."
        ))
    })?;
    Ok(match stored.as_deref() {
        Some("on") => Telemetry::On,
        Some("off") => Telemetry::Off,
        _ => default,
    })
}

/// The wire word for a preference: the same two words
/// [`Telemetry::resolve`] understands, so a stored value and an environment
/// value are the same kind of thing.
fn on_off(value: Telemetry) -> &'static str {
    if value.is_on() { "on" } else { "off" }
}

/// This build's own version, as the batch will carry it.
///
/// A release is what this reads, so it must satisfy `Version`'s grammar. If a
/// version like `0.1.0-3-gabc1234-dirty` or a `v0.1.0` tag were ever put into
/// `Cargo.toml`, this is the line that refuses it — and the error says so
/// rather than quietly sending nothing, because a batch that quietly lost its
/// version would be a batch whose dashboards could not say which build it came
/// from.
fn version() -> CliResult<Version> {
    Version::new(env!("CARGO_PKG_VERSION"))
        .map_err(|e| err(format!("this build's version is not a version: {e}")))
}

/// The host this build is running on.
///
/// Read from `std::env::consts`, not from `cfg!(target_os)`: the batch reports
/// the machine it was counted on, and on a native binary those are the same
/// answer reached by the one route that also works for a future cross-compiled
/// one. Anything not named goes to `Other`, which is a real bucket and not a
/// silent drop.
fn platform() -> Platform {
    match std::env::consts::OS {
        "linux" => Platform::Linux,
        "macos" => Platform::Macos,
        "windows" => Platform::Windows,
        _ => Platform::Other,
    }
}

/// Seconds since the epoch, or zero if the clock is before it (which would be a
/// broken clock rather than a reason to fail a read-only command).
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The next UTC midnight, as the batch's `period_end_unix`.
///
/// A daily period, so a reader who runs `status` twice sees the same boundary
/// rather than a clock-derived number that moves under them.
fn next_period_end() -> u64 {
    (now_unix() / DAY + 1) * DAY
}

/// The batch `status` shows: a real one, with every map empty.
///
/// Empty on purpose. This command does not count anything — it has no brain
/// turn, MCP call or search to count — and inventing a plausible `3` for
/// `counters.pages` would be a self-hoster looking at a number and believing
/// their install reported three pages. An empty map is the honest shape: the
/// same nine keys, the same validated ids, the same vocabulary, zero events.
/// The four builders the sender uses during a period (`count_by`,
/// `observe_latency`, `record_outcome`, `count_error`) are deliberately not
/// called here for exactly that reason.
fn next_batch() -> UsageBatch {
    let usage_id = UsageId::new(random_id("livingbrain-usage-id"))
        .expect("sixteen lowercase hex characters satisfy the usage-id rule");
    UsageBatch::new(
        usage_id,
        version().expect("the crate version is a version"),
        platform(),
        next_period_end(),
    )
}

/// A fresh random id: sixteen lowercase hexadecimal characters.
///
/// # Why `RandomState`
///
/// The CLI has no RNG and this PR does not add one. `std`'s `RandomState` is
/// the only randomness already in the graph: its keys come from the operating
/// system on first use, and each instance derives a different pair, so hashing
/// a constant label through a fresh one gives a value that differs between
/// runs and between machines.
///
/// That is weaker than a CSPRNG and it is enough here, for two reasons. First,
/// what must not leak is not the value but **where it came from**: only a
/// constant label is hashed, so nothing about this machine, this account, this
/// workspace or this checkout can reach the wire through it. Second, the value
/// is display-only — `status` shows it and this file sends nothing. The real
/// sender mints the id it puts on the wire with its own generator, which is
/// the arrangement `livingbrain-telemetry` asks for: that crate holds no RNG,
/// and the caller supplies the id.
fn random_id(label: &str) -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write(label.as_bytes());
    format!("{:016x}", hasher.finish())
}

/// `on` / `off` for a person.
fn render_set(want: Telemetry, resolved: Telemetry, env: &Env) {
    if want.is_on() {
        println!("Anonymous usage data is on.");
        println!("turn it off for good:  livingbrain telemetry off");
        println!("turn it off for a run: {ENV_VAR}=0");
    } else {
        println!("Anonymous usage data is off. Nothing is collected and nothing is sent.");
        println!("turn it back on:       livingbrain telemetry on");
        println!("turn it on for a run:  {ENV_VAR}=1");
    }
    render_override(resolved, want, env);
}

/// The one line that keeps a stored value from being mistaken for the truth.
fn render_override(resolved: Telemetry, want: Telemetry, env: &Env) {
    if resolved == want {
        return;
    }
    let Some(raw) = env.telemetry.as_deref() else {
        return;
    };
    println!();
    println!(
        "note: {ENV_VAR} is set to {raw:?}, so it is {} for this run whatever the stored preference says.",
        on_off(resolved)
    );
}

/// `status` for a person: both switches, where each goes, and the batch.
fn render_status(
    live_map: Telemetry,
    stored: Telemetry,
    resolved: Telemetry,
    env: &Env,
    batch: &UsageBatch,
) {
    println!(
        "anonymous usage data: {}  (stored {}, {ENV_VAR} {})",
        on_off(resolved),
        on_off(stored),
        env.telemetry.as_deref().unwrap_or("unset")
    );
    println!("live map:              {}", live_map_line(live_map));
    println!("sent to:               {USAGE_ENDPOINT}");
    println!();
    if resolved.is_on() {
        // The crate's own announcement, verbatim: it names both off switches
        // and prints the batch exactly as the sender would send it.
        print!("{}", Telemetry::notice(batch, &body()));
        return;
    }
    // While it is off there is no period and no announcement, so printing the
    // notice would open with "Living Brain sends anonymous usage data once per
    // period" on a machine that is sending none. The batch is still shown —
    // issue #44 asks a Settings view for it, and "here is what it would be" is
    // useful precisely when the answer is off — but it is printed as the batch
    // and nothing more.
    println!("Nothing is collected and nothing is sent while this is off.");
    println!("Turn it on with `livingbrain telemetry on`, or for one run with {ENV_VAR}=1.");
    println!();
    println!("the batch that would go, and nothing else would:");
    println!(
        "{}",
        serde_json::to_string_pretty(batch).expect("a batch is always serialisable")
    );
}

/// The live-map sentence in the human view.
fn live_map_line(stored: Telemetry) -> &'static str {
    if stored.is_on() {
        "opted in — no heartbeat from this command, which sends nothing"
    } else {
        "off — no install id exists, so nothing is sent"
    }
}

/// The sender's line about this period, passed to [`Telemetry::notice`] and
/// included verbatim there.
fn body() -> String {
    format!(
        "the period ending at unix {} (the next UTC midnight)",
        next_period_end()
    )
}
#[cfg(test)]
mod tests {
    use super::{
        Action, ENV_VAR, Env, LIVE_MAP_DEFAULT, LIVE_MAP_KEY, Out, Preference, USAGE_DEFAULT,
        USAGE_KEY, next_batch, random_id, run, version,
    };
    use livingbrain_telemetry::Version;
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    /// A store that remembers, because keyring's mock does not: it keeps the
    /// value inside the `Entry` it was written to and a fresh `Entry::new`
    /// starts empty. This is what the OS keychain does across runs.
    #[derive(Default)]
    struct Memory {
        values: RefCell<BTreeMap<String, String>>,
    }

    impl Memory {
        fn with(self, name: &str, value: &str) -> Self {
            self.values.borrow_mut().insert(name.into(), value.into());
            self
        }
    }

    impl Preference for Memory {
        fn get(&self, name: &str) -> crate::CliResult<Option<String>> {
            Ok(self.values.borrow().get(name).cloned())
        }
        fn set(&self, name: &str, value: &str) -> crate::CliResult<()> {
            self.values.borrow_mut().insert(name.into(), value.into());
            Ok(())
        }
    }

    /// A store whose keychain refuses to answer, the way a locked keyring or a
    /// Linux box with no secret service does.
    ///
    /// This is not a fabricated failure dressed up as a test: `Preference` is
    /// the same seam `KeychainPreference` implements, and the error this
    /// returns is the same [`crate::CliError`] the keychain path builds. There
    /// is no `LIVINGBRAIN_TEST_*` value that makes keyring's mock fail — it
    /// cannot — so driving a real keychain error from the integration tests in
    /// `tests/telemetry.rs` is not possible, and this is where the branch is
    /// covered instead. The binary under test is not reachable from an
    /// in-crate test, so the two suites cannot be merged into one test.
    #[derive(Default)]
    struct Locked {
        /// The keys this store refuses to answer. Everything else answers, so a
        /// test can drive one failing key past a read of a working one.
        fail: Vec<&'static str>,
        /// Set when a write succeeded, so the test can tell "the write landed
        /// and the later read failed" from "nothing happened".
        wrote: RefCell<Vec<String>>,
    }

    impl Locked {
        fn failing(key: &'static str) -> Self {
            Self {
                fail: vec![key],
                ..Self::default()
            }
        }
    }

    impl Preference for Locked {
        fn get(&self, name: &str) -> crate::CliResult<Option<String>> {
            if self.fail.contains(&name) {
                return Err(crate::err(
                    "the OS keychain is unavailable: no secret service",
                ));
            }
            Ok(None)
        }
        fn set(&self, name: &str, _value: &str) -> crate::CliResult<()> {
            self.wrote.borrow_mut().push(name.to_owned());
            Ok(())
        }
    }

    fn env(telemetry: Option<&str>) -> Env {
        Env {
            telemetry: telemetry.map(str::to_owned),
        }
    }

    /// Both defaults are the crate's, in the direction each switch promises:
    /// usage on out of the box, the live map off until someone opts in. The
    /// live map asserting `Telemetry::default()` would be the bug — it would
    /// print `live map: on` for an install that has never sent a beat.
    #[test]
    fn defaults_are_the_crates_defaults() {
        assert_eq!(USAGE_DEFAULT, livingbrain_telemetry::Telemetry::default());
        assert!(USAGE_DEFAULT.is_on(), "usage is on out of the box");
        assert!(!LIVE_MAP_DEFAULT.is_on(), "the map is opt-in");
        assert!(livingbrain_telemetry::Telemetry::default().is_on());
    }

    /// This crate's own version satisfies `Version`'s grammar.
    ///
    /// The canary for the tightened grammar. `version()` is called at runtime
    /// on every path that builds a batch or a beat, so a workspace version
    /// that is no longer a version — a `v0.1.0` tag, a `git describe` output
    /// stamped into `Cargo.toml` — would be a runtime error for every user
    /// rather than a compile error here. This test is that compile error.
    #[test]
    fn this_builds_version_is_a_version() {
        let raw = env!("CARGO_PKG_VERSION");
        assert!(
            Version::new(raw).is_ok(),
            "livingbrain-cli {raw} must satisfy the grammar it ships"
        );
        assert_eq!(
            version().expect("the crate version is a version").as_str(),
            raw
        );
    }

    /// An empty store resolves the way the crate says it does: usage on, live
    /// map off, and no install id.
    #[test]
    fn an_empty_store_is_usage_on_and_the_map_off() {
        let store = Memory::default();
        let out = Out { json: false };
        run(Action::Status, &out, &store, &env(None)).expect("status");
        assert_eq!(
            store.values.borrow().get(LIVE_MAP_KEY),
            None,
            "status must not write anything"
        );
    }

    /// `off` stores the word the reader of the keychain would expect, and `on`
    /// overwrites it. Tested through `run` so the key name is the one the
    /// command really uses.
    #[test]
    fn on_and_off_persist_the_preference() {
        let store = Memory::default();
        let out = Out { json: false };
        run(Action::Off, &out, &store, &env(None)).expect("off");
        assert_eq!(
            store.values.borrow().get(USAGE_KEY).map(String::as_str),
            Some("off")
        );
        run(Action::On, &out, &store, &env(None)).expect("on");
        assert_eq!(
            store.values.borrow().get(USAGE_KEY).map(String::as_str),
            Some("on")
        );
    }

    /// The rule the acceptance criterion names: an environment of `0` beats a
    /// stored `on`. Driven through the injected `Env`, never through
    /// `set_var`, which is `unsafe` in edition 2024 and forbidden here.
    #[test]
    fn the_environment_beats_the_stored_preference() {
        let store = Memory::default().with(USAGE_KEY, "on");
        let out = Out { json: false };
        // Stored on, env `0`: `off` resolves. And `on` under the same env is
        // stored as `on` and still resolves off, with the note saying so.
        run(Action::Status, &out, &store, &env(Some("0"))).expect("status");
        run(Action::On, &out, &store, &env(Some("0"))).expect("on");
        assert_eq!(
            store.values.borrow().get(USAGE_KEY).map(String::as_str),
            Some("on"),
            "the stored value is what was asked for, not what resolved"
        );
        // A typo decides nothing, in either direction.
        let store = Memory::default().with(USAGE_KEY, "off");
        run(Action::Status, &out, &store, &env(Some("garbage"))).expect("status");
    }

    /// The defect this file used to have: `read` swallowed the keychain error
    /// and returned the default, and for usage data the default is **on**. So
    /// `status` on a locked keyring printed "anonymous usage data: on" —
    /// indistinguishable from an empty store, and the answer that collects.
    ///
    /// Now the error propagates, in both directions: nothing is printed as if
    /// the state were known, and the message names both the key it could not
    /// read and the environment variable that still decides this run.
    #[test]
    fn an_unreadable_keychain_is_an_error_not_a_silent_on() {
        // `status` alone, and deliberately: `on`/`off` *write* the usage key and
        // never read it, so an unreadable usage key is no reason to refuse a
        // switch. What they must not do is succeed while reporting a state, and
        // that is covered by the live-map test below.
        let store = Locked::failing(USAGE_KEY);
        let out = Out { json: false };

        let error = run(Action::Status, &out, &store, &env(None))
            .expect_err("a locked keychain must not be reported as a clean state");
        let message = error.to_string();
        assert!(
            message.contains(USAGE_KEY),
            "did not say which preference it could not read: {message}"
        );
        assert!(
            message.contains("no secret service"),
            "dropped the underlying cause: {message}"
        );
        assert!(
            message.contains(ENV_VAR),
            "left the reader without a way forward: {message}"
        );
        // The one thing this must never do is print a state. Returning `Err`
        // means `out.json_or` never ran, so the sentence that *would* have
        // claimed one is absent by construction — asserted anyway, because that
        // is the defect: `status` used to print "anonymous usage data: on"
        // here, from a keychain it had just failed to read.
        for claim in [
            "anonymous usage data:",
            "live map:",
            "Anonymous usage data is",
        ] {
            assert!(
                !message.contains(claim),
                "claimed a state it could not read: {message}"
            );
        }
    }

    /// `on` and `off` do not read the key they write, so a usage key that will
    /// not answer must not stop a switch being recorded — that is the write
    /// doing its job, and `set` was never the defect.
    #[test]
    fn a_switch_is_still_recorded_when_the_usage_key_cannot_be_read() {
        let store = Locked::failing(USAGE_KEY);
        let out = Out { json: false };
        run(Action::Off, &out, &store, &env(None)).expect("off writes without reading");
        assert_eq!(store.wrote.borrow().as_slice(), [USAGE_KEY.to_owned()]);
    }

    /// The live map is the second key, and it defaults to *off* — so failing
    /// open there is a lie in the other direction: `status` would say
    /// "off — no install id exists" on a machine whose opt-in it never read.
    /// Same rule, so it is asserted separately rather than assumed, and the
    /// usage key is left readable so the live-map read is the one that fails.
    #[test]
    fn an_unreadable_keychain_also_covers_the_live_map() {
        let store = Locked::failing(LIVE_MAP_KEY);
        let out = Out { json: false };
        let message = run(Action::Status, &out, &store, &env(None))
            .expect_err("a locked keychain must not be reported as a clean state")
            .to_string();
        assert!(message.contains(LIVE_MAP_KEY), "{message}");
        assert!(
            !message.contains("live map:"),
            "claimed a live-map state it could not read: {message}"
        );
    }

    /// `set` is deliberately unchanged by the fix: the write is what `on`/`off`
    /// is for, it lands, and the error that follows it is about the *other*
    /// key. A command that failed here would tell the user their switch did not
    /// change when it did.
    #[test]
    fn a_read_failure_after_a_write_does_not_undo_the_write() {
        let store = Locked::failing(LIVE_MAP_KEY);
        let out = Out { json: false };
        assert!(run(Action::Off, &out, &store, &env(None)).is_err());
        assert_eq!(
            store.wrote.borrow().as_slice(),
            [USAGE_KEY.to_owned()],
            "the preference was stored before the later read failed"
        );
    }

    /// An id is sixteen lowercase hex characters, which is the usage-id rule,
    /// and two ids from two fresh hashers are not the same value.
    #[test]
    fn an_id_is_sixteen_lowercase_characters() {
        for label in ["livingbrain-usage-id", "livingbrain-install-id"] {
            let id = random_id(label);
            assert_eq!(id.len(), 16, "{label}: {id}");
            assert!(
                id.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()),
                "{label}: {id}"
            );
        }
        assert_ne!(
            random_id("livingbrain-usage-id"),
            random_id("livingbrain-usage-id"),
            "two ids must not be the same value"
        );
    }

    /// The batch `status` shows is a real one: nine keys, a validated id, this
    /// build's version, and every map empty — no invented counts.
    #[test]
    fn the_shown_batch_is_schema_valid_and_empty() {
        let batch = next_batch();
        let value = serde_json::to_value(&batch).expect("serialise");
        let object = value.as_object().expect("an object");
        assert_eq!(object.len(), 9, "{value}");
        for key in [
            "payload_version",
            "usage_id",
            "version",
            "platform",
            "period_end_unix",
            "counters",
            "latencies",
            "outcomes",
            "errors",
        ] {
            assert!(object.contains_key(key), "missing {key}");
        }
        assert_eq!(object["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(object["usage_id"].as_str().map(str::len), Some(16));
        for empty in ["counters", "latencies", "outcomes", "errors"] {
            assert_eq!(
                object[empty],
                serde_json::json!({}),
                "{empty} must be empty: this command counts nothing"
            );
        }
    }
}
