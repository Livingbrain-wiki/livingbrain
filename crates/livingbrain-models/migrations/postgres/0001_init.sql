-- Model connections (issue #10), Postgres form. Identical to
-- migrations/sqlite/0001_init.sql: this schema is the portable subset
-- (ADR 0004) — TEXT columns, 0/1 INTEGER flags, BLOB ciphertext,
-- ISO-8601 timestamps — so nothing here is dialect-specific. The set is
-- shipped anyway so the parity job applies a postgres migration rather
-- than falling back to the sqlite one, and so a later dialect-specific
-- change has a file to land in without renumbering.
CREATE TABLE IF NOT EXISTS model_connections (
    workspace_id        TEXT NOT NULL,
    role                TEXT NOT NULL,
    provider            TEXT NOT NULL,
    base_url            TEXT NOT NULL,
    model               TEXT NOT NULL,
    key_ciphertext      BYTEA NOT NULL,
    key_last4           TEXT NOT NULL,
    fallback_to_managed INTEGER NOT NULL DEFAULT 0,
    status              TEXT NOT NULL,
    missing             TEXT NOT NULL,
    context_size        TEXT,
    checked_at          TEXT NOT NULL,
    updated_by          TEXT NOT NULL,
    PRIMARY KEY (workspace_id, role)
);
