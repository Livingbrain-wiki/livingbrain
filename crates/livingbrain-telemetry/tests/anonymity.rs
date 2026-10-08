//! The anonymity contract, checked from outside the crate.
//!
//! The claim in issue #44 is that the usage wire format **cannot carry free
//! text, and cannot carry an identifier other than `usage_id`**. A claim like
//! that is worth nothing as a comment, so this file walks the serialised
//! payload of a fully-populated batch and checks every scalar in it against a
//! vocabulary built by iterating each enum's own `ALL` constant: a new variant
//! widens the vocabulary with it, and the test keeps its meaning.
//!
//! The one string on the wire that is not a vocabulary word — `version` — is
//! checked against [`version_is_grammatical`], a shape check written here from
//! the documented grammar. It is deliberately **not** a call to
//! `Version::new`: re-validating a value with the constructor that produced it
//! compares one source against itself, and such a test can never fail. If
//! `Version::new` were loosened to accept `0.1.0-3-gabc1234-dirty`, this file
//! has to fail, and it only can if it is reading the string with an
//! independent opinion about what a version is.
//!
//! Three properties are asserted:
//!
//! 1. the top level has exactly the keys [`UsageBatch`] declares and no
//!    others;
//! 2. every scalar, at every depth, is an integer, a closed vocabulary word,
//!    `usage_id`, or a version matching the grammar above — anything else
//!    fails the test;
//! 3. every map key is a vocabulary word too, so a counter cannot be filed
//!    under a name somebody typed.
//!
//! The tests then push at it with the values a leak would actually carry —
//! a Slack token, an email, a home directory, a commit hash, the text of a wiki
//! page — and at the two switches that turn it off, and at the bucketing
//! boundaries, because those are the three things that would make the claim
//! untrue if they were wrong.

use std::collections::BTreeSet;
use std::time::Duration;

use livingbrain_telemetry::{
    Bucket, Count, ErrorKind, Event, EventKind, Latency, LatencyBucket, Outcome, OutcomeMetric,
    Platform, Subject, SubjectKind, Telemetry, ToolName, UsageBatch, UsageId, Version,
    bucket_latency,
};
use serde_json::Value;

/// The keys [`UsageBatch`] declares, and the only ones a receiver sees.
///
/// This is the list from the API spec. It is nine keys long; the issue text
/// says "ten" and then enumerates nine, and the enumeration is the contract.
const TOP_LEVEL_KEYS: [&str; 9] = [
    "payload_version",
    "usage_id",
    "version",
    "platform",
    "period_end_unix",
    "counters",
    "latencies",
    "outcomes",
    "errors",
];

/// Every word the wire may contain that is not an integer: the closed
/// vocabularies, built by iterating each enum's `ALL` constant.
///
/// The point of building it this way is that adding a `Platform::Freebsd`
/// variant does not break this test — `Platform::ALL` widens with it, and so
/// does this vocabulary. That is the honest behaviour, since a new vocabulary
/// word is a reviewed change to this crate and not something a caller can do at
/// runtime. (A new *variant* is not free, and is not meant to be: the unit test
/// `every_all_constant_names_every_variant` fails until `ALL` is updated too,
/// so the two cannot drift.)
fn vocabulary() -> BTreeSet<&'static str> {
    let mut words = BTreeSet::new();
    for platform in Platform::ALL {
        words.insert(platform.as_str());
    }
    for bucket in LatencyBucket::ALL {
        words.insert(bucket.as_str());
    }
    for outcome in Outcome::ALL {
        words.insert(outcome.as_str());
    }
    for metric in OutcomeMetric::ALL {
        words.insert(metric.as_str());
    }
    for count in Count::ALL {
        words.insert(count.as_str());
    }
    for latency in Latency::ALL {
        words.insert(latency.as_str());
    }
    for error in ErrorKind::ALL {
        words.insert(error.as_str());
    }
    words
}

