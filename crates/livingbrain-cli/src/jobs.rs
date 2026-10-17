//! `livingbrain jobs` — the local runner: the brain's model calls, run by
//! the person's own Claude Code or Codex under their own subscription.
//!
//! **Why a runner.** A consumer subscription (Claude Pro/Max, ChatGPT
//! Plus/Pro) may only be used inside its vendor's own client. Its
//! credentials therefore never leave this machine and never reach the Living
//! Brain server: when a person sets "run my jobs on my device", the server
//! queues each model call as a job, and this command claims it, runs the
//! vendor's CLI with the job's prompt, and posts the text back. The server
//! redacts and validates the text again on arrival.
//!
//! **What the CLI may do.** Text in, text out, nothing else: Claude Code runs
//! with every tool off (`--tools ""`), no MCP servers, no settings, no slash
//! commands and one turn; Codex runs in its read-only sandbox, ephemeral,
//! with the user config ignored. Both run in a fresh empty directory. No
//! permission-bypass flag is ever passed — [`FORBIDDEN`] lists them, and the
//! argv builders are pure functions the tests check against it.
//!
//! **What leaves the machine.** The job's text answer and its token counts.
//! Never a prompt or an answer in a log line, never the CLIs' own stderr,
//! never their credentials or environment: a failure is a reason from a
//! fixed vocabulary ([`Reason`]). The Living Brain token is removed from the
//! child's environment.
//!
//! | Command | What it does |
//! | :--- | :--- |
//! | `jobs agents` | Which of `claude` / `codex` are installed, their versions and whether they are signed in (no email, no org) |
//! | `jobs run` | Claim and run the caller's jobs one at a time until stopped (`--once`: at most one) |

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Process, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use clap::{Args, Subcommand, ValueEnum};
use serde_json::{Value, json};

use crate::api::{Client, encode_segment};
use crate::{CliError, CliResult, Out, auth, err};

/// Flags that would let a job do more than produce text. None of them is
/// ever passed; a test asserts the argv builders never contain one.
pub const FORBIDDEN: &[&str] = &[
    "--dangerously-skip-permissions",
    "--allow-dangerously-skip-permissions",
    "bypassPermissions",
    "--dangerously-bypass-approvals-and-sandbox",
    "--dangerously-bypass-hook-trust",
    "danger-full-access",
    "workspace-write",
    "--approve-for-me",
];

/// The most of a CLI's stdout one job reads; past it the job fails.
const MAX_STDOUT: usize = 8 * 1024 * 1024;
/// How long `--version` / a login-status check may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
/// The longest wait between polls after repeated network errors.
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Which vendor CLI runs a job.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Agent {
    /// Claude Code (`claude -p`), under a Claude Pro/Max subscription.
    Claude,
    /// Codex (`codex exec`), under a ChatGPT Plus/Pro sign-in.
    Codex,
}

impl Agent {
    fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }
}

/// Where the two CLIs are, when not on `PATH`.
#[derive(Args, Debug, Clone, Default)]
pub struct Paths {
    /// The `claude` binary to use (default: found on PATH or in the usual install dirs)
    #[arg(long, value_name = "PATH")]
    claude: Option<PathBuf>,
    /// The `codex` binary to use (default: found on PATH or in the usual install dirs)
    #[arg(long, value_name = "PATH")]
    codex: Option<PathBuf>,
}

/// The `livingbrain jobs` subcommands.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Show which of Claude Code and Codex are installed and signed in
    Agents {
        #[command(flatten)]
        paths: Paths,
    },
    /// Run your brain jobs with your own Claude Code or Codex subscription
    Run {
        /// Use this CLI for every job (default: the one each job names)
        #[arg(long, value_enum)]
        agent: Option<Agent>,
        /// Run at most one job, then exit (0 even when there was none)
        #[arg(long)]
        once: bool,
        /// Seconds between polls while there is nothing to do
        #[arg(long, default_value_t = 5, value_name = "SECONDS")]
        poll: u64,
        /// Seconds one job's CLI may run before it is stopped
        #[arg(long, default_value_t = 300, value_name = "SECONDS")]
        timeout: u64,
        #[command(flatten)]
        paths: Paths,
    },
}

