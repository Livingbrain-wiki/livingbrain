-- Model connections (issue #10): one row per workspace and per role.
-- A member with admin or owner rights connects a provider key or a custom
-- OpenAI-compatible endpoint; the module probes it, stores the connection
-- with the key encrypted, and reports the capability check's result.
--
-- All columns are TEXT, 0/1 INTEGER flags and BLOB ciphertext (the
-- portable subset, ADR 0004); timestamps are ISO-8601 strings.
--
-- `key_ciphertext` is AES-256-GCM over the provider key, with a random
-- 12-byte nonce prepended. The AAD binds it to `(workspace_id, role)`, so
-- a ciphertext lifted from one row cannot decrypt in another. The key is
-- never stored in plaintext, and `key_last4` is the only thing shown.
CREATE TABLE IF NOT EXISTS model_connections (
    workspace_id        TEXT NOT NULL,
    role                TEXT NOT NULL,
    provider            TEXT NOT NULL,
    base_url            TEXT NOT NULL,
    model               TEXT NOT NULL,
    key_ciphertext      BLOB NOT NULL,
    key_last4           TEXT NOT NULL,
    fallback_to_managed INTEGER NOT NULL DEFAULT 0,
    status              TEXT NOT NULL,
    missing             TEXT NOT NULL,
    context_size        TEXT,
    checked_at          TEXT NOT NULL,
    updated_by          TEXT NOT NULL,
    PRIMARY KEY (workspace_id, role)
);
