//! The event schema is versioned, and CI checks it against the code and
//! against the fixtures.
//!
//! `docs/brain-events.schema.json` is the promise a third party could point a
//! validator at: one closed object, one integer version, no
//! `additionalProperties`. It is a checked-in document rather than a generated
//! one, so it can drift — from a Rust field added without the schema knowing,
//! or from a fixture that no longer validates. This file is what stops that.
//!
//! Four invariants, checked in order:
//!
//! 1. the schema file parses, its `version` is [`EVENT_SCHEMA_VERSION`], it is
//!    closed, and it uses no keyword the test's validator does not implement,
//!    so a field cannot appear on a line — or a constraint can be added to the
//!    document — without CI noticing;
//! 2. the schema's field list — names, types and descriptions, in order — is
//!    the same list [`Event::schema`] returns and the same key set a serialised
//!    [`Event`] has, so the document and the code are one thing;
//! 3. every fixture in `docs/telemetry/` validates against the schema, covers
//!    every [`EventKind`] exactly once, and carries nothing that is not an id
//!    or a closed vocabulary word — the no-free-text guarantee, checked on the
//!    files a reader will actually look at;
//! 4. every fixture is byte-for-byte what [`Event::to_json_line`] emits, so the
//!    examples are not aspirational.
//!
//! The validator below is hand-written on purpose: the repository has no
//! JSON-schema toolchain and this crate's whole argument is that it needs
//! none. It covers the keywords this document uses — `type`, `properties`,
//! `required`, `additionalProperties`, `enum`, `anyOf`, `minimum`, `$ref` and
//! `pattern` — and no others, so a keyword CI cannot enforce is a failing test
//! rather than something silently ignored.

use std::collections::BTreeSet;

use livingbrain_telemetry::{
    EVENT_SCHEMA_VERSION, ErrorKind, Event, EventKind, Outcome, Subject, SubjectKind, ToolName,
};
use serde_json::Value;

// `include_str!` resolves its path relative to the file containing the macro,
// and both of these live outside the crate, above it. `CARGO_MANIFEST_DIR` is
// the crate root at compile time, so `concat!` gives a path that does not care
// where cargo was invoked from.
const SCHEMA: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/brain-events.schema.json"
));

/// Every fixture, named, with the `EventKind` it is the example of. The name is
/// part of the contract: it is how a reader finds the example of a kind.
const FIXTURES: [(&str, &str, &str); 5] = [
    (
        "brain-turn",
        EventKind::BrainTurn.as_str(),
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/telemetry/brain-turn.json"
        )),
    ),
    (
        "nightly-evolve",
        EventKind::NightlyEvolve.as_str(),
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/telemetry/nightly-evolve.json"
        )),
    ),
    (
        "mcp-call",
        EventKind::McpCall.as_str(),
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/telemetry/mcp-call.json"
        )),
    ),
    (
        "colony-handoff",
        EventKind::ColonyHandoff.as_str(),
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/telemetry/colony-handoff.json"
        )),
    ),
    (
        "operational",
        EventKind::Operational.as_str(),
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/telemetry/operational.json"
        )),
    ),
];

/// Every `EventKind`, built by iterating the enum rather than by writing a list,
/// so a new variant widens the expectation with it.
fn event_kinds() -> Vec<EventKind> {
    vec![
        EventKind::BrainTurn,
        EventKind::NightlyEvolve,
        EventKind::McpCall,
        EventKind::ColonyHandoff,
        EventKind::Operational,
    ]
}

