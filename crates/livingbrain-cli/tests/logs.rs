//! Integration tests for `livingbrain logs`: the real binary against a local
//! mock API and the fixture logs on disk — Claude Code and Codex layouts,
//! planted where the agents would have written them.
//!
//! The order of the assertions mirrors the order of the command's guarantees:
//! nothing opted in, no request at all; opted in, only redacted bytes travel;
//! re-run, the same bytes, read back as unchanged; never, a session whose repo
//! is not opted in — including one that moved between an opted-in repo and a
//! stranger.

mod common;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use common::{Mock, Output, Reply, run, temp_dir};

/// The token the tests hand the binary through the environment.
const TOKEN: &str = "tok-logs-bearer-1234567890";

/// The repository the demo sessions were recorded in — a path that does not
/// exist on the test machine, which is the point: `logs allow` must accept a
/// checkout that is not mounted here yet.
const DEMO_REPO: &str = "/home/alice/code/demo";

/// The side repository one fixture session was recorded in, and the second
/// cwd of the session that moved between repos mid-flight.
const SIDE_REPO: &str = "/home/alice/code/side";

/// The fixture session ids, plus the moved session the multi-cwd test builds.
const CLAUDE_SESSION: &str = "3f2a9c1e-5b7d-4e8a-9c1f-2a4b6d8e0f1a";
const SIDE_SESSION: &str = "7c9d1e3a-2b4f-4c6e-8a0d-5f7e9b1d3c5a";
const CODEX_SESSION: &str = "a1b2c3d4-e5f6-4a9b-8c7d-3e5f7a9b1c2d";
const MOVED_SESSION: &str = "9b3c7f2a-1d4e-4f6a-8b2c-5d7e9a1b3c4f";

/// The placeholder the fixture files carry where a fake Anthropic key was
/// planted — a whole key literal in the tree would be refused by GitHub push
/// protection — spliced out for the runtime value as the fixtures are copied.
const PLANTED_KEY_PLACEHOLDER: &str = "__PLANTED_ANTHROPIC_KEY__";

/// The same arrangement for the fixture's fake AWS access key.
const PLANTED_AWS_KEY_PLACEHOLDER: &str = "__PLANTED_AWS_KEY__";

/// A fake API key planted in both sessions' user prompts — shaped exactly
/// like the real thing so the redactor must catch it on the wire, but
/// assembled from fragments so no source file carries a key-shaped literal.
fn planted_key() -> String {
    ["sk-", "ant-", "api03-fake", &"0".repeat(22)].concat()
}

/// The fake AWS access key in the Codex fixture, built the same way.
fn planted_aws_key() -> String {
    ["AKIA", "IOSFODNN7", "EXAMPLE"].concat()
}

/// The one JSON object `--json` printed on stdout.
fn json_of(output: &Output) -> Value {
    serde_json::from_str(output.stdout.trim()).unwrap_or_else(|e| {
        panic!(
            "stdout was not JSON ({e}): {:?} / stderr {:?}",
            output.stdout, output.stderr
        )
    })
}

fn api_envs(base: &str) -> Vec<(&str, &str)> {
    vec![("LIVINGBRAIN_API_URL", base), ("LIVINGBRAIN_TOKEN", TOKEN)]
}

/// The API variables plus a real-shaped `HOME`. The harness points `HOME` at
/// the temp dir first and applies these after, so this wins — which is how the
/// home-elision tests get a `$HOME` the fixtures were actually written under.
/// `XDG_CONFIG_HOME` still points at the temp dir, so the opt-in file stays in
/// the sandbox while the elision sees the real-shaped home.
fn real_home_envs(base: &str) -> Vec<(&'static str, &str)> {
    vec![
        ("LIVINGBRAIN_API_URL", base),
        ("LIVINGBRAIN_TOKEN", TOKEN),
        ("HOME", "/home/alice"),
    ]
}

fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A mock that answers `POST /v1/pages/sources` the way the server does:
/// `201 created:true` the first time it sees a body's SHA-256, `200
/// created:false` after — dedupe on the body, which is the ledger's own
/// idempotency key — and echoes the `kind` that arrived.
///
/// It answers `POST /v1/notes` the way the notes route does for a keyed
/// note: one page per `slug`, a new version only when the note's content
/// (title, project, body) changed, `unchanged: true` otherwise.
fn sources_mock() -> Mock {
    let seen: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let pages: Arc<Mutex<HashMap<String, (String, u64)>>> = Arc::new(Mutex::new(HashMap::new()));
    let handler = move |request: &common::Request| {
        if request.path == "/v1/notes" {
            let body: Value = serde_json::from_str(&request.body_text()).unwrap_or(Value::Null);
            let Some(slug) = body["slug"].as_str() else {
                return Reply::json(400, r#"{"error":"the mock only keeps keyed notes"}"#);
            };
            let content = format!("{}|{}|{}", body["title"], body["project"], body["body"]);
            let mut pages = pages.lock().expect("pages lock");
            let (version, unchanged) = match pages.get(slug) {
                Some((held, version)) if *held == content => (*version, true),
                Some((_, version)) => (version + 1, false),
                None => (1, false),
            };
            pages.insert(slug.to_owned(), (content, version));
            let reply = json!({
                "id": format!("user-scope/{slug}"),
                "url": format!("https://livingbrain.wiki/brain/user-scope/{slug}"),
                "version": version,
                "redacted": 0,
                "unchanged": unchanged,
            });
            return Reply::json(200, &reply.to_string());
        }
        if request.path != "/v1/pages/sources" {
            return Reply::json(404, r#"{"error":"no such route"}"#);
        }
        let body: Value = serde_json::from_str(&request.body_text()).unwrap_or(Value::Null);
        let text = body["body"].as_str().unwrap_or("");
        let digest = sha256_hex(text);
        let created = seen.lock().expect("seen lock").insert(digest.clone());
        let reply = json!({
            "id": format!("src-{}", &digest[..12]),
            "kind": body["kind"].clone(),
            "scope": body["scope"].clone(),
            "path": body["path"].clone(),
            "project": body["project"].clone(),
            "sha256": digest,
            "wikilinks": Vec::<String>::new(),
            "redacted": 0,
            "created": created,
            "created_at": "2026-10-01T10:15:00Z",
        });
        Reply::json(if created { 201 } else { 200 }, &reply.to_string())
    };
    Mock::start(handler)
}

/// Copy the fixture logs into a fake home, where the agents would have
/// written them: `fixtures/logs/claude` becomes `~/.claude`, `fixtures/logs/
/// codex` becomes `~/.codex`, so `logs sync` finds them at its defaults.
fn plant(home: &Path) {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for (from, to) in [
        ("tests/fixtures/logs/claude", ".claude"),
        ("tests/fixtures/logs/codex", ".codex"),
    ] {
        copy_tree(&manifest.join(from), &home.join(to));
    }
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("create destination");
    for entry in std::fs::read_dir(from).expect("read source").flatten() {
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            let raw = std::fs::read_to_string(entry.path()).expect("read fixture file");
            let planted = raw
                .replace(PLANTED_KEY_PLACEHOLDER, &planted_key())
                .replace(PLANTED_AWS_KEY_PLACEHOLDER, &planted_aws_key());
            std::fs::write(&target, planted).expect("copy fixture file");
        }
    }
}

/// `logs allow <path> --json`, against the test home.
fn allow(home: &Path, path: &str) -> Output {
    let cwd = temp_dir("logs-cwd");
    run(&cwd, home, &["logs", "allow", path, "--json"], &[], None)
}

/// `logs sync --json` over the planted fixtures.
fn sync(home: &Path, envs: &[(&str, &str)], extra: &[&str]) -> Output {
    let cwd = temp_dir("logs-cwd");
    let mut args = vec!["logs", "sync", "--json"];
    args.extend_from_slice(extra);
    run(&cwd, home, &args, envs, None)
}

/// Point the sync at the planted fixture trees explicitly — for the runs that
/// override `HOME` to a path where the defaults would not exist.
fn fixture_dirs(home: &Path) -> Vec<String> {
    vec![
        "--claude-dir".to_owned(),
        home.join(".claude")
            .join("projects")
            .to_string_lossy()
            .into_owned(),
        "--codex-dir".to_owned(),
        home.join(".codex")
            .join("sessions")
            .to_string_lossy()
            .into_owned(),
    ]
}

/// Every `POST /v1/pages/sources` envelope the mock saw, parsed.
fn source_bodies(mock: &Mock) -> Vec<Value> {
    mock.requests()
        .into_iter()
        .filter(|request| request.path == "/v1/pages/sources")
        .map(|request| serde_json::from_str(&request.body_text()).expect("a JSON source body"))
        .collect()
}

/// Every `POST /v1/notes` body the mock saw, parsed.
fn note_bodies(mock: &Mock) -> Vec<Value> {
    mock.requests()
        .into_iter()
        .filter(|request| request.path == "/v1/notes")
        .map(|request| serde_json::from_str(&request.body_text()).expect("a JSON note body"))
        .collect()
}

/// The redacted session record an envelope carries.
fn record_of(envelope: &Value) -> Value {
    serde_json::from_str(envelope["body"].as_str().expect("a body string"))
        .expect("a JSON session record")
}

/// (a) With nothing opted in, a sync reads the fixtures and reports them —
/// and makes no request at all, the auth one included: the token is in the
/// environment, so the only thing that kept the socket closed is the command.
/// A file that could not be read and a file with no session in it are
/// counted separately; both fixtures parse, so neither counter moves here.
#[test]
fn sync_without_opt_in_makes_no_requests() {
    let mock = sources_mock();
    let home = temp_dir("logs-home");
    plant(&home);

    let output = sync(&home, &api_envs(&mock.base), &[]);
    assert!(output.status.success(), "stderr: {}", output.stderr);
    let report = json_of(&output);
    assert_eq!(report["scanned"], 3, "{report}");
    assert_eq!(report["uploaded"], 0);
    assert_eq!(report["unchanged"], 0);
    assert_eq!(report["skipped"], 3);
    assert_eq!(report["unreadable"], 0, "{report}");
    assert_eq!(report["unparsed"], 0, "{report}");
    assert_eq!(
        report["redacted"], 0,
        "no body is built for a skipped session"
    );
    for session in report["sessions"].as_array().expect("sessions") {
        assert_eq!(session["status"], "skipped");
        assert_eq!(session["reason"], "repo not opted in");
    }
    assert!(
        mock.requests().is_empty(),
        "a sync with nothing opted in still touched the network"
    );
}

/// (b) Opting the demo repo in uploads its two sessions; every request
/// carries the bearer token and the `agent_log` kind, and no body carries the
/// planted key or either spelling of the home. This run has a real-shaped
/// `HOME`, so elision rewrites `/home/alice` to `~` before redaction — the
/// home path never needs the redactor at all.
#[test]
fn an_opted_in_repo_uploads_redacted_sessions() {
    let mock = sources_mock();
    let home = temp_dir("logs-home");
    plant(&home);
    let allowed = allow(&home, DEMO_REPO);
    assert!(allowed.status.success(), "stderr: {}", allowed.stderr);

    let extra = fixture_dirs(&home);
    let extra: Vec<&str> = extra.iter().map(String::as_str).collect();
    let output = sync(&home, &real_home_envs(&mock.base), &extra);
    assert!(output.status.success(), "stderr: {}", output.stderr);
    let report = json_of(&output);
    assert_eq!(report["scanned"], 3, "{report}");
    assert_eq!(report["uploaded"], 2, "{report}");
    assert_eq!(report["skipped"], 1, "{report}");
    // One finding per uploaded session: its planted key. The home prefix was
    // rewritten to `~` before redaction ran, so it costs the redactor nothing.
    assert_eq!(report["redacted"], 2, "{report}");

    let requests: Vec<common::Request> = mock
        .requests()
        .into_iter()
        .filter(|request| request.path != "/v1/notes")
        .collect();
    assert_eq!(requests.len(), 2, "exactly the opted-in sessions went out");
    for request in &requests {
        assert_eq!(request.path, "/v1/pages/sources");
        assert_eq!(request.bearer().as_deref(), Some(TOKEN));
        let envelope: Value = serde_json::from_str(&request.body_text()).expect("a JSON envelope");
        assert_eq!(envelope["kind"], "agent_log", "{envelope}");
        // Filed under the repository's basename, so the server can list and
        // filter the sessions by project without opening a body.
        assert_eq!(envelope["project"], "demo", "{envelope}");
        // The bytes that crossed the socket are the redacted ones: the record
        // carries neither the planted key nor either spelling of the home.
        let body = envelope["body"].as_str().expect("a body string");
        assert!(!body.contains(planted_key().as_str()), "{body}");
        assert!(!body.contains("/home/alice"), "{body}");
        assert!(!body.contains("-home-alice"), "{body}");
        assert!(body.contains("[REDACTED:"), "{body}");
    }
}

/// (c) A second sync sends byte-identical bodies — the record is
/// deterministic — and the mock's `200` path reads them back as unchanged,
/// so the CLI reports nothing new.
#[test]
fn re_syncing_sends_identical_bodies_and_reports_nothing_new() {
    let mock = sources_mock();
    let home = temp_dir("logs-home");
    plant(&home);
    let allowed = allow(&home, DEMO_REPO);
    assert!(allowed.status.success(), "stderr: {}", allowed.stderr);

    let first = sync(&home, &api_envs(&mock.base), &[]);
    assert!(first.status.success(), "stderr: {}", first.stderr);
    let first_report = json_of(&first);
    assert_eq!(first_report["uploaded"], 2, "{first_report}");
    let first_bodies: Vec<String> = source_bodies(&mock)
        .iter()
        .map(|body| body["body"].as_str().expect("a body").to_owned())
        .collect();

    let second = sync(&home, &api_envs(&mock.base), &[]);
    assert!(second.status.success(), "stderr: {}", second.stderr);
    let second_report = json_of(&second);
    assert_eq!(second_report["uploaded"], 0, "{second_report}");
    assert_eq!(second_report["unchanged"], 2, "{second_report}");

    let second_bodies: Vec<String> = source_bodies(&mock)
        .iter()
        .skip(first_bodies.len())
        .map(|body| body["body"].as_str().expect("a body").to_owned())
        .collect();
    let mut a = first_bodies.clone();
    let mut b = second_bodies.clone();
    a.sort();
    b.sort();
    assert_eq!(
        a, b,
        "the re-sync sent different bytes for the same sessions"
    );
}

/// (d) The side-repo session, whose repo was never allowed, never goes out —
/// even though its file sits in the same `~/.claude/projects` tree.
#[test]
fn a_session_outside_the_opt_in_is_never_uploaded() {
    let mock = sources_mock();
    let home = temp_dir("logs-home");
    plant(&home);
    let allowed = allow(&home, DEMO_REPO);
    assert!(allowed.status.success(), "stderr: {}", allowed.stderr);

    let output = sync(&home, &api_envs(&mock.base), &[]);
    assert!(output.status.success(), "stderr: {}", output.stderr);
    for request in mock.requests() {
        assert!(
            !request.body_text().contains(SIDE_SESSION),
            "the side-repo session was uploaded"
        );
    }
    let report = json_of(&output);
    let sessions = report["sessions"].as_array().expect("sessions");
    let side = sessions
        .iter()
        .find(|session| session["session_id"] == SIDE_SESSION)
        .expect("the side session is reported");
    assert_eq!(side["status"], "skipped");
    assert_eq!(side["repo"], SIDE_REPO);
}

/// (e) A session that `cd`'d mid-flight from the demo repo into the side one
/// records two working directories, and the opt-in is decided on **all** of
/// them: with only the demo repo allowed it is skipped and nothing is sent —
/// riding the first repo's allowance would leak the second. Once both repos
/// are allowed, the same bytes go out, under the session's first repo.
#[test]
fn a_session_that_moved_repos_needs_both_opted_in() {
    let mock = sources_mock();
    let home = temp_dir("logs-home");
    plant(&home);
    // A third session, built from the moved pattern: opened in the demo repo,
    // continued in the side one, planted in its own project directory.
    let session_dir = home
        .join(".claude")
        .join("projects")
        .join("-home-alice-code-mixed");
    std::fs::create_dir_all(&session_dir).expect("create the moved session's project dir");
    let user_line = format!(
        r#"{{"type":"user","sessionId":"{MOVED_SESSION}","cwd":"{DEMO_REPO}","gitBranch":"main","timestamp":"2026-10-01T11:00:00.000Z","message":{{"role":"user","content":"Continuing in both checkouts today."}}}}"#
    );
    let assistant_line = format!(
        r#"{{"type":"assistant","sessionId":"{MOVED_SESSION}","cwd":"{SIDE_REPO}","gitBranch":"main","timestamp":"2026-10-01T11:00:05.000Z","message":{{"id":"msg_9F2A","role":"assistant","model":"claude-sonnet-4-5","content":[{{"type":"text","text":"Noted."}}],"usage":{{"input_tokens":10,"output_tokens":5}}}}}}"#
    );
    std::fs::write(
        session_dir.join(format!("{MOVED_SESSION}.jsonl")),
        format!("{user_line}\n{assistant_line}\n"),
    )
    .expect("plant the moved session");

    let allowed = allow(&home, DEMO_REPO);
    assert!(allowed.status.success(), "stderr: {}", allowed.stderr);

    let first = sync(&home, &api_envs(&mock.base), &[]);
    assert!(first.status.success(), "stderr: {}", first.stderr);
    let report = json_of(&first);
    // The two demo-repo sessions are opted in and go out; the moved one,
    // half-demo half-side, does not ride the demo allowance.
    assert_eq!(report["uploaded"], 2, "{report}");
    let sessions = report["sessions"].as_array().expect("sessions");
    let moved = sessions
        .iter()
        .find(|session| session["session_id"] == MOVED_SESSION)
        .expect("the moved session is reported");
    assert_eq!(moved["status"], "skipped", "{report}");
    for request in mock.requests() {
        assert!(
            !request.body_text().contains(MOVED_SESSION),
            "the moved session rode the demo repo's allowance"
        );
    }

    // Both ends allowed: the same session now goes out, under its first repo.
    // The demo sessions re-upload byte-identically, so they read back as
    // unchanged; the moved session and the side-repo one are the new rows.
    let allowed = allow(&home, SIDE_REPO);
    assert!(allowed.status.success(), "stderr: {}", allowed.stderr);
    let second = sync(&home, &api_envs(&mock.base), &[]);
    assert!(second.status.success(), "stderr: {}", second.stderr);
    let report = json_of(&second);
    assert_eq!(report["uploaded"], 2, "{report}");
    assert_eq!(report["unchanged"], 2, "{report}");
    assert_eq!(report["skipped"], 0, "{report}");
    let moved_body = source_bodies(&mock)
        .iter()
        .find(|envelope| envelope["body"].as_str().unwrap().contains(MOVED_SESSION))
        .expect("the moved session was uploaded")
        .clone();
    let record = record_of(&moved_body);
    // The temp HOME leaves nothing to elide, so the redactor's home-path
    // class catches the recorded `/home/alice` on this machine instead.
    assert_eq!(
        record["repo"], "/home/[REDACTED:home_path]/code/demo",
        "{record}"
    );
}

/// (f) Both formats land as one normalised record: the versioned schema
/// fields, the hand-computed totals (the Claude split message counted once,
/// the Codex cumulative count taken once, the wrapped bootstrap context
/// skipped), and each tool call with its input and result. This run has a
/// real-shaped `HOME`,
/// so the repo fields come out home-elided.
#[test]
fn claude_and_codex_sessions_land_in_the_normalised_schema() {
    let mock = sources_mock();
    let home = temp_dir("logs-home");
    plant(&home);
    let allowed = allow(&home, DEMO_REPO);
    assert!(allowed.status.success(), "stderr: {}", allowed.stderr);

    let extra = fixture_dirs(&home);
    let extra: Vec<&str> = extra.iter().map(String::as_str).collect();
    let output = sync(&home, &real_home_envs(&mock.base), &extra);
    assert!(output.status.success(), "stderr: {}", output.stderr);

    let mut claude = None;
    let mut codex = None;
    for envelope in source_bodies(&mock) {
        assert_eq!(envelope["scope"], "personal");
        let record = record_of(&envelope);
        match record["agent"].as_str().expect("an agent") {
            "claude_code" => claude = Some(record),
            "codex" => codex = Some(record),
            other => panic!("an unknown agent on the wire: {other}"),
        }
    }
    let (claude, codex) = (
        claude.expect("the claude session"),
        codex.expect("the codex session"),
    );

    // Claude Code: one assistant message streamed as two lines, counted once.
    assert_eq!(claude["schema_version"], 2);
    assert_eq!(claude["collector_version"], 2);
    assert_eq!(claude["detail"], "full");
    assert_eq!(claude["session_id"], CLAUDE_SESSION);
    // Elision ran with the real-shaped HOME, so the repo folds to `~` before
    // the body is built — no raw path, and no redaction finding for one.
    assert_eq!(claude["repo"], "~/code/demo", "{claude}");
    assert_eq!(claude["branch"], "main");
    assert_eq!(claude["models"], json!(["claude-sonnet-4-5"]));
    assert_eq!(claude["started_at"], "2026-10-01T09:30:00.000Z");
    assert_eq!(claude["ended_at"], "2026-10-01T09:30:08.000Z");
    assert_eq!(
        claude["totals"],
        json!({
            "input_tokens": 120,
            "output_tokens": 45,
            "cache_read_tokens": 30,
            "cache_creation_tokens": 15,
            "reasoning_tokens": 0,
            "cost_usd": 0.012,
        })
    );
    let claude_turns = claude["turns"].as_array().expect("turns");
    assert_eq!(claude_turns.len(), 2, "{claude}");
    assert_eq!(claude_turns[0]["role"], "user");
    assert_eq!(
        claude_turns[1]["tool_calls"],
        json!([{
            "name": "Bash",
            "format": "shell",
            "input": "ls ~/.claude/projects",
            "output": "demo\nside",
            "status": "ok",
        }])
    );
    assert_eq!(claude["outcome"], Value::Null);
    assert_eq!(claude["truncated"], false);
    // The redaction happened before the body was built, so the record's text
    // already carries the token's replacement.
    assert!(
        claude_turns[0]["text"]
            .as_str()
            .expect("text")
            .contains("[REDACTED:anthropic_key]"),
        "{}",
        claude_turns[0]
    );
    assert!(!claude.to_string().contains(planted_key().as_str()));

    // Codex: the last cumulative token count, no cost invented.
    assert_eq!(codex["schema_version"], 2);
    assert_eq!(codex["session_id"], CODEX_SESSION);
    assert_eq!(codex["repo"], "~/code/demo", "{codex}");
    assert_eq!(codex["branch"], "main");
    assert_eq!(codex["models"], json!(["gpt-5.1-codex"]));
    assert_eq!(
        codex["totals"],
        json!({
            "input_tokens": 800,
            "output_tokens": 120,
            "cache_read_tokens": 96,
            "cache_creation_tokens": 0,
            "reasoning_tokens": 40,
            "cost_usd": Value::Null,
        })
    );
    let codex_turns = codex["turns"].as_array().expect("turns");
    assert_eq!(codex_turns.len(), 3, "{codex}");
    assert_eq!(
        codex_turns[1]["tool_calls"],
        json!([{
            "name": "shell",
            "format": "shell",
            "input": "ls",
            "output": "README.md\nsrc\n",
        }])
    );
    // The planted AWS key is left as a replacement token, not as itself,
    // and the wrapped `<user_instructions>` opener made no turn.
    let codex_text = codex.to_string();
    assert!(!codex_text.contains(planted_aws_key().as_str()));
    assert!(
        !codex_text.contains("user_instructions"),
        "bootstrap context leaked: {codex_text}"
    );
    assert!(
        codex_text.contains("[REDACTED:aws_access_key]"),
        "{codex_text}"
    );
}

/// (g) The opt-in itself: `allow` is idempotent, `status` reads back the
/// file, `deny` revokes — and a sync after `deny` closes the socket again.
#[test]
fn allow_status_and_deny_manage_the_opt_in() {
    let mock = sources_mock();
    let home = temp_dir("logs-home");
    plant(&home);
    let envs = api_envs(&mock.base);

    let empty = run(
        &temp_dir("logs-cwd"),
        &home,
        &["logs", "status", "--json"],
        &[],
        None,
    );
    assert!(empty.status.success(), "stderr: {}", empty.stderr);
    assert_eq!(json_of(&empty)["allowed"], json!([]));

    let first = allow(&home, DEMO_REPO);
    assert_eq!(json_of(&first)["changed"], true, "{}", first.stdout);
    let second = allow(&home, DEMO_REPO);
    assert_eq!(
        json_of(&second)["changed"],
        false,
        "allow is idempotent: {}",
        second.stdout
    );

    let status = run(
        &temp_dir("logs-cwd"),
        &home,
        &["logs", "status", "--json"],
        &[],
        None,
    );
    assert_eq!(
        json_of(&status)["allowed"],
        json!([DEMO_REPO]),
        "the allowlist reads back sorted and canonical"
    );

    let denied = run(
        &temp_dir("logs-cwd"),
        &home,
        &["logs", "deny", DEMO_REPO, "--json"],
        &[],
        None,
    );
    assert!(json_of(&denied)["changed"].as_bool().expect("a flag"));

    let output = sync(&home, &envs, &[]);
    assert_eq!(json_of(&output)["uploaded"], 0, "{:?}", output.stdout);
    assert!(
        mock.requests().is_empty(),
        "a sync after `logs deny` still touched the network"
    );
}

/// `--dry-run` reports exactly what would have been sent and touches nothing.
#[test]
fn a_dry_run_sends_nothing() {
    let mock = sources_mock();
    let home = temp_dir("logs-home");
    plant(&home);
    let allowed = allow(&home, DEMO_REPO);
    assert!(allowed.status.success(), "stderr: {}", allowed.stderr);

    let output = sync(&home, &api_envs(&mock.base), &["--dry-run"]);
    assert!(output.status.success(), "stderr: {}", output.stderr);
    let report = json_of(&output);
    assert_eq!(report["dry_run"], true, "{report}");
    assert_eq!(report["uploaded"], 0);
    for session in report["sessions"].as_array().expect("sessions") {
        if session["status"] != "skipped" {
            assert_eq!(session["status"], "would_upload", "{report}");
        }
    }
    // The notes the run would write are reported too, by slug and project,
    // without a byte of them sent.
    assert_eq!(report["notes"], json!({ "would_write": 2 }), "{report}");
    let claude = report["sessions"]
        .as_array()
        .expect("sessions")
        .iter()
        .find(|session| session["session_id"] == CLAUDE_SESSION)
        .expect("the claude session is reported");
    assert_eq!(
        claude["note"],
        json!({
            "slug": format!("agent-log-claude-code-{CLAUDE_SESSION}"),
            "project": "demo",
            "status": "would_write",
        }),
        "{report}"
    );
    assert!(
        mock.requests().is_empty(),
        "a dry run still touched the network"
    );
}

/// (h) Every uploaded session also gets a searchable note: keyed by agent
/// and session, filed under the repo's basename as its project, built from
/// the same elided and redacted record, citing the source the upload
/// answered with. A session that did not upload gets no note.
#[test]
fn each_uploaded_session_gets_a_note_filed_under_its_repo() {
    let mock = sources_mock();
    let home = temp_dir("logs-home");
    plant(&home);
    let allowed = allow(&home, DEMO_REPO);
    assert!(allowed.status.success(), "stderr: {}", allowed.stderr);

    let extra = fixture_dirs(&home);
    let extra: Vec<&str> = extra.iter().map(String::as_str).collect();
    let output = sync(&home, &real_home_envs(&mock.base), &extra);
    assert!(output.status.success(), "stderr: {}", output.stderr);
    let report = json_of(&output);
    assert_eq!(report["notes"], json!({ "written": 2 }), "{report}");

    let notes = note_bodies(&mock);
    assert_eq!(notes.len(), 2, "one note per uploaded session: {notes:?}");
    let mut slugs: Vec<&str> = notes
        .iter()
        .map(|note| note["slug"].as_str().expect("a slug"))
        .collect();
    slugs.sort();
    assert_eq!(
        slugs,
        [
            format!("agent-log-claude-code-{CLAUDE_SESSION}"),
            format!("agent-log-codex-{CODEX_SESSION}"),
        ],
        "{notes:?}"
    );
    for note in &notes {
        assert_eq!(note["project"], "demo", "{note}");
        let body = note["body"].as_str().expect("a body");
        assert!(body.contains("- Repo: ~/code/demo"), "{body}");
        assert!(body.contains("- Source: src-"), "cites the source: {body}");
        assert!(body.contains("## Conversation"), "{body}");
        assert!(!body.contains(planted_key().as_str()), "{body}");
        assert!(!body.contains(planted_aws_key().as_str()), "{body}");
        assert!(!body.contains("/home/alice"), "{body}");
        assert!(!body.contains(SIDE_SESSION), "{body}");
    }
    let claude = notes
        .iter()
        .find(|note| note["body"].as_str().unwrap().contains(CLAUDE_SESSION))
        .expect("the claude note");
    assert_eq!(
        claude["title"],
        format!("claude_code session {CLAUDE_SESSION}")
    );
    let body = claude["body"].as_str().expect("a body");
    assert!(body.contains("- Branch: main"), "{body}");
    assert!(body.contains("- Models: claude-sonnet-4-5"), "{body}");
    assert!(body.contains("- Tool calls: 1 (Bash ×1)"), "{body}");
    assert!(
        body.contains("- Tokens: 120 in, 45 out, 30 cache read, 15 cache write, 0 reasoning"),
        "{body}"
    );
    assert!(body.contains("**User**"), "{body}");
    assert!(body.contains("- `Bash` · ok"), "{body}");
    assert!(body.contains("```sh\nls ~/.claude/projects\n```"), "{body}");
    assert!(body.contains("- Detail: full"), "{body}");
    assert!(body.contains("[REDACTED:anthropic_key]"), "{body}");

    // The report names each note by where it landed.
    let sessions = report["sessions"].as_array().expect("sessions");
    for session in sessions {
        if session["status"] == "skipped" {
            assert!(session.get("note").is_none(), "{session}");
        } else {
            assert_eq!(session["note"]["status"], "written", "{session}");
            assert_eq!(session["note"]["project"], "demo", "{session}");
        }
    }
}

/// (i) Notes are idempotent: a second sync of the same sessions sends the
/// same notes to the same slugs, and the server's `unchanged` answer is
/// what the report says — no second page, no new version.
#[test]
fn re_syncing_leaves_the_notes_unchanged() {
    let mock = sources_mock();
    let home = temp_dir("logs-home");
    plant(&home);
    let allowed = allow(&home, DEMO_REPO);
    assert!(allowed.status.success(), "stderr: {}", allowed.stderr);

    let first = sync(&home, &api_envs(&mock.base), &[]);
    assert!(first.status.success(), "stderr: {}", first.stderr);
    assert_eq!(json_of(&first)["notes"], json!({ "written": 2 }));
    let first_notes = note_bodies(&mock);

    let second = sync(&home, &api_envs(&mock.base), &[]);
    assert!(second.status.success(), "stderr: {}", second.stderr);
    let report = json_of(&second);
    assert_eq!(report["notes"], json!({ "unchanged": 2 }), "{report}");
    let second_notes: Vec<Value> = note_bodies(&mock)
        .into_iter()
        .skip(first_notes.len())
        .collect();
    let key = |note: &Value| note["slug"].as_str().unwrap_or("").to_owned();
    let mut a = first_notes.clone();
    let mut b = second_notes.clone();
    a.sort_by_key(key);
    b.sort_by_key(key);
    assert_eq!(
        a, b,
        "the re-sync sent different notes for the same sessions"
    );
}

/// The planted Claude session file for `session_id`, under the test home.
fn claude_file(home: &Path, project_dir: &str, session_id: &str) -> String {
    home.join(".claude")
        .join("projects")
        .join(project_dir)
        .join(format!("{session_id}.jsonl"))
        .to_string_lossy()
        .into_owned()
}

/// (j) `--session` narrows the scan to the named file: one session scanned,
/// one uploaded, the others never read — and the report carries the local
/// fields a client needs to list it (file, branch, times).
#[test]
fn sync_with_session_scans_only_that_file() {
    let mock = sources_mock();
    let home = temp_dir("logs-home");
    plant(&home);
    let allowed = allow(&home, DEMO_REPO);
    assert!(allowed.status.success(), "stderr: {}", allowed.stderr);

    let file = claude_file(&home, "-home-alice-code-demo", CLAUDE_SESSION);
    let output = sync(&home, &api_envs(&mock.base), &["--session", &file]);
    assert!(output.status.success(), "stderr: {}", output.stderr);
    let report = json_of(&output);
    assert_eq!(report["scanned"], 1, "{report}");
    assert_eq!(report["uploaded"], 1, "{report}");
    let session = &report["sessions"][0];
    assert_eq!(session["session_id"], CLAUDE_SESSION, "{session}");
    assert_eq!(session["branch"], "main", "{session}");
    assert_eq!(
        session["started_at"], "2026-10-01T09:30:00.000Z",
        "{session}"
    );
    assert!(session["ended_at"].is_string(), "{session}");
    assert!(
        session["file"]
            .as_str()
            .is_some_and(|path| path.ends_with(&format!("{CLAUDE_SESSION}.jsonl"))),
        "{session}"
    );
    assert_eq!(
        source_bodies(&mock).len(),
        1,
        "only the named session went out"
    );
}

/// (k) `--session` never widens the opt-in: naming a session from a repo
/// that is not allowed reports it skipped and sends nothing.
#[test]
fn sync_with_session_still_honours_the_opt_in() {
    let mock = sources_mock();
    let home = temp_dir("logs-home");
    plant(&home);
    let allowed = allow(&home, DEMO_REPO);
    assert!(allowed.status.success(), "stderr: {}", allowed.stderr);

    let file = claude_file(&home, "-home-alice-code-side", SIDE_SESSION);
    let output = sync(&home, &api_envs(&mock.base), &["--session", &file]);
    assert!(output.status.success(), "stderr: {}", output.stderr);
    let report = json_of(&output);
    assert_eq!(report["scanned"], 1, "{report}");
    assert_eq!(report["skipped"], 1, "{report}");
    assert!(
        mock.requests().is_empty(),
        "a skipped --session sync touched the network"
    );
}

/// (l) A `--session` path that is missing, or that is not under either
/// agent's tree, is an error, not a silent empty scan.
#[test]
fn sync_with_a_stray_session_is_an_error() {
    let mock = sources_mock();
    let home = temp_dir("logs-home");
    plant(&home);

    let missing = home.join("nope.jsonl").to_string_lossy().into_owned();
    let output = sync(&home, &api_envs(&mock.base), &["--session", &missing]);
    assert!(!output.status.success());
    assert!(output.stderr.contains("--session"), "{}", output.stderr);

    let outside = home.join("elsewhere.jsonl");
    std::fs::write(&outside, "{}\n").expect("write a stray file");
    let outside = outside.to_string_lossy().into_owned();
    let output = sync(&home, &api_envs(&mock.base), &["--session", &outside]);
    assert!(!output.status.success());
    assert!(
        output.stderr.contains("not a session file"),
        "{}",
        output.stderr
    );
    assert!(mock.requests().is_empty());
}

/// (m) `logs show` prints the redacted record for one file — opted in or
/// not — with no request at all: the planted key and the home never appear,
/// and the record is the one a sync would send.
#[test]
fn show_prints_the_redacted_record_locally() {
    let mock = sources_mock();
    let home = temp_dir("logs-home");
    plant(&home);

    let file = claude_file(&home, "-home-alice-code-side", SIDE_SESSION);
    let cwd = temp_dir("logs-cwd");
    // `real_home_envs` moves HOME away from the planted tree, so the trees
    // are named explicitly.
    let mut args = vec![
        "logs".to_owned(),
        "show".to_owned(),
        file.clone(),
        "--json".to_owned(),
    ];
    args.extend(fixture_dirs(&home));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = run(&cwd, &home, &args, &real_home_envs(&mock.base), None);
    assert!(output.status.success(), "stderr: {}", output.stderr);
    let shown = json_of(&output);
    assert_eq!(shown["opted_in"], false, "{shown}");
    assert_eq!(shown["record"]["session_id"], SIDE_SESSION, "{shown}");
    let body = shown["body"]
        .as_str()
        .expect("the exact body is printed too");
    let reparsed: Value = serde_json::from_str(body).expect("the fixture body parses");
    assert_eq!(reparsed, shown["record"], "record is the parse of body");
    let text = shown["record"].to_string();
    assert!(!text.contains(planted_key().as_str()), "{text}");
    assert!(!text.contains("/home/alice"), "{text}");
    assert!(mock.requests().is_empty(), "logs show touched the network");
}

/// Plant the full-detail fixtures — tool calls with results, a long output,
/// boilerplate, interruptions, secrets in tool output — where the agents
/// would have written them, and return their paths (claude, codex).
fn plant_full(home: &Path) -> (String, String) {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fixtures = manifest.join("tests/fixtures/logs-full");
    let claude_dir = home.join(".claude/projects/-home-alice-code-demo");
    let codex_dir = home.join(".codex/sessions/2026/10/02");
    std::fs::create_dir_all(&claude_dir).expect("claude dir");
    std::fs::create_dir_all(&codex_dir).expect("codex dir");
    let claude = claude_dir.join(format!("{FULL_CLAUDE_SESSION}.jsonl"));
    let codex = codex_dir.join(format!(
        "rollout-2026-10-02T09-00-00-{FULL_CODEX_SESSION}.jsonl"
    ));
    for (from, to) in [
        (format!("claude-{FULL_CLAUDE_SESSION}.jsonl"), &claude),
        (format!("codex-{FULL_CODEX_SESSION}.jsonl"), &codex),
    ] {
        let raw = std::fs::read_to_string(fixtures.join(from)).expect("read fixture");
        let planted = raw
            .replace(PLANTED_KEY_PLACEHOLDER, &planted_key())
            .replace(PLANTED_AWS_KEY_PLACEHOLDER, &planted_aws_key());
        std::fs::write(to, planted).expect("plant fixture");
    }
    (
        claude.to_string_lossy().into_owned(),
        codex.to_string_lossy().into_owned(),
    )
}

/// The full-detail fixture session ids.
const FULL_CLAUDE_SESSION: &str = "5e1f2a3b-4c5d-4e6f-8a7b-9c0d1e2f3a4b";
const FULL_CODEX_SESSION: &str = "b2c3d4e5-f6a7-4b8c-9d0e-1f2a3b4c5d6e";

/// (n) At `full`, the whole conversation travels — tool inputs and clipped
/// results in order — with the secrets inside tool output redacted on this
/// machine, in both the source and the note, and every body still JSON.
#[test]
fn full_detail_sends_tool_calls_with_secrets_redacted() {
    let mock = sources_mock();
    let home = temp_dir("logs-home");
    plant(&home);
    let (claude, codex) = plant_full(&home);
    let allowed = allow(&home, DEMO_REPO);
    assert!(allowed.status.success(), "stderr: {}", allowed.stderr);

    let output = sync(
        &home,
        &api_envs(&mock.base),
        &["--session", &claude, "--session", &codex],
    );
    assert!(output.status.success(), "stderr: {}", output.stderr);
    let report = json_of(&output);
    assert_eq!(report["uploaded"], 2, "{report}");
    assert!(report["redacted"].as_u64().unwrap() >= 3, "{report}");
    for session in report["sessions"].as_array().expect("sessions") {
        assert_eq!(session["detail"], "full", "{session}");
    }

    let wire: String = mock
        .requests()
        .iter()
        .map(common::Request::body_text)
        .collect();
    assert!(
        !wire.contains(planted_key().as_str()),
        "anthropic key on the wire"
    );
    assert!(
        !wire.contains(planted_aws_key().as_str()),
        "aws key on the wire"
    );
    assert!(!wire.contains("bob@example.com"), "address on the wire");
    assert!(
        !wire.contains("encryptedreasoning"),
        "encrypted reasoning on the wire"
    );

    let records: Vec<Value> = source_bodies(&mock).iter().map(record_of).collect();
    let claude = records
        .iter()
        .find(|record| record["session_id"] == FULL_CLAUDE_SESSION)
        .expect("the claude session");
    let calls: Vec<&Value> = claude["turns"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|turn| turn["tool_calls"].as_array().unwrap())
        .collect();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0]["input"], "cargo test -p demo");
    assert_eq!(calls[0]["exit_code"], 101);
    assert!(
        calls[0]["output"]
            .as_str()
            .unwrap()
            .contains("bytes omitted")
    );
    assert_eq!(calls[1]["name"], "Edit");
    assert!(calls[2]["output"].as_str().unwrap().contains("[REDACTED:"));
    let roles: Vec<&str> = claude["turns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|turn| turn["role"].as_str().unwrap())
        .collect();
    assert!(
        roles.contains(&"reasoning") && roles.contains(&"event"),
        "{roles:?}"
    );

    let codex = records
        .iter()
        .find(|record| record["session_id"] == FULL_CODEX_SESSION)
        .expect("the codex session");
    assert!(
        !codex.to_string().contains("AGENTS.md"),
        "bootstrap leaked: {codex}"
    );

    for note in note_bodies(&mock) {
        let body = note["body"].as_str().expect("a body");
        assert!(body.contains("- `"), "{body}");
        assert!(!body.contains(planted_key().as_str()), "{body}");
        assert!(!body.contains(planted_aws_key().as_str()), "{body}");
    }
}

