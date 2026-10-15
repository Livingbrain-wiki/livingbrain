//! Integration tests for `livingbrain import markdown`: the real binary
//! against a local mock API and a fixture vault on disk.

mod common;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use common::{Mock, Output, Reply, run, temp_dir};

/// The token the tests hand the binary through the environment.
const TOKEN: &str = "tok-import-bearer-1234567890";

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

fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A mock that answers `POST /v1/pages/sources` the way the server does:
/// `201 created:true` the first time it sees a body's SHA-256, `200
/// created:false` after. The set is shared across runs, so importing the same
/// vault twice against one mock exercises the idempotency key.
fn sources_mock() -> Mock {
    let seen: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let handler = move |request: &common::Request| {
        if request.path != "/v1/pages/sources" {
            return Reply::json(404, r#"{"error":"no such route"}"#);
        }
        let body: Value = serde_json::from_str(&request.body_text()).unwrap_or(Value::Null);
        let text = body["body"].as_str().unwrap_or("");
        let digest = sha256_hex(text);
        let created = seen.lock().expect("seen lock").insert(digest.clone());
        let reply = json!({
            "id": format!("src-{}", &digest[..12]),
            "kind": "import",
            "scope": body["scope"].clone(),
            "path": body["path"].clone(),
            "sha256": digest,
            "wikilinks": Vec::<String>::new(),
            "redacted": 0,
            "created": created,
            "created_at": "2026-04-15T09:41:02Z",
        });
        Reply::json(if created { 201 } else { 200 }, &reply.to_string())
    };
    Mock::start(handler)
}

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent");
    }
    std::fs::write(path, contents).expect("write fixture file");
}

/// The fixture vault: three real notes (one nested, one with wikilinks), and
/// one of every kind of file the walk is supposed to pass over.
fn fixture_vault() -> PathBuf {
    let vault = temp_dir("vault");
    write(
        &vault.join("index.md"),
        "# Index\n\nStart at [[daily/2026-04-15]].\n",
    );
    write(
        &vault.join("daily/2026-04-15.md"),
        "# Wednesday\n\nNothing special.\n",
    );
    write(
        &vault.join("daily/notes.markdown"),
        "## Scratch\n\n- one\n- two\n",
    );
    // Ignored by the vault's own `.gitignore`, honoured without a git repo.
    write(&vault.join(".gitignore"), "ignored.md\n");
    write(&vault.join("ignored.md"), "# Ignored\n\nNever uploaded.\n");
    // A file that is text and is not notes: passed over for its type.
    write(&vault.join("diagram.svg"), "<svg/>\n");
    // A `.md` that is not text at all: the NUL is what makes it binary.
    std::fs::write(vault.join("binary.md"), b"# Bin\0\xff\xfe\n").expect("write binary file");
    // A vault's own bookkeeping, hidden and skipped.
    write(&vault.join(".obsidian/app.json"), "{\"x\":1}\n");
    // Larger than the `--max-bytes` the test passes.
    write(&vault.join("huge.md"), &"filler\n".repeat(200));
    vault
}

/// Every request body the mock saw, as parsed JSON.
fn source_requests(mock: &Mock) -> Vec<Value> {
    mock.requests()
        .into_iter()
        .filter(|request| request.path == "/v1/pages/sources")
        .map(|request| serde_json::from_str(&request.body_text()).expect("a JSON source body"))
        .collect()
}

/// The uploaded paths, in the order they were sent.
fn uploaded_paths(bodies: &[Value]) -> Vec<String> {
    bodies
        .iter()
        .map(|body| body["path"].as_str().expect("a path").to_owned())
        .collect()
}

