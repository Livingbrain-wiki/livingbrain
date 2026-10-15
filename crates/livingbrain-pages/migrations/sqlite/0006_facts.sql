-- Temporal facts (docs/temporal-facts.md, format v1): one row per fact.
--
-- A fact is `attribute(subject) = value`, true in the world over
-- `[valid_from, valid_to)` and learned by the brain at `observed_at`. A new
-- value never overwrites an old one: the old row's `valid_to`, `status` and
-- `superseded_by` are updated and the row stays, so a timeline is history.
--
-- `subject` (`<entity_type>/<slug>`) and `attribute` are plaintext, like a
-- page slug, because a timeline is selected by them. What a person said —
-- value, unit, source, quote — is `sealed`: base64 of an envelope under the
-- scope's data key (issue #43), bound to the scope, the fact id and
-- `key_version`. Forgetting a scope deletes these rows and destroys the key.
--
-- Dates are kept twice: as written (`2026`, `2026-10`, `2026-10-15`, or an
-- RFC 3339 instant), which is what is shown, and as the RFC 3339 instant the
-- period starts at (`*_at`), which is what sorts. `confidence` is per mille.
--
-- Same portable subset as 0001-0005 — all TEXT and INTEGER — so Postgres runs
-- this file unchanged and no postgres override ships.
CREATE TABLE IF NOT EXISTS facts (
    scope           TEXT NOT NULL,    -- page scope, workspace folded in
    id              TEXT NOT NULL,    -- fact-<ULID>
    subject         TEXT NOT NULL,    -- <entity_type>/<slug>
    attribute       TEXT NOT NULL,    -- normalised snake_case key
    cardinality     TEXT NOT NULL,    -- 'one' | 'many'
    valid_from      TEXT NOT NULL,    -- partial date, as written
    valid_from_at   TEXT NOT NULL,    -- RFC 3339 start of valid_from
    stated_valid_to TEXT,             -- the valid_to the writer stated
    valid_to        TEXT,             -- effective end (exclusive); NULL = open
    valid_to_at     TEXT,             -- RFC 3339 start of valid_to
    observed_at     TEXT NOT NULL,    -- RFC 3339, when the brain learned it
    status          TEXT NOT NULL,    -- current|superseded|retracted|disputed
    supersedes      TEXT,             -- fact id
    superseded_by   TEXT,             -- fact id
    confidence      INTEGER NOT NULL, -- 0..=1000
    sealed          TEXT NOT NULL,    -- base64 envelope of {value, unit, source, quote}
    key_version     INTEGER NOT NULL, -- the scope key it is sealed under
    written_by      TEXT NOT NULL,    -- the writer's user id
    created_at      TEXT NOT NULL,    -- RFC 3339
    updated_at      TEXT NOT NULL,    -- RFC 3339
    PRIMARY KEY (scope, id)
);

-- A timeline read: one subject+attribute in one scope, in validity order.
CREATE INDEX IF NOT EXISTS facts_by_timeline
    ON facts (scope, subject, attribute, valid_from_at);

-- A retraction finds its fact by id alone, then checks the scope.
CREATE INDEX IF NOT EXISTS facts_by_id ON facts (id);
