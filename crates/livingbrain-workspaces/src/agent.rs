//! The agent loop (issue #123): somebody says the brain's name, and the brain
//! answers from that person's own pages, in the thread they asked in.
//!
//! Everything it does is a gate, and each one fails **closed** — no message,
//! no answer — because every input here arrived from somebody else's chat
//! client:
//!
//! 1. the signed envelope's team resolves to a workspace this deployment
//!    serves;
//! 2. the event parses as a Slack message the app did not write;
//! 3. it either names the app or arrived in a DM;
//! 4. the author resolves to a **member** of the workspace — not merely
//!    somebody with a Slack id, which everybody in the team has;
//! 5. the fast judge (issue #112) says this is worth answering;
//! 6. the answer reads only what the access model (issue #106) grants that
//!    member at that location.
//!
//! Gates 4 and 5 are in that order on purpose: a Slack user id is not an
//! account, so a member is resolved **before** any of this member's words
//! reach a provider. Everybody in a team has a Slack id, and a judge costs a
//! classifier call — a gate after the classifier is a gate nobody had to
//! pass.
//!
//! ## The judge gate
//!
//! The policy, in one sentence: **a DM is triaged; an explicit @mention is
//! answered only when there is no classifier to silence it with.**
//!
//! A human typing the app's name in a channel is a request, and refusing it
//! because a probability fell below a threshold is the kind of cleverness
//! that gets an app uninstalled. A DM is the opposite: a message in a stream
//! nobody is watching, where a reply to "on it" is noise. So the gate goes
//! wherever it can — both paths with a classifier, the DM alone without one —
//! and what it refuses, it refuses silently. `outcome == false` says nothing
//! now: this loop keeps no memory and calls no memory gate, so a message it
//! turns away is not kept anywhere either.
//!
//! **No structured event is written here, deliberately.** `livingbrain_judge`
//! names its event and `Judgement` is what it would carry, but no
//! `livingbrain-*` module emits a structured event today; inventing a log
//! path for one judgement is a bigger change than this issue, and a judgement
//! that reaches no sink is not worth a new one. Treat this as a known gap.
//!
//! Errors are swallowed the way [`crate::events`] swallows them: nothing
//! awaits this future, Slack has already been told `200`, and the inbox claim
//! is what keeps the retry from doing it twice. A *diagnostic* line still
//! goes out through [`report`] — swallowing and being unobservable are
//! different things.

use std::sync::Arc;

use cratefield_core::{Database, HttpClient};
use livingbrain_access::{ChannelId, Location as AccessLocation};
use livingbrain_channel::slack::{Slack, SlackEvent};
use livingbrain_channel::{Channel, Location, Message, Visibility};
use livingbrain_judge::{Judge, JudgeSettings};
use livingbrain_pages::{Answered, Answers, Asker};
use livingbrain_redact::{Policy, redact};
use serde_json::{Value, json};

use crate::handlers::ModuleState;
use crate::reply;
use crate::store;

