-- The coding-agents compiler's triage and compaction (Settings → Usage &
-- limits): two switches and a strictness, NULL inheriting the deployment's
-- `CODING_AGENTS_TRIAGE`, `CODING_AGENTS_COMPACTION` and
-- `CODING_AGENTS_TRIAGE_STRICTNESS` like every other column of this row.
ALTER TABLE usage_budgets ADD COLUMN compiler_triage BIGINT;             -- 1 on, 0 off, NULL inherit (on)
ALTER TABLE usage_budgets ADD COLUMN compiler_compaction BIGINT;         -- 1 on, 0 off, NULL inherit (on)
ALTER TABLE usage_budgets ADD COLUMN compiler_triage_strictness BIGINT;  -- 0 lenient, 1 balanced, 2 strict, NULL inherit
