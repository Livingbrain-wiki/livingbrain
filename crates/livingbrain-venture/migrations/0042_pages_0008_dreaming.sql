-- Freshness and dreaming for temporal facts (docs/temporal-facts.md,
-- "Freshness" and "Dreaming").
--
-- `last_confirmed_at` is when a write last said a fact again: the same value
-- for the same timeline, which the store answers with the existing fact
-- instead of a second row. Set to the observation time on insert; NULL on the
-- rows that predate this column, which are backfilled from `observed_at` so
-- every fact has one. Freshness is measured from it, not from `valid_from`:
-- a fact can be true for years and still be unconfirmed for months.
ALTER TABLE facts ADD COLUMN last_confirmed_at TEXT;
UPDATE facts SET last_confirmed_at = observed_at WHERE last_confirmed_at IS NULL;

-- One row per (scope, subject) that dreaming has been asked to tidy: the
-- queue (`status = 'queued'`) and the record of the last pass (`dreamt_at`,
-- what it applied). Counts and fixed-vocabulary reasons only; the facts
-- themselves stay sealed in `facts`.
CREATE TABLE IF NOT EXISTS fact_dreams (
    scope       TEXT NOT NULL,    -- page scope, workspace folded in
    subject     TEXT NOT NULL,    -- <entity_type>/<slug>
    status      TEXT NOT NULL,    -- queued|running|done|failed|skipped
    queued_at   TEXT NOT NULL,    -- when it was last asked for
    dreamt_at   TEXT,             -- RFC 3339, when the last pass finished
    attempts    INTEGER NOT NULL DEFAULT 0,
    facts_seen  INTEGER NOT NULL DEFAULT 0, -- current facts the last pass read
    decisions   INTEGER NOT NULL DEFAULT 0, -- decisions the model proposed
    applied     INTEGER NOT NULL DEFAULT 0, -- decisions the store applied
    last_error  TEXT,             -- fixed vocabulary, never model text
    lease_until TEXT,             -- a running claim's expiry
    updated_at  TEXT NOT NULL,
    PRIMARY KEY (scope, subject)
);

-- The queue is read by status, oldest request first.
CREATE INDEX IF NOT EXISTS fact_dreams_by_status ON fact_dreams (status, queued_at);

-- Every decision a pass proposed, kept whether or not it was applied: the
-- history of why a fact was ended, and training data for a later model that
-- learns ADD / UPDATE / DELETE / NOOP over facts (Memory-R1 style).
-- `input_ids` is the JSON array of the fact ids the model was shown.
CREATE TABLE IF NOT EXISTS fact_dream_decisions (
    scope          TEXT NOT NULL,
    dream_at       TEXT NOT NULL,   -- the pass this decision belongs to
    seq            INTEGER NOT NULL,
    subject        TEXT NOT NULL,
    op             TEXT NOT NULL,   -- supersede|retract|end|rename
    fact_id        TEXT NOT NULL,   -- the fact the decision is about
    by_fact_id     TEXT,            -- supersede: the fact that replaces it
    new_valid_to   TEXT,            -- end, supersede: the partial date it ends at
    new_attribute  TEXT,            -- rename: the canonical key
    reason         TEXT,            -- short, fixed vocabulary from the model
    applied        INTEGER NOT NULL DEFAULT 0,
    input_ids      TEXT NOT NULL,   -- JSON array of the fact ids shown
    model          TEXT NOT NULL,   -- the call's role, see docs/temporal-facts.md
    decided_at     TEXT NOT NULL,
    PRIMARY KEY (scope, dream_at, seq)
);

-- The read path asks what a pass did to one fact.
CREATE INDEX IF NOT EXISTS fact_dream_decisions_by_fact
    ON fact_dream_decisions (scope, fact_id);
