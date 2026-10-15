//! `livingbrain-compiler`: synced coding-agent sessions, compiled into an
//! LLM-maintained wiki — Andrej Karpathy's "LLM wiki" pattern, under
//! `coding-agents/`.
//!
//! - **Raw sources stay immutable.** The sessions are the `agent_log` rows
//!   of the source ledger, sealed per scope; nothing here writes them.
//! - **A queue feeds the compiler** ([`queue`]): one job per source body,
//!   idempotent on the ledger's own key, with a status the person can see —
//!   `queued`, `processing`, `done`, `failed`, `skipped-no-model`,
//!   `over-budget`, `superseded`, `skipped`, `waiting-runner`.
//! - **The compiler maintains pages incrementally** ([`compile`]): it shows
//!   the workspace's own model the conventions page, the pages the session
//!   may touch and one transcript chunk, and applies the model's answer —
//!   a closed edit list ([`edits`]) — with the page store's optimistic
//!   concurrency. Re-processing updates pages rather than duplicating them.
//! - **The folder** ([`layout`]) is `coding-agents/` → agent → project,
//!   spelled in slugs with `--` between segments, plus an index, an
//!   append-only log, a conventions page and a cross-agent project rollup.
//! - **History is kept** — dated entries, superseded rather than rewritten
//!   — and the same changes go to the bi-temporal facts layer through one
//!   adapter ([`facts`]).
//! - **Safety**: model input and output redacted, a per-job token cap and a
//!   per-workspace daily and monthly budget, the transcript fenced as data,
//!   and every edit outside the session's own folder refused.

#![forbid(unsafe_code)]

pub mod compile;
pub mod edits;
pub mod facts;
pub mod layout;
pub mod prompt;
pub mod queue;
pub mod record;

pub use compile::{Call, Compiler, Gate, Model, ModelFailure, NoGate, Processed, Reply, Settings};
pub use facts::{Emitted, Fact, FactSink, NoFacts};
pub use queue::{ClaimFilter, Finish, Job, JobStatus, Queue};

use cratefield_core::SqlMigration;

/// The queue's tables, SQLite form (and the portable subset D1 runs).
pub const MIGRATION_INIT: SqlMigration = SqlMigration::new(
    "0001",
    "init",
    include_str!("../migrations/sqlite/0001_init.sql"),
);

/// The Postgres form: the counters are `BIGINT`.
pub const MIGRATION_INIT_POSTGRES: SqlMigration = SqlMigration::new(
    "0001",
    "init",
    include_str!("../migrations/postgres/0001_init.sql"),
);

/// The second migration: a compile can wait on a member's own runner.
pub const MIGRATION_WAITING_RUNNER: SqlMigration = SqlMigration::new(
    "0002",
    "waiting_runner",
    include_str!("../migrations/sqlite/0002_waiting_runner.sql"),
);

/// The Postgres form: the counter is `BIGINT`, like the others.
pub const MIGRATION_WAITING_RUNNER_POSTGRES: SqlMigration = SqlMigration::new(
    "0002",
    "waiting_runner",
    include_str!("../migrations/postgres/0002_waiting_runner.sql"),
);

/// The tables the module that mounts the compiler owns.
pub const TABLES: [&str; 2] = ["coding_agent_jobs", "coding_agent_spend"];
