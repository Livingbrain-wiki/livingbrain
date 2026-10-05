-- Per-scope envelope keys and the blind index (issue #43, ADR 0003).
--
-- A page body is sealed under a 32-byte data key that never leaves this
-- database: `scope_keys` holds it wrapped by the KMS (base64), and
-- `page_terms` holds one keyed HMAC per distinct word of a page's current
-- body, so search never needs the body back. Same portable subset as
-- 0001 — all TEXT and INTEGER, timestamps ISO-8601 — so Postgres runs this
-- file unchanged and no postgres override ships.
--
-- This migration is also where 0001's immutability note is qualified. 0001
-- says no `page_versions` row is ever updated or deleted while the page
-- lives, so every version stays recoverable; that still holds, except for
-- the single column added below: `page_versions.key_version`, which a key
-- rotation updates as it re-seals. The body itself is still never
-- rewritten, only re-wrapped under a newer data key.

-- One row per (scope, key version). Versions are monotonic per scope and
-- never reused, so an envelope written under version 7 stays readable
-- across any number of rotations.
--
-- `wrapped_dek` NULL is the whole of crypto-shredding: the ciphertext stays
-- in R2 and every body in it becomes permanently unreadable, without a
-- rewrite of the blobs. `retired_at` is set only by a rotation that has
-- finished re-sealing; a row NULL in both was *shredded*, which is how
-- `PageStore::forget_scope` is told apart from a scope that was merely
-- rotated past. `kms_provider`/`kms_key_ref` travel with the wrapped key so
-- changing custodian is a re-wrap, not a schema change. The primary key
-- already indexes `(scope, key_version)`, so there is no index to add.
CREATE TABLE IF NOT EXISTS scope_keys (
    scope        TEXT NOT NULL,   -- workspace/team namespace
    key_version  INTEGER NOT NULL,-- 1, then monotonic; never reused
    kms_provider TEXT NOT NULL,   -- 'local-file' | 'worker-secret' | …
    kms_key_ref  TEXT NOT NULL,   -- the vendor's name for the master key
    wrapped_dek  TEXT,            -- base64 of Kms::wrap output; NULL once destroyed
    created_at   TEXT NOT NULL,   -- RFC 3339
    retired_at   TEXT,            -- RFC 3339, set when a rotation retired it
    PRIMARY KEY (scope, key_version)
);

-- The blind index: one row per (page, distinct token) of the page's *current*
-- body, so a search is a keyed equality over this table and never reads an
-- object. `term` is hex(HMAC-SHA256(blind index key, token)), which leaks
-- token equality inside one scope and nothing at all across scopes. A page's
-- rows are replaced on every write, alongside the blob, and a re-encryption
-- pass replaces them again under the new key — so which key version made a
-- row is a fact about the past that a search re-derives for itself, and is
-- deliberately not stored.
CREATE TABLE IF NOT EXISTS page_terms (
    scope       TEXT NOT NULL,
    term        TEXT NOT NULL,   -- hex HMAC, not the word
    slug        TEXT NOT NULL,
    PRIMARY KEY (scope, term, slug)
);

CREATE INDEX IF NOT EXISTS page_terms_by_page ON page_terms (scope, slug);

-- Which scope key version a body is sealed under, recorded on the version row
-- rather than trusted from the envelope's own header. The header is what a
-- re-encryption pass rewrites, so believing it would mean believing the thing
-- the pass exists to fix; D1 is the side both the writer and the reader can
-- defend, and a body whose row and header disagree fails to open rather than
-- being silently rewritten. `NOT NULL DEFAULT 0` so the statement runs on a
-- table that already holds rows: 0 means "written before this column existed",
-- which a re-encryption pass reads as "not on any live key" and re-seals.
ALTER TABLE page_versions ADD COLUMN key_version INTEGER NOT NULL DEFAULT 0;