/// Answers one verified Slack event, or decides not to.
///
/// Everything comes out of [`ModuleState`], so the ports are read apart from
/// the gates: a missing `Database` or `HttpClient` is a deployment the
/// harness would already have refused, and `answers` being absent is a
/// composition that wired no agent at all — a page store is a tenant-wide
/// capability and this module has no business holding one.
pub(crate) async fn consider(state: &ModuleState, team_id: &str, event: &Value) {
    let (Some(db), Some(http)) = (state.ctx.ports.db.clone(), state.ctx.ports.http.clone()) else {
        return;
    };
    let db: &dyn Database = &*db;
    let http: &dyn HttpClient = &*http;
    let Some(workspace_id) = workspace(db, team_id).await else {
        return;
    };
    let Some(slack) = slack(db, team_id).await else {
        return;
    };
    // The team is the one the *signed envelope* named (ADR 0001), never one
    // the payload chose, so it is put back onto the event here rather than
    // read out of it.
    let parsed = json!({"team_id": team_id, "event": event});
    let Ok(envelope) = serde_json::from_value::<SlackEvent>(parsed) else {
        return;
    };
    let Some(message) = slack.receive(&envelope) else {
        return;
    };
    if !addresses_us(&message) {
        return;
    }
    // Before the judge: this is the gate that decides whose words may leave
    // the machine at all, and a stranger's must not reach a provider to find
    // out.
    let Some(user_id) = member(db, &workspace_id, &message).await else {
        return;
    };
    if !worth_answering(state, &message, &slack.bot_user_id).await {
        return;
    }
    let Some(answers) = state.answers.as_deref() else {
        return;
    };
    let answered = match ask(
        answers,
        &workspace_id,
        &user_id,
        &message,
        &slack.bot_user_id,
    )
    .await
    {
        Ok(answered) => answered,
        Err(err) => {
            // `Unavailable` is a deployment that wired an agent with nothing
            // behind it — no key custodian, so no page store was ever built
            // (issue #123). Without this line such a bot is mute for ever and
            // says nothing about why.
            report(&format!("agent: nothing to answer from: {err:?}"));
            return;
        }
    };
    let Some(token) = bot_token(db, &state.kms, team_id).await else {
        return;
    };
    let out = slack.reply(
        &message,
        &livingbrain_channel::Reply {
            text: answered.text,
            citations: answered.citations.iter().map(citation).collect(),
        },
    );
    // The refusal carries Slack's own snake_case code and never the token or
    // the message (see `SlackError::refused`), so it is safe to say.
    if let Err(err) = reply::post(http, &token, &out).await {
        report(&format!("agent: could not post the answer: {err:?}"));
    }
}

/// One diagnostic line about a turn that produced no answer.
///
/// Nothing awaits this future and Slack has already been told `200`, so an
/// error here is never returned anywhere — but a loop that swallows one is a
/// loop that can be mute for ever for a reason nobody can see. This is the
/// harness's own one-line sink, which the Cloudflare runtime points at
/// `console_error!`: `tracing` is dropped on `wasm32`, where this loop
/// actually runs. The line passes the harness's own text scrubber, and none
/// of these carry message text in any case.
fn report(line: &str) {
    cratefield_core::forward_control_event(cratefield_core::ControlLevel::Warn, line);
}

/// `Citation` from a page citation — the channel's own type, because the
/// adapter is what renders one and this module does not render anything.
fn citation(source: &livingbrain_pages::Citation) -> livingbrain_channel::Citation {
    livingbrain_channel::Citation {
        title: source.title.clone(),
        url: source.url.clone(),
    }
}

/// The workspace this Slack team is linked to, or `None` for a team this
/// deployment does not serve.
async fn workspace(db: &dyn Database, team_id: &str) -> Option<String> {
    store::workspace_for_platform(db, "slack", team_id)
        .await
        .ok()
        .flatten()
        .filter(|id| !id.is_empty())
}

/// The Slack adapter for this team's installation, or `None` when the team
/// never installed the app.
async fn slack(db: &dyn Database, team_id: &str) -> Option<Slack> {
    Some(Slack {
        bot_user_id: store::bot_user_id(db, team_id).await.ok().flatten()?,
    })
}

/// Whether this message is one the loop owes an answer to.
///
/// A plain message in a channel is neither. `Channel::receive` cannot tell
/// them apart for us — it normalises a message, it does not judge it — so
/// this is where "do not interrupt a channel" is decided.
fn addresses_us(message: &Message) -> bool {
    message.mentions_bot || message.location.visibility == Visibility::Direct
}

