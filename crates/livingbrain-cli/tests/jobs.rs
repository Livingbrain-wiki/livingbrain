//! `livingbrain jobs`: the local runner, against a mock job server and fake
//! `claude` / `codex` binaries that record how they were called.
//!
//! The fakes write what they saw — argv, a digest of stdin, their working
//! directory, whether the Living Brain token reached their environment —
//! into a directory the test reads. No test prints a prompt.

#![cfg(unix)]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value;
use sha2::{Digest, Sha256};

use common::{Mock, Output, Reply, run, temp_dir};

const TOKEN: &str = "tok-test-bearer-1234567890";
const SYSTEM: &str = "You output JSON only.";
const PROMPT: &str = "Compile this session: a private transcript line.";

/// Every flag that would let a job do more than produce text.
const FORBIDDEN: &[&str] = &[
    "--dangerously-skip-permissions",
    "--allow-dangerously-skip-permissions",
    "bypassPermissions",
    "--dangerously-bypass-approvals-and-sandbox",
    "danger-full-access",
    "workspace-write",
];

/// A job as the server hands it out.
fn job(agent: &str, model: &str) -> String {
    serde_json::json!({
        "job": {
            "id": "job-0001-abcdef",
            "kind": "compile",
            "agent": agent,
            "model": model,
            "settings": "isolated",
            "system": SYSTEM,
            "user": PROMPT,
            "max_tokens": 6000,
            "json": true,
            "lease_until": "2026-10-15T10:10:00Z",
        }
    })
    .to_string()
}

/// A mock that hands out `job` once, then has nothing (204); records posts.
fn server(job: String) -> Mock {
    let handed = Arc::new(AtomicUsize::new(0));
    Mock::start(
        move |request| match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/v1/model-jobs/next") => {
                if handed.fetch_add(1, Ordering::SeqCst) == 0 {
                    Reply::ok_json(&job)
                } else {
                    Reply::bytes(204, "application/json", Vec::new())
                }
            }
            ("POST", path) if path.ends_with("/result") => Reply::ok_json(r#"{"status":"done"}"#),
            ("POST", path) if path.ends_with("/fail") => Reply::ok_json(r#"{"status":"failed"}"#),
            _ => Reply::json(404, r#"{"error":"no such route"}"#),
        },
    )
}

/// Writes an executable shell script.
fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}")).expect("write fake");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

/// The recorder every fake starts with: argv one per line, stdin into a
/// file (read back only as a digest), cwd, and whether our token leaked.
const RECORD: &str = r#"
: > "$FAKE_OUT/args"
for a in "$@"; do printf '%s\n' "$a" >> "$FAKE_OUT/args"; done
cat > "$FAKE_OUT/stdin"
pwd > "$FAKE_OUT/cwd"
if [ -n "$LIVINGBRAIN_TOKEN" ]; then echo leaked > "$FAKE_OUT/token"; fi
"#;

const CLAUDE_OK: &str = r#"echo '{"type":"result","subtype":"success","is_error":false,"result":"{\"pages\":[]}","usage":{"input_tokens":3,"cache_creation_input_tokens":100,"cache_read_input_tokens":20,"output_tokens":7}}'"#;

fn sha(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

fn envs<'a>(base: &'a str, out: &'a str, path: &'a str) -> Vec<(&'a str, &'a str)> {
    vec![
        ("LIVINGBRAIN_API_URL", base),
        ("LIVINGBRAIN_TOKEN", TOKEN),
        ("FAKE_OUT", out),
        ("PATH", path),
    ]
}

fn lines(output: &Output) -> Vec<Value> {
    output
        .stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line:?}")))
        .collect()
}

fn posted(mock: &Mock, suffix: &str) -> Vec<Value> {
    mock.requests()
        .into_iter()
        .filter(|r| r.method == "POST" && r.path.ends_with(suffix))
        .map(|r| {
            assert_eq!(r.bearer().as_deref(), Some(TOKEN));
            serde_json::from_slice(&r.body).expect("JSON body")
        })
        .collect()
}