/// Runs a `jobs` subcommand.
pub fn run(command: &Command, api_url: &str, out: &Out) -> CliResult<()> {
    match command {
        Command::Agents { paths } => {
            let agents: Vec<Value> = [Agent::Claude, Agent::Codex]
                .into_iter()
                .map(|agent| describe(agent, paths))
                .collect();
            let value = json!({ "agents": agents });
            out.json_or(&value, || render_agents(&agents));
            Ok(())
        }
        Command::Run {
            agent,
            once,
            poll,
            timeout,
            paths,
        } => {
            let client = Client::new(api_url).with_token(auth::token_for(api_url)?);
            let runner = Runner {
                client,
                agent: *agent,
                paths: paths.clone(),
                timeout: Duration::from_secs((*timeout).max(1)),
                out,
            };
            runner.serve(*once, Duration::from_secs((*poll).max(1)))
        }
    }
}

// ---------------------------------------------------------------------------
// Finding the CLIs
// ---------------------------------------------------------------------------

/// The binary for `agent`: the override when given, else the first match on
/// `PATH`, else the usual install directories (a menu-bar app starts this
/// command with a short `PATH`).
fn locate(agent: Agent, paths: &Paths) -> Option<PathBuf> {
    let explicit = match agent {
        Agent::Claude => paths.claude.clone(),
        Agent::Codex => paths.codex.clone(),
    };
    if let Some(path) = explicit {
        return path.is_file().then_some(path);
    }
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        dirs.push(home.join(".local/bin"));
        dirs.push(home.join(".claude/local"));
        dirs.push(home.join(".npm-global/bin"));
    }
    dirs.push(PathBuf::from("/opt/homebrew/bin"));
    dirs.push(PathBuf::from("/usr/local/bin"));
    dirs.into_iter()
        .map(|dir| dir.join(agent.name()))
        .find(|candidate| candidate.is_file())
}

/// One agent's entry in `jobs agents`: found, path, version, signed in, and
/// how — never an email, an org or a token.
fn describe(agent: Agent, paths: &Paths) -> Value {
    let Some(path) = locate(agent, paths) else {
        return json!({ "agent": agent.name(), "found": false });
    };
    let version = capture(&path, &["--version"])
        .and_then(|(_, stdout, _)| stdout.lines().next().map(|l| l.trim().to_owned()))
        .unwrap_or_default();
    let (logged_in, login) = match agent {
        Agent::Claude => claude_login(&path),
        Agent::Codex => codex_login(&path),
    };
    json!({
        "agent": agent.name(),
        "found": true,
        "path": path.display().to_string(),
        "version": version,
        "logged_in": logged_in,
        "login": login,
    })
}

/// `claude auth status --json`, reduced to `loggedIn`, `authMethod` and
/// `subscriptionType`. The rest (email, org) is never read into the output.
fn claude_login(path: &Path) -> (bool, String) {
    let Some((_, stdout, _)) = capture(path, &["auth", "status", "--json"]) else {
        return (false, String::new());
    };
    let parsed: Value = serde_json::from_str(stdout.trim()).unwrap_or_default();
    let logged_in = parsed
        .get("loggedIn")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let word = |key: &str| {
        parsed
            .get(key)
            .and_then(Value::as_str)
            .filter(|v| v.len() <= 40 && !v.contains('@'))
            .map(str::to_owned)
    };
    let login = match (word("authMethod"), word("subscriptionType")) {
        (Some(method), Some(plan)) => format!("{method} ({plan})"),
        (Some(method), None) => method,
        (None, Some(plan)) => plan,
        (None, None) => String::new(),
    };
    (logged_in, if logged_in { login } else { String::new() })
}

/// `codex login status`: signed in when it says "Logged in", and how — the
/// first line after "Logged in using", with anything that looks like an
/// email dropped.
fn codex_login(path: &Path) -> (bool, String) {
    let Some((_, stdout, stderr)) = capture(path, &["login", "status"]) else {
        return (false, String::new());
    };
    let text = format!("{stdout}\n{stderr}");
    let Some(line) = text.lines().find(|line| line.contains("Logged in")) else {
        return (false, String::new());
    };
    let how = line
        .split_once("Logged in using")
        .map_or("", |(_, rest)| rest)
        .split_whitespace()
        .filter(|word| !word.contains('@'))
        .collect::<Vec<_>>()
        .join(" ");
    (true, how.chars().take(40).collect())
}

