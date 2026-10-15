//! The two contracts this crate makes with the rest of the tree: its record
//! kinds carry the ledger's own wire names, and its dependencies stay
//! wasm-safe (the Worker is the only place they run).

use cratefield_testing::assert_wasm_safe_deps;
use livingbrain_github::record::{GithubKind, RecordError, RepoRef};
use livingbrain_pages::SourceKind;

#[test]
fn kind_wire_names_are_the_ledgers() {
    for (ours, theirs) in [
        (GithubKind::Commit, SourceKind::GithubCommit),
        (GithubKind::Issue, SourceKind::GithubIssue),
        (GithubKind::PullRequest, SourceKind::GithubPr),
    ] {
        assert_eq!(ours.wire_name(), theirs.as_str());
        assert_eq!(SourceKind::parse(ours.wire_name()), Some(theirs));
    }
}

#[test]
fn project_rule_matches_the_ledgers() {
    // A repository name is filed as a project, so it must pass the ledger's
    // own project rule, and the names the ledger refuses must be refused here.
    for name in ["EarthOS.world", "a", "x_y-z.1"] {
        assert!(livingbrain_pages::is_project(name));
        assert!(RepoRef::parse(&format!("o/{name}")).is_ok(), "{name}");
    }
    let too_long = "a".repeat(65);
    assert!(!livingbrain_pages::is_project(&too_long));
    assert_eq!(
        RepoRef::parse(&format!("o/{too_long}")),
        Err(RecordError::InvalidRepo(format!("o/{too_long}")))
    );
    assert!(!livingbrain_pages::is_project(".hidden"));
    assert!(RepoRef::parse("o/.hidden").is_err());
}

#[test]
fn deps_are_wasm_safe() {
    assert_wasm_safe_deps(env!("CARGO_PKG_NAME"));
}
