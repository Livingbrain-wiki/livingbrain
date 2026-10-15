//! Triage: before a session costs a model call, decide how much of a
//! compile it deserves.
//!
//! - **`skip`** — nothing happened: an empty or aborted session, a greeting.
//!   The job ends `skipped-trivial`; no model call, no page written.
//! - **`summary`** — something happened but the wiki has little to learn: a
//!   session that changed no file and went round a failing loop, or (at the
//!   stricter settings) any short session that changed nothing. The job
//!   ends `summary-only`; the server appends one model-free line to the
//!   folder's log saying what the session did. No model call.
//! - **`full`** — real work: the normal compile, on the compacted transcript
//!   ([`crate::compact`]), shown only the pages its topics touch.
//!
//! **Nothing is lost by a wrong call.** The source stays in the ledger,
//! untouched, and a `skipped-trivial` or `summary-only` job can be compiled
//! in full at any time ("Compile fully", `POST /v1/coding-agents/compile-fully`).
//! And the triage leans one way: a session that changed a file is always
//! compiled in full, whatever the rule or the classifier says.
//!
//! **Two deciders.** The rule ([`rule`]) is free and always there: counts
//! from the record — turns, tool calls, failures, file changes. When the
//! venture wires a [`Classifier`] (Jev, TypeSafe's System One), one call per
//! session also asks it ([`Plan::ask`]): a `choice` between the three, which
//! existing topic pages the session bears on, and which bulky tool outputs
//! are needed word for word. The classifier may always *raise* the rule's
//! decision; it may lower it only when it is confident (per [`Strictness`]),
//! and never below `full` for a session that changed files. A classifier
//! that is unwired, slow or wrong leaves the rule's decision in place.

use std::collections::BTreeMap;

use cratefield_core::{AnswerValue, Classifier, Question};
use livingbrain_redact::{Policy, redact};

use crate::compact::{CallRef, Keep};
use crate::record::{Session, ToolCall, cap};

/// What a session gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Triage {
    Skip,
    Summary,
    Full,
}

impl Triage {
    /// The wire name: `skip`, `summary`, `full`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Skip => "skip",
            Self::Summary => "summary",
            Self::Full => "full",
        }
    }

    fn parse(label: &str) -> Option<Self> {
        match label {
            "skip" => Some(Self::Skip),
            "summary" => Some(Self::Summary),
            "full" => Some(Self::Full),
            _ => None,
        }
    }
}

/// How readily the triage holds a session back from a full compile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Strictness {
    /// Only empty sessions are skipped, only failing loops summarised; the
    /// classifier must be very sure to lower a decision.
    Lenient,
    /// The default.
    #[default]
    Balanced,
    /// Any short session that changed nothing is summarised.
    Strict,
}

impl Strictness {
    /// Every level, in the order the settings list them; the stored number
    /// is the index.
    pub const ALL: [Strictness; 3] = [Self::Lenient, Self::Balanced, Self::Strict];

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lenient => "lenient",
            Self::Balanced => "balanced",
            Self::Strict => "strict",
        }
    }

    /// From a stored or configured name (`lenient`/`balanced`/`strict`) or
    /// its index (`0`/`1`/`2`).
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim().to_ascii_lowercase();
        Self::ALL
            .into_iter()
            .enumerate()
            .find(|(index, level)| level.as_str() == value || index.to_string() == value)
            .map(|(_, level)| level)
    }

    /// The stored number.
    #[must_use]
    pub fn index(self) -> u64 {
        match self {
            Self::Lenient => 0,
            Self::Balanced => 1,
            Self::Strict => 2,
        }
    }

    /// From the stored number; out of range is the default.
    #[must_use]
    pub fn from_index(index: u64) -> Self {
        match index {
            0 => Self::Lenient,
            2 => Self::Strict,
            _ => Self::Balanced,
        }
    }

    /// How confident the classifier must be to lower the rule's decision.
    fn lower_threshold(self) -> f32 {
        match self {
            Self::Lenient => 0.9,
            Self::Balanced => 0.75,
            Self::Strict => 0.6,
        }
    }
}