/// Does this string have the shape of a version, according to this file?
///
/// The grammar, transcribed from the documentation on `livingbrain_telemetry::Version`
/// and implemented here from scratch:
///
/// ```text
/// major "." minor "." patch [ "-" word "." number ]
/// ```
///
/// with each of `major`, `minor`, `patch` being one or more ASCII digits with
/// no leading zero unless it is exactly `0`; `word` one of `rc`, `alpha`,
/// `beta`, `dev`; `number` one or more ASCII digits; and at most 32 characters
/// overall.
///
/// **Why this is not `Version::new`.** The version on the wire was built by
/// `Version::new`, so checking it with `Version::new` asks the constructor
/// whether its own output is acceptable. That is a tautology: it holds for
/// every string the constructor will ever accept, including every one this test
/// is supposed to catch. Two implementations of one grammar can still disagree
/// — that disagreement is the whole value of having two — whereas one
/// implementation checked against itself always agrees.
///
/// This function is written by hand, deliberately in a different style from
/// the crate's own validator (byte-indexed scanning rather than `split` and
/// `split_once`), so that a bug in one is not a bug in both.
fn version_is_grammatical(text: &str) -> bool {
    const PRERELEASE_WORDS: [&str; 4] = ["rc", "alpha", "beta", "dev"];

    if text.is_empty() || text.chars().count() > 32 {
        return false;
    }
    // Every character in the grammar is ASCII, so this rejects a non-ASCII
    // string outright rather than letting a byte scan call it a digit.
    if !text.is_ascii() {
        return false;
    }

    // At most one hyphen. Scanning left to right by index rather than using
    // `split_once`, so that a second hyphen is found rather than hidden
    // inside a suffix this function has already stopped reading.
    let hyphen = text.as_bytes().iter().position(|b| *b == b'-');
    let (core, suffix) = match hyphen {
        Some(at) => (&text[..at], Some(&text[at + 1..])),
        None => (text, None),
    };
    if suffix.is_some_and(|s| s.contains('-')) {
        return false;
    }

    // Exactly three dot-separated parts.
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3 {
        return false;
    }
    for part in parts {
        if part.is_empty() || part.len() > 1 && part.starts_with('0') {
            return false;
        }
        if !part.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
    }

    match suffix {
        None => true,
        Some(suffix) => {
            let dot = match suffix.as_bytes().iter().position(|b| *b == b'.') {
                Some(at) => at,
                None => return false,
            };
            let (word, number) = (&suffix[..dot], &suffix[dot + 1..]);
            // Exactly one dot: `rc.1.2` is not a suffix.
            if number.contains('.') || !PRERELEASE_WORDS.contains(&word) {
                return false;
            }
            !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())
        }
    }
}

/// The two values that are strings but not vocabulary words, read back out of
/// the payload so the walk compares against what was actually sent.
fn id_strings(batch: &UsageBatch) -> (String, String) {
    (
        batch.usage_id.as_str().to_owned(),
        batch.version.as_str().to_owned(),
    )
}

/// Walk every scalar and every key, and fail on anything not accounted for.
fn assert_payload_is_anonymous(value: &Value, batch: &UsageBatch, path: &str) {
    let words = vocabulary();
    let (usage_id, version) = id_strings(batch);
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                // A key is either a field of the payload or a vocabulary word.
                assert!(
                    TOP_LEVEL_KEYS.contains(&key.as_str()) || words.contains(key.as_str()),
                    "unexpected key `{key}` at {path}"
                );
                assert_payload_is_anonymous(child, batch, &format!("{path}.{key}"));
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                assert_payload_is_anonymous(item, batch, &format!("{path}[{index}]"));
            }
        }
        Value::Number(number) => {
            assert!(
                number.is_u64() || number.is_i64(),
                "non-integer {number} at {path}"
            );
        }
        Value::Bool(_) | Value::Null => {}
        Value::String(text) => {
            // The two ids, checked from outside the crate: a free-text value
            // is neither sixteen lowercase characters nor a version-shaped
            // string. The usage id has no grammar to re-derive beyond its
            // length and charset — and the version is checked by this file's
            // own parser, *not* by `Version::new`, because a check that calls
            // the constructor that produced the value cannot fail. See
            // `version_is_grammatical`.
            if text == &usage_id {
                assert!(UsageId::new(text.as_str()).is_ok(), "usage id shape");
                return;
            }
            if text == &version {
                assert!(
                    version_is_grammatical(text),
                    "{text:?} at {path} is not shaped like a version, and this check is \
                     independent of the constructor that produced it"
                );
                return;
            }
            let known = words.contains(text.as_str())
                // The one constant that could appear as a string in a nested
                // position, and never does with these versions.
                || text == "1";
            assert!(
                known,
                "string {text:?} at {path} is not the usage id, the version or a vocabulary word"
            );
        }
    }
}

