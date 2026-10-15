-- Triage and compaction before a compile (docs/coding-agents.md, "Triage
-- and compaction"). Two new statuses need no schema change (`status` is
-- TEXT): `skipped-trivial` and `summary-only`. The new columns record what
-- triage decided, the plan a resumed compile must cut the transcript by,
-- the "Compile fully" override, and the token estimate before and after.
-- Ids, flags and counts only: no source text, no model output.
ALTER TABLE coding_agent_jobs ADD COLUMN triage TEXT;                              -- skip|summary|full, once a run decided
ALTER TABLE coding_agent_jobs ADD COLUMN triage_reason TEXT;                       -- fixed vocabulary: failing-loop, jev:full, forced, ...
ALTER TABLE coding_agent_jobs ADD COLUMN triage_plan TEXT;                         -- JSON: compaction on/off, kept/cut call positions, topic slugs
ALTER TABLE coding_agent_jobs ADD COLUMN force_full BIGINT NOT NULL DEFAULT 0;    -- 1 = "Compile fully": no triage, no compaction
ALTER TABLE coding_agent_jobs ADD COLUMN est_tokens_raw BIGINT NOT NULL DEFAULT 0;  -- estimated input tokens without triage and compaction
ALTER TABLE coding_agent_jobs ADD COLUMN est_tokens_sent BIGINT NOT NULL DEFAULT 0; -- estimated input tokens with them
