//! Triage and compaction over synthetic sessions: the decisions, what
//! compaction keeps and drops, what it saves, and how a classifier's
//! answers fold in.

mod support;

use std::collections::BTreeMap;
use std::sync::Mutex;

use cratefield_core::{
    Answer, Calibration, Classifier, ClassifierError, ClassifierProfile, Question,
};
use livingbrain_compiler::compact::{self, Keep};
use livingbrain_compiler::record;
use livingbrain_compiler::triage::{self, Plan, Strictness, Triage};
use support::sessions;

fn decide(body: &str, strictness: Strictness) -> (Triage, String) {
    let session = record::parse(body).expect("a session");
    let decision = triage::rule(&triage::signals(&session), strictness);
    (decision.triage, decision.reason)
}

#[test]
fn real_feature_work_is_compiled_in_full_at_every_strictness() {
    for strictness in Strictness::ALL {
        assert_eq!(
            decide(&sessions::feature_work(), strictness),
            (Triage::Full, "changes-files".to_owned())
        );
    }
}

#[test]
fn a_failing_retry_loop_that_changed_nothing_is_summary_only() {
    for strictness in Strictness::ALL {
        assert_eq!(
            decide(&sessions::failing_loop(), strictness),
            (Triage::Summary, "failing-loop".to_owned())
        );
    }
    let session = record::parse(&sessions::failing_loop()).expect("a session");
    let signals = triage::signals(&session);
    assert_eq!(signals.worst_retry, 7);
    assert_eq!(signals.changes, 0);
    let text = triage::summary_text(&signals, "failing-loop");
    assert!(text.contains("7 failed"), "{text}");
    assert!(text.contains("no files changed"), "{text}");
}

#[test]
fn a_lookup_with_no_changes_is_summary_only_unless_lenient() {
    assert_eq!(
        decide(&sessions::no_changes(), Strictness::Balanced),
        (Triage::Summary, "no-changes".to_owned())
    );
    assert_eq!(
        decide(&sessions::no_changes(), Strictness::Lenient).0,
        Triage::Full
    );
}

#[test]
fn a_greeting_is_skipped_but_not_when_lenient() {
    assert_eq!(
        decide(&sessions::trivial(), Strictness::Balanced),
        (Triage::Skip, "trivial".to_owned())
    );
    assert_eq!(
        decide(&sessions::trivial(), Strictness::Lenient).0,
        Triage::Full
    );
    assert_eq!(
        decide(&support::sessions::record("e", vec![]), Strictness::Lenient),
        (Triage::Skip, "empty".to_owned())
    );
}

#[test]
fn compaction_keeps_errors_commands_and_files_and_drops_the_noise() {
    let session = record::parse(&sessions::feature_work()).expect("a session");
    let (compacted, stats) = compact::compact(&session, &BTreeMap::new());
    let (chunks, _) = compacted.chunks(1_000_000, 1);
    let text = &chunks[0];
    // Kept: the commands, the failure and where it was, the exit code, the
    // files, the agent's decisions, the person's words.
    for kept in [
        "$ cargo test -p livingbrain-venture",
        "panicked at src/store.rs:212:9",
        "test result: FAILED. 47 passed; 1 failed",
        "exit 101",
        "crates/venture/migrations/0022_pages_0006_facts.sql",
        "Decision: compare statements, not comments.",
        "Move the facts table to D1",
        "git commit -am",
    ] {
        assert!(text.contains(kept), "lost {kept:?}:\n{text}");
    }
    // Dropped: the progress lines, the passing test lines, the file bodies.
    assert!(!text.contains("Compiling dependency-100"), "{text}");
    assert!(!text.contains("case_30 ... ok"), "{text}");
    assert!(!text.contains("let value_90"), "{text}");
    assert!(stats.chars_after < stats.chars_before);
    // The source is untouched: the original session still has every byte.
    assert_eq!(
        session,
        record::parse(&sessions::feature_work()).expect("a session")
    );
}

#[test]
fn a_retry_loop_compacts_to_one_call_and_a_count() {
    let session = record::parse(&sessions::failing_loop()).expect("a session");
    let (compacted, stats) = compact::compact(&session, &BTreeMap::new());
    let (chunks, _) = compacted.chunks(1_000_000, 1);
    let text = &chunks[0];
    assert_eq!(stats.repeats_collapsed, 6);
    assert_eq!(text.matches("ERR_CONNECTION_REFUSED").count(), 1, "{text}");
    assert!(
        text.contains("same call as in turn 2, again, failed with exit 1 (#7)"),
        "{text}"
    );
}

#[test]
fn a_classifier_marking_an_output_verbatim_keeps_it() {
    let session = record::parse(&sessions::feature_work()).expect("a session");
    // The failing test run is turn 2, call 2.
    let keep = BTreeMap::from([((2, 2), Keep::Verbatim)]);
    let (compacted, _) = compact::compact(&session, &keep);
    let (chunks, _) = compacted.chunks(1_000_000, 1);
    assert!(chunks[0].contains("assertion `left == right` failed"));
    // Word for word, with only runs of same-shape lines collapsed.
    assert!(chunks[0].contains("case_0 ... ok"), "{}", chunks[0]);
    assert!(chunks[0].contains("similar"), "{}", chunks[0]);
}

