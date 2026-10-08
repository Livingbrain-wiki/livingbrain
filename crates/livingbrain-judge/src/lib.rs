//! `livingbrain-judge`: the three questions a brain asks before it acts.
//!
//! Triage ("should the brain reply here?"), the memory gate ("is this
//! worth remembering?") and the contradiction check ("do these two
//! facts disagree?") are one shape: a typed question set, one `ask` against
//! one state, a threshold that turns a probability into a decision. The
//! policy — which question, which labels, which number — is in this crate;
//! the classifier only answers.
//!
//! **Confidence is per family.** A `0.8` from a purpose-trained classifier
//! is not a `0.8` elicited from a language model, so every threshold is
//! stored per family ([`JudgeSettings`]) and picked per call from the
//! classifier's own profile ([`Judge::thresholds`]). [`pick`] chooses which
//! classifier a workspace has at all.
//!
//! **Proactivity moves one number**, the triage threshold
//! ([`Thresholds::triage_for`]), and nothing else. [`Proactivity::Off`] is
//! a threshold no probability can reach, not a branch: answering a person
//! who said the brain's name is the caller's call, not a judgement's.
//!
//! **The record carries no text.** A [`Judgement`] has probabilities and a
//! threshold, never the thread, the message or the two facts. There is no
//! `publish(&EventBus)` either: `emit_in` needs a `Scope` with a live
//! `Defer`, which a library owning no request cannot fabricate. The caller
//! logs it under [`EVENT_NAME`].

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use cratefield_core::{Answer, Calibration, Classifier, ClassifierError, Question};

/// The event-log name a [`Judgement`] is filed under.
pub const EVENT_NAME: &str = "judge.decided";
/// The triage question id.
pub const TRIAGE_QUESTION: &str = "reply";
/// The memory gate's question id.
pub const MEMORY_QUESTION: &str = "kind";
/// The contradiction question id.
pub const CONFLICT_QUESTION: &str = "conflict";
/// The one memory-gate label worth keeping never. The gate is the mass of
/// everything *else*, so this is the only label whose probability subtracts.
pub const CHATTER: &str = "chatter";
/// Separates the two facts of a contradiction check, so the classifier
/// reads two statements rather than one run-on sentence.
pub const FACT_DELIMITER: &str = "\n--- end of fact ---\n";

/// How eagerly a brain replies: one triage threshold per family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Proactivity {
    /// Never reply unprompted; mentions are the caller's to answer.
    Off,
    /// Reply only when nearly certain.
    Quiet,
    /// The default: reply when the classifier leans that way.
    #[default]
    Normal,
    /// Reply on a lean.
    Eager,
}

/// One family's thresholds, in `[0, 1]`. Above `1.0` can never be met,
/// which is how [`Proactivity::Off`] is spelled.
///
/// Deliberately not `#[serde(default)]`: a family object is all three
/// numbers or it does not load. Filling one would have to guess whose
/// numbers to borrow, and guessing wrong writes the language model's budget
/// as the classifier's. [`JudgeSettings`] defaults an *absent* family.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Thresholds {
    /// P(reply) at which the brain replies.
    pub triage: f32,
    /// Non-chatter mass at which a message is remembered.
    pub memory: f32,
    /// P(conflict) at which two facts disagree.
    pub contradiction: f32,
}

impl Thresholds {
    /// A purpose-trained classifier: sharper numbers, so a lower bar.
    pub const CLASSIFIER: Self = Self {
        triage: 0.70,
        memory: 0.50,
        contradiction: 0.80,
    };
    /// A language model: flatter, so every bar is higher.
    pub const LANGUAGE_MODEL: Self = Self {
        triage: 0.85,
        memory: 0.65,
        contradiction: 0.90,
    };
    /// Named for serde's `default =`, which needs a path to a function.
    #[must_use]
    pub fn classifier() -> Self {
        Self::CLASSIFIER
    }
    /// As [`Self::classifier`], for the language-model family.
    #[must_use]
    pub fn language_model() -> Self {
        Self::LANGUAGE_MODEL
    }

