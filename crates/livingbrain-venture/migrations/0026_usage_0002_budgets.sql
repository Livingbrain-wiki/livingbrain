-- Per-workspace budgets and limits (the "Usage & limits" settings tab):
-- one row per workspace that has changed any of them.
--
-- Every limit column is NULLABLE, and NULL is the point: it means "not set
-- here", so the effective value falls through to the deployment's env var
-- (`CODING_AGENTS_*`, `ASK_CONTEXT_CHARS_MAX`, `USAGE_BUDGET_CENTS_MONTHLY`)
-- and then to the built-in default. Clearing a setting writes NULL back; it
-- never writes the inherited number, so a later change to the deployment's
-- default still reaches a workspace that never chose its own.
--
-- The hard ceilings (sessions per run, the per-session token cap) are not
-- stored: they are code, enforced by the compiler however a row got here.
--
-- Nothing here names or describes a person: the row is the workspace's.
-- Who changed a limit belongs in the settings audit log (livingbrain-audit),
-- not in a column erasure would have to reach.
--
-- The portable subset (ADR 0004): TEXT and INTEGER only. The Postgres file
-- differs only in spelling the token counts BIGINT.
CREATE TABLE IF NOT EXISTS usage_budgets (
    workspace_id               TEXT PRIMARY KEY,
    compiler_paused            INTEGER,  -- 1 = paused, 0 = running, NULL = inherit (running)
    compiler_daily_tokens      INTEGER,  -- compile tokens per UTC day
    compiler_monthly_tokens    INTEGER,  -- compile tokens per UTC month
    compiler_job_tokens        INTEGER,  -- one session's cap, in and out
    compiler_batch             INTEGER,  -- sessions one cron run compiles
    compiler_max_chunks        INTEGER,  -- model calls one session may take
    compiler_chunk_chars       INTEGER,  -- transcript characters per call
    compiler_max_output_tokens INTEGER,  -- completion ceiling per call
    ask_context_chars          INTEGER,  -- the most brief one ask sends
    ask_max_output_tokens      INTEGER,  -- the answer's completion ceiling
    spend_cents_monthly        INTEGER,  -- the monthly spend limit, US cents
    updated_at                 TEXT NOT NULL  -- RFC 3339
);
