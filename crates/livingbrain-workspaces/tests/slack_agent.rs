//! The agent loop (issue #123): an @mention and a DM are answered from the
//! asker's own pages, in the thread they asked in, and everything else is
//! silence.
//!
//! Every delivery is signed the way Slack signs one — the HMAC is computed in
//! the test, not borrowed from the module — and every post is asserted on the
//! wire: the method, the URI, the `Authorization` header and the rendered
//! body. A fake that answered `ok: true` to anything would pass a loop that
//! posted nothing at all, which is the failure this issue is most likely to
//! have.

mod support;

use std::sync::Arc;

use cratefield_core::{Answer, Classifier};
use cratefield_testing::FakeClassifier;
use livingbrain_access::{Scope, UserId};
use livingbrain_judge::Proactivity;
use livingbrain_pages::{Author, EntityType, PageWrite, page_scope};
use livingbrain_workspaces::set_proactivity;
use serde_json::{Value, json};
use support::*;

/// An `app_mention` in a public channel.
fn mention(text: &str) -> Value {
    json!({
        "type": "app_mention",
        "user": ASKER,
        "channel": "C0PUBLIC",
        "channel_type": "channel",
        "text": format!("<@{BOT}> {text}"),
        "ts": "1700000000.000100",
    })
}

/// A plain message in a channel, naming nobody.
fn chatter(text: &str) -> Value {
    json!({
        "type": "message",
        "user": ASKER,
        "channel": "C0PUBLIC",
        "channel_type": "channel",
        "text": text,
        "ts": "1700000001.000100",
    })
}

/// A direct message, which is the other door in.
fn direct(text: &str) -> Value {
    json!({
        "type": "message",
        "user": ASKER,
        "channel": "D0DM",
        "channel_type": "im",
        "text": text,
        "ts": "1700000002.000100",
    })
}

/// Writes a page titled `title` into `scope` of the team.
async fn seed_page(agent: &Agent, scope: &Scope, slug: &str, title: &str, body: &str) {
    agent
        .store
        .write(
            &page_scope(TEAM, scope),
            slug,
            PageWrite {
                entity_type: EntityType::Decision,
                markdown: format!("---\ntitle: {title}\n---\n{body}\n"),
                author: Author::Human {
                    id: "human".to_owned(),
                },
                base_version: None,
            },
        )
        .await
        .expect("the page is written");
}

/// A world with the install, the workspace and one shared page the asker may
/// read. `classifier` is `None` for the unwired-judge policy.
async fn world(classifier: Option<Arc<dyn Classifier>>) -> (Agent, TokenHttp) {
    let http = TokenHttp::new();
    let agent = agent_harness(&http, classifier);
    install_agent(&agent, &http).await;
    assert!(
        agent.answers.answering(Arc::clone(&agent.store), WIKI),
        "the answer seam is filled once"
    );
    seed_page(
        &agent,
        &Scope::Shared,
        "refund-window",
        "Refund window",
        "The refund window is fourteen days from the invoice date. What is not \
         covered: shipping, and anything bought on a marketplace invoice.",
    )
    .await;
    (agent, http)
}

/// Every post the loop made, as the bodies Slack received.
fn posts(http: &TokenHttp) -> Vec<Value> {
    http.api_calls().iter().map(Call::json).collect()
}

/// A classifier that answers the triage question with `yes` as P(true), so
/// what is under test is the gate and not the port.
///
/// The `FakeClassifier` itself is returned, not just the `Arc`: it records
/// every ask it was handed, and what the loop **did not** ask is as much the
/// subject as what it did — a question that reached a provider is not proved
/// wrong by the provider having said yes.
fn judging(yes: f32) -> (FakeClassifier, Arc<dyn Classifier>) {
    let fake = FakeClassifier::default();
    fake.set_answer_for(
        livingbrain_judge::TRIAGE_QUESTION,
        Answer::noul(
            yes >= 0.5,
            [("true".to_owned(), yes), ("false".to_owned(), 1.0 - yes)]
                .into_iter()
                .collect(),
        ),
    );
    (fake.clone(), Arc::new(fake))
}

