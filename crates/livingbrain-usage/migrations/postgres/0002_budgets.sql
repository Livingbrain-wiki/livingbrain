-- Per-workspace budgets and limits, Postgres form. The same schema as
-- migrations/sqlite/0002_budgets.sql with every number spelled BIGINT:
-- a monthly token budget can pass Postgres INTEGER's 2^31 - 1, and SQLite's
-- INTEGER is 64-bit already. NULL in a limit column means "not set here" —
-- the effective value falls through to the env var, then the default.
CREATE TABLE IF NOT EXISTS usage_budgets (
    workspace_id               TEXT PRIMARY KEY,
    compiler_paused            BIGINT,
    compiler_daily_tokens      BIGINT,
    compiler_monthly_tokens    BIGINT,
    compiler_job_tokens        BIGINT,
    compiler_batch             BIGINT,
    compiler_max_chunks        BIGINT,
    compiler_chunk_chars       BIGINT,
    compiler_max_output_tokens BIGINT,
    ask_context_chars          BIGINT,
    ask_max_output_tokens      BIGINT,
    spend_cents_monthly        BIGINT,
    updated_at                 TEXT NOT NULL
);
