-- Members, roles and invitations (ADR 0004, phase a).
--
-- `role` is what a member may do: 'owner', 'admin', 'member' or 'viewer'.
-- `workspaces.owner_id` stays as the founder — written once, never again
-- (ADR 0001) — and the backfill makes that person an 'owner', so every
-- workspace starts with exactly the owner it had. Slack's admin flag keeps
-- its own column (`is_admin`, mirrored from Slack) and is not copied into
-- `role`: a Slack admin is still an admin here, and a later Slack demotion
-- still takes effect, because the code reads the two together.
--
-- `removed_at` is a tombstone rather than a delete, so a removed person who
-- signs in with Slack again lands on a row that is still removed instead of
-- being re-created as a fresh member; every read filters on it.
--
-- The portable subset (harness ADR 0004): TEXT columns, `ALTER TABLE ADD
-- COLUMN`, no dialect type. The Postgres file is this one byte for byte.
ALTER TABLE workspace_members ADD COLUMN role TEXT NOT NULL DEFAULT 'member';
ALTER TABLE workspace_members ADD COLUMN removed_at TEXT;

UPDATE workspace_members SET role = 'owner'
WHERE EXISTS (
    SELECT 1 FROM workspaces w
    WHERE w.id = workspace_members.workspace_id
      AND w.owner_id = workspace_members.user_id
);

-- One row per invitation. Accepting one is spending a sign-in link minted
-- for the same workspace and address (`link_hash` names that link, so
-- revoking the invitation spends its link too); the address is held in the
-- clear, as `sign_in_links` already holds it, because the person who accepts
-- is matched on it.
CREATE TABLE IF NOT EXISTS workspace_invites (
    id               TEXT PRIMARY KEY,
    workspace_id     TEXT NOT NULL,
    email            TEXT NOT NULL,
    role             TEXT NOT NULL,
    invited_by       TEXT NOT NULL,
    link_hash        TEXT NOT NULL,
    created_at       TEXT NOT NULL,
    expires_at       TEXT NOT NULL,
    accepted_at      TEXT,
    accepted_user_id TEXT,
    revoked_at       TEXT
);

CREATE INDEX IF NOT EXISTS workspace_invites_by_address
    ON workspace_invites (workspace_id, email);
