//! The turn (issue #7), end to end through the Events webhook: two messages
//! in one thread never produce interleaved replies, a message that lands
//! mid-turn is folded into the next turn rather than dropped, two mid-turn
//! messages coalesce into **one** next turn, and a turn that cannot finish
//! in one conversation blocks no other.
//!
//! The ordinary `deliver` helper would prove nothing here: it drains the
//! deferred work *sequentially*, which is the serialisation under test, not
//! a way to exercise it. These tests take the deferred futures before
//! anything runs and drive them **together**, one poll each per rotation, so
//! the second dispatch genuinely races the first — and what the coordinator
//! does about it is the thing being asserted.

mod support;

use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::Poll;

use async_trait::async_trait;
use cratefield_core::axum::http::StatusCode;
use cratefield_core::{Config, Defer};
use cratefield_testing::TestHarness;
use livingbrain_pages::{AnswerError, Answered, Answers, Asker};
use livingbrain_workspaces::{
    Admit, Exchange, LocalTurns, Pending, Turn, TurnError, Turns, Workspaces,
};
use serde_json::{Value, json};
use support::*;

// ---------------------------------------------------------------------------
// A defer the test drives itself

/// One deferred dispatch, exactly the shape `Defer::wait_until` hands over.
type Deferred = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Collects the dispatch futures instead of running them, so a test decides
/// when they run and against whom.
#[derive(Clone, Default)]
struct SharedDefer(Arc<Mutex<Vec<Deferred>>>);

impl SharedDefer {
    fn take(&self) -> Vec<Deferred> {
        std::mem::take(&mut self.0.lock().expect("lock"))
    }
}

impl Defer for SharedDefer {
    fn wait_until(&self, fut: Deferred) {
        self.0.lock().expect("lock").push(fut);
    }
}

/// Polls every deferred future once per rotation, until they are all done or
/// the rotations run out. A completed future is never polled again — a
/// resumed-after-completion future is a bug, not a no-op — and a test that
/// expects completion asserts on its posts, so running out of rotations
/// shows up as a failed assertion, not a silence.
async fn drive(defer: &SharedDefer, rotations: usize) {
    let mut futs: Vec<Option<Deferred>> = defer.take().into_iter().map(Some).collect();
    std::future::poll_fn(move |cx| {
        for _ in 0..rotations {
            let mut done = true;
            for slot in futs.iter_mut() {
                if let Some(fut) = slot {
                    if fut.as_mut().poll(cx).is_ready() {
                        *slot = None;
                    } else {
                        done = false;
                    }
                }
            }
            if done {
                break;
            }
        }
        Poll::Ready(())
    })
    .await;
}

// ---------------------------------------------------------------------------
// The gate: arrivals, and the answers held behind them

/// What the test holds an answer behind. Every `arrive` the coordinator sees
/// is counted **per conversation**, and a conversation's answers stay parked
/// until its arrivals reach the hold — which is what puts a mid-turn arrival
/// on the timeline: the first turn is demonstrably still open when the next
/// message is admitted.
///
/// A question carrying the `free` marker is never held. That is how one test
/// keeps two conversations at once: one parked on a hold that never opens,
/// one flowing past it.
#[derive(Clone)]
struct Gate {
    hold_until: usize,
    state: Arc<Mutex<GateState>>,
}

#[derive(Default)]
struct GateState {
    arrivals: HashMap<String, usize>,
    questions: HashMap<String, Vec<String>>,
    released: HashSet<String>,
    events: Vec<String>,
}

impl Gate {
    fn new(hold_until: usize) -> Self {
        Self {
            hold_until,
            state: Arc::new(Mutex::new(GateState::default())),
        }
    }

    fn arrive(&self, key: &str, question: &str) {
        let mut state = self.state.lock().expect("lock");
        state
            .questions
            .entry(key.to_owned())
            .or_default()
            .push(question.to_owned());
        let count = state.arrivals.entry(key.to_owned()).or_default();
        *count += 1;
        if *count >= self.hold_until {
            let waiting = state.questions[key].clone();
            state.released.extend(waiting);
        }
        state.events.push(format!("arrive {question}"));
    }

    fn released(&self, question: &str) -> bool {
        self.state.lock().expect("lock").released.contains(question)
    }

    /// Entry to the answerer, recorded **before** the hold: the moment a
    /// turn begins spending on a question.
    fn started(&self, question: &str) {
        self.state
            .lock()
            .expect("lock")
            .events
            .push(format!("start:{question}"));
    }

    /// Completion of the answer, after the hold has opened.
    fn answered(&self, question: &str) {
        self.state
            .lock()
            .expect("lock")
            .events
            .push(format!("end:{question}"));
    }

