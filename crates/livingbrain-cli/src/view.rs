//! `livingbrain view`: the 3D brain, served from this machine.
//!
//! The page is the web app's own brain map: `app/brain.html` and
//! `app/assets/`, embedded by `build.rs` from the source tree, so the site and
//! the CLI run the same files. Nothing is forked. This process serves them on
//! `127.0.0.1` at a random free port, under a random per-run path token:
//!
//! ```text
//! http://127.0.0.1:<port>/<token>/
//! ```
//!
//! The map's API calls (`/v1/…`) come back to this server under the same
//! token. The server forwards the handful of reads the map makes to the API
//! and adds the CLI's stored credential here, on the server side. The
//! credential is never in the page, the URL or the output.
//!
//! The guards, in order, for every request:
//!
//! 1. The `Host` header must name `127.0.0.1` or `localhost` with this
//!    server's port. A page on another site that rebinds its DNS to the
//!    loopback address sends its own name, and is refused.
//! 2. The path must start with the run token, compared in constant time.
//!    Anything else is a plain `404`, so an answer does not say whether a
//!    token was close.
//! 3. Under the token, only the assets, the one page, the allowlisted reads
//!    (with only their own query keys) and a redirect for the app's other
//!    pages are served. Everything else is a `404`.
//! 4. No response carries a CORS header, so no other site can read one.
//!
//! The token is 32 bytes from `/dev/urandom` on Unix. The Windows fallback
//! mixes `RandomState` with the clock, because this crate may not call the OS
//! RNG through `unsafe`.
//!
//! Not built here: the offline cache (`--offline`, issue #35), so the map needs
//! the network; and `--project`, because the map's own project filter already
//! scopes it.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;
#[cfg(not(unix))]
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::api::Client;
use crate::auth;
use crate::{CliError, CliResult, Out, err};

include!(concat!(env!("OUT_DIR"), "/embedded.rs"));

/// The one page the view serves: the web app's brain map.
const PAGE: &str = "brain.html";

/// The meta tag the page reads its API base from. The page ships it empty
/// (same origin); the view points it at the run's path, so the map's `/v1/…`
/// calls come back here, under the token.
const API_META: &str = r#"<meta name="lb-api" content="" />"#;

/// The page's CSP: the site's own policy (`app/_headers`), with the inline
/// theme bootstrap's hash. `brain.html`'s inline script is the one the test
/// `the_inline_script_hash_matches_the_policy` checks against this hash.
const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; script-src 'self' 'sha256-GsRRMiFErXfPA/V4np3z0CtFFGhxjhelBLl+TjY8sYA='; style-src 'self'; img-src 'self' data:; connect-src 'self'; font-src 'self'; object-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// Headers on every response: nothing cached, nothing sniffed, no referrer,
/// and no CORS.
const SECURITY_HEADERS: &str = concat!(
    "Cache-Control: no-store\r\n",
    "Referrer-Policy: no-referrer\r\n",
    "X-Content-Type-Options: nosniff\r\n",
    "X-Frame-Options: DENY\r\n",
    "Cross-Origin-Resource-Policy: same-origin\r\n",
);

/// The request head, larger than this is refused.
const MAX_HEAD_BYTES: usize = 16 * 1024;
/// Connections served at once; the map makes a handful of requests.
const MAX_IN_FLIGHT: usize = 32;
const READ_TIMEOUT: Duration = Duration::from_secs(10);
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest query string the view forwards or redirects with.
const MAX_QUERY_BYTES: usize = 1024;

/// `livingbrain view`'s flags.
#[derive(clap::Args, Debug, Clone, PartialEq, Eq)]
pub struct ViewArgs {
    /// Print the URL instead of opening the browser
    #[arg(long)]
    pub no_open: bool,
    /// Show this workspace's brain (its token: `LIVINGBRAIN_TOKEN_<WORKSPACE>`, or the Mac app's keychain item)
    #[arg(long, value_name = "WORKSPACE")]
    pub workspace: Option<String>,
}