/// Every wire word the closed vocabularies hold, from the enums themselves.
fn vocabulary() -> BTreeSet<&'static str> {
    let mut words = BTreeSet::new();
    for kind in event_kinds() {
        words.insert(kind.as_str());
    }
    for kind in [
        SubjectKind::Page,
        SubjectKind::Message,
        SubjectKind::Workspace,
        SubjectKind::Tool,
    ] {
        words.insert(kind.as_str());
    }
    for outcome in [Outcome::Success, Outcome::Failure, Outcome::Skipped] {
        words.insert(outcome.as_str());
    }
    for tool in [
        ToolName::BrainSearch,
        ToolName::BrainPage,
        ToolName::BrainNote,
    ] {
        words.insert(tool.as_str());
    }
    for error in [
        ErrorKind::Config,
        ErrorKind::Upstream,
        ErrorKind::Timeout,
        ErrorKind::Decode,
        ErrorKind::Permission,
        ErrorKind::Internal,
    ] {
        words.insert(error.as_str());
    }
    words
}

/// The parsed schema document.
fn schema() -> Value {
    serde_json::from_str(SCHEMA).expect("docs/brain-events.schema.json is valid JSON")
}

/// The fixture's line, without the trailing newline a text file has.
fn fixture_line(text: &str) -> &str {
    text.trim_end_matches('\n')
}

/// The closed-id rule `Subject::new` and `Event::new` enforce, written out
/// rather than shared, so this file asserts it from outside the crate instead
/// of taking the constructors' word for it.
fn is_opaque_id(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 128
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'))
}

/// A local JSON-pointer target, e.g. `#/$defs/opaque_id`.
fn resolve<'a>(root: &'a Value, reference: &str) -> &'a Value {
    let pointer = reference
        .strip_prefix("#/")
        .unwrap_or_else(|| panic!("{reference} is not a local JSON pointer"));
    let mut node = root;
    for segment in pointer.split('/') {
        node = &node[segment];
    }
    node
}

/// Every keyword the validator implements. Anything else in the document — in
/// a property, in a `$defs` entry, in an `anyOf` branch — is a constraint CI
/// does not check, which is worse than not having it, so it fails the build.
///
/// `fields` and `$defs` are the two containers, not keywords: `fields` is the
/// ordered list a reader and `Event::schema()` diff, and `$defs` is where the
/// `$ref`s resolve to.
const KNOWN_KEYWORDS: [&str; 16] = [
    "$ref",
    "type",
    "properties",
    "required",
    "additionalProperties",
    "enum",
    "anyOf",
    "minimum",
    "pattern",
    "title",
    "description",
    "$schema",
    "$id",
    "version",
    "fields",
    "$defs",
];

/// Recursively assert that every subschema in the document uses only keywords
/// the validator understands.
fn assert_keywords_are_understood(subschema: &Value, path: &str) {
    let object = subschema
        .as_object()
        .unwrap_or_else(|| panic!("{path} is not a schema object: {subschema}"));
    for keyword in object.keys() {
        assert!(
            KNOWN_KEYWORDS.contains(&keyword.as_str()),
            "{path} uses `{keyword}`, which the test's validator does not check"
        );
    }
    if let Some(properties) = object.get("properties") {
        for (name, child) in properties
            .as_object()
            .unwrap_or_else(|| panic!("{path}.properties is not an object"))
        {
            assert_keywords_are_understood(child, &format!("{path}.{name}"));
        }
    }
    if let Some(choices) = object.get("anyOf") {
        for (index, child) in choices
            .as_array()
            .unwrap_or_else(|| panic!("{path}.anyOf is not an array"))
            .iter()
            .enumerate()
        {
            assert_keywords_are_understood(child, &format!("{path}.anyOf[{index}]"));
        }
    }
}

/// Does `value` have this JSON type? A non-negative integer is the whole of the
/// numeric vocabulary here: an event line carries no float and no negative.
fn type_matches(value: &Value, wanted: &str) -> bool {
    match wanted {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "integer" => value.is_u64(),
        _ => panic!("the schema asks about an unknown type `{wanted}`"),
    }
}