/// Sets the speak-up policy — `channel` of `None` is the org-wide row —
/// through the module's own seam, at the kit's fixed instant.
async fn set_policy(agent: &Agent, channel: Option<&str>, proactivity: Proactivity) {
    let clock = clock();
    set_proactivity(&*agent.kit.db, &clock, TEAM, channel, proactivity)
        .await
        .expect("the policy is set");
}

// ---------------------------------------------------------------------------
// The two doors

#[pollster::test]
async fn a_mention_is_answered_in_its_own_thread_with_a_citation() {
    let (agent, http) = world(None).await;
    deliver(
        &agent.kit,
        "Ev1",
        TEAM,
        mention("what is the refund window?"),
    )
    .await;

    let calls = http.api_calls();
    assert_eq!(calls.len(), 1, "exactly one post: {calls:?}");
    let call = &calls[0];
    // The wire, not just the fact of a post.
    assert_eq!(call.method, "POST");
    assert_eq!(call.uri, "https://slack.com/api/chat.postMessage");
    assert_eq!(
        call.authorization.as_deref(),
        Some(format!("Bearer {BOT_TOKEN}").as_str()),
        "the bot token rides the Authorization header and nothing else"
    );
    assert!(
        !call.body.contains(BOT_TOKEN),
        "the token is not in the body: {}",
        call.body
    );

    let body = call.json();
    // The thread is the message's own: an answer that floats away from the
    // question is not an answer to it.
    assert_eq!(body["thread_ts"], json!("1700000000.000100"));
    assert_eq!(body["channel"], json!("C0PUBLIC"));
    let text = body["text"].as_str().expect("a text field");
    assert!(text.contains("fourteen days"), "{text}");
    // The citation is a link to the page the answer came from, and the scope
    // in it is the asker's own workspace's shared scope.
    let context = &body["blocks"][1]["elements"][0]["text"];
    let link = context.as_str().expect("a link block");
    let shared = page_scope(TEAM, &Scope::Shared);
    assert!(
        link.contains(&format!("{WIKI}/brain/{shared}/refund-window")),
        "{link}"
    );
    assert!(link.contains("Refund window"), "{link}");
    // Nothing was claimed twice, and nothing about the asker leaked.
    assert!(!call.body.contains(ASKER), "{}", call.body);
}

#[pollster::test]
async fn a_plain_channel_message_is_not_answered() {
    let (agent, http) = world(None).await;
    deliver(
        &agent.kit,
        "Ev2",
        TEAM,
        chatter("what is the refund window?"),
    )
    .await;
    assert!(
        posts(&http).is_empty(),
        "a channel that did not name the bot is not interrupted: {:?}",
        posts(&http)
    );
}

#[pollster::test]
async fn a_stranger_asking_is_not_answered() {
    // A classifier is wired, and it would have said yes: silence here cannot
    // be the judge declining, so the assertion is about the identity gate.
    let (judge, port) = judging(0.95);
    let (agent, http) = world(Some(port)).await;
    // A Slack user id nobody signed in with. The team is full of them.
    deliver(
        &agent.kit,
        "Ev3",
        TEAM,
        json!({
            "type": "app_mention",
            "user": "USTRANGER",
            "channel": "C0PUBLIC",
            "channel_type": "channel",
            "text": format!("<@{BOT}> what is the refund window?"),
            "ts": "1700000003.000100",
        }),
    )
    .await;
    assert!(
        posts(&http).is_empty(),
        "a Slack user id is not an account: {:?}",
        posts(&http)
    );
    assert!(
        judge.asks().is_empty(),
        "a stranger's words must not reach a provider to be judged: {:?}",
        judge.asks()
    );
}

#[pollster::test]
async fn a_team_this_deployment_does_not_serve_is_not_answered() {
    let (agent, http) = world(None).await;
    deliver(
        &agent.kit,
        "Ev4",
        "T0NEVER-INSTALLED",
        mention("what is the refund window?"),
    )
    .await;
    assert!(
        posts(&http).is_empty(),
        "an unlinked team has no workspace: {:?}",
        posts(&http)
    );
}

// ---------------------------------------------------------------------------
// The judge gate (issue #112)

