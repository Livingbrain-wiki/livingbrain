-- A workspace's subject aliases (docs/temporal-facts.md): one entity written
-- under several names (`project/world369`, `project/earthos-world`) maps to
-- one canonical subject, and a fact the compiler writes under an alias is
-- stored under the canonical subject. One row per alias per workspace.
--
-- `state` is `set` (in force: the owner set or accepted it, `set_by` names
-- them), `proposed` (dreaming suggests it; not applied, `set_by` is blank),
-- or `dismissed` (an owner removed it; kept so it is not proposed again).
-- `canonical` of a `set` row is never itself a `set` alias: one hop only.
-- Names of entities and the time of the change; no fact text.
CREATE TABLE IF NOT EXISTS fact_subject_aliases (
    workspace_id TEXT NOT NULL,
    alias        TEXT NOT NULL,
    canonical    TEXT NOT NULL,
    state        TEXT NOT NULL DEFAULT 'set',
    shared       BIGINT NOT NULL DEFAULT 0,
    set_by       TEXT NOT NULL DEFAULT '',
    updated_at   TEXT NOT NULL,
    PRIMARY KEY (workspace_id, alias)
);
