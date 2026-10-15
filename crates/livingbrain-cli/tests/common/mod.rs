//! Test support: a tiny blocking HTTP mock and a helper to run the real
//! `livingbrain` binary against it. No test framework, no extra dependency.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

/// One request the mock saw.
#[derive(Clone)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    /// A header value, case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The bearer token, if the request carried one.
    pub fn bearer(&self) -> Option<String> {
        self.header("authorization")
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::to_owned)
    }

    /// The body as text.
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// One canned reply.
pub struct Reply {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn ok_json(body: &str) -> Self {
        Self::json(200, body)
    }

    pub fn json(status: u16, body: &str) -> Self {
        Self::bytes(status, "application/json", body.as_bytes().to_vec())
    }

    pub fn bytes(status: u16, content_type: &str, body: Vec<u8>) -> Self {
        Self {
            status,
            content_type: content_type.to_owned(),
            body,
        }
    }
}

/// A running mock server on an ephemeral local port.
pub struct Mock {
    pub base: String,
    requests: Arc<Mutex<Vec<Request>>>,
}

impl Mock {
    /// Start a server that answers every request with `handler`.
    pub fn start<F>(handler: F) -> Self
    where
        F: Fn(&Request) -> Reply + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let base = format!("http://{}", listener.local_addr().expect("local addr"));
        let requests: Arc<Mutex<Vec<Request>>> = Arc::new(Mutex::new(Vec::new()));
        let handler = Arc::new(handler);
        let seen = Arc::clone(&requests);
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = serve(stream, handler.as_ref(), &seen);
            }
        });
        Self { base, requests }
    }

    /// Every request the mock has seen, oldest first.
    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().expect("requests lock").clone()
    }
}

/// Answer one connection: parse the request, record it, reply with `Reply`.
fn serve<F: Fn(&Request) -> Reply>(
    mut stream: TcpStream,
    handler: &F,
    seen: &Mutex<Vec<Request>>,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut start = String::new();
    if reader.read_line(&mut start)? == 0 {
        return Ok(());
    }
    let mut parts = start.split_whitespace();
    let method = parts.next().unwrap_or("").to_owned();
    let target = parts.next().unwrap_or("/");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let (path, query) = (path.to_owned(), query.to_owned());
    let mut headers = Vec::new();
    let mut length = 0;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 || header.trim().is_empty() {
            break;
        }
        if let Some((key, value)) = header.trim().split_once(':') {
            if key.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap_or(0);
            }
            headers.push((key.trim().to_owned(), value.trim().to_owned()));
        }
    }
    let mut body = vec![0u8; length];
    if length > 0 {
        reader.read_exact(&mut body)?;
    }
    let request = Request {
        method,
        path,
        query,
        headers,
        body,
    };
    let reply = handler(&request);
    seen.lock().expect("requests lock").push(request);
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        reply.status,
        reason(reply.status),
        reply.content_type,
        reply.body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(&reply.body)?;
    stream.flush()
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Status",
    }
}

/// A fresh, empty directory under the system temp directory.
pub fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let mut path = std::env::temp_dir();
    path.push(format!(
        "livingbrain-cli-test-{tag}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&path).expect("create temp dir");
    path
}

/// One finished run of the real `livingbrain` binary.
pub struct Output {
    pub status: std::process::ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

/// Every `LIVINGBRAIN_*` variable the binary reads, and which way this helper
/// deals with it.
///
/// The rule: **a test run must not depend on the developer's shell.** Every
/// variable here has a value in a real shell that changes what the binary
/// does — `LIVINGBRAIN_API_URL` points a test at a stranger's server,
/// `LIVINGBRAIN_TOKEN` makes a command that should say "not logged in" report
/// success, and `LIVINGBRAIN_TELEMETRY` flips the resolution rule that
/// `tests/telemetry.rs` is entirely about. A test that only passes because
/// nobody happened to export the variable is not a test, and worse, the one
/// this list was written for is near-guaranteed to be set: `livingbrain
/// telemetry off` tells the reader to `export LIVINGBRAIN_TELEMETRY=0`, so
/// setting it is the documented next step after using the feature.
///
/// The list is spelled out as a whole rather than a bare `env_remove` at each
/// site so that adding a command which reads a new `LIVINGBRAIN_*` variable
/// means editing this one place, and so that the *reason* a variable is here is
/// attached to the variable. New `LIVINGBRAIN_*` readers get added here, or
/// their tests will inherit the shell.
///
/// The two `LIVINGBRAIN_TEST_*` hooks are deliberately absent: this helper
/// sets both itself (below), because a test needs them and cannot have a
/// developer's shell decide whether they are on.
const SCRUBBED: &[&str] = &[
    // The API base URL. A developer's `LIVINGBRAIN_API_URL` points every
    // request at their own server instead of the mock.
    "LIVINGBRAIN_API_URL",
    // The bearer token. Without this a logged-in developer gets a command that
    // should refuse to run — or a `logout` test — quietly succeeding.
    "LIVINGBRAIN_TOKEN",
    // The one-run telemetry override, read by `telemetry::Env::from_process`.
    // This is the variable the docs tell people to export, so it is the one
    // most likely to be set when the suite runs. It was missing here, and
    // `LIVINGBRAIN_TELEMETRY=0` in the developer's shell failed two tests in
    // `tests/telemetry.rs` and `=1` failed a third.
    "LIVINGBRAIN_TELEMETRY",
];

/// Run the binary in `cwd` with an isolated `home`, the given environment and
/// optional stdin. The test-only keychain hook is always set.
pub fn run(
    cwd: &Path,
    home: &Path,
    args: &[&str],
    envs: &[(&str, &str)],
    stdin: Option<&str>,
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_livingbrain"));
    command.current_dir(cwd);
    for key in [
        "HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
        "XDG_RUNTIME_DIR",
        "APPDATA",
        "LOCALAPPDATA",
        "USERPROFILE",
    ] {
        command.env(key, home);
    }
    command.env("LIVINGBRAIN_TEST_KEYRING", "mock");
    // Debug-only hook (see `auth::poll_interval`): poll the device token with no
    // sleep. Ignored by a release build, so the login test is `ignore`d there.
    command.env("LIVINGBRAIN_TEST_POLL_INTERVAL", "0");
    // Scrubbed before `envs` is applied, so a test that wants one of these
    // values still sets it explicitly — an inherited value can never win.
    for key in SCRUBBED {
        command.env_remove(key);
    }
    for (key, value) in envs {
        command.env(key, value);
    }
    command.args(args);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    command.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    let mut child = command.spawn().expect("spawn livingbrain");
    if let Some(input) = stdin {
        child
            .stdin
            .take()
            .expect("stdin pipe")
            .write_all(input.as_bytes())
            .expect("write stdin");
    }
    let output = child.wait_with_output().expect("wait");
    Output {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// Assert no file anywhere under `dir` contains `needle`.
pub fn assert_absent_from_tree(dir: &Path, needle: &str) {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(bytes) = std::fs::read(&path) {
                let leaked = bytes
                    .windows(needle.len())
                    .any(|window| window == needle.as_bytes());
                assert!(!leaked, "the token leaked into {}", path.display());
            }
        }
    }
}
