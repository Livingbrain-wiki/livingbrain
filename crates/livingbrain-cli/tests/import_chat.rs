//! Integration tests for `livingbrain import chatgpt` and
//! `livingbrain import claude`: the real binary against a local mock API, over
//! export zips built at test time from the committed `conversations.json`
//! fixtures, so no binary fixture is ever committed.

mod common;

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

use common::{Mock, Output, Reply, run, temp_dir};

/// The token the tests hand the binary through the environment.
const TOKEN: &str = "tok-import-chat-1234567890";

/// The obviously fake key the ChatGPT fixture plants in a user message: the
/// `sk-proj-` shape the redactor detects, spelled entirely in `test`. Not a
/// credential; that is the point.
const FAKE_KEY: &str = "sk-proj-testtesttesttesttesttesttesttesttesttesttesttest";

/// The fixture's conversation ids: the branched ChatGPT chat (`ALPHA`), the
/// flat one (`BETA`), and the branched and old-format Claude chats.
const ALPHA: &str = "9f1c2a3b-4d5e-4f60-8a71-2b3c4d5e6f01";
const BETA: &str = "7c2d3e4f-5a6b-4c70-9d81-3e4f5a6b7c02";
const CLAUDE_BRANCH: &str = "c3d4e5f6-a1b2-4c30-8d90-4a5b6c7d8e01";
const CLAUDE_OLD: &str = "e4f5a6b7-c8d9-4e40-9a10-5b6c7d8e9f02";

/// The messages each conversation keeps — the live branches only.
const ALPHA_MESSAGES: [&str; 5] = [
    "aa100000-0000-4000-8000-000000000001",
    "aa200000-0000-4000-8000-000000000002",
    "aa300000-0000-4000-8000-000000000003",
    "aa4b0000-0000-4000-8000-000000000005",
    "aa500000-0000-4000-8000-000000000006",
];
const BETA_MESSAGES: [&str; 2] = [
    "bb100000-0000-4000-8000-000000000001",
    "bb200000-0000-4000-8000-000000000002",
];
const CLAUDE_BRANCH_MESSAGES: [&str; 4] = [
    "d1000000-0000-4000-8000-000000000001",
    "d2000000-0000-4000-8000-000000000002",
    "d5000000-0000-4000-8000-000000000005",
    "d6000000-0000-4000-8000-000000000006",
];
const CLAUDE_OLD_MESSAGES: [&str; 2] = [
    "f1000000-0000-4000-8000-000000000001",
    "f2000000-0000-4000-8000-000000000002",
];

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
/// created:false` after. Also hands back the set of digests it has seen, so a
/// test can read the state the dedupe rests on.
fn sources_mock() -> (Mock, Arc<Mutex<HashSet<String>>>) {
    let seen: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let digests = Arc::clone(&seen);
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
    (Mock::start(handler), digests)
}

/// Every request body the mock saw, as parsed JSON.
fn source_requests(mock: &Mock) -> Vec<Value> {
    mock.requests()
        .into_iter()
        .filter(|request| request.path == "/v1/pages/sources")
        .map(|request| serde_json::from_str(&request.body_text()).expect("a JSON source body"))
        .collect()
}

/// The committed `conversations.json` of one format.
fn fixture(format: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/import")
        .join(format)
        .join("conversations.json");
    std::fs::read(&path).unwrap_or_else(|e| panic!("read fixture {}: {e}", path.display()))
}

/// Zip the fixture into a fresh temp file under the entry path `inner`, which
/// exercises the reader's root and top-level-directory lookups.
fn zipped(format: &str, inner: &str) -> PathBuf {
    let path = temp_dir("chat-export").join("export.zip");
    let file = std::fs::File::create(&path).expect("create the export zip");
    let mut zip = ZipWriter::new(file);
    zip.start_file(inner, SimpleFileOptions::default())
        .expect("start the zip entry");
    zip.write_all(&fixture(format))
        .expect("write the zip entry");
    zip.finish().expect("finish the zip");
    path
}