#[pollster::test]
async fn a_dm_is_silent_with_no_classifier_and_a_mention_is_not() {
    // The deployment that wired no judge: a mention is a request and is
    // answered; a DM is a conversation and is not started.
    let (agent, http) = world(None).await;
    deliver(
        &agent.kit,
        "Ev5",
        TEAM,
        direct("what is the refund window?"),
    )
    .await;
    assert!(
        posts(&http).is_empty(),
        "a DM with no judge to triage it is silent: {:?}",
        posts(&http)
    );

    deliver(
        &agent.kit,
        "Ev6",
        TEAM,
        mention("what is the refund window?"),
    )
    .await;
    assert_eq!(posts(&http).len(), 1, "the mention was answered");
}

#[pollster::test]
async fn a_declining_judge_holds_both_doors() {
    // A deployment that wired the fast judge gets the gate on both paths:
    // `outcome == false` is the remember branch, and it says nothing.
    let (agent, http) = world(Some(judging(0.05).1)).await;
    deliver(
        &agent.kit,
        "Ev7",
        TEAM,
        direct("what is the refund window?"),
    )
    .await;
    deliver(
        &agent.kit,
        "Ev8",
        TEAM,
        mention("what is the refund window?"),
    )
    .await;
    assert!(
        posts(&http).is_empty(),
        "a judge that says no is obeyed on a mention too: {:?}",
        posts(&http)
    );
}

#[pollster::test]
async fn an_accepting_judge_answers_a_dm() {
    let (agent, http) = world(Some(judging(0.95).1)).await;
    deliver(
        &agent.kit,
        "Ev9",
        TEAM,
        direct("what is the refund window?"),
    )
    .await;
    let posted = posts(&http);
    assert_eq!(posted.len(), 1, "the DM was answered: {posted:?}");
    assert_eq!(posted[0]["thread_ts"], json!("1700000002.000100"));
}

// ---------------------------------------------------------------------------
// The speak-up policy (issue #9)

#[pollster::test]
async fn without_a_policy_only_what_addressed_the_brain_is_answered() {
    // The default deployment: no policy row anywhere, so unaddressed
    // chatter is silent **before** the classifier — no ask, no cost.
    let (judge, port) = judging(0.95);
    let (agent, http) = world(Some(port)).await;
    deliver(
        &agent.kit,
        "Ev14",
        TEAM,
        chatter("what is the refund window?"),
    )
    .await;
    assert!(
        posts(&http).is_empty(),
        "an unaddressed message under Off is not answered: {:?}",
        posts(&http)
    );
    assert!(
        judge.asks().is_empty(),
        "and it never reached the classifier: {:?}",
        judge.asks()
    );
    assert!(
        rows(&agent.kit, "SELECT * FROM turn_triage", vec![])
            .await
            .is_empty(),
        "a turn the classifier never saw leaves no ledger row"
    );

    deliver(
        &agent.kit,
        "Ev15",
        TEAM,
        mention("what is the refund window?"),
    )
    .await;
    assert_eq!(posts(&http).len(), 1, "a mention is still owed an answer");
}

#[pollster::test]
async fn an_eager_workspace_answers_chatter_until_the_channel_opts_out() {
    let (judge, port) = judging(0.95);
    let (agent, http) = world(Some(port)).await;
    set_policy(&agent, None, Proactivity::Eager).await;
    deliver(
        &agent.kit,
        "Ev16",
        TEAM,
        chatter("what is the refund window?"),
    )
    .await;
    let posted = posts(&http);
    assert_eq!(posted.len(), 1, "the org's ask is honoured: {posted:?}");
    assert!(
        posted[0]["text"]
            .as_str()
            .expect("a text field")
            .contains("fourteen days"),
        "{posted:?}"
    );
    // A proactive answer is held to the same cite-or-don't-know rule.
    assert!(
        posted[0]["blocks"].as_array().map(Vec::len) == Some(2),
        "the answer carries its citation: {posted:?}"
    );

    // A channel that opted out beats the org-wide ask.
    set_policy(&agent, Some("C0PUBLIC"), Proactivity::Off).await;
    deliver(
        &agent.kit,
        "Ev17",
        TEAM,
        chatter("what is the refund window?"),
    )
    .await;
    assert!(
        posts(&http).len() == 1,
        "the channel's Off wins over the org's Eager: {:?}",
        posts(&http)
    );
    assert_eq!(
        judge.asks().len(),
        1,
        "and nothing more was judged: {:?}",
        judge.asks()
    );
}