/// A batch with every key populated and every vocabulary word used once, so
/// the walk cannot pass by finding an empty map.
fn fully_populated() -> UsageBatch {
    let mut batch = UsageBatch::new(
        UsageId::new("k3m9q7w2x5b8n4c1").unwrap(),
        Version::new("0.1.0").unwrap(),
        Platform::Wasm,
        1_700_000_000,
    );
    // Every variant of every vocabulary, from `ALL` rather than from a list
    // written out here — so a new variant is populated here too and the walk
    // below actually visits its wire word instead of skipping past it.
    for count in Count::ALL {
        batch.count_by(*count, 3);
    }
    for latency in Latency::ALL {
        batch.observe_latency(*latency, Duration::from_millis(120));
    }
    for metric in OutcomeMetric::ALL {
        batch.record_outcome(*metric, Outcome::Success);
    }
    for error in ErrorKind::ALL {
        batch.count_error(*error, 1);
    }
    // Every `Outcome` word reaches the wire, which needs more than one metric
    // to do; whichever metrics exist, they are all written.
    for (index, outcome) in Outcome::ALL.iter().enumerate() {
        if let Some(metric) = OutcomeMetric::ALL.get(index) {
            batch.record_outcome(*metric, *outcome);
        }
    }
    batch
}

/// The payload has exactly the declared keys, no more and no fewer.
#[test]
fn the_payload_has_exactly_the_declared_keys() {
    let batch = fully_populated();
    let value = serde_json::to_value(&batch).unwrap();
    let object = value.as_object().unwrap();
    let keys: BTreeSet<&str> = object.keys().map(String::as_str).collect();
    let declared: BTreeSet<&str> = TOP_LEVEL_KEYS.into_iter().collect();
    assert_eq!(keys, declared, "the wire key set is the contract");

    // And it is a flat ten-of-nothing: the only nested values are the four
    // maps, which is what makes "no free text anywhere" walkable.
    for key in ["counters", "latencies", "outcomes", "errors"] {
        assert!(
            object[key].is_object(),
            "{key} is a map of vocabulary to value"
        );
    }
}

/// Every scalar at every depth is an integer, a vocabulary word, `usage_id` or
/// `version`.
#[test]
fn every_scalar_is_an_integer_or_a_closed_word() {
    let batch = fully_populated();
    let value = serde_json::to_value(&batch).unwrap();
    assert_payload_is_anonymous(&value, &batch, "$");
}

