-- The GitHub connection's storage (docs/github.md, "Storage & secrets").
--
-- Every instant is INTEGER unix seconds, not RFC 3339 text: expiry, replay
-- pruning and queue leases are compared in SQL, and a fixed-width integer
-- compares correctly where text with optional fractional seconds does not.
--
-- The portable subset (harness ADR 0004): TEXT and INTEGER only. The
-- Postgres file is this schema with every INTEGER spelled BIGINT (GitHub
-- ids and unix seconds both outgrow a Postgres INTEGER).

-- One GitHub App per workspace, created by the manifest flow. The three
-- credentials are sealed: one KMS-wrapped data key per row, and each
-- credential under its own nonce with an AAD naming its purpose, the
-- workspace and the app id, so no ciphertext opens in another column or
-- another row. Nothing here is ever returned by a route.
CREATE TABLE IF NOT EXISTS github_app (
    app_id                INTEGER PRIMARY KEY,
    workspace_id          TEXT NOT NULL UNIQUE,
    slug                  TEXT NOT NULL,
    client_id             TEXT NOT NULL,
    wrapped_dek           TEXT NOT NULL,
    kms_key_ref           TEXT NOT NULL,
    private_key_nonce     TEXT NOT NULL,
    private_key_sealed    TEXT NOT NULL,
    webhook_secret_nonce  TEXT NOT NULL,
    webhook_secret_sealed TEXT NOT NULL,
    client_secret_nonce   TEXT NOT NULL,
    client_secret_sealed  TEXT NOT NULL,
    created_by            TEXT,             -- member id; NULL once erased
    created_at            INTEGER NOT NULL
);

-- Single-use state tokens for the manifest and install round trips. Only
-- the SHA-256 of a token is stored; spending one is a compare-and-set on
-- spent_at, so a token is good for exactly one callback.
CREATE TABLE IF NOT EXISTS github_states (
    state_hash   TEXT PRIMARY KEY,
    purpose      TEXT NOT NULL,             -- 'manifest' | 'install'
    workspace_id TEXT NOT NULL,
    created_by   TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    expires_at   INTEGER NOT NULL,
    spent_at     INTEGER
);

CREATE INDEX IF NOT EXISTS github_states_by_expiry ON github_states (expires_at);

-- An installation of a workspace's app on a GitHub account. A row may be
-- seen (a verified webhook) before it is bound; workspace_id is set once,
-- by an owner or admin, and never moved to another workspace.
CREATE TABLE IF NOT EXISTS github_installations (
    installation_id INTEGER PRIMARY KEY,
    app_id          INTEGER NOT NULL,
    workspace_id    TEXT,                   -- NULL until bound
    account_login   TEXT NOT NULL DEFAULT '',
    account_type    TEXT NOT NULL DEFAULT '', -- 'User' | 'Organization'
    status          TEXT NOT NULL DEFAULT 'active', -- 'active' | 'suspended' | 'deleted'
    bound_by        TEXT,                   -- member id; NULL once erased
    bound_at        INTEGER,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS github_installations_by_workspace
    ON github_installations (workspace_id);

-- The repositories an installation reaches, with the workspace denormalised
-- so every read filters on it. The backfill columns are written only by
-- the in-Worker cron drain.
CREATE TABLE IF NOT EXISTS github_repos (
    installation_id      INTEGER NOT NULL,
    repo_id              INTEGER NOT NULL,
    workspace_id         TEXT NOT NULL,
    full_name            TEXT NOT NULL,
    project              TEXT NOT NULL,
    scope                TEXT NOT NULL DEFAULT 'shared',
    included             INTEGER NOT NULL DEFAULT 1,
    active               INTEGER NOT NULL DEFAULT 1,  -- 0 once removed, suspended or deleted
    backfill_state       TEXT NOT NULL DEFAULT 'pending', -- 'pending' | 'running' | 'done'
    backfill_cursor      TEXT,              -- opaque JSON, the drain's own
    backfill_items       INTEGER NOT NULL DEFAULT 0,
    backfill_batches     INTEGER NOT NULL DEFAULT 0,
    backfill_started_at  INTEGER,
    backfill_finished_at INTEGER,
    removed_at           INTEGER,              -- GitHub took it out of the installation
    last_webhook_at      INTEGER,
    last_error           TEXT,
    last_error_at        INTEGER,
    created_at           INTEGER NOT NULL,
    updated_at           INTEGER NOT NULL,
    PRIMARY KEY (installation_id, repo_id)
);

CREATE INDEX IF NOT EXISTS github_repos_by_workspace ON github_repos (workspace_id);

-- Webhook deliveries already accepted, for replay protection. A delivery
-- signature carries no timestamp, so this table is the only thing that
-- tells a redelivered body from a new one; rows are pruned after the
-- retention window.
CREATE TABLE IF NOT EXISTS github_deliveries (
    app_id      INTEGER NOT NULL,
    delivery_id TEXT NOT NULL,
    event       TEXT NOT NULL,
    received_at INTEGER NOT NULL,
    PRIMARY KEY (app_id, delivery_id)
);

CREATE INDEX IF NOT EXISTS github_deliveries_by_age ON github_deliveries (received_at);

-- Work a verified webhook leaves for the cron drain. A row is a reference
-- (kind + subject, e.g. 'issues' + '42'), never a copy of GitHub content:
-- the drain re-reads the current state, so a replayed or reordered event
-- costs a refetch and nothing else.
CREATE TABLE IF NOT EXISTS github_queue (
    id              TEXT PRIMARY KEY,
    workspace_id    TEXT NOT NULL,
    installation_id INTEGER NOT NULL,
    repo_id         INTEGER,
    kind            TEXT NOT NULL,
    subject         TEXT NOT NULL DEFAULT '',
    enqueued_at     INTEGER NOT NULL,
    available_at    INTEGER NOT NULL,
    attempts        INTEGER NOT NULL DEFAULT 0,
    dead            INTEGER NOT NULL DEFAULT 0,
    lease_token     TEXT,
    lease_until     INTEGER,
    last_error      TEXT
);

CREATE INDEX IF NOT EXISTS github_queue_ready ON github_queue (dead, available_at);
