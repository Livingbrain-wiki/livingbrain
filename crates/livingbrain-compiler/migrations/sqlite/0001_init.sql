-- The coding-agents compiler queue (Karpathy-style LLM wiki over synced
-- coding-agent sessions): one job per `agent_log` source body, and the
-- compiler's own per-workspace daily token spend.
--
-- Raw sources stay immutable in the ledger (`sources`); this table only
-- records what the compiler did with each one. A job is keyed exactly like
-- its source, `(scope, body_sha256)`, so enqueueing is idempotent: the
-- enqueue is an `INSERT ... SELECT ... ON CONFLICT DO NOTHING` over the
-- ledger, and the same body is never a second job however often it is
-- synced. No source text, no model output and no page body is stored here:
-- the columns are ids, counters, timestamps and a fixed-vocabulary error.
--
-- Portable subset (ADR 0004): TEXT and integer counters, RFC 3339 strings.
--
-- SQLite form: INTEGER is 64-bit already.
CREATE TABLE IF NOT EXISTS coding_agent_jobs (
    scope             TEXT NOT NULL,      -- the source's folded page scope; compiled pages land here too
    body_sha256       TEXT NOT NULL,      -- the source's ledger key: one job per distinct body
    source_id         TEXT NOT NULL,      -- sources.id, the citation every compiled page carries
    workspace         TEXT NOT NULL,      -- sources.workspace, plain
    owner             TEXT NOT NULL,      -- sources.imported_by: whose session it is
    session_key       TEXT NOT NULL,      -- sources.rel_path ("claude-code session <id>")
    project           TEXT,               -- sources.project, when the source named one
    source_created_at TEXT NOT NULL,      -- when the source arrived (RFC 3339)
    status            TEXT NOT NULL,      -- queued|processing|done|failed|skipped-no-model|over-budget|superseded|skipped
    attempts          INTEGER NOT NULL DEFAULT 0,
    last_error        TEXT,               -- a short, fixed-vocabulary reason; never model or source text
    prompt_tokens     INTEGER NOT NULL DEFAULT 0,
    completion_tokens INTEGER NOT NULL DEFAULT 0,
    pages             TEXT NOT NULL DEFAULT '[]', -- JSON array of the slugs the compile wrote
    rejected          INTEGER NOT NULL DEFAULT 0, -- edits refused (outside the session's folder, stale, invalid)
    facts             INTEGER NOT NULL DEFAULT 0, -- temporal facts emitted
    queued_at         TEXT NOT NULL,
    updated_at        TEXT NOT NULL,
    lease_until       TEXT,               -- a processing claim's expiry; a dead worker's claim lapses
    next_attempt_at   TEXT,               -- when a failed or parked job may run again
    PRIMARY KEY (scope, body_sha256)
);

-- The drain reads by status and readiness, oldest first.
CREATE INDEX IF NOT EXISTS coding_agent_jobs_by_status
    ON coding_agent_jobs (status, queued_at);

-- The status route reads one workspace, the asker's scopes in it, newest first.
CREATE INDEX IF NOT EXISTS coding_agent_jobs_by_workspace
    ON coding_agent_jobs (workspace, scope, queued_at);

-- A session that grew arrives as a new body under the same session key;
-- the older job is superseded by it, found through this index.
CREATE INDEX IF NOT EXISTS coding_agent_jobs_by_session
    ON coding_agent_jobs (scope, session_key);

-- The compiler's own token budget: what its model calls spent, per
-- workspace per UTC day. Written by atomic increments only.
CREATE TABLE IF NOT EXISTS coding_agent_spend (
    workspace  TEXT NOT NULL,
    day        TEXT NOT NULL,             -- UTC 'YYYY-MM-DD'
    tokens     INTEGER NOT NULL DEFAULT 0,
    calls      INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (workspace, day)
);
