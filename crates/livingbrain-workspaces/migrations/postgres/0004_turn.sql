-- The speak-up policy and its cost ledger (issue #9).
--
-- `turn_policies` is how eagerly a workspace lets the brain join a
-- conversation that did not name it: one row per channel that has chosen,
-- plus one org-wide row whose channel id is the empty string. The channel
-- row wins when there is one; no row at all means Off — the behaviour the
-- agent loop had before this table existed.
--
-- `turn_triage` is one row per message that reached the classifier. It
-- names the message by the platform's own ids and carries no text and no
-- author; tokens are the cratefield estimate, and `token_source` says so.
--
-- The portable subset (ADR 0004) throughout: TEXT columns, no dialect
-- type, `ON CONFLICT … DO UPDATE` spelled the way both engines accept.
-- The Postgres file is this one byte for byte.
CREATE TABLE IF NOT EXISTS turn_policies (
    workspace_id TEXT NOT NULL,
    channel_id   TEXT NOT NULL, -- '' is the org-wide default
    proactivity  TEXT NOT NULL, -- the judge's snake_case name
    updated_at   TEXT NOT NULL,
    PRIMARY KEY (workspace_id, channel_id)
);

CREATE TABLE IF NOT EXISTS turn_triage (
    id               TEXT PRIMARY KEY, -- a ULID, from the IdGen port
    workspace_id     TEXT NOT NULL,
    conversation_key TEXT NOT NULL,    -- platform:team:channel:thread
    message_id       TEXT NOT NULL,    -- the platform's own message id
    addressed        INTEGER NOT NULL, -- 1 = mention or DM
    proactivity      TEXT NOT NULL,    -- the policy in force for the turn
    decision         TEXT NOT NULL,    -- 'reply' | 'react' | 'silent'
    calibration      TEXT NOT NULL,    -- the classifier profile's family
    input_tokens     INTEGER NOT NULL,
    output_tokens    INTEGER NOT NULL,
    token_source     TEXT NOT NULL,    -- 'estimated' today, always
    recorded_at      TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS turn_triage_by_workspace
    ON turn_triage (workspace_id, recorded_at);