/// The counts the rule decides on, read from the record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Signals {
    pub user_turns: usize,
    pub assistant_turns: usize,
    /// Characters of the agent's own text.
    pub assistant_chars: usize,
    pub tool_calls: usize,
    pub failed_calls: usize,
    /// Calls that changed a file (an edit, a write, a patch) or the
    /// repository (a commit, a push).
    pub changes: usize,
    /// The files those calls named, distinct, in order.
    pub files_changed: Vec<String>,
    /// The most times one identical call failed.
    pub worst_retry: usize,
    /// The command (or tool) of that call.
    pub worst_retry_call: Option<String>,
    /// Characters of the rendered transcript.
    pub transcript_chars: usize,
    /// Whether any call carries more than its name (a `full` record): a
    /// version-1 record names its tools and nothing else, so "changed
    /// nothing" cannot be told from it, and the rule does not try.
    pub detailed: bool,
}

/// The tools that change a file whatever their input says.
const EDIT_TOOLS: [&str; 10] = [
    "edit",
    "multiedit",
    "write",
    "notebookedit",
    "apply_patch",
    "str_replace_editor",
    "str_replace_based_edit_tool",
    "create_file",
    "write_file",
    "edit_file",
];

/// Shell commands that change the repository.
const CHANGING_COMMANDS: [&str; 6] = [
    "git commit",
    "git push",
    "gh pr create",
    "git merge",
    "git rebase",
    "cargo publish",
];

/// Whether a call changed something worth a page.
#[must_use]
pub fn changes_something(call: &ToolCall) -> bool {
    if call.failed() {
        return false;
    }
    let name = call.name.to_ascii_lowercase();
    if EDIT_TOOLS.contains(&name.as_str()) || call.format.as_deref() == Some("diff") {
        return true;
    }
    call.input.as_deref().is_some_and(|input| {
        let input = input.to_ascii_lowercase();
        CHANGING_COMMANDS
            .iter()
            .any(|command| input.contains(command))
    })
}

/// The rule's inputs.
#[must_use]
pub fn signals(session: &Session) -> Signals {
    let mut out = Signals {
        transcript_chars: crate::compact::rendered_chars(session),
        ..Signals::default()
    };
    let mut failures: BTreeMap<String, usize> = BTreeMap::new();
    for turn in &session.turns {
        match turn.role.as_str() {
            "user" if !turn.text.trim().is_empty() => out.user_turns += 1,
            "assistant" => {
                out.assistant_turns += 1;
                out.assistant_chars += turn.text.trim().chars().count();
            }
            _ => {}
        }
        // A record whose calls carry names only still counts them.
        out.tool_calls += turn.calls.len().max(turn.tools.len());
        // A subagent's calls are the session's work too: an edit it made is
        // a change, a loop it went round is a loop.
        let nested: Vec<&ToolCall> = turn.calls.iter().flat_map(ToolCall::with_nested).collect();
        out.tool_calls += nested.len() - turn.calls.len();
        for call in nested {
            out.detailed |= call.input.is_some()
                || call.output.is_some()
                || call.status.is_some()
                || call.path.is_some();
            if call.failed() {
                out.failed_calls += 1;
                let key = format!(
                    "{} {}",
                    call.name,
                    cap(call.input.as_deref().unwrap_or_default().trim(), 80)
                );
                *failures.entry(key).or_default() += 1;
            }
            if changes_something(call) {
                out.changes += 1;
                if let Some(path) = &call.path
                    && !out.files_changed.contains(path)
                {
                    out.files_changed.push(path.clone());
                }
            }
        }
    }
    if let Some((call, count)) = failures.into_iter().max_by_key(|(_, count)| *count) {
        out.worst_retry = count;
        out.worst_retry_call = Some(call.trim().to_owned());
    }
    out
}

/// A triage decision and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub triage: Triage,
    /// A fixed-vocabulary reason: `empty`, `trivial`, `failing-loop`,
    /// `no-changes`, `work`, `changes-files`, prefixed `jev:` when the
    /// classifier's answer decided it, and `forced` for "Compile fully".
    pub reason: String,
}

impl Decision {
    /// A decision with its reason.
    #[must_use]
    pub fn new(triage: Triage, reason: &str) -> Self {
        Self {
            triage,
            reason: reason.to_owned(),
        }
    }
}