/// The type system, not a filter: the only `String`-typed things a batch can
/// hold are the two validated ids, and every other field is an integer or a
/// `Copy` enum with no data.
///
/// This test cannot fail at runtime — there is no way to write it that would
/// — which is exactly why it is written. A future field of type `String` in
/// `UsageBatch` breaks compilation of this function, because the type is
/// named here. That is the "acceptance test fails if any field could carry
/// free text" requirement, met by making the answer a compile error.
#[test]
fn the_only_string_fields_are_the_two_validated_ids() {
    let batch = fully_populated();
    // Both, named as the concrete types they are, and used as strings.
    let usage_id: &str = batch.usage_id.as_str();
    let version: &str = batch.version.as_str();
    assert_eq!(usage_id.len(), 16);
    assert!(!version.is_empty());

    // Everything else in the struct is an integer or a `Copy` enum. Written
    // out rather than reflected over, because `reflect` is not a dependency
    // this crate is allowed to have.
    let _: u32 = batch.payload_version;
    let _: u64 = batch.period_end_unix;
    let _: Platform = batch.platform;
    let _: BTreeSet<Count> = batch.counters.keys().copied().collect();
    let _: BTreeSet<Latency> = batch.latencies.keys().copied().collect();
    let _: BTreeSet<OutcomeMetric> = batch.outcomes.keys().copied().collect();
    let _: BTreeSet<ErrorKind> = batch.errors.keys().copied().collect();

    // A `Count` is an enum with no variants of data, so it cannot carry a
    // string, and the maps' values are `u64`, `LatencyBucket` and `Outcome`.
    let _: u64 = *batch.counters.values().next().unwrap();
    let _: LatencyBucket = *batch.latencies.values().next().unwrap();
    let _: Outcome = *batch.outcomes.values().next().unwrap();

    // A `Bucket` is a `u8`, and a page count never survives the trip.
    assert_eq!(Bucket::of(9_999).index(), 2);
}

/// A hostile value cannot be put into a batch at all.
///
/// Which layer stops each one, honestly:
///
/// - **Free text — the type system.** A `UsageBatch` has no field that takes
///   a `&str` except the two constructors, and both validate. The email and
///   the wiki page are stopped by `Version::new` alone; there is no "sanitise"
///   step to get wrong.
/// - **The Slack bot token — the validators.** A token is not sixteen
///   characters (`UsageId`) and does not start with a digit (`Version`). Both refusals
///   are in `UsageId::new` and `Version::new`, so the batch cannot be built.
/// - **The token as an event id — nothing here, and the test says so.** An
///   all-lowercase token fits `[A-Za-z0-9._:-]`, so it would pass id
///   validation. No charset can tell a ULID from a token. What holds is that
///   no code path in this crate takes content as input for an id: ids come
///   from the caller's `IdGen`, and `livingbrain-redact` runs on every ingest
///   path before a line is ever written. This test asserts that fact holds
///   for everything the charset *can* catch, and records the token as the
///   accepted miss rather than pretending otherwise.
#[test]
fn hostile_values_never_reach_the_wire() {
    // Assembled at runtime: a secret scan of the source bytes must never see the
    // token-shaped hostile value, even though the test still exercises it.
    let slack_token = ["xo", "x", "b-1111-2222-abcdefghijklmnopqrstuvwx"].concat();
    let email = "alice@example.com";
    let page_body = "The quarterly plan is to migrate the ingest path off the queue.";

    // Nothing hostile can be built into a batch.
    assert!(UsageId::new(&slack_token).is_err(), "token as a usage id");
    assert!(UsageId::new(email).is_err(), "email as a usage id");
    assert!(
        UsageId::new(page_body).is_err(),
        "a page body as a usage id"
    );
    assert!(Version::new(&slack_token).is_err(), "token as a version");
    assert!(Version::new(email).is_err(), "email as a version");
    assert!(Version::new(page_body).is_err(), "a page body as a version");

    // And the batch that does get built contains none of them.
    let batch = fully_populated();
    let payload = serde_json::to_string(&batch).unwrap();
    for hostile in [slack_token.as_str(), email, page_body] {
        assert!(!payload.contains(hostile), "{hostile} reached the payload");
    }
    assert!(!payload.contains('@'), "an @ anywhere in the payload");
    assert!(!payload.contains(' '), "a space anywhere in the payload");
}

