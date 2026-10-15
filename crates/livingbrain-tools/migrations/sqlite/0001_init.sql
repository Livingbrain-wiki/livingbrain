-- Tools (issue #11): Livingbrain as an MCP client of remote servers.
-- One row per member and per provider a workspace's members connected;
-- the module calls the server on a member's behalf through the write gate.
--
-- All columns are TEXT (the portable subset, ADR 0004); timestamps are
-- ISO-8601 strings. The token columns hold base64, so no BLOB is needed.
--
-- `ciphertext` is XChaCha20-Poly1305 over the connection token under a
-- fresh KMS-wrapped data key per row, with the workspace, the member and
-- the provider as the AAD — a row copied to another member, workspace or
-- provider does not decrypt. The token is never stored in plaintext and
-- is opened only on the call path.
CREATE TABLE IF NOT EXISTS tool_connections (
    workspace_id   TEXT NOT NULL,
    member_id      TEXT NOT NULL,
    provider       TEXT NOT NULL,
    server_url     TEXT NOT NULL,
    wrapped_dek    TEXT NOT NULL,
    nonce          TEXT NOT NULL,
    ciphertext     TEXT NOT NULL,
    kms_key_ref    TEXT NOT NULL,
    disabled_tools TEXT NOT NULL DEFAULT '[]',
    created_at     TEXT NOT NULL,
    PRIMARY KEY (workspace_id, member_id, provider)
);
-- The workspace-wide tool switch: tool names no member may be offered,
-- as a JSON array of names.
CREATE TABLE IF NOT EXISTS tool_workspace_settings (
    workspace_id   TEXT NOT NULL PRIMARY KEY,
    disabled_tools TEXT NOT NULL DEFAULT '[]'
);
-- One borrowed write waiting for the owner's approve/deny. The row
-- records the whole call, so resolving it replays exactly what was
-- asked — the gate runs what was recorded, not what resolves it.
CREATE TABLE IF NOT EXISTS tool_pending_approvals (
    approval_id     TEXT NOT NULL PRIMARY KEY,
    workspace_id    TEXT NOT NULL,
    owner_member_id TEXT NOT NULL,
    asker_member_id TEXT NOT NULL,
    provider        TEXT NOT NULL,
    tool_name       TEXT NOT NULL,
    arguments       TEXT NOT NULL,
    created_at      TEXT NOT NULL
);
