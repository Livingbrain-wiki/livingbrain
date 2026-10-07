//! `livingbrain-access`: the read-with-the-asker's-access permission model.
//!
//! Every memory item carries a [`Scope`] — [`Shared`](Scope::Shared),
//! [`Channel`](Scope::Channel) or [`User`](Scope::User) — and every read goes
//! through [`scopes_for`], which returns a [`ScopeSet`] whose constructor is
//! private, so no code can query memory without it.
//!
//! ## The access table
//!
//! | Asked in        | May draw on                                                              |
//! |-----------------|--------------------------------------------------------------------------|
//! | Public channel  | Shared memory only                                                       |
//! | Private channel | That channel's memory + shared (only if the asker is a member)          |
//! | DM              | The asker's personal memory + every private channel they are in + shared |
//!
//! Writes to an external connection go through [`connection_for_write`]:
//! [`Own`](ConnectionUse::Own) when the asker owns it, otherwise
//! [`NeedsApproval`](ConnectionUse::NeedsApproval), resolved with an
//! [`Approval`].

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;

/// A user id. [`UserId::new`] trusts its caller, so pass an id from a
/// verified source (Slack/Discord); it accepts an empty string. Ids arriving
/// as untrusted scope strings are validated at the [`Scope::from_str`]
/// boundary, which rejects an empty id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UserId(String);

impl UserId {
    #[must_use]
    pub fn new(id: String) -> Self {
        Self(id)
    }
}

impl fmt::Display for UserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A private-channel id. See [`UserId`] re: non-emptiness.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChannelId(String);

impl ChannelId {
    #[must_use]
    pub fn new(id: String) -> Self {
        Self(id)
    }
}

impl fmt::Display for ChannelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The scope a memory item carries. String forms: `shared`, `channel:<id>`,
/// `user:<id>`, round-tripping through [`Display`](Scope::fmt) / [`FromStr`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Scope {
    Shared,
    Channel(ChannelId),
    User(UserId),
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Scope::Shared => f.write_str("shared"),
            Scope::Channel(id) => write!(f, "channel:{id}"),
            Scope::User(id) => write!(f, "user:{id}"),
        }
    }
}

impl FromStr for Scope {
    type Err = ScopeParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "shared" {
            return Ok(Scope::Shared);
        }
        if let Some(rest) = s.strip_prefix("channel:") {
            return if rest.is_empty() {
                Err(ScopeParseError::EmptyId)
            } else {
                Ok(Scope::Channel(ChannelId::new(rest.to_owned())))
            };
        }
        if let Some(rest) = s.strip_prefix("user:") {
            return if rest.is_empty() {
                Err(ScopeParseError::EmptyId)
            } else {
                Ok(Scope::User(UserId::new(rest.to_owned())))
            };
        }
        Err(ScopeParseError::Malformed)
    }
}

/// A scope string that is not `shared`, `channel:<id>` or `user:<id>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeParseError {
    Malformed,
    EmptyId,
}

impl fmt::Display for ScopeParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ScopeParseError::Malformed => "malformed scope",
            ScopeParseError::EmptyId => "empty scope id",
        })
    }
}

impl std::error::Error for ScopeParseError {}

/// The set of scopes an asker may read, as decided by [`scopes_for`]. The
/// constructor is private — the only way to obtain one is [`scopes_for`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeSet {
    scopes: BTreeSet<Scope>,
}

impl ScopeSet {
    #[must_use]
    pub fn contains(&self, scope: &Scope) -> bool {
        self.scopes.contains(scope)
    }

    /// The string forms of every scope, for a future `WHERE scope IN (...)`
    /// query. The `BTreeSet` makes the order deterministic.
    pub fn scope_strings(&self) -> impl Iterator<Item = String> + '_ {
        self.scopes.iter().map(Scope::to_string)
    }
}

/// Where the asker asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    PublicChannel,
    PrivateChannel(ChannelId),
    Dm,
}

/// A read-only view of channel membership, so a future D1 store can implement
/// it without changing [`scopes_for`].
pub trait MembershipView {
    fn is_member(&self, channel: &ChannelId, user: &UserId) -> bool;
    fn private_channels_of(&self, user: &UserId) -> Vec<ChannelId>;
}

/// An in-memory snapshot of private-channel memberships, as a membership sync
/// would deliver.
///
/// Only *private* channels belong here. The model has no scope for a public
/// channel: asking in one yields [`Scope::Shared`], so public-channel memory
/// must be stored as [`Scope::Shared`] to be readable at all. A public channel
/// synced in here would instead grant its [`Scope::Channel`] scope to every DM
/// its members see.
#[derive(Debug, Clone, Default)]
pub struct ChannelMemberships {
    channels: BTreeMap<ChannelId, BTreeSet<UserId>>,
}