/// (1) A fixture vault imports once: the second run finds every source already
/// present, and the files the walk skipped were never uploaded.
#[test]
fn a_fixture_vault_imported_twice_creates_sources_once() {
    let mock = sources_mock();
    let (cwd, home) = (temp_dir("import-cwd"), temp_dir("import-home"));
    let envs = api_envs(&mock.base);
    let vault = fixture_vault();
    let dir = vault.display().to_string();

    let first = run(
        &cwd,
        &home,
        &[
            "import",
            "markdown",
            &dir,
            "--yes",
            "--json",
            "--max-bytes",
            "64",
        ],
        &envs,
        None,
    );
    assert!(first.status.success(), "stderr: {}", first.stderr);
    let first_value = json_of(&first);
    assert_eq!(first_value["scope"], "personal");
    assert_eq!(first_value["files"], 3);
    assert_eq!(first_value["created"], 3);
    assert_eq!(first_value["unchanged"], 0);
    // `binary.md` (NUL), `huge.md` (over the flag) and `diagram.svg` are the
    // three the walk passed over; `ignored.md` and `.obsidian/app.json` are
    // never walked at all.
    assert_eq!(first_value["skipped"]["binary"], 1, "{first_value}");
    assert_eq!(first_value["skipped"]["too_large"], 1, "{first_value}");
    assert_eq!(first_value["skipped"]["not_markdown"], 1, "{first_value}");
    assert_eq!(first_value["sources"].as_array().expect("sources").len(), 3);

    // The preview is on stderr, never on stdout, so `--json` stdout stays one
    // object.
    assert!(first.stderr.contains("Import preview"), "{}", first.stderr);

    let second = run(
        &cwd,
        &home,
        &[
            "import",
            "markdown",
            &dir,
            "--yes",
            "--json",
            "--max-bytes",
            "64",
        ],
        &envs,
        None,
    );
    assert!(second.status.success(), "stderr: {}", second.stderr);
    let second_value = json_of(&second);
    assert_eq!(second_value["created"], 0, "{second_value}");
    assert_eq!(second_value["unchanged"], 3, "{second_value}");

    let requests = mock.requests();
    let bodies = source_requests(&mock);
    assert_eq!(bodies.len(), 6, "three sources per run, twice");
    for request in &requests {
        assert_eq!(request.bearer().as_deref(), Some(TOKEN));
    }
    for body in &bodies {
        assert_eq!(body["kind"], "import");
        assert_eq!(body["scope"], "personal");
    }
    // Paths are vault-relative and `/`-separated, whatever the host is.
    let paths = uploaded_paths(&bodies);
    assert_eq!(paths[0], "daily/2026-04-15.md");
    assert_eq!(paths[1], "daily/notes.markdown");
    assert_eq!(paths[2], "index.md");
    assert!(paths.iter().all(|path| !path.contains('\\')));

    // Nothing that was skipped, ignored or hidden ever crossed the socket.
    for path in [
        "ignored.md",
        "binary.md",
        "huge.md",
        "app.json",
        "diagram.svg",
    ] {
        assert!(
            !paths.iter().any(|uploaded| uploaded.contains(path)),
            "{path} was uploaded"
        );
    }
}

/// (2) Nothing is uploaded before the confirmation: `n` and a closed stdin both
/// abort with no request at all, and `y` proceeds.
#[test]
fn nothing_uploads_before_confirmation() {
    let mock = sources_mock();
    let (cwd, home) = (temp_dir("confirm-cwd"), temp_dir("confirm-home"));
    let envs = api_envs(&mock.base);
    let vault = temp_dir("confirm-vault");
    write(&vault.join("one.md"), "# One\n");
    let dir = vault.display().to_string();
    let args = ["import", "markdown", &dir, "--json"];

    let declined = run(&cwd, &home, &args, &envs, Some("n\n"));
    assert!(!declined.status.success(), "{}", declined.stdout);
    assert!(declined.stderr.contains("cancelled"), "{}", declined.stderr);
    assert!(
        mock.requests().is_empty(),
        "a declined import made a request"
    );

    // No stdin at all: an EOF is a no, not a yes.
    let eof = run(&cwd, &home, &args, &envs, None);
    assert!(!eof.status.success());
    assert!(eof.stderr.contains("cancelled"), "{}", eof.stderr);
    assert!(mock.requests().is_empty(), "an EOF import made a request");

    let accepted = run(&cwd, &home, &args, &envs, Some("y\n"));
    assert!(accepted.status.success(), "stderr: {}", accepted.stderr);
    assert_eq!(json_of(&accepted)["created"], 1);
    assert_eq!(source_requests(&mock).len(), 1);

    // The prompt names the target, on stderr.
    assert!(
        accepted
            .stderr
            .contains("Upload 1 files to your personal brain?"),
        "{}",
        accepted.stderr
    );

    // `--shared` sends the other scope, and names the shared brain.
    let shared = run(
        &cwd,
        &home,
        &["import", "markdown", &dir, "--yes", "--json", "--shared"],
        &envs,
        None,
    );
    assert!(shared.status.success(), "stderr: {}", shared.stderr);
    assert_eq!(json_of(&shared)["scope"], "shared");
    let last = source_requests(&mock).pop().expect("a source request");
    assert_eq!(last["scope"], "shared");
}

/// (3) A secret planted in a vault is redacted locally: what the mock received
/// carries the replacement token, and never the secret.
///
/// The key is assembled at runtime from pieces so no real-looking credential is
/// committed to the repository; `AKIA` plus 16 uppercase alphanumerics is the
/// AWS access key id shape the detector matches, and `EXAMPLE` is AWS's own
/// documentation value, so this is not a live credential either way.
#[test]
fn a_planted_secret_is_redacted_before_upload() {
    let key = ["AKIA", "IOSFODNN", "7EXAMPLE"].concat();
    let mock = sources_mock();
    let (cwd, home) = (temp_dir("redact-cwd"), temp_dir("redact-home"));
    let envs = api_envs(&mock.base);
    let vault = temp_dir("redact-vault");
    write(
        &vault.join("deploy.md"),
        &format!("# Deploy\n\nAWS_ACCESS_KEY_ID={key}\nregion eu-west-1\n"),
    );
    let dir = vault.display().to_string();

    let output = run(
        &cwd,
        &home,
        &["import", "markdown", &dir, "--yes"],
        &envs,
        None,
    );
    assert!(output.status.success(), "stderr: {}", output.stderr);
    assert!(output.stdout.contains("1 redacted"), "{}", output.stdout);

    let sent = source_requests(&mock);
    let body = sent.first().expect("a source request")["body"]
        .as_str()
        .expect("the body")
        .to_owned();
    assert!(body.contains("[REDACTED:"), "{body}");
    assert!(
        !body.contains(&key),
        "the secret crossed the socket: {body}"
    );
    // The mock's own idempotency key is over the redacted text, so the secret
    // is not in the reply either.
    assert!(
        !output.stdout.contains(&key) && !output.stderr.contains(&key),
        "the secret reached a stream"
    );
}

