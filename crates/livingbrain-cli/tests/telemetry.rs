//! Integration tests for `livingbrain telemetry on|off|status`.
//!
//! The headline one is the network-level check: issue #44's acceptance
//! criterion is "With telemetry off or `LIVINGBRAIN_TELEMETRY=0`, nothing is
//! sent (network-level test)", and this file proves it by pointing every
//! request the CLI could possibly make at a `TcpListener` we own and then
//! asserting that listener never accepted a connection. Not "the mock saw no
//! request" — the listener saw no *connection*, so a socket that opened and
//! was abandoned still fails the test.
//!
//! No test framework and no extra dependency, matching `tests/cli.rs`.

mod common;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use common::{Output, run, temp_dir};

/// A listener on an ephemeral port that counts the connections it accepts.
///
/// The redirect works because `ureq` — the CLI's only HTTP client — reads
/// `ALL_PROXY`/`HTTPS_PROXY`/`HTTP_PROXY` when it builds its agent config
/// (`Proxy::try_from_env`), and `--api-url` is where the API base URL comes
/// from. Anything the binary dialled, for any reason, on any host, lands here
/// first.
struct Trap {
    base: String,
    accepted: Arc<AtomicUsize>,
}

impl Trap {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the trap");
        let base = format!("http://{}", listener.local_addr().expect("trap addr"));
        let accepted = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&accepted);
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                counter.fetch_add(1, Ordering::SeqCst);
                // Answer, so a stray connection cannot hang the binary rather
                // than failing the test. It is counted either way.
                let _ = answer(stream);
            }
        });
        Self { base, accepted }
    }

    /// Connections accepted so far. Zero is the assertion; the settle window
    /// below is what makes "so far" mean "at all".
    fn count(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }
}

