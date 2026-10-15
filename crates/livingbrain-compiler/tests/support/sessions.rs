//! Synthetic but realistic session records, in the collector's `full`
//! shape (`livingbrain logs sync`): tool calls with `format`, `path`,
//! `input`, `output` (clipped to 4 KiB head and tail), `status` and
//! `exit_code`. Shared by the triage and compiler tests.

#![allow(dead_code)]

use serde_json::{Value, json};

/// A tool call as the collector writes it.
pub fn call(name: &str, path: Option<&str>, input: &str, output: &str, exit: Option<i64>) -> Value {
    let mut call = json!({"name": name, "input": input, "output": clip(output)});
    if let Some(path) = path {
        call["path"] = json!(path);
    }
    if name == "Bash" {
        call["format"] = json!("shell");
    }
    match exit {
        Some(0) => {
            call["status"] = json!("ok");
            call["exit_code"] = json!(0);
        }
        Some(code) => {
            call["status"] = json!("error");
            call["exit_code"] = json!(code);
        }
        None => call["status"] = json!("ok"),
    }
    call
}

/// The collector's own clip: 4 KiB, head and tail.
fn clip(output: &str) -> String {
    if output.len() <= 4_096 {
        return output.to_owned();
    }
    let head: String = output.chars().take(2_000).collect();
    let tail: String = output
        .chars()
        .rev()
        .take(2_000)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}\n… [clipped]\n{tail}")
}

pub fn turn(role: &str, text: &str, calls: Vec<Value>) -> Value {
    json!({"role": role, "at": "2026-10-15T09:00:00Z", "text": text, "tool_calls": calls})
}

pub fn record(id: &str, turns: Vec<Value>) -> String {
    json!({
        "schema_version": 2, "agent": "claude_code", "collector_version": 4, "detail": "full",
        "session_id": id, "repo": "~/code/demo", "branch": "main", "models": ["claude-opus-5-5"],
        "started_at": "2026-10-15T09:00:00Z", "ended_at": "2026-10-15T10:00:00Z",
        "turns": turns, "totals": {"input_tokens": 120000, "output_tokens": 9000},
        "outcome": null, "truncated": false, "omitted_turns": 0
    })
    .to_string()
}

/// `cargo build` noise: a page of `Compiling` lines, then the end.
pub fn cargo_build(crates: usize, tail: &str) -> String {
    let mut out = String::new();
    for i in 0..crates {
        out.push_str(&format!("   Compiling dependency-{i} v0.{}.{i}\n", i % 9));
    }
    out.push_str(tail);
    out
}

/// A failing `cargo test` run: the progress, one failure, the summary.
pub fn cargo_test_failure(passed: usize) -> String {
    let mut out = String::from("running 48 tests\n");
    for i in 0..passed {
        out.push_str(&format!("test store::tests::case_{i} ... ok\n"));
    }
    out.push_str("test store::tests::migrates_d1 ... FAILED\n\nfailures:\n\n---- store::tests::migrates_d1 stdout ----\n");
    out.push_str("thread 'store::tests::migrates_d1' panicked at src/store.rs:212:9:\n");
    out.push_str(
        "assertion `left == right` failed: the facts table exists\n  left: 0\n right: 1\n",
    );
    out.push_str(&format!(
        "\nfailures:\n    store::tests::migrates_d1\n\ntest result: FAILED. {passed} passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.41s\n\nerror: test failed, to rerun pass `-p demo --lib`\n"
    ));
    out
}