/// (o) `--detail messages` keeps every message and drops tool I/O; the
/// level is stored per path, reported by `status`, kept by a bare
/// re-`allow`, and changed by an explicit one.
#[test]
fn the_detail_level_is_per_repository() {
    let mock = sources_mock();
    let home = temp_dir("logs-home");
    plant(&home);
    let (claude, _) = plant_full(&home);
    let cwd = temp_dir("logs-cwd");
    let allowed = run(
        &cwd,
        &home,
        &["logs", "allow", DEMO_REPO, "--detail", "messages", "--json"],
        &[],
        None,
    );
    assert!(allowed.status.success(), "stderr: {}", allowed.stderr);
    assert_eq!(json_of(&allowed)["detail"][DEMO_REPO], "messages");

    let again = allow(&home, DEMO_REPO);
    assert_eq!(json_of(&again)["changed"], false, "{}", again.stdout);
    assert_eq!(json_of(&again)["detail"][DEMO_REPO], "messages");
    let status = run(&cwd, &home, &["logs", "status", "--json"], &[], None);
    assert_eq!(json_of(&status)["detail"][DEMO_REPO], "messages");

    let output = sync(&home, &api_envs(&mock.base), &["--session", &claude]);
    assert!(output.status.success(), "stderr: {}", output.stderr);
    let record = record_of(&source_bodies(&mock)[0]);
    assert_eq!(record["detail"], "messages");
    let text = record.to_string();
    assert!(!text.contains("cargo test -p demo"), "{text}");
    assert!(!text.contains("\"output\""), "{text}");
    assert!(text.contains("Fixed: the assertion"), "{text}");

    let full = run(
        &cwd,
        &home,
        &["logs", "allow", DEMO_REPO, "--detail", "full", "--json"],
        &[],
        None,
    );
    assert_eq!(json_of(&full)["changed"], true, "{}", full.stdout);
}

