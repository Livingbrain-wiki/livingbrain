-- The Discord connection's storage (docs/discord.md).
--
-- Every instant is INTEGER unix seconds, so expiry, leases and "due" checks
-- compare in SQL. Discord ids are snowflakes, which exceed what a JavaScript
-- number holds exactly, so they are stored and returned as TEXT.
--
-- The portable subset (harness ADR 0004): TEXT and INTEGER only. The Postgres
-- file is this schema with every INTEGER spelled BIGINT.

-- Single-use state for the connect round trip. Only the SHA-256 of the token
-- is stored; spending it is a compare-and-set on spent_at.
CREATE TABLE IF NOT EXISTS discord_states (
    state_hash   TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL,
    created_by   TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    expires_at   INTEGER NOT NULL,
    spent_at     INTEGER
);

CREATE INDEX IF NOT EXISTS discord_states_by_expiry ON discord_states (expires_at);

-- A Discord server (guild) the bot was added to, for one workspace. A server
-- belongs to one workspace at a time; disconnecting stops every read of it.
CREATE TABLE IF NOT EXISTS discord_connections (
    guild_id           TEXT PRIMARY KEY,
    workspace_id       TEXT NOT NULL,
    guild_name         TEXT NOT NULL,
    connected_by       TEXT,              -- member id; NULL once erased
    connected_at       INTEGER NOT NULL,
    disconnected_at    INTEGER,           -- set: the drain ignores this server
    channels_synced_at INTEGER,           -- when the channel list was last read
    last_error         TEXT,              -- a status and a fixed phrase, never a body
    last_error_at      INTEGER,
    updated_at         INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS discord_connections_by_workspace ON discord_connections (workspace_id);

-- The readable channels and threads of a connected server, with their
-- backfill and live cursors. Written only by the cron drain, except
-- `included`, which an admin sets.
CREATE TABLE IF NOT EXISTS discord_channels (
    guild_id       TEXT NOT NULL,
    channel_id     TEXT NOT NULL,
    workspace_id   TEXT NOT NULL,
    name           TEXT NOT NULL DEFAULT '',
    kind           INTEGER NOT NULL,      -- Discord channel type
    nsfw           INTEGER NOT NULL DEFAULT 0,
    parent_id      TEXT,                  -- the text channel a thread hangs off
    included       INTEGER NOT NULL DEFAULT 1,
    present        INTEGER NOT NULL DEFAULT 1,  -- 0 once Discord no longer lists it
    backfill_state TEXT NOT NULL DEFAULT 'pending', -- 'pending' | 'running' | 'done'
    oldest_id      TEXT,                  -- backfill cursor: the next page is `before` this
    newest_id      TEXT,                  -- live cursor: polling reads `after` this
    messages       INTEGER NOT NULL DEFAULT 0,  -- messages read so far
    archive_state  TEXT NOT NULL DEFAULT 'pending', -- archived-thread listing, text channels
    archive_before TEXT,                  -- the archived-thread listing's next `before`
    lease_token    TEXT,                  -- a drain run holds the channel while it works
    lease_until    INTEGER,
    polled_at      INTEGER,
    retry_at       INTEGER,               -- no read before this (after a refusal)
    last_error     TEXT,                  -- a status and a fixed text, never a body
    last_error_at  INTEGER,
    updated_at     INTEGER NOT NULL,
    PRIMARY KEY (guild_id, channel_id)
);

CREATE INDEX IF NOT EXISTS discord_channels_by_workspace ON discord_channels (workspace_id);

-- One row per channel-day (UTC) that has messages. `dirty` means the day
-- changed since it was last filed; the drain re-reads a dirty day and files
-- its full transcript. The transcript itself is never stored here.
CREATE TABLE IF NOT EXISTS discord_days (
    guild_id     TEXT NOT NULL,
    channel_id   TEXT NOT NULL,
    workspace_id TEXT NOT NULL,
    day          TEXT NOT NULL,           -- YYYY-MM-DD, UTC
    state        TEXT NOT NULL DEFAULT 'dirty', -- 'dirty' | 'filed'
    filed_at     INTEGER,
    updated_at   INTEGER NOT NULL,
    PRIMARY KEY (guild_id, channel_id, day)
);

CREATE INDEX IF NOT EXISTS discord_days_by_state ON discord_days (state, updated_at);