/// A file's contents, as a `Read` returns them.
pub fn file_body(lines: usize) -> String {
    (1..=lines)
        .map(|n| format!("{n:>6}\u{2192}    let value_{n} = compute(input, {n}).await?;"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Real feature work: reads, edits, a failing test, a fix, a green run, a
/// commit.
pub fn feature_work() -> String {
    record(
        "feature-1",
        vec![
            turn(
                "user",
                "Move the facts table to D1 and make the migration collected so production creates it. Keep Postgres working.",
                vec![],
            ),
            turn(
                "assistant",
                "I'll look at how migrations are collected first.",
                vec![
                    call(
                        "Read",
                        Some("crates/venture/src/lib.rs"),
                        "{\"file_path\":\"crates/venture/src/lib.rs\"}",
                        &file_body(180),
                        None,
                    ),
                    call(
                        "Grep",
                        None,
                        "{\"pattern\":\"MIGRATION_\"}",
                        &(0..60)
                            .map(|i| format!("crates/x/src/lib.rs:{i}: pub const MIGRATION_{i}"))
                            .collect::<Vec<_>>()
                            .join("\n"),
                        None,
                    ),
                    call(
                        "Read",
                        Some("crates/pages/migrations/sqlite/0006_facts.sql"),
                        "{}",
                        &file_body(40),
                        None,
                    ),
                ],
            ),
            turn(
                "assistant",
                "The facts migration is not in `migrations/`. I'll add it as 0022 and register it in the lock file.",
                vec![
                    call(
                        "Write",
                        Some("crates/venture/migrations/0022_pages_0006_facts.sql"),
                        "CREATE TABLE IF NOT EXISTS facts (...)",
                        "File created successfully",
                        None,
                    ),
                    call(
                        "Edit",
                        Some("crates/venture/migrations/.harness-lock.json"),
                        "{\"old_string\":\"...\",\"new_string\":\"...\"}",
                        "The file has been updated.",
                        None,
                    ),
                    call(
                        "Bash",
                        None,
                        "cargo test -p livingbrain-venture",
                        &cargo_test_failure(47),
                        Some(101),
                    ),
                ],
            ),
            turn(
                "assistant",
                "The test expects the statements to match exactly; the collected copy had a reworded comment. Decision: compare statements, not comments.",
                vec![
                    call(
                        "Edit",
                        Some("crates/venture/src/lib.rs"),
                        "{\"old_string\":\"collected == migration.sql\",\"new_string\":\"statements(&collected) == statements(migration.sql)\"}",
                        "The file has been updated.",
                        None,
                    ),
                    call(
                        "Bash",
                        None,
                        "cargo build -p livingbrain-venture",
                        &cargo_build(
                            220,
                            "    Finished `dev` profile [unoptimized + debuginfo] target(s) in 41.20s\n",
                        ),
                        Some(0),
                    ),
                    call(
                        "Bash",
                        None,
                        "cargo test -p livingbrain-venture",
                        &format!(
                            "running 48 tests\n{}test result: ok. 48 passed; 0 failed; 0 ignored\n",
                            (0..48)
                                .map(|i| format!("test store::tests::case_{i} ... ok\n"))
                                .collect::<String>()
                        ),
                        Some(0),
                    ),
                    call(
                        "Bash",
                        None,
                        "git commit -am 'fix(venture): collect the facts migration for D1'",
                        "[fix/facts 773d48f] fix(venture): collect the facts migration for D1\n 3 files changed, 41 insertions(+), 2 deletions(-)",
                        Some(0),
                    ),
                ],
            ),
            turn("user", "Great, thanks.", vec![]),
            turn(
                "assistant",
                "Done: migration 0022 collected, the lock pins it, and the drift check compares statements so a comment change no longer fails it. Learned: D1 only runs what is in `migrations/`, so every module migration must be collected.",
                vec![],
            ),
        ],
    )
}

/// A failing retry loop: the same command, failing the same way, and
/// nothing changed.
pub fn failing_loop() -> String {
    let mut turns = vec![turn("user", "Run the e2e suite and get it green.", vec![])];
    for attempt in 0..7 {
        turns.push(turn(
            "assistant",
            if attempt == 0 { "Running the suite." } else { "Retrying." },
            vec![call(
                "Bash",
                None,
                "npx playwright test --project=chromium",
                &format!(
                    "Running 64 tests using 4 workers\n{}\n  1) [chromium] › login.spec.ts:12:5 › signs in\n    Error: page.goto: net::ERR_CONNECTION_REFUSED at http://localhost:5173/\n\n  1 failed\n  63 did not run\n",
                    (0..30).map(|i| format!("  ✓  {i} [chromium] › smoke.spec.ts:{i}:1 › loads ({i}ms)")).collect::<Vec<_>>().join("\n")
                ),
                Some(1),
            )],
        ));
    }
    turns.push(turn("assistant", "The dev server is not running.", vec![]));
    record("loop-1", turns)
}

/// A lookup that changed nothing.
pub fn no_changes() -> String {
    record(
        "lookup-1",
        vec![
            turn("user", "Which port does the API listen on locally?", vec![]),
            turn(
                "assistant",
                "Checking.",
                vec![call(
                    "Grep",
                    None,
                    "{\"pattern\":\"listen\"}",
                    "src/main.rs:14:    let addr = \"127.0.0.1:8787\";",
                    None,
                )],
            ),
            turn(
                "assistant",
                "It listens on 127.0.0.1:8787 (src/main.rs:14).",
                vec![],
            ),
        ],
    )
}

/// A greeting.
pub fn trivial() -> String {
    record(
        "hi-1",
        vec![
            turn("user", "hi", vec![]),
            turn("assistant", "Hi! What are we working on?", vec![]),
        ],
    )
}

/// Schema 3: the main agent only delegates; a subagent reads, edits and
/// tests, nested in the `Agent` call that launched it.
pub fn delegated() -> String {
    let mut agent = call(
        "Agent",
        None,
        "{\"subagent_type\":\"general-purpose\",\"prompt\":\"Fix the flaky retry test\"}",
        "The retry test is fixed: the clock is injected now.",
        None,
    );
    agent["subagent"] = json!({
        "id": "agent-1", "name": "general-purpose",
        "description": "Fix the flaky retry test", "status": "completed",
        "turns": [
            turn("user", "Fix the flaky retry test", vec![]),
            turn("assistant", "Reading the test.", vec![
                call("Read", Some("tests/retry.rs"), "{}", &file_body(300), None),
                call("Bash", None, "cargo test retry", &cargo_test_failure(20), Some(101)),
            ]),
            turn("assistant", "Injecting the clock.", vec![
                call("Edit", Some("tests/retry.rs"), "{\"old_string\":\"now()\"}", "The file has been updated.", None),
                call("Bash", None, "cargo test retry", "test result: ok. 21 passed; 0 failed", Some(0)),
            ]),
            turn("assistant", "Fixed: the test used the wall clock; it now takes an injected clock, and passes 21/21.", vec![]),
        ],
        "truncated": false, "omitted_turns": 0
    });
    let mut body: Value = serde_json::from_str(&record(
        "delegated-1",
        vec![
            turn("user", "The retry test is flaky, can you fix it?", vec![]),
            turn("assistant", "Delegating.", vec![agent]),
            turn("assistant", "Done — see the subagent's fix.", vec![]),
        ],
    ))
    .expect("json");
    body["schema_version"] = json!(3);
    body.to_string()
}

/// A long working session: the feature-work pattern six times over on
/// different files — 60 tool calls, a 4 × 32 000-character transcript
/// before compaction.
pub fn long_feature_work() -> String {
    let one: Value = serde_json::from_str(&feature_work()).expect("json");
    let mut turns = Vec::new();
    for round in 0..6 {
        for turn in one["turns"].as_array().expect("turns") {
            let text = serde_json::to_string(turn)
                .expect("json")
                .replace("crates/venture", &format!("crates/module-{round}"));
            turns.push(serde_json::from_str::<Value>(&text).expect("json"));
        }
    }
    record("long-1", turns)
}
