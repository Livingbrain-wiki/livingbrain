-- The usage ledger (issue #12): one row per workspace per UTC day, the
-- facts a hosted plan's bill is built from.
--
-- All columns are TEXT and INTEGER counters (the portable subset, ADR
-- 0004); timestamps are ISO-8601 strings, so the same shape runs on
-- Postgres. The `day` is the UTC calendar day, `YYYY-MM-DD`, and
-- lexicographic order on it is chronological order.
--
-- The counters are written only by atomic increments — an `INSERT ... ON
-- CONFLICT DO UPDATE` that adds `excluded` to what is already there — so
-- two callers recording in the same day merge instead of one overwriting
-- the other. Nothing here is ever deleted: a day's facts are facts, and
-- the budget is computed from them after the fact.
--
-- `d1_queries` is written by no code path yet: during the
-- single-workspace pilot D1 usage is account-level, read from the
-- Cloudflare dashboard. The column is ready for the day query-level
-- attribution lands.
CREATE TABLE IF NOT EXISTS usage_daily (
    workspace_id      TEXT NOT NULL,      -- the workspace the usage belongs to
    day               TEXT NOT NULL,      -- UTC 'YYYY-MM-DD'
    model_requests    INTEGER NOT NULL DEFAULT 0,
    prompt_tokens     INTEGER NOT NULL DEFAULT 0,
    completion_tokens INTEGER NOT NULL DEFAULT 0,
    d1_queries        INTEGER NOT NULL DEFAULT 0,
    emails_sent       INTEGER NOT NULL DEFAULT 0,
    updated_at        TEXT NOT NULL,      -- RFC 3339, when the day was last touched
    PRIMARY KEY (workspace_id, day)
);

-- The one query the read endpoint does not need (it filters by workspace)
-- but the per-day rollup across workspaces does: which days have facts.
CREATE INDEX IF NOT EXISTS usage_daily_by_day
    ON usage_daily (day);