/// Runs a short probe (`--version`, a login status) with no stdin and a
/// timeout; `(exit success, stdout, stderr)`, or `None` when it would not
/// start or did not finish.
fn capture(path: &Path, args: &[&str]) -> Option<(bool, String, String)> {
    let mut command = Process::new(path);
    command.args(args).stdin(Stdio::null());
    // A probe of an agent CLI never needs, and never gets, a LivingBrain token.
    auth::scrub_inherited_tokens(&mut command);
    let finished = execute(command, None, PROBE_TIMEOUT, 64 * 1024).ok()?;
    Some((
        finished.success,
        String::from_utf8_lossy(&finished.stdout).into_owned(),
        String::from_utf8_lossy(&finished.stderr).into_owned(),
    ))
}

fn render_agents(agents: &[Value]) {
    for agent in agents {
        let name = agent["agent"].as_str().unwrap_or("");
        if agent["found"] != true {
            println!("{name}: not found");
            continue;
        }
        let state = if agent["logged_in"] == true {
            match agent["login"].as_str().unwrap_or("") {
                "" => "signed in".to_owned(),
                how => format!("signed in ({how})"),
            }
        } else {
            "not signed in".to_owned()
        };
        println!(
            "{name}: {} — {} — {state}",
            agent["version"].as_str().unwrap_or("?"),
            agent["path"].as_str().unwrap_or("")
        );
    }
}

// ---------------------------------------------------------------------------
// The invocations — pure, so the tests can hold them to the rules
// ---------------------------------------------------------------------------

/// Claude Code's argv (after the binary). The user turn goes on stdin.
///
/// `-p` with one JSON result, every built-in tool off, no MCP servers, no
/// slash commands, one turn and no saved session; the permission mode
/// refuses anything that would ask. `settings` `user` loads the person's own
/// Claude Code settings — the way Claude Code is pointed at Z.AI's GLM
/// Coding Plan — and anything else loads none.
#[must_use]
pub fn claude_argv(system: &str, model: Option<&str>, settings: Option<&str>) -> Vec<String> {
    let sources = if settings == Some("user") { "user" } else { "" };
    let mut argv: Vec<String> = [
        "-p",
        "--output-format",
        "json",
        "--tools",
        "",
        "--permission-mode",
        "dontAsk",
        "--strict-mcp-config",
        "--setting-sources",
        sources,
        "--disable-slash-commands",
        "--no-session-persistence",
        "--max-turns",
        "1",
        "--system-prompt",
        system,
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    if let Some(model) = model.filter(|m| !m.is_empty()) {
        argv.push("--model".to_owned());
        argv.push(model.to_owned());
    }
    argv
}

/// Codex's argv (after the binary): `exec` in the read-only sandbox,
/// ephemeral, the user config and rules ignored, JSONL events on stdout and
/// the last message written to `last`. The prompt is read from stdin (`-`).
#[must_use]
pub fn codex_argv(dir: &Path, last: &Path, model: Option<&str>) -> Vec<String> {
    let mut argv: Vec<String> = [
        "exec",
        "--sandbox",
        "read-only",
        "--skip-git-repo-check",
        "--ephemeral",
        "--ignore-user-config",
        "--ignore-rules",
        "--json",
        "--color",
        "never",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    argv.push("-C".to_owned());
    argv.push(dir.display().to_string());
    argv.push("-o".to_owned());
    argv.push(last.display().to_string());
    if let Some(model) = model.filter(|m| !m.is_empty()) {
        argv.push("-m".to_owned());
        argv.push(model.to_owned());
    }
    argv.push("-".to_owned());
    argv
}

/// Codex has no system-prompt flag, so the job's instructions lead its
/// stdin, ahead of the job's data.
#[must_use]
pub fn codex_stdin(system: &str, user: &str) -> String {
    format!("{system}\n\n{user}")
}

/// Why a job did not produce text — the whole vocabulary a failure is
/// reported in, locally and to the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    Timeout,
    CliMissing,
    NotLoggedIn,
    CliError,
    BadOutput,
}

impl Reason {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::CliMissing => "cli-missing",
            Self::NotLoggedIn => "not-logged-in",
            Self::CliError => "cli-error",
            Self::BadOutput => "bad-output",
        }
    }
}

