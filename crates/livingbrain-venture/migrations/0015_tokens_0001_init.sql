-- Personal access tokens (issue #72): one row per token a member minted
-- for a machine they own — the CLI, a coding agent.
--
-- The token itself is never stored. What is stored is
-- `secret_hash`, the lowercase-hex SHA-256 of the whole
-- `{prefix}_{secret}` string, re-derived on every request and compared in
-- constant time; `prefix` is public and is the lookup key, the way the
-- public half of an API key is. A dump, a backup or a query log therefore
-- holds nothing that can be replayed as a bearer credential.
--
-- `workspace_id` and `user_id` are kept apart rather than concatenated
-- into one subject: both are queried (a member's own tokens are listed
-- and revoked by the pair, and a token that names a member who has since
-- left the workspace is refused by looking the member row up).
--
-- `scopes` is a space-separated subset of the access model's scope
-- vocabulary; the empty string means "no subset" — the token carries
-- every scope its member holds at the moment it is used.
--
-- All columns are TEXT and NULLable timestamps (the portable subset,
-- ADR 0004); timestamps are ISO-8601 strings, which order
-- chronologically as strings.
CREATE TABLE IF NOT EXISTS personal_access_tokens (
    prefix       TEXT PRIMARY KEY, -- lbp_<16 hex>, public, shown to the owner
    secret_hash  TEXT NOT NULL,    -- SHA-256 of the whole token, hex
    workspace_id TEXT NOT NULL,
    user_id      TEXT NOT NULL,
    name         TEXT NOT NULL DEFAULT '',
    scopes       TEXT NOT NULL DEFAULT '',
    created_at   TEXT NOT NULL,
    revoked_at   TEXT
);

-- Listing and revoking a member's own tokens is the only query that
-- filters on the pair, and it is the one every settings page runs.
CREATE INDEX IF NOT EXISTS personal_access_tokens_member
    ON personal_access_tokens (workspace_id, user_id);
