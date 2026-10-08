//! Anonymous usage data: how much Living Brain was used, never by whom.
//!
//! One [`UsageBatch`] per on-period. Every number in it is a count or a
//! duration, every duration is bucketed by [`bucket_latency`], and every
//! non-numeric value is a variant of a closed enum compiled into this file.
//! There is no field a caller can fill with a sentence, and the one string
//! that is not a vocabulary word — `usage_id` — is a random 16-character
//! value the *caller* mints from its own `IdGen`, precisely so this crate
//! carries no randomness and no id-generation dependency.
//!
//! # The contract
//!
//! A batch is a fixed set of keys, [`UsageBatch::new`] is the only way to
//! make one, and the four maps inside it are keyed by [`Count`], [`Latency`],
//! [`OutcomeMetric`] and [`ErrorKind`] — enums that are `Copy` and have no
//! `String` in them. A value the type system cannot express cannot be sent,
//! which is why `tests/anonymity.rs` walks the serialised payload and asserts
//! that every scalar is an integer, a closed vocabulary word, or one of the
//! two validated ids.
//!
//! # Non-goals
//!
//! Not a analytics SDK: there is no queue, no retry, no batching window, no
//! transport. Where a batch goes is [`USAGE_ENDPOINT`] and a caller's HTTP
//! client; this crate builds the value and the one honest announcement about
//! it ([`Telemetry::notice`]). Not a way to sample or weight installs: the
//! on-period is the sender's business, not this crate's.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The version of the wire payload this build produces.
///
/// A receiver that sees a number it does not know can reject the batch
/// instead of guessing at the meaning of a key. Bumping it is a deliberate
/// act: the count of keys in [`UsageBatch`] is part of the promise.
pub const USAGE_PAYLOAD_VERSION: u32 = 1;

/// Where a batch is posted. A constant rather than a setting: the question a
/// self-hoster asks first is *where does my data go*, and the answer should
/// be one line they can read.
pub const USAGE_ENDPOINT: &str = "https://telemetry.livingbrain.wiki/v1/telemetry/events";

/// How long a [`UsageId`] is, in characters. Fixed, not a maximum: a
/// fixed-length id cannot carry a fingerprint, because there is nowhere in it
/// to put a name.
pub const USAGE_ID_LEN: usize = 16;

/// The host a batch came from, as the map and the dashboards count it.
///
/// [`Platform::Other`] is the honest bucket for anything this enum does not
/// name, and it is a real bucket rather than an `Unknown`: an unrecognised
/// platform is a fact about a platform, and dropping it would make the total
/// number of installs disagree with the number of known platforms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    /// Linux, native or in a Worker.
    Linux,
    /// macOS.
    Macos,
    /// Windows.
    Windows,
    /// A browser build (`wasm32-unknown-unknown`).
    Wasm,
    /// Anything this build does not recognise.
    Other,
}

impl Platform {
    /// Every platform, in declaration order.
    ///
    /// Exists so a consumer that needs the *whole* vocabulary — a test that
    /// walks a serialised payload, a CLI that renders a picker — iterates the
    /// enum instead of writing a second list that can fall behind it. Adding a
    /// variant and forgetting the list is the failure this removes.
    pub const ALL: &'static [Self] = &[
        Self::Linux,
        Self::Macos,
        Self::Windows,
        Self::Wasm,
        Self::Other,
    ];

    /// The wire name of this platform, the same string [`Self`] serialises
    /// to. The two are asserted equal in this module's tests, so a rename on
    /// one side is a failing test rather than a silently new vocabulary word.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Linux => "linux",
            Self::Macos => "macos",
            Self::Windows => "windows",
            Self::Wasm => "wasm",
            Self::Other => "other",
        }
    }
}

/// A duration, rounded to one of five buckets.
///
/// The buckets are chosen so that the interesting boundary — under a second,
/// which is whether an interaction feels instant — is a boundary, and so that
/// a slow call cannot be told from a fast one by the shape of its bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum LatencyBucket {
    /// Under 50 ms.
    #[serde(rename = "lt_50ms")]
    Under50ms,
    /// From 50 ms up to but not including 250 ms.
    #[serde(rename = "lt_250ms")]
    Under250ms,
    /// From 250 ms up to but not including 1 s.
    #[serde(rename = "lt_1s")]
    Under1s,
    /// From 1 s up to but not including 5 s.
    #[serde(rename = "lt_5s")]
    Under5s,
    /// 5 s and over. Open-ended on purpose: the exact figure is the interesting
    /// part, and this payload is not where it can go.
    #[serde(rename = "gt_5s")]
    Over5s,
}

impl LatencyBucket {
    /// Every bucket, in declaration order. See [`Platform::ALL`] for why.
    pub const ALL: &'static [Self] = &[
        Self::Under50ms,
        Self::Under250ms,
        Self::Under1s,
        Self::Under5s,
        Self::Over5s,
    ];

    /// The wire name of this bucket.
    ///
    /// The `serde` attributes above are per-variant renames rather than
    /// `rename_all`, because `rename_all` would give `under50ms` and
    /// `under1s` — different words from these, and words that read worse in a
    /// dashboard query. Deriving is still the implementation; only the
    /// spelling is stated.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Under50ms => "lt_50ms",
            Self::Under250ms => "lt_250ms",
            Self::Under1s => "lt_1s",
            Self::Under5s => "lt_5s",
            Self::Over5s => "gt_5s",
        }
    }
}

/// A thing whose magnitude is worth counting.
///
/// Closed because the batch is a schema: a new `Count` is a new word on the
/// wire, and adding one is a decision about what an install may reveal, not
/// a refactor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Count {
    /// Workspaces on this install.
    Workspaces,
    /// Pages on this install.
    Pages,
    /// Brain turns: one per ask of the brain.
    BrainTurns,
    /// MCP tool calls.
    McpCalls,
}