/// The committed fixture by its own path: a bare `conversations.json` is a
/// valid `EXPORT` argument, no zip required.
fn bare_fixture(format: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/import")
        .join(format)
        .join("conversations.json")
        .display()
        .to_string()
}

/// Every message of `conversation` is citable in `body` by its citation key —
/// the anchor pages generated from the source must carry.
fn assert_citations(body: &str, conversation: &str, messages: &[&str]) {
    for message in messages {
        let key = format!("conversation:{conversation}#{message}");
        assert!(body.contains(&key), "the body is missing {key}:\n{body}");
    }
}

/// (1, 7) The ChatGPT fixture imports: the branched conversation keeps only its
/// `current_node` branch, secrets are redacted before the wire, the preview
/// says so, and every message is citable.
#[test]
fn chatgpt_fixture_imports_only_the_current_branch() {
    assert!(
        fixture("chatgpt")
            .windows(FAKE_KEY.len())
            .any(|w| w == FAKE_KEY.as_bytes()),
        "the fixture is expected to plant the fake key"
    );
    let (mock, _seen) = sources_mock();
    let (cwd, home) = (temp_dir("chatgpt-cwd"), temp_dir("chatgpt-home"));
    let envs = api_envs(&mock.base);
    let zip = zipped("chatgpt", "conversations.json");

    let output = run(
        &cwd,
        &home,
        &[
            "import",
            "chatgpt",
            zip.to_str().unwrap(),
            "--yes",
            "--json",
        ],
        &envs,
        None,
    );
    assert!(output.status.success(), "stderr: {}", output.stderr);
    let value = json_of(&output);
    assert_eq!(value["source"], "chatgpt");
    assert_eq!(value["scope"], "personal");
    assert_eq!(value["conversations"], 2, "{value}");
    assert_eq!(value["created"], 2, "{value}");
    assert_eq!(value["unchanged"], 0, "{value}");

    // One POST per conversation, oldest first.
    let bodies = source_requests(&mock);
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[0]["path"], format!("chatgpt/{BETA}"));
    assert_eq!(bodies[1]["path"], format!("chatgpt/{ALPHA}"));

    // The live branch travels, with its model slug and the attachment name;
    // the abandoned sibling, the hidden system prompt and the raw
    // asset-pointer object never do.
    let alpha = bodies[1]["body"].as_str().expect("the alpha body");
    assert!(alpha.contains("Answer B: batch your requests"), "{alpha}");
    assert!(alpha.contains("gpt-4o"), "{alpha}");
    assert!(alpha.contains("Attachments: screenshot.png"), "{alpha}");
    assert!(!alpha.contains("Answer A"), "{alpha}");
    assert!(!alpha.contains("You are ChatGPT"), "{alpha}");
    let beta = bodies[0]["body"].as_str().expect("the beta body");
    assert!(!beta.contains("size_bytes"), "{beta}");
    // The non-string part became an attachment reference, not prose.
    assert!(
        beta.contains("Attachments: file-service://file-demo-0001"),
        "{beta}"
    );

    // The planted secret and the email are redacted locally: what crossed the
    // socket carries the replacement tokens, never the values.
    let joined = format!("{beta}\n{alpha}");
    assert!(
        !joined.contains(FAKE_KEY),
        "the key reached the wire: {joined}"
    );
    assert!(!joined.contains("alice@example.com"), "{joined}");
    assert!(joined.contains("[REDACTED:openai_key]"), "{joined}");
    assert!(joined.contains("[REDACTED:email]"), "{joined}");

    // Every kept message is citable in its own conversation's body.
    assert_citations(alpha, ALPHA, &ALPHA_MESSAGES);
    assert_citations(beta, BETA, &BETA_MESSAGES);

    // The preview is on stderr and names the classes it redacted.
    assert!(
        output.stderr.contains("Import preview for chatgpt export"),
        "{}",
        output.stderr
    );
    assert!(output.stderr.contains("openai_key: 1"), "{}", output.stderr);
    assert!(output.stderr.contains("email: 1"), "{}", output.stderr);
}

