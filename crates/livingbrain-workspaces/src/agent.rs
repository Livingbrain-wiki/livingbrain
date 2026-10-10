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
//! ## The turn
//!
//! Every gate stands **before** the coordinator (issue #7), so what it is
//! ever handed is a message that will be answered: [`Turns`] admits it to
//! its conversation's turn — keyed by [`Message::conversation_key`] — or
//! queues it behind the turn already running; the answer posts exactly as a
//! lone message's does; and the finish hands back whatever arrived mid-turn,
//! coalesced, as the next turn. Errors are swallowed the way
//! [`crate::events`] swallows them — nothing awaits this future and Slack
//! has already been told `200` — with a diagnostic line through [`report`].

use std::sync::Arc;

use cratefield_core::{Database, HttpClient};
use livingbrain_access::{ChannelId, Location as AccessLocation};
use livingbrain_channel::slack::{Slack, SlackEvent};
use livingbrain_channel::{Channel, Location, Message, Visibility};
use livingbrain_judge::{Judge, JudgeSettings};
use livingbrain_pages::{Answers, Asker};
use livingbrain_redact::{Policy, redact};
use serde_json::{Value, json};

use crate::conversation::{Admit, Exchange, Pending, Room, Turn};
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
    // The `HttpClient` is only held to check, here, that the deployment has
    // one; the turn's answer re-reads both ports per turn, because a
    // coalesced turn runs long after the event that started it.
    let (Some(db), Some(_)) = (state.ctx.ports.db.clone(), state.ctx.ports.http.clone()) else {
        return;
    };
    let db: &dyn Database = &*db;
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
    // The turn (issue #7): every gate has passed, so this message is going
    // to be answered — the only question the coordinator settles is whether
    // it answers now, or joins the turn already running.
    let Some(clock) = state.ctx.ports.clock.clone() else {
        return;
    };
    let key = message.conversation_key();
    let mut turn = match state
        .turns
        .arrive(
            &key,
            pending_for(&message, &user_id, &slack.bot_user_id),
            clock.now().unix_timestamp(),
        )
        .await
    {
        Ok(Admit::Run(turn)) => turn,
        // The running turn folds this message in when it finishes: nothing
        // is said now, and nothing is dropped.
        Ok(Admit::Queued) => return,
        // A coordinator that cannot be reached must not silence the bot: the
        // gates all passed. Answering unserialised is the pre-#7 behaviour,
        // the honest degradation; the lease id below is one no conversation
        // ever mints (ids start at one), so the finish releases nothing.
        Err(err) => {
            report(&format!(
                "agent: turn coordinator unavailable, answering unserialised: {err:?}"
            ));
            Turn {
                lease: 0,
                messages: vec![pending_for(&message, &user_id, &slack.bot_user_id)],
                context: Vec::new(),
            }
        }
    };
    loop {
        let exchange = answer_turn(state, answers, team_id, &workspace_id, &slack, &turn).await;
        match state
            .turns
            .finish(&key, turn.lease, exchange, clock.now().unix_timestamp())
            .await
        {
            // Whatever arrived mid-turn is the next turn, already coalesced.
            Ok(Some(next)) => turn = next,
            Ok(None) => break,
            // The finish is what releases the floor; when even it fails,
            // stopping is all there is — the lease's TTL hands the thread
            // to the next arrival.
            Err(err) => {
                report(&format!("agent: could not finish the turn: {err:?}"));
                break;
            }
        }
    }
}

/// What the coordinator is handed for one gated message: enough to answer
/// it long after the event that carried it is gone, and enough to reply to
/// it (see [`Pending::reply_target`]). The question is computed once, here,
/// because the judge and the answer must read the same words.
fn pending_for(message: &Message, user_id: &str, bot_user_id: &str) -> Pending {
    Pending {
        user: user_id.to_owned(),
        slack_user: message.author.user.clone(),
        channel: message.location.channel.clone(),
        room: Room::of(message.location.visibility),
        thread: message.thread.clone().unwrap_or_else(|| message.id.clone()),
        ts: message.id.clone(),
        question: question(&message.text, bot_user_id),
    }
}

/// Answers one admitted turn, and reports what the conversation should
/// remember: `Some` only when the reply actually posted — a turn that asked
/// nothing, or whose post never landed, is `None`, so the context never
/// remembers an answer the thread never saw. The turn is one ask, because
/// the coordinator coalesced one member's messages: the questions joined in
/// arrival order, answered under that member's own scope, posted once, to
/// the thread they all share.
async fn answer_turn(
    state: &ModuleState,
    answers: &dyn Answers,
    team_id: &str,
    workspace_id: &str,
    slack: &Slack,
    turn: &Turn,
) -> Option<Exchange> {
    // The same ports `consider` read, re-read here: the turn may outlive the
    // clones the caller held (a coalesced next turn runs long after the
    // event that started it).
    let (Some(db), Some(http)) = (state.ctx.ports.db.clone(), state.ctx.ports.http.clone()) else {
        return None;
    };
    let db: &dyn Database = &*db;
    let http: &dyn HttpClient = &*http;
    let lead = &turn.messages[0];
    let asked = turn
        .messages
        .iter()
        .map(|pending| pending.question.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    // The reply target is every message's: one conversation is one thread.
    let to = lead.reply_target(team_id);
    let answered = match answers
        .answer(
            &Asker::new(workspace_id, &lead.user).at(access_location(&to.location)),
            &asked,
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
            return None;
        }
    };
    let token = bot_token(db, &state.kms, team_id).await?;
    let out = slack.reply(
        &to,
        &livingbrain_channel::Reply {
            text: answered.text.clone(),
            citations: answered.citations.iter().map(citation).collect(),
        },
    );
    // The refusal carries Slack's own snake_case code and never the token or
    // the message (see `SlackError::refused`), so it is safe to say.
    if let Err(err) = reply::post(http, &token, &out).await {
        report(&format!("agent: could not post the answer: {err:?}"));
        return None;
    }
    Some(Exchange {
        question: asked,
        answer: answered.text,
    })
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
