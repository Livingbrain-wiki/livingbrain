-- Personal access tokens (issue #72). The token itself is never stored:
-- `secret_hash` is the lowercase-hex SHA-256 of the whole `{prefix}_{secret}`
-- string, re-derived on every request and compared in constant time, so a
-- dump, a backup or a query log holds nothing replayable as a credential.
--
-- `workspace_id` and `user_id` are kept apart rather than concatenated: both
-- are queried, since a member's own tokens are listed and revoked by the
-- pair, and a token naming a member who has left the workspace is refused by
-- looking the member row up.
--
-- `scopes` is a space-separated subset of the access model's vocabulary; the
-- empty string means "no subset" — every scope the member holds.
--
-- TEXT columns and NULLable timestamps (the portable subset, ADR 0004);
-- ISO-8601 stamps, which order chronologically as strings.
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

-- The only query that filters on the pair, and the one every settings page
-- runs.
CREATE INDEX IF NOT EXISTS personal_access_tokens_member
    ON personal_access_tokens (workspace_id, user_id);