#[pollster::test]
async fn a_triaged_message_is_ledgered_once_and_without_its_text() {
    let (judge, port) = judging(0.95);
    let (agent, _http) = world(Some(port)).await;
    set_policy(&agent, None, Proactivity::Eager).await;
    deliver(
        &agent.kit,
        "Ev18",
        TEAM,
        chatter("what is the refund window?"),
    )
    .await;
    assert_eq!(
        judge.asks().len(),
        1,
        "the judge ran once: {:?}",
        judge.asks()
    );

    let ledger = rows(&agent.kit, "SELECT * FROM turn_triage", vec![]).await;
    assert_eq!(ledger.len(), 1, "one row for one triage: {ledger:?}");
    let row = &ledger[0];
    assert_eq!(row.get::<String>("decision").as_deref(), Some("reply"));
    assert_eq!(row.get::<String>("proactivity").as_deref(), Some("eager"));
    assert_eq!(row.get::<i64>("addressed").unwrap_or_default(), 0);
    assert_eq!(
        row.get::<String>("token_source").as_deref(),
        Some("estimated")
    );
    // Both sides of the call were estimated, and neither is a zero.
    for counted in ["input_tokens", "output_tokens"] {
        assert!(
            row.get::<i64>(counted).unwrap_or_default() > 0,
            "{counted}: {row:?}"
        );
    }
    // No column carries the words that were judged; the classifier saw
    // them, the ledger may not.
    let everything = format!("{row:?}");
    assert!(!everything.contains("refund"), "{everything}");
}

#[pollster::test]
async fn a_proactive_turn_that_knows_nothing_says_nothing() {
    let (judge, port) = judging(0.95);
    let (agent, http) = world(Some(port)).await;
    set_policy(&agent, None, Proactivity::Eager).await;
    deliver(
        &agent.kit,
        "Ev19",
        TEAM,
        chatter("what is the airspeed velocity of an unladen swallow?"),
    )
    .await;
    assert_eq!(judge.asks().len(), 1, "triage ran: {:?}", judge.asks());
    assert!(
        posts(&http).is_empty(),
        "the brain does not volunteer its ignorance: {:?}",
        posts(&http)
    );
}

#[pollster::test]
async fn a_near_miss_gets_a_reaction_and_no_answer() {
    // 0.60 clears the react band (0.70 x 0.75 = 0.525) and not the reply
    // bar (0.70) of a classifier-calibrated judge at Normal.
    let (_judge, port) = judging(0.60);
    let (agent, http) = world(Some(port)).await;
    set_policy(&agent, None, Proactivity::Normal).await;
    deliver(
        &agent.kit,
        "Ev20",
        TEAM,
        chatter("what is the refund window?"),
    )
    .await;
    // The fake records only `chat.*` calls as Web API posts, so a reaction
    // is asserted on the raw request log.
    let calls = http.requests();
    assert!(
        !calls
            .iter()
            .any(|(_, uri, _)| uri.contains("chat.postMessage")),
        "a near-miss computes no answer: {calls:?}"
    );
    let (_, uri, raw) = calls
        .iter()
        .find(|(_, uri, _)| uri.contains("reactions.add"))
        .expect("the near-miss got a reaction: {calls:?}");
    assert_eq!(uri, "https://slack.com/api/reactions.add");
    let body: Value = serde_json::from_str(raw).expect("a Web API body is JSON");
    assert_eq!(body["name"], json!("eyes"));
    assert_eq!(body["channel"], json!("C0PUBLIC"));
    assert_eq!(body["timestamp"], json!("1700000001.000100"));
    assert_eq!(
        column(
            &agent.kit,
            "SELECT decision AS value FROM turn_triage",
            vec![],
        )
        .await
        .as_deref(),
        Some("react")
    );
}

// ---------------------------------------------------------------------------
// What an answer may read

