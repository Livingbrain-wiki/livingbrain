-- Compiles run side by side (docs/coding-agents.md, "Parallel compiles").
--
-- `coding_agent_reservations`: a hold on a workspace's compile budget for
-- one model call in flight. A call is made only after its hold is taken, by
-- one INSERT ... SELECT that checks the day's and the month's spend plus the
-- live holds, so parallel compiles cannot together run the budget past one
-- call's overshoot. Settled (deleted, and the real spend recorded) when the
-- call answers; a hold whose compile died lapses at `expires_at`.
--
-- `coding_agent_pool`: one row per workspace — how many of its compiles run
-- at once now (halved by a provider 429, raised by one by a full run
-- without one), until when its pool waits out a 429's Retry-After, and the
-- rolling average length of one compile call, which decides whether a cron
-- run still has time to start one.
--
-- Workspace ids, counts and timestamps only: nothing about a person.
--
-- SQLite form: INTEGER is 64-bit already.
CREATE TABLE IF NOT EXISTS coding_agent_reservations (
    id          TEXT NOT NULL PRIMARY KEY, -- <scope>:<body_sha256>:<chunk>:<attempt>
    workspace   TEXT NOT NULL,
    day         TEXT NOT NULL,             -- UTC 'YYYY-MM-DD' the hold counts against
    tokens      INTEGER NOT NULL,          -- the call's estimate: prompt plus its output ceiling
    expires_at  TEXT NOT NULL              -- RFC 3339; a lapsed hold counts for nothing
);

CREATE INDEX IF NOT EXISTS coding_agent_reservations_by_workspace
    ON coding_agent_reservations (workspace, day);

CREATE TABLE IF NOT EXISTS coding_agent_pool (
    workspace     TEXT NOT NULL PRIMARY KEY,
    concurrency   INTEGER,                 -- compiles at once now; NULL = the workspace's setting
    paused_until  TEXT,                    -- RFC 3339: no compile call before this (a 429)
    avg_call_ms   INTEGER NOT NULL DEFAULT 0,
    calls         INTEGER NOT NULL DEFAULT 0,
    updated_at    TEXT NOT NULL
);
