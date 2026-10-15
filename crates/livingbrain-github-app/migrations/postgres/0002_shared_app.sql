-- The Postgres form of migrations/sqlite/0002_shared_app.sql (BIGINT ids).
-- The shared "Living Brain" GitHub App (docs/github.md, "The shared app").
--
-- One operator-owned app now serves every workspace beside the per-workspace
-- apps of migration 0001. Its credentials are Worker secrets, not rows, so
-- nothing here holds a key. An installation belongs to exactly one app:
-- app_kind says which kind ('workspace' = the bound workspace's own app,
-- the only kind 0001 knew, so every existing row keeps that meaning;
-- 'shared' = the operator's app named by GITHUB_APP_ID).
--
-- account_id and installer_id are GitHub numeric ids taken only from
-- GitHub's own answers or a verified webhook: the account the app is
-- installed on, and the GitHub user who installed it (the `sender` of the
-- signed `installation` `created` delivery). A shared installation binds
-- only for the GitHub user who installed it (or, on a personal account,
-- that account itself), proven with a one-time OAuth user token.
--
-- github_states.subject carries what a state is about beyond its workspace
-- and member: the installation id a 'verify' state may bind.

ALTER TABLE github_installations ADD COLUMN app_kind TEXT NOT NULL DEFAULT 'workspace';
ALTER TABLE github_installations ADD COLUMN account_id BIGINT;
ALTER TABLE github_installations ADD COLUMN installer_id BIGINT;
ALTER TABLE github_states ADD COLUMN subject TEXT;