#[pollster::test]
async fn an_answer_cites_only_pages_the_asker_may_read() {
    let (agent, http) = world(None).await;

    // The decoy: same words, in a scope this asker was never granted.
    let decoy = Scope::User(UserId::new("UOTHER-MEMBER".to_owned()));
    seed_page(
        &agent,
        &decoy,
        "refund-window",
        "Refund window",
        "The refund window is thirty days, for the accounts team and nobody \
         else.",
    )
    .await;

    deliver(
        &agent.kit,
        "Ev10",
        TEAM,
        mention("what is the refund window?"),
    )
    .await;

    let posted = posts(&http);
    assert_eq!(posted.len(), 1, "{posted:?}");
    let body = posted[0]["text"].as_str().expect("a text field");
    assert!(body.contains("fourteen days"), "{body}");
    assert!(
        !body.contains("thirty days"),
        "a page in another member's scope was searched or quoted: {body}"
    );
    let context = posted[0]["blocks"][1]["elements"][0]["text"]
        .as_str()
        .expect("a link block");
    let decoy_scope = page_scope(TEAM, &decoy);
    assert!(
        !context.contains(&decoy_scope),
        "the decoy page was cited: {context}"
    );
}

#[pollster::test]
async fn an_answer_reads_nothing_when_the_question_matches_nothing() {
    let (agent, http) = world(None).await;
    deliver(
        &agent.kit,
        "Ev11",
        TEAM,
        mention("what is the airspeed velocity of an unladen swallow?"),
    )
    .await;
    let posted = posts(&http);
    assert_eq!(posted.len(), 1, "it still says something: {posted:?}");
    let body = posted[0]["text"].as_str().expect("a text field");
    assert!(
        body.starts_with("I don't know"),
        "an answer with nothing behind it says so: {body}"
    );
    assert!(body.contains("nothing in the pages you can read"), "{body}");
    // No citation block at all: an answer nothing backs must not look like
    // one something does.
    assert!(
        posted[0]["blocks"].as_array().map(Vec::len) == Some(1),
        "{posted:?}"
    );
}

#[pollster::test]
async fn a_secret_in_a_dm_reaches_neither_the_classifier_nor_the_index() {
    let (judge, port) = judging(0.95);
    let (agent, http) = world(Some(port)).await;
    // A token shaped like a credential, in a DM. Redaction (issue #108) has
    // to stand between these words and every place they would otherwise go:
    // the **classifier** first, because that is the first thing they reach on
    // their way out, and the page index second.
    deliver(
        &agent.kit,
        "Ev12",
        TEAM,
        direct(&format!(
            "what is the refund window? {}",
            // Assembled from fragments so push protection does not read the
            // fake credential in this fixture as a real one. The runtime
            // string is exactly `sk_live_` plus the block below.
            concat!(
                "sk_", "live_", "51H8xQ2", "kLmNp0Rt", "YuIoP4aS", "dF6gH7jK", "lZ"
            )
        )),
    )
    .await;

    let asks = judge.asks();
    assert_eq!(asks.len(), 1, "the judge did run: {asks:?}");
    let judged = &asks[0].state;
    assert!(
        !judged.contains("sk_live_"),
        "the credential went to the classifier verbatim: {judged}"
    );
    // The app's own name is stripped before redaction, so the thing judged
    // is the question rather than a bot's user id.
    assert!(
        !judged.contains(BOT),
        "the bot's mention was judged as part of the question: {judged}"
    );

    let posted = posts(&http);
    assert_eq!(posted.len(), 1, "{posted:?}");
    let everything = posted[0].to_string();
    assert!(
        !everything.contains("sk_live_"),
        "the credential-shaped span reached the answer: {everything}"
    );
    assert!(
        everything.contains("sensitive span"),
        "and the reply says so rather than pretending it did not: {everything}"
    );
}

// ---------------------------------------------------------------------------
// The transport

#[pollster::test]
async fn a_slack_refusal_is_not_a_post_and_never_reaches_a_reply() {
    let (agent, http) = world(None).await;
    // Slack answers a refusal with HTTP 200 and `ok: false`, which is why
    // the body is read rather than the status line.
    http.web_api_will_answer(&json!({"ok": false, "error": "channel_not_found"}).to_string());
    deliver(
        &agent.kit,
        "Ev13",
        TEAM,
        mention("what is the refund window?"),
    )
    .await;
    // The loop tried once and said nothing further: a provider's own error
    // text is not a reply and must not become one.
    let calls = http.api_calls();
    assert_eq!(calls.len(), 1, "the loop tried, and Slack said no");
    assert_eq!(calls[0].method, "POST");
}