/// A claimed Claude job runs the CLI with no tools, the prompt on stdin, in
/// an empty directory of its own, without our token — and the answer and its
/// tokens are posted back.
#[test]
fn a_claude_job_is_run_and_its_answer_posted() {
    let mock = server(job("claude", "sonnet"));
    let (cwd, home, bin, out) = (
        temp_dir("jobs-cwd"),
        temp_dir("jobs-home"),
        temp_dir("jobs-bin"),
        temp_dir("jobs-out"),
    );
    script(&bin, "claude", &format!("{RECORD}\n{CLAUDE_OK}\n"));
    let path = format!("{}:/usr/bin:/bin", bin.display());
    let out_s = out.display().to_string();
    let output = run(
        &cwd,
        &home,
        &["--json", "jobs", "run", "--once"],
        &envs(&mock.base, &out_s, &path),
        None,
    );
    assert!(output.status.success(), "stderr: {}", output.stderr);
    let events = lines(&output);
    let names: Vec<&str> = events.iter().filter_map(|e| e["event"].as_str()).collect();
    assert_eq!(
        names,
        ["started", "claimed", "done", "stopped"],
        "{events:?}"
    );
    assert_eq!(events[1]["model"], "sonnet");
    assert_eq!(events[2]["prompt_tokens"], 123);
    assert!(!output.stdout.contains(PROMPT), "the prompt was printed");

    let results = posted(&mock, "/v1/model-jobs/job-0001-abcdef/result");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["text"], "{\"pages\":[]}");
    assert_eq!(results[0]["prompt_tokens"], 123);
    assert_eq!(results[0]["completion_tokens"], 7);

    let args = std::fs::read_to_string(out.join("args")).expect("args");
    let argv: Vec<&str> = args.lines().collect();
    for flag in FORBIDDEN {
        assert!(!args.contains(flag), "{flag} was passed");
    }
    let after = |flag: &str| {
        argv.iter()
            .position(|a| *a == flag)
            .map(|i| argv.get(i + 1).copied().unwrap_or("<none>"))
    };
    assert_eq!(argv[0], "-p");
    assert_eq!(after("--tools"), Some(""));
    assert_eq!(after("--setting-sources"), Some(""));
    assert_eq!(after("--max-turns"), Some("1"));
    assert_eq!(after("--model"), Some("sonnet"));
    assert_eq!(after("--system-prompt"), Some(SYSTEM));
    assert!(argv.contains(&"--strict-mcp-config"));
    let stdin = std::fs::read_to_string(out.join("stdin")).expect("stdin");
    assert_eq!(
        sha(&stdin),
        sha(PROMPT),
        "the prompt did not arrive on stdin"
    );
    let ran_in = std::fs::read_to_string(out.join("cwd")).expect("cwd");
    let ran_in = PathBuf::from(ran_in.trim());
    assert_ne!(ran_in, cwd);
    assert!(!ran_in.exists(), "the job's directory was left behind");
    assert!(
        !out.join("token").exists(),
        "LIVINGBRAIN_TOKEN reached the CLI"
    );
}

