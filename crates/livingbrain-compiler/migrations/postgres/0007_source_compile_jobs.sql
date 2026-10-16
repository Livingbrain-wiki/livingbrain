-- Compiling GitHub items, Discord channel-days and imported files
-- (docs/coding-agents.md, "Source units").
--
-- `source_compile_units`: one row per unit of source material to compile.
-- A GitHub unit is one repository's day (its commits, pull requests and
-- issues stored that day), a Discord unit is one channel-day, an import
-- unit is one imported file. `lane` is what the status surface groups by.
-- The unit compiles into the scope its sources are stored in, never another.
--
-- `source_compile_members`: which sources a unit holds, and how far each
-- has been compiled (`pieces_done` of `pieces_total` pieces; `compiled_at`
-- once all of them are). A source with no member row has not been queued:
-- that is what the bounded backlog pass looks for.
--
-- Workspace ids, counts, statuses and timestamps only: no source text.
CREATE TABLE IF NOT EXISTS source_compile_units (
    scope              TEXT NOT NULL,
    unit_key           TEXT NOT NULL,
    workspace          TEXT NOT NULL,
    owner              TEXT NOT NULL,           -- the member the model call runs for
    lane               TEXT NOT NULL,           -- 'github' | 'discord' | 'imports'
    unit_day           TEXT NOT NULL,           -- UTC 'YYYY-MM-DD' the unit is for
    label              TEXT NOT NULL,           -- the project folder the pages go under
    status             TEXT NOT NULL,
    attempts           BIGINT NOT NULL DEFAULT 0,
    last_error         TEXT,
    prompt_tokens      BIGINT NOT NULL DEFAULT 0,
    completion_tokens  BIGINT NOT NULL DEFAULT 0,
    pages              TEXT NOT NULL DEFAULT '[]',
    rejected           BIGINT NOT NULL DEFAULT 0,
    facts              BIGINT NOT NULL DEFAULT 0,
    queued_at          TEXT NOT NULL,
    updated_at         TEXT NOT NULL,
    lease_until        TEXT,
    next_attempt_at    TEXT,
    PRIMARY KEY (scope, unit_key)
);

CREATE INDEX IF NOT EXISTS source_compile_units_by_status
    ON source_compile_units (workspace, status, queued_at);

CREATE TABLE IF NOT EXISTS source_compile_members (
    scope              TEXT NOT NULL,
    source_id          TEXT NOT NULL,
    unit_key           TEXT NOT NULL,
    pieces_total       BIGINT NOT NULL DEFAULT 0,
    pieces_done        BIGINT NOT NULL DEFAULT 0,
    compiled_at        TEXT,
    source_created_at  TEXT NOT NULL,
    PRIMARY KEY (scope, source_id)
);

CREATE INDEX IF NOT EXISTS source_compile_members_by_unit
    ON source_compile_members (scope, unit_key, compiled_at);

-- The source units' share of the workspace's compile budget: the tokens
-- their calls spent, per workspace and UTC day. Sessions never read it; it
-- is what the units' cap (`source_share_percent` of the daily and monthly
-- budgets) is checked against, so a backlog cannot use the whole budget.
CREATE TABLE IF NOT EXISTS source_compile_spend (
    workspace   TEXT NOT NULL,
    day         TEXT NOT NULL,              -- UTC 'YYYY-MM-DD'
    tokens      BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (workspace, day)
);

-- The backlog pass's high-water mark over `sources`: the last source id it
-- scanned (ULIDs sort in arrival order). Each pass reads only ids after it,
-- so a source is read once, not once per tick. One row, named `sources`.
CREATE TABLE IF NOT EXISTS source_compile_cursor (
    name        TEXT NOT NULL PRIMARY KEY,
    last_id     TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);

