-- A subject alias's state, who set it, and how much evidence a proposal had
-- (docs/temporal-facts.md). Forward-only: migration 0008 is applied in
-- production and stays as it shipped, so its table gains these columns here.
--
-- `state`: `set` (in force), `proposed` (dreaming suggests it), or `dismissed`
-- (an owner removed it). Rows that exist before this migration were in force
-- when they were written, so they become `set`; their setter was not recorded
-- and stays blank. `shared`: attribute families the two subjects shared, a
-- proposal's evidence. `set_by`: the user who set or accepted the alias.
-- Names of entities and the time of the change; no fact text.
ALTER TABLE fact_subject_aliases ADD COLUMN state TEXT NOT NULL DEFAULT 'set';
ALTER TABLE fact_subject_aliases ADD COLUMN shared BIGINT NOT NULL DEFAULT 0;
ALTER TABLE fact_subject_aliases ADD COLUMN set_by TEXT NOT NULL DEFAULT '';