fn answer(mut stream: TcpStream) -> std::io::Result<()> {
    let mut scratch = [0u8; 1024];
    let _ = stream.read(&mut scratch);
    stream.write_all(
        b"HTTP/1.1 500 No Route For You\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    )
}

/// How long to keep watching for a connection that was never expected.
///
/// A connection made during the run is already in the accept queue by the
/// time the child exits, so this only covers the kernel being slower than the
/// child. It is short because the expected outcome is silence and the test
/// should not sit on it.
const SETTLE: Duration = Duration::from_millis(500);

/// Wait out the settle window, then assert nothing arrived.
fn assert_silent(trap: &Trap, what: &str) {
    let deadline = Instant::now() + SETTLE;
    while Instant::now() < deadline && trap.count() == 0 {
        thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(
        trap.count(),
        0,
        "{what} opened a connection, so it is not true that nothing is sent"
    );
}

/// Run the binary with the API base URL and every proxy variable aimed at the
/// trap, so no route out of the process goes unobserved.
fn run_trapped(
    cwd: &Path,
    home: &Path,
    args: &[&str],
    trap: &Trap,
    envs: &[(&str, &str)],
) -> Output {
    let mut all = vec![
        ("LIVINGBRAIN_API_URL", trap.base.as_str()),
        ("ALL_PROXY", trap.base.as_str()),
        ("HTTP_PROXY", trap.base.as_str()),
        ("HTTPS_PROXY", trap.base.as_str()),
        ("all_proxy", trap.base.as_str()),
        ("http_proxy", trap.base.as_str()),
        ("https_proxy", trap.base.as_str()),
        // An inherited `NO_PROXY` would exempt the trap's own host and defeat
        // the whole redirect. An empty one exempts nothing.
        ("NO_PROXY", ""),
        ("no_proxy", ""),
    ];
    all.extend_from_slice(envs);
    run(cwd, home, args, &all, None)
}

fn json_of(output: &Output) -> Value {
    serde_json::from_str(output.stdout.trim()).unwrap_or_else(|e| {
        panic!(
            "stdout was not JSON ({e}): {:?} / stderr {:?}",
            output.stdout, output.stderr
        )
    })
}

/// (1) `status --json` parses and has the keys a Settings view needs: both
/// switches, where each goes, and the next batch with the crate's nine keys.
#[test]
fn status_json_has_the_expected_keys() {
    let (cwd, home) = (
        temp_dir("telemetry-status-cwd"),
        temp_dir("telemetry-status-home"),
    );
    let output = run(&cwd, &home, &["telemetry", "status", "--json"], &[], None);
    assert!(output.status.success(), "stderr: {}", output.stderr);
    let value = json_of(&output);

    let usage = value["usage"].as_object().expect("usage object");
    for key in [
        "stored",
        "resolved",
        "env",
        "env_var",
        "endpoint",
        "next_batch",
    ] {
        assert!(usage.contains_key(key), "usage is missing {key}: {value}");
    }
    assert_eq!(usage["env_var"], "LIVINGBRAIN_TELEMETRY");
    assert_eq!(
        usage["endpoint"],
        "https://telemetry.livingbrain.wiki/v1/telemetry/events"
    );

    let live_map = value["live_map"].as_object().expect("live_map object");
    for key in ["stored", "sends_heartbeat", "endpoint"] {
        assert!(live_map.contains_key(key), "live_map is missing {key}");
    }
    assert_eq!(live_map["stored"], "off", "the map is opt-in");
    assert_eq!(
        live_map["sends_heartbeat"], false,
        "no install id, so no heartbeat"
    );
    assert_eq!(
        live_map["endpoint"],
        "https://telemetry.livingbrain.wiki/v1/telemetry/heartbeat"
    );

    // The batch is the crate's own serialisation, so all nine of its keys are
    // here and every map is empty: this command counts nothing.
    let batch = usage["next_batch"].as_object().expect("next_batch object");
    assert_eq!(
        batch.len(),
        9,
        "the batch carries the nine listed keys: {batch:?}"
    );
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
        assert!(batch.contains_key(key), "the batch is missing {key}");
    }
    for empty in ["counters", "latencies", "outcomes", "errors"] {
        assert_eq!(
            batch[empty],
            serde_json::json!({}),
            "{empty} must be empty, not a made-up count"
        );
    }
    let id = batch["usage_id"].as_str().expect("usage_id string");
    assert_eq!(id.len(), 16, "{id}");
    assert!(
        id.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    );
}

/// (2) `LIVINGBRAIN_TELEMETRY=0` resolves to `off` even though the stored
/// preference is `on` — the default here, because keyring's mock keeps nothing
/// between `Entry`s. That is exactly the case the criterion names.
///
/// The env is passed to the *child* through `Command::env`, so nothing calls
/// `std::env::set_var`, which is `unsafe` in edition 2024 and forbidden by
/// `[workspace.lints.rust] unsafe_code = "forbid"`. The module's own `Env`
/// parameter is what makes the same rule testable in-process too.
#[test]
fn the_environment_var_forces_off_over_a_stored_on() {
    let (cwd, home) = (
        temp_dir("telemetry-env-cwd"),
        temp_dir("telemetry-env-home"),
    );

    let stored_on = run(&cwd, &home, &["telemetry", "status", "--json"], &[], None);
    assert!(stored_on.status.success(), "stderr: {}", stored_on.stderr);
    assert_eq!(json_of(&stored_on)["usage"]["stored"], "on");
    assert_eq!(json_of(&stored_on)["usage"]["resolved"], "on");

    let forced = run(
        &cwd,
        &home,
        &["telemetry", "status", "--json"],
        &[("LIVINGBRAIN_TELEMETRY", "0")],
        None,
    );
    assert!(forced.status.success(), "stderr: {}", forced.stderr);
    let value = json_of(&forced);
    assert_eq!(
        value["usage"]["stored"], "on",
        "the stored value is unchanged"
    );
    assert_eq!(value["usage"]["resolved"], "off", "the env decides the run");
    assert_eq!(value["usage"]["env"], "0");

    // The human view says which one is in force and names the variable, so a
    // reader is never left wondering why `on` did not take.
    let human = run(
        &cwd,
        &home,
        &["telemetry", "on"],
        &[("LIVINGBRAIN_TELEMETRY", "0")],
        None,
    );
    assert!(human.status.success(), "stderr: {}", human.stderr);
    assert!(
        human.stdout.contains("LIVINGBRAIN_TELEMETRY"),
        "{}",
        human.stdout
    );
    assert!(
        human.stdout.contains("whatever the stored preference says"),
        "{}",
        human.stdout
    );
}

/// (3) The issue's acceptance criterion, at the network level: `on`, `off` and
/// `status` open no connection at all.
///
/// Every route out of the process — `--api-url` and all six proxy variables —
/// points at one listener this test owns, so a single accepted connection
/// fails the test whatever it was carrying.
#[test]
fn no_connection_is_opened_by_on_off_or_status() {
    let trap = Trap::start();
    let (cwd, home) = (
        temp_dir("telemetry-net-cwd"),
        temp_dir("telemetry-net-home"),
    );

    for args in [
        &["telemetry", "status"][..],
        &["telemetry", "off"][..],
        &["telemetry", "on"][..],
        &["telemetry", "status", "--json"][..],
        &["telemetry", "off", "--json"][..],
    ] {
        let output = run_trapped(&cwd, &home, args, &trap, &[]);
        assert!(
            output.status.success(),
            "{args:?} failed: {}",
            output.stderr
        );
        assert!(!output.stdout.is_empty(), "{args:?} printed nothing");
    }

    // The same three with telemetry forced off, which is the other half of the
    // criterion ("with telemetry off *or* LIVINGBRAIN_TELEMETRY=0").
    for args in [
        &["telemetry", "status"][..],
        &["telemetry", "off"][..],
        &["telemetry", "on"][..],
    ] {
        let output = run_trapped(&cwd, &home, args, &trap, &[("LIVINGBRAIN_TELEMETRY", "0")]);
        assert!(
            output.status.success(),
            "{args:?} failed: {}",
            output.stderr
        );
    }

    assert_silent(&trap, "`telemetry on` / `off` / `status`");
}

/// (4) Nothing is written to disk: the preference lives in the OS keychain, so
/// no file under `$HOME` or the working directory mentions it afterwards. This
/// is `auth.rs`'s argument applied to a value that is not even a secret — the
/// CLI has no config file, and this command does not start one.
#[test]
fn the_preference_is_never_written_to_a_file() {
    let (cwd, home) = (
        temp_dir("telemetry-file-cwd"),
        temp_dir("telemetry-file-home"),
    );
    let output = run(&cwd, &home, &["telemetry", "off"], &[], None);
    assert!(output.status.success(), "stderr: {}", output.stderr);
    common::assert_absent_from_tree(&home, "telemetry.usage");
    common::assert_absent_from_tree(&cwd, "telemetry.usage");
}

/// (5) `on` and `off` print what changed and how to override it, in the
/// command's own style, and both honour `--json`. Neither needs a token: this
/// is the only command that works while logged out.
#[test]
fn on_and_off_name_the_override() {
    let (cwd, home) = (temp_dir("telemetry-on-cwd"), temp_dir("telemetry-on-home"));

    let off = run(&cwd, &home, &["telemetry", "off"], &[], None);
    assert!(off.status.success(), "stderr: {}", off.stderr);
    assert!(
        off.stdout
            .contains("Nothing is collected and nothing is sent."),
        "{}",
        off.stdout
    );
    assert!(
        off.stdout.contains("livingbrain telemetry on"),
        "{}",
        off.stdout
    );
    assert!(
        off.stdout.contains("LIVINGBRAIN_TELEMETRY=1"),
        "{}",
        off.stdout
    );

    let on = run(&cwd, &home, &["telemetry", "on"], &[], None);
    assert!(on.status.success(), "stderr: {}", on.stderr);
    assert!(
        on.stdout.contains("Anonymous usage data is on."),
        "{}",
        on.stdout
    );
    assert!(
        on.stdout.contains("livingbrain telemetry off"),
        "{}",
        on.stdout
    );
    assert!(
        on.stdout.contains("LIVINGBRAIN_TELEMETRY=0"),
        "{}",
        on.stdout
    );

    let json = run(&cwd, &home, &["telemetry", "off", "--json"], &[], None);
    assert!(json.status.success(), "stderr: {}", json.stderr);
    let value = json_of(&json);
    assert_eq!(value["usage"]["stored"], "off");
    assert_eq!(value["usage"]["resolved"], "off");
    assert!(value["usage"]["endpoint"].is_string());
}

/// (6) `status` prints the batch verbatim, through the crate's own
/// announcement, so the reader sees the nine keys and both off switches
/// without this file inventing a rendering of them.
#[test]
fn status_prints_the_next_batch_verbatim() {
    let (cwd, home) = (
        temp_dir("telemetry-batch-cwd"),
        temp_dir("telemetry-batch-home"),
    );
    let output = run(&cwd, &home, &["telemetry", "status"], &[], None);
    assert!(output.status.success(), "stderr: {}", output.stderr);
    let stdout = output.stdout;
    assert!(stdout.contains("exactly what this batch says"), "{stdout}");
    for key in [
        "\"payload_version\"",
        "\"usage_id\"",
        "\"version\"",
        "\"platform\"",
        "\"period_end_unix\"",
        "\"counters\"",
        "\"latencies\"",
        "\"outcomes\"",
        "\"errors\"",
    ] {
        assert!(stdout.contains(key), "{key} missing from:\n{stdout}");
    }
    // And it names where the data goes, which is the question a self-hoster
    // runs this for.
    assert!(
        stdout.contains("https://telemetry.livingbrain.wiki/v1/telemetry/events"),
        "{stdout}"
    );
    assert!(
        stdout.contains("live map:              off"),
        "the map is opt-in and must read as off:\n{stdout}"
    );
}