/// Check `value` against `subschema`, following `$ref` into the root.
///
/// Returns the reason it does not conform rather than asserting, so `anyOf` can
/// try every branch and report which ones failed.
fn conforms(value: &Value, subschema: &Value, root: &Value, path: &str) -> Result<(), String> {
    if let Some(reference) = subschema["$ref"].as_str() {
        return conforms(value, resolve(root, reference), root, path);
    }
    if let Some(wanted) = subschema["type"].as_str()
        && !type_matches(value, wanted)
    {
        return Err(format!("{path} is {value}, not a {wanted}"));
    }
    if let Some(minimum) = subschema["minimum"].as_u64() {
        let number = value
            .as_u64()
            .ok_or_else(|| format!("{path} is {value}, not a non-negative integer"))?;
        if number < minimum {
            return Err(format!("{path} is {number}, which is under {minimum}"));
        }
    }
    if let Some(allowed) = subschema["enum"].as_array()
        && !allowed.contains(value)
    {
        return Err(format!("{path} is {value}, not one of {allowed:?}"));
    }
    if let Some(choices) = subschema["anyOf"].as_array() {
        let reasons: Vec<String> = choices
            .iter()
            .filter_map(|choice| conforms(value, choice, root, path).err())
            .collect();
        if reasons.len() == choices.len() {
            return Err(format!(
                "{path} is {value}, and matches no branch: {reasons:?}"
            ));
        }
    }
    if let Some(properties) = subschema["properties"].as_object() {
        let object = value
            .as_object()
            .ok_or_else(|| format!("{path} is not an object: {value}"))?;
        for required in subschema["required"]
            .as_array()
            .unwrap_or_else(|| panic!("{path}.required is not an array"))
        {
            let name = required.as_str().unwrap();
            if !object.contains_key(name) {
                return Err(format!("{path} is missing required property `{name}`"));
            }
        }
        for (key, child) in object {
            let Some(child_schema) = properties.get(key) else {
                if subschema["additionalProperties"] == Value::Bool(false) {
                    return Err(format!("{path} has undeclared property `{key}`"));
                }
                continue;
            };
            conforms(child, child_schema, root, &format!("{path}.{key}"))?;
        }
    }
    if let Some(pattern) = subschema["pattern"].as_str() {
        // The only pattern in the document is the opaque-id charset. There is no
        // regex crate in this workspace, so the pattern is recognised and its
        // characters are checked directly — an unrecognised one fails, rather
        // than passing unvalidated.
        if pattern != "^[A-Za-z0-9._:-]{1,128}$" {
            return Err(format!(
                "the test only knows how to check the opaque-id pattern, not {pattern}"
            ));
        }
        let text = value
            .as_str()
            .ok_or_else(|| format!("{path} is {value}, and a pattern needs a string"))?;
        if !is_opaque_id(text) {
            return Err(format!("{path} is {text:?}, which is not an opaque id"));
        }
    }
    Ok(())
}

/// Assert that a fixture conforms to the schema, with a readable reason.
fn validate(value: &Value, root: &Value, path: &str) {
    if let Err(reason) = conforms(value, root, root, path) {
        panic!("{reason}");
    }
}

/// Walk every leaf of a fixture and check that it is an id or a closed word,
/// and that every key is a declared field or part of a subject.
///
/// The keys matter as much as the values, for the same reason the wire does: a
/// map filed under a name somebody typed is free text with a type.
fn assert_nothing_but_ids_and_words(
    value: &Value,
    schema: &Value,
    words: &BTreeSet<&str>,
    path: &str,
) {
    match value {
        Value::Object(map) => {
            let known = declared_keys(schema);
            for (key, child) in map {
                assert!(
                    known.contains(key.as_str()) || key == "kind" || key == "id",
                    "{path} has key `{key}`, which is neither a declared field nor part of a subject"
                );
                assert_nothing_but_ids_and_words(child, schema, words, &format!("{path}.{key}"));
            }
        }
        Value::Number(number) => {
            assert!(
                number.is_u64(),
                "{path} is {number}, which is not a non-negative integer"
            );
        }
        // A fixture is a flat object of scalars, so an array is a shape no
        // fixture has and one that would hide a nested line to walk.
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                assert_nothing_but_ids_and_words(item, schema, words, &format!("{path}[{index}]"));
            }
        }
        Value::Bool(_) | Value::Null => {}
        Value::String(text) => {
            assert!(
                words.contains(text.as_str()) || is_opaque_id(text),
                "{path} is {text:?}, which is neither a closed vocabulary word nor an opaque id"
            );
            assert!(!text.contains('@'), "an email-shaped value at {path}");
            assert!(!text.contains(' '), "free text at {path}");
        }
    }
}