    /// The triage threshold this proactivity means here: the one place
    /// proactivity and thresholds meet.
    #[must_use]
    pub fn triage_for(self, proactivity: Proactivity) -> f32 {
        let base = self.triage;
        match proactivity {
            // Above one on purpose: unreachable rather than a special case
            // in the comparison below, so `Off` takes the same path.
            Proactivity::Off => 2.0,
            Proactivity::Quiet => (base + 1.0) / 2.0,
            Proactivity::Normal => base,
            Proactivity::Eager => base * 0.75,
        }
    }

    /// This family's thresholds with triage replaced by what `proactivity`
    /// means. Memory and contradiction are untouched.
    #[must_use]
    pub fn with_proactivity(self, proactivity: Proactivity) -> Self {
        Self {
            triage: self.triage_for(proactivity),
            ..self
        }
    }
}

/// What a workspace configured. Serde so a future per-workspace settings row
/// stores it as one JSON column.
///
/// An absent family falls back to that family's own defaults, so a row
/// written before thresholds were tuned loads rather than resets a
/// workspace; a family that is present must be complete (see [`Thresholds`]).
/// An unknown proactivity value **fails to deserialize, on purpose**:
/// storing a stranger's idea of `Off` is how a brain gets louder than its
/// owner asked for.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct JudgeSettings {
    /// How eagerly the brain replies. Memory and contradiction ignore it.
    pub proactivity: Proactivity,
    #[serde(default = "Thresholds::classifier")]
    pub classifier: Thresholds,
    #[serde(default = "Thresholds::language_model")]
    pub language_model: Thresholds,
}

impl Default for JudgeSettings {
    fn default() -> Self {
        Self {
            proactivity: Proactivity::default(),
            classifier: Thresholds::CLASSIFIER,
            language_model: Thresholds::LANGUAGE_MODEL,
        }
    }
}

impl JudgeSettings {
    /// The thresholds for `calibration`, proactivity applied to triage.
    #[must_use]
    pub fn thresholds_for(&self, calibration: Calibration) -> Thresholds {
        self.family(calibration).with_proactivity(self.proactivity)
    }

    /// The family's stored thresholds, before proactivity.
    #[must_use]
    pub fn family(&self, calibration: Calibration) -> Thresholds {
        match calibration {
            Calibration::Classifier => self.classifier,
            Calibration::LanguageModel => self.language_model,
        }
    }
}

/// The one noul [`Judge::triage`] asks.
#[must_use]
pub fn triage_questions() -> BTreeMap<String, Question> {
    BTreeMap::from([(
        TRIAGE_QUESTION.to_owned(),
        Question::Noul {
            instructions: "Should the brain reply in this thread? It should when someone asked it something, raised something it can settle, or is waiting on it; not when the thread is closed, the others are talking past each other, or the reply would be noise."
                .to_owned(),
        },
    )])
}

/// The one choice [`Judge::memory_gate`] asks, over the four kinds of
/// message. [`CHATTER`] is the only kind not worth keeping.
#[must_use]
pub fn memory_questions() -> BTreeMap<String, Question> {
    BTreeMap::from([(
        MEMORY_QUESTION.to_owned(),
        Question::Choice {
            instructions: "Which kind of message is this?".to_owned(),
            criteria: {
                let kind = |name: &str, what: &str| (name.to_owned(), what.to_owned());
                BTreeMap::from([
                    kind("decision", "A commitment made: a plan, a deadline."),
                    kind("owner", "A thing and who is responsible for it."),
                    kind("durable_fact", "Still true tomorrow, whatever it is."),
                    kind(CHATTER, "Small talk and noise: read once, never keep."),
                ])
            },
        },
    )])
}

/// The one noul [`Judge::contradiction`] asks.
#[must_use]
pub fn contradiction_questions() -> BTreeMap<String, Question> {
    BTreeMap::from([(
        CONFLICT_QUESTION.to_owned(),
        Question::Noul {
            instructions: "Do these two statements conflict? They do when both cannot be true at once: a different value for the same thing, a decision reversed, a deadline that moved. Repetition, elaboration and two true statements that are merely related are not a conflict."
                .to_owned(),
        },
    )])
}

/// One decision and the record of how it was reached: probabilities and a
/// threshold, never the state. See the crate header.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Judgement {
    pub decision: &'static str,
    /// The family the numbers came from, and the only one a threshold is
    /// meaningful against.
    pub calibration: Calibration,
    pub questions: Vec<String>,
    /// Per question id, the probability mass per label.
    pub probabilities: BTreeMap<String, BTreeMap<String, f32>>,
    pub threshold: f32,
    pub score: f32,
    pub outcome: bool,
}