impl Count {
    /// Every counter, in declaration order. See [`Platform::ALL`] for why.
    pub const ALL: &'static [Self] = &[
        Self::Workspaces,
        Self::Pages,
        Self::BrainTurns,
        Self::McpCalls,
    ];

    /// The wire name of this counter.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Workspaces => "workspaces",
            Self::Pages => "pages",
            Self::BrainTurns => "brain_turns",
            Self::McpCalls => "mcp_calls",
        }
    }
}

/// What something is worth counting as: the last outcome of a unit of work.
///
/// An `Outcome`, not a number, because "how many brain turns failed" is a
/// fact about the software and "which page failed" is a fact about a person.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// It worked.
    Success,
    /// It did not.
    Failure,
    /// It was not attempted — off, not configured, or out of scope.
    Skipped,
}

impl Outcome {
    /// Every outcome, in declaration order. See [`Platform::ALL`] for why.
    pub const ALL: &'static [Self] = &[Self::Success, Self::Failure, Self::Skipped];

    /// The wire name of this outcome.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Skipped => "skipped",
        }
    }
}

/// Which unit of work an [`Outcome`] belongs to. Kept apart from [`Outcome`]
/// so the batch can say "the last brain turn failed" without a count and
/// without a second schema for the same three words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeMetric {
    /// The nightly evolve pass.
    NightlyEvolve,
    /// One ask of the brain.
    BrainTurn,
    /// One MCP tool call.
    McpCall,
}

impl OutcomeMetric {
    /// Every metric, in declaration order. See [`Platform::ALL`] for why.
    pub const ALL: &'static [Self] = &[Self::NightlyEvolve, Self::BrainTurn, Self::McpCall];

    /// The wire name of this metric.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NightlyEvolve => "nightly_evolve",
            Self::BrainTurn => "brain_turn",
            Self::McpCall => "mcp_call",
        }
    }
}

/// Which duration is being measured.
///
/// The enum has one variant today and that is the point: measuring a new
/// latency is a deliberate addition to the schema, and because the duration
/// is bucketed before it is stored, adding one cannot widen what a bucket
/// reveals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Latency {
    /// How long a search takes.
    Search,
}

impl Latency {
    /// Every latency, in declaration order. See [`Platform::ALL`] for why.
    ///
    /// One variant today. It is written as a slice rather than a bare value so
    /// the second latency is an addition here and nowhere else.
    pub const ALL: &'static [Self] = &[Self::Search];

    /// The wire name of this latency.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Search => "search",
        }
    }
}

/// How a failure was classified.
///
/// Closed so that a new kind is a code change someone reviewed: an open error
/// taxonomy is where "the model returned an error mentioning the user's
/// question" would arrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// The caller's own configuration — no key, no workspace, nothing to do.
    Config,
    /// A dependency answered, and answered badly.
    Upstream,
    /// It did not answer in time.
    Timeout,
    /// The answer could not be turned into what was asked for.
    Decode,
    /// The asker was not allowed to.
    Permission,
    /// A bug, which is the bucket every unmapped failure falls into.
    Internal,
}

impl ErrorKind {
    /// Every error kind, in declaration order. See [`Platform::ALL`] for why.
    pub const ALL: &'static [Self] = &[
        Self::Config,
        Self::Upstream,
        Self::Timeout,
        Self::Decode,
        Self::Permission,
        Self::Internal,
    ];

    /// The wire name of this error kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Upstream => "upstream",
            Self::Timeout => "timeout",
            Self::Decode => "decode",
            Self::Permission => "permission",
            Self::Internal => "internal",
        }
    }
}

/// Check a value against the shared opaque-id rule: non-empty, at most
/// [`MAX_OPAQUE_ID`] characters, and drawn from `[A-Za-z0-9._:-]`.
///
/// The reason strings are `&'static str` rather than formatted, so an error
/// carries no copy of the value it rejected — the same rule `livingbrain-redact`
/// follows for a finding. `charset_reason` says which field is being refused,
/// for the reader doing the debugging.
pub(crate) fn validate_opaque_id(
    raw: &str,
    charset_reason: &'static str,
) -> Result<(), &'static str> {
    if raw.is_empty() {
        return Err("an id is never empty");
    }
    if raw.chars().count() > MAX_OPAQUE_ID {
        return Err("an id is at most 128 characters");
    }
    if !raw
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
    {
        return Err(charset_reason);
    }
    Ok(())
}

/// The longest an opaque id may be, anywhere in this crate.
pub(crate) const MAX_OPAQUE_ID: usize = 128;

/// The longest a [`Version`] may be, in characters.
const MAX_VERSION_LEN: usize = 32;

/// The only words a [`Version`]'s prerelease suffix may use.
///
/// Closed rather than "anything alphabetic" because a `git describe` output
/// ends in a commit count and a hash — `0.1.0-3-gabc1234-dirty` — and a build
/// from a named branch ends in the branch's name. Both are the same shape as a
/// prerelease suffix, and both carry a person's checkout onto the wire.
const VERSION_PRERELEASE_WORDS: [&str; 4] = ["rc", "alpha", "beta", "dev"];

