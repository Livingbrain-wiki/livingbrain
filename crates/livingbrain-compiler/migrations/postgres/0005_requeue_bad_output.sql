-- Requeue the compiles that failed as a bare `bad-output` (docs/coding-agents.md).
--
-- Before this release a model answer that was cut off at the output limit,
-- or not quite the edit list's shape, failed its job as `bad-output` with no
-- detail and no second try, and three such failures left it failed for good.
-- The compiler now asks for the edit list as a forced tool call where the
-- provider serves one, accepts find/replace edits so an answer stays short,
-- repairs an unusable answer once and says why when it still fails
-- (`bad-output: …`). The jobs that failed the old way are queued again with
-- their attempts reset, so the next drain compiles them under the new rules.
-- A failure recorded after this release names its reason and is not matched.
--
-- Status and counters only: nothing about a person. The same statement in
-- both dialects.
UPDATE coding_agent_jobs
   SET status = 'queued', attempts = 0, next_attempt_at = NULL, lease_until = NULL
 WHERE status = 'failed' AND last_error = 'bad-output';