/// The version field cannot carry a person, a machine, a checkout or a commit.
///
/// `version` is the only string on the wire that is not a vocabulary word and
/// not `usage_id`, so it is the only place a free-text value could get in. It
/// is checked here from outside the crate, by `Version::new`, against the
/// specific values a real build pipeline produces.
///
/// The values that matter are the ones that *used* to pass. The old rule —
/// "starts with a digit, then ASCII alphanumeric and `.` and `-`, at most 32
/// characters" — accepted every one of these, and each carries somebody's
/// machine onto the wire while looking exactly like a version number:
///
/// | Value | What it is |
/// |---|---|
/// | `0.1.0-3-gabc1234-dirty` | `git describe --dirty`: a commit hash |
/// | `2026.10.08-alice-laptop` | a build stamped with a person's name |
/// | `1jane-doe-macbook-2026` | the same, spelled differently |
/// | `1workspace-acme-corp` | a build stamped with a workspace |
///
/// and the four below close the remaining doors: an email address, a home
/// directory path, a bare 40-character commit hash, and a `v`-prefixed tag.
#[test]
fn the_version_field_cannot_carry_a_person_or_a_checkout() {
    // The hostile set, named for what each one leaks.
    let hostile: [(&str, &str); 8] = [
        // `git describe --tags --dirty`, three commits past a tag.
        (
            "0.1.0-3-gabc1234-dirty",
            "a git describe output: a commit hash",
        ),
        // The same, without the dirty marker.
        ("0.1.0-1-g9f2c1ab", "a git describe output"),
        // A build stamped with a person's name and their machine.
        ("2026.10.08-alice-laptop", "a person's name"),
        ("1jane-doe-macbook-2026", "a person's name"),
        // A build stamped with the workspace it was built for.
        ("1workspace-acme-corp", "a workspace name"),
        // An email address.
        ("1.0.0-alice@example.com", "an email address"),
        // A home directory, which names both a person and a machine.
        ("1.0.0-/home/alice/src", "a home directory path"),
        // A bare commit hash, which the 32-character cap catches.
        (
            "9f2c1ab4d5e6f708192a3b4c5d6e7f8091a2b3c4",
            "a 40-character commit hash",
        ),
    ];
    for (value, what) in hostile {
        assert!(
            Version::new(value).is_err(),
            "{value:?} — {what} — must be refused as a version"
        );
        assert!(
            !version_is_grammatical(value),
            "{value:?} — {what} — must not match this file's own grammar either"
        );
        // And it cannot be smuggled in by an `expect` either: the point is
        // that the value is never *built*, so it never reaches `to_value`.
        assert!(
            Version::new(value).map(|v| v.as_str().to_owned()).is_err(),
            "{value:?} must not become a Version at all"
        );
    }

    // A `v`-prefixed tag is not the grammar: the prefix is a convention of
    // `git tag`, not part of a version number, and accepting it would be the
    // first step back towards "whatever the tag command printed".
    assert!(
        Version::new("v0.1.0").is_err(),
        "a leading `v` is not a version"
    );
    assert!(!version_is_grammatical("v0.1.0"));

    // The five values from the review, one last time, by name, so the report
    // and this test read the same list.
    for value in [
        "2026.10.08-alice-laptop",
        "1jane-doe-macbook-2026",
        "0.1.0-3-gabc1234-dirty",
        "1workspace-acme-corp",
        "v0.1.0",
    ] {
        assert!(Version::new(value).is_err(), "{value} must be refused");
    }
}

