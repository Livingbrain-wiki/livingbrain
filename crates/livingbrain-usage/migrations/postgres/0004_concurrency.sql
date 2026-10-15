-- How many sessions the coding-agents compiler compiles at once for the
-- workspace (Settings → Usage & limits → Advanced), NULL inheriting the
-- deployment's `CODING_AGENTS_CONCURRENCY` like every other column of this
-- row. The compiler clamps it to 1–8 whatever is stored.
ALTER TABLE usage_budgets ADD COLUMN compiler_concurrency BIGINT;   -- 1..8, NULL inherit (4)
