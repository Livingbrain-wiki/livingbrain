//! `livingbrain-accounts`: agent accounts, access grants and the audit log
//! (ADR 0004).
//!
//! Three modules, one per mount:
//!
//! - [`Agents`] at `/v1/agents` — agent accounts (non-human principals a
//!   member owns) and their API keys: create, rename, disable, hand over,
//!   delete; mint, rotate, revoke keys (each shown once).
//! - [`Access`] at `/v1/access` — the grants: list, create, delete, and "who
//!   can see this page". It also owns the tables all three use.
//! - [`Audit`] at `/v1/audit` — the log, for owners and admins.
//!
//! Every route here is a **session** route: a person in a browser, resolved
//! by the tokens module's `session` — never a bearer. A personal access token
//! cannot mint agent keys or hand out access, and an agent account (which
//! never has a session) cannot manage anything, itself included. Every write
//! takes the shared same-origin check.
//!
//! The engine is the harness's `cratefield-module-orgs`: its
//! `service_accounts`, `grants` and `audit` functions, and its migrations,
//! which [`Access`] applies under its own name. Its routes are not mounted —
//! they answer to the harness `Auth` port, and Living Brain's identity is its
//! own session.

#![forbid(unsafe_code)]

mod access;
mod agents;
mod audit;
mod common;

pub use access::Access;
pub use agents::Agents;
pub use audit::{Audit, Event, record};
