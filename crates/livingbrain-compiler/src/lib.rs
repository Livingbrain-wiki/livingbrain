//! `livingbrain-compiler`: synced coding-agent sessions, compiled into an
//! LLM-maintained wiki — Andrej Karpathy's "LLM wiki" pattern, under
//! `coding-agents/`.
//!
//! - **Raw sources stay immutable.** The sessions are the `agent_log` rows
//!   of the source ledger, sealed per scope; nothing here writes them.
//! - **A queue feeds the compiler** ([`queue`]): one job per source body,
//!   idempotent on the ledger's own key, with a status the person can see —
//!   `queued`, `processing`, `done`, `failed`, `skipped-no-model`,
//!   `over-budget`, `superseded`, `skipped`, `waiting-runner`,
//!   `skipped-trivial`, `summary-only`.
//! - **Triage and compaction come first** ([`triage`], [`compact`]): a
//!   trivial session is skipped and a failing loop logged in one line, with
//!   no model call; the rest is compiled from a transcript cut down to its
//!   errors, commands and files. The source stays whole, and "Compile
//!   fully" compiles any triaged session without either.
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
//! - **In parallel** ([`pool`]): a drain runs several compiles at once —
//!   up to each workspace's `concurrency`, never more than six per
//!   invocation — fair across workspaces; a 429 pauses and halves that
//!   workspace's pool; budget holds keep parallel calls inside the token
//!   budgets; two compiles writing one page take turns and merge
//!   ([`merge`]) rather than overwrite.
//! - **Safety**: model input and output redacted, a per-job token cap and a
//!   per-workspace daily and monthly budget, the transcript fenced as data,
//!   and every edit outside the session's own folder refused.

#![forbid(unsafe_code)]

pub mod compact;
pub mod compile;
pub mod edits;
pub mod facts;
pub mod layout;
pub mod merge;
pub mod pool;
pub mod prompt;
pub mod queue;
pub mod record;
pub mod triage;

pub use compile::{
    BATCH_CEILING, CHUNK_CHARS_CEILING, Call, Compiler, Gate, JOB_TOKENS_CEILING,
    MAX_CHUNKS_CEILING, MAX_OUTPUT_TOKENS_CEILING, Model, ModelFailure, NoGate, NoTuning,
    Overrides, Processed, Reply, Settings, Tuning,
};
pub use facts::{Emitted, Fact, FactSink, NoFacts};
pub use pool::{CONCURRENCY_CEILING, MAX_CALL, MAX_IN_FLIGHT, WALL_AFTER_BUDGET};
pub use queue::{ClaimFilter, Finish, Job, JobStatus, PoolRow, Queue, Savings};

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

/// The third migration: triage and compaction before a compile.
pub const MIGRATION_TRIAGE: SqlMigration = SqlMigration::new(
    "0003",
    "triage",
    include_str!("../migrations/sqlite/0003_triage.sql"),
);

/// The Postgres form: the counters are `BIGINT`, like the others.
pub const MIGRATION_TRIAGE_POSTGRES: SqlMigration = SqlMigration::new(
    "0003",
    "triage",
    include_str!("../migrations/postgres/0003_triage.sql"),
);

/// The fourth migration: compiles run side by side — budget holds for the
/// calls in flight, and each workspace's pool.
pub const MIGRATION_PARALLEL: SqlMigration = SqlMigration::new(
    "0004",
    "parallel",
    include_str!("../migrations/sqlite/0004_parallel.sql"),
);

/// The Postgres form: the counters are `BIGINT`, like the others.
pub const MIGRATION_PARALLEL_POSTGRES: SqlMigration = SqlMigration::new(
    "0004",
    "parallel",
    include_str!("../migrations/postgres/0004_parallel.sql"),
);

/// The tables the module that mounts the compiler owns.
pub const TABLES: [&str; 4] = [
    "coding_agent_jobs",
    "coding_agent_spend",
    "coding_agent_reservations",
    "coding_agent_pool",
];
