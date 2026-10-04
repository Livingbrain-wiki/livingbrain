-- Workspace tenancy (issue #5), Postgres form. Identical to
-- migrations/sqlite/0001_init.sql: this schema is the portable subset
-- (ADR 0004) — TEXT columns, 0/1 INTEGER flags, ISO-8601 timestamps — so
-- nothing here is dialect-specific. The set is shipped anyway so the
-- parity job applies a postgres migration rather than falling back to the
-- sqlite one, and so a later dialect-specific change has a file to land
-- in without renumbering.
--
-- `workspaces.owner_id` is the Slack user id of the first person to sign
-- in. It is written by an insert-if-absent and never updated anywhere,
-- which is how "the first person to sign in owns the workspace" holds
-- exactly once.
CREATE TABLE IF NOT EXISTS workspaces (
    id         TEXT PRIMARY KEY, -- the Slack team id
    name       TEXT NOT NULL DEFAULT '',
    owner_id   TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS workspace_members (
    workspace_id TEXT NOT NULL,
    user_id      TEXT NOT NULL,
    name         TEXT NOT NULL DEFAULT '',
    timezone     TEXT,
    is_admin     INTEGER NOT NULL DEFAULT 0,
    updated_at   TEXT NOT NULL,
    PRIMARY KEY (workspace_id, user_id)
);
