//! Guards on the test harness itself: every run of the binary goes through the
//! isolated `common::run`, and that run keeps the opt-in file in its sandbox.
//! A test that bypassed the sandbox would write the developer's real
//! `~/.config/livingbrain/logs-allow.json`.

mod common;

use std::path::{Path, PathBuf};

use common::{run, temp_dir};

fn tests_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests")
}

/// Only `common/mod.rs` may name the binary to spawn it, and that helper must
/// point the config directory at the sandbox.
#[test]
fn every_binary_run_goes_through_the_sandboxed_harness() {
    let mut offenders = Vec::new();
    for entry in std::fs::read_dir(tests_dir()).expect("tests dir") {
        let path = entry.expect("entry").path();
        if path.extension().is_none_or(|ext| ext != "rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("read test file");
        // This file names the marker itself; it spawns nothing.
        if path.file_name() != Some("isolation.rs".as_ref())
            && text.contains("CARGO_BIN_EXE_livingbrain")
        {
            offenders.push(path.display().to_string());
        }
    }
    assert!(
        offenders.is_empty(),
        "these files spawn the binary outside common::run: {offenders:?}"
    );

    let harness =
        std::fs::read_to_string(tests_dir().join("common").join("mod.rs")).expect("harness source");
    assert!(
        harness.contains("\"XDG_CONFIG_HOME\""),
        "the harness must point XDG_CONFIG_HOME at the sandbox"
    );
}

/// A run writes its opt-in file under the sandbox home, never anywhere else.
#[test]
fn an_allow_run_writes_its_opt_in_file_into_the_sandbox() {
    let home = temp_dir("isolation-home");
    let cwd = temp_dir("isolation-cwd");
    let repo = temp_dir("isolation-repo");
    let output = run(
        &cwd,
        &home,
        &[
            "logs",
            "allow",
            repo.to_str().expect("utf-8 path"),
            "--workspace",
            "default",
            "--json",
        ],
        &[],
        None,
    );
    assert!(output.status.success(), "stderr: {}", output.stderr);
    assert!(
        home.join("livingbrain").join("logs-allow.json").is_file(),
        "the opt-in file must land under the sandbox home"
    );
}