/// (1) The Claude fixture imports: the branched conversation keeps the latest
/// leaf's branch, the old-format conversation falls back to its bare `text`,
/// and the zip's single top-level directory is seen through.
#[test]
fn claude_fixture_imports_the_latest_branch_and_the_old_format() {
    let (mock, _seen) = sources_mock();
    let (cwd, home) = (temp_dir("claude-cwd"), temp_dir("claude-home"));
    let envs = api_envs(&mock.base);
    let zip = zipped("claude", "claude-export/conversations.json");

    let output = run(
        &cwd,
        &home,
        &["import", "claude", zip.to_str().unwrap(), "--yes", "--json"],
        &envs,
        None,
    );
    assert!(output.status.success(), "stderr: {}", output.stderr);
    let value = json_of(&output);
    assert_eq!(value["source"], "claude");
    assert_eq!(value["conversations"], 2, "{value}");
    assert_eq!(value["created"], 2, "{value}");

    // Oldest first: the old-format chat (April 20) precedes the branched one
    // (May 1).
    let bodies = source_requests(&mock);
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[0]["path"], format!("claude/{CLAUDE_OLD}"));
    assert_eq!(bodies[1]["path"], format!("claude/{CLAUDE_BRANCH}"));

    // The latest leaf's branch survives whole — root answer included; the
    // earlier Postgres leaf does not.
    let branched = bodies[1]["body"].as_str().expect("the branched body");
    assert!(
        branched.contains("SQLite plan: ship the schema to SQLite"),
        "{branched}"
    );
    assert!(
        branched.contains("Actually, try SQLite first"),
        "{branched}"
    );
    assert!(branched.contains("Attachments: schema.dmp"), "{branched}");
    assert!(branched.contains("Here is a plan: step one"), "{branched}");
    assert!(!branched.contains("Postgres plan"), "{branched}");

    // The older format's bare `text` is read when there are no content blocks.
    let old = bodies[0]["body"].as_str().expect("the old-format body");
    assert!(old.contains("Hello there. I attached my notes."), "{old}");
    assert!(old.contains("Attachments: notes.txt"), "{old}");

    // The header names the source, the parser version and the conversation.
    assert!(branched.contains("- source: claude"), "{branched}");
    assert!(branched.contains("- parser-version: 1"), "{branched}");
    assert!(
        branched.contains(&format!("- conversation: {CLAUDE_BRANCH}")),
        "{branched}"
    );
    assert!(
        branched.contains("- created: 2026-05-01T10:00:00"),
        "{branched}"
    );

    assert_citations(branched, CLAUDE_BRANCH, &CLAUDE_BRANCH_MESSAGES);
    assert_citations(old, CLAUDE_OLD, &CLAUDE_OLD_MESSAGES);
}

/// (2) Nothing is uploaded before the confirmation: `n` and a closed stdin both
/// abort with no request at all, and `y` proceeds.
#[test]
fn nothing_uploads_before_confirmation() {
    let (mock, _seen) = sources_mock();
    let (cwd, home) = (temp_dir("chat-confirm-cwd"), temp_dir("chat-confirm-home"));
    let envs = api_envs(&mock.base);
    let zip = zipped("chatgpt", "conversations.json");
    let args = ["import", "chatgpt", zip.to_str().unwrap(), "--json"];

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
    assert!(mock.requests().is_empty(), "an EOF import made a request");

    let accepted = run(&cwd, &home, &args, &envs, Some("y\n"));
    assert!(accepted.status.success(), "stderr: {}", accepted.stderr);
    assert_eq!(json_of(&accepted)["created"], 2);
    assert_eq!(source_requests(&mock).len(), 2);
    assert!(
        accepted
            .stderr
            .contains("Upload 2 files to your personal brain?"),
        "{}",
        accepted.stderr
    );
}