/// A Codex job runs `exec` read-only, with the instructions leading stdin,
/// and its answer comes from the last-message file.
#[test]
fn a_codex_job_is_run_read_only() {
    let mock = server(job("codex", "gpt-5.5"));
    let (cwd, home, bin, out) = (
        temp_dir("codex-cwd"),
        temp_dir("codex-home"),
        temp_dir("codex-bin"),
        temp_dir("codex-out"),
    );
    let body = format!(
        r#"{RECORD}
last=""
prev=""
for a in "$@"; do [ "$prev" = "-o" ] && last="$a"; prev="$a"; done
printf 'hello from codex' > "$last"
echo '{{"type":"thread.started"}}'
echo '{{"type":"turn.completed","usage":{{"input_tokens":40,"cached_input_tokens":4,"output_tokens":5}}}}'
"#
    );
    let codex = script(&bin, "codex", &body);
    let path = "/usr/bin:/bin".to_owned();
    let out_s = out.display().to_string();
    let codex_s = codex.display().to_string();
    let output = run(
        &cwd,
        &home,
        &["--json", "jobs", "run", "--once", "--codex", &codex_s],
        &envs(&mock.base, &out_s, &path),
        None,
    );
    assert!(output.status.success(), "stderr: {}", output.stderr);
    let results = posted(&mock, "/result");
    assert_eq!(results[0]["text"], "hello from codex");
    assert_eq!(results[0]["prompt_tokens"], 40);
    assert_eq!(results[0]["completion_tokens"], 5);
    let args = std::fs::read_to_string(out.join("args")).expect("args");
    for flag in FORBIDDEN {
        assert!(!args.contains(flag), "{flag} was passed");
    }
    let argv: Vec<&str> = args.lines().collect();
    assert_eq!(argv[0], "exec");
    assert!(argv.windows(2).any(|w| w == ["--sandbox", "read-only"]));
    assert!(argv.windows(2).any(|w| w == ["-m", "gpt-5.5"]));
    assert_eq!(argv.last(), Some(&"-"));
    let stdin = std::fs::read_to_string(out.join("stdin")).expect("stdin");
    assert_eq!(sha(&stdin), sha(&format!("{SYSTEM}\n\n{PROMPT}")));
}

/// A CLI that runs past `--timeout` is killed and the job reported failed.
#[test]
fn a_job_past_its_timeout_is_failed() {
    let mock = server(job("claude", ""));
    let (cwd, home, bin, out) = (
        temp_dir("slow-cwd"),
        temp_dir("slow-home"),
        temp_dir("slow-bin"),
        temp_dir("slow-out"),
    );
    script(&bin, "claude", "cat > /dev/null\nsleep 20\n");
    let path = format!("{}:/usr/bin:/bin", bin.display());
    let out_s = out.display().to_string();
    let started = std::time::Instant::now();
    let output = run(
        &cwd,
        &home,
        &["--json", "jobs", "run", "--once", "--timeout", "1"],
        &envs(&mock.base, &out_s, &path),
        None,
    );
    assert!(output.status.success(), "stderr: {}", output.stderr);
    assert!(started.elapsed().as_secs() < 15, "the CLI was not killed");
    let fails = posted(&mock, "/fail");
    assert_eq!(fails.len(), 1);
    assert_eq!(fails[0]["reason"], "timeout");
    assert!(posted(&mock, "/result").is_empty());
}

/// A CLI that exits non-zero with nothing readable is a `cli-error`; its
/// stderr goes nowhere.
#[test]
fn a_failing_cli_is_a_cli_error() {
    let mock = server(job("claude", "haiku"));
    let (cwd, home, bin, out) = (
        temp_dir("bad-cwd"),
        temp_dir("bad-home"),
        temp_dir("bad-bin"),
        temp_dir("bad-out"),
    );
    script(
        &bin,
        "claude",
        "cat > /dev/null\necho 'secret-ish detail from the CLI' >&2\nexit 3\n",
    );
    let path = format!("{}:/usr/bin:/bin", bin.display());
    let out_s = out.display().to_string();
    let output = run(
        &cwd,
        &home,
        &["--json", "jobs", "run", "--once"],
        &envs(&mock.base, &out_s, &path),
        None,
    );
    assert!(output.status.success(), "stderr: {}", output.stderr);
    let fails = posted(&mock, "/fail");
    assert_eq!(fails[0]["reason"], "cli-error");
    assert!(!output.stdout.contains("secret-ish"));
    assert!(!output.stderr.contains("secret-ish"));
    for request in mock.requests() {
        assert!(!request.body_text().contains("secret-ish"));
    }
}

/// An agent that is not installed fails the job as `cli-missing`.
#[test]
fn a_missing_cli_is_reported() {
    let mock = server(job("codex", ""));
    let (cwd, home, out) = (
        temp_dir("miss-cwd"),
        temp_dir("miss-home"),
        temp_dir("miss-out"),
    );
    let out_s = out.display().to_string();
    let output = run(
        &cwd,
        &home,
        &[
            "--json",
            "jobs",
            "run",
            "--once",
            "--codex",
            "/nonexistent/codex",
        ],
        &envs(&mock.base, &out_s, "/usr/bin:/bin"),
        None,
    );
    assert!(output.status.success(), "stderr: {}", output.stderr);
    assert_eq!(posted(&mock, "/fail")[0]["reason"], "cli-missing");
}

