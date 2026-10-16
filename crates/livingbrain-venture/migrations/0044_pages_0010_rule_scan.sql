-- The dreaming rule pass's position (docs/temporal-facts.md, "Dreaming").
--
-- Every dreaming tick reads a bounded batch of current facts, oldest first,
-- and retracts the ones the shared attribute rules refuse (events, per-item
-- keys). Keepers stay current, so a batch always starts after the last fact
-- it read: `cursor` is that position (`created_at|id`), and it is cleared
-- when the scan reaches the end, so the next tick starts over. One row, named
-- `facts`. Holds a position only: no fact text, no person.
CREATE TABLE IF NOT EXISTS fact_rule_scan (
    name       TEXT NOT NULL PRIMARY KEY,
    cursor     TEXT,
    updated_at TEXT NOT NULL
);