/// The API reads the server forwards. The query keys are the ones the map sends.
/// A path not listed here is a `404`, whatever the token.
fn proxied_target(rest: &str, query: &str) -> Option<String> {
    let allowed: &[&str] = match rest {
        "/v1/workspaces/me" => &[],
        "/v1/graph" => &["project", "kind", "limit"],
        "/v1/graph/activity" => &["since"],
        "/v1/search" => &["q", "project", "limit"],
        _ => {
            let segment = rest.strip_prefix("/v1/pages/")?;
            if segment.is_empty()
                || segment == "."
                || segment == ".."
                || !segment.bytes().all(is_path_byte)
            {
                return None;
            }
            &[]
        }
    };
    let kept = keep_query(query, allowed)?;
    Some(if kept.is_empty() {
        rest.to_owned()
    } else {
        format!("{rest}?{kept}")
    })
}

/// `query` when every key is allowed and every value is plain; `None` when
/// anything is not (refused, never silently dropped).
fn keep_query(query: &str, allowed: &[&str]) -> Option<String> {
    if query.is_empty() {
        return Some(String::new());
    }
    if query.len() > MAX_QUERY_BYTES {
        return None;
    }
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if !allowed.contains(&key) || !value.bytes().all(is_query_byte) {
            return None;
        }
    }
    Some(query.to_owned())
}

/// Bytes a path segment may hold: unreserved characters and percent-escapes.
fn is_path_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"%._~-".contains(&byte)
}

/// Bytes a query value may hold: what the map's `encodeURIComponent` and
/// `URLSearchParams` produce.
fn is_query_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"%._~-+*".contains(&byte)
}

/// Whether the `Host` header names this server. Anything else, including a
/// missing header, is refused: that is the DNS-rebinding guard.
fn host_allowed(host: Option<&str>, port: u16) -> bool {
    match host {
        Some(host) => {
            let host = host.to_ascii_lowercase();
            host == format!("127.0.0.1:{port}") || host == format!("localhost:{port}")
        }
        None => false,
    }
}

/// Equal length and equal bytes, in time that does not depend on where they
/// first differ.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The media type a served file is sent as, from its extension.
fn content_type(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or("") {
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "html" => "text/html; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "webmanifest" => "application/manifest+json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

/// The request line and headers the guards read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    pub method: String,
    pub target: String,
    pub host: Option<String>,
}

/// A response: status, media type, body, and a `Location` for a redirect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
    pub location: Option<String>,
}

impl Reply {
    fn new(status: u16, content_type: &str, body: Vec<u8>) -> Self {
        Self {
            status,
            content_type: content_type.to_owned(),
            body,
            location: None,
        }
    }

    fn text(status: u16, message: &str) -> Self {
        Self::new(
            status,
            "text/plain; charset=utf-8",
            message.as_bytes().to_vec(),
        )
    }

    fn json(status: u16, value: &Value) -> Self {
        Self::new(
            status,
            "application/json; charset=utf-8",
            value.to_string().into_bytes(),
        )
    }

    fn not_found() -> Self {
        Self::text(404, "not found")
    }

    fn redirect(location: String) -> Self {
        let mut reply = Self::new(302, "text/plain; charset=utf-8", Vec::new());
        reply.location = Some(location);
        reply
    }
}

/// The API as the server sees it: one GET, answered with status and body.
/// The real one is the CLI's client; tests use a fake.
pub trait Upstream: Send + Sync {
    fn fetch(&self, path_and_query: &str) -> CliResult<(u16, Vec<u8>)>;
}

impl Upstream for Client {
    fn fetch(&self, path_and_query: &str) -> CliResult<(u16, Vec<u8>)> {
        self.get_status(path_and_query)
    }
}

/// One run's server: the token, the port it is bound to, where the app's
/// other pages live, and the API.
pub struct Server {
    token: String,
    port: u16,
    site_base: String,
    upstream: Box<dyn Upstream>,
}