/// `--once` with nothing queued exits 0.
#[test]
fn once_with_no_job_exits_cleanly() {
    let mock = Mock::start(|_| Reply::bytes(204, "application/json", Vec::new()));
    let (cwd, home) = (temp_dir("none-cwd"), temp_dir("none-home"));
    let output = run(
        &cwd,
        &home,
        &["--json", "jobs", "run", "--once"],
        &[
            ("LIVINGBRAIN_API_URL", &mock.base),
            ("LIVINGBRAIN_TOKEN", TOKEN),
        ],
        None,
    );
    assert!(output.status.success(), "stderr: {}", output.stderr);
    let names: Vec<String> = lines(&output)
        .iter()
        .filter_map(|e| e["event"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(names, ["started", "stopped"]);
}

/// A signed-out token exits 2, like every other command.
#[test]
fn a_signed_out_token_exits_2() {
    let mock = Mock::start(|_| Reply::json(401, r#"{"title":"Unauthorized"}"#));
    let (cwd, home) = (temp_dir("401-cwd"), temp_dir("401-home"));
    let output = run(
        &cwd,
        &home,
        &["--json", "jobs", "run"],
        &[
            ("LIVINGBRAIN_API_URL", &mock.base),
            ("LIVINGBRAIN_TOKEN", TOKEN),
        ],
        None,
    );
    assert_eq!(output.status.code(), Some(2), "stderr: {}", output.stderr);
    assert!(
        output.stderr.contains("livingbrain login"),
        "{}",
        output.stderr
    );
}

/// `jobs agents` says which CLIs are there and signed in — never an email
/// or an org, even when the CLI's own status prints them.
#[test]
fn agents_reports_sign_in_without_identity() {
    let (cwd, home, bin) = (
        temp_dir("agents-cwd"),
        temp_dir("agents-home"),
        temp_dir("agents-bin"),
    );
    script(
        &bin,
        "claude",
        r#"case "$1" in
--version) echo "2.1.300 (Claude Code)";;
auth) echo '{"loggedIn":true,"authMethod":"claude.ai","email":"person@example.com","orgId":"org-123","orgName":"Acme","subscriptionType":"max"}';;
esac
"#,
    );
    let codex = script(
        &bin,
        "codex-fake",
        r#"case "$1" in
--version) echo "codex-cli 0.148.0";;
login) echo "Logged in using ChatGPT person@example.com" >&2;;
esac
"#,
    );
    let path = format!("{}:/usr/bin:/bin", bin.display());
    let codex_s = codex.display().to_string();
    let output = run(
        &cwd,
        &home,
        &["--json", "jobs", "agents", "--codex", &codex_s],
        &[("PATH", path.as_str())],
        None,
    );
    assert!(output.status.success(), "stderr: {}", output.stderr);
    let value: Value = serde_json::from_str(output.stdout.trim()).expect("JSON");
    let claude = &value["agents"][0];
    assert_eq!(claude["agent"], "claude");
    assert_eq!(claude["logged_in"], true);
    assert_eq!(claude["login"], "claude.ai (max)");
    assert_eq!(claude["version"], "2.1.300 (Claude Code)");
    let codex = &value["agents"][1];
    assert_eq!(codex["logged_in"], true);
    assert_eq!(codex["login"], "ChatGPT");
    for secret in ["person@example.com", "org-123", "Acme"] {
        assert!(!output.stdout.contains(secret), "{secret} was printed");
    }

    let human = run(
        &cwd,
        &home,
        &["jobs", "agents", "--codex", "/nonexistent/codex"],
        &[("PATH", path.as_str())],
        None,
    );
    assert!(human.stdout.contains("claude: 2.1.300"), "{}", human.stdout);
    assert!(
        human.stdout.contains("codex: not found"),
        "{}",
        human.stdout
    );
}
