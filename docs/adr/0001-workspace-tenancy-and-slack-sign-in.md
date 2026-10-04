# 0001 — Workspace tenancy and sign in with Slack

Status: Accepted

## Context

Issue #5 asks for one tenant per Slack workspace, sign in with Slack, and a
member mirror. Three facts shaped the answer:

- **`auth-oidc` cannot do Slack yet.** Its `PROVIDERS` is `[GOOGLE, APPLE]`,
  its callback drops every claim but subject, email and name, and
  `identities.provider` has a `CHECK` that rejects `'slack'`.
- **Tenant-per-database is unavailable on Workers.** `TenantDatabases` needs
  `ports.tenants`, which the runtime cannot set: a Worker binds the D1
  databases it was deployed with (harness ADR 0008).
- **Slack's OIDC is ordinary OIDC.** `.../authorize` and `.../token`, an
  `id_token` whose `nonce` ties it to the attempt, the team and user in
  `https://slack.com/team_id` and `https://slack.com/user_id`.

## Decision

**One D1, every workspace in it, isolation enforced by the module.** The module
owns `workspaces(id TEXT PRIMARY KEY)` (the Slack team id) and
`workspace_members(workspace_id, user_id, …)`. Every query is scoped by a
workspace id from the verified session cookie or a Slack `id_token`; no route
takes one from request input, and `/me` and `/members` answer `401` unless the
session's `(team_id, user_id)` has a member row. A harder boundary becomes a
migration plus a runtime, not a rewrite, because the module reaches the
database only through ports.

**A venture-owned Slack OIDC code flow over the ports.** `GET /slack/start`
mints a random `state` and `nonce`, seals them with the `Signer` into an
HttpOnly, Secure, SameSite=Lax `__Host-lb_flow` cookie (ten minutes), and 302s
to Slack; `GET /slack/callback` checks the cookie and a constant-time `state`
match, exchanges the code through `HttpClient`, and seals a seven-day
`__Host-lb_session` cookie. Migrate to `auth-oidc` once it grows a Slack
provider and a `'slack'` that passes the constraint.

**The `id_token` signature is deliberately not verified.** Slack returns it in
the body of a TLS response to its token endpoint, which we authenticated with
our client secret — OIDC Core §3.1.3.7 item 6 lets a client treat that response
as integrity-protected. We check `iss`, `aud`, `exp` and `nonce` against the
flow cookie, rejecting a token meant for another client, attempt or moment.

**The session is a signed cookie, not a row.** It carries the workspace, user
and expiry, signed by the `Signer`: no server-side table to keep, expire or
replicate. Revocation means rotating `HARNESS_SECRET` — acceptable at seven
days, and why the payload holds ids and nothing else.

**The owner is set exactly once, by the first person to sign in.** `INSERT INTO
workspaces … ON CONFLICT (id) DO NOTHING` with that person as `owner_id`; no
code path writes it again, and later signers join as members with `is_owner`
false, because ownership is `owner_id == user_id` read from the row. There is
no transfer.

**The member mirror has a seam, not a route.** `apply_user_change(db, clock,
team_id, event)` is public: issue #6's webhook, after verifying the signature,
calls it with the `team_id` from the signed envelope, never from
`event["user"]` — it no-ops an event whose `user.team_id` disagrees. It
refreshes name, timezone and `is_admin` for a member of a workspace we have,
ignores teams we do not, and never touches `owner_id`.

## Consequences

- A scoping bug in a query is a cross-tenant read; the isolation tests
  (`crates/livingbrain-workspaces/tests/sign_in.rs`) are the guard.
- Sign-in needs a Slack app and a public HTTPS origin (a tunnel under
  `wrangler dev`); without credentials the routes answer `503 not configured`,
  and the Worker still starts, so misconfiguration is visible rather than
  fatal.
- `fz doctor` validates the three `WORKSPACES_SLACK_*`/`WORKSPACES_REDIRECT_BASE`
  keys, not the build, and the collected migration beside the venture is what
  `wrangler d1 migrations apply --local` reads — the module's own
  `migrations/` is the source, kept in step by `.harness-lock.json`.