/// A job's answer.
#[derive(Debug, PartialEq, Eq)]
pub struct Answer {
    pub text: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

/// Words in an error a CLI reports when it is not signed in.
fn sounds_signed_out(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "not logged in",
        "/login",
        "log in",
        "login",
        "unauthorized",
        "401",
        "invalid api key",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// Claude Code's `--output-format json` result.
///
/// # Errors
///
/// [`Reason::NotLoggedIn`] for a sign-in error, [`Reason::CliError`] for any
/// other error result, [`Reason::BadOutput`] when it is not the result object.
pub fn parse_claude(stdout: &str) -> Result<Answer, Reason> {
    let parsed: Value = serde_json::from_str(stdout.trim()).map_err(|_| Reason::BadOutput)?;
    let text = parsed.get("result").and_then(Value::as_str);
    let failed = parsed.get("is_error").and_then(Value::as_bool) == Some(true)
        || parsed
            .get("subtype")
            .and_then(Value::as_str)
            .is_some_and(|s| s != "success");
    if failed {
        return Err(if sounds_signed_out(text.unwrap_or("")) {
            Reason::NotLoggedIn
        } else {
            Reason::CliError
        });
    }
    let text = text.ok_or(Reason::BadOutput)?;
    let usage = parsed.get("usage").cloned().unwrap_or_default();
    let count = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    Ok(Answer {
        text: text.to_owned(),
        prompt_tokens: count("input_tokens")
            + count("cache_creation_input_tokens")
            + count("cache_read_input_tokens"),
        completion_tokens: count("output_tokens"),
    })
}

/// Codex's `--json` event stream plus the last message it wrote: the text is
/// the `-o` file (else the last `agent_message` item), the usage the
/// `turn.completed` event's.
///
/// # Errors
///
/// [`Reason::NotLoggedIn`] / [`Reason::CliError`] for a failed turn,
/// [`Reason::BadOutput`] when no message came back.
pub fn parse_codex(events: &str, last_message: Option<&str>) -> Result<Answer, Reason> {
    let mut usage = (0, 0);
    let mut message: Option<String> = None;
    let mut failure: Option<String> = None;
    for line in events.lines() {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match event.get("type").and_then(Value::as_str).unwrap_or("") {
            "turn.completed" => {
                let u = event.get("usage").cloned().unwrap_or_default();
                let n = |key: &str| u.get(key).and_then(Value::as_u64).unwrap_or(0);
                usage = (n("input_tokens"), n("output_tokens"));
            }
            "turn.failed" => {
                failure = Some(
                    event["error"]["message"]
                        .as_str()
                        .unwrap_or("failed")
                        .to_owned(),
                );
            }
            "item.completed" => {
                let item = &event["item"];
                if item["type"] == "agent_message"
                    && let Some(text) = item["text"].as_str()
                {
                    message = Some(text.to_owned());
                }
            }
            _ => {}
        }
    }
    if let Some(failure) = failure {
        return Err(if sounds_signed_out(&failure) {
            Reason::NotLoggedIn
        } else {
            Reason::CliError
        });
    }
    let text = last_message
        .map(str::to_owned)
        .filter(|t| !t.trim().is_empty())
        .or(message)
        .ok_or(Reason::BadOutput)?;
    Ok(Answer {
        text,
        prompt_tokens: usage.0,
        completion_tokens: usage.1,
    })
}

// ---------------------------------------------------------------------------
// Running a child with a timeout
// ---------------------------------------------------------------------------

/// A child that ran to the end.
struct Finished {
    success: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Why a child did not finish.
enum Stopped {
    /// It would not start.
    Spawn,
    /// It ran past the timeout and was killed.
    Timeout,
    /// Its stdout passed the cap.
    TooLarge,
}

/// A pipe's bytes so far, and whether more arrived than the cap allowed.
type Captured = Arc<Mutex<(Vec<u8>, bool)>>;

/// Reads a pipe to the end into a shared buffer, keeping at most `cap` bytes
/// (and noting that there was more).
fn drain<R: Read + Send + 'static>(mut pipe: R, cap: usize) -> (Captured, thread::JoinHandle<()>) {
    let shared = Arc::new(Mutex::new((Vec::new(), false)));
    let sink = Arc::clone(&shared);
    let handle = thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Ok(mut guard) = sink.lock() {
                        if guard.0.len() + n > cap {
                            guard.1 = true;
                        } else {
                            guard.0.extend_from_slice(&chunk[..n]);
                        }
                    }
                }
            }
        }
    });
    (shared, handle)
}

