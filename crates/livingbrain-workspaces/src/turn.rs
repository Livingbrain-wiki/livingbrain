//! The speak-up policy (issue #9): how eagerly a turn may be started, and
//! what triage cost. `livingbrain-judge` had [`Proactivity`] — one number,
//! the triage threshold — but nowhere for a workspace to choose one, and
//! nothing recording what a triaged turn decided or spent. This module is
//! both: `turn_policies` stores the ask (a channel row over an org-wide row
//! whose channel id is empty), and `turn_triage` records, textless, what
//! every message that reached the classifier decided and cost.

use std::collections::BTreeMap;

use cratefield_core::{Answer, Classifier, Clock, Database, DbError, IdGen, Statement, Tokens};
use livingbrain_channel::Message;
use livingbrain_judge::{Decided, Proactivity, TRIAGE_QUESTION, triage_questions};
use sea_query::Value;

use crate::agent;

/// The share of the reply threshold at which a declined message still gets
/// a reaction: the classifier nearly said yes, but the answer is not worth
/// the thread. Below it the turn is silent — where most chatter lands.
const REACT_BAND: f32 = 0.75;

/// The reaction a near-miss gets. Slack names emoji by slug.
pub(crate) const REACT_EMOJI: &str = "eyes";

/// What a triage says the turn should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Speak {
    /// Answer in the thread.
    Reply,
    /// Acknowledge with a reaction and compute no answer.
    React,
    /// Nothing at all.
    Silent,
}

impl Speak {
    /// The ledger's name for the decision.
    fn as_str(self) -> &'static str {
        match self {
            Self::Reply => "reply",
            Self::React => "react",
            Self::Silent => "silent",
        }
    }
}

/// The decision a triage's numbers turn into: `outcome` replies, a score
/// still inside [`REACT_BAND`] of the threshold reacts, silence below. The
/// band reads off the judgement's own threshold, so it moves with the
/// classifier family and the proactivity both.
pub(crate) fn speak(decided: &Decided) -> Speak {
    if decided.outcome {
        return Speak::Reply;
    }
    if decided.judgement.score >= decided.judgement.threshold * REACT_BAND {
        Speak::React
    } else {
        Speak::Silent
    }
}

/// The proactivity in force in one channel: the channel's own row when it
/// has spoken, the org-wide row when only the workspace has, and
/// [`Proactivity::Off`] when neither has — which is what keeps an
/// unaddressed message from reaching the classifier at all. A read that
/// fails is `Off` too: a policy must fail quiet, not loud.
pub(crate) async fn proactivity_for(
    db: &dyn Database,
    workspace_id: &str,
    channel_id: &str,
) -> Proactivity {
    // The channel id sorts after the empty org row, so the first result is
    // the most specific policy there is.
    let statement = Statement::with_values(
        "SELECT proactivity FROM turn_policies \
         WHERE workspace_id = ? AND channel_id IN (?, '') \
         ORDER BY channel_id DESC",
        vec![text(workspace_id), text(channel_id)],
    );
    let Ok(found) = db.query(&statement).await else {
        return Proactivity::Off;
    };
    found
        .first()
        .and_then(|row| row.get::<String>("proactivity"))
        .map(|stored| proactivity_of(&stored))
        .unwrap_or(Proactivity::Off)
}

/// Sets the policy for one channel, or for the whole workspace when
/// `channel` is `None` (the org row's empty channel id). An existing row is
/// updated in place, never duplicated.
///
/// # Errors
///
/// A [`DbError`] when the write fails.
pub async fn set_proactivity(
    db: &dyn Database,
    clock: &dyn Clock,
    workspace_id: &str,
    channel: Option<&str>,
    proactivity: Proactivity,
) -> Result<(), DbError> {
    let statement = Statement::with_values(
        "INSERT INTO turn_policies (workspace_id, channel_id, proactivity, updated_at) \
         VALUES (?, ?, ?, ?) \
         ON CONFLICT (workspace_id, channel_id) DO UPDATE \
         SET proactivity = excluded.proactivity, updated_at = excluded.updated_at",
        vec![
            text(workspace_id),
            text(channel.unwrap_or_default()),
            text(&name(proactivity)),
            text(&now(clock)),
        ],
    );
    db.execute(&statement).await.map(|_| ())
}