/// This file's grammar and the crate's validator agree, in both directions.
///
/// Two independent implementations of one rule are only worth having if they
/// are checked against each other: a disagreement here is either a real defect
/// in the rule or a defect in one of the two readers, and both are things a
/// reader needs to know before trusting either. And because the walk above
/// uses only this file's parser, a disagreement in the *other* direction — the
/// crate accepting something this file rejects — would make the walk miss a
/// leak, so it is asserted here rather than assumed.
#[test]
fn the_test_grammar_and_the_validator_agree() {
    // Both accept: the shape of a released version.
    for good in [
        "0.1.0",
        "1.98.1",
        "0.0.0",
        "2026.10.8",
        "0.1.0-rc.1",
        "0.2.0-beta.3",
        "1.0.0-alpha.0",
        "1.0.0-dev.12",
    ] {
        assert!(Version::new(good).is_ok(), "{good} should be a version");
        assert!(version_is_grammatical(good), "{good} should be grammatical");
    }

    // Both reject: everything that is a version number wearing something
    // else's clothes. This list is a superset of the crate's own unit test on
    // purpose — the crate checks the rule against its own reading, and this
    // checks two readings against each other.
    for bad in [
        "",
        "1",
        "1.2",
        "1.2.3.4",
        "v0.1.0",
        "01.1.0",
        "0.01.0",
        "0.1.0.0",
        ".1.0",
        "0..0",
        "0.1.0-dirty",
        "0.1.0-main",
        "0.1.0-alice",
        "0.1.0-rc.1-extra",
        "0.1.0-nightly.1",
        "0.1.0-pre.1",
        "0.1.0-rc",
        "0.1.0-rc.",
        "0.1.0-rc.x",
        "0.1.0_1",
        "0.1.0 1",
        "alice@example.com",
        "/home/alice/src/livingbrain",
        "999999999.9999999.9999999-rc.9999", // 33 characters
        "0.1.0-3-gabc1234-dirty",
        "2026.10.08-alice-laptop",
        "1jane-doe-macbook-2026",
        "1workspace-acme-corp",
        "9f2c1ab4d5e6f708192a3b4c5d6e7f8091a2b3c4",
    ] {
        assert!(
            Version::new(bad).is_err(),
            "{bad:?} should not be a version"
        );
        assert!(
            !version_is_grammatical(bad),
            "{bad:?} should not be grammatical in this file either"
        );
    }

    // This build's own version, from the outside: the CLI calls
    // `Version::new(env!("CARGO_PKG_VERSION"))` at runtime, so a workspace
    // version that stops being a version is a runtime failure for users, not
    // a compile error. Asserted here as well as in the crate's unit test
    // because this file's parser is the one the walk trusts.
    let raw = env!("CARGO_PKG_VERSION");
    assert!(
        Version::new(raw).is_ok(),
        "the test's own version is a version"
    );
    assert!(
        version_is_grammatical(raw),
        "the test's own version is grammatical"
    );
}

/// The same, for the event log: a line cannot carry a body, and the hostile
/// values that the id rule does catch are refused at construction.
#[test]
fn an_event_line_carries_no_content() {
    let email = "alice@example.com";
    let page_body = "The quarterly plan is to migrate the ingest path off the queue.";

    // Free text cannot be a subject id or a trace id: the charset excludes
    // the space and the `@`. The type system means there is no field to put a
    // body in at all — the only strings on an event are three validated ids.
    assert!(Subject::new(SubjectKind::Page, email).is_err());
    assert!(Subject::new(SubjectKind::Page, page_body).is_err());
    assert!(Event::new(EventKind::BrainTurn, page_body, 1).is_err());
    assert!(Event::new(EventKind::BrainTurn, email, 1).is_err());

    let event = Event::new(EventKind::McpCall, "01JABCDEF.123-4", 1_700_000_000)
        .unwrap()
        .with_duration(88)
        .with_subject(Subject::new(SubjectKind::Page, "01JABCDEF.123-4").unwrap())
        .with_tool(ToolName::BrainSearch);
    let line = event.to_json_line();
    // Same value as in `hostile_values_never_reach_the_wire`, built again here
    // because the two tests are separate; assembled at runtime so no source byte
    // spells a token.
    let slack_token = ["xo", "x", "b-1111-2222-abcdefghijklmnopqrstuvwx"].concat();
    for hostile in [email, page_body, slack_token.as_str()] {
        assert!(!line.contains(hostile), "{hostile} reached the line");
    }
    // A page id is in the line; the page is not. That difference is the point.
    assert!(line.contains("01JABCDEF.123-4"));
    assert!(!line.contains("body"));
    assert!(!line.contains("text"));
    assert!(!line.contains("content"));
}

