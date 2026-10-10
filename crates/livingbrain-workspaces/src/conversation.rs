//! The per-conversation turn (issue #7): several people can talk in one
//! thread at once, and the agent answers one turn at a time. Every message
//! goes to a coordinator keyed by [`Message::conversation_key`], which admits
//! the first to a **turn** and queues the rest; a finishing turn is handed
//! the mid-turn arrivals, coalesced, as the next turn — so two messages in
//! one thread never produce interleaved replies, and a message that lands
//! mid-turn is folded into the next turn rather than dropped.
//!
//! All pure: a [`Conversation`] is a serde state machine with no I/O (`now`
//! arrives on every call), so the Durable Object — which lives in the
//! venture; a library crate cannot export one — is *load, [`apply`], store*,
//! the coordinator itself is the [`Turns`] port, and [`LocalTurns`] is what
//! a composition without the venture's object gets. A holder killed mid-turn
//! recovers through the lease: it expires ([`LEASE_TTL`]), the next arrival
//! takes the thread over, and the dead holder's finish is refused without
//! touching the queue.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::Mutex;

use livingbrain_channel::{Identity, Location, Message, Platform, Visibility};
use serde::{Deserialize, Serialize};

/// How long a turn may hold the thread before the next arrival takes over.
///
/// 120s sits well above a search, an answer and a Slack post — too short
/// and live turns are taken over, too long and a dead one holds the floor.
/// A turn that does outlive its lease can be taken over, and the old
/// holder's reply may then land after the new turn's: the stale finish
/// protects the queue, not the ordering.
const LEASE_TTL: i64 = 120;

/// The exchanges a conversation remembers for its turns. Short by design:
/// a working context for the next answer, not a history — and held for the
/// turn only so far. A turn receives it in [`Turn::context`], but nothing
/// feeds it to the answerer yet, because
/// [`Answers`](livingbrain_pages::Answers) takes no history; that is the
/// follow-up this constant is sized for.
const CONTEXT_EXCHANGES: usize = 8;

/// How many characters of a question or an answer the context keeps. Cut on
/// a char boundary — a Slack message is UTF-8, and a string cut mid-character
/// is not a string that can exist.
const CONTEXT_TEXT_CHARS: usize = 500;

/// One question and the answer it was given, as the working context of the
/// conversation a turn is handed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exchange {
    /// What was asked — the coalesced questions when a turn answered more
    /// than one message.
    pub question: String,
    /// What the agent said back.
    pub(crate) answer: String,
}

/// Who could read the message, as the channel adapter classified it.
///
/// The one part of a message's [`Location`] a turn still needs once the
/// event that carried it is gone: the access model grants an answer by
/// visibility, and every message in one conversation shares it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Room {
    /// A channel anyone in the platform can read.
    Public,
    /// A channel only its members can read.
    Private,
    /// Two people only.
    Direct,
}

impl Room {
    /// The room a message was seen in. The classification is the channel
    /// adapter's, which fails closed already — an unreadable channel is
    /// [`Visibility::Private`] before it ever gets here.
    #[must_use]
    pub(crate) fn of(visibility: Visibility) -> Self {
        match visibility {
            Visibility::Public => Self::Public,
            Visibility::Private => Self::Private,
            Visibility::Direct => Self::Direct,
        }
    }

    /// The visibility back, for the location a reply target carries.
    #[must_use]
    pub(crate) fn visibility(self) -> Visibility {
        match self {
            Self::Public => Visibility::Public,
            Self::Private => Visibility::Private,
            Self::Direct => Visibility::Direct,
        }
    }
}

/// One gated message, kept until its turn comes: everything the loop needs
/// to answer it later, the event that carried it long gone. The question is
/// the already-gated text — a turn never re-runs the gates, and never
/// answers a message they turned away.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    /// The member the author resolves to — the identity an answer is
    /// access-scoped to.
    pub user: String,
    /// The author's platform id, for the reply target.
    pub slack_user: String,
    /// The channel the message arrived in.
    pub channel: String,
    /// Who could read it there.
    pub room: Room,
    /// The thread the reply lands in: the message's own thread, or its own
    /// id — the same value [`Message::conversation_key`] keys the
    /// conversation by.
    pub thread: String,
    /// The platform id of the message itself.
    pub ts: String,
    /// The question, gated and with the bot's own name taken off.
    pub question: String,
    /// Whether the message named the bot or arrived in a DM (issue #9): a
    /// turn that holds one is asked for, so an uncited answer says "I don't
    /// know" rather than posting nothing. Defaults to `false` for a pending
    /// message queued before the field existed.
    #[serde(default)]
    pub addressed: bool,
}