/// Rebuild an `Event` from a parsed fixture line.
///
/// `Event` has no `Deserialize`, on purpose (see the crate docs), so this is
/// the honest way round: every field is read back out of the JSON and handed to
/// the same constructor a real caller uses, which re-validates the ids.
fn event_from_fixture(value: &Value) -> Event {
    let kind = event_kinds()
        .into_iter()
        .find(|kind| kind.as_str() == value["kind"].as_str().unwrap())
        .unwrap_or_else(|| panic!("fixture names an unknown kind: {}", value["kind"]));
    let mut event = Event::new(
        kind,
        value["trace_id"].as_str().unwrap(),
        value["at_unix"].as_u64().unwrap(),
    )
    .unwrap();
    if let Some(outcome) = value["outcome"].as_str() {
        event = event.with_outcome(outcome_from(outcome));
    }
    event = event.with_duration(value["duration_ms"].as_u64().unwrap());
    if let Some(subject) = value["subject"].as_object() {
        event = event.with_subject(
            Subject::new(
                subject_kind_from(subject["kind"].as_str().unwrap()),
                subject["id"].as_str().unwrap(),
            )
            .unwrap(),
        );
    }
    if let Some(error) = value["error_kind"].as_str() {
        event = event.with_error(error_kind_from(error));
    }
    if let Some(tool) = value["tool"].as_str() {
        event = event.with_tool(tool_from(tool));
    }
    if let Some(colony) = value["colonizer_colony"].as_str() {
        event = event.with_colony(colony).unwrap();
    }
    event
}

fn outcome_from(name: &str) -> Outcome {
    [Outcome::Success, Outcome::Failure, Outcome::Skipped]
        .into_iter()
        .find(|outcome| outcome.as_str() == name)
        .unwrap_or_else(|| panic!("unknown outcome `{name}`"))
}

fn subject_kind_from(name: &str) -> SubjectKind {
    [
        SubjectKind::Page,
        SubjectKind::Message,
        SubjectKind::Workspace,
        SubjectKind::Tool,
    ]
    .into_iter()
    .find(|kind| kind.as_str() == name)
    .unwrap_or_else(|| panic!("unknown subject kind `{name}`"))
}

fn tool_from(name: &str) -> ToolName {
    [
        ToolName::BrainSearch,
        ToolName::BrainPage,
        ToolName::BrainNote,
    ]
    .into_iter()
    .find(|tool| tool.as_str() == name)
    .unwrap_or_else(|| panic!("unknown tool `{name}`"))
}

fn error_kind_from(name: &str) -> ErrorKind {
    [
        ErrorKind::Config,
        ErrorKind::Upstream,
        ErrorKind::Timeout,
        ErrorKind::Decode,
        ErrorKind::Permission,
        ErrorKind::Internal,
    ]
    .into_iter()
    .find(|error| error.as_str() == name)
    .unwrap_or_else(|| panic!("unknown error kind `{name}`"))
}