/// What a compile would send, in estimated input tokens, under the default
/// settings (4 calls × 32 000 characters): each call's transcript chunk plus
/// the fixed prompt and the default conventions it carries. (The pages shown
/// per call come on top, on both sides.)
fn sent_tokens(session: &record::Session) -> u64 {
    use livingbrain_compiler::prompt::{DEFAULT_SCHEMA, SYSTEM, estimate_tokens};
    let overhead = estimate_tokens(SYSTEM) + estimate_tokens(DEFAULT_SCHEMA);
    session
        .chunks(32_000, 4)
        .0
        .iter()
        .map(|chunk| estimate_tokens(chunk) + overhead)
        .sum()
}

/// The tokens-saved accounting on the kinds of session, printed for the
/// record (`cargo test -p livingbrain-compiler --test triage -- --nocapture`).
#[test]
fn tokens_saved_on_the_synthetic_sessions() {
    let mut total_before = 0;
    let mut total_after = 0;
    for (name, body) in [
        ("feature work", sessions::feature_work()),
        ("long feature", sessions::long_feature_work()),
        ("delegated", sessions::delegated()),
        ("failing loop", sessions::failing_loop()),
        ("lookup", sessions::no_changes()),
        ("greeting", sessions::trivial()),
    ] {
        let session = record::parse(&body).expect("a session");
        let decision = triage::rule(&triage::signals(&session), Strictness::Balanced);
        let before = sent_tokens(&session);
        let after = match decision.triage {
            Triage::Full => sent_tokens(&compact::compact(&session, &BTreeMap::new()).0),
            Triage::Summary | Triage::Skip => 0,
        };
        println!(
            "{name:>12}: {before:>6} → {after:>6} input tokens, {} calls → {} ({}, {})",
            session.chunks(32_000, 4).0.len(),
            if after == 0 {
                0
            } else {
                compact::compact(&session, &BTreeMap::new())
                    .0
                    .chunks(32_000, 4)
                    .0
                    .len()
            },
            decision.triage.as_str(),
            decision.reason
        );
        total_before += before;
        total_after += after;
    }
    println!(
        "{:>12}: {total_before:>6} → {total_after:>6} input tokens",
        "total"
    );
    assert!(total_after * 2 < total_before, "at least half saved");
}

/// A classifier that answers from a script, and remembers what it was asked.
struct Scripted {
    answers: BTreeMap<String, Answer>,
    asked: Mutex<Vec<(String, BTreeMap<String, Question>)>>,
}

