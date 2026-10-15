-- The local runner (docs/local-runner.md): a compile whose owner runs their
-- jobs on their own device hands each model call to that device and waits.
-- `chunks_done` is how many transcript chunks the compile has already
-- applied, so a resumed compile carries on where it stopped instead of
-- asking again for chunks whose edits are already written. The new status
-- `waiting-runner` needs no schema change: `status` is TEXT.
ALTER TABLE coding_agent_jobs ADD COLUMN chunks_done INTEGER NOT NULL DEFAULT 0;