impl Pending {
    /// The message a reply to this pending one is rendered against. Only
    /// what [`livingbrain_channel::Channel::reply`] reads is meaningful: the
    /// channel, the thread and the message id. `thread` is set even for the
    /// message that started it — Slack treats a `thread_ts` equal to a
    /// message's own id as the top of the thread.
    #[must_use]
    pub(crate) fn reply_target(&self, team_id: &str) -> Message {
        Message {
            author: Identity {
                platform: Platform::Slack,
                team: team_id.to_owned(),
                user: self.slack_user.clone(),
            },
            location: Location {
                platform: Platform::Slack,
                channel: self.channel.clone(),
                visibility: self.room.visibility(),
            },
            thread: Some(self.thread.clone()),
            id: self.ts.clone(),
            text: self.question.clone(),
            mentions_bot: true,
        }
    }
}

/// The right to answer the conversation's next reply, held for one turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Lease {
    id: u64,
    expires_at: i64,
}

/// One conversation's turn state: who holds the floor, what arrived mid-turn
/// and what the conversation remembers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conversation {
    /// The turn in progress, or `None` when the conversation is idle.
    lease: Option<Lease>,
    /// Messages that arrived mid-turn, in arrival order.
    pending: Vec<Pending>,
    /// The last few exchanges, oldest first.
    context: VecDeque<Exchange>,
    /// The next lease id. Monotonic per conversation, which is what makes a
    /// superseded holder's finish recognisable rather than a guess.
    next_lease: u64,
}

/// What [`Conversation::arrive`] decided to do with a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Admit {
    /// The conversation was idle (or its lease had expired): the message
    /// starts a turn the caller answers now.
    Run(Turn),
    /// A turn is already running: the message is queued and the running
    /// turn folds it in when it finishes. The caller says nothing.
    Queued,
}

/// One turn of the conversation: the lease to present at [`finish`](Self::finish),
/// the messages to answer, and what the conversation remembers so far.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Turn {
    /// Present this at [`Conversation::finish`]; a turn whose lease no
    /// longer matches was superseded and must release quietly.
    pub(crate) lease: u64,
    /// The gated messages this turn answers, in arrival order, all from one
    /// member.
    pub(crate) messages: Vec<Pending>,
    /// The conversation's working context, oldest first.
    pub(crate) context: Vec<Exchange>,
}

impl Conversation {
    /// What becomes of a newly arrived message: a turn to answer now, or a
    /// place in the queue of the turn already running.
    ///
    /// Taking over an expired lease folds the stranded messages into the
    /// new turn ahead of this one — they arrived first, and arrival order
    /// is the order the thread saw.
    pub fn arrive(&mut self, pending: Pending, now: i64) -> Admit {
        if self
            .lease
            .as_ref()
            .is_some_and(|lease| lease.expires_at > now)
        {
            self.pending.push(pending);
            return Admit::Queued;
        }
        self.pending.push(pending);
        Admit::Run(self.start(now))
    }

    /// The turn's answer is in — or was never produced, or never posted,
    /// which is `exchange` of `None` and records nothing. Returns the next
    /// turn to answer, or `None` when the conversation is idle again.
    ///
    /// A `lease` that does not match means this holder was superseded: the
    /// TTL expired and another arrival took the thread over, stranded
    /// messages and all. It is refused **without mutating anything** — the
    /// pending queue belongs to the turn that took over now.
    pub fn finish(&mut self, lease: u64, exchange: Option<Exchange>, now: i64) -> Option<Turn> {
        if self.lease.as_ref().is_none_or(|held| held.id != lease) {
            return None;
        }
        if let Some(exchange) = exchange {
            self.context.push_back(truncated(exchange));
            while self.context.len() > CONTEXT_EXCHANGES {
                self.context.pop_front();
            }
        }
        match self.pending.is_empty() {
            true => {
                self.lease = None;
                None
            }
            false => Some(self.start(now)),
        }
    }

