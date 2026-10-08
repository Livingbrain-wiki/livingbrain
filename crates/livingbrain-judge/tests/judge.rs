//! The judge against a scripted classifier.
//!
//! The fake records every ask and answers with the probabilities a test
//! scripts, and its profile is settable — so one test can run the *same*
//! probabilities through both calibration families and see two different
//! decisions. That is the point of the crate: a threshold is only ever
//! meaningful against the family it was tuned for, and these tests are where
//! that claim is either true or a comment.

use std::collections::BTreeMap;
use std::sync::Arc;

use cratefield_core::{Answer, Calibration, DEFAULT_MAX_STATE_CHARS, validate_questions};
use cratefield_testing::{ClassifierMode, FakeClassifier, assert_wasm_safe_deps};
use livingbrain_judge::{
    CHATTER, CONFLICT_QUESTION, Judge, JudgeError, JudgeSettings, MEMORY_QUESTION, Proactivity,
    TRIAGE_QUESTION, Thresholds,
};

/// Distinctive, so "the record carries no text" is a checkable claim.
const MARKER: &str = "zebra-unmistakable-customer-marker";

fn noul(true_p: f32) -> Answer {
    Answer::noul(
        true_p >= 0.5,
        BTreeMap::from([
            ("true".to_owned(), true_p),
            ("false".to_owned(), 1.0 - true_p),
        ]),
    )
}

/// A judge over a fake whose profile is `family`, plus the fake itself so a
/// test can script the probabilities and read the asks back.
/// Asserts a record carries the decision's reasoning and none of the text.
fn record_has_no_text(judgement: &livingbrain_judge::Judgement) {
    let json = serde_json::to_string(judgement).expect("serialises");
    assert!(
        !json.contains(MARKER),
        "the record leaked its input: {json}"
    );
}

fn scripting(family: Calibration, settings: JudgeSettings) -> (Judge, FakeClassifier) {
    let classifier = FakeClassifier::new(ClassifierMode::Deterministic);
    classifier.set_profile(family, DEFAULT_MAX_STATE_CHARS);
    (
        Judge::new(Arc::new(classifier.clone()), settings),
        classifier,
    )
}

#[test]
fn judge_deps_are_wasm_safe() {
    assert_wasm_safe_deps(env!("CARGO_PKG_NAME"));
}

#[test]
fn the_question_sets_and_settings_are_whole() {
    for questions in [
        livingbrain_judge::triage_questions(),
        livingbrain_judge::memory_questions(),
        livingbrain_judge::contradiction_questions(),
    ] {
        assert!(validate_questions(&questions).is_ok(), "{questions:?}");
    }
    // A future settings row stores these as JSON, so they have to survive it.
    let json = serde_json::to_string(&JudgeSettings::default()).expect("serialises");
    let back: JudgeSettings = serde_json::from_str(&json).expect("deserialises");
    assert_eq!(back, JudgeSettings::default());
    assert!(json.contains("\"normal\""), "{json}");
}

