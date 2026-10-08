//! The family mapping, proven against the two real adapters.
//!
//! Everything else in the suite thresholds against a fake that agrees with
//! whatever this crate assumes. This file is the check that it is right: a
//! `TypeSafe` adapter reports [`Calibration::Classifier`] and a
//! `ClassifierLlm` reports [`Calibration::LanguageModel`], so `Judge` picks
//! the two different threshold sets — and the same 0.75 that the classifier
//! family answers "reply" on, the language-model family answers "say
//! nothing" on. Both are driven end to end from a canned provider response,
//! because a profile nobody reads is a profile that drifts unnoticed.

use std::sync::Arc;

use cratefield_adapter_classifier_llm::ClassifierLlm;
use cratefield_adapter_typesafe::TypeSafe;
use cratefield_core::{Calibration, Classifier};
use cratefield_testing::{FakeHttpClient, FakeTextModel, FixedClock, TextModelMode};
use livingbrain_judge::{Judge, JudgeSettings, TRIAGE_QUESTION, pick};

/// One TypeSafe answer: a noul at p = 0.75, the vendor's own wire shape.
const JEV_BODY: &str = r#"{"model":"jev-2026","answers":{"reply":{"type":"noul","noul":0.75}}}"#;
/// One language-model answer to the same question, same 0.75.
const LLM_BODY: &str =
    r#"{"answers":{"reply":{"value":"true","probabilities":{"true":0.75,"false":0.25}}}}"#;

fn jev() -> Arc<dyn Classifier> {
    Arc::new(TypeSafe::new(
        Arc::new(FakeHttpClient::ok_json(JEV_BODY)),
        Arc::new(FixedClock(time::OffsetDateTime::UNIX_EPOCH)),
        Some("typesafe-key".to_owned()),
    ))
}

fn llm() -> Arc<dyn Classifier> {
    Arc::new(ClassifierLlm::new(Arc::new(FakeTextModel::new(
        TextModelMode::Reply(LLM_BODY.to_owned()),
    ))))
}

#[test]
fn the_real_adapters_report_the_families_the_settings_are_keyed_by() {
    assert_eq!(jev().profile().calibration, Calibration::Classifier);
    assert_eq!(llm().profile().calibration, Calibration::LanguageModel);
}

#[test]
fn the_same_answer_decides_one_way_in_each_family() {
    let settings = JudgeSettings::default();
    let sharp = Judge::new(jev(), settings);
    let flat = Judge::new(llm(), settings);

    assert_eq!(sharp.thresholds().triage, settings.classifier.triage);
    assert_eq!(flat.thresholds().triage, settings.language_model.triage);

    let sharp = pollster::block_on(sharp.triage("are we still shipping Friday?"))
        .expect("typesafe answered");
    let flat = pollster::block_on(flat.triage("are we still shipping Friday?"))
        .expect("the language model answered");

    // Both providers returned p(reply) = 0.75.
    assert_eq!(sharp.judgement.score, 0.75);
    assert_eq!(flat.judgement.score, 0.75);
    assert!(sharp.outcome, "0.75 clears the classifier bar of 0.70");
    assert!(
        !flat.outcome,
        "0.75 does not clear the language-model bar of 0.85"
    );
    assert_eq!(sharp.judgement.calibration, Calibration::Classifier);
    assert_eq!(flat.judgement.calibration, Calibration::LanguageModel);
    assert_eq!(sharp.judgement.questions, vec![TRIAGE_QUESTION]);
}

#[test]
fn pick_gives_a_workspace_the_classifier_it_has_a_key_for() {
    let chosen = pick(Some(jev()), llm());
    assert_eq!(chosen.profile().calibration, Calibration::Classifier);
    // With no key for the purpose-trained one, the fallback is what is left.
    let chosen = pick(None, llm());
    assert_eq!(chosen.profile().calibration, Calibration::LanguageModel);
}