impl Server {
    pub fn new(token: String, port: u16, site_base: String, upstream: Box<dyn Upstream>) -> Self {
        Self {
            token,
            port,
            site_base,
            upstream,
        }
    }

    /// The answer to one request, after every guard.
    pub fn respond(&self, head: &Head) -> Reply {
        if !host_allowed(head.host.as_deref(), self.port) {
            return Reply::text(403, "forbidden: the Host header must name this server");
        }
        if head.method != "GET" {
            return Reply::text(405, "method not allowed");
        }
        let (path, query) = head.target.split_once('?').unwrap_or((&head.target, ""));
        let Some(rest) = self.strip_token(path) else {
            return Reply::not_found();
        };
        if rest == "/v1" || rest.starts_with("/v1/") {
            return self.proxy(rest, query);
        }
        self.asset(rest, query)
    }

    /// The path under the run token (`/` for the page itself), or `None` when
    /// the path does not start with the token as a whole segment.
    fn strip_token<'a>(&self, path: &'a str) -> Option<&'a str> {
        let prefix = format!("/{}", self.token);
        let head = path.get(..prefix.len())?;
        if !constant_time_eq(head.as_bytes(), prefix.as_bytes()) {
            return None;
        }
        let rest = &path[prefix.len()..];
        if rest.is_empty() {
            Some("/")
        } else if rest.starts_with('/') {
            Some(rest)
        } else {
            None
        }
    }

    fn proxy(&self, rest: &str, query: &str) -> Reply {
        let Some(target) = proxied_target(rest, query) else {
            return Reply::not_found();
        };
        match self.upstream.fetch(&target) {
            // The page's own "signed out" path (a 401) reads `error`; the
            // message says what to do, and nothing from the request is echoed.
            Ok((401, _)) => Reply::json(
                401,
                &json!({"error": "not signed in: run `livingbrain login`"}),
            ),
            Ok((status, body)) => Reply::new(status, "application/json; charset=utf-8", body),
            Err(error) => Reply::json(
                502,
                &json!({"error": format!("could not reach the API: {error}")}),
            ),
        }
    }

    fn asset(&self, rest: &str, query: &str) -> Reply {
        let name = rest.trim_start_matches('/');
        if name.is_empty() || name == PAGE {
            return self.page();
        }
        if name
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        {
            return Reply::not_found();
        }
        if let Some((_, bytes)) = FILES.iter().find(|(path, _)| *path == name) {
            return Reply::new(200, content_type(name), bytes.to_vec());
        }
        // The app's other pages are not served here. They are the hosted app's
        // own pages, so a link to one goes to the same app on its origin.
        let is_page = name.ends_with(".html")
            && !name.contains('/')
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-');
        if is_page {
            let query = if !query.is_empty() && plain_query(query) {
                format!("?{query}")
            } else {
                String::new()
            };
            return Reply::redirect(format!("{}/{name}{query}", self.site_base));
        }
        Reply::not_found()
    }

    /// The brain map, with its API base pointed at this run's path.
    fn page(&self) -> Reply {
        let Some((_, bytes)) = FILES.iter().find(|(path, _)| *path == PAGE) else {
            return Reply::text(500, "the embedded page is missing");
        };
        let Ok(html) = std::str::from_utf8(bytes) else {
            return Reply::text(500, "the embedded page is not UTF-8");
        };
        if html.matches(API_META).count() != 1 {
            return Reply::text(500, "the embedded page has no API meta tag to point");
        }
        let meta = format!(r#"<meta name="lb-api" content="/{}" />"#, self.token);
        Reply::new(
            200,
            "text/html; charset=utf-8",
            html.replacen(API_META, &meta, 1).into_bytes(),
        )
    }
}

/// Whether a query string is only the characters a query is made of, so it
/// can be put in a `Location` header as it is.
fn plain_query(query: &str) -> bool {
    query.len() <= MAX_QUERY_BYTES
        && query
            .bytes()
            .all(|b| is_query_byte(b) || b == b'=' || b == b'&')
}

