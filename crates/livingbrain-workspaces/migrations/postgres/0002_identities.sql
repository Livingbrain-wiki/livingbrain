-- Workspace identities (issue #71): workspace ids are now our own; Slack
-- becomes a linked connection and email magic link a sign-in method that
-- needs no Slack at all. The new tables carry the links and the single-use
-- sign-in tokens. Existing Slack-created workspaces and members are
-- backfilled so they keep working unchanged — ids are NOT rewritten, so
-- cookies, owner_id and other modules' rows keyed by workspace id stay valid.
CREATE TABLE IF NOT EXISTS workspace_connections (
    workspace_id TEXT NOT NULL,
    platform     TEXT NOT NULL, -- 'slack' | 'discord'
    external_id  TEXT NOT NULL,
    created_at   TEXT NOT NULL,
    PRIMARY KEY (workspace_id, platform),
    UNIQUE (platform, external_id)
);

CREATE TABLE IF NOT EXISTS member_identities (
    workspace_id TEXT NOT NULL,
    platform     TEXT NOT NULL, -- 'slack' | 'discord' | 'email'
    external_id  TEXT NOT NULL,
    user_id      TEXT NOT NULL,
    created_at   TEXT NOT NULL,
    PRIMARY KEY (workspace_id, platform, external_id),
    UNIQUE (workspace_id, platform, user_id)
);

CREATE TABLE IF NOT EXISTS sign_in_links (
    token_hash     TEXT PRIMARY KEY, -- sha-256 hex, never the token
    email          TEXT NOT NULL,
    workspace_id   TEXT,             -- NULL = create a workspace
    workspace_name TEXT NOT NULL DEFAULT '',
    expires_at     TEXT NOT NULL,
    spent_at       TEXT
);

-- Backfill: one Slack team linked to one workspace, one Slack user linked
-- to one member. The WHERE NOT EXISTS guards make this safe to re-run
-- (the conformance suite applies migrations twice on a fresh database).
INSERT INTO workspace_connections (workspace_id, platform, external_id, created_at)
SELECT id, 'slack', id, created_at FROM workspaces
WHERE NOT EXISTS (
    SELECT 1 FROM workspace_connections c
    WHERE c.workspace_id = workspaces.id AND c.platform = 'slack'
);

INSERT INTO member_identities (workspace_id, platform, external_id, user_id, created_at)
SELECT workspace_id, 'slack', user_id, user_id, updated_at FROM workspace_members
WHERE NOT EXISTS (
    SELECT 1 FROM member_identities i
    WHERE i.workspace_id = workspace_members.workspace_id
      AND i.platform = 'slack'
      AND i.external_id = workspace_members.user_id
);