/// Check a value against the version grammar documented on [`Version`].
///
/// `major.minor.patch`, each part one or more ASCII digits with no leading
/// zero unless the number is exactly `0`; then, optionally, exactly one
/// `-<word>.<n>` where `word` is one of [`VERSION_PRERELEASE_WORDS`] and `n` is
/// one or more ASCII digits. Nothing else: no second hyphen group, no `.`, `-`,
/// `_` or space anywhere else, no trailing characters, and at most
/// [`MAX_VERSION_LEN`] characters.
///
/// The reason strings are `&'static str` rather than formatted, for the reason
/// [`validate_opaque_id`]'s are: a rejected value is not a thing to echo back.
pub(crate) fn validate_version(raw: &str) -> Result<(), &'static str> {
    if raw.is_empty() {
        return Err("a version is never empty");
    }
    if raw.chars().count() > MAX_VERSION_LEN {
        return Err("a version is at most 32 characters");
    }
    // At most one hyphen, and it introduces the prerelease suffix. Taking the
    // *first* hyphen and refusing a second is what rejects
    // `0.1.0-3-gabc1234-dirty` and `2026.10.08-alice-laptop`: both read as a
    // version number followed by a second, hyphen-separated fact about a
    // person's machine.
    let (core, prerelease) = match raw.split_once('-') {
        Some((core, suffix)) => {
            if suffix.contains('-') {
                return Err(
                    "a version has at most one hyphen, and it introduces the prerelease suffix",
                );
            }
            (core, Some(suffix))
        }
        None => (raw, None),
    };
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3 {
        return Err("a version is major.minor.patch: exactly three numbers, separated by dots");
    }
    for part in &parts {
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return Err("major, minor and patch are each one or more ASCII digits");
        }
        if part.len() > 1 && part.starts_with('0') {
            return Err("major, minor and patch carry no leading zero unless the number is 0");
        }
    }
    if let Some(suffix) = prerelease {
        let Some((word, number)) = suffix.split_once('.') else {
            return Err("a prerelease suffix is -rc.N, -alpha.N, -beta.N or -dev.N");
        };
        if !VERSION_PRERELEASE_WORDS.contains(&word) {
            return Err(
                "the only prerelease words are rc, alpha, beta and dev; a build or a branch is not a version",
            );
        }
        if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
            return Err("the number after a prerelease word is one or more ASCII digits");
        }
    }
    Ok(())
}

/// Check a value against the usage-id rule: exactly [`USAGE_ID_LEN`] characters
/// of `[a-z0-9]`.
pub(crate) fn validate_usage_id(raw: &str) -> Result<(), &'static str> {
    if raw.len() != USAGE_ID_LEN {
        return Err("a usage id is exactly 16 characters");
    }
    if !raw
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    {
        return Err("a usage id is lowercase letters and digits only, with no hyphen");
    }
    Ok(())
}

/// Why a [`UsageId`] was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidUsageId {
    /// What was wrong, in one sentence. Never the value: an id that was
    /// rejected is not a thing to echo back into a log.
    pub reason: &'static str,
}

impl fmt::Display for InvalidUsageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.reason)
    }
}

impl std::error::Error for InvalidUsageId {}

/// A random id for one on-period, shared by every batch in that period.
///
/// The point of the type is the field it hides: `UsageId` has exactly one way
/// to exist and that way is through a validator, so the string on the wire is
/// always sixteen lowercase characters. Nothing derived from the machine, the
/// workspace or the person is in it, and nothing in it is derived from
/// anything either — the caller's `IdGen` mints it and this crate never sees
/// a source for it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct UsageId(String);

impl UsageId {
    /// Take a caller's random value, if it is one.
    ///
    /// # Errors
    ///
    /// [`InvalidUsageId`] when `raw` is not exactly [`USAGE_ID_LEN`]
    /// characters of `[a-z0-9]`. Uppercase, hyphens and a UUID are all
    /// refused: a UUID has hyphens, and an id with a canonical shape is an id
    /// that can be correlated with another log.
    pub fn new(raw: impl Into<String>) -> Result<Self, InvalidUsageId> {
        let raw = raw.into();
        validate_usage_id(&raw).map_err(|reason| InvalidUsageId { reason })?;
        Ok(Self(raw))
    }

    /// The id as the wire sees it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for UsageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<&str> for UsageId {
    type Error = InvalidUsageId;

    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        Self::new(raw)
    }
}

impl TryFrom<String> for UsageId {
    type Error = InvalidUsageId;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::new(raw)
    }
}

/// Why a [`Version`] was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidVersion {
    /// What was wrong, in one sentence. Never the value.
    pub reason: &'static str,
}

impl fmt::Display for InvalidVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.reason)
    }
}

impl std::error::Error for InvalidVersion {}

/// The sender's own crate version, as the dashboards label an install.
///
/// # The grammar
///
/// ```text
/// major "." minor "." patch [ "-" word "." number ]
/// ```
///
/// - `major`, `minor`, `patch`: one or more ASCII digits, with no leading
///   zero unless the number is exactly `0`. `0.1.0` and `1.98.1` are versions;
///   `01.1.0` is a typo someone made, not a version.
/// - `word`: one of exactly four — `rc`, `alpha`, `beta`, `dev`. Nothing else,
///   and no second hyphen group, so `0.1.0-rc.1` and `0.2.0-beta.3` are
///   versions and `0.1.0-dirty` is not.
/// - `number`: one or more ASCII digits.
/// - At most 32 characters, and no `.`, `-`, `_`, space or any other character
///   anywhere outside the places above. A leading `v` is not part of the
///   grammar either: `v0.1.0` is refused.
///
/// # Why the grammar is that tight
///
/// A looser rule — "starts with a digit, letters and digits and dots and
/// hyphens only" — is still free text, because a person's checkout and this
/// crate's released versions are the same characters in the same order. The
/// values below are what a real build pipeline produces, and each one carries
/// somebody's machine onto the wire while looking like a version number:
///
/// | Value | What it actually is |
/// |---|---|
/// | `0.1.0-3-gabc1234-dirty` | `git describe`: a commit hash and a dirty flag |
/// | `2026.10.08-alice-laptop` | a build stamped with a person's name |
/// | `1jane-doe-macbook-2026` | the same, spelled differently |
/// | `1workspace-acme-corp` | a build stamped with a workspace |
///
/// A `git describe` output or a named build branch is refused **because** it
/// can carry a person's name or a commit hash, not because it is untidy. The
/// grammar is therefore the *shape of a released version*: if a value has
/// anything more to say than `major.minor.patch`, it is not a version and this
/// field is not the place for it.
///
/// Deliberately not `semver::Version`: the harness has no semver dependency
/// and adding one to say "digits and dots" would be the wrong trade. This is
/// also stricter than semver in the two places that matter here — semver
/// accepts any dot-separated identifier as a prerelease tag and any
/// dot-separated identifier as build metadata, and either can be a name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct Version(String);

