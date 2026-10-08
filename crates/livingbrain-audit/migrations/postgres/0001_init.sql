-- The settings audit log (issue #30), Postgres form. Identical to
-- migrations/sqlite/0001_init.sql: this schema is the portable subset
-- (ADR 0004) — TEXT columns, 0/1 INTEGER flags, ISO-8601 timestamps, no
-- `AUTOINCREMENT` and no `SERIAL` — so nothing here is dialect-specific.
-- The set is shipped anyway so the parity job applies a postgres migration
-- rather than falling back to the sqlite one, and so a later
-- dialect-specific change has a file to land in without renumbering.
CREATE TABLE IF NOT EXISTS settings_audit (
    id           TEXT NOT NULL,          -- ULID, unique
    seq          INTEGER NOT NULL,       -- 1, 2, 3… per workspace; the ordering key
    workspace_id TEXT NOT NULL,
    section      TEXT NOT NULL,          -- models|proactivity|tools|automations|skills|colonizer
    setting_key  TEXT NOT NULL,
    change       TEXT NOT NULL,          -- set|secret|cleared
    value        TEXT NOT NULL DEFAULT '',
    redacted     INTEGER NOT NULL DEFAULT 0,
    actor_kind   TEXT NOT NULL,          -- 'human' | 'brain'
    actor        TEXT NOT NULL,          -- the actor's user id, or 'brain'
    request_id   TEXT,
    recorded_at  TEXT NOT NULL,          -- RFC 3339
    PRIMARY KEY (id),
    UNIQUE (workspace_id, seq)
);

CREATE INDEX IF NOT EXISTS settings_audit_by_workspace
    ON settings_audit (workspace_id, seq);