//! The chat-app port: normalised messages, scopes and the Slack and Discord
//! adapters. The agent loop and memory only ever see what this crate hands
//! them.
//!
//! A plain library crate — not a Cratefield Module (no tables, no routes), and
//! not registered in the venture. The Slack events route (#6), the agent loop
//! (#7) and memory (#8) are built on top of the types and the [`Channel`] trait
//! defined here.

#![forbid(unsafe_code)]

pub mod discord;
pub mod slack;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// Which chat app a message or identity came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Platform {
    Slack,
    Discord,
}

impl Platform {
    /// The lowercase slug used in scope keys: "slack" or "discord".
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Slack => "slack",
            Self::Discord => "discord",
        }
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What the source platform says about who can read a conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Visibility {
    /// Anyone in the platform can read it (Slack `#channel`, a guild channel
    /// `@everyone` can view).
    Public,
    /// Only the members the adapter can enumerate (Slack private group, a
    /// role-gated guild channel).
    Private,
    /// Two people only: a DM or a group DM.
    Direct,
}

/// One conversation on one platform.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Location {
    /// The platform the channel id belongs to.
    pub platform: Platform,
    /// The platform's own channel id (`C…`, a Discord snowflake).
    pub channel: String,
    /// Who can read it, as classified by the adapter.
    pub visibility: Visibility,
}

impl Location {
    /// `"{platform}:{channel}"` — namespaced so a Slack and a Discord id can
    /// never collide in a scope key.
    #[must_use]
    pub fn key(&self) -> String {
        format!("{}:{}", self.platform.as_str(), self.channel)
    }
}

/// A person on one platform. One person on Slack and Discord is two identities
/// linked (via [`Links`]) to one [`MemberId`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Identity {
    /// The platform the ids below belong to.
    pub platform: Platform,
    /// The workspace or guild: one person is a different id per team.
    pub team: String,
    /// The platform's user id — a *different* id space from [`MemberId`].
    pub user: String,
}

/// A normalised inbound message: everything the brain sees of one event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// Who sent it, on the platform it arrived from.
    pub author: Identity,
    /// The conversation. For a thread this is the parent channel.
    pub location: Location,
    /// The thread's platform id, when the message is inside one.
    pub thread: Option<String>,
    /// The platform's message id — what a reaction targets.
    pub id: String,
    /// The message body, mentions left unrendered.
    pub text: String,
    /// Whether the message names the brain, i.e. whether it is asking for a reply.
    pub mentions_bot: bool,
}

impl Message {
    /// `"{platform}:{team}:{channel}:{thread or id}"` — the per-conversation
    /// Durable Object key (#7).
    #[must_use]
    pub fn conversation_key(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.location.platform.as_str(),
            self.author.team,
            self.location.channel,
            self.thread.as_deref().unwrap_or(&self.id),
        )
    }
}

/// Where a memory item is stored.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Scope {
    /// Every member of the venture may read it.
    Shared,
    /// One conversation, keyed by [`Location::key`], for its members only.
    Channel(String),
    /// One member's own items, keyed by [`MemberId`].
    User(String),
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Shared => f.write_str("shared"),
            Self::Channel(key) => write!(f, "channel:{key}"),
            Self::User(member) => write!(f, "user:{member}"),
        }
    }
}

/// A member id: one person across platforms (the venture's own primary key).
pub type MemberId = String;

/// Where an item said *here* is stored.
///
/// Public → shared; private → that channel; direct → personal.
#[must_use]
pub fn scope_of(location: &Location, author: &MemberId) -> Scope {
    match location.visibility {
        Visibility::Public => Scope::Shared,
        Visibility::Private => Scope::Channel(location.key()),
        Visibility::Direct => Scope::User(author.clone()),
    }
}