/// Runs `command` with `stdin` written in (on its own thread, so a child that
/// answers before it reads cannot deadlock), stdout and stderr read up to
/// `cap`, and kills it at `timeout`.
fn execute(
    mut command: Process,
    stdin: Option<String>,
    timeout: Duration,
    cap: usize,
) -> Result<Finished, Stopped> {
    command
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child: Child = command.spawn().map_err(|_| Stopped::Spawn)?;
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        thread::spawn(move || {
            let _ = pipe.write_all(text.as_bytes());
        });
    }
    let (stdout, out_thread) = drain(child.stdout.take().ok_or(Stopped::Spawn)?, cap);
    let (stderr, err_thread) = drain(child.stderr.take().ok_or(Stopped::Spawn)?, 64 * 1024);
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Stopped::Timeout);
            }
            Ok(None) => thread::sleep(Duration::from_millis(50)),
            Err(_) => {
                let _ = child.kill();
                return Err(Stopped::Spawn);
            }
        }
    };
    let _ = out_thread.join();
    let _ = err_thread.join();
    let take = |shared: Captured| {
        shared
            .lock()
            .map(|guard| (guard.0.clone(), guard.1))
            .unwrap_or_default()
    };
    let (stdout, overflow) = take(stdout);
    if overflow {
        return Err(Stopped::TooLarge);
    }
    let (stderr, _) = take(stderr);
    Ok(Finished {
        success: status.success(),
        stdout,
        stderr,
    })
}

/// A fresh, private, empty directory for one job's CLI to run in.
fn job_dir() -> std::io::Result<PathBuf> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let dir = std::env::temp_dir().join(format!("livingbrain-job-{}-{nanos}", std::process::id()));
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&dir)?;
    Ok(dir)
}

// ---------------------------------------------------------------------------
// The poll loop
// ---------------------------------------------------------------------------

/// One claimed job, as `GET /v1/model-jobs/next` hands it out.
struct Job {
    id: String,
    kind: String,
    agent: Option<Agent>,
    model: Option<String>,
    settings: Option<String>,
    system: String,
    user: String,
}

impl Job {
    fn from_value(value: &Value) -> Option<Self> {
        let job = value.get("job")?;
        let text = |key: &str| {
            job.get(key)
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
        };
        Some(Self {
            id: text("id")?,
            kind: text("kind").unwrap_or_else(|| "job".to_owned()),
            agent: text("agent").as_deref().and_then(Agent::parse),
            model: text("model"),
            settings: text("settings"),
            system: text("system").unwrap_or_default(),
            user: text("user")?,
        })
    }

    /// The first eight characters of the id, for a human line.
    fn short(&self) -> String {
        self.id.chars().take(8).collect()
    }
}

/// What a poll came back with.
enum Poll {
    Job(Box<Job>),
    Empty,
}

struct Runner<'a> {
    client: Client,
    agent: Option<Agent>,
    paths: Paths,
    timeout: Duration,
    out: &'a Out,
}