/// `livingbrain view`: serve the brain map, then run until Ctrl-C.
pub fn run(api_url: &str, args: &ViewArgs, out: &Out) -> CliResult<()> {
    let token = match args.workspace.as_deref() {
        Some(workspace) => auth::workspace_token(api_url, workspace)?.ok_or_else(|| {
            CliError::NotLoggedIn(format!(
                "no token for workspace {workspace} — set {} or choose it in Living Brain for Mac",
                auth::workspace_env_var(workspace)
            ))
        })?,
        None => auth::token_for(api_url)?,
    };
    let client = Client::new(api_url).with_token(token);
    let run_token = new_run_token()?;
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .map_err(|e| err(format!("could not bind 127.0.0.1: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| err(format!("could not read the bound port: {e}")))?
        .port();
    let url = format!("http://127.0.0.1:{port}/{run_token}/");
    let server = Server::new(
        run_token,
        port,
        api_url.trim_end_matches('/').to_owned(),
        Box::new(client),
    );

    let opened = !args.no_open && open_browser(&url);
    // The server outlives its output: a closed stdout or stderr (a pipe whose
    // reader went away, `| head`) must not kill it, so these writes ignore errors
    // where `println!` would panic and exit.
    if out.json {
        say(&json!({"url": url, "opened": opened}).to_string());
    } else {
        say(&format!("Living Brain view: {url}"));
        if args.no_open {
            say("Open that URL in a browser. Ctrl-C stops the server.");
        } else if opened {
            say("Opened in your browser. Ctrl-C stops the server.");
        } else {
            warn("could not open a browser; open the URL above. Ctrl-C stops the server.");
        }
    }
    serve(listener, Arc::new(server))
}

/// One line on stdout. A failed write is ignored: see `run`.
fn say(line: &str) {
    let _ = writeln!(std::io::stdout(), "{line}");
}

/// One line on stderr. A failed write is ignored: see `run`.
fn warn(line: &str) {
    let _ = writeln!(std::io::stderr(), "{line}");
}

/// Accept connections until the process ends (Ctrl-C ends it; nothing is
/// written to disk, so there is nothing to clean up).
fn serve(listener: TcpListener, server: Arc<Server>) -> CliResult<()> {
    let in_flight = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else {
            continue;
        };
        if in_flight.fetch_add(1, Ordering::SeqCst) >= MAX_IN_FLIGHT {
            in_flight.fetch_sub(1, Ordering::SeqCst);
            let _ = write_reply(&mut stream, &Reply::text(503, "busy, try again"));
            continue;
        }
        let server = Arc::clone(&server);
        let in_flight = Arc::clone(&in_flight);
        thread::spawn(move || {
            let _ = handle(&mut stream, &server);
            in_flight.fetch_sub(1, Ordering::SeqCst);
        });
    }
    Ok(())
}

fn handle(stream: &mut TcpStream, server: &Server) -> std::io::Result<()> {
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    let reply = match read_head(stream) {
        Ok(head) => server.respond(&head),
        Err(ReadError::Malformed) => Reply::text(400, "bad request"),
        Err(ReadError::Io) => return Ok(()),
    };
    write_reply(stream, &reply)
}

enum ReadError {
    Malformed,
    Io,
}

/// Read up to the blank line that ends the head. The body, if any, is never
/// read: every route here is a GET.
fn read_head(stream: &mut TcpStream) -> Result<Head, ReadError> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|window| window == b"\r\n\r\n") {
        if buf.len() > MAX_HEAD_BYTES {
            return Err(ReadError::Malformed);
        }
        let read = stream.read(&mut chunk).map_err(|_| ReadError::Io)?;
        if read == 0 {
            return Err(ReadError::Malformed);
        }
        buf.extend_from_slice(&chunk[..read]);
    }
    parse_head(&buf).ok_or(ReadError::Malformed)
}