/// What a call decided.
#[derive(Debug, Clone, PartialEq)]
pub struct Decided {
    /// Reply / remember / conflict.
    pub outcome: bool,
    pub label: Option<String>,
    pub judgement: Judgement,
}

/// The purpose-trained adapter when the workspace has a key for one, the
/// language-model fallback otherwise.
#[must_use]
pub fn pick(
    jev: Option<Arc<dyn Classifier>>,
    fallback: Arc<dyn Classifier>,
) -> Arc<dyn Classifier> {
    jev.unwrap_or(fallback)
}

/// What went wrong deciding something.
///
/// `Debug` is [`Display`], not the derived one: [`ClassifierError`]'s own
/// `Debug` prints the raw provider string, and a `{:?}` would undo the
/// scrubbing `Display` exists to do.
pub enum JudgeError {
    Classifier(ClassifierError),
    MissingAnswer {
        /// The question id that came back without an answer.
        question: String,
    },
    /// The adapter reported numbers that cannot decide anything: a label
    /// with no mass, a non-finite mass, or masses summing to zero. The
    /// reason is a static string — the numbers belong in the [`Judgement`].
    MalformedAnswer {
        /// Why the answer could not be scored. Never its text.
        reason: &'static str,
    },
    /// The state is over the adapter's ceiling, so it would be truncated
    /// and the answer would come back confident and wrong.
    StateTooLong {
        /// The `max_state_chars` that was exceeded.
        limit: usize,
    },
}

impl fmt::Debug for JudgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl fmt::Display for JudgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // The inner error's Display already scrubs what the provider sent.
            Self::Classifier(error) => write!(f, "{error}"),
            Self::MissingAnswer { question } => {
                write!(f, "classifier returned no answer for question {question:?}")
            }
            Self::MalformedAnswer { reason } => write!(f, "unusable classifier answer: {reason}"),
            Self::StateTooLong { limit } => {
                write!(f, "state is over the {limit}-char classifier ceiling")
            }
        }
    }
}

