-- A workspace's subject aliases (docs/temporal-facts.md): one entity written
-- under several names (`project/world369`, `project/earthos-world`) maps to
-- one canonical subject. A fact the compiler writes for an alias is stored
-- under the canonical subject. One row per alias per workspace; `canonical`
-- is never itself an alias, so a lookup is one hop. Names of entities only.
CREATE TABLE IF NOT EXISTS fact_subject_aliases (
    workspace_id TEXT NOT NULL,
    alias        TEXT NOT NULL,
    canonical    TEXT NOT NULL,
    updated_at   TEXT NOT NULL,
    PRIMARY KEY (workspace_id, alias)
);