#[test]
fn a_partial_settings_row_fills_from_the_defaults() {
    // A row naming only the classifier family, in full.
    let row = r#"{"classifier":{"triage":0.8,"memory":0.5,"contradiction":0.8}}"#;
    let settings: JudgeSettings = serde_json::from_str(row).expect("loads");
    assert_eq!(settings.classifier.triage, 0.8);
    // The absent family is filled with the language model's own numbers,
    // not the classifier's.
    assert_eq!(settings.language_model, Thresholds::LANGUAGE_MODEL);
    assert_eq!(settings.proactivity, Proactivity::Normal);

    // A family object that is present but short is refused rather than
    // completed from the other family's numbers.
    assert!(serde_json::from_str::<JudgeSettings>(r#"{"language_model":{"triage":0.9}}"#).is_err());
    // An unknown proactivity is refused on purpose, not guessed at.
    assert!(serde_json::from_str::<JudgeSettings>(r#"{"proactivity":"loud"}"#).is_err());
}

#[test]
fn proactivity_moves_the_triage_threshold_and_nothing_else() {
    let settings = JudgeSettings::default();
    let normal = settings.thresholds_for(Calibration::Classifier);
    let eager = JudgeSettings {
        proactivity: Proactivity::Eager,
        ..settings
    }
    .thresholds_for(Calibration::Classifier);

    assert!(eager.triage < normal.triage);
    assert_eq!(eager.memory, normal.memory);
    assert_eq!(eager.contradiction, normal.contradiction);

    // The same fixed probability flips with the setting.
    let (j, fake) = scripting(Calibration::Classifier, settings);
    fake.set_answer_for(TRIAGE_QUESTION, noul(0.75));
    let quiet = JudgeSettings {
        proactivity: Proactivity::Quiet,
        ..settings
    };
    let quiet_judge = Judge::new(Arc::new(fake), quiet);
    assert!(
        pollster::block_on(j.triage("thread"))
            .expect("answered")
            .outcome
    );
    assert!(
        !pollster::block_on(quiet_judge.triage("thread"))
            .expect("answered")
            .outcome
    );

    // `Off` is the far end of the same table: unreachable rather than a
    // branch, so even a certain yes is declined.
    let (off, fake) = scripting(
        Calibration::Classifier,
        JudgeSettings {
            proactivity: Proactivity::Off,
            ..settings
        },
    );
    fake.set_answer_for(TRIAGE_QUESTION, noul(1.0));
    let declined = pollster::block_on(off.triage(MARKER)).expect("answered");
    assert!(!declined.outcome);
    assert!(declined.judgement.threshold > 1.0);
    assert_eq!(declined.judgement.score, 1.0);
}

#[test]
fn the_record_carries_the_reason_and_never_the_text() {
    let (j, fake) = scripting(Calibration::Classifier, JudgeSettings::default());
    fake.set_answer_for(TRIAGE_QUESTION, noul(0.9));
    let decided = pollster::block_on(j.triage(MARKER)).expect("answered");
    let json = serde_json::to_string(&decided.judgement).expect("serialises");

    assert!(
        !json.contains(MARKER),
        "the record leaked the thread: {json}"
    );
    assert!(json.contains(TRIAGE_QUESTION), "{json}");
    assert!(
        json.contains("\"true\""),
        "no probabilities in the record: {json}"
    );
    assert!(json.contains("0.7"), "no threshold in the record: {json}");
    assert!(
        decided
            .judgement
            .probabilities
            .contains_key(TRIAGE_QUESTION)
    );
    assert_eq!(
        fake.last().expect("one ask").question_ids,
        vec![TRIAGE_QUESTION]
    );

    // The other two records are built the same way and leak the same nothing.
    fake.set_answer_for(
        MEMORY_QUESTION,
        Answer::choice(CHATTER, BTreeMap::from([(CHATTER.to_owned(), 0.9)])),
    );
    let gate = pollster::block_on(j.memory_gate(MARKER)).expect("answered");
    record_has_no_text(&gate.judgement);

    fake.set_answer_for(CONFLICT_QUESTION, noul(0.9));
    let conflict =
        pollster::block_on(j.contradiction(MARKER, "the launch is in March")).expect("answered");
    record_has_no_text(&conflict.judgement);
}

#[test]
fn the_memory_gate_reads_the_non_chatter_mass() {
    let (j, fake) = scripting(Calibration::Classifier, JudgeSettings::default());
    let chatter = BTreeMap::from([
        ("decision".to_owned(), 0.05),
        ("owner".to_owned(), 0.05),
        ("durable_fact".to_owned(), 0.05),
        (CHATTER.to_owned(), 0.85),
    ]);
    fake.set_answer_for(MEMORY_QUESTION, Answer::choice(CHATTER, chatter));
    let loud = pollster::block_on(j.memory_gate(MARKER)).expect("answered");
    assert!(!loud.outcome);
    assert_eq!(loud.label.as_deref(), Some(CHATTER));

    let decision = BTreeMap::from([
        ("decision".to_owned(), 0.80),
        ("owner".to_owned(), 0.05),
        ("durable_fact".to_owned(), 0.10),
        (CHATTER.to_owned(), 0.05),
    ]);
    fake.set_answer_for(MEMORY_QUESTION, Answer::choice("decision", decision));
    let worth = pollster::block_on(j.memory_gate(MARKER)).expect("answered");
    assert!(worth.outcome);
    assert_eq!(worth.label.as_deref(), Some("decision"));
    // The threshold is the memory one, not the triage one.
    assert_eq!(
        worth.judgement.threshold,
        JudgeSettings::default().classifier.memory
    );

    // Top-k, as TypeSafe answers it: the durable share is 0.2/0.5 = 0.4,
    // under the 0.50 bar. Read as `1 - P(chatter)` it would look 70% durable.
    let top_k = BTreeMap::from([(CHATTER.to_owned(), 0.30), ("decision".to_owned(), 0.20)]);
    fake.set_answer_for(MEMORY_QUESTION, Answer::choice(CHATTER, top_k));
    let decided = pollster::block_on(j.memory_gate("short message")).expect("answered");
    assert!(
        (decided.judgement.score - 0.4).abs() < 1e-6,
        "{}",
        decided.judgement.score
    );
    assert!(!decided.outcome);
    // A missing `chatter` is not certainty; the answer said nothing about it.
    let unweighed = BTreeMap::from([("decision".to_owned(), 0.9)]);
    fake.set_answer_for(MEMORY_QUESTION, Answer::choice("decision", unweighed));
    let err = pollster::block_on(j.memory_gate("short message")).expect_err("unusable");
    // The missing-label reason, not the NaN one — and the reason carries no
    // label text, so there is nothing in it to quote.
    assert!(
        matches!(err, JudgeError::MalformedAnswer { reason } if reason.contains("no probability")),
        "{err:?}"
    );
    // Nor is a NaN, which compares false against every threshold.
    let not_a_number =
        BTreeMap::from([(CHATTER.to_owned(), f32::NAN), ("decision".to_owned(), 0.5)]);
    fake.set_answer_for(MEMORY_QUESTION, Answer::choice(CHATTER, not_a_number));
    let err = pollster::block_on(j.memory_gate("short message")).expect_err("unusable");
    assert!(matches!(err, JudgeError::MalformedAnswer { .. }), "{err:?}");
}

#[test]
fn a_state_longer_than_the_adapters_ceiling_is_refused_not_trimmed() {
    let classifier = FakeClassifier::new(ClassifierMode::Deterministic);
    classifier.set_profile(Calibration::Classifier, 32);
    let j = Judge::new(Arc::new(classifier.clone()), JudgeSettings::default());
    let err = pollster::block_on(j.triage(&MARKER.repeat(8))).expect_err("too long");
    assert!(
        matches!(err, JudgeError::StateTooLong { limit: 32 }),
        "{err:?}"
    );
    assert!(
        classifier.asks().is_empty(),
        "it was refused before the wire"
    );
}

#[test]
fn the_contradiction_check_delimits_the_two_facts() {
    let (j, fake) = scripting(Calibration::Classifier, JudgeSettings::default());
    fake.set_answer_for(CONFLICT_QUESTION, noul(0.95));
    let decided =
        pollster::block_on(j.contradiction("the launch is in March", MARKER)).expect("answered");
    assert!(decided.outcome);
    let state = fake.last().expect("one ask").state;
    assert!(state.contains(MARKER));
    assert!(state.contains(livingbrain_judge::FACT_DELIMITER));
}

/// A classifier that answers nothing, which no real adapter does: a dropped
/// question id must be an error here, not a quiet "no".
struct Silent;

#[async_trait::async_trait]
impl cratefield_core::Classifier for Silent {
    fn profile(&self) -> cratefield_core::ClassifierProfile {
        cratefield_core::ClassifierProfile::new(Calibration::Classifier, DEFAULT_MAX_STATE_CHARS)
    }
    async fn ask(
        &self,
        _state: &str,
        _questions: &BTreeMap<String, cratefield_core::Question>,
    ) -> Result<BTreeMap<String, Answer>, cratefield_core::ClassifierError> {
        Ok(BTreeMap::new())
    }
}

#[test]
fn errors_are_errors_and_never_a_silent_no() {
    let j = Judge::new(Arc::new(Silent), JudgeSettings::default());
    let err = pollster::block_on(j.triage("thread")).expect_err("no answer");
    assert!(matches!(err, JudgeError::MissingAnswer { .. }), "{err}");
    assert!(err.to_string().contains(TRIAGE_QUESTION), "{err}");

    // An unwired port keeps the port's own error, wrapped whole.
    let unwired = Judge::new(
        Arc::new(FakeClassifier::new(ClassifierMode::NotConfigured)),
        JudgeSettings::default(),
    );
    let err = pollster::block_on(unwired.triage("thread")).expect_err("unwired");
    assert!(matches!(err, JudgeError::Classifier(_)), "{err}");
}
