-- The source ledger (issue #81): one row per distinct imported file.
--
-- A source is a file that came in from outside — an Obsidian vault, a
-- documentation site, a checkout — as opposed to a page, which this brain
-- wrote. Its body is a Markdown file sealed exactly the way a page body is
-- (issue #43), so the two properties the pages table holds are kept: the
-- identifying content is in the body, and forgetting a scope destroys the key
-- that opens it.
--
-- Same portable subset as 0001 and 0002 — all TEXT and INTEGER, timestamps
-- ISO-8601 — so Postgres runs this file unchanged and no postgres override
-- ships.

-- One row per (scope, body hash). The body is the identity, not the path: an
-- import that lands the same bytes twice, under two paths or two runs, is one
-- source, and the second import reads the first one's row back rather than
-- sealing a second copy of the same text.
--
-- `body_key` names the sealed body in the blob store and `key_version` the
-- scope key version it was sealed under, so a body written before a rotation
-- is still readable after one. `wikilinks` is a JSON array of the `[[links]]`
-- the file names, kept with the row because an importer resolves them before
-- anything here is a page.
CREATE TABLE IF NOT EXISTS sources (
    scope        TEXT NOT NULL,   -- workspace/team namespace
    body_sha256  TEXT NOT NULL,   -- hex sha256 of the redacted body
    id           TEXT NOT NULL,   -- ULID, assigned on first import
    kind         TEXT NOT NULL,   -- 'import'
    rel_path     TEXT NOT NULL,   -- the path it was imported from, relative
    wikilinks    TEXT NOT NULL,   -- JSON array of the [[link]] targets
    body_key     TEXT NOT NULL,   -- the sealed body in the blob store
    key_version  INTEGER NOT NULL,-- the scope key it is sealed under
    imported_by  TEXT NOT NULL,   -- the importer's user id
    created_at   TEXT NOT NULL,   -- RFC 3339
    PRIMARY KEY (scope, body_sha256)
);