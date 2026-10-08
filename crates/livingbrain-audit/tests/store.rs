//! Behaviour tests for the settings audit store, against a real SQL
//! database (the kit's in-memory SQLite adapter, with the module's
//! migrations applied).
//!
//! The tests are about the four things that could silently be wrong: a
//! secret reaching the table, a clear being recorded as a `NULL`, one
//! workspace seeing another's history, and a plain setting's value not
//! surviving the redaction pass intact.

mod support;

use livingbrain_audit::{Actor, AuditEntry, AuditStore, Section, SettingChange};
use support::{rendered, store};

/// A member who changes things.
fn human() -> Actor {
    Actor::Human {
        id: "usr_ada".to_owned(),
    }
}

/// Records one change in `T0WORKSPACE` and returns its id.
fn record(store: &AuditStore, key: &str, change: SettingChange) -> String {
    pollster::block_on(store.record(
        "T0WORKSPACE",
        Section::Models,
        &human(),
        key,
        change,
        Some("req_1"),
    ))
    .expect("a valid change is recorded")
    .id
}

/// The one change a listing returned, or a panic naming how many it got.
fn only(listed: &[AuditEntry]) -> &AuditEntry {
    assert_eq!(
        listed.len(),
        1,
        "expected exactly one entry, got {listed:?}"
    );
    &listed[0]
}

/// A settings value carrying a secret shape, assembled at run time so that no
/// credential-shaped literal is written into this file: a word-shaped
/// provider prefix, then EXAMPLE repeated. The pieces only make a token when
/// they are concatenated, and a test fixture is not the place for a value
/// that reads as real. The prefix is also what a detector keys on, which is
/// why this shape and not some invented one.
const EXAMPLE_AUTH_HEADER: &str = concat!(
    "Authorization: Bearer ",
    "sk",
    "-EXAMPLE-",
    "EXAMPLEEXAMPLE",
);

#[test]
fn a_secret_is_redacted_before_it_reaches_the_table() {
    let (kit, store) = store();
    // What detects this is the key prefix in the token, not the header: the
    // detector table has no `Bearer` pattern, and the bare-key spec
    // (`Class::OpenAiKey`) is what fires. The fixture keeps this shape on
    // purpose so the redaction path is genuinely exercised. It arrives as a
    // settings value — a member pasted it into a BYOK field — so it must
    // leave as a token.
    record(
        &store,
        "model.default",
        SettingChange::set(EXAMPLE_AUTH_HEADER).unwrap(),
    );

    let listed = pollster::block_on(store.list_for_scope("T0WORKSPACE", 10)).unwrap();
    match &only(&listed).change {
        SettingChange::Set { value, redacted } => {
            assert!(*redacted, "a detector matched, so the row says so");
            assert!(
                value.contains("[REDACTED:"),
                "replaced, not dropped: {value}"
            );
        }
        other => panic!("a set value records as a set, not {other:?}"),
    }

    // And it is not anywhere else in the row either — this is the assertion
    // that would fail if redaction ever moved later than the write.
    let raw = pollster::block_on(rendered(&kit));
    assert!(
        !raw.contains(EXAMPLE_AUTH_HEADER),
        "the raw token must not survive anywhere in the table:\n{raw}"
    );
}

#[test]
fn a_cleared_setting_is_an_explicit_row_never_a_null() {
    let (kit, store) = store();
    record(&store, "model.default", SettingChange::cleared());

    let listed = pollster::block_on(store.list_for_scope("T0WORKSPACE", 10)).unwrap();
    let entry = only(&listed);
    assert_eq!(entry.change, SettingChange::Cleared);
    assert_eq!(entry.key, "model.default");
    assert!(
        !entry.change.was_redacted(),
        "nothing was withheld — there was nothing to withhold"
    );

    // The column is NOT NULL by schema, so assert the stored form too: the
    // row exists, its `change` says `cleared`, and its `value` is the empty
    // string rather than a SQL NULL. "This key was removed on Tuesday" is a
    // sentence the audit log has to be able to say.
    let raw = pollster::block_on(rendered(&kit)).to_lowercase();
    assert!(raw.contains("cleared"), "the row names the clear:\n{raw}");
    assert!(
        !raw.contains("null"),
        "a clear is stored as an explicit row, not a NULL:\n{raw}"
    );
}

#[test]
fn a_secret_key_stores_no_value_at_all() {
    let (kit, store) = store();
    // The caller never hands over the value: `SettingChange::secret` takes no
    // argument to redact, so there is nothing for this crate to leak.
    record(&store, "anthropic.api_key", SettingChange::secret());

    let listed = pollster::block_on(store.list_for_scope("T0WORKSPACE", 10)).unwrap();
    let entry = only(&listed);
    assert_eq!(entry.change, SettingChange::Secret);
    assert_eq!(entry.change.stored_value(), "");
    assert!(entry.change.was_redacted());

    let raw = pollster::block_on(rendered(&kit));
    assert!(
        raw.contains("secret"),
        "the row says the value was withheld:\n{raw}"
    );
}