/// The free decision, from the counts alone.
#[must_use]
pub fn rule(signals: &Signals, strictness: Strictness) -> Decision {
    if signals.changes > 0 {
        return Decision::new(Triage::Full, "changes-files");
    }
    if signals.user_turns == 0 && signals.assistant_chars == 0 && signals.tool_calls == 0 {
        return Decision::new(Triage::Skip, "empty");
    }
    let trivial_chars = match strictness {
        Strictness::Lenient => 0,
        Strictness::Balanced => 300,
        Strictness::Strict => 1_200,
    };
    if signals.user_turns <= 1 && signals.tool_calls == 0 && signals.assistant_chars < trivial_chars
    {
        return Decision::new(Triage::Skip, "trivial");
    }
    // A loop: one call failing again and again, most calls failing, and
    // nothing changed.
    let looping = signals.worst_retry >= 3 && signals.failed_calls * 2 >= signals.tool_calls;
    if looping {
        return Decision::new(Triage::Summary, "failing-loop");
    }
    // Changed nothing and said little: a lookup, a question answered in a
    // line. A long explanation is kept — that is where learnings live.
    let quiet_chars = match strictness {
        Strictness::Lenient => 0,
        Strictness::Balanced => 800,
        Strictness::Strict => 4_000,
    };
    // Only a record that says what its calls did can show nothing
    // changed; one that names its tools only, or has none, is not judged
    // on that.
    if signals.assistant_chars < quiet_chars && (signals.detailed || signals.tool_calls == 0) {
        return Decision::new(Triage::Summary, "no-changes");
    }
    Decision::new(Triage::Full, "work")
}

/// The question ids of a classifier call.
const TRIAGE_ID: &str = "triage";
const TOPIC_PREFIX: &str = "topic_";
const KEEP_PREFIX: &str = "keep_";
/// The most topic pages and outputs one call asks about.
pub const MAX_TOPIC_QUESTIONS: usize = 24;
pub const MAX_KEEP_QUESTIONS: usize = 32;
/// The longest state one call carries (Jev's own state budget is 32k
/// tokens; this stays well inside it).
pub const MAX_STATE_CHARS: usize = 24_000;
/// An output shorter than this is not worth a question.
const BULKY_OUTPUT_CHARS: usize = 600;
/// P(true) a topic page needs to be shown.
const TOPIC_THRESHOLD: f32 = 0.5;
/// Below this P(true), an output is cut to its signal lines; above
/// [`VERBATIM_THRESHOLD`] it is kept word for word.
const DROP_THRESHOLD: f32 = 0.3;
const VERBATIM_THRESHOLD: f32 = 0.8;

/// The plan for one session: the decision, and when a classifier answered,
/// the topic pages to show and the outputs to keep or cut.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub decision: Decision,
    /// `None` shows every page in the folder, as before triage; `Some`
    /// shows the fixed pages plus these topic pages only.
    pub topics: Option<Vec<String>>,
    pub keep: BTreeMap<CallRef, Keep>,
    /// The classifier's model version, when one answered.
    pub classifier_model: Option<String>,
}

impl Plan {
    /// The plan from the rule alone.
    #[must_use]
    pub fn from_rule(decision: Decision) -> Self {
        Self {
            decision,
            topics: None,
            keep: BTreeMap::new(),
            classifier_model: None,
        }
    }

    /// Asks the classifier once about this session, and folds its answers
    /// into the rule's plan. `candidates` are the folder's topic pages
    /// (`slug`, title) beyond the fixed ones. Any failure — unwired, slow,
    /// refused, an answer outside the options — leaves the rule's plan.
    pub async fn ask(
        self,
        classifier: &dyn Classifier,
        session: &Session,
        signals: &Signals,
        strictness: Strictness,
        candidates: &[(String, String)],
    ) -> Self {
        let (state, questions, bulky) = questions(session, signals, candidates);
        let Ok(answers) = classifier.ask(&state, &questions).await else {
            return self;
        };
        let mut plan = self;
        plan.classifier_model = answers.values().find_map(|answer| answer.model.clone());
        if let Some(answer) = answers.get(TRIAGE_ID)
            && let AnswerValue::Choice(label) = &answer.value
            && let Some(picked) = Triage::parse(label)
        {
            let rule = plan.decision.triage;
            let raise = picked > rule;
            let lower = picked < rule
                && answer.confidence >= strictness.lower_threshold()
                && signals.changes == 0;
            if raise || lower {
                plan.decision = Decision::new(picked, &format!("jev:{}", picked.as_str()));
            }
        }
        let topics: Vec<String> = candidates
            .iter()
            .take(MAX_TOPIC_QUESTIONS)
            .enumerate()
            .filter(|(index, _)| {
                answers
                    .get(&format!("{TOPIC_PREFIX}{index}"))
                    .and_then(|answer| answer.probabilities.get("true"))
                    .is_some_and(|p| *p >= TOPIC_THRESHOLD)
            })
            .map(|(_, (slug, _))| slug.clone())
            .collect();
        // Only a call that answered every topic question narrows the pages:
        // a partial answer is no reason to hide a page.
        let answered_topics = (0..candidates.len().min(MAX_TOPIC_QUESTIONS))
            .all(|index| answers.contains_key(&format!("{TOPIC_PREFIX}{index}")));
        if answered_topics && !candidates.is_empty() {
            plan.topics = Some(topics);
        }
        for (index, call) in bulky.iter().enumerate() {
            let Some(p) = answers
                .get(&format!("{KEEP_PREFIX}{index}"))
                .and_then(|answer| answer.probabilities.get("true"))
            else {
                continue;
            };
            if *p >= VERBATIM_THRESHOLD {
                plan.keep.insert(*call, Keep::Verbatim);
            } else if *p < DROP_THRESHOLD {
                plan.keep.insert(*call, Keep::SignalOnly);
            }
        }
        plan
    }
}