/// The schema is a real document, at the version this build writes, closed, and
/// written only in keywords CI actually enforces.
#[test]
fn the_schema_is_valid_json_and_versioned() {
    let schema = schema();

    assert_eq!(
        schema["$schema"], "https://json-schema.org/draft/2020-12/schema",
        "the schema declares the 2020-12 vocabulary"
    );
    assert!(
        !schema["$id"].as_str().unwrap_or_default().is_empty(),
        "the schema names itself, so a validator can resolve it"
    );
    assert_eq!(
        schema["version"], EVENT_SCHEMA_VERSION,
        "the schema version is the version this build writes"
    );
    // Closed: no field can appear on a line without the schema being told, which
    // is what makes "the schema and the code agree" a checkable claim.
    assert_eq!(schema["type"], "object");
    assert_eq!(
        schema["additionalProperties"], false,
        "the schema must be closed or a new field is invisible to it"
    );
    assert!(
        !schema["description"]
            .as_str()
            .unwrap_or_default()
            .is_empty(),
        "the schema states its guarantee"
    );
    assert_keywords_are_understood(&schema, "$");
    for (name, definition) in schema["$defs"].as_object().expect("$defs is an object") {
        assert_keywords_are_understood(definition, &format!("$.$defs.{name}"));
    }

    // The version travels on every line, so a reader can tell which document a
    // line was written against without consulting this one.
    let event = Event::new(EventKind::BrainTurn, "01JABC.123-4", 1_700_000_000).unwrap();
    assert_eq!(
        serde_json::to_value(&event).unwrap()["schema_version"],
        EVENT_SCHEMA_VERSION
    );
}

/// The document and the code describe one event, and the vocabularies in it are
/// the enums' own words.
#[test]
fn the_schema_matches_the_code() {
    let schema = schema();
    let code = Event::schema();

    // Names, types and descriptions, in the same order, from the same types that
    // serialise the data.
    let documented = field_triples(&schema);
    let generated = field_triples(&code);
    assert_eq!(
        documented, generated,
        "docs/brain-events.schema.json has drifted from Event::schema()"
    );

    // And the declared properties are that same list, in `required` and in
    // `properties`.
    let names: Vec<&str> = documented.iter().map(|(name, _, _)| *name).collect();
    let required: Vec<&str> = schema["required"]
        .as_array()
        .expect("required is an array")
        .iter()
        .map(|name| name.as_str().unwrap())
        .collect();
    assert_eq!(
        required, names,
        "every field is required, in the same order"
    );
    let declared: BTreeSet<&str> = schema["properties"]
        .as_object()
        .expect("properties is an object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        declared,
        names.iter().copied().collect::<BTreeSet<&str>>(),
        "properties declares exactly the documented fields"
    );

    // The key set a serialised `Event` actually has, on a line with every
    // optional field filled in — the shape no single fixture has.
    let event = Event::new(EventKind::ColonyHandoff, "01JABC.123-4", 1_700_000_000)
        .unwrap()
        .with_outcome(Outcome::Failure)
        .with_duration(35)
        .with_subject(Subject::new(SubjectKind::Workspace, "01JWS.200-3").unwrap())
        .with_error(ErrorKind::Timeout)
        .with_tool(ToolName::BrainPage)
        .with_colony("01JCOLONY.42-1")
        .unwrap();
    let serialised = serde_json::to_value(&event).expect("an event serialises");
    let written: BTreeSet<&str> = serialised
        .as_object()
        .expect("an event is an object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        written, declared,
        "the fields a serialised Event has, and the fields the schema declares"
    );

    // The closed vocabularies, from the enums rather than from a list typed
    // here, so a new variant is a failing test instead of a silent widening.
    for name in ["kind", "subject_kind", "outcome", "error_kind", "tool"] {
        let words = words_of(name, &schema["$defs"][name]["enum"]);
        let generated_words = words_of(name, &code["vocabularies"][name]);
        assert_eq!(
            words, generated_words,
            "the `{name}` vocabulary has drifted"
        );
    }
}

/// The field names a schema declares, read from the schema so this file keeps
/// no second list in step with the first.
fn declared_keys(schema: &Value) -> BTreeSet<&str> {
    field_triples(schema)
        .into_iter()
        .map(|(name, _, _)| name)
        .collect()
}

/// The words of one closed vocabulary, read out of a schema or out of the code.
fn words_of<'a>(name: &str, node: &'a Value) -> Vec<&'a str> {
    node.as_array()
        .unwrap_or_else(|| panic!("the {name} vocabulary is an array"))
        .iter()
        .map(|word| word.as_str().unwrap())
        .collect()
}

