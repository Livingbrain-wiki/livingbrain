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
pub mod dream;
pub mod dream_settings;
pub mod edits;
pub mod facts;
pub mod hubs;
pub mod layout;
pub mod merge;
pub mod pool;
pub mod prompt;
pub mod queue;
pub mod record;
pub mod source_compile;
pub mod source_units;
pub mod subject_aliases;
pub mod triage;

pub use compile::{
    BATCH_CEILING, CHUNK_CHARS_CEILING, Call, Classifiers, Compiler, Gate, JOB_TOKENS_CEILING,
    MAX_CHUNKS_CEILING, MAX_OUTPUT_TOKENS_CEILING, Model, ModelFailure, NoGate, NoTuning,
    Overrides, Processed, Reply, Settings, Tuning,
};
pub use dream::{Dreams, Sweep, Window};
pub use dream_settings::{DreamSettings, DreamStatus, DreamTuning, StoredDreamTuning};
pub use facts::{COMPILER_WRITER, Emitted, Fact, FactSink, HeldFact, NoFacts};
pub use pool::{CONCURRENCY_CEILING, MAX_CALL, MAX_IN_FLIGHT, WALL_AFTER_BUDGET};
pub use queue::{ClaimFilter, Finish, Job, JobStatus, PoolRow, Queue, Savings};
pub use source_compile::{UnitRun, render_item, split_pieces};
pub use source_units::{
    BACKLOG_PER_TICK, Filed, LANE_DISCORD, LANE_GITHUB, LANE_IMPORTS, LaneCounts, UNIT_KINDS, Unit,
    UnitFinish,
};

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

/// The fifth migration: the compiles that failed as a bare `bad-output`
/// before the edit list was asked for as a forced tool call, kept short and
/// repaired once are queued again, their attempts reset.
pub const MIGRATION_REQUEUE_BAD_OUTPUT: SqlMigration = SqlMigration::new(
    "0005",
    "requeue_bad_output",
    include_str!("../migrations/sqlite/0005_requeue_bad_output.sql"),
);

/// The Postgres form: the same statement.
pub const MIGRATION_REQUEUE_BAD_OUTPUT_POSTGRES: SqlMigration = SqlMigration::new(
    "0005",
    "requeue_bad_output",
    include_str!("../migrations/postgres/0005_requeue_bad_output.sql"),
);

/// The sixth migration: dreaming's per-workspace settings (`dreaming_settings`):
/// on or off, the quick rules, the model's job slot and a pending "Run now".
pub const MIGRATION_DREAMING_SETTINGS: SqlMigration = SqlMigration::new(
    "0006",
    "dreaming_settings",
    include_str!("../migrations/sqlite/0006_dreaming_settings.sql"),
);

/// The Postgres form of [`MIGRATION_DREAMING_SETTINGS`].
pub const MIGRATION_DREAMING_SETTINGS_POSTGRES: SqlMigration = SqlMigration::new(
    "0006",
    "dreaming_settings",
    include_str!("../migrations/postgres/0006_dreaming_settings.sql"),
);

/// The seventh migration: source compile units and their members — GitHub
/// items, Discord channel-days and imported files compiled into pages.
pub const MIGRATION_SOURCE_UNITS: SqlMigration = SqlMigration::new(
    "0007",
    "source_compile_jobs",
    include_str!("../migrations/sqlite/0007_source_compile_jobs.sql"),
);

/// The Postgres form: the counters are `BIGINT`, like the others.
pub const MIGRATION_SOURCE_UNITS_POSTGRES: SqlMigration = SqlMigration::new(
    "0007",
    "source_compile_jobs",
    include_str!("../migrations/postgres/0007_source_compile_jobs.sql"),
);

/// The eighth migration: a workspace's subject aliases (`fact_subject_aliases`),
/// one entity under several names filed under one subject.
pub const MIGRATION_SUBJECT_ALIASES: SqlMigration = SqlMigration::new(
    "0008",
    "subject_aliases",
    include_str!("../migrations/sqlite/0008_subject_aliases.sql"),
);

/// The Postgres form of [`MIGRATION_SUBJECT_ALIASES`].
pub const MIGRATION_SUBJECT_ALIASES_POSTGRES: SqlMigration = SqlMigration::new(
    "0008",
    "subject_aliases",
    include_str!("../migrations/postgres/0008_subject_aliases.sql"),
);

/// The ninth migration: the alias table's state, setter and evidence columns.
/// Forward-only: `0008` is applied in production and is not edited.
pub const MIGRATION_SUBJECT_ALIAS_STATE: SqlMigration = SqlMigration::new(
    "0009",
    "subject_alias_state",
    include_str!("../migrations/sqlite/0009_subject_alias_state.sql"),
);

/// The Postgres form of [`MIGRATION_SUBJECT_ALIAS_STATE`].
pub const MIGRATION_SUBJECT_ALIAS_STATE_POSTGRES: SqlMigration = SqlMigration::new(
    "0009",
    "subject_alias_state",
    include_str!("../migrations/postgres/0009_subject_alias_state.sql"),
);

/// The tables the module that mounts the compiler owns.
pub const TABLES: [&str; 10] = [
    "coding_agent_jobs",
    "coding_agent_spend",
    "coding_agent_reservations",
    "coding_agent_pool",
    "dreaming_settings",
    "source_compile_units",
    "source_compile_members",
    "source_compile_spend",
    "source_compile_cursor",
    "fact_subject_aliases",
];