    /// Starts a turn from the head of the pending queue: the leading run of
    /// messages from one member, and only that run, in arrival order. An
    /// answer is access-scoped to one member — the
    /// [`Asker`](livingbrain_pages::Asker) is who the pages are searched
    /// for — so a turn that mixed two members would answer one of them from
    /// pages only the other may read; whoever is behind the run stays queued
    /// for the turn after, still serialised and never dropped.
    fn start(&mut self, now: i64) -> Turn {
        let user = self.pending[0].user.clone();
        let taken = self
            .pending
            .iter()
            .take_while(|pending| pending.user == user)
            .count();
        let messages = self.pending.drain(..taken).collect();
        self.next_lease += 1;
        self.lease = Some(Lease {
            id: self.next_lease,
            expires_at: now + LEASE_TTL,
        });
        Turn {
            lease: self.next_lease,
            messages,
            context: self.context.iter().cloned().collect(),
        }
    }
}

/// Keeps the working context small, whatever a turn produced.
fn truncated(exchange: Exchange) -> Exchange {
    Exchange {
        question: take_chars(&exchange.question),
        answer: take_chars(&exchange.answer),
    }
}

/// The first [`CONTEXT_TEXT_CHARS`] characters, cut on a char boundary.
fn take_chars(text: &str) -> String {
    text.chars().take(CONTEXT_TEXT_CHARS).collect()
}

/// One call to a conversation's coordinator, over the wire between the
/// agent loop and the Durable Object. Both sides speak these types: the
/// module defines them, the venture's object answers them with [`apply`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Call {
    /// A message arrived; admit or queue it.
    Arrive { pending: Pending, now: i64 },
    /// A turn finished; record the exchange and hand back the next one.
    Finish {
        lease: u64,
        exchange: Option<Exchange>,
        now: i64,
    },
}

/// What the coordinator answered. Which variant is meaningful depends on
/// the [`Call`]; the mismatch is an error for the caller, not a guess here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Answer {
    /// The reply to [`Call::Arrive`].
    Arrived(Admit),
    /// The reply to [`Call::Finish`]: the next turn, or `None` when the
    /// conversation is idle.
    Finished(Option<Turn>),
}

/// Applies one call to one conversation — the whole Durable Object handler,
/// minus the load and the store.
///
/// The venture's object loads the conversation from object storage, calls
/// this, and stores it back with **no outbound I/O in between**: workerd's
/// input gate holds every other event off the object until the handler
/// yields, so the read-modify-write is atomic and two messages arriving in
/// the same thread are serialised by the object itself.
pub fn apply(state: &mut Conversation, call: Call) -> Answer {
    match call {
        Call::Arrive { pending, now } => Answer::Arrived(state.arrive(pending, now)),
        Call::Finish {
            lease,
            exchange,
            now,
        } => Answer::Finished(state.finish(lease, exchange, now)),
    }
}

/// Why a turn could not be coordinated. The text is a diagnostic for the
/// agent's one-line report and never carries a message. The field is public
/// because the coordinators that fail are mostly not this crate's — the
/// venture's Durable Object client maps every transport fault to one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnError(pub String);

impl fmt::Display for TurnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TurnError {}

/// The turn coordinator: one conversation per key, keyed by
/// [`Message::conversation_key`].
///
/// A dyn seam like [`Answers`](crate::Answers), for the same reason: the
/// coordinator is the venture's Durable Object, which this module may not
/// hold. Both methods are infallible in their state machine and fallible on
/// the way to it — a coordinator that cannot be reached is the caller's
/// decision, not a silent queue.
#[async_trait::async_trait]
pub trait Turns: Send + Sync {
    /// Admits a message to its conversation: a turn to answer now, or a
    /// queue slot in the turn already running.
    ///
    /// # Errors
    /// When the coordinator for `key` could not be reached.
    async fn arrive(&self, key: &str, pending: Pending, now: i64) -> Result<Admit, TurnError>;