impl std::error::Error for JudgeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Classifier(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ClassifierError> for JudgeError {
    fn from(error: ClassifierError) -> Self {
        Self::Classifier(error)
    }
}

/// Holds a classifier and a workspace's settings, and turns one `ask` into
/// one decision. Each method fails as [`JudgeError`] rather than guessing.
pub struct Judge {
    classifier: Arc<dyn Classifier>,
    settings: JudgeSettings,
}

impl Judge {
    /// A judge over one classifier; the family its profile names picks the
    /// thresholds.
    #[must_use]
    pub fn new(classifier: Arc<dyn Classifier>, settings: JudgeSettings) -> Self {
        Self {
            classifier,
            settings,
        }
    }

    /// The settings as given.
    #[must_use]
    pub fn settings(&self) -> &JudgeSettings {
        &self.settings
    }

    /// This classifier's own family decides which thresholds apply, read per
    /// call so an adapter swapped in at runtime cannot leave a threshold
    /// belonging to the other family behind.
    #[must_use]
    pub fn thresholds(&self) -> Thresholds {
        self.settings
            .thresholds_for(self.classifier.profile().calibration)
    }

    /// Refuses a state the adapter would truncate: a silently trimmed state
    /// is the one failure a classifier must not have, the answer coming
    /// back confident and wrong.
    fn check_state(&self, state: &str) -> Result<(), JudgeError> {
        let limit = self.classifier.profile().max_state_chars;
        if state.chars().count() > limit {
            return Err(JudgeError::StateTooLong { limit });
        }
        Ok(())
    }

    /// Should the brain reply in `thread`? One ask, scored on P(true).
    pub async fn triage(&self, thread: &str) -> Result<Decided, JudgeError> {
        self.check_state(thread)?;
        let threshold = self.thresholds().triage;
        let questions = triage_questions();
        let answers = self.classifier.ask(thread, &questions).await?;
        let answer = take(&answers, TRIAGE_QUESTION)?;
        Ok(self.decide(
            "triage",
            questions,
            TRIAGE_QUESTION,
            answer,
            mass(answer, "true")?,
            threshold,
        ))
    }

    /// Is `message` worth remembering? Scored on the non-chatter mass, so
    /// an answer spreading thinly across the three durable kinds still
    /// passes while one undecided about chatter does not. The likely kind
    /// comes back on [`Decided::label`].
    pub async fn memory_gate(&self, message: &str) -> Result<Decided, JudgeError> {
        self.check_state(message)?;
        let threshold = self.thresholds().memory;
        let questions = memory_questions();
        let answers = self.classifier.ask(message, &questions).await?;
        let answer = take(&answers, MEMORY_QUESTION)?;
        let decided = self.decide(
            "memory_gate",
            questions,
            MEMORY_QUESTION,
            answer,
            durable_mass(answer)?,
            threshold,
        );
        Ok(Decided {
            label: most_likely(answer),
            ..decided
        })
    }

    /// Do `first` and `second` conflict? One ask over both facts joined by
    /// [`FACT_DELIMITER`], scored on P(true).
    pub async fn contradiction(&self, first: &str, second: &str) -> Result<Decided, JudgeError> {
        let threshold = self.thresholds().contradiction;
        let questions = contradiction_questions();
        let state = format!("{first}{FACT_DELIMITER}{second}");
        self.check_state(&state)?;
        let answers = self.classifier.ask(&state, &questions).await?;
        let answer = take(&answers, CONFLICT_QUESTION)?;
        Ok(self.decide(
            "contradiction",
            questions,
            CONFLICT_QUESTION,
            answer,
            mass(answer, "true")?,
            threshold,
        ))
    }

    fn decide(
        &self,
        decision: &'static str,
        questions: BTreeMap<String, Question>,
        question_id: &str,
        answer: &Answer,
        score: f32,
        threshold: f32,
    ) -> Decided {
        let outcome = score >= threshold;
        Decided {
            outcome,
            label: None,
            judgement: Judgement {
                decision,
                calibration: self.classifier.profile().calibration,
                questions: questions.keys().cloned().collect(),
                probabilities: BTreeMap::from([(
                    question_id.to_owned(),
                    answer.probabilities.clone(),
                )]),
                threshold,
                score,
                outcome,
            },
        }
    }
}

/// The one answer for `question_id`, or [`JudgeError::MissingAnswer`].
fn take<'a>(
    answers: &'a BTreeMap<String, Answer>,
    question_id: &str,
) -> Result<&'a Answer, JudgeError> {
    answers
        .get(question_id)
        .ok_or_else(|| JudgeError::MissingAnswer {
            question: question_id.to_owned(),
        })
}

/// The probability mass on `label`, or [`JudgeError::MalformedAnswer`].
///
/// A missing label is not zero — an answer that never mentions it has said
/// nothing about it — and a NaN would compare false against every
/// threshold and decide the question silently.
fn mass(answer: &Answer, label: &str) -> Result<f32, JudgeError> {
    let value = answer
        .probabilities
        .get(label)
        .copied()
        .ok_or(JudgeError::MalformedAnswer {
            reason: "no probability was reported for a label that was asked about",
        })?;
    if !value.is_finite() {
        return Err(JudgeError::MalformedAnswer {
            reason: "a reported probability is not a finite number",
        });
    }
    Ok(value)
}

/// The share of the reported mass that is not chatter, `(total - chatter) /
/// total`.
///
/// The port does not require masses summing to one and a top-k adapter
/// reports only the labels it has, so the gate divides by what was
/// actually reported rather than reading `0.30` as "70% durable".
fn durable_mass(answer: &Answer) -> Result<f32, JudgeError> {
    // Every mass is checked, not just chatter's: one NaN in the sum is a NaN out.
    if answer.probabilities.values().any(|mass| !mass.is_finite()) {
        return Err(JudgeError::MalformedAnswer {
            reason: "a reported probability is not a finite number",
        });
    }
    let chatter = mass(answer, CHATTER)?;
    let total: f32 = answer.probabilities.values().copied().sum();
    if total <= 0.0 {
        return Err(JudgeError::MalformedAnswer {
            reason: "the reported probabilities sum to zero",
        });
    }
    Ok((total - chatter) / total)
}

/// The label carrying the most mass; ties go to set order, so an undecided
/// answer names the same kind every time.
fn most_likely(answer: &Answer) -> Option<String> {
    answer
        .probabilities
        .iter()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map(|(label, _)| label.clone())
}
