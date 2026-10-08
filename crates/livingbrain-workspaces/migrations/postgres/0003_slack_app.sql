-- The Slack app as a bot (issue #6): the Events API's dedup ledger and one
-- sealed bot token per workspace that installed it.
--
-- Both tables are additive and a deployment with no Slack app never writes
-- either: the first is claimed only by a *verified* delivery, and the second
-- only by an install callback Slack answered.
--
-- `workspaces_slack_inbox` is `cratefield_core::Inbox::create_table_sql()`
-- verbatim — the DDL the module's own claim statement is written against,
-- so the migration and the code cannot drift.
--
-- The portable subset (ADR 0004) throughout: TEXT columns, no dialect type,
-- no `AUTOINCREMENT`, `ON CONFLICT … DO UPDATE` spelled the way both
-- engines accept. The Postgres file is this one byte for byte.
CREATE TABLE IF NOT EXISTS workspaces_slack_inbox (
    event_key TEXT PRIMARY KEY,
    seen_at TEXT NOT NULL
);

-- One row per team that installed the bot. The token is never stored in the
-- clear: `wrapped_dek` is a KMS-wrapped data key, and the token is sealed
-- under it with XChaCha20-Poly1305, bound to the team by its AAD — so a row
-- copied from one team's install into another's does not decrypt. Base64
-- rather than BLOB, so one file serves both dialects and a value read out of
-- the database is printable for debugging without being a credential.
CREATE TABLE IF NOT EXISTS slack_installs (
    team_id       TEXT PRIMARY KEY, -- Slack team id, as oauth.v2.access reports it
    app_id        TEXT NOT NULL,
    bot_user_id   TEXT NOT NULL,
    wrapped_dek   TEXT NOT NULL,    -- base64, never the key itself
    nonce         TEXT NOT NULL,    -- base64, XChaCha20's 24 bytes
    ciphertext    TEXT NOT NULL,    -- base64, the sealed xoxb- token and its tag
    kms_key_ref   TEXT NOT NULL,    -- which master key wrapped the data key
    installed_at  TEXT NOT NULL
);