/// (p) Upgrading re-sends what changed: a session synced at one level and
/// synced again after its record changed (as the schema bump changes every
/// record) uploads a new source and replaces its note in place — the same
/// slug, a new version — rather than being skipped as already there.
#[test]
fn a_changed_record_re_uploads_and_replaces_its_note() {
    let mock = sources_mock();
    let home = temp_dir("logs-home");
    plant(&home);
    let cwd = temp_dir("logs-cwd");
    let summary = run(
        &cwd,
        &home,
        &["logs", "allow", DEMO_REPO, "--detail", "summary", "--json"],
        &[],
        None,
    );
    assert!(summary.status.success(), "stderr: {}", summary.stderr);
    let first = sync(&home, &api_envs(&mock.base), &[]);
    assert_eq!(json_of(&first)["uploaded"], 2, "{}", first.stdout);

    run(
        &cwd,
        &home,
        &["logs", "allow", DEMO_REPO, "--detail", "full", "--json"],
        &[],
        None,
    );
    let second = sync(&home, &api_envs(&mock.base), &[]);
    let report = json_of(&second);
    assert_eq!(report["uploaded"], 2, "{report}");
    assert_eq!(report["notes"], json!({ "written": 2 }), "{report}");
    let notes = note_bodies(&mock);
    let first_slugs: HashSet<&str> = notes[..2]
        .iter()
        .map(|n| n["slug"].as_str().unwrap())
        .collect();
    let second_slugs: HashSet<&str> = notes[2..]
        .iter()
        .map(|n| n["slug"].as_str().unwrap())
        .collect();
    assert_eq!(
        first_slugs, second_slugs,
        "the same pages, updated in place"
    );
}
