//! `livingbrain-github`: the pure core of the GitHub connection.
//!
//! The connection has four parts, and this crate holds the three that need no
//! I/O:
//!
//! - [`webhook`]: verify a delivery's signature over the raw body, read its
//!   envelope (delivery id, event, installation, repository), and read the
//!   payload for the subscribed events.
//! - [`api`]: typed models of the REST responses for commits, issues, pull
//!   requests, comments and reviews, and of the webhook objects. Only the
//!   fields the mapping uses; no email is modelled anywhere.
//! - [`record`]: map those objects to deterministic records and to the parts
//!   a `SourceIngest` is built from.
//! - [`planner`] and [`rate`]: the backfill planner (which requests next,
//!   within a per-tick budget, resumably) and the rate-limit backoff decision.
//!
//! The fourth part, the HTTP client, the installation token and the ledger
//! writes, lives in the Worker and is not in this crate. Nothing here names a
//! runtime, a vendor SDK or a route, and nothing here reads a secret: the
//! webhook secret is passed in by the caller. The record schema is documented
//! in `docs/github.md`.

pub mod api;
mod hex;
pub mod planner;
pub mod rate;
pub mod record;
pub mod webhook;
