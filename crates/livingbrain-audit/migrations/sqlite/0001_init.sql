-- The settings audit log (issue #30): one row per settings change, forever.
-- The web app's acceptance criterion is "All settings changes are audited",
-- so this table is append-only — nothing here is ever updated, and the only
-- statement that removes a row is the subject's own erasure.
--
-- All columns are TEXT and 0/1 INTEGER flags (the portable subset,
-- ADR 0004); timestamps are ISO-8601 strings, so the same file runs
-- unchanged on Postgres.
--
-- `value` holds the setting's value **after** `livingbrain-redact` has run
-- over it, never the bytes a member typed. A secret the detectors know
-- comes back as its `[REDACTED:<class>]` token; a value under a key that
-- names a credential (`api_key`, `token`, ...) is never handed to this
-- table at all (`SettingChange::secret`). `redacted` is 1 when a detector
-- actually changed the value, so a reader can tell "this was `daily`" from
-- "this was a key and we did not keep it".
--
-- `id` is a ULID from the `IdGen` port, so it is unique without a database
-- sequence. It is deliberately **not** the ordering key: `UlidIdGen` fills
-- a ULID's random component randomly, so two changes in the same
-- millisecond do not sort by write order. `seq` is the ordering key — a
-- per-workspace counter, so "newest first" means what it says rather than
-- approximately what it says.
CREATE TABLE IF NOT EXISTS settings_audit (
    id           TEXT NOT NULL,          -- ULID, unique
    seq          INTEGER NOT NULL,       -- 1, 2, 3… per workspace; the ordering key
    workspace_id TEXT NOT NULL,          -- the workspace the settings belong to
    section      TEXT NOT NULL,          -- models|proactivity|tools|automations|skills|colonizer
    setting_key  TEXT NOT NULL,          -- the key inside the section
    change       TEXT NOT NULL,          -- set|secret|cleared
    value        TEXT NOT NULL DEFAULT '', -- the redacted value; '' when cleared
    redacted     INTEGER NOT NULL DEFAULT 0, -- 1 when a detector changed the value
    actor_kind   TEXT NOT NULL,          -- 'human' | 'brain'
    actor        TEXT NOT NULL,          -- the actor's user id, or 'brain'
    request_id   TEXT,                   -- ADR 0007 request id, when there is one
    recorded_at  TEXT NOT NULL,          -- RFC 3339
    PRIMARY KEY (id),
    UNIQUE (workspace_id, seq)
);

-- The one query the web app's settings screen makes, and the one every
-- listing runs: one workspace's entries, newest first.
CREATE INDEX IF NOT EXISTS settings_audit_by_workspace
    ON settings_audit (workspace_id, seq);