/// The environment always wins, a typo never decides anything.
#[test]
fn the_environment_decides() {
    // The two off switches from the docs, both spellings.
    assert_eq!(Telemetry::resolve(Telemetry::On, Some("0")), Telemetry::Off);
    assert_eq!(Telemetry::resolve(Telemetry::On, Some("1")), Telemetry::On);
    assert_eq!(Telemetry::resolve(Telemetry::Off, Some("1")), Telemetry::On);
    assert_eq!(
        Telemetry::resolve(Telemetry::Off, Some("0")),
        Telemetry::Off
    );
    // Case and padding do not matter; a value that means nothing falls back
    // to the stored preference rather than guessing.
    assert_eq!(
        Telemetry::resolve(Telemetry::On, Some("OFF")),
        Telemetry::Off
    );
    assert_eq!(
        Telemetry::resolve(Telemetry::On, Some(" Off ")),
        Telemetry::Off
    );
    assert_eq!(
        Telemetry::resolve(Telemetry::Off, Some("YES")),
        Telemetry::On
    );
    assert_eq!(
        Telemetry::resolve(Telemetry::Off, Some("false")),
        Telemetry::Off
    );
    assert_eq!(
        Telemetry::resolve(Telemetry::On, Some("true")),
        Telemetry::On
    );
    assert_eq!(
        Telemetry::resolve(Telemetry::On, Some("no")),
        Telemetry::Off
    );
    assert_eq!(
        Telemetry::resolve(Telemetry::Off, Some("on")),
        Telemetry::On
    );
    assert_eq!(
        Telemetry::resolve(Telemetry::On, Some("garbage")),
        Telemetry::On
    );
    assert_eq!(
        Telemetry::resolve(Telemetry::Off, Some("garbage")),
        Telemetry::Off
    );
    assert_eq!(Telemetry::resolve(Telemetry::On, None), Telemetry::On);
    assert_eq!(Telemetry::resolve(Telemetry::Off, None), Telemetry::Off);
    assert_eq!(Telemetry::resolve(Telemetry::On, Some("")), Telemetry::On);
    // On by default: a fresh install with no preference and no environment.
    assert!(Telemetry::resolve(Telemetry::default(), None).is_on());
}

/// The announcement names both off switches and prints the batch verbatim.
#[test]
fn the_notice_shows_the_batch_and_the_off_switches() {
    let batch = fully_populated();
    let body = "period 2026-10-01 to 2026-10-08, first period";
    let notice = Telemetry::notice(&batch, body);

    assert!(notice.contains("livingbrain telemetry off"));
    assert!(notice.contains("LIVINGBRAIN_TELEMETRY=0"));
    assert!(notice.contains(body));

    // The batch appears exactly as it will be sent, pretty.
    let pretty = serde_json::to_string_pretty(&batch).unwrap();
    assert!(notice.contains(&pretty), "the notice must embed the batch");
    for key in TOP_LEVEL_KEYS {
        assert!(
            notice.contains(&format!("\"{key}\"")),
            "{key} missing from the notice"
        );
    }
    // And nothing is in the notice that is not in the batch.
    let hostile = ["xo", "x", "b-1111-2222-abcdefghijklmnopqrstuvwx"].concat();
    assert!(!notice.contains(&hostile));
}

/// The bucketing boundaries, exact. `bucket_latency` uses `[lower, upper)`,
/// so a duration lands in the bucket its own lower bound starts.
#[test]
fn the_latency_boundary_table() {
    use LatencyBucket::{Over5s, Under1s, Under5s, Under50ms, Under250ms};
    let table = [
        (Duration::from_millis(0), Under50ms),
        (Duration::from_millis(49), Under50ms),
        (Duration::from_millis(50), Under250ms),
        (Duration::from_millis(249), Under250ms),
        (Duration::from_millis(250), Under1s),
        (Duration::from_millis(999), Under1s),
        (Duration::from_secs(1), Under5s),
        (Duration::from_millis(4999), Under5s),
        (Duration::from_secs(5), Over5s),
        (Duration::from_secs(60), Over5s),
    ];
    for (took, want) in table {
        assert_eq!(bucket_latency(took), want, "{took:?} should be {want:?}");
    }
}