/// One classifier call's state and questions, and the calls the `keep_`
/// questions are about, in question order.
#[must_use]
pub fn questions(
    session: &Session,
    signals: &Signals,
    candidates: &[(String, String)],
) -> (String, BTreeMap<String, Question>, Vec<CallRef>) {
    let state = digest(session, signals);
    let mut questions = BTreeMap::new();
    questions.insert(
        TRIAGE_ID.to_owned(),
        Question::Choice {
            instructions: "The state is a digest of one coding-agent session (a person working \
                           with an AI coding agent). It is about to be compiled into the \
                           person's engineering wiki of decisions, learnings and architecture. \
                           How much of a compile does this session deserve?"
                .to_owned(),
            criteria: BTreeMap::from([
                (
                    "skip".to_owned(),
                    "Nothing worth recording: empty, aborted, a greeting or a trivial \
                     exchange with no decision, finding or change."
                        .to_owned(),
                ),
                (
                    "summary".to_owned(),
                    "Worth one line in the log but not a page edit: the agent only looked \
                     things up, or retried a failing step without resolving it, and no \
                     decision or lesson came out of it."
                        .to_owned(),
                ),
                (
                    "full".to_owned(),
                    "Real work: files changed, a decision was made, a bug was understood, \
                     or something was learned that the wiki should keep."
                        .to_owned(),
                ),
            ]),
        },
    );
    for (index, (slug, title)) in candidates.iter().take(MAX_TOPIC_QUESTIONS).enumerate() {
        questions.insert(
            format!("{TOPIC_PREFIX}{index}"),
            Question::Noul {
                instructions: format!(
                    "This session adds to, changes or contradicts what the existing wiki page \
                     `{slug}` (titled \"{}\") is about.",
                    cap(title, 120)
                ),
            },
        );
    }
    let mut bulky = Vec::new();
    'turns: for (t, turn) in session.turns.iter().enumerate() {
        for (c, call) in turn.calls.iter().enumerate() {
            if bulky.len() >= MAX_KEEP_QUESTIONS {
                break 'turns;
            }
            if call
                .output
                .as_deref()
                .is_some_and(|output| output.chars().count() >= BULKY_OUTPUT_CHARS)
            {
                questions.insert(
                    format!("{KEEP_PREFIX}{}", bulky.len()),
                    Question::Noul {
                        instructions: format!(
                            "The full output of call #{} ({}{}) must be read word for word to \
                             record what this session decided, changed or learned; its error \
                             lines alone would not do.",
                            call_number(session, t, c),
                            call.name,
                            call.path
                                .as_deref()
                                .map(|path| format!(" on {path}"))
                                .unwrap_or_default()
                        ),
                    },
                );
                bulky.push((t, c));
            }
        }
    }
    (state, questions, bulky)
}

/// The 1-based number a call has in the digest.
fn call_number(session: &Session, turn: usize, call: usize) -> usize {
    session.turns[..turn]
        .iter()
        .map(|turn| turn.calls.len())
        .sum::<usize>()
        + call
        + 1
}

