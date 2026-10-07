-- Entity pages. A page is an entity (person, project, decision, customer,
-- system, glossary) whose Markdown body lives in the blob store at an
-- immutable key and whose metadata lives here. All columns are TEXT and
-- INTEGER (the portable subset, ADR 0004); timestamps are ISO-8601 strings.
-- The portable SQL runs unchanged on Postgres, so no postgres override ships.

-- The page head: one row per (scope, slug). `head_version` is the current
-- version; `human_pending_version` is set while a human edit has not been
-- reconciled by the brain, which is what stops the brain overwriting it.
CREATE TABLE IF NOT EXISTS pages (
    scope                 TEXT NOT NULL,   -- workspace/team namespace
    slug                  TEXT NOT NULL,   -- lowercase ascii alnum and `-`
    entity_type           TEXT NOT NULL,   -- person|project|decision|customer|system|glossary
    head_version          INTEGER NOT NULL, -- current version, starts at 1
    human_pending_version INTEGER,          -- NULL when nothing is pending
    updated_at            TEXT NOT NULL,    -- RFC 3339
    PRIMARY KEY (scope, slug)
);

-- One row per version, append-only. `body_key` names the immutable Markdown
-- body; no row here is ever updated or deleted while the page lives, so every
-- version stays recoverable. Two writers racing for the same next version
-- cannot both win: the second insert loses on the primary key and its whole
-- batch rolls back.
CREATE TABLE IF NOT EXISTS page_versions (
    scope        TEXT NOT NULL,
    slug         TEXT NOT NULL,
    version      INTEGER NOT NULL,
    author_kind  TEXT NOT NULL, -- 'human' | 'brain'
    author       TEXT NOT NULL, -- the editor's id, or 'brain'
    base_version INTEGER,       -- the version this write was based on; NULL for the first
    body_key     TEXT NOT NULL,
    created_at   TEXT NOT NULL, -- RFC 3339
    PRIMARY KEY (scope, slug, version)
);

-- Outgoing links, one row per (page, target). A page's links are replaced on
-- every write; the index answers backlinks ("who links here").
CREATE TABLE IF NOT EXISTS page_links (
    scope     TEXT NOT NULL,
    from_slug TEXT NOT NULL,
    to_slug   TEXT NOT NULL,
    PRIMARY KEY (scope, from_slug, to_slug)
);

CREATE INDEX IF NOT EXISTS page_links_by_target ON page_links (scope, to_slug);