/// The request line and headers of a head; `None` when it is not HTTP/1.x,
/// a header has no colon, or `Host` appears twice.
fn parse_head(bytes: &[u8]) -> Option<Head> {
    let text = std::str::from_utf8(bytes).ok()?;
    let head = text.split("\r\n\r\n").next()?;
    let mut lines = head.split("\r\n");
    let mut request = lines.next()?.split(' ');
    let method = request.next()?;
    let target = request.next()?;
    let version = request.next()?;
    if !version.starts_with("HTTP/1.") || request.next().is_some() {
        return None;
    }
    let mut host = None;
    for line in lines {
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("host") {
            if host.is_some() {
                return None;
            }
            host = Some(value.trim().to_owned());
        }
    }
    Some(Head {
        method: method.to_owned(),
        target: target.to_owned(),
        host,
    })
}

fn write_reply(stream: &mut TcpStream, reply: &Reply) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n",
        reply.status,
        reason(reply.status),
        reply.content_type,
        reply.body.len()
    );
    if let Some(location) = &reply.location {
        head.push_str(&format!("Location: {location}\r\n"));
    }
    if reply.content_type.starts_with("text/html") {
        head.push_str(&format!(
            "Content-Security-Policy: {CONTENT_SECURITY_POLICY}\r\n"
        ));
    }
    head.push_str(SECURITY_HEADERS);
    head.push_str("Connection: close\r\n\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(&reply.body)?;
    stream.flush()
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

/// A per-run path token: 32 bytes, hex. See the module note on the fallback.
fn new_run_token() -> CliResult<String> {
    let mut bytes = [0u8; 32];
    #[cfg(unix)]
    {
        std::fs::File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut bytes))
            .map_err(|e| err(format!("could not read the system random source: {e}")))?;
    }
    #[cfg(not(unix))]
    {
        use std::collections::hash_map::RandomState;
        use std::hash::{BuildHasher, Hasher};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        for (index, chunk) in bytes.chunks_mut(8).enumerate() {
            let mut hasher = RandomState::new().build_hasher();
            hasher.write_usize(index);
            hasher.write_u128(nanos);
            let word = hasher.finish().to_le_bytes();
            chunk.copy_from_slice(&word[..chunk.len()]);
        }
    }
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Hand the URL to the platform's opener. Returns whether it started; the
/// URL is still printed either way.
fn open_browser(url: &str) -> bool {
    let mut command = if cfg!(target_os = "macos") {
        let mut command = Command::new("open");
        command.arg(url);
        command
    } else if cfg!(windows) {
        let mut command = Command::new("cmd");
        command.args(["/C", "start", "", url]);
        command
    } else {
        let mut command = Command::new("xdg-open");
        command.arg(url);
        command
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_ok()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use clap::Parser;

    use super::*;

    const TOKEN: &str = "run-token-0123456789abcdef";
    const API_TOKEN: &str = "api-bearer-SECRET-do-not-echo";
    const PORT: u16 = 49152;

    /// A fake API that records every path it was asked for.
    struct Fake {
        seen: Arc<Mutex<Vec<String>>>,
        status: u16,
        fail: bool,
    }

    impl Upstream for Fake {
        fn fetch(&self, path_and_query: &str) -> CliResult<(u16, Vec<u8>)> {
            self.seen.lock().unwrap().push(path_and_query.to_owned());
            if self.fail {
                return Err(CliError::Api("could not reach the API: refused".into()));
            }
            Ok((self.status, br#"{"nodes":[],"edges":[]}"#.to_vec()))
        }
    }

    fn make_server(status: u16, fail: bool) -> (Server, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let fake = Fake {
            seen: Arc::clone(&seen),
            status,
            fail,
        };
        let server = Server::new(
            TOKEN.to_owned(),
            PORT,
            "https://app.example".to_owned(),
            Box::new(fake),
        );
        (server, seen)
    }

    fn get(target: &str) -> Head {
        Head {
            method: "GET".into(),
            target: target.into(),
            host: Some(format!("127.0.0.1:{PORT}")),
        }
    }

    fn body_text(reply: &Reply) -> String {
        String::from_utf8_lossy(&reply.body).into_owned()
    }

    #[test]
    fn the_run_token_is_64_hex_characters_and_differs_per_run() {
        let a = new_run_token().expect("token");
        let b = new_run_token().expect("token");
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn only_the_loopback_host_with_this_port_is_allowed() {
        assert!(host_allowed(Some("127.0.0.1:49152"), 49152));
        assert!(host_allowed(Some("LOCALHOST:49152"), 49152));
        assert!(!host_allowed(Some("127.0.0.1:1"), 49152));
        assert!(!host_allowed(Some("evil.example:49152"), 49152));
        assert!(!host_allowed(Some("127.0.0.1.evil.example:49152"), 49152));
        assert!(!host_allowed(Some("127.0.0.1"), 49152));
        assert!(!host_allowed(None, 49152));
    }

    #[test]
    fn a_foreign_host_is_refused_before_anything_else() {
        let (server, seen) = make_server(200, false);
        let head = Head {
            method: "GET".into(),
            target: format!("/{TOKEN}/v1/graph"),
            host: Some("evil.example".into()),
        };
        assert_eq!(server.respond(&head).status, 403);
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn a_request_without_the_run_token_is_a_plain_not_found() {
        let (server, seen) = make_server(200, false);
        for target in [
            "/".to_owned(),
            "/v1/graph".to_owned(),
            "/brain.html".to_owned(),
            "/wrong-token/v1/graph".to_owned(),
            format!("/{TOKEN}x/v1/graph"),
            format!("/{TOKEN}v1/graph"),
        ] {
            let reply = server.respond(&get(&target));
            assert_eq!(reply.status, 404, "{target}");
            assert!(!body_text(&reply).contains(TOKEN), "{target}");
        }
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn only_get_is_served() {
        let (server, _) = make_server(200, false);
        let head = Head {
            method: "POST".into(),
            target: format!("/{TOKEN}/"),
            host: Some(format!("127.0.0.1:{PORT}")),
        };
        assert_eq!(server.respond(&head).status, 405);
    }

    #[test]
    fn the_page_is_served_under_the_token_with_its_api_base_pointed_here() {
        let (server, seen) = make_server(200, false);
        let reply = server.respond(&get(&format!("/{TOKEN}/")));
        assert_eq!(reply.status, 200);
        assert!(reply.content_type.starts_with("text/html"));
        let html = body_text(&reply);
        assert!(html.contains(&format!(r#"<meta name="lb-api" content="/{TOKEN}" />"#)));
        assert!(!html.contains(API_TOKEN));
        assert!(html.contains("Brain map"));
        assert!(seen.lock().unwrap().is_empty());
        assert_eq!(
            server.respond(&get(&format!("/{TOKEN}/brain.html"))).status,
            200
        );
    }

    #[test]
    fn assets_are_served_by_name_and_nothing_else_is() {
        let (server, _) = make_server(200, false);
        let js = server.respond(&get(&format!("/{TOKEN}/assets/app.js")));
        assert_eq!(js.status, 200);
        assert!(js.content_type.starts_with("text/javascript"));
        let css = server.respond(&get(&format!("/{TOKEN}/assets/brain.css")));
        assert!(css.content_type.starts_with("text/css"));
        for target in [
            format!("/{TOKEN}/assets/nope.js"),
            format!("/{TOKEN}/assets/../brain.html"),
            format!("/{TOKEN}/assets/%2e%2e/brain.html"),
            format!("/{TOKEN}/../../etc/passwd"),
            format!("/{TOKEN}/sw.js"),
            format!("/{TOKEN}/assets/tests/x.test.mjs"),
        ] {
            assert_eq!(server.respond(&get(&target)).status, 404, "{target}");
        }
    }

    #[test]
    fn the_map_reads_are_forwarded_with_their_query_and_never_echo_the_token() {
        let (server, seen) = make_server(200, false);
        let reply = server.respond(&get(&format!("/{TOKEN}/v1/graph?limit=2000&project=kaje")));
        assert_eq!(reply.status, 200);
        assert_eq!(body_text(&reply), r#"{"nodes":[],"edges":[]}"#);
        assert!(!body_text(&reply).contains(TOKEN));
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            ["/v1/graph?limit=2000&project=kaje"]
        );
        for target in [
            format!("/{TOKEN}/v1/graph/activity?since=2026-10-20T10%3A00%3A00Z"),
            format!("/{TOKEN}/v1/workspaces/me"),
            format!("/{TOKEN}/v1/pages/hello%20world"),
            format!("/{TOKEN}/v1/search?q=kestrel&limit=10"),
        ] {
            assert_eq!(server.respond(&get(&target)).status, 200, "{target}");
        }
    }

    #[test]
    fn anything_outside_the_allowlist_is_not_forwarded() {
        let (server, seen) = make_server(200, false);
        for target in [
            format!("/{TOKEN}/v1/tokens"),
            format!("/{TOKEN}/v1/admin/users"),
            format!("/{TOKEN}/v1/graph?limit=2000&evil=1"),
            format!("/{TOKEN}/v1/graph?limit=2000#frag"),
            format!("/{TOKEN}/v1/graph?limit=%27%22&project=a'b"),
            format!("/{TOKEN}/v1/pages/.."),
            format!("/{TOKEN}/v1/pages/a/b"),
            format!("/{TOKEN}/v1/workspaces/me?x=1"),
            format!("/{TOKEN}/v1/pages/"),
        ] {
            assert_eq!(server.respond(&get(&target)).status, 404, "{target}");
        }
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn a_signed_out_map_gets_the_sign_in_hint_and_an_unreachable_api_a_502() {
        let (server, _) = make_server(401, false);
        let reply = server.respond(&get(&format!("/{TOKEN}/v1/graph")));
        assert_eq!(reply.status, 401);
        assert!(body_text(&reply).contains("livingbrain login"));
        let (server, _) = make_server(200, true);
        let reply = server.respond(&get(&format!("/{TOKEN}/v1/graph")));
        assert_eq!(reply.status, 502);
        assert!(body_text(&reply).contains("could not reach the API"));
    }

    #[test]
    fn the_app_s_other_pages_redirect_to_the_hosted_app_without_the_token() {
        let (server, seen) = make_server(200, false);
        let reply = server.respond(&get(&format!("/{TOKEN}/wiki.html?slug=kestrel-routes")));
        assert_eq!(reply.status, 302);
        assert_eq!(
            reply.location.as_deref(),
            Some("https://app.example/wiki.html?slug=kestrel-routes")
        );
        let plain = server.respond(&get(&format!("/{TOKEN}/index.html")));
        assert_eq!(
            plain.location.as_deref(),
            Some("https://app.example/index.html")
        );
        let kept = server.respond(&get(&format!("/{TOKEN}/wiki.html?view=sessions&x=a%20b")));
        assert_eq!(
            kept.location.as_deref(),
            Some("https://app.example/wiki.html?view=sessions&x=a%20b")
        );
        let refused = server.respond(&get(&format!("/{TOKEN}/wiki.html?x=a\"b")));
        assert_eq!(
            refused.location.as_deref(),
            Some("https://app.example/wiki.html")
        );
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn the_head_is_parsed_and_a_duplicate_host_is_malformed() {
        let head = parse_head(b"GET /t/ HTTP/1.1\r\nHost: 127.0.0.1:1\r\nAccept: */*\r\n\r\n")
            .expect("a head");
        assert_eq!(head.method, "GET");
        assert_eq!(head.target, "/t/");
        assert_eq!(head.host.as_deref(), Some("127.0.0.1:1"));
        assert!(parse_head(b"GET / HTTP/1.1\r\nHost: a\r\nHost: b\r\n\r\n").is_none());
        assert!(parse_head(b"GET / SPDY/3\r\n\r\n").is_none());
        assert!(parse_head(b"GET / HTTP/1.1\r\nno-colon-here\r\n\r\n").is_none());
    }

    #[test]
    fn the_embedded_app_is_the_web_app_and_its_imports_all_resolve() {
        let names: Vec<&str> = FILES.iter().map(|(name, _)| *name).collect();
        assert!(names.contains(&"brain.html"));
        assert!(names.contains(&"assets/app.js"));
        assert!(
            names
                .iter()
                .all(|name| !name.contains("tests") && !name.ends_with("sw.js"))
        );
        let html = FILES.iter().find(|(n, _)| *n == "brain.html").unwrap().1;
        let html = std::str::from_utf8(html).unwrap();
        assert_eq!(html.matches(API_META).count(), 1);

        for (name, bytes) in FILES.iter().filter(|(name, _)| name.ends_with(".js")) {
            let text = std::str::from_utf8(bytes).expect("utf-8 module");
            for specifier in module_specifiers(text) {
                if !specifier.starts_with('.') {
                    continue;
                }
                let dir = name.rsplit_once('/').map_or("", |(dir, _)| dir);
                let resolved = normalise(&format!("{dir}/{specifier}"));
                assert!(
                    FILES.iter().any(|(path, _)| *path == resolved),
                    "{name} imports {specifier}, which is not embedded"
                );
            }
        }
    }

    /// The quoted specifiers of `import … from "x"`, `import "x"` and `import("x")`.
    fn module_specifiers(text: &str) -> Vec<String> {
        let mut out = Vec::new();
        for marker in [
            "from \"",
            "import \"",
            "import(\"",
            "from '",
            "import '",
            "import('",
        ] {
            let quote = marker.chars().last().unwrap();
            let mut rest = text;
            while let Some(index) = rest.find(marker) {
                let after = &rest[index + marker.len()..];
                if let Some(end) = after.find(quote) {
                    out.push(after[..end].to_owned());
                }
                rest = after;
            }
        }
        out
    }

    /// `a/./b/../c.js` → `a/c.js`.
    fn normalise(path: &str) -> String {
        let mut parts: Vec<&str> = Vec::new();
        for part in path.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                other => parts.push(other),
            }
        }
        parts.join("/")
    }

    #[test]
    fn the_inline_script_hash_matches_the_policy() {
        use sha2::{Digest, Sha256};
        let html = FILES.iter().find(|(n, _)| *n == "brain.html").unwrap().1;
        let html = std::str::from_utf8(html).unwrap();
        let start = html.find("<script>").expect("the theme bootstrap") + "<script>".len();
        let end = html[start..].find("</script>").expect("its end") + start;
        let digest = Sha256::digest(&html.as_bytes()[start..end]);
        let expected = format!("'sha256-{}'", base64_standard(&digest));
        assert!(
            CONTENT_SECURITY_POLICY.contains(&expected),
            "the CSP names a different hash than brain.html's inline script ({expected})"
        );
    }

    fn base64_standard(bytes: &[u8]) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            for index in 0..4 {
                if index <= chunk.len() {
                    out.push(ALPHABET[((n >> (18 - 6 * index)) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    #[derive(clap::Parser)]
    struct Probe {
        #[command(flatten)]
        args: ViewArgs,
    }

    #[test]
    fn the_flags_parse() {
        let plain = Probe::try_parse_from(["view"]).expect("no flags").args;
        assert_eq!(
            plain,
            ViewArgs {
                no_open: false,
                workspace: None
            }
        );
        let parsed = Probe::try_parse_from(["view", "--no-open", "--workspace", "ws-1"])
            .expect("both flags")
            .args;
        assert!(parsed.no_open);
        assert_eq!(parsed.workspace.as_deref(), Some("ws-1"));
        assert!(Probe::try_parse_from(["view", "--port", "8080"]).is_err());
        assert!(Probe::try_parse_from(["view", "--workspace"]).is_err());
    }
}