/// (5) A refusal reaches stderr as the server's own sentence. The modules
/// answer an RFC 9457 problem (`title`, `detail`) as
/// `application/problem+json`, so the CLI prints the detail rather than the
/// JSON document, and a 401 still carries the login hint.
#[test]
fn a_refusal_shows_the_problem_detail_not_the_json() {
    let mock = Mock::start(|request: &common::Request| {
        if request.path != "/v1/pages/sources" {
            return Reply::json(404, r#"{"error":"no such route"}"#);
        }
        Reply::bytes(
            403,
            "application/problem+json",
            br#"{"type":"https://livingbrain.dev/problems/sources/not-your-scope",
                 "title":"Not your scope",
                 "status":403,
                 "detail":"the shared brain is not in your grant"}"#
                .to_vec(),
        )
    });
    let (cwd, home) = (temp_dir("403-cwd"), temp_dir("403-home"));
    let envs = api_envs(&mock.base);
    let vault = temp_dir("403-vault");
    write(&vault.join("one.md"), "# One\n");

    let refused = run(
        &cwd,
        &home,
        &["import", "markdown", &vault.display().to_string(), "--yes"],
        &envs,
        None,
    );
    assert!(!refused.status.success(), "{}", refused.stdout);
    let stderr = &refused.stderr;
    assert!(
        stderr.contains("the shared brain is not in your grant"),
        "{stderr}"
    );
    assert!(stderr.contains("Not your scope"), "{stderr}");
    // The document itself is not what the reader is shown: the file names the
    // failure, then the server's own sentence.
    assert!(!stderr.contains("\"detail\""), "{stderr}");
    assert!(!stderr.contains("problem+json"), "{stderr}");
    assert!(!stderr.contains('{'), "the raw body was printed: {stderr}");

    // The same shape with a 401 keeps the login hint.
    let unauthorized = Mock::start(|_| {
        Reply::bytes(
            401,
            "application/problem+json",
            br#"{"title":"Unauthorized","status":401,"detail":"no such token"}"#.to_vec(),
        )
    });
    let unauth = run(
        &cwd,
        &home,
        &["import", "markdown", &vault.display().to_string(), "--yes"],
        &api_envs(&unauthorized.base),
        None,
    );
    assert!(!unauth.status.success());
    assert!(unauth.stderr.contains("no such token"), "{}", unauth.stderr);
    assert!(
        unauth.stderr.contains("livingbrain login"),
        "{}",
        unauth.stderr
    );
}

/// (6) `--exclude` takes a glob and keeps what it matches off the wire, and a
/// directory with no candidates at all is a successful no-op.
#[test]
fn exclude_globs_keep_files_off_the_wire_and_an_empty_vault_uploads_nothing() {
    let mock = sources_mock();
    let (cwd, home) = (temp_dir("exclude-cwd"), temp_dir("exclude-home"));
    let envs = api_envs(&mock.base);
    let vault = temp_dir("exclude-vault");
    write(&vault.join("keep.md"), "# Keep\n");
    write(&vault.join("drafts/wip.md"), "# WIP\n");
    let dir = vault.display().to_string();

    let output = run(
        &cwd,
        &home,
        &[
            "import",
            "markdown",
            &dir,
            "--yes",
            "--json",
            "--exclude",
            "drafts/**",
        ],
        &envs,
        None,
    );
    assert!(output.status.success(), "stderr: {}", output.stderr);
    let value = json_of(&output);
    assert_eq!(value["files"], 1, "{value}");
    assert_eq!(uploaded_paths(&source_requests(&mock)), ["keep.md"]);

    let empty = temp_dir("empty-vault");
    let empty_dir = empty.display().to_string();
    let none = run(
        &cwd,
        &home,
        &["import", "markdown", &empty_dir, "--yes", "--json"],
        &envs,
        None,
    );
    assert!(none.status.success(), "stderr: {}", none.stderr);
    assert_eq!(json_of(&none)["files"], 0);
    assert_eq!(
        source_requests(&mock).len(),
        1,
        "the empty vault made a request"
    );
}