/// The judge gate; see the module docs for the policy.
///
/// With no classifier a mention never asked and is answered, and a DM falls
/// silent because there is nothing to triage it with.
async fn worth_answering(state: &ModuleState, message: &Message, bot_user_id: &str) -> bool {
    let Some(classifier) = state.ctx.ports.classifier.clone() else {
        return message.mentions_bot;
    };
    // The app's own name comes off **before** redaction, so what the judge
    // reads is the question a person typed rather than a bot's user id —
    // and redaction runs **before** the classifier does, because this is the
    // first time any of these words leave the machine. `PageAnswers` makes
    // the same call between a question and the page index; here it stands
    // between a question and a third party, and a member who DMs
    // `what's the launch status? sk_live_…` has the token judged as
    // `<redacted>` rather than forwarded on the strength of "only a triage
    // call".
    let question = question(&message.text, bot_user_id);
    let Ok((judged, _)) = redact(&question, Policy::Redact) else {
        return false;
    };
    // The judgement is dropped: see the module docs on telemetry.
    Judge::new(classifier, JudgeSettings::default())
        .triage(&judged)
        .await
        .is_ok_and(|decided| decided.outcome)
}

/// The message text with the app's own name taken off it.
///
/// Search matches every word of the query, so leaving `<@U…>` in would ask the
/// pages about the bot's user id — a term no page has — and return "nothing"
/// to every question ever asked. Slack addresses people the same way in a
/// mention as in ordinary text, so what is left is the question. Both the
/// judge and the answer read this rather than the raw text, so the two can
/// never disagree about what was asked.
fn question(text: &str, bot_user_id: &str) -> String {
    text.replace(&format!("<@{bot_user_id}>"), " ")
        .trim()
        .to_owned()
}

/// The member the message's author resolves to, or `None`.
///
/// Failing closed here is the point: a Slack user id is not an account.
/// Everybody in the team has one; only a member has an identity this
/// deployment will answer as.
async fn member(db: &dyn Database, workspace_id: &str, message: &Message) -> Option<String> {
    store::member_for_identity(db, workspace_id, "slack", &message.author.user)
        .await
        .ok()
        .flatten()
        .map(|member| member.user_id)
        .filter(|id| !id.is_empty())
}

/// The answer, from that member's own pages at the place they asked.
///
/// The question is the same [`question`] the judge read, so a message the
/// judge saw as `what is the refund window?` is searched for under exactly
/// those words.
async fn ask(
    answers: &dyn Answers,
    workspace_id: &str,
    user_id: &str,
    message: &Message,
    bot_user_id: &str,
) -> Result<Answered, livingbrain_pages::AnswerError> {
    answers
        .answer(
            &Asker::new(workspace_id, user_id).at(access_location(&message.location)),
            &question(&message.text, bot_user_id),
        )
        .await
}

/// The bot token this team's answer is sent with, or `None` when there is
/// no key custodian or no install row.
async fn bot_token(
    db: &dyn Database,
    kms: &Option<Arc<dyn cratefield_kms::Kms>>,
    team_id: &str,
) -> Option<String> {
    crate::install::bot_token(kms.as_ref()?.as_ref(), db, team_id)
        .await
        .ok()
        .flatten()
}

/// The access model's location for a channel's visibility.
///
/// **Fail-closed by construction.** The adapter classifies visibility and
/// anything it cannot read is [`Visibility::Private`], so a stranger cannot
/// reach `PublicChannel` by naming one. A private channel grants nothing to a
/// non-member, and there is no membership view here that says anybody is one
/// — the roster producer behind [`Channel::viewers`] does not exist yet, and
/// an empty [`ChannelMemberships`] is what "not yet" looks like from this
/// side. Every question therefore reads the shared scope, plus the asker's
/// own in a DM.
fn access_location(location: &Location) -> AccessLocation {
    match location.visibility {
        Visibility::Public => AccessLocation::PublicChannel,
        Visibility::Private => {
            AccessLocation::PrivateChannel(ChannelId::new(location.channel.clone()))
        }
        Visibility::Direct => AccessLocation::Dm,
    }
}