    fn events(&self) -> Vec<String> {
        self.state.lock().expect("lock").events.clone()
    }
}

/// The coordinator the tests wire: the real [`LocalTurns`], with the gate
/// counting on the way in.
struct GateTurns {
    gate: Gate,
    local: LocalTurns,
}

#[async_trait]
impl Turns for GateTurns {
    async fn arrive(&self, key: &str, pending: Pending, now: i64) -> Result<Admit, TurnError> {
        self.gate.arrive(key, &pending.question);
        self.local.arrive(key, pending, now).await
    }

    async fn finish(
        &self,
        key: &str,
        lease: u64,
        exchange: Option<Exchange>,
        now: i64,
    ) -> Result<Option<Turn>, TurnError> {
        self.local.finish(key, lease, exchange, now).await
    }
}

/// The answer seam the tests wire: a turn's ask is held until the gate
/// releases it, then answered with words that name the question, so a post
/// carries the ask it was answering.
struct GatedAnswers {
    gate: Gate,
}

#[async_trait]
impl Answers for GatedAnswers {
    async fn answer(&self, _asker: &Asker, question: &str) -> Result<Answered, AnswerError> {
        self.gate.started(question);
        let gate = self.gate.clone();
        let held = question.to_owned();
        // A turn's ask is the coalesced questions joined with newlines, so a
        // coalesced turn goes when every question it carries is released.
        std::future::poll_fn(move |cx| {
            let open = held.contains("free") || held.split('\n').all(|part| gate.released(part));
            if open {
                Poll::Ready(())
            } else {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
        .await;
        self.gate.answered(question);
        Ok(Answered {
            text: format!("said: {question}"),
            citations: Vec::new(),
        })
    }
}

// ---------------------------------------------------------------------------
// The world

/// A harness whose agent answers through a [`GatedAnswers`] and coordinates
/// through a [`GateTurns`], with the deferred dispatch collected in a
/// [`SharedDefer`] for the test to drive.
struct World {
    kit: TestHarness,
    gate: Gate,
    defer: SharedDefer,
    http: TokenHttp,
}

/// A question in one thread of one channel: each message its own `ts`, all
/// of them the same `thread_ts` — which is the conversation the coordinator
/// keys on.
fn threaded(ts: &str, text: &str) -> Value {
    json!({
        "type": "app_mention",
        "user": ASKER,
        "channel": "C0PUBLIC",
        "channel_type": "channel",
        "text": format!("<@{BOT}> {text}"),
        "ts": ts,
        "thread_ts": "1700000000.000100",
    })
}

/// A question in a different channel: a different conversation key, so
/// nothing about it may wait on the first.
fn elsewhere(text: &str) -> Value {
    json!({
        "type": "app_mention",
        "user": ASKER,
        "channel": "C0OTHER",
        "channel_type": "channel",
        "text": format!("<@{BOT}> {text}"),
        "ts": "1700000000.000900",
    })
}

fn world(hold_until: usize) -> World {
    let gate = Gate::new(hold_until);
    let defer = SharedDefer::default();
    let http = TokenHttp::new();
    let kit = TestHarness::with_ports(
        vec![Box::new(
            Workspaces::new()
                .answering(Arc::new(GatedAnswers { gate: gate.clone() }))
                .taking_turns(Arc::new(GateTurns {
                    gate: gate.clone(),
                    local: LocalTurns::default(),
                })),
        )],
        {
            let defer = defer.clone();
            let http = http.clone();
            move |ports| {
                let config: Arc<dyn Config> = Arc::new(config());
                ports.config = config;
                ports.http = Some(Arc::new(http));
                ports.clock = Some(Arc::new(clock()));
                // The unwired judge: a mention is answered, a DM is not.
                ports.classifier = None;
                ports.defer = Some(Arc::new(defer));
            }
        },
    );
    World {
        kit,
        gate,
        defer,
        http,
    }
}

/// The workspace, the Slack install and the asker's own membership — the
/// same prerequisites the issue #123 tests set, through the same routes.
async fn install(world: &World) {
    seed_legacy_workspace(&world.kit, TEAM, "a workspace", ASKER).await;
    let attempt = start_install(&world.kit).await;
    world
        .http
        .will_answer(&install_answer(TEAM, "A0APP", BOT, BOT_TOKEN));
    let callback = get(
        &world.kit.router,
        &attempt.callback("install-code"),
        &attempt.cookies(),
    )
    .await;
    assert_eq!(
        callback.status,
        StatusCode::OK,
        "the install completes: {}",
        String::from_utf8_lossy(&callback.body)
    );
}

/// Hands one signed delivery to the module and stops there: the dispatch is
/// in [`SharedDefer`] now, waiting for [`drive`].
async fn hand_in(kit: &TestHarness, event_id: &str, event: Value) {
    let response = post_signed(&kit.router, &envelope(event_id, TEAM, event), NOW).await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "the delivery is acknowledged: {}",
        String::from_utf8_lossy(&response.body)
    );
}

/// The answers the agent posted to Slack, in order.
fn posts(http: &TokenHttp) -> Vec<Call> {
    http.api_calls()
        .into_iter()
        .filter(|call| call.uri.contains("chat.postMessage"))
        .collect()
}

// ---------------------------------------------------------------------------
// The acceptance criteria

/// Two messages in the same thread: the second is admitted while the first's
/// turn is still open — the gate holds that turn open until both have
/// arrived — and the replies still come out one after the other, in the
/// thread, in order.
#[pollster::test]
async fn two_messages_in_one_thread_cannot_interleave_their_replies() {
    let world = world(2);
    install(&world).await;
    hand_in(&world.kit, "Ev1", threaded("1700000000.000100", "q1")).await;
    hand_in(&world.kit, "Ev2", threaded("1700000000.000200", "q2")).await;
    drive(&world.defer, 100).await;

    let posts = posts(&world.http);
    assert_eq!(posts.len(), 2, "one answer per turn: {posts:?}");
    for post in &posts {
        assert_eq!(
            post.json()["thread_ts"],
            json!("1700000000.000100"),
            "every reply stays in the thread"
        );
    }
    assert!(posts[0].body.contains("said: q1"), "{:?}", posts[0].body);
    assert!(posts[1].body.contains("said: q2"), "{:?}", posts[1].body);
    // The order that matters: q2 was admitted **before** q1's turn answered
    // it — a mid-turn arrival — and still the turns never overlapped: q1's
    // turn began and ended before q2's turn began, and the posts came out in
    // that same order.
    assert_eq!(
        world.gate.events(),
        [
            "arrive q1",
            "start:q1",
            "arrive q2",
            "end:q1",
            "start:q2",
            "end:q2",
        ],
        "the second message arrived inside the first's turn, and the turns did not overlap"
    );
}

/// Two messages that land while the first turn runs are coalesced into one
/// next turn: one ask carrying both questions, one post, in order.
#[pollster::test]
async fn messages_arriving_mid_turn_are_coalesced_into_one_next_turn() {
    let world = world(3);
    install(&world).await;
    hand_in(&world.kit, "Ev1", threaded("1700000000.000100", "q1")).await;
    hand_in(&world.kit, "Ev2", threaded("1700000000.000200", "q2")).await;
    hand_in(&world.kit, "Ev3", threaded("1700000000.000300", "q3")).await;
    drive(&world.defer, 100).await;

    let posts = posts(&world.http);
    assert_eq!(posts.len(), 2, "two turns, not three: {posts:?}");
    assert!(posts[0].body.contains("said: q1"), "{:?}", posts[0].body);
    assert!(
        posts[1].body.contains("said: q2") && posts[1].body.contains("q3"),
        "the second turn carries both questions: {:?}",
        posts[1].body
    );
    assert_eq!(
        world.gate.events(),
        [
            "arrive q1",
            "start:q1",
            "arrive q2",
            "arrive q3",
            "end:q1",
            "start:q2\nq3",
            "end:q2\nq3",
        ],
        "the mid-turn arrivals answered together, in one turn, in order"
    );
}

/// A turn that cannot finish — its answer is held for ever — blocks nothing
/// outside its own conversation: the other channel's message is admitted,
/// answered and posted while the first is still parked.
#[pollster::test]
async fn a_turn_parked_in_one_conversation_blocks_no_other() {
    let world = world(2);
    install(&world).await;
    hand_in(
        &world.kit,
        "Ev1",
        threaded("1700000000.000100", "hold the line"),
    )
    .await;
    hand_in(&world.kit, "Ev2", elsewhere("free and quick")).await;
    drive(&world.defer, 20).await;

    let posts = posts(&world.http);
    assert_eq!(
        posts.len(),
        1,
        "only the other conversation answers: {posts:?}"
    );
    assert!(
        posts[0].body.contains("said: free and quick"),
        "{:?}",
        posts[0].body
    );
    assert_eq!(
        world.gate.events(),
        [
            "arrive hold the line",
            "start:hold the line",
            "arrive free and quick",
            "start:free and quick",
            "end:free and quick",
        ],
        "the held conversation stayed held"
    );
}
