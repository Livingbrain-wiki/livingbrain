-- Dreaming's settings per workspace (docs/temporal-facts.md, section 9).
--
-- One row per workspace that has changed its dreaming settings; a missing
-- row is the defaults: dreaming on, the quick rules on, the Main model.
-- `run_requested_at` is set by "Run now": the next cron tick marks the
-- workspace's subjects due and clears it. Choices and times only: nothing
-- about a person and nothing about a fact.
CREATE TABLE IF NOT EXISTS dreaming_settings (
    workspace_id       TEXT NOT NULL PRIMARY KEY,
    enabled            BIGINT NOT NULL DEFAULT 1,
    rules_enabled      BIGINT NOT NULL DEFAULT 1,
    model_role         TEXT NOT NULL DEFAULT 'main',
    run_requested_at   TEXT,
    updated_at         TEXT NOT NULL,
    updated_by         TEXT
);
