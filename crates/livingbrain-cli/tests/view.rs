//! `livingbrain view` end to end: the real binary serves the brain map on
//! loopback, and the requests a browser would make go through its guards to a
//! mock API that records what it was sent.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Stdio};

use common::{Mock, Reply, run, sandboxed, temp_dir};

/// The stored API credential. It must reach the mock, and never the page.
const API_TOKEN: &str = "tok-view-api-credential-1234567890";

/// A child process that is killed when the test ends, pass or fail.
struct Viewer(Child);

impl Drop for Viewer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// One raw HTTP/1.1 exchange: status, the head as text, and the body as text.
fn http(port: u16, target: &str, host: &str) -> (u16, String, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to the view");
    write!(
        stream,
        "GET {target} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    )
    .expect("send request");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read response");
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .expect("a status line");
    (status, head.to_owned(), body.to_owned())
}

/// Start `livingbrain view` against `api` and return the child with the first
/// line it printed.
fn spawn_view(api: &str, extra: &[&str]) -> (Viewer, String) {
    let mut args = vec!["view"];
    args.extend_from_slice(extra);
    let home = temp_dir("view-home");
    let cwd = temp_dir("view-cwd");
    let mut command = sandboxed(
        &cwd,
        &home,
        &args,
        &[
            ("LIVINGBRAIN_API_URL", api),
            ("LIVINGBRAIN_TOKEN", API_TOKEN),
            ("LIVINGBRAIN_NO_KEYCHAIN", "1"),
        ],
    );
    command.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = command.spawn().expect("spawn livingbrain view");
    let mut first = String::new();
    BufReader::new(child.stdout.take().expect("stdout"))
        .read_line(&mut first)
        .expect("the first line");
    (Viewer(child), first.trim().to_owned())
}

/// Start `livingbrain view` (human output) and return the child with the port,
/// the run's path token and the URL.
fn start_view(api: &str, extra: &[&str]) -> (Viewer, u16, String, String) {
    let (viewer, first) = spawn_view(api, extra);
    let url = first
        .strip_prefix("Living Brain view: ")
        .unwrap_or_else(|| panic!("unexpected first line: {first:?}"))
        .to_owned();
    let rest = url.strip_prefix("http://127.0.0.1:").expect("loopback URL");
    let (port, path) = rest.split_once('/').expect("a path");
    let port: u16 = port.parse().expect("a port");
    let run_token = path.trim_end_matches('/').to_owned();
    (viewer, port, run_token, url)
}

fn api_mock() -> Mock {
    Mock::start(|request| match request.path.as_str() {
        "/v1/workspaces/me" => Reply::ok_json(r#"{"workspace":{"id":"ws-test"}}"#),
        "/v1/graph" => Reply::ok_json(r#"{"nodes":[{"id":"p1","label":"Kestrel"}],"edges":[]}"#),
        _ => Reply::json(404, r#"{"error":"no such route"}"#),
    })
}

#[test]
fn the_map_is_served_under_the_run_token_and_the_api_sees_the_credential() {
    let api = api_mock();
    let (_viewer, port, token, url) = start_view(&api.base, &["--no-open"]);
    let host = format!("127.0.0.1:{port}");

    let (status, head, page) = http(port, &format!("/{token}/"), &host);
    assert_eq!(status, 200, "{head}");
    assert!(page.contains("Brain map"));
    assert!(page.contains(&format!(r#"content="/{token}""#)));
    assert!(
        !page.contains(API_TOKEN),
        "the page must never carry the credential"
    );
    assert!(
        !head
            .to_ascii_lowercase()
            .contains("access-control-allow-origin")
    );
    assert!(head.contains("Content-Security-Policy: default-src 'self'"));

    let (status, _, body) = http(port, &format!("/{token}/v1/graph?limit=2000"), &host);
    assert_eq!(status, 200);
    assert!(body.contains("Kestrel"));
    assert!(!body.contains(API_TOKEN));

    let graph = api
        .requests()
        .into_iter()
        .find(|request| request.path == "/v1/graph")
        .expect("the API was asked for the graph");
    assert_eq!(graph.query, "limit=2000");
    assert_eq!(graph.bearer().as_deref(), Some(API_TOKEN));
    assert!(url.ends_with(&format!("/{token}/")));
}

#[test]
fn a_request_without_the_run_token_never_reaches_the_api() {
    let api = api_mock();
    let (_viewer, port, _token, _url) = start_view(&api.base, &["--no-open"]);
    let host = format!("127.0.0.1:{port}");
    let before = api.requests().len();

    let (status, _, body) = http(port, "/v1/graph?limit=2000", &host);
    assert_eq!(status, 404);
    assert!(!body.contains("Kestrel"));
    let (status, _, _) = http(port, "/wrong-token/v1/graph", &host);
    assert_eq!(status, 404);
    assert_eq!(api.requests().len(), before, "the API saw nothing");
}

#[test]
fn a_foreign_host_is_refused_even_with_the_run_token() {
    let api = api_mock();
    let (_viewer, port, token, _url) = start_view(&api.base, &["--no-open"]);
    let before = api.requests().len();

    let (status, _, _) = http(port, &format!("/{token}/v1/graph"), "attacker.example");
    assert_eq!(status, 403);
    let (status, _, _) = http(port, &format!("/{token}/"), "attacker.example:");
    assert_eq!(status, 403);
    assert_eq!(api.requests().len(), before);
}

#[test]
fn json_mode_prints_the_url_as_one_object() {
    let api = api_mock();
    let (_viewer, first) = spawn_view(&api.base, &["--no-open", "--json"]);
    let value: serde_json::Value = serde_json::from_str(&first).expect("one JSON object");
    let url = value["url"].as_str().expect("a url");
    assert!(url.starts_with("http://127.0.0.1:"), "{url}");
    assert_eq!(value["opened"], false);
}

#[test]
fn without_a_login_the_view_says_so_and_serves_nothing() {
    let api = api_mock();
    let home = temp_dir("view-no-login");
    let cwd = temp_dir("view-no-login-cwd");
    let output = run(
        &cwd,
        &home,
        &["view", "--no-open"],
        &[("LIVINGBRAIN_API_URL", &api.base)],
        None,
    );
    assert_eq!(output.status.code(), Some(2), "{}", output.stderr);
    assert!(output.stderr.contains("not logged in"));
    assert!(output.stdout.is_empty());
}
