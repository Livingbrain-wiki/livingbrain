-- The local runner: "jobs for me run on my device" (docs/local-runner.md).
--
-- A consumer subscription (Claude Pro/Max, ChatGPT Plus/Pro) may only be
-- used inside its vendor's own client, so its credential never reaches this
-- server. Instead a member says, per job slot, that *their* jobs run on
-- their own machine, where their own `claude` / `codex` CLI makes the call.
--
-- `model_runners`: one row per workspace, member and role. Any member sets
-- their own; it spends nothing of the workspace's. `agent` is `claude` or
-- `codex`, `model` the CLI's model alias or id ('' = the CLI's default),
-- `settings` `isolated` (the CLI ignores the user's own settings) or `user`.
-- `last_seen_at` is when that member's runner last asked for work.
CREATE TABLE IF NOT EXISTS model_runners (
    workspace_id TEXT NOT NULL,
    user_id      TEXT NOT NULL,
    role         TEXT NOT NULL,
    kind         TEXT NOT NULL,
    agent        TEXT NOT NULL,
    model        TEXT NOT NULL,
    settings     TEXT NOT NULL,
    updated_at   TEXT NOT NULL,
    last_seen_at TEXT,
    PRIMARY KEY (workspace_id, user_id, role)
);

-- `model_jobs`: one model call handed to a member's runner. The prompt
-- (`payload`) and the answer (`result`) are AES-256-GCM under the
-- deployment's MODEL_KEYS_SECRET, bound to (workspace_id, 'job:' || id), and
-- both are wiped (set NULL) once the job is consumed, failed or expired: the
-- row then holds counts and statuses only. `ref_key` is the caller's handle
-- for the work the call belongs to (a compile job and chunk, an ask);
-- `prompt_sha256` lets the same call find its own answer again.
-- Statuses: queued, claimed, done, failed, consumed, expired.
CREATE TABLE IF NOT EXISTS model_jobs (
    id                TEXT NOT NULL PRIMARY KEY,
    workspace_id      TEXT NOT NULL,
    owner             TEXT NOT NULL,
    role              TEXT NOT NULL,
    kind              TEXT NOT NULL,
    ref_key           TEXT NOT NULL,
    prompt_sha256     TEXT NOT NULL,
    agent             TEXT NOT NULL,
    model             TEXT NOT NULL,
    settings          TEXT NOT NULL,
    max_tokens        INTEGER NOT NULL,
    json              INTEGER NOT NULL DEFAULT 0,
    payload           BLOB,
    result            BLOB,
    status            TEXT NOT NULL,
    attempts          INTEGER NOT NULL DEFAULT 0,
    lease_until       TEXT,
    error             TEXT,
    prompt_tokens     INTEGER NOT NULL DEFAULT 0,
    completion_tokens INTEGER NOT NULL DEFAULT 0,
    created_at        TEXT NOT NULL,
    updated_at        TEXT NOT NULL,
    expires_at        TEXT NOT NULL
);

-- A runner's claim: the owner's live jobs, oldest first.
CREATE INDEX IF NOT EXISTS model_jobs_by_owner
    ON model_jobs (workspace_id, owner, status, created_at);

-- A caller finding its own call again.
CREATE INDEX IF NOT EXISTS model_jobs_by_ref
    ON model_jobs (workspace_id, owner, kind, ref_key);