impl Version {
    /// Take a version string, if it is shaped like one.
    ///
    /// # Errors
    ///
    /// [`InvalidVersion`] when `raw` does not match the grammar on [`Version`].
    /// The reason names the rule that was broken, so a caller reporting a
    /// failed build can be told what to fix without the value being echoed
    /// back into the message.
    pub fn new(raw: &str) -> Result<Self, InvalidVersion> {
        validate_version(raw).map_err(|reason| InvalidVersion { reason })?;
        Ok(Self(raw.to_owned()))
    }

    /// The version as the wire sees it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The whole of what a [`UsageBatch`] puts on the wire.
///
/// Public fields because this is a DTO with a fixed shape and no invariants
/// beyond the types themselves: every field is either an integer, a closed
/// enum, a validated id, or a map keyed by a closed enum. There is no
/// invariant for a setter to protect, and no way to add a field without
/// changing what the batch means.
///
/// Built through [`UsageBatch::new`], which fills the maps empty and stamps
/// [`USAGE_PAYLOAD_VERSION`]. The `*_by` methods add to a counter rather than
/// set it, so a caller that counts per request does not have to know whether
/// the period already had one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UsageBatch {
    /// Always [`USAGE_PAYLOAD_VERSION`]. A receiver checks it before it reads
    /// anything else.
    pub payload_version: u32,
    /// The random id for this on-period.
    pub usage_id: UsageId,
    /// The sender's own version.
    pub version: Version,
    /// Which host this was.
    pub platform: Platform,
    /// When the on-period ends, in seconds since the Unix epoch. A timestamp,
    /// not an interval, so two batches can be ordered without trusting either
    /// sender's clock arithmetic.
    pub period_end_unix: u64,
    /// Magnitudes, keyed by what is being counted.
    pub counters: BTreeMap<Count, u64>,
    /// Bucketed durations, keyed by which latency was measured.
    pub latencies: BTreeMap<Latency, LatencyBucket>,
    /// The last outcome of each unit of work.
    pub outcomes: BTreeMap<OutcomeMetric, Outcome>,
    /// Failures, classified and counted.
    pub errors: BTreeMap<ErrorKind, u64>,
}

impl UsageBatch {
    /// A batch for one on-period, with every map empty.
    ///
    /// An empty map is an absent fact, not a zero: the receiver can tell "no
    /// search ran this period" from "no search was measured", which a
    /// pre-filled zero in every bucket could not.
    pub fn new(
        usage_id: UsageId,
        version: Version,
        platform: Platform,
        period_end_unix: u64,
    ) -> Self {
        Self {
            payload_version: USAGE_PAYLOAD_VERSION,
            usage_id,
            version,
            platform,
            period_end_unix,
            counters: BTreeMap::new(),
            latencies: BTreeMap::new(),
            outcomes: BTreeMap::new(),
            errors: BTreeMap::new(),
        }
    }

    /// Add `by` to a counter, creating it at zero first.
    ///
    /// Saturating, not `+=`: a debug build panics on overflow and a release
    /// build wraps silently, so the count a caller reads would depend on how it
    /// was compiled. A counter that hits `u64::MAX` is already nonsense, and
    /// pinning it there is the honest reading of "more than we can count".
    pub fn count_by(&mut self, what: Count, by: u64) -> &mut Self {
        let slot = self.counters.entry(what).or_default();
        *slot = slot.saturating_add(by);
        self
    }

    /// Record a duration. Bucketed here, so no caller can pass a number that
    /// would later be recognised.
    pub fn observe_latency(&mut self, which: Latency, took: Duration) -> &mut Self {
        self.latencies.insert(which, bucket_latency(took));
        self
    }

    /// Record the outcome of a unit of work.
    pub fn record_outcome(&mut self, metric: OutcomeMetric, outcome: Outcome) -> &mut Self {
        self.outcomes.insert(metric, outcome);
        self
    }

    /// Count one classified failure.
    ///
    /// Saturating, for the same reason [`Self::count_by`] is: `+=` panics in a
    /// debug build and wraps in a release one, so the number that reaches the
    /// wire would depend on the build profile.
    pub fn count_error(&mut self, kind: ErrorKind, by: u64) -> &mut Self {
        let slot = self.errors.entry(kind).or_default();
        *slot = slot.saturating_add(by);
        self
    }
}

/// Bucket a duration into the vocabulary a batch may contain.
///
/// The convention is **half-open, lower bound inclusive**: a bucket covers
/// `[lower, next lower)`, so 50 ms is `lt_250ms` and 250 ms is `lt_1s`. There
/// is no gap and no double count, which is the only reason two sends of the
/// same number can be compared.
#[must_use]
pub fn bucket_latency(d: Duration) -> LatencyBucket {
    if d < Duration::from_millis(50) {
        LatencyBucket::Under50ms
    } else if d < Duration::from_millis(250) {
        LatencyBucket::Under250ms
    } else if d < Duration::from_secs(1) {
        LatencyBucket::Under1s
    } else if d < Duration::from_secs(5) {
        LatencyBucket::Under5s
    } else {
        LatencyBucket::Over5s
    }
}

/// A whole number reduced to a bucket index.
///
/// Lives here, next to [`bucket_latency`], because it is the same decision:
/// the payload carries the bucket and never the number.
/// [`crate::live_map::PagesBucket`] in
/// `live_map` is this over a page count, named for what it counts, so the
/// wire type says what the number was instead of saying "index 2".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Bucket(u8);

