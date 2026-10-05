# 0002 — Workspaces without Slack

Status: Accepted

## Context

Issue #71 asks for workspaces and sign-in that do not depend on Slack. The
2026-10-03 04:19 WITA surface decision holds that coding harnesses, the CLI and
the app come first, and Slack and Discord are equal, optional team chats. Three
things follow:

- **A CLI-only or Discord-only team must be able to create a workspace.** If the
  only way in is Slack, every team without it is locked out, and sign-in must
  not depend on Slack credentials being present.
- **ADR 0001 tied the workspace id to the Slack team id.** That made Slack
  load-bearing for identity, not just for OAuth, so it blocked every other entry
  path.
- **The Cratefield modules named in the issue aren't ready.** `auth-magic-link`
  and `auth-passkeys` are not on crates.io, and `cratefield-module-orgs` 0.1.0
  needs Cratefield's `Auth` port rather than this module's signed session
  cookie, so the magic link is built here over the `Mailer` port and passkeys
  plus org invitations move to follow-up issues.

## Decision

**Workspace ids are our own.** New workspaces get `ws_` plus an IdGen id.
Existing Slack-created rows keep their id, now treated as opaque — rewriting it
would invalidate live session cookies and the rows other modules key on. Nothing
parses a workspace id as a Slack team id, and every Slack lookup goes through
connections. This supersedes ADR 0001's "workspace id = Slack team id".

**Slack and Discord are linked connections.**
`workspace_connections(workspace_id, platform, external_id)`, with
`UNIQUE (platform, external_id)` so one Slack team or Discord guild links to at
most one workspace, and a primary key of `(workspace_id, platform)` so a
workspace has at most one connection per platform. Migration `workspaces/0002`
backfills a Slack connection for every existing row, so Slack sign-in behaves
as before.

**People link through identities.**
`member_identities(workspace_id, platform, external_id, user_id)` for slack,
discord and email, unique per workspace. A member first seen through Slack keeps
the Slack user id as its member id, so existing rows and the issue #6
`apply_user_change` seam are unchanged; members first seen by email get `usr_`
plus an id. A person's Slack or Discord id links to the same member.

**Sign-in is Slack or an email magic link.** Slack is the unchanged flow from
ADR 0001. The email route, `POST /v1/workspaces/email/start`, always answers
`202` with the same body so addresses can't be enumerated; the token is 32
random bytes and only its SHA-256 is stored, in `sign_in_links`, single use with
a 15-minute TTL. `GET /email/verify` only renders a form, because mail scanners
prefetch links; `POST /email/verify` spends the token and seals the same
session cookie as Slack. A link either creates a workspace, with the email
owner as owner, or signs in to a workspace where that email is a member
identity. The email routes work without Slack credentials and answer `503`
without a mailer. The session cookie now carries `workspace_id`, and a serde
alias still accepts the old `team_id`.

**Linking reuses the Slack OAuth flow.** A signed-in caller starts
`GET /v1/workspaces/connections/slack/start`, using the same Slack OAuth flow
and redirect URI; the flow cookie carries the caller's workspace and user. Only
the owner may link a team that isn't linked yet, and a clash answers `409`. The
caller's Slack user id links to their member. Discord's platform side is a stub:
public `link_connection` and `link_identity` seams with no route — issue #56
adds Discord OAuth. Their contract is that the caller must have verified the
platform id and never passes a client-supplied one.

**Linking does not prove the caller runs the team.** Accepted for now, and
named here so it is not mistaken for something the flow decided: the owner
rule above is the caller's authority over *their own* workspace, and nothing
establishes their authority over the Slack team being linked. Slack's OpenID
Connect cannot answer that — its discovery document supports only `openid`,
`profile` and `email`, and its `id_token` carries no admin flag — so any
member of an unlinked public team who owns a workspace here can claim that
team permanently, and every later member of it lands in their workspace.
Closing it needs a second Slack app that can ask Slack who a team's admins
are (the legacy `admin` scope, or an app installed by an admin), not a
change to this flow. `link_slack_workspace` says so at the call site.

**The owner rule is unchanged.** Set once by insert-if-absent, never rewritten;
applies to email-created workspaces too. When `auth-magic-link` ships, this
module should move to it, just as ADR 0001 planned to move Slack to `auth-oidc`.

## Consequences

- Isolation still rests on the module — the session's workspace id plus a member
  row — and tests guard it across both sign-in methods
  (`crates/livingbrain-workspaces/tests/tenancy.rs`, `tests/email.rs`).
- Magic links and their `202` response give no address enumeration, but the
  start route can send mail to any address, so rate limiting is a follow-up.
- The magic-link POST is open to login CSRF: an attacker could make a victim's
  browser sign in to the attacker's workspace with the attacker's token. It is
  an accepted, known risk.
- A Slack-first member id is a Slack user id while an email-first one is
  `usr_…`; ids are opaque either way.
- The collected migration `0010_workspaces_0002_identities.sql` sits beside the
  venture and is pinned in `.harness-lock.json`.