impl Runner<'_> {
    /// One event: a JSON line in `--json` mode, a human line otherwise.
    fn event(&self, value: &Value, human: &str) {
        if self.out.json {
            println!("{value}");
        } else {
            println!("{human}");
        }
        let _ = std::io::stdout().flush();
    }

    /// Polls, runs and reports until stopped (or, `once`, after one job or
    /// none). A signed-out token ends the loop (exit 2); a network error is
    /// reported and retried with backoff.
    fn serve(&self, once: bool, poll: Duration) -> CliResult<()> {
        let found: Vec<&str> = [Agent::Claude, Agent::Codex]
            .into_iter()
            .filter(|agent| self.agent.is_none_or(|only| only == *agent))
            .filter(|agent| locate(*agent, &self.paths).is_some())
            .map(Agent::name)
            .collect();
        self.event(
            &json!({ "event": "started", "agents": found }),
            &format!(
                "Running brain jobs with {} — your subscription, on this machine. Ctrl-C stops.",
                if found.is_empty() {
                    "no agent found".to_owned()
                } else {
                    found.join(" / ")
                }
            ),
        );
        let mut failures: u32 = 0;
        loop {
            match self.next() {
                Ok(Poll::Job(job)) => {
                    failures = 0;
                    self.handle(&job)?;
                    if once {
                        break;
                    }
                }
                Ok(Poll::Empty) => {
                    failures = 0;
                    if once {
                        break;
                    }
                    thread::sleep(poll);
                }
                Err(CliError::NotLoggedIn(message)) => return Err(CliError::NotLoggedIn(message)),
                Err(error) => {
                    self.event(
                        &json!({ "event": "error", "message": error.to_string() }),
                        &format!("error: {error}"),
                    );
                    if once {
                        return Err(error);
                    }
                    failures = failures.saturating_add(1);
                    let backoff = poll
                        .saturating_mul(2u32.saturating_pow(failures.min(6)))
                        .min(MAX_BACKOFF);
                    thread::sleep(backoff);
                }
            }
        }
        self.event(&json!({ "event": "stopped" }), "Stopped.");
        Ok(())
    }

    /// `GET /v1/model-jobs/next`.
    fn next(&self) -> CliResult<Poll> {
        let (status, body) = self.client.get_status("/v1/model-jobs/next")?;
        match status {
            204 => Ok(Poll::Empty),
            200 => {
                let value: Value = serde_json::from_slice(&body)
                    .map_err(|_| err("the job server's answer was not JSON"))?;
                if value.get("job").is_none_or(Value::is_null) {
                    return Ok(Poll::Empty);
                }
                Job::from_value(&value)
                    .map(|job| Poll::Job(Box::new(job)))
                    .ok_or_else(|| err("the job server handed out a malformed job"))
            }
            401 => Err(CliError::NotLoggedIn(
                "not logged in — run `livingbrain login`".into(),
            )),
            other => Err(CliError::Api(format!(
                "the job server answered HTTP {other}"
            ))),
        }
    }

    /// Runs one claimed job and posts its result or its failure.
    fn handle(&self, job: &Job) -> CliResult<()> {
        let agent = self.agent.or(job.agent).unwrap_or(Agent::Claude);
        // A model chosen for the job's own agent means nothing to another.
        let model = if self.agent.is_some() && job.agent.is_some_and(|a| a != agent) {
            None
        } else {
            job.model.clone()
        };
        self.event(
            &json!({
                "event": "claimed",
                "job": job.id,
                "kind": job.kind,
                "agent": agent.name(),
                "model": model,
            }),
            &format!(
                "job {} ({}) → {}{}",
                job.short(),
                job.kind,
                agent.name(),
                model
                    .as_deref()
                    .map(|m| format!(" {m}"))
                    .unwrap_or_default()
            ),
        );
        let started = Instant::now();
        let outcome = self.execute_job(agent, model.as_deref(), job);
        let seconds = (started.elapsed().as_secs_f64() * 10.0).round() / 10.0;
        let id = encode_segment(&job.id);
        match outcome {
            Ok(answer) => {
                let (status, _) = self.client.post_status(
                    &format!("/v1/model-jobs/{id}/result"),
                    &json!({
                        "text": answer.text,
                        "prompt_tokens": answer.prompt_tokens,
                        "completion_tokens": answer.completion_tokens,
                    }),
                )?;
                match status {
                    200..=299 => self.event(
                        &json!({
                            "event": "done",
                            "job": job.id,
                            "kind": job.kind,
                            "seconds": seconds,
                            "prompt_tokens": answer.prompt_tokens,
                            "completion_tokens": answer.completion_tokens,
                        }),
                        &format!(
                            "job {} done in {seconds}s ({} in / {} out tokens)",
                            job.short(),
                            answer.prompt_tokens,
                            answer.completion_tokens
                        ),
                    ),
                    401 => {
                        return Err(CliError::NotLoggedIn(
                            "not logged in — run `livingbrain login`".into(),
                        ));
                    }
                    422 => self.event(
                        &json!({ "event": "failed", "job": job.id, "reason": "rejected" }),
                        &format!("job {} failed: the server refused the answer", job.short()),
                    ),
                    other => self.event(
                        &json!({
                            "event": "error",
                            "message": format!("posting job {} answered HTTP {other}", job.short()),
                        }),
                        &format!("error: posting job {} answered HTTP {other}", job.short()),
                    ),
                }
            }
            Err(reason) => {
                let (status, _) = self.client.post_status(
                    &format!("/v1/model-jobs/{id}/fail"),
                    &json!({ "reason": reason.as_str() }),
                )?;
                if status == 401 {
                    return Err(CliError::NotLoggedIn(
                        "not logged in — run `livingbrain login`".into(),
                    ));
                }
                self.event(
                    &json!({ "event": "failed", "job": job.id, "reason": reason.as_str() }),
                    &format!("job {} failed: {}", job.short(), reason.as_str()),
                );
            }
        }
        Ok(())
    }

    /// Runs the job's CLI in a fresh directory and reads its answer.
    fn execute_job(&self, agent: Agent, model: Option<&str>, job: &Job) -> Result<Answer, Reason> {
        let binary = locate(agent, &self.paths).ok_or(Reason::CliMissing)?;
        let dir = job_dir().map_err(|_| Reason::CliError)?;
        let result = self.execute_in(&dir, &binary, agent, model, job);
        let _ = std::fs::remove_dir_all(&dir);
        result
    }

    fn execute_in(
        &self,
        dir: &Path,
        binary: &Path,
        agent: Agent,
        model: Option<&str>,
        job: &Job,
    ) -> Result<Answer, Reason> {
        let last = dir.join("last-message.txt");
        let (argv, stdin) = match agent {
            Agent::Claude => (
                claude_argv(&job.system, model, job.settings.as_deref()),
                job.user.clone(),
            ),
            Agent::Codex => (
                codex_argv(dir, &last, model),
                codex_stdin(&job.system, &job.user),
            ),
        };
        // Belt and braces: the builders never emit a bypass flag, and a job
        // whose argv somehow held one is refused rather than run. The
        // system prompt is the job's text, not a flag, so it is not checked.
        if argv
            .iter()
            .filter(|arg| **arg != job.system)
            .any(|arg| FORBIDDEN.iter().any(|flag| arg.contains(flag)))
        {
            return Err(Reason::CliError);
        }
        let mut command = Process::new(binary);
        command.args(&argv).current_dir(dir);
        // The agent CLI signs in with its own credentials; it never needs ours,
        // not the default token nor any workspace's handed over by the Mac app.
        auth::scrub_inherited_tokens(&mut command);
        let finished = match execute(command, Some(stdin), self.timeout, MAX_STDOUT) {
            Ok(finished) => finished,
            Err(Stopped::Timeout) => return Err(Reason::Timeout),
            Err(Stopped::Spawn) => return Err(Reason::CliMissing),
            Err(Stopped::TooLarge) => return Err(Reason::BadOutput),
        };
        let stdout = String::from_utf8_lossy(&finished.stdout);
        let parsed = match agent {
            Agent::Claude => parse_claude(&stdout),
            Agent::Codex => {
                let last = std::fs::read_to_string(&last).ok();
                parse_codex(&stdout, last.as_deref())
            }
        };
        match parsed {
            Ok(answer) if finished.success => Ok(answer),
            // A non-zero exit with no readable answer: say whether it was a
            // sign-in problem, and nothing else of what it printed.
            Ok(_) | Err(Reason::BadOutput) if !finished.success => {
                if sounds_signed_out(&String::from_utf8_lossy(&finished.stderr)) {
                    Err(Reason::NotLoggedIn)
                } else {
                    Err(Reason::CliError)
                }
            }
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_safe(argv: &[String]) {
        for arg in argv {
            for flag in FORBIDDEN {
                assert!(!arg.contains(flag), "{arg:?} contains {flag}");
            }
        }
    }

    #[test]
    fn claude_runs_with_no_tools_and_no_bypass() {
        let argv = claude_argv("sys", Some("sonnet"), None);
        assert_safe(&argv);
        let joined = argv.join(" ");
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "--tools" && w[1].is_empty())
        );
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "--setting-sources" && w[1].is_empty())
        );
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "--max-turns" && w[1] == "1")
        );
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "--permission-mode" && w[1] == "dontAsk")
        );
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "--model" && w[1] == "sonnet")
        );
        assert!(joined.contains("--strict-mcp-config"));
        assert!(joined.contains("--no-session-persistence"));
        assert!(!claude_argv("s", None, None).contains(&"--model".to_owned()));
        let user = claude_argv("s", Some(""), Some("user"));
        assert!(
            user.windows(2)
                .any(|w| w[0] == "--setting-sources" && w[1] == "user")
        );
        assert!(!user.contains(&"--model".to_owned()));
        assert_safe(&user);
    }

    #[test]
    fn codex_runs_read_only_and_reads_stdin() {
        let argv = codex_argv(
            Path::new("/tmp/x"),
            Path::new("/tmp/x/last"),
            Some("gpt-5.5"),
        );
        assert_safe(&argv);
        assert_eq!(argv[0], "exec");
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "--sandbox" && w[1] == "read-only")
        );
        assert!(argv.windows(2).any(|w| w[0] == "-m" && w[1] == "gpt-5.5"));
        assert!(argv.contains(&"--ephemeral".to_owned()));
        assert!(argv.contains(&"--ignore-user-config".to_owned()));
        assert_eq!(argv.last().map(String::as_str), Some("-"));
        assert_eq!(codex_stdin("rules", "data"), "rules\n\ndata");
    }

    #[test]
    fn claude_results_and_usage_are_read() {
        let ok = r#"{"type":"result","subtype":"success","is_error":false,"result":"{\"ok\":true}","usage":{"input_tokens":2,"cache_creation_input_tokens":600,"cache_read_input_tokens":10,"output_tokens":9}}"#;
        assert_eq!(
            parse_claude(ok),
            Ok(Answer {
                text: "{\"ok\":true}".to_owned(),
                prompt_tokens: 612,
                completion_tokens: 9
            })
        );
        let out = r#"{"type":"result","subtype":"success","is_error":true,"result":"Not logged in · Please run /login"}"#;
        assert_eq!(parse_claude(out), Err(Reason::NotLoggedIn));
        let other = r#"{"type":"result","subtype":"error_max_turns","is_error":true,"result":"x"}"#;
        assert_eq!(parse_claude(other), Err(Reason::CliError));
        assert_eq!(parse_claude("not json"), Err(Reason::BadOutput));
    }

    #[test]
    fn codex_events_are_read() {
        let events = concat!(
            r#"{"type":"thread.started"}"#,
            "\n",
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"hello"}}"#,
            "\n",
            r#"{"type":"turn.completed","usage":{"input_tokens":30,"cached_input_tokens":5,"output_tokens":4}}"#,
        );
        assert_eq!(
            parse_codex(events, None),
            Ok(Answer {
                text: "hello".to_owned(),
                prompt_tokens: 30,
                completion_tokens: 4
            })
        );
        assert_eq!(
            parse_codex(events, Some("from file")).map(|a| a.text),
            Ok("from file".to_owned())
        );
        let failed =
            r#"{"type":"turn.failed","error":{"message":"unexpected status 401 Unauthorized"}}"#;
        assert_eq!(parse_codex(failed, None), Err(Reason::NotLoggedIn));
        let paid = r#"{"type":"turn.failed","error":{"message":"unexpected status 402 Payment Required"}}"#;
        assert_eq!(parse_codex(paid, None), Err(Reason::CliError));
        assert_eq!(parse_codex("", None), Err(Reason::BadOutput));
    }
}
