-- Personal access tokens (issue #72), Postgres form: identical to
-- migrations/sqlite/0001_init.sql, because the schema is the portable subset
-- (ADR 0004). Shipped anyway so the parity job applies a postgres migration
-- rather than falling back to the sqlite one, and so a later dialect-specific
-- change has a file to land in without renumbering.
CREATE TABLE IF NOT EXISTS personal_access_tokens (
    prefix       TEXT PRIMARY KEY,
    secret_hash  TEXT NOT NULL,
    workspace_id TEXT NOT NULL,
    user_id      TEXT NOT NULL,
    name         TEXT NOT NULL DEFAULT '',
    scopes       TEXT NOT NULL DEFAULT '',
    created_at   TEXT NOT NULL,
    revoked_at   TEXT
);

CREATE INDEX IF NOT EXISTS personal_access_tokens_member
    ON personal_access_tokens (workspace_id, user_id);