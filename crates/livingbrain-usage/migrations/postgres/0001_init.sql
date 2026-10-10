-- The usage ledger (issue #12), Postgres form. The same schema as
-- migrations/sqlite/0001_init.sql with one deliberate difference: the five
-- counters are BIGINT here. They are running totals that only ever grow by
-- increments, and Postgres INTEGER stops at 2^31 - 1 — a busy workspace's
-- prompt tokens for one day can plausibly pass that, and a counter that
-- overflows into an error would silently refuse usage the plan owed money
-- for. SQLite's INTEGER is 64-bit already, so the sqlite file keeps the
-- portable subset's INTEGER spelling and nothing else differs. The set is
-- shipped anyway so the parity job applies a postgres migration rather
-- than falling back to the sqlite one.
CREATE TABLE IF NOT EXISTS usage_daily (
    workspace_id      TEXT NOT NULL,      -- the workspace the usage belongs to
    day               TEXT NOT NULL,      -- UTC 'YYYY-MM-DD'
    model_requests    BIGINT NOT NULL DEFAULT 0,
    prompt_tokens     BIGINT NOT NULL DEFAULT 0,
    completion_tokens BIGINT NOT NULL DEFAULT 0,
    d1_queries        BIGINT NOT NULL DEFAULT 0,
    emails_sent       BIGINT NOT NULL DEFAULT 0,
    updated_at        TEXT NOT NULL,      -- RFC 3339, when the day was last touched
    PRIMARY KEY (workspace_id, day)
);

CREATE INDEX IF NOT EXISTS usage_daily_by_day
    ON usage_daily (day);
