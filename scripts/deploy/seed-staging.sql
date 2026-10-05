-- The workspace the staging smoke test signs in as (issue #93). Staging
-- only: production is never seeded, and the health check is all the
-- production smoke test runs.
--
-- Both statements are idempotent, so this can be applied on every deploy.
-- The column list and the values mirror what the Slack callback writes, so
-- the row looks to the module exactly like a real first sign-in.
INSERT INTO workspaces (id, name, owner_id, created_at)
VALUES ('T0SMOKETEST', 'Smoke Test', 'U0SMOKETEST', '2026-01-01T00:00:00Z')
ON CONFLICT (id) DO NOTHING;

INSERT INTO workspace_members (workspace_id, user_id, name, timezone, is_admin, updated_at)
VALUES ('T0SMOKETEST', 'U0SMOKETEST', 'Smoke Test', NULL, 0, '2026-01-01T00:00:00Z')
ON CONFLICT (workspace_id, user_id) DO NOTHING;