/// (3) The selection flags narrow the upload locally, and a malformed date is
/// refused before the export is even read.
#[test]
fn selection_flags_narrow_what_uploads() {
    let (cwd, home) = (temp_dir("chat-select-cwd"), temp_dir("chat-select-home"));

    // `--keyword` keeps only the matching conversation; the fixture goes in by
    // its bare `conversations.json` path, no zip.
    let (keyword_mock, _) = sources_mock();
    let json = bare_fixture("chatgpt");
    let keyword = run(
        &cwd,
        &home,
        &[
            "import",
            "chatgpt",
            &json,
            "--yes",
            "--json",
            "--keyword",
            "weekend",
        ],
        &api_envs(&keyword_mock.base),
        None,
    );
    assert!(keyword.status.success(), "stderr: {}", keyword.stderr);
    assert_eq!(json_of(&keyword)["conversations"], 1, "{}", keyword.stdout);
    let bodies = source_requests(&keyword_mock);
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["path"], format!("chatgpt/{BETA}"));

    // `--since` keeps the newer one only.
    let (since_mock, _) = sources_mock();
    let since = run(
        &cwd,
        &home,
        &[
            "import",
            "chatgpt",
            &json,
            "--yes",
            "--json",
            "--since",
            "2026-04-10",
        ],
        &api_envs(&since_mock.base),
        None,
    );
    assert!(since.status.success(), "stderr: {}", since.stderr);
    let bodies = source_requests(&since_mock);
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["path"], format!("chatgpt/{ALPHA}"));

    // A malformed date is a flag error, before any file is read.
    let requests_before = since_mock.requests().len();
    let bad = run(
        &cwd,
        &home,
        &["import", "chatgpt", &json, "--yes", "--since", "15/04/2026"],
        &api_envs(&since_mock.base),
        None,
    );
    assert!(!bad.status.success());
    assert!(bad.stderr.contains("YYYY-MM-DD"), "{}", bad.stderr);
    assert_eq!(
        since_mock.requests().len(),
        requests_before,
        "a bad date must not reach the wire"
    );
}

/// (4) Re-importing the same export: every upload comes back unchanged, and
/// the mock's dedupe set holds one row per conversation — no duplicates.
#[test]
fn reimporting_the_same_export_creates_nothing_new() {
    let (mock, seen) = sources_mock();
    let (cwd, home) = (temp_dir("chat-re-cwd"), temp_dir("chat-re-home"));
    let envs = api_envs(&mock.base);
    let zip = zipped("claude", "conversations.json");
    let args = ["import", "claude", zip.to_str().unwrap(), "--yes", "--json"];

    let first = run(&cwd, &home, &args, &envs, None);
    assert!(first.status.success(), "stderr: {}", first.stderr);
    let value = json_of(&first);
    assert_eq!(value["created"], 2, "{value}");
    assert_eq!(value["unchanged"], 0, "{value}");

    let second = run(&cwd, &home, &args, &envs, None);
    assert!(second.status.success(), "stderr: {}", second.stderr);
    let value = json_of(&second);
    assert_eq!(value["created"], 0, "{value}");
    assert_eq!(value["unchanged"], 2, "{value}");

    // Four uploads, two distinct bodies — the second run sent byte-identical
    // parts and the ledger's `(scope, sha256)` key folded them.
    assert_eq!(seen.lock().expect("seen lock").len(), 2);
    let bodies = source_requests(&mock);
    assert_eq!(bodies.len(), 4);
    let digests: HashSet<String> = bodies
        .iter()
        .map(|body| sha256_hex(body["body"].as_str().expect("the body")))
        .collect();
    assert_eq!(digests.len(), 2, "the bodies were not deterministic");
}