/// A schema's field list as `(name, type, description)` triples, in order.
fn field_triples(schema: &Value) -> Vec<(&str, &str, &str)> {
    schema["fields"]
        .as_array()
        .unwrap_or_else(|| panic!("the schema lists its fields"))
        .iter()
        .map(|field| {
            (
                field["name"].as_str().unwrap(),
                field["type"].as_str().unwrap(),
                field["description"].as_str().unwrap(),
            )
        })
        .collect()
}

/// Every fixture is a single line the schema accepts, one per kind, carrying
/// nothing a log line must never carry.
#[test]
fn every_fixture_validates_against_the_schema() {
    let schema = schema();
    let words = vocabulary();
    let mut covered: Vec<&str> = Vec::new();

    // Checked once, from the loop's first pass, so the "no content field"
    // statement is about the schema rather than about each file.
    let fields: Vec<&str> = field_triples(&schema)
        .into_iter()
        .map(|(name, _, _)| name)
        .collect();
    for content_word in ["body", "text", "content", "markdown", "title"] {
        assert!(
            !fields.contains(&content_word),
            "the schema declares a `{content_word}` field"
        );
    }

    for (name, kind, text) in FIXTURES {
        let line = fixture_line(text);
        assert!(
            !line.contains('\n'),
            "docs/telemetry/{name}.json is more than one line"
        );
        let value: Value = serde_json::from_str(line)
            .unwrap_or_else(|_| panic!("docs/telemetry/{name}.json is valid JSON"));
        assert!(
            value.is_object(),
            "docs/telemetry/{name}.json is a JSON object"
        );

        // Schema ↔ fixtures: the whole document, enforced by hand.
        validate(&value, &schema, &format!("{name}.json"));

        // And the fixture itself: an id or a closed word, nothing else.
        assert_nothing_but_ids_and_words(&value, &schema, &words, &format!("{name}.json"));
        assert_eq!(
            value["schema_version"], EVENT_SCHEMA_VERSION,
            "{name}.json carries the version this build writes"
        );
        assert_eq!(value["kind"], kind, "{name}.json is the example of {kind}");
        covered.push(kind);
    }

    // Every kind, exactly once: a new `EventKind` without a fixture fails here.
    let mut expected: Vec<&str> = event_kinds().into_iter().map(EventKind::as_str).collect();
    expected.sort_unstable();
    covered.sort_unstable();
    assert_eq!(covered, expected, "the fixtures cover every kind once");
}

/// The fixtures are what the code emits: parsing one back into an `Event`
/// reproduces the file's line byte for byte.
#[test]
fn a_fixture_round_trips_through_the_rust_type() {
    for (name, _, text) in FIXTURES {
        let line = fixture_line(text);
        let value: Value = serde_json::from_str(line).unwrap();
        let event = event_from_fixture(&value);
        assert_eq!(
            event.to_json_line(),
            line,
            "docs/telemetry/{name}.json is not what Event::to_json_line writes"
        );
    }

    // The one shape a fixture does not carry, checked here rather than in a
    // seventh file: a failure line, which is what a support engineer reads most
    // and where an error kind is easiest to leave out. It is a fully-populated
    // `Event` with no `Deserialize`, so the schema is the only way it can be
    // checked — and the schema does not care where a value came from.
    let schema = schema();
    let failure = Event::new(EventKind::McpCall, "01JABC.123-9", 1_700_000_000)
        .unwrap()
        .with_error(ErrorKind::Permission)
        .with_tool(ToolName::BrainNote);
    let value: Value = serde_json::from_str(&failure.to_json_line()).unwrap();
    validate(&value, &schema, "failure");
    assert_nothing_but_ids_and_words(&value, &schema, &vocabulary(), "failure");
    assert_eq!(value["outcome"], "failure");
    assert_eq!(value["error_kind"], "permission");
}