impl Scripted {
    fn new(answers: Vec<(&str, Answer)>) -> Self {
        Self {
            answers: answers
                .into_iter()
                .map(|(id, answer)| (id.to_owned(), answer))
                .collect(),
            asked: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl Classifier for Scripted {
    fn profile(&self) -> ClassifierProfile {
        ClassifierProfile::new(Calibration::Classifier, 96_000)
    }

    async fn ask(
        &self,
        state: &str,
        questions: &BTreeMap<String, Question>,
    ) -> Result<BTreeMap<String, Answer>, ClassifierError> {
        self.asked
            .lock()
            .expect("lock")
            .push((state.to_owned(), questions.clone()));
        Ok(self
            .answers
            .iter()
            .filter(|(id, _)| questions.contains_key(*id))
            .map(|(id, answer)| (id.clone(), answer.clone()))
            .collect())
    }
}

fn choice(label: &str, p: f32) -> Answer {
    let mut probabilities = BTreeMap::from([
        ("skip".to_owned(), (1.0 - p) / 2.0),
        ("summary".to_owned(), (1.0 - p) / 2.0),
        ("full".to_owned(), (1.0 - p) / 2.0),
    ]);
    probabilities.insert(label.to_owned(), p);
    Answer::choice(label, probabilities).with_model("jev-1.13.0")
}

fn noul(p: f32) -> Answer {
    Answer::noul(
        p >= 0.5,
        BTreeMap::from([("true".to_owned(), p), ("false".to_owned(), 1.0 - p)]),
    )
}

fn plan(body: &str, classifier: &dyn Classifier, candidates: &[(String, String)]) -> Plan {
    let session = record::parse(body).expect("a session");
    let signals = triage::signals(&session);
    let rule = triage::rule(&signals, Strictness::Balanced);
    pollster::block_on(Plan::from_rule(rule).ask(
        classifier,
        &session,
        &signals,
        Strictness::Balanced,
        candidates,
    ))
}

#[test]
fn the_classifier_may_raise_a_decision_freely_and_lower_it_only_when_sure() {
    // Raise: the rule summarised the lookup, Jev says it is real.
    let raised = plan(
        &sessions::no_changes(),
        &Scripted::new(vec![("triage", choice("full", 0.4))]),
        &[],
    );
    assert_eq!(raised.decision.triage, Triage::Full);
    assert_eq!(raised.decision.reason, "jev:full");
    assert_eq!(raised.classifier_model.as_deref(), Some("jev-1.13.0"));
    // Lower, unsure: the rule stands.
    let unsure = plan(
        &sessions::failing_loop(),
        &Scripted::new(vec![("triage", choice("skip", 0.6))]),
        &[],
    );
    assert_eq!(unsure.decision.triage, Triage::Summary);
    assert_eq!(unsure.decision.reason, "failing-loop");
    // Lower, sure: taken.
    let sure = plan(
        &sessions::failing_loop(),
        &Scripted::new(vec![("triage", choice("skip", 0.95))]),
        &[],
    );
    assert_eq!(sure.decision.triage, Triage::Skip);
    // Never below full for a session that changed files.
    let floor = plan(
        &sessions::feature_work(),
        &Scripted::new(vec![("triage", choice("skip", 0.99))]),
        &[],
    );
    assert_eq!(floor.decision.triage, Triage::Full);
    assert_eq!(floor.decision.reason, "changes-files");
}

#[test]
fn the_classifier_picks_topic_pages_and_outputs_in_one_call() {
    let candidates = vec![
        (
            "coding-agents--claude--demo--migrations".to_owned(),
            "D1 migrations".to_owned(),
        ),
        (
            "coding-agents--claude--demo--billing".to_owned(),
            "Billing".to_owned(),
        ),
    ];
    let classifier = Scripted::new(vec![
        ("triage", choice("full", 0.9)),
        ("topic_0", noul(0.92)),
        ("topic_1", noul(0.04)),
        ("keep_0", noul(0.1)),
        ("keep_1", noul(0.95)),
    ]);
    let plan = plan(&sessions::feature_work(), &classifier, &candidates);
    assert_eq!(
        plan.topics,
        Some(vec!["coding-agents--claude--demo--migrations".to_owned()])
    );
    let asked = classifier.asked.lock().expect("lock");
    assert_eq!(asked.len(), 1, "one call per session");
    let (state, questions) = &asked[0];
    assert!(state.contains("files changed: crates/venture/migrations/0022"));
    assert!(state.len() <= triage::MAX_STATE_CHARS);
    assert!(questions.contains_key("triage"));
    // The bulky outputs are asked about, and the answers land on them.
    let keeps: Vec<_> = plan.keep.values().copied().collect();
    assert_eq!(keeps, vec![Keep::SignalOnly, Keep::Verbatim]);
}

#[test]
fn a_classifier_that_fails_leaves_the_rule() {
    struct Down;
    #[async_trait::async_trait]
    impl Classifier for Down {
        fn profile(&self) -> ClassifierProfile {
            ClassifierProfile::new(Calibration::Classifier, 96_000)
        }
        async fn ask(
            &self,
            _state: &str,
            _questions: &BTreeMap<String, Question>,
        ) -> Result<BTreeMap<String, Answer>, ClassifierError> {
            Err(ClassifierError::Transient { retry_after: None })
        }
    }
    let plan = plan(
        &sessions::failing_loop(),
        &Down,
        &[("a".into(), "A".into())],
    );
    assert_eq!(plan.decision.reason, "failing-loop");
    assert_eq!(plan.topics, None, "every page shown, as without triage");
    assert!(plan.keep.is_empty());
}

#[test]
fn a_subagent_s_edits_count_and_its_work_is_outlined_not_dropped() {
    let session = record::parse(&sessions::delegated()).expect("a schema-3 session");
    let signals = triage::signals(&session);
    assert_eq!(
        signals.changes, 1,
        "the subagent's edit is the session's change"
    );
    assert_eq!(signals.files_changed, vec!["tests/retry.rs".to_owned()]);
    assert_eq!(
        triage::rule(&signals, Strictness::Strict),
        triage::Decision::new(Triage::Full, "changes-files")
    );
    let (compacted, _) = compact::compact(&session, &BTreeMap::new());
    let (chunks, _) = compacted.chunks(1_000_000, 1);
    let text = &chunks[0];
    for kept in [
        "↳ subagent general-purpose · completed: Fix the flaky retry test",
        "- Edit · tests/retry.rs",
        "failed with exit 101",
        "answer: Fixed: the test used the wall clock",
    ] {
        assert!(text.contains(kept), "lost {kept:?}:\n{text}");
    }
    assert!(
        !text.contains("let value_200"),
        "the subagent's reads stay in the source"
    );
    // The uncompacted renderer cuts the whole subagent to one JSON line.
    let (raw, _) = session.chunks(1_000_000, 1);
    assert!(!raw[0].contains("answer: Fixed"));
    // The digest Jev reads names the subagent's work in one line.
    let digest = triage::digest(&session, &signals);
    assert!(
        digest.contains("subagent general-purpose: 4 calls, 1 failed, 1 changes, completed"),
        "{digest}"
    );
}
