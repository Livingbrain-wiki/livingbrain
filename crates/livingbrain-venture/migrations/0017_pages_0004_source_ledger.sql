-- The source ledger widens into the ingest ledger (issue #77): one row per
-- distinct thing that arrived from outside, of any kind — chat, mail, agent
-- logs, imports, git, monitoring, radar, notes — not only imported files.
--
-- Every new column is one the import-only ledger could already answer for,
-- asked generally: which workspace the source belongs to (the folded page
-- scope hashes the workspace away, so a citation resolver needs the plain
-- id back), where the source came from (a permalink or a message id — the
-- import's `rel_path` stays, because a file path is filing, not an origin),
-- the authoring member when one is known (a message sender; NULL when the
-- source arrived with no member behind it), and whether screening said hold.
--
-- Same portable subset as 0001–0003 — all TEXT and INTEGER, timestamps
-- ISO-8601 — so Postgres runs this file unchanged and no postgres override
-- ships. `held` is 0 for every row this migration finds: what arrived before
-- screening existed arrived passed.

-- The workspace the folded scope belongs to. The scope names it only as a
-- hash (see `page_scope`), and `GET /v1/sources/:id` must answer a member of
-- one workspace with a flat no about another's source — which needs the id,
-- not the hash.
ALTER TABLE sources ADD COLUMN workspace TEXT NOT NULL DEFAULT '';

-- Where the source came from, when it has one name: a permalink, a message
-- id. Nullable — a paste has none. Redacted before it is stored, like the
-- body: free text is a place secrets end up.
ALTER TABLE sources ADD COLUMN origin_ref TEXT;

-- The member who authored the source, when the ledger knows one. Nullable:
-- an imported file's author is whoever wrote it outside the workspace, which
-- is nobody the member table names. (The importer keeps `imported_by`.)
ALTER TABLE sources ADD COLUMN author TEXT;

-- Screening's verdict (issue #50's hook, plumbed by #77): a held source is
-- stored, never extracted, until it is released. 0 is the default both for
-- new kinds that arrive passed and for every row already in the table.
ALTER TABLE sources ADD COLUMN held INTEGER NOT NULL DEFAULT 0;

-- `GET /v1/sources/:id` looks a source up by id alone and then decides from
-- the row whether the asker may read it, so the id needs an index of its own
-- — the primary key answers lookups by (scope, hash), not by id.
CREATE INDEX IF NOT EXISTS sources_by_id ON sources (id);