    /// Finishes a turn: records the exchange, and hands back the next turn
    /// when messages were coalesced meanwhile. `None` releases the floor.
    ///
    /// # Errors
    /// When the coordinator for `key` could not be reached. The caller
    /// stops rather than finishing again — the lease's TTL recovers the
    /// thread.
    async fn finish(
        &self,
        key: &str,
        lease: u64,
        exchange: Option<Exchange>,
        now: i64,
    ) -> Result<Option<Turn>, TurnError>;
}

/// The coordinator a composition gets when the venture injects nothing:
/// one [`Conversation`] per key, behind a mutex, for this isolate only.
///
/// Native tests and development run here; a Worker isolate is
/// single-threaded, so within one isolate this serialises exactly as the
/// Durable Object does across them. Across isolates it does not — that is
/// what the venture's object is for.
#[derive(Default)]
pub struct LocalTurns {
    conversations: Mutex<HashMap<String, Conversation>>,
}

impl LocalTurns {
    /// The map every conversation lives in, for the duration of one call.
    fn conversations(&self) -> std::sync::MutexGuard<'_, HashMap<String, Conversation>> {
        self.conversations.lock().expect("conversation lock")
    }
}

#[async_trait::async_trait]
impl Turns for LocalTurns {
    async fn arrive(&self, key: &str, pending: Pending, now: i64) -> Result<Admit, TurnError> {
        let mut conversations = self.conversations();
        Ok(conversations
            .entry(key.to_owned())
            .or_default()
            .arrive(pending, now))
    }

    async fn finish(
        &self,
        key: &str,
        lease: u64,
        exchange: Option<Exchange>,
        now: i64,
    ) -> Result<Option<Turn>, TurnError> {
        let mut conversations = self.conversations();
        Ok(match conversations.get_mut(key) {
            Some(conversation) => conversation.finish(lease, exchange, now),
            // Nothing to finish: no conversation was ever started, so
            // there is no lease to match and no state to invent.
            None => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The instant the tests run at.
    const NOW: i64 = 1_800_000_000;

    /// A pending message from `user` asking `question`.
    fn pending(user: &str, question: &str) -> Pending {
        Pending {
            user: user.to_owned(),
            slack_user: user.to_owned(),
            channel: "C0THREAD".to_owned(),
            room: Room::Public,
            thread: "1700000000.000100".to_owned(),
            ts: "1700000000.000100".to_owned(),
            question: question.to_owned(),
            addressed: true,
        }
    }

    /// The exchange a finished turn is recorded with.
    fn exchange(turn: &Turn) -> Exchange {
        Exchange {
            question: turn
                .messages
                .iter()
                .map(|pending| pending.question.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            answer: format!("said: {}", turn.messages[0].question),
        }
    }

    /// The questions of a turn, in order.
    fn questions(turn: &Turn) -> Vec<&str> {
        turn.messages
            .iter()
            .map(|pending| pending.question.as_str())
            .collect()
    }

    #[test]
    fn an_idle_conversation_admits_the_message_to_a_turn() {
        let mut conversation = Conversation::default();
        let admit = conversation.arrive(pending("U1", "first?"), NOW);
        let Admit::Run(turn) = admit else {
            panic!("an idle conversation runs the message: {admit:?}");
        };
        assert_eq!(questions(&turn), ["first?"]);
        assert!(turn.context.is_empty(), "nothing to remember yet");
    }

    #[test]
    fn finishing_a_turn_with_pending_messages_starts_the_next_one() {
        let mut conversation = Conversation::default();
        let Admit::Run(first) = conversation.arrive(pending("U1", "first?"), NOW) else {
            panic!("the first message runs");
        };
        conversation.arrive(pending("U1", "second?"), NOW);
        conversation.arrive(pending("U1", "third?"), NOW);

        let next = conversation
            .finish(first.lease, Some(exchange(&first)), NOW)
            .expect("pending messages earn a next turn");
        // Both mid-turn arrivals, in arrival order, in ONE turn.
        assert_eq!(questions(&next), ["second?", "third?"]);
        // And the turn carries what the conversation remembers.
        assert_eq!(next.context.len(), 1, "{:?}", next.context);
        assert_eq!(next.context[0].question, "first?");
    }

    #[test]
    fn a_different_members_message_waits_for_its_own_turn() {
        let mut conversation = Conversation::default();
        let Admit::Run(first) = conversation.arrive(pending("U1", "mine?"), NOW) else {
            panic!("the first message runs");
        };
        // Two arrivals mid-turn from a different member, then one more from
        // the first: the next turn answers only the other member's run.
        conversation.arrive(pending("U2", "theirs?"), NOW);
        conversation.arrive(pending("U2", "theirs too?"), NOW);
        conversation.arrive(pending("U1", "mine again?"), NOW);

        let second = conversation
            .finish(first.lease, Some(exchange(&first)), NOW)
            .expect("the other member's run runs next");
        assert_eq!(questions(&second), ["theirs?", "theirs too?"]);
        assert_eq!(
            second.messages.len(),
            2,
            "U1's message must not be answered under U2's scope"
        );

        let third = conversation
            .finish(second.lease, Some(exchange(&second)), NOW)
            .expect("the leftover message runs after");
        assert_eq!(questions(&third), ["mine again?"]);
    }

    #[test]
    fn finishing_a_quiet_conversation_releases_the_floor() {
        let mut conversation = Conversation::default();
        let Admit::Run(turn) = conversation.arrive(pending("U1", "only?"), NOW) else {
            panic!("the first message runs");
        };
        assert!(
            conversation
                .finish(turn.lease, Some(exchange(&turn)), NOW)
                .is_none()
        );
        // And the conversation is idle: the next arrival runs again.
        assert!(matches!(
            conversation.arrive(pending("U1", "again?"), NOW),
            Admit::Run(_)
        ));
    }

    #[test]
    fn a_superseded_lease_finishes_nothing() {
        let mut conversation = Conversation::default();
        let Admit::Run(first) = conversation.arrive(pending("U1", "held?"), NOW) else {
            panic!("the first message runs");
        };
        // The holder dies; the lease expires; the next arrival takes over,
        // and another message queues behind the takeover mid-turn.
        let Admit::Run(second) = conversation.arrive(pending("U1", "next?"), NOW + LEASE_TTL + 1)
        else {
            panic!("an expired lease is taken over");
        };
        conversation.arrive(pending("U1", "queued?"), NOW + LEASE_TTL + 2);
        assert_ne!(first.lease, second.lease, "a takeover mints a new lease");

        // The dead holder's finish is refused, and mutates nothing: the
        // queue the takeover owns still earns its turn, and the takeover's
        // exchange is the only one remembered.
        assert!(
            conversation
                .finish(first.lease, Some(exchange(&first)), NOW + LEASE_TTL + 1)
                .is_none()
        );
        let next = conversation
            .finish(second.lease, Some(exchange(&second)), NOW + LEASE_TTL + 1)
            .expect("the takeover's own finish still works");
        assert_eq!(questions(&next), ["queued?"]);
        assert_eq!(next.context.len(), 1, "{:?}", next.context);
        assert_eq!(
            next.context[0].question, "next?",
            "the refused finish recorded no exchange of its own"
        );
    }

    #[test]
    fn an_expired_lease_takeover_folds_the_stranded_messages() {
        let mut conversation = Conversation::default();
        let Admit::Run(first) = conversation.arrive(pending("U1", "held?"), NOW) else {
            panic!("the first message runs");
        };
        // Messages that queued behind a holder that then died.
        conversation.arrive(pending("U1", "stranded one?"), NOW + 1);
        conversation.arrive(pending("U1", "stranded two?"), NOW + 2);

        // The takeover runs the strand, in order, ahead of nothing: it IS
        // the next turn.
        let Admit::Run(second) =
            conversation.arrive(pending("U1", "latecomer?"), NOW + LEASE_TTL + 1)
        else {
            panic!("an expired lease is taken over");
        };
        assert_eq!(
            questions(&second),
            ["stranded one?", "stranded two?", "latecomer?"],
            "the strand is folded in, in arrival order"
        );
        let _ = first;
    }

    #[test]
    fn the_working_context_is_bounded_and_truncated() {
        let mut conversation = Conversation::default();
        let long = "a question that goes on".repeat(100);
        for _ in 0..CONTEXT_EXCHANGES + 2 {
            let Admit::Run(turn) = conversation.arrive(pending("U1", &long), NOW) else {
                panic!("each turn runs");
            };
            let mut answered = exchange(&turn);
            answered.answer = "an answer".repeat(100);
            let _ = conversation.finish(turn.lease, Some(answered), NOW);
        }
        // The next turn sees the bound: the oldest exchanges fell off, and
        // what is left is cut to the character ceiling.
        let Admit::Run(turn) = conversation.arrive(pending("U1", "what do you remember?"), NOW)
        else {
            panic!("the turn runs");
        };
        assert_eq!(
            turn.context.len(),
            CONTEXT_EXCHANGES,
            "{:?}",
            turn.context.len()
        );
        for exchange in &turn.context {
            assert_eq!(exchange.question.chars().count(), CONTEXT_TEXT_CHARS);
            assert_eq!(exchange.answer.chars().count(), CONTEXT_TEXT_CHARS);
        }
    }

    #[test]
    fn the_wire_types_round_trip_through_json() {
        let mut conversation = Conversation::default();
        let Admit::Run(turn) = conversation.arrive(pending("U1", "first?"), NOW) else {
            panic!("the first message runs");
        };
        conversation.arrive(pending("U1", "second?"), NOW);
        let finished = apply(
            &mut conversation,
            Call::Finish {
                lease: turn.lease,
                exchange: Some(exchange(&turn)),
                now: NOW,
            },
        );
        let encoded = serde_json::to_string(&finished).expect("an answer encodes");
        assert_eq!(
            serde_json::from_str::<Answer>(&encoded).expect("and decodes"),
            finished,
            "the Durable Object's reply survives the wire"
        );
    }

    #[test]
    fn a_finished_answer_is_truncated_on_the_way_into_the_context() {
        let mut conversation = Conversation::default();
        let Admit::Run(turn) = conversation.arrive(pending("U1", &"q".repeat(1000)), NOW) else {
            panic!("the first message runs");
        };
        let _ = apply(
            &mut conversation,
            Call::Finish {
                lease: turn.lease,
                exchange: Some(Exchange {
                    question: "q".repeat(1000),
                    answer: "a".repeat(1000),
                }),
                now: NOW,
            },
        );
        let Admit::Run(next) = conversation.arrive(pending("U1", "again?"), NOW) else {
            panic!("the next message runs");
        };
        assert_eq!(next.context.len(), 1, "{:?}", next.context);
        let exchange = &next.context[0];
        assert_eq!(exchange.question.chars().count(), CONTEXT_TEXT_CHARS);
        assert_eq!(exchange.answer.chars().count(), CONTEXT_TEXT_CHARS);
    }

    /// Two conversations are two floors: a held turn in one never queues
    /// the other.
    #[test]
    fn local_turns_keys_conversations_apart() {
        let turns = LocalTurns::default();
        let admit = pollster::block_on(turns.arrive("slack:T:C1:1.1", pending("U1", "held?"), NOW))
            .expect("the first conversation admits");
        assert!(matches!(admit, Admit::Run(_)), "{admit:?}");
        let elsewhere =
            pollster::block_on(turns.arrive("slack:T:C2:2.2", pending("U1", "elsewhere?"), NOW))
                .expect("the other conversation admits");
        assert!(
            matches!(elsewhere, Admit::Run(_)),
            "another conversation's floor is free: {elsewhere:?}"
        );
        let queued =
            pollster::block_on(turns.arrive("slack:T:C1:1.1", pending("U1", "queued?"), NOW))
                .expect("the held conversation admits");
        assert_eq!(queued, Admit::Queued, "the held conversation still queues");
    }

    #[test]
    fn a_finish_for_an_unknown_conversation_is_refused_quietly() {
        let turns = LocalTurns::default();
        assert!(
            pollster::block_on(turns.finish("slack:T:C1:1.1", 7, None, NOW))
                .expect("a missing conversation is not an error")
                .is_none(),
        );
    }
}