#[test]
fn listing_is_scoped_to_one_workspace() {
    let (_kit, store) = store();
    record(&store, "daily", SettingChange::set("on").unwrap());
    pollster::block_on(store.record(
        "T0OTHER",
        Section::Proactivity,
        &human(),
        "weekly",
        SettingChange::set("off").unwrap(),
        None,
    ))
    .unwrap();

    let mine = pollster::block_on(store.list_for_scope("T0WORKSPACE", 50)).unwrap();
    assert_eq!(mine.len(), 1);
    assert!(
        mine.iter().all(|entry| entry.workspace_id == "T0WORKSPACE"),
        "a listing never crosses workspaces: {mine:?}"
    );
    assert_eq!(mine[0].key, "daily");

    // The other workspace sees its own entry and not ours.
    let theirs = pollster::block_on(store.list_for_scope("T0OTHER", 50)).unwrap();
    assert_eq!(theirs.len(), 1);
    assert_eq!(theirs[0].key, "weekly");
    // Sequence numbers are per workspace, so both logs start at 1 and
    // neither can tell where the other's is up to.
    assert_eq!(mine[0].seq, 1);
    assert_eq!(theirs[0].seq, 1);

    assert_eq!(
        pollster::block_on(store.count_for_scope("T0WORKSPACE")).unwrap(),
        1
    );
    assert_eq!(
        pollster::block_on(store.count_for_scope("T0OTHER")).unwrap(),
        1
    );
}

#[test]
fn listing_is_newest_first() {
    let (_kit, store) = store();
    for key in ["first", "second", "third", "fourth"] {
        record(&store, key, SettingChange::set("on").unwrap());
    }

    let listed = pollster::block_on(store.list_for_scope("T0WORKSPACE", 50)).unwrap();
    let got: Vec<&str> = listed.iter().map(|entry| entry.key.as_str()).collect();
    assert_eq!(got, vec!["fourth", "third", "second", "first"]);

    // The limit is honoured, and it takes the newest.
    let capped = pollster::block_on(store.list_for_scope("T0WORKSPACE", 2)).unwrap();
    assert_eq!(capped.len(), 2);
    assert_eq!(capped[0].key, "fourth");
    assert!(
        pollster::block_on(store.list_for_scope("T0WORKSPACE", 0))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_non_sensitive_value_round_trips() {
    let (_kit, store) = store();
    record(&store, "daily", SettingChange::set("at 09:00").unwrap());

    let listed = pollster::block_on(store.list_for_scope("T0WORKSPACE", 10)).unwrap();
    let entry = only(&listed);
    match &entry.change {
        SettingChange::Set { value, redacted } => {
            assert!(
                !*redacted,
                "nothing matched, so the row does not claim it did"
            );
            assert_eq!(value, "at 09:00", "a plain setting is stored as written");
        }
        other => panic!("a plain value records as a set, not {other:?}"),
    }

    // The rest of the row survives too: the section, the actor and the
    // request id are what make it an audit entry rather than a diff.
    assert_eq!(entry.seq, 1, "the first change in a workspace is change 1");
    assert_eq!(entry.section, Section::Models);
    assert_eq!(entry.actor, human());
    assert_eq!(entry.request_id.as_deref(), Some("req_1"));
    assert!(!entry.recorded_at.is_empty(), "every row is stamped");
    assert!(!entry.id.is_empty(), "every row has a ULID");
}

#[test]
fn a_brain_actor_is_recorded_as_the_brain() {
    let (_kit, store) = store();
    pollster::block_on(store.record(
        "T0WORKSPACE",
        Section::Automations,
        &Actor::Brain,
        "daily.digest",
        SettingChange::set("on").unwrap(),
        None,
    ))
    .unwrap();

    let listed = pollster::block_on(store.list_for_scope("T0WORKSPACE", 10)).unwrap();
    let entry = only(&listed);
    assert_eq!(entry.actor, Actor::Brain);
    assert_eq!(entry.request_id, None);
    assert_eq!(entry.section, Section::Automations);
}

#[test]
fn a_workspace_with_no_changes_lists_empty() {
    let (_kit, store) = store();
    assert!(
        pollster::block_on(store.list_for_scope("T0QUIET", 10))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        pollster::block_on(store.count_for_scope("T0QUIET")).unwrap(),
        0
    );
}

#[test]
fn a_bad_workspace_or_key_is_refused_before_anything_is_written() {
    let (_kit, store) = store();
    let refused = pollster::block_on(store.record(
        "has space",
        Section::Tools,
        &human(),
        "daily",
        SettingChange::set("on").unwrap(),
        None,
    ));
    assert!(refused.is_err(), "a workspace id cannot contain whitespace");

    let refused = pollster::block_on(store.record(
        "T0WORKSPACE",
        Section::Tools,
        &human(),
        "Not A Key",
        SettingChange::set("on").unwrap(),
        None,
    ));
    assert!(refused.is_err(), "a setting key is lowercase ascii");

    assert_eq!(
        pollster::block_on(store.count_for_scope("T0WORKSPACE")).unwrap(),
        0,
        "a refused change writes nothing"
    );
}