/// The serde name a policy row stores — the judge's own spelling.
fn name(proactivity: Proactivity) -> String {
    serde_json::to_value(proactivity)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// The proactivity a stored name means; a name this judge does not know is
/// [`Proactivity::Off`], for the same reason it fails to deserialize there.
fn proactivity_of(stored: &str) -> Proactivity {
    serde_json::from_value(serde_json::Value::String(stored.to_owned())).unwrap_or(Proactivity::Off)
}

/// The estimated cost of one triage call: cratefield's own
/// [`Tokens::estimate`] over the state the classifier read, the question
/// set and, when it answered, the answer rebuilt from the judgement's own
/// probabilities. The port reports no usage, so an estimate is what there
/// is, and the row says so.
pub(crate) fn estimated_tokens(
    classifier: &dyn Classifier,
    state: &str,
    decided: Option<&Decided>,
) -> Tokens {
    let questions = triage_questions();
    let answers = decided.map(|decided| {
        BTreeMap::from([(
            TRIAGE_QUESTION.to_owned(),
            Answer::noul(
                decided.outcome,
                decided
                    .judgement
                    .probabilities
                    .get(TRIAGE_QUESTION)
                    .cloned()
                    .unwrap_or_default(),
            ),
        )])
    });
    Tokens::estimate(&classifier.profile(), state, &questions, answers.as_ref())
}

/// One triage, as the ledger receives it.
pub(crate) struct Triage<'a> {
    pub workspace_id: &'a str,
    pub message: &'a Message,
    /// Whether the message named the brain or arrived in a DM.
    pub addressed: bool,
    /// The policy in force for the turn (the default on an addressed one).
    pub proactivity: Proactivity,
    pub speak: Speak,
    /// The classifier profile's family, by name.
    pub calibration: &'a str,
    pub tokens: Tokens,
}

/// Writes one triage row — one per message that reached the classifier,
/// whatever came back, because a silent decision is spent tokens too. Best
/// effort on purpose: the turn is already decided, and a write that does
/// not land is one diagnostic line and nothing more.
pub(crate) async fn record(
    db: &dyn Database,
    id_gen: &dyn IdGen,
    clock: &dyn Clock,
    triage: Triage<'_>,
) {
    // The conversation key and the platform's message id are the only pointers.
    let statement = Statement::with_values(
        "INSERT INTO turn_triage \
           (id, workspace_id, conversation_key, message_id, addressed, proactivity, \
            decision, calibration, input_tokens, output_tokens, token_source, recorded_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'estimated', ?)",
        vec![
            text(&id_gen.ulid()),
            text(triage.workspace_id),
            text(&triage.message.conversation_key()),
            text(&triage.message.id),
            flag(triage.addressed),
            text(&name(triage.proactivity)),
            text(triage.speak.as_str()),
            text(triage.calibration),
            int(triage.tokens.input),
            int(triage.tokens.output),
            text(&now(clock)),
        ],
    );
    if let Err(err) = db.execute(&statement).await {
        agent::report(&format!("agent: could not record the triage: {err:?}"));
    }
}

/// A non-null text bind — the same shape `store` binds with.
fn text(value: &str) -> Value {
    Value::String(Some(Box::new(value.to_owned())))
}

/// A boolean as the portable 0/1 INTEGER flag (ADR 0004).
fn flag(value: bool) -> Value {
    Value::BigInt(Some(i64::from(value)))
}

/// A token count as an INTEGER bind, saturating rather than failing.
fn int(value: u64) -> Value {
    Value::BigInt(Some(i64::try_from(value).unwrap_or(i64::MAX)))
}

/// The RFC 3339 instant the rows are written with, from the [`Clock`] port.
fn now(clock: &dyn Clock) -> String {
    clock
        .now()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}
