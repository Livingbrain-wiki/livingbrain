-- Workspace tenancy (issue #5): one row per Slack workspace, and one row
-- per Slack user seen in it. All columns are TEXT and 0/1 INTEGER flags
-- (the portable subset, ADR 0004); timestamps are ISO-8601 strings.
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