impl Bucket {
    /// The bucket `n` falls in.
    #[must_use]
    pub const fn of(n: u64) -> Self {
        if n < 10 {
            Self(0)
        } else if n < 100 {
            Self(1)
        } else {
            Self(2)
        }
    }

    /// The bucket index, which is what goes on the wire.
    #[must_use]
    pub const fn index(self) -> u8 {
        self.0
    }
}

/// Whether anonymous usage data is being collected.
///
/// [`On`](Self::On) is the default, and that is the part a reader of the
/// license deserves to notice: Living Brain sends counted usage out of the
/// box, and the two switches below are how you stop it. What it sends is
/// shown once per on-period by [`Telemetry::notice`], verbatim, because the
/// only honest way to default to on is to make the default visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Telemetry {
    /// Collect and send.
    #[default]
    On,
    /// Collect nothing and send nothing.
    Off,
}

impl Telemetry {
    /// Decide whether telemetry is on, and in what order.
    ///
    /// The environment always wins over the stored preference: a var set for
    /// one run is a deliberate act, and it should beat a preference written
    /// months ago. `0`, `false`, `off` and `no` force [`Off`](Self::Off);
    /// `1`, `true`, `on` and `yes` force [`On`](Self::On). Matching is
    /// case-insensitive and surrounding space is ignored. Anything else,
    /// including an unset var and an empty string, returns `stored` — a typo
    /// must not silently switch telemetry on or off.
    #[must_use]
    pub fn resolve(stored: Telemetry, env: Option<&str>) -> Telemetry {
        match env.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            Some("0" | "false" | "off" | "no") => Self::Off,
            Some("1" | "true" | "on" | "yes") => Self::On,
            _ => stored,
        }
    }

    /// Whether telemetry is on.
    #[must_use]
    pub fn is_on(self) -> bool {
        matches!(self, Self::On)
    }

    /// The one announcement, printed once per on-period.
    ///
    /// This is the honesty contract, so it is spelled out rather than
    /// assembled from parts: the message names both off switches *verbatim* —
    /// the command a reader can paste and the environment variable they can
    /// set — and then prints the batch exactly as it will be sent, pretty, so
    /// a reader can see every key, every vocabulary word and every number
    /// that leaves their machine before any of it does.
    ///
    /// `body` is the sender's own line or two about the period (when it
    /// started, what it covered); it is included verbatim, so a caller cannot
    /// make the announcement say something the batch does not show.
    #[must_use]
    pub fn notice(batch: &UsageBatch, body: &str) -> String {
        // Serialising this struct cannot fail — every field is an integer, a
        // unit enum, a validated id or a map of those. The fallback keeps
        // `notice` infallible so it can be called from anywhere, including
        // from a destructor.
        let json = serde_json::to_string_pretty(batch).unwrap_or_else(|_| "{}".to_owned());
        format!(
            "Living Brain sends anonymous usage data once per period: counts, \
             bucketed durations and outcomes, with a random id and nothing that \
             names a workspace, a person or a page.\n\n\
             {body}\n\n\
             turn it off for good:  livingbrain telemetry off\n\
             turn it off for a run: LIVINGBRAIN_TELEMETRY=0\n\n\
             exactly what this batch says, and nothing else does:\n{json}\n"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BTreeMap, Bucket, Count, ErrorKind, InvalidUsageId, Latency, LatencyBucket, Outcome,
        OutcomeMetric, Platform, Telemetry, USAGE_ID_LEN, USAGE_PAYLOAD_VERSION, UsageBatch,
        UsageId, Version, bucket_latency, validate_usage_id,
    };
    use std::collections::BTreeSet;
    use std::time::Duration;

    /// Every `as_str` is the string `serde` produced, for every variant. The
    /// anonymity test depends on both agreeing; this is where that is caught
    /// when a variant is renamed on one side only.
    #[test]
    fn vocabulary_matches_serde() {
        let platforms = [
            (Platform::Linux, "linux"),
            (Platform::Macos, "macos"),
            (Platform::Windows, "windows"),
            (Platform::Wasm, "wasm"),
            (Platform::Other, "other"),
        ];
        for (value, text) in platforms {
            assert_eq!(value.as_str(), text);
            assert_eq!(
                serde_json::to_string(&value).unwrap(),
                format!("\"{text}\"")
            );
        }
        let buckets = [
            (LatencyBucket::Under50ms, "lt_50ms"),
            (LatencyBucket::Under250ms, "lt_250ms"),
            (LatencyBucket::Under1s, "lt_1s"),
            (LatencyBucket::Under5s, "lt_5s"),
            (LatencyBucket::Over5s, "gt_5s"),
        ];
        for (value, text) in buckets {
            assert_eq!(value.as_str(), text);
            assert_eq!(
                serde_json::to_string(&value).unwrap(),
                format!("\"{text}\"")
            );
        }
        let counts = [
            (Count::Workspaces, "workspaces"),
            (Count::Pages, "pages"),
            (Count::BrainTurns, "brain_turns"),
            (Count::McpCalls, "mcp_calls"),
        ];
        for (value, text) in counts {
            assert_eq!(value.as_str(), text);
            assert_eq!(
                serde_json::to_string(&value).unwrap(),
                format!("\"{text}\"")
            );
        }
        let outcomes = [
            (Outcome::Success, "success"),
            (Outcome::Failure, "failure"),
            (Outcome::Skipped, "skipped"),
        ];
        for (value, text) in outcomes {
            assert_eq!(value.as_str(), text);
            assert_eq!(
                serde_json::to_string(&value).unwrap(),
                format!("\"{text}\"")
            );
        }
        let metrics = [
            (OutcomeMetric::NightlyEvolve, "nightly_evolve"),
            (OutcomeMetric::BrainTurn, "brain_turn"),
            (OutcomeMetric::McpCall, "mcp_call"),
        ];
        for (value, text) in metrics {
            assert_eq!(value.as_str(), text);
            assert_eq!(
                serde_json::to_string(&value).unwrap(),
                format!("\"{text}\"")
            );
        }
        assert_eq!(Latency::Search.as_str(), "search");
        let errors = [
            (ErrorKind::Config, "config"),
            (ErrorKind::Upstream, "upstream"),
            (ErrorKind::Timeout, "timeout"),
            (ErrorKind::Decode, "decode"),
            (ErrorKind::Permission, "permission"),
            (ErrorKind::Internal, "internal"),
        ];
        for (value, text) in errors {
            assert_eq!(value.as_str(), text);
            assert_eq!(
                serde_json::to_string(&value).unwrap(),
                format!("\"{text}\"")
            );
        }
    }

    /// Every `ALL` constant names exactly the variants `as_str` can produce.
    ///
    /// `ALL` is what `tests/anonymity.rs` iterates to build its vocabulary, so
    /// a variant added to an enum but left out of `ALL` would shrink the test's
    /// vocabulary — and the test would then pass *less*, quietly, which is the
    /// opposite of what it is for. This is the assertion that makes "the
    /// vocabulary grows with the enum" true rather than merely intended.
    ///
    /// It also pins the count, so a variant added to both is caught here as a
    /// deliberate line rather than an accident.
    #[test]
    fn every_all_constant_names_every_variant() {
        assert_eq!(Platform::ALL.len(), 5);
        assert_eq!(LatencyBucket::ALL.len(), 5);
        assert_eq!(Count::ALL.len(), 4);
        assert_eq!(Outcome::ALL.len(), 3);
        assert_eq!(OutcomeMetric::ALL.len(), 3);
        assert_eq!(Latency::ALL.len(), 1);
        assert_eq!(ErrorKind::ALL.len(), 6);

        // `ALL` is the whole vocabulary: every spelling `as_str` produces is
        // produced by some element of `ALL`, and `ALL` produces nothing else.
        let words = |all: &[Platform]| -> BTreeSet<&'static str> {
            all.iter().copied().map(Platform::as_str).collect()
        };
        assert_eq!(
            words(Platform::ALL),
            BTreeSet::from(["linux", "macos", "windows", "wasm", "other"])
        );
        let bucket_words = |all: &[LatencyBucket]| -> BTreeSet<&'static str> {
            all.iter().copied().map(LatencyBucket::as_str).collect()
        };
        assert_eq!(
            bucket_words(LatencyBucket::ALL),
            BTreeSet::from(["lt_50ms", "lt_250ms", "lt_1s", "lt_5s", "gt_5s"])
        );
        let count_words = |all: &[Count]| -> BTreeSet<&'static str> {
            all.iter().copied().map(Count::as_str).collect()
        };
        assert_eq!(
            count_words(Count::ALL),
            BTreeSet::from(["workspaces", "pages", "brain_turns", "mcp_calls"])
        );
        let error_words = |all: &[ErrorKind]| -> BTreeSet<&'static str> {
            all.iter().copied().map(ErrorKind::as_str).collect()
        };
        assert_eq!(
            error_words(ErrorKind::ALL),
            BTreeSet::from([
                "config",
                "upstream",
                "timeout",
                "decode",
                "permission",
                "internal"
            ])
        );
        let outcome_words = |all: &[Outcome]| -> BTreeSet<&'static str> {
            all.iter().copied().map(Outcome::as_str).collect()
        };
        assert_eq!(
            outcome_words(Outcome::ALL),
            BTreeSet::from(["success", "failure", "skipped"])
        );
        let metric_words = |all: &[OutcomeMetric]| -> BTreeSet<&'static str> {
            all.iter().copied().map(OutcomeMetric::as_str).collect()
        };
        assert_eq!(
            metric_words(OutcomeMetric::ALL),
            BTreeSet::from(["nightly_evolve", "brain_turn", "mcp_call"])
        );
        let latency_words = |all: &[Latency]| -> BTreeSet<&'static str> {
            all.iter().copied().map(Latency::as_str).collect()
        };
        assert_eq!(latency_words(Latency::ALL), BTreeSet::from(["search"]));
    }

    /// The latency boundary table, at the boundaries and either side of one.
    #[test]
    fn latency_buckets_are_half_open() {
        let table: [(Duration, LatencyBucket); 7] = [
            (Duration::ZERO, LatencyBucket::Under50ms),
            (Duration::from_millis(49), LatencyBucket::Under50ms),
            (Duration::from_millis(50), LatencyBucket::Under250ms),
            (Duration::from_millis(250), LatencyBucket::Under1s),
            (Duration::from_millis(999), LatencyBucket::Under1s),
            (Duration::from_secs(1), LatencyBucket::Under5s),
            (Duration::from_secs(5), LatencyBucket::Over5s),
        ];
        for (took, want) in table {
            assert_eq!(bucket_latency(took), want, "for {took:?}");
        }
    }

    /// A bucket index says how much, not how much exactly.
    #[test]
    fn bucket_reduces_a_magnitude() {
        assert_eq!(Bucket::of(0).index(), 0);
        assert_eq!(Bucket::of(9).index(), 0);
        assert_eq!(Bucket::of(10).index(), 1);
        assert_eq!(Bucket::of(99).index(), 1);
        assert_eq!(Bucket::of(100).index(), 2);
        assert_eq!(Bucket::of(1_000_000).index(), 2);
    }

    /// A usage id is sixteen lowercase characters and nothing else.
    #[test]
    fn usage_id_is_exactly_sixteen_lowercase() {
        assert!(validate_usage_id("abc123def456ab78").is_ok());
        assert_eq!(USAGE_ID_LEN, 16);
        assert!(UsageId::new("abc123def456ab78").is_ok());
        assert!(UsageId::new("abc123def456ab").is_err(), "15 chars");
        assert!(UsageId::new("abc123def456ab789").is_err(), "17 chars");
        assert!(UsageId::new("ABC123def456ab7").is_err(), "uppercase");
        assert!(UsageId::new("abc-23def456ab7").is_err(), "hyphen");
        assert!(UsageId::new("").is_err(), "empty");
        assert!(UsageId::try_from("7f3c9a1b2d4e6f80").is_ok());
        let err: InvalidUsageId = UsageId::try_from(String::from("nope")).unwrap_err();
        assert!(!err.to_string().is_empty());
        assert!(std::error::Error::source(&err).is_none());
    }

    /// A version is a version and not a build path, a branch or a commit.
    ///
    /// The accepted column is the grammar on [`Version`], read literally; the
    /// rejected column is every way a person's checkout can wear a version
    /// number's clothes.
    #[test]
    fn version_refuses_fingerprints() {
        // Accepted: major.minor.patch, then at most one prerelease suffix.
        for good in [
            "0.1.0",
            "1.98.1",
            "2026.10.8",
            "0.0.0",
            "10.20.30",
            "0.1.0-rc.1",
            "0.2.0-beta.3",
            "1.0.0-alpha.0",
            "1.0.0-dev.12",
            "0.1.0-beta.1000",
        ] {
            assert!(Version::new(good).is_ok(), "{good} should be a version");
        }

        // Rejected: not three numbers.
        for bad in [
            "",        // empty
            "1",       // one number
            "1.2",     // two
            "1.2.3.4", // four
            "v0.1.0",  // a leading `v` is not the grammar
            "01.1.0",  // a leading zero
            "0.01.0",  // a leading zero
            "0.1.0.0", // a trailing dot
            ".1.0",    // an empty part
            "0..0",    // an empty part
        ] {
            assert!(Version::new(bad).is_err(), "{bad} should not be a version");
        }

        // Rejected: the values a real build pipeline produces. Each of these
        // serialises onto the wire under the old loose rule, and each carries
        // a person, a commit or a checkout with it.
        for hostile in [
            // `git describe --tags --dirty` on a checkout three commits past a tag.
            "0.1.0-3-gabc1234-dirty",
            "0.1.0-1-g9f2c1ab",
            // A build stamped with a person's name and machine.
            "2026.10.08-alice-laptop",
            "1jane-doe-macbook-2026",
            // A build stamped with a workspace.
            "1workspace-acme-corp",
            // A branch name, and a dirty marker, and anything else after the
            // single hyphen the grammar allows.
            "0.1.0-dirty",
            "0.1.0-main",
            "0.1.0-alice",
            "0.1.0-rc.1-extra",
            // A prerelease word that is not one of the four.
            "0.1.0-nightly.1",
            "0.1.0-pre.1",
            // A prerelease with no number, or a non-numeric one.
            "0.1.0-rc",
            "0.1.0-rc.",
            "0.1.0-rc.x",
            // The characters a path, an email or an id is made of.
            "/home/alice/src/livingbrain",
            "alice@example.com",
            "0.1.0_1",
            "0.1.0 1",
            // A 40-character commit hash, which the length cap also catches.
            "9f2c1ab4d5e6f708192a3b4c5d6e7f8091a2b3c4",
            "0.1.0-9f2c1ab4d5e6f708192a3b4c5d6e7f8",
        ] {
            assert!(
                Version::new(hostile).is_err(),
                "{hostile} must not reach the wire as a version"
            );
        }

        // Length is a property of the whole string, so the boundary is stated
        // in characters: the longest legal version, and one character over.
        let at_cap = "99999999.9999999.9999999-rc.9999";
        assert_eq!(at_cap.chars().count(), 32);
        assert!(Version::new(at_cap).is_ok(), "32 characters is at the cap");
        let over = "999999999.9999999.9999999-rc.9999";
        assert_eq!(over.chars().count(), 33);
        assert!(Version::new(over).is_err(), "33 characters is over 32");
    }

    /// Every rejection names the rule it broke, and never echoes the value.
    ///
    /// A caller that gets this error is looking at a failed build, and the
    /// message is what tells them what to fix — so a generic "not a version"
    /// would leave them guessing. And a rejected value may itself be the
    /// secret: `2026.10.08-alice-laptop` is a person's name.
    #[test]
    fn a_rejection_names_the_rule_and_not_the_value() {
        let cases: [(&str, &str); 7] = [
            ("", "never empty"),
            ("0.1.0.0", "major.minor.patch"),
            ("01.1.0", "leading zero"),
            ("0.1.0-rc.1-extra", "at most one hyphen"),
            ("0.1.0-nightly.1", "rc, alpha, beta and dev"),
            ("0.1.0-rc.x", "ASCII digits"),
            ("0.1.0-main", "prerelease suffix"),
        ];
        for (value, expected) in cases {
            let err = Version::new(value).unwrap_err();
            assert!(
                err.reason.contains(expected),
                "{value:?} should be refused for a reason naming {expected:?}, got {:?}",
                err.reason
            );
            if !value.is_empty() {
                assert!(
                    !err.reason.contains(value),
                    "the reason must not echo the rejected value"
                );
            }
            assert!(!err.to_string().is_empty());
            assert!(std::error::Error::source(&err).is_none());
        }
    }

    /// This crate's own version validates, or `Version::new` in the CLI is a
    /// runtime failure waiting for the next release.
    #[test]
    fn this_crates_version_is_a_version() {
        let raw = env!("CARGO_PKG_VERSION");
        assert!(
            Version::new(raw).is_ok(),
            "livingbrain-telemetry {raw} must satisfy the grammar it ships"
        );
    }

    /// A counter that reaches `u64::MAX` saturates: no debug panic, no
    /// release wrap-back-to-zero, and the same answer in both profiles.
    ///
    /// `+=` is the bug this catches. In a debug build it panics here, so the
    /// test fails loudly; in a release build it wraps to a small number, which
    /// is far worse — a dashboard would show a count of zero for an install
    /// that had counted `u64::MAX` things.
    #[test]
    fn counters_saturate_instead_of_wrapping_or_panicking() {
        let mut batch = UsageBatch::new(
            UsageId::new("abc123def456ab78").unwrap(),
            Version::new("0.1.0").unwrap(),
            Platform::Linux,
            1_700_000_000,
        );
        batch.count_by(Count::BrainTurns, u64::MAX);
        batch.count_by(Count::BrainTurns, 1);
        assert_eq!(batch.counters[&Count::BrainTurns], u64::MAX);

        batch.count_error(ErrorKind::Timeout, u64::MAX);
        batch.count_error(ErrorKind::Timeout, 1);
        assert_eq!(batch.errors[&ErrorKind::Timeout], u64::MAX);

        // A different counter is untouched: saturation is per-key, and the
        // whole point is that the number on the wire is still the truth.
        assert!(!batch.counters.contains_key(&Count::Pages));
        assert_eq!(
            batch.count_error(ErrorKind::Config, 1).errors[&ErrorKind::Config],
            1
        );

        // And it still serialises: a saturated count is a real `u64` on the
        // wire, not a number the schema has to special-case.
        let value = serde_json::to_value(&batch).unwrap();
        assert_eq!(value["counters"]["brain_turns"], u64::MAX);
        assert_eq!(value["errors"]["timeout"], u64::MAX);
    }

    /// A batch counts by adding, so a caller never has to read it first.
    #[test]
    fn counters_accumulate_and_maps_start_empty() {
        let mut batch = UsageBatch::new(
            UsageId::new("abc123def456ab78").unwrap(),
            Version::new("0.1.0").unwrap(),
            Platform::Linux,
            1_700_000_000,
        );
        assert!(batch.counters.is_empty());
        assert_eq!(batch.payload_version, USAGE_PAYLOAD_VERSION);
        batch.count_by(Count::BrainTurns, 1);
        batch.count_by(Count::BrainTurns, 4);
        batch.count_by(Count::Pages, 2);
        assert_eq!(batch.counters[&Count::BrainTurns], 5);
        assert_eq!(batch.counters[&Count::Pages], 2);
        batch.observe_latency(Latency::Search, Duration::from_millis(120));
        assert_eq!(batch.latencies[&Latency::Search], LatencyBucket::Under250ms);
        batch.record_outcome(OutcomeMetric::BrainTurn, Outcome::Success);
        batch.count_error(ErrorKind::Timeout, 1);
        assert_eq!(batch.outcomes[&OutcomeMetric::BrainTurn], Outcome::Success);
        assert_eq!(batch.errors[&ErrorKind::Timeout], 1);
    }

    /// The maps serialise as objects keyed by the vocabulary, in order.
    ///
    /// Nine keys, which is the number of fields [`UsageBatch`] has. The issue
    /// text says "ten" and then lists nine: the list is the contract, and the
    /// assertion below is on the list.
    #[test]
    fn batch_serialises_to_exactly_the_nine_listed_keys() {
        let mut batch = UsageBatch::new(
            UsageId::new("abc123def456ab78").unwrap(),
            Version::new("0.1.0").unwrap(),
            Platform::Wasm,
            1_700_000_000,
        );
        batch.count_by(Count::Pages, 3);
        batch.record_outcome(OutcomeMetric::McpCall, Outcome::Failure);
        let value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&batch).unwrap()).unwrap();
        let object = value.as_object().unwrap();
        assert_eq!(object.len(), 9);
        for key in [
            "payload_version",
            "usage_id",
            "version",
            "platform",
            "period_end_unix",
            "counters",
            "latencies",
            "outcomes",
            "errors",
        ] {
            assert!(object.contains_key(key), "missing {key}");
        }
        assert_eq!(object["counters"], serde_json::json!({ "pages": 3 }));
        assert_eq!(
            object["outcomes"],
            serde_json::json!({ "mcp_call": "failure" })
        );
        // BTreeMap ordering: the wire is sorted, so a diff of two batches is
        // readable.
        assert_eq!(
            serde_json::to_string(&BTreeMap::from([(Count::Pages, 1)])).unwrap(),
            "{\"pages\":1}"
        );
    }

    /// The environment decides, a typo does not, and the notice names both
    /// off switches and shows the batch.
    #[test]
    fn telemetry_resolves_and_announces() {
        assert_eq!(Telemetry::resolve(Telemetry::On, Some("0")), Telemetry::Off);
        assert_eq!(Telemetry::resolve(Telemetry::Off, Some("1")), Telemetry::On);
        assert_eq!(
            Telemetry::resolve(Telemetry::Off, Some("OFF")),
            Telemetry::Off
        );
        assert_eq!(
            Telemetry::resolve(Telemetry::Off, Some(" Yes ")),
            Telemetry::On
        );
        assert_eq!(
            Telemetry::resolve(Telemetry::Off, Some("garbage")),
            Telemetry::Off
        );
        assert_eq!(
            Telemetry::resolve(Telemetry::On, Some("garbage")),
            Telemetry::On
        );
        assert_eq!(Telemetry::resolve(Telemetry::On, None), Telemetry::On);
        assert!(!Telemetry::Off.is_on());
        assert!(Telemetry::default().is_on(), "on by default, and said so");

        let batch = UsageBatch::new(
            UsageId::new("abc123def456ab78").unwrap(),
            Version::new("0.1.0").unwrap(),
            Platform::Linux,
            1,
        );
        let notice = Telemetry::notice(&batch, "the first period, 2026-10-01 to 2026-10-08");
        assert!(notice.contains("livingbrain telemetry off"));
        assert!(notice.contains("LIVINGBRAIN_TELEMETRY=0"));
        assert!(notice.contains("the first period, 2026-10-01 to 2026-10-08"));
        assert!(notice.contains(&serde_json::to_string_pretty(&batch).unwrap()));
    }
}
