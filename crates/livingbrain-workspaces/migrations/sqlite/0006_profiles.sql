-- Names and avatars people choose for themselves and their workspace.
--
-- `workspace_members.display_name` is the name a member chose here. It is a
-- column of its own rather than a write to `name`, because `name` is the
-- mirror of what the sign-in method reports (Slack's `user_change` rewrites
-- it): a chosen name must survive the next mirror refresh, and clearing it
-- (NULL) falls back to the mirrored one. A member id is minted per workspace
-- (ADR 0002), so this is a per-membership profile — there is no person row
-- above the workspaces to hang one on.
--
-- `avatar` on both tables is an opaque avatar id (`av_<ulid>`), never a URL
-- and never image bytes: the image lives in the blob store under
-- `avatars/<workspace id>/<avatar id>`, and `GET /v1/avatars/{id}` serves it
-- only to members of the workspace whose row names it. NULL is no avatar,
-- and the app draws initials.
--
-- `workspaces.name` already exists and nothing else writes it after the
-- first sign-in, so renaming a workspace needs no new column.
--
-- The portable subset (harness ADR 0004): TEXT columns, `ALTER TABLE ADD
-- COLUMN`, no dialect type. The Postgres file is this one byte for byte.
ALTER TABLE workspaces ADD COLUMN avatar TEXT;
ALTER TABLE workspace_members ADD COLUMN display_name TEXT;
ALTER TABLE workspace_members ADD COLUMN avatar TEXT;
