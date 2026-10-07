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
    command.env_remove("LIVINGBRAIN_TOKEN");
    command.env_remove("LIVINGBRAIN_API_URL");
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
