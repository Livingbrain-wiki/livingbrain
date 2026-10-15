//! Proposals: things a person might want to do about what the radar found,
//! and the type-level gate that keeps them from happening on their own.
//!
//! # The gate
//!
//! [`Pending`] is the only thing [`Radar::run`](crate::Radar::run) can
//! produce, and it has no public constructor other than the ones in this
//! module. [`Pending::approve`] consumes it with a [`PersonId`] and
//! returns [`Approved`], whose fields are private and whose only
//! constructor is that method. A [`ProposalSink`] — the GitHub App that
//! opens a draft issue, the Colonizer that starts a colony — takes an
//! [`Approved`] and nothing else.
//!
//! So there is no code path from a run to a dispatch. Not "the nightly pass
//! does not call the sink today", which is a promise about a caller; the
//! type system refuses the call. A future change that wanted one would have
//! to write `Approved { .. }` in this module, and that is a diff a
//! reviewer sees.
//!
//! `PersonId` is a string newtype, not a membership check: this crate does
//! not know who a person is. The pages and workspaces modules do, and it is
//! their job to decide whether the id in [`Pending::approve`] is a person
//! who may. That is a deliberate boundary — a radar that could look up its
//! own authoriser would be a second, weaker access model.

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PersonId(String);

impl PersonId {
    /// Wraps an id the caller has already authenticated.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What kind of thing a proposal would do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Try the work: read it properly, prototype it, spike it.
    Experiment,
    /// Fix it: an advisory for a version this workspace locks.
    Remediation,
}

impl Kind {
    /// The stable wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Experiment => "experiment",
            Self::Remediation => "remediation",
        }
    }
}

/// A proposal a person has not yet approved.
///
/// There is no `Pending::new` for callers and no `pub` fields: the only
/// way to hold one is to have received it from a run, and the only way to
/// move it on is [`Pending::approve`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    kind: Kind,
    pub summary: String,
    /// The source URLs this proposal came from. Never empty: a proposal
    /// with no source is not reviewable, and a run that could not cite one
    /// does not make the proposal.
    pub sources: Vec<String>,
}

impl Pending {
    fn new(kind: Kind, summary: String, sources: Vec<String>) -> Self {
        Self {
            kind,
            summary,
            sources,
        }
    }

    #[must_use]
    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// A person has read this and says yes. Consumes the pending proposal:
    /// there is exactly one approval, and approving twice is not a thing
    /// that can be expressed.
    #[must_use]
    pub fn approve(self, approver: PersonId) -> Approved {
        Approved {
            kind: self.kind,
            summary: self.summary,
            sources: self.sources,
            approver,
        }
    }
}

/// A proposal a person approved. Constructible only by
/// [`Pending::approve`] — there is no other way to hold this value, and
/// [`ProposalSink::dispatch`] takes nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approved {
    kind: Kind,
    pub summary: String,
    pub sources: Vec<String>,
    pub approver: PersonId,
}

impl Approved {
    /// What this proposal would do.
    #[must_use]
    pub fn kind(&self) -> Kind {
        self.kind
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkError {
    pub reason: SinkReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkReason {
    NotConfigured,
    Rejected,
}

impl std::fmt::Display for SinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self.reason {
            SinkReason::NotConfigured => "the sink is not configured",
            SinkReason::Rejected => "the sink rejected the proposal",
        })
    }
}

impl std::error::Error for SinkError {}

/// Somewhere an **approved** proposal can go: the GitHub App that opens a
/// draft issue, the Colonizer that starts a colony.
///
/// The signature is the gate: `dispatch` takes an [`Approved`], and
/// [`Approved`] has one constructor, so nothing a run produced can reach
/// here. A sink that is wrong for a workspace simply is not wired.
pub trait ProposalSink {
    /// Takes one approved proposal. Called by a human's action, never by
    /// [`Radar::run`](crate::Radar::run) — this crate has no code that
    /// does so.
    ///
    /// # Errors
    ///
    /// [`SinkError`] when the sink cannot take it.
    fn dispatch(&self, proposal: Approved) -> Result<(), SinkError>;
}

#[must_use]
pub fn for_read(item_id: String) -> Pending {
    Pending::new(
        Kind::Experiment,
        format!("Read `{item_id}` properly and decide whether to try it"),
        vec![item_id],
    )
}

#[must_use]
pub fn for_advisory(advisory: &crate::advisory::Advisory) -> Pending {
    Pending::new(
        Kind::Remediation,
        format!(
            "{} affects `{}` {}: review the fix",
            advisory.id, advisory.package, advisory.vulnerable
        ),
        vec![advisory.url.clone()],
    )
}
