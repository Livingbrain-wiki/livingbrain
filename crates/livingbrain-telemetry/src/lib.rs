//! `livingbrain-telemetry`: what leaves a Living Brain machine, and what
//! does not.
//!
//! Living Brain is open-core and self-hostable, which means the answer to
//! "what do you send, and when" has to be readable rather than reassuring.
//! This crate is that answer, in three pieces that share one idea: **what
//! leaves is a closed set of types, and the types are the promise.** Nothing
//! here takes content as input, so nothing here can send it.
//!
//! - [`usage`] — anonymous counted usage, on by default. A [`usage::UsageBatch`]
//!   is integers, bucketed durations and closed vocabulary words, plus one
//!   random id per period. [`usage::Telemetry::notice`] prints the batch,
//!   verbatim, once per period; [`usage::Telemetry::resolve`] is how
//!   `LIVINGBRAIN_TELEMETRY=0` and `livingbrain telemetry off` win over the
//!   stored preference.
//! - [`live_map`] — the map of installs, **off until switched on**. No id
//!   exists before it is, nothing is sent while it is not, and switching it
//!   off sends one `online: false` beat and forgets the id. The 25 km cell is
//!   derived server-side from the request address; the client sends no
//!   coordinate and [`live_map::Heartbeat`] has no field to put one in.
//! - [`events`] — the structured event log, one JSON line per brain turn,
//!   nightly-evolve change, MCP call and colony hand-off, against
//!   [`events::EVENT_SCHEMA_VERSION`]. Content is referenced by page or
//!   message id and never copied into the line.
//!
//! # The guarantees, and where each one lives
//!
//! | Guarantee | Where it is enforced |
//! |---|---|
//! | The usage payload carries no free text and no identifier but `usage_id` | [`usage::UsageBatch`]'s field types; walked in `tests/anonymity.rs` |
//! | Anonymous usage can be turned off, by env or by command | [`usage::Telemetry::resolve`] |
//! | The live map sends nothing until it is switched on | [`live_map::LiveMap::heartbeat`] returning `None` |
//! | No coordinate is ever sent | the absence of a field on [`live_map::Heartbeat`] |
//! | No log line can carry message text or a page body | the absence of a field on [`events::Event`], plus id validation |
//!
//! The last two are the kind of guarantee a test cannot assert directly, so
//! it is asserted by shape: the type has no field to put the content in, and
//! `tests/anonymity.rs` and `tests/live_map.rs` check the shape from outside
//! the crate.
//!
//! # What this crate deliberately does not do
//!
//! It does not send anything. There is no HTTP client, no queue, no retry and
//! no timer here: the endpoints ([`usage::USAGE_ENDPOINT`],
//! [`live_map::LIVE_MAP_ENDPOINT`]) are constants, and the transport, the
//! schedule, the storage of the preference and the random id all belong to
//! the caller, which already has an `IdGen` and an HTTP client. It does not
//! generate randomness, hold a clock, or read configuration, so it pulls in
//! no dependency that could do any of those behind its back.
//!
//! The Teams audit log is a different project (issue #92) and is not here.
//!
//! The redaction of secrets and PII on the way *in* is
//! `livingbrain-redact`'s job, not this crate's: this crate is the last line,
//! and it is a structural one rather than a filter.

#![forbid(unsafe_code)]

pub mod events;
pub mod live_map;
pub mod usage;

pub use events::{
    EVENT_SCHEMA_VERSION, Event, EventKind, InvalidId, Subject, SubjectKind, ToolName,
};
pub use live_map::{
    AlreadyEnabled, Heartbeat, InstallId, InvalidInstallId, LIVE_MAP_ENDPOINT, LiveMap, PagesBucket,
};
pub use usage::{
    Bucket, Count, ErrorKind, InvalidUsageId, InvalidVersion, Latency, LatencyBucket, Outcome,
    OutcomeMetric, Platform, Telemetry, USAGE_ENDPOINT, USAGE_PAYLOAD_VERSION, UsageBatch, UsageId,
    Version, bucket_latency,
};
