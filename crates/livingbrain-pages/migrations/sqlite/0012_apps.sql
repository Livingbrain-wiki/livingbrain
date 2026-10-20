-- Connected apps (issues #51 and #146): the memories an app keeps for its
-- own end members, and the pages a workspace owner has published to the
-- customer-safe `public` scope.
--
-- Same portable subset as 0001-0011 — all TEXT and INTEGER — so Postgres
-- runs this file unchanged and no postgres override ships.

-- One row per memory an app (EarthOS Aura, say) noted for one of its
-- subjects. `scope` is the subject's page scope (`user-<hash>`, with the
-- workspace and the `<app>:<subject>` id folded into the hash), so a
-- subject's rows are exactly the rows of its scope. What the member chose to
-- keep, and the conversation it came from, is `sealed`: base64 of an
-- envelope under the scope's data key (issue #43), bound to the scope, the
-- id and `key_version`. `workspace_id` and `app` are plaintext because a
-- workspace erasure finds the scopes to shred by them, and a memory is
-- hard-deleted by id (`brain_memory_forget`) or by scope
-- (`brain_memory_forget_all`).
CREATE TABLE IF NOT EXISTS app_memories (
    scope        TEXT NOT NULL,    -- the subject's page scope
    id           TEXT NOT NULL,    -- mem_<ULID>
    workspace_id TEXT NOT NULL,
    app          TEXT NOT NULL,    -- the connected app's name
    sealed       TEXT NOT NULL,    -- base64 envelope of {text, source}
    key_version  INTEGER NOT NULL, -- the scope key it is sealed under
    created_at   TEXT NOT NULL,    -- RFC 3339
    PRIMARY KEY (scope, id)
);

-- A recall reads one scope, newest first.
CREATE INDEX IF NOT EXISTS app_memories_by_scope
    ON app_memories (scope, created_at, id);

-- A workspace erasure finds every subject scope the workspace holds.
CREATE INDEX IF NOT EXISTS app_memories_by_workspace
    ON app_memories (workspace_id);

-- The publication ledger. A page is public only while it has a row here:
-- the published copy lives in the workspace's `public` scope, and this row
-- says an owner put it there, when, and where its citation points.
CREATE TABLE IF NOT EXISTS public_pages (
    scope        TEXT NOT NULL,    -- the workspace's public page scope
    slug         TEXT NOT NULL,
    workspace_id TEXT NOT NULL,
    title        TEXT NOT NULL,    -- shown in a citation; public by construction
    url          TEXT NOT NULL,    -- http(s) address a citation links to
    source_scope TEXT NOT NULL,    -- the scope the copy was taken from
    source_version INTEGER NOT NULL, -- the head version that was copied
    published_by TEXT NOT NULL,    -- the owner who published it
    published_at TEXT NOT NULL,    -- RFC 3339
    PRIMARY KEY (scope, slug)
);

CREATE INDEX IF NOT EXISTS public_pages_by_workspace
    ON public_pages (workspace_id);
