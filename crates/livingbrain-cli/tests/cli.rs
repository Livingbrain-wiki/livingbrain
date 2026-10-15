//! Integration tests: the real `livingbrain` binary against a local mock API.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value;

use common::{Mock, Output, Reply, assert_absent_from_tree, run, temp_dir};

/// The token the tests hand the binary through the environment.
const TOKEN: &str = "tok-test-bearer-1234567890";

fn api_envs(base: &str) -> Vec<(&str, &str)> {
    vec![("LIVINGBRAIN_API_URL", base), ("LIVINGBRAIN_TOKEN", TOKEN)]
}

fn json_of(output: &Output) -> Value {
    serde_json::from_str(output.stdout.trim()).unwrap_or_else(|e| {
        panic!(
            "stdout was not JSON ({e}): {:?} / stderr {:?}",
            output.stdout, output.stderr
        )
    })
}

/// (1) `ask` returns a cited answer, sends the bearer token, and `--json`
/// gives a parseable object with citations.
#[test]
fn ask_cites_and_sends_the_token() {
    let mock = Mock::start(|request| match request.path.as_str() {
        "/v1/ask" => Reply::ok_json(
            r#"{"answer":"Use KV for hot data.","citations":[{"title":"Storage","url":"https://wiki.example/storage"}]}"#,
        ),
        _ => Reply::json(404, r#"{"error":"no such route"}"#),
    });
    let (cwd, home) = (temp_dir("ask-cwd"), temp_dir("ask-home"));
    let envs = api_envs(&mock.base);

    let human = run(&cwd, &home, &["ask", "where", "is", "storage"], &envs, None);
    assert!(human.status.success(), "stderr: {}", human.stderr);
    assert!(
        human.stdout.contains("Use KV for hot data."),
        "{}",
        human.stdout
    );
    assert!(
        human
            .stdout
            .contains("[1] Storage <https://wiki.example/storage>"),
        "{}",
        human.stdout
    );

    let ask = mock
        .requests()
        .into_iter()
        .find(|r| r.path == "/v1/ask")
        .expect("the CLI called /v1/ask");
    assert_eq!(ask.method, "POST");
    assert_eq!(ask.bearer().as_deref(), Some(TOKEN));

    let json = run(
        &cwd,
        &home,
        &["ask", "where", "is", "storage", "--json"],
        &envs,
        None,
    );
    assert!(json.status.success(), "stderr: {}", json.stderr);
    let value = json_of(&json);
    assert_eq!(value["answer"], "Use KV for hot data.");
    assert!(
        !value["citations"]
            .as_array()
            .expect("citations array")
            .is_empty()
    );
}

/// (2) Every subcommand accepts `--json` and emits valid JSON on stdout.
#[test]
fn every_command_supports_json() {
    let mock = Mock::start(|request| match request.path.as_str() {
        "/v1/ask" => Reply::ok_json(r#"{"answer":"a","citations":[]}"#),
        "/v1/search" => {
            Reply::ok_json(r#"{"results":[{"title":"T","url":"https://u","snippet":"s"}]}"#)
        }
        "/v1/notes" => Reply::ok_json(r#"{"id":"n1","url":"https://u/n1"}"#),
        "/v1/export" => Reply::bytes(200, "application/zip", b"PK\x03\x04zip".to_vec()),
        path if path.starts_with("/v1/pages/") => {
            Reply::ok_json(r##"{"slug":"a","title":"A","markdown":"# A","url":"https://u/a"}"##)
        }
        _ => Reply::json(404, r#"{"error":"no such route"}"#),
    });
    let (cwd, home) = (temp_dir("table-cwd"), temp_dir("table-home"));
    let envs = api_envs(&mock.base);

    let table: [&[&str]; 5] = [
        &["ask", "q", "--json"],
        &["search", "q", "--json"],
        &["note", "a note", "--json"],
        &["page", "some/slug", "--json"],
        &["export", "--json"],
    ];
    for args in table {
        let output = run(&cwd, &home, args, &envs, None);
        assert!(
            output.status.success(),
            "{args:?} failed: {}",
            output.stderr
        );
        assert!(
            json_of(&output).is_object(),
            "{args:?} was not a JSON object"
        );
    }

    // The page slug is percent-encoded on the wire.
    assert!(
        mock.requests()
            .iter()
            .any(|r| r.path == "/v1/pages/some%2Fslug"),
        "the slug was not percent-encoded"
    );

    // `export` refuses to clobber, and `--force` overwrites atomically.
    let again = run(&cwd, &home, &["export", "--json"], &envs, None);
    assert!(!again.status.success());
    assert!(again.stderr.contains("--force"), "{}", again.stderr);
    let forced = run(&cwd, &home, &["export", "--json", "--force"], &envs, None);
    assert!(forced.status.success(), "stderr: {}", forced.stderr);

    // `mcp` speaks JSON on stdout regardless of `--json`.
    let input = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","method":"notifications/cancelled"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":3,"method":"who/knows"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":4,"method":"notifications/initialized"}"#,
        "\n",
    );
    let mcp = run(&cwd, &home, &["mcp", "--json"], &envs, Some(input));
    assert!(mcp.status.success(), "stderr: {}", mcp.stderr);
    let lines: Vec<Value> = mcp
        .stdout
        .lines()
        .map(|line| serde_json::from_str(line).expect("each MCP line is JSON"))
        .collect();
    // initialize, tools/list, the unknown-method error and the id-bearing
    // notification — but neither id-less notification (notifications/initialized
    // and the unknown notifications/cancelled).
    assert_eq!(lines.len(), 4, "{:?}", mcp.stdout);
    assert_eq!(lines[0]["id"], 1);
    assert_eq!(lines[1]["result"]["tools"][0]["name"], "brain_search");
    assert_eq!(lines[2]["error"]["code"], -32601);
    assert_eq!(lines[3]["id"], 4);
}

/// (3) `note` reads its body from stdin when it is piped.
#[test]
fn note_reads_stdin_when_piped() {
    let mock = Mock::start(|_| Reply::ok_json(r#"{"id":"n1","url":"https://u/n1"}"#));
    let (cwd, home) = (temp_dir("note-cwd"), temp_dir("note-home"));
    let envs = api_envs(&mock.base);

    let output = run(
        &cwd,
        &home,
        &["note", "--project", "api"],
        &envs,
        Some("git log -5 output\n"),
    );
    assert!(output.status.success(), "stderr: {}", output.stderr);
    assert!(output.stdout.contains("https://u/n1"), "{}", output.stdout);

    let note = mock
        .requests()
        .into_iter()
        .find(|r| r.path == "/v1/notes")
        .expect("the CLI called /v1/notes");
    let body: Value = serde_json::from_str(&note.body_text()).expect("the note body is JSON");
    assert_eq!(body["body"], "git log -5 output");
    assert_eq!(body["project"], "api");
}

/// (4) `login` never writes the token to disk, stdout or stderr.
#[cfg_attr(not(debug_assertions), ignore = "needs the debug-only mock keyring")]
#[test]
fn login_never_persists_the_token_in_plaintext() {
    const LEAKY: &str = "tok-LEAK-cafebabe-9f8e7d6c";
    let calls = Arc::new(AtomicUsize::new(0));
    let handler = {
        let calls = Arc::clone(&calls);
        move |request: &common::Request| match request.path.as_str() {
            "/v1/device-auth/code" => Reply::ok_json(
                r#"{"device_code":"dc","user_code":"WDJB-MJHT","verification_uri":"https://example.test/device","interval":0,"expires_in":60}"#,
            ),
            "/v1/device-auth/token" => {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    Reply::json(400, r#"{"error":"authorization_pending"}"#)
                } else {
                    Reply::ok_json(&format!(r#"{{"access_token":"{LEAKY}"}}"#))
                }
            }
            _ => Reply::json(404, r#"{"error":"no such route"}"#),
        }
    };
    let mock = Mock::start(handler);
    let (cwd, home) = (temp_dir("login-cwd"), temp_dir("login-home"));

    let output = run(
        &cwd,
        &home,
        &["login", "--json"],
        &[("LIVINGBRAIN_API_URL", mock.base.as_str())],
        None,
    );
    assert!(output.status.success(), "stderr: {}", output.stderr);
    // The final result is on stdout; the hint (code + URL) on stderr.
    assert_eq!(json_of(&output)["logged_in"], true);
    assert!(output.stderr.contains("WDJB-MJHT"), "{}", output.stderr);

    // The token is in neither stream, and in no file under either tree.
    assert!(!output.stdout.contains(LEAKY) && !output.stderr.contains(LEAKY));
    assert_absent_from_tree(&home, LEAKY);
    assert_absent_from_tree(&cwd, LEAKY);
}

/// (5) A 401 exits non-zero with a login hint; `--json` puts JSON on stderr.
#[test]
fn unauthorized_is_a_clear_json_error() {
    let mock = Mock::start(|_| Reply::json(401, r#"{"error":"token expired"}"#));
    let (cwd, home) = (temp_dir("401-cwd"), temp_dir("401-home"));
    let envs = api_envs(&mock.base);

    let human = run(&cwd, &home, &["search", "q"], &envs, None);
    assert!(!human.status.success());
    assert!(human.stdout.is_empty());
    assert!(human.stderr.contains("token expired"), "{}", human.stderr);
    assert!(
        human.stderr.contains("livingbrain login"),
        "{}",
        human.stderr
    );

    let json = run(&cwd, &home, &["search", "q", "--json"], &envs, None);
    assert!(!json.status.success());
    let error: Value = serde_json::from_str(json.stderr.trim()).expect("JSON error on stderr");
    assert!(
        error["error"]
            .as_str()
            .expect("error string")
            .contains("login")
    );
}

/// (6) `about` prints the vendored stack with no token and no API (issue #61):
/// the list is compiled into the binary, so a reader gets it before signing in.
#[test]
fn about_needs_no_credentials() {
    let (cwd, home) = (temp_dir("about-cwd"), temp_dir("about-home"));

    let human = run(&cwd, &home, &["about"], &[], None);
    assert!(human.status.success(), "stderr: {}", human.stderr);
    for expected in [
        "Built with",
        "Cratefield",
        "Cloudflare",
        "https://cratefield.com/",
        "https://factory0.ventures/ventures/living-brain/",
        "https://factory0.ventures/stack.json",
    ] {
        assert!(
            human.stdout.contains(expected),
            "missing {expected}:\n{}",
            human.stdout
        );
    }
    // Nothing is deployed but the site, and the row says so: hosting is live
    // and the framework it is built with is not.
    let hosting = human
        .stdout
        .lines()
        .find(|line| line.contains("Cloudflare"))
        .expect("the hosting row");
    assert!(hosting.contains(" live "), "{hosting}");
    let framework = human
        .stdout
        .lines()
        .find(|line| line.contains("Cratefield"))
        .expect("the framework row");
    assert!(framework.contains(" planned "), "{framework}");

    let json = run(&cwd, &home, &["about", "--json"], &[], None);
    assert!(json.status.success(), "stderr: {}", json.stderr);
    let value = json_of(&json);
    assert_eq!(value["venture"]["id"], "FZ-018");
    assert_eq!(value["source"], "https://factory0.ventures/stack.json");
    assert_eq!(value["uses"].as_array().expect("uses").len(), 9);
    assert_eq!(value["uses"][8]["status"], "live");
    assert_eq!(value["uses"][0]["status"], "planned");
}