/// What the classifier reads: the counts, then every turn short — the
/// person's words, the agent's, and each call as one line (number, tool,
/// file, outcome, the command's head). Redacted, and at most
/// [`MAX_STATE_CHARS`].
#[must_use]
pub fn digest(session: &Session, signals: &Signals) -> String {
    let mut out = format!(
        "agent: {}\nturns: {} from the person, {} from the agent\ntool calls: {} ({} failed)\nfiles changed: {}\n",
        session.agent,
        signals.user_turns,
        signals.assistant_turns,
        signals.tool_calls,
        signals.failed_calls,
        if signals.files_changed.is_empty() {
            "none".to_owned()
        } else {
            signals.files_changed.join(", ")
        },
    );
    if let Some(call) = &signals.worst_retry_call
        && signals.worst_retry > 1
    {
        out.push_str(&format!(
            "most repeated failure: {} ×{}\n",
            cap(call, 120),
            signals.worst_retry
        ));
    }
    out.push('\n');
    let mut number = 0;
    for turn in &session.turns {
        let limit = match turn.role.as_str() {
            "user" => 600,
            "assistant" => 400,
            _ => 160,
        };
        let text = turn.text.split_whitespace().collect::<Vec<_>>().join(" ");
        if !text.is_empty() {
            out.push_str(&format!("[{}] {}\n", turn.role, cap(&text, limit)));
        }
        for call in &turn.calls {
            number += 1;
            let mut line = format!("  #{number} {}", call.name);
            if let Some(agent) = &call.subagent {
                let nested = call.with_nested();
                let failed = nested.iter().skip(1).filter(|c| c.failed()).count();
                let changed = nested
                    .iter()
                    .skip(1)
                    .filter(|c| changes_something(c))
                    .count();
                line.push_str(&format!(
                    " (subagent {}: {} calls, {failed} failed, {changed} changes{})",
                    cap(&agent.name, 40),
                    nested.len() - 1,
                    agent
                        .status
                        .as_deref()
                        .map(|status| format!(", {status}"))
                        .unwrap_or_default()
                ));
            }
            if let Some(path) = &call.path {
                line.push_str(&format!(" {path}"));
            }
            if call.failed() {
                line.push_str(" FAILED");
                if let Some(code) = call.exit_code {
                    line.push_str(&format!(" (exit {code})"));
                }
            }
            if let Some(input) = &call.input {
                let input = input.split_whitespace().collect::<Vec<_>>().join(" ");
                line.push_str(&format!(": {}", cap(&input, 120)));
            }
            out.push_str(&line);
            out.push('\n');
        }
        if out.chars().count() > MAX_STATE_CHARS {
            break;
        }
    }
    let out = cap(&out, MAX_STATE_CHARS);
    redact(&out, Policy::Redact).map_or_else(|_| String::new(), |(clean, _)| clean)
}

/// The model-free log text of a `summary-only` session: what it did, in
/// counts, and why it was not compiled.
#[must_use]
pub fn summary_text(signals: &Signals, reason: &str) -> String {
    let mut parts = vec![format!(
        "summary only ({reason}): {} message{} from you, {} tool call{}",
        signals.user_turns,
        if signals.user_turns == 1 { "" } else { "s" },
        signals.tool_calls,
        if signals.tool_calls == 1 { "" } else { "s" },
    )];
    if signals.failed_calls > 0 {
        parts.push(format!("{} failed", signals.failed_calls));
    }
    if let Some(call) = &signals.worst_retry_call
        && signals.worst_retry > 1
    {
        parts.push(format!(
            "`{}` failed {}×",
            cap(call, 60).replace('`', "'"),
            signals.worst_retry
        ));
    }
    parts.push(if signals.files_changed.is_empty() {
        "no files changed".to_owned()
    } else {
        format!("{} files changed", signals.files_changed.len())
    });
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strictness_reads_names_and_numbers() {
        assert_eq!(Strictness::parse("strict"), Some(Strictness::Strict));
        assert_eq!(Strictness::parse(" Lenient "), Some(Strictness::Lenient));
        assert_eq!(Strictness::parse("1"), Some(Strictness::Balanced));
        assert_eq!(Strictness::parse("max"), None);
        assert_eq!(Strictness::from_index(9), Strictness::Balanced);
    }

    #[test]
    fn a_change_is_always_full() {
        let signals = Signals {
            changes: 1,
            failed_calls: 9,
            tool_calls: 10,
            worst_retry: 9,
            ..Signals::default()
        };
        for strictness in Strictness::ALL {
            assert_eq!(rule(&signals, strictness).triage, Triage::Full);
        }
    }
}