impl ChannelMemberships {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces the channel's private-member set — what a sync delivers: the
    /// full current member list, not a delta. A member dropped from `members`
    /// is gone from the next [`scopes_for`], since nothing here is cached
    /// per-asker. Pass private channels only; see [`ChannelMemberships`].
    pub fn sync_channel(&mut self, channel: ChannelId, members: impl IntoIterator<Item = UserId>) {
        self.channels.insert(channel, members.into_iter().collect());
    }
}

impl MembershipView for ChannelMemberships {
    fn is_member(&self, channel: &ChannelId, user: &UserId) -> bool {
        self.channels.get(channel).is_some_and(|m| m.contains(user))
    }
    fn private_channels_of(&self, user: &UserId) -> Vec<ChannelId> {
        self.channels
            .iter()
            .filter(|(_, members)| members.contains(user))
            .map(|(channel, _)| channel.clone())
            .collect()
    }
}

/// The one function every memory read goes through. See the [access
/// table](self#the-access-table).
#[must_use]
pub fn scopes_for<M: MembershipView + ?Sized>(
    asker: &UserId,
    location: Location,
    memberships: &M,
) -> ScopeSet {
    let mut scopes = BTreeSet::new();
    scopes.insert(Scope::Shared);
    match location {
        Location::PublicChannel => {}
        // Defensive: a non-member asking in a private channel is not in the
        // channel, so it does not grant it — they fall back to shared only,
        // as if they had asked in public.
        Location::PrivateChannel(channel) => {
            if memberships.is_member(&channel, asker) {
                scopes.insert(Scope::Channel(channel));
            }
        }
        // DM: the asker's own memory plus every private channel they are
        // currently in, each contributed by the membership view.
        Location::Dm => {
            scopes.insert(Scope::User(asker.clone()));
            for channel in memberships.private_channels_of(asker) {
                scopes.insert(Scope::Channel(channel));
            }
        }
    }
    ScopeSet { scopes }
}

/// One memory item: an id, its scope, and its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryItem {
    pub id: String,
    pub scope: Scope,
    pub content: String,
}

/// A recall answer: the cited item ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    pub citations: Vec<String>,
}

/// An in-memory index of [`MemoryItem`]s. The only read path is
/// [`recall`](Self::recall), which requires a [`ScopeSet`].
#[derive(Debug, Default)]
pub struct MemoryIndex {
    items: Vec<MemoryItem>,
}

impl MemoryIndex {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, item: MemoryItem) {
        self.items.push(item);
    }

    /// Recall items whose scope is in `scopes` and whose content contains
    /// `query`. The only memory read API — requires a [`ScopeSet`].
    #[must_use]
    pub fn recall(&self, scopes: &ScopeSet, query: &str) -> Answer {
        Answer {
            citations: self
                .items
                .iter()
                .filter(|item| scopes.contains(&item.scope) && item.content.contains(query))
                .map(|item| item.id.clone())
                .collect(),
        }
    }
}

/// Whether a write to an external connection may proceed, or needs approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionUse {
    /// The asker owns the connection — proceed.
    Own,
    /// The owner must approve.
    NeedsApproval { owner: UserId },
}

/// Decide whether a write to `connection_owner`'s connection may proceed.
#[must_use]
pub fn connection_for_write(asker: &UserId, connection_owner: &UserId) -> ConnectionUse {
    if asker == connection_owner {
        ConnectionUse::Own
    } else {
        ConnectionUse::NeedsApproval {
            owner: connection_owner.clone(),
        }
    }
}

/// An approval decision on a borrowed connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    Approve,
    Deny,
}

/// The resolved grant or refusal after an [`Approval`] is applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionGrant {
    Granted,
    Refused { owner: UserId },
}

impl ConnectionUse {
    /// Apply an [`Approval`]. [`Own`](ConnectionUse::Own) always grants — a
    /// user's own connection never needs a card, so the approval is ignored.
    #[must_use]
    pub fn resolve(self, approval: Approval) -> ConnectionGrant {
        match (self, approval) {
            (ConnectionUse::Own, _) => ConnectionGrant::Granted,
            (ConnectionUse::NeedsApproval { owner: _ }, Approval::Approve) => {
                ConnectionGrant::Granted
            }
            (ConnectionUse::NeedsApproval { owner }, Approval::Deny) => {
                ConnectionGrant::Refused { owner }
            }
        }
    }
}