/// The one read-permission function (#8). Nothing queries memory without it.
///
/// `private_channels` is the asker's *readable* set — the private
/// conversations they are a member of, supplied by the caller from adapter
/// `viewers` lookups. It is the only membership evidence this function has, so
/// a `channel:<key>` scope is granted on a private location only when that
/// same location is in the set; a non-member asking about a private channel
/// gets `{shared}` alone. Non-Private entries in the set are ignored.
///
/// - Public channel → `{shared}`.
/// - Private channel → `{shared}` ∪ `channel:<key>` if `location ∈ private_channels`.
/// - DM → `{shared, user:<asker>}` ∪ `channel:<key>` for every private channel
///   the asker is in.
///
/// It deliberately does not look at `platform`.
#[must_use]
pub fn scopes_for(
    asker: &MemberId,
    location: &Location,
    private_channels: &[Location],
) -> BTreeSet<Scope> {
    let mut scopes = BTreeSet::from([Scope::Shared]);
    match location.visibility {
        Visibility::Public => {}
        Visibility::Private => {
            // Membership is required: the asker must appear in their own
            // readable set for this exact conversation.
            if private_channels.contains(location) {
                scopes.insert(Scope::Channel(location.key()));
            }
        }
        Visibility::Direct => {
            scopes.insert(Scope::User(asker.clone()));
            for channel in private_channels {
                if channel.visibility == Visibility::Private {
                    scopes.insert(Scope::Channel(channel.key()));
                }
            }
        }
    }
    scopes
}

/// Identity linking: one person on Slack and Discord maps to one member.
///
/// In-memory; persistence comes with linked connections (#71). Mail, CLI and
/// coding agents authenticate as the member directly, so they never need a
/// link entry here.
#[derive(Debug, Default)]
pub struct Links {
    map: BTreeMap<Identity, MemberId>,
}

impl Links {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Links `identity` to `member`, replacing any prior link.
    pub fn link(&mut self, identity: Identity, member: MemberId) {
        self.map.insert(identity, member);
    }

    /// The member `identity` is linked to, if any.
    #[must_use]
    pub fn member(&self, identity: &Identity) -> Option<&MemberId> {
        self.map.get(identity)
    }
}

/// One source backing a factual claim in a reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Citation {
    pub title: String,
    pub url: String,
}

/// The brain's answer: text plus the sources it rests on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub text: String,
    pub citations: Vec<Citation>,
}

/// An approve/deny card the brain posts before borrowing a teammate's
/// connection. The buttons carry `approve:<id>` / `deny:<id>` action ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalCard {
    pub id: String,
    pub prompt: String,
}

/// A rendered body plus where it goes. Returning this instead of a bare
/// `Value` keeps platform knowledge (the API path, the target channel) inside
/// the adapter, so a route can POST it without matching on platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outbound {
    /// The adapter that produced this; a route can assert on it.
    pub platform: Platform,
    /// API method or path the adapter wants called, e.g. `chat.postMessage`.
    pub method: String,
    /// The channel the body goes to.
    pub channel: String,
    /// Platform thread handle, when the reply belongs in a thread.
    pub thread: Option<String>,
    /// The request body, citations and buttons rendered natively.
    pub body: serde_json::Value,
}

/// The port: one adapter per platform. HTTP sending and roster fetching are
/// the adapters' routes (#6, #56); the [`Outbound`] values returned here are
/// what gets POSTed.
pub trait Channel {
    /// Which platform this adapter speaks.
    const PLATFORM: Platform;
    /// A verified inbound event (signature already checked by the route).
    type Event;
    /// Who-can-see data for one conversation (Slack: its member list; Discord:
    /// guild roles, overwrites and members).
    type Roster;
    /// `None` for events the brain ignores (its own or other bots' messages,
    /// edits, joins).
    fn receive(&self, event: &Self::Event) -> Option<Message>;
    /// The platform's post body for a reply in `to`'s conversation, citations
    /// rendered natively.
    fn reply(&self, to: &Message, reply: &Reply) -> Outbound;
    /// The body for adding `emoji` to `to`; the target is the message itself.
    fn react(&self, to: &Message, emoji: &str) -> Outbound;
    /// The body for `card` in `to`'s conversation.
    fn approval(&self, to: &Message, card: &ApprovalCard) -> Outbound;
    /// Platform user ids that can read the conversation.
    fn viewers(&self, roster: &Self::Roster) -> BTreeSet<String>;
}
