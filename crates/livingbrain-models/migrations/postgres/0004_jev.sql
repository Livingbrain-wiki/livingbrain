-- Jev (TypeSafe's System One), the coding-agents compiler's optional triage
-- classifier, per workspace (Settings -> Models -> Jev).
--
-- One row per workspace that changed anything; no row means the defaults:
-- on, the deployment's TYPESAFE_API_KEY if it has one, the adapter's pinned
-- model. `enabled` = 0 turns Jev off for the workspace even when the
-- deployment carries a key (the compiler's free triage rules still run).
-- `model` NULL is the default model. `key_ciphertext` is the workspace's own
-- TypeSafe key, AES-256-GCM under MODEL_KEYS_SECRET exactly like
-- model_connections.key_ciphertext, bound to (workspace_id, 'jev'); NULL means
-- no workspace key. `key_last4` is the only part of it ever shown.
CREATE TABLE IF NOT EXISTS jev_settings (
    workspace_id   TEXT NOT NULL PRIMARY KEY,
    enabled        INTEGER NOT NULL DEFAULT 1,
    model          TEXT,
    key_ciphertext BYTEA,
    key_last4      TEXT,
    updated_by     TEXT NOT NULL,
    updated_at     TEXT NOT NULL
);
