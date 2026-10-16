# 0004 — Accounts, agent accounts and per-account access

Status: Accepted. Implementation is phased (see "Phases"); each phase is
its own PR.

## Context

The request: "Make it also multi user and make agent accounts possible. And
make it possible that we can say what account has access to what data."

What exists today (read from the code, not assumed):

- **Tenancy** (ADR 0001, 0002). One D1, every workspace in it. A request's
  workspace comes only from a verified session cookie (`__Host-lb_session`,
  a `Signer`-sealed `{workspace_id, user_id, exp}`) or a personal access
  token row; no route takes a workspace id from input.
- **Members.** `workspace_members(workspace_id, user_id, name, timezone,
  is_admin)`. Ownership is `workspaces.owner_id`, written once by the first
  sign-in and never again. `is_admin` is mirrored from Slack. More people
  join only by signing in with Slack into a linked team; the email magic
  link can create a workspace or sign in to one where the address is
  *already* a member identity. There is no invite, no role change, no
  removal.
- **The access model** (`livingbrain-access`). Three scopes: `shared`,
  `channel:<id>`, `user:<id>`. `scopes_for(asker, Location, memberships)`
  is the one function that decides which a reader holds; the web, CLI and
  MCP surfaces ask as a DM (own `user:` scope + `shared`).
- **Page scopes** (`livingbrain-pages::page_scope`). A page is
  `(scope, slug)` with no tenant column; the page-store scope is
  `{prefix}-{hex(sha256(workspace ␟ prefix ␟ id))[..16]}`, so the workspace
  is folded in. Every body is sealed under a per-scope data key (ADR 0003)
  and search runs over a per-scope blind index.
- **Folders** are spelled in slugs: a slug is `[a-z0-9-]{1,128}`, and the
  coding-agents compiler (#165) separates path segments with `--`
  (`coding-agents--claude--chi-infra--decisions`).
- **Credentials.** Session cookie (web app); personal access tokens
  (`lbp_…`, SHA-256 at rest, optional `scopes` subset that is *intersected*
  with the access model, minted only from a session, refused once the
  member row is gone); the device grant mints PATs. MCP is bearer-only.
  Cookie writes pass `same_origin` (`sec-fetch-site`, then `Origin`).
- **Admin checks** are `is_admin` (owner or Slack admin) on models, usage,
  and Slack linking; a narrowed PAT is never admin.
- **Sources** (#154): personal sources list for their owner only, shared
  for the workspace. **Facts** (#167, in flight) carry a page scope and are
  sealed under it. **Coding-agents pages** (#165/#166) are written by the
  compiler into the *session owner's personal scope* under `coding-agents--`.

## Where the code lives

Living Brain is a Cratefield venture. The owner's direction is that what is
generic belongs in the harness:

| Piece | Home | Why |
|---|---|---|
| Grants engine: principal × resource selector × permission, allow/deny, expiry, deny-by-default, prefix selectors, credential-scope intersection | **Cratefield `cratefield-module-orgs`** (`grants`) | Nothing in it knows about pages; any venture with orgs needs it. |
| Service (agent) accounts and their API keys: create, disable, mint/rotate/revoke, last-used, expiry, hashed at rest | **Cratefield `cratefield-module-orgs`** (`service_accounts`) | Same: an org-owned non-human principal is generic. |
| Audit log: who (principal, via which credential, on behalf of whom) did what to which resource | **Cratefield `cratefield-module-orgs`** (`audit`) | Generic, append-only. |
| Members, roles, invitations | **Living Brain `livingbrain-workspaces`** | `module-orgs` already has members/roles/invites, but its routes act for the harness `Auth` port's subject. Living Brain's identity is its own signed session (ADR 0002 — Slack + magic link, Slack-first member ids, the Slack member mirror). Moving members onto `org_members` means moving sign-in onto `auth-core`; that is a separate migration (follow-up), and doing it here would rewrite every member id and cookie in flight. |
| Resource selectors for pages / folders / sources / facts / coding-agents, the one authorization function every route calls, search/ask filtering, UI | **Living Brain** | Venture-specific. |

`module-orgs` is chosen over a new crate because it is already on
crates.io: a new crate needs a manual first publish by the owner before the
release workflow can publish it over OIDC (`docs/RELEASING.md` in the
harness). The new pieces are pure functions over the `Database` port plus
portable migrations; Living Brain applies the migrations through its own
modules (collected into `crates/livingbrain-venture/migrations/` and pinned
in `.harness-lock.json`, like 0019/0020) and does **not** mount the
`module-orgs` routes.

## Decision

### Principals

| Principal | Id | Signs in to the web? | Credentials |
|---|---|---|---|
| **Member** (human) | existing member id (Slack id or `usr_…`) | yes: Slack, email magic link | session cookie; PATs (`lbp_…`) |
| **Agent account** | `agt_…` (module-orgs service account) | **never** — no session is ever sealed for one | API keys only (`lba_…`), shown once, SHA-256 at rest, optional expiry, rotatable, revocable |
| **Role group** | `role:owner` / `role:admin` / `role:member` / `role:viewer` | — | grant target only |

Roles on `workspace_members.role`: `owner`, `admin`, `member`, `viewer`.

- Backfill: `owner_id` → `owner`; everyone else `member`. Slack's
  `is_admin` is **not** copied into `role` (it stays its own mirrored
  column), so a later Slack demotion still takes effect.
- `workspaces.owner_id` stays as the *founder* (never rewritten, ADR 0001);
  authority comes from `role`. There may be several owners.
- **Last-owner protection:** demoting or removing the last `owner` is one
  guarded statement and answers `409`.
- Slack's admin flag keeps mirroring into `is_admin` and still makes a
  member an admin (`is_admin_effective = role ∈ {owner, admin} ∨ is_admin`),
  so nobody who is admin today loses it. A workspace that wants Living
  Brain roles to be the only source can unlink Slack admin with a follow-up
  setting.
- Who may do what: owners everything; admins manage members (not owners),
  agent accounts, shared-space grants, and read the audit log; members
  create agent accounts **for themselves** and share **their own** personal
  space; viewers read.

An **agent account** has: `id`, `workspace_id`, `name`, `description`,
`owner_id` (a member), `created_by`, `created_at`, `disabled_at`. Every
request it makes is attributed to the agent, with `on_behalf_of = owner_id`
in the audit log. It has **no access by default** — not even to its owner's
personal space — until a grant gives it some. (A member who creates an
agent for themselves is offered a one-click "read/write my
coding-agents/ folder" grant in the UI; it is still an explicit row.)

### Resources and selectors

A selector is `{space}[/{path}]` plus a resource kind:

| Part | Values |
|---|---|
| space | `shared` · `personal:<member id>` · `channel:<id>` |
| path | a folder prefix in `/` form (`coding-agents/claude/`); empty = the whole space |
| kind | `pages` · `sources` · `facts` · `*` |

How a path matches each kind:

- **pages** (incl. coding-agents pages, notes, entity pages): the folder
  path is mapped to the slug spelling — `coding-agents/claude/` ⇢ slug
  prefix `coding-agents--claude--` — and matched on a **segment boundary**:
  a slug matches when it equals the folder slug or starts with
  `folder--`. So `coding-agents/claude/` never matches
  `coding-agents--claudette--…`.
- **sources**: path is `project/<name>` (the source's filed project, #154)
  or `kind/<kind>`; empty = every source in the space.
- **facts**: path is a subject prefix (`person/u0nick`), segment boundary
  on `/`.

Permissions are ordered `read < write < admin`. `admin` on a selector means
"may create and delete grants inside it".

### Grants

`grant = (principal, selector, permission, effect allow|deny, expires_at?,
created_by, created_at)`, stored in module-orgs' `org_grants`, keyed by
`org_id = workspace_id`.

**Evaluation — deny-by-default, explicit deny wins:**

1. Collect the principal set: the member or agent id, plus the member's
   `role:` group (agents have no role).
2. The *implicit* grants (not rows — derived, so today's behaviour holds
   with an empty grants table):
   - a member: `admin` on `personal:<self>` (their own space);
   - owner/admin/member: `write` on `shared` and on `channel:<id>` they are
     in (as today); viewer: `read` on those.
3. Add every unexpired stored grant whose principal is in the set.
4. If any applicable **deny** covers the request, refuse. Otherwise allow
   iff some allow covers it with a permission ≥ the one asked.

The exceptions to (4): a deny can never remove a member's access to
**their own** personal space — nobody can be locked out of their own memory,
and no admin can do that to them — and a deny can never remove an **owner's**
access to the shared space, so an admin cannot lock the owners out of the
space (and the grants) they answer for.

**Why deny-wins and not most-specific-wins.** Deny-wins is monotonic and
order-free: adding an allow never revokes, adding a deny never grants, and
the answer never depends on which of two equally long prefixes was written
last. It still expresses the carve-out people ask for — allow
`coding-agents/`, deny `coding-agents/codex/`. Most-specific-wins can also
re-open a folder under a deny (`deny finance/`, `allow finance/public/`);
that is the one thing it gives, and it is exactly the property that makes a
review of "who can see this" non-local. If it is needed later it can be
added as a separate effect without changing existing rows.

**Who may create a grant.**

- On `personal:<X>`: only member X, or a principal holding `admin` on
  that selector (which only X can hand out). **Workspace admins and owners
  cannot grant themselves into someone's personal space.**
- On `shared` / `channel:`: owners and admins, or a holder of `admin` on
  the selector.
- Nobody can create a grant wider than they hold (a `write` holder cannot
  mint `admin`; you cannot share what you cannot read).

### Credentials narrow, never widen

`effective = grants(principal) ∩ credential_scopes`. A PAT or agent key
with no subset carries everything its principal holds; a subset (today's
vocabulary: `shared`, `user:<id>`, `channel:<id>`, extended with
`personal:<id>[/path]` selectors) only removes. A narrowed credential is
never admin (unchanged). Agent keys, like PATs, cannot mint credentials:
minting needs a session, and an agent never has one.

### The enforcement point

One function, in Living Brain:

```text
livingbrain_access::authorize(asker, grants, request) -> Decision
Access::for_asker(asker) -> ReadPlan        // what may be read, once per request
ReadPlan::scopes() -> [(page_scope, SlugFilter)]
ReadPlan::can_read(scope, slug) / can_write(scope, slug)
```

Built on the module-orgs engine, it replaces `page_scopes(asker)` as the
only way a route names a scope. Every route goes through it:

| Route / tool | Today | With grants |
|---|---|---|
| `GET /v1/pages`, `GET /v1/pages/{slug}` | asker's DM scopes | `ReadPlan` scopes, each with its slug filter applied **in the query** before `LIMIT` |
| `PUT /v1/pages/{slug}` | edit in place / new page in own scope | edit requires `can_write(scope, slug)`; a new page lands in own scope, or a granted `write` folder the caller names by slug; otherwise 404 (not 403, so existence is not confirmed) |
| `GET /v1/search`, `brain_search` | blind index over DM scopes | blind index over plan scopes; **slug filter before ranking and limit**; facets/counts computed from the filtered set |
| `POST /v1/ask`, `brain_context_for` | DM scopes | context retrieved only through the plan; nothing ungranted reaches a model, a citation or a count |
| backlinks / citations on a page | all backlinks in the page's scope | backlinks filtered through `can_read`; citations are slugs already in the readable body, resolved through the plan (an unreadable target renders as plain text, never a title) |
| `POST /v1/notes`, `brain_note` | own scope | own scope; an agent writes only where it holds `write` |
| `GET /v1/export` | every page in DM scopes | the plan; audited |
| `GET /v1/sources*`, `POST /v1/pages/sources` | workspace + scope | plan with `sources` kind; personal sources stay owner-only unless shared |
| `/v1/facts*`, `brain_facts` (#167) | scope ∈ asker's scopes | plan with `facts` kind (subject prefix) |
| `/v1/coding-agents/*`, compiler | session owner's scope | jobs carry the source's scope (unchanged); a job is enqueued only for a source the asker can `write` |
| `/v1/usage`, `/v1/models` admin routes | `is_admin` | `is_admin_effective`, member principals only (an agent is never admin) |
| `/v1/workspaces/members`, invites, roles | — | admin/owner; audited |
| `/v1/agents/*` (new) | — | admin, or a member for agents they own; audited |
| `/v1/access/grants` (new) | — | per "who may create a grant"; audited |
| `/v1/audit` (new) | — | owner/admin |

Search/ask leakage rules: grants are applied before ranking and before
`LIMIT`; a refused page is indistinguishable from a page that does not
exist (same 404, same body); counts and facets are over the filtered set;
error messages never echo a slug the caller could not read. Timing: reads
of personal scopes the caller holds no grant on are never issued at all,
so there is no per-page timing difference to measure.

### Audit log

module-orgs `org_audit(id, org_id, actor_kind, actor_id, on_behalf_of,
credential, action, resource, detail, at)`. `credential` is the token or
key **prefix** (never the secret) or `session`. Written for: invites,
accepts, role changes, removals, agent create/disable/delete, key
mint/rotate/revoke, grant create/delete, writes by agents, exports. Readable
by owners and admins at `GET /v1/audit`, newest first, paginated.
Personal-data disposition: `retain` with the member id pseudonymous
(declared in `personal_data`), because an audit trail that an erasure can
rewrite is no trail.

### Members and invitations (phase a)

- `POST /v1/workspaces/invites {email, role}` (admin; `owner` role only by
  an owner): stores `workspace_invites` and mails the existing branded
  sign-in mail with a link whose `sign_in_links` row carries the workspace
  and a 7-day TTL. Accepting is the existing `POST /email/verify`: a live
  invite for `(workspace, email)` creates the member (`usr_…`), the email
  identity and the role, and stamps the invite accepted. An expired link
  can be replaced by a normal magic link for that workspace while the
  invite is live.
- `GET /v1/workspaces/members` gains `role`; `PATCH
  /v1/workspaces/members/{id} {role}`; `DELETE /v1/workspaces/members/{id}`.
- **Removal** deletes the member row and identities. That already kills
  their sessions (a session whose member row is gone is a 401) and their
  PATs (`member_for` refuses). Their agent accounts are **disabled and their
  keys revoked** (fail closed), not deleted, so an admin can re-own one
  (`PATCH /v1/agents/{id} {owner_id}`), which mints nothing — the new owner
  rotates a key. Their personal pages stay encrypted under their scope;
  nobody gains access by the removal. Grants naming them are deleted.
- Every cookie write takes `same_origin`.
- `GET /v1/me` (bearer **or** cookie): `{workspace, principal: {kind,
  id, name, role}, credential: {kind, prefix, scopes}}` — the "connected as"
  the Mac app and Colonizer show.
- Multi-workspace: a person's email identity can be a member of several
  workspaces. `GET /v1/workspaces` lists the workspaces the signed-in
  person's verified email identity is a member of; `POST
  /v1/workspaces/switch {workspace_id}` seals a session for the member that
  identity maps to there (refused unless that identity row exists — no
  workspace is reachable by naming it).

## Deleting a workspace

An **owner** may delete the workspace they are in: `DELETE
/v1/workspaces/current` with the session cookie, body `{"confirm": "<the
workspace's exact name>"}`. Admins and members get `403`; a name that does not
match exactly gets `400`; a person with no other workspace to sign in to gets
`409 This is your only workspace.` (their other workspaces are the ones
`store::memberships_for` lists for their email identities). The answer is
`{"deleted": id, "next": {id, name}}` and a session cookie for `next`: the
oldest other workspace the person belongs to.

**Which content is whose.** A page scope folds the workspace in (`page_scope`,
above), so every scope a workspace owns is distinct from every other
workspace's, even when the member id is the same:

| Space | Scope | Erased |
|---|---|---|
| `shared` | `shared-…` for this workspace | yes |
| `personal:<member id>` | one per member id the workspace ever had, removed members included | yes |
| `channel:slack:<channel>` | one per private channel the agent's triage ledger names for this workspace | yes, from the ledger |

A private channel the triage ledger never recorded has no entry to find it
by, so its scope is **not** shredded. That is a known gap: the fix is a
channel registry in the workspaces module, not a guess here.

**Order and atomicity.** Everything that can be refused is checked first; the
next session is sealed; then the erasure runs, then the workspace's own rows.
The erasure is idempotent and **fails closed**: no page store in the request,
or any shred failing, answers `500` and the workspace is untouched, so the
owner can run the deletion again. Only after it succeeds are this module's
rows removed, in one batch. Per scope, each shred is atomic; across scopes the
store offers no transaction, which is why the erasure goes first and is
retry-safe.

**Hard or soft.** The workspace row is **tombstoned** (`workspaces.deleted_at`,
migration `0007_delete_workspace`), its name and avatar cleared, so the id is
never handed out again and every read of a workspace (`store::workspace`,
`memberships_for`) refuses it. Everything else is deleted: members and their
sign-in identities (they hold names and timezones), invitations and every
sign-in link naming the workspace (they hold addresses), connections, the
Slack bot install copy, turn policies and the triage ledger.

| Table | How | Owner |
|---|---|---|
| `workspaces` | tombstone (`deleted_at`), name and avatar cleared | workspaces |
| `workspace_members`, `member_identities`, `workspace_connections` | delete | workspaces |
| `workspace_invites`, `sign_in_links` (for the workspace and its invitations) | delete | workspaces |
| `slack_installs` (team bound to the workspace) | delete | workspaces |
| `turn_policies`, `turn_triage` | delete | workspaces |
| `personal_access_tokens` | revoke | tokens (via the composition) |
| `org_service_accounts`, `org_service_account_keys` | disable, revoke keys | access (via the composition) |
| `org_grants` | delete | access (via the composition) |
| `model_connections` (provider keys) | delete | models (via the composition) |
| `tool_connections`, `tool_workspace_settings`, `tool_pending_approvals` | delete | tools (via the composition) |
| `github_app` (the workspace's app private key), `github_installations`, `github_repos`, `github_states` | delete | github-app (via the composition) |
| `org_audit`, `settings_audit`, `usage_daily`, `model_jobs`, `model_runners`, `jev_settings` | **kept** (audit trail and counts; no content) | — |
| page `pages`, `page_versions` | kept; bodies unreadable once their scope's key is gone (ADR 0003) | pages |

The erasure runs in the composition (`WipeWorkspace` in
`crates/livingbrain-venture`), because the workspaces module cannot name a page
scope or a token table (the graph runs the other way). It is injected through
`Workspaces::on_workspace_erasure`; a composition without it refuses every
deletion.

**What remains, stated plainly.** The shred is immediate in D1 and R2 and not
in their history: D1 Time Travel and retained R2 versions keep the wrapped keys
until they expire, as ADR 0003 says. The GitHub App installation stays
installed on GitHub's side, and the Slack app stays installed in Slack, until
their owners remove them; only this deployment's copies of their tokens and
keys go. A member's avatar image, uploaded through the `me` module's own blob
scope, is not reachable from this module and is left in R2.

## Threat model

| Threat | Control |
|---|---|
| Cross-workspace read | Workspace from credential only; grants keyed by `org_id = workspace_id`; selectors never contain a workspace; page scopes fold the workspace in. |
| Admin reads a member's private memory | Implicit grants give admins nothing in personal spaces; only the space's owner can share it. |
| Prefix confusion (`claude` vs `claudette`, `a--b` smuggling) | Segment-boundary match; segments cannot contain `--` (compiler rule), selector paths are validated to the slug alphabet. |
| Leakage via search ranking, counts, facets, backlinks, citations, export, errors | Filter in the query before rank/limit; same 404 for refused and absent; no slug echoed. |
| Leaked agent key | Hash at rest, prefix shown, revocation immediate, optional expiry, `disabled_at` → 401, last-used visible. |
| Agent uses the web UI | No session is ever sealed for an `agt_`; cookie routes resolve members only; key routes resolve via `Authorization` only. |
| Narrowed credential widens via grants | Intersection, computed after grant expansion. |
| Removed member keeps access | Member row gone → session and PATs 401; their agents disabled; grants deleted. |
| CSRF on new cookie writes | Every new mutating route uses the shared `same_origin`. |
| Privilege escalation via grants | Cannot grant above what you hold; personal-space grants only by their owner; last-owner guard. |
| Audit tampering | Append-only API, no update/delete route. |
| Owner deletes the wrong workspace | Typed exact name; owners only; refused as the person's last workspace. |
| A deleted workspace comes back | Tombstoned id, never reissued; invitations and sign-in links deleted; reads refuse `deleted_at`; Slack connection removed so a team re-install makes a new workspace. |

Known, accepted: session cookies are self-contained (ADR 0001) — a removed
member's cookie stops working because the member row is checked on every
request, not because the cookie is revoked. Removal therefore **tombstones**
the row (`removed_at`) instead of deleting it: a removed Slack member who signs
in with Slack again lands on the tombstone and is refused, and the Slack member
mirror cannot revive it. Re-inviting a removed person mints a fresh member id,
so none of their old cookies or tokens come back.

## Migration path

Backward compatible by construction: with an empty grants table the
implicit grants reproduce today's access exactly (own personal space;
shared and channel spaces for members). Existing PATs carry `scopes = ''`
(no subset) or a subset of `shared`/`user:`/`channel:` names, both of which
mean what they meant. Migrations, in order:

1. `workspaces/0005_roles` — `workspace_members.role`, backfill,
   `workspace_invites`.
2. `orgs/0002_service_accounts`, `orgs/0003_grants`, `orgs/0004_audit`
   (from `cratefield-module-orgs`), collected into the venture.

All portable SQL (harness ADR 0004), one file per dialect, re-runnable.

## Implementation notes (phase a)

- `viewer` is accepted by the model but refused by the routes (`422`) until
  phase c makes it read-only everywhere: a role that says "read-only" and is
  not would be worse than none.
- The audit log arrives with the module-orgs release (phases b/c); phase a
  writes none.
- Removal runs the composition's `MemberRemoval` seam after the tombstone; the
  venture revokes the member's PATs there.

## Implementation notes (phases b and c)

- Engine: `cratefield-module-orgs` (Cratefield/harness#879) — `grants`,
  `service_accounts`, `audit`, pinned at `=0.1.2` (the Cratefield 0.9 round,
  `cratefield-core` 0.9; Living Brain moved to it in #204).
- `livingbrain-pages::access::AccessPlan` is the one authorization function.
  Every wiki route (`pages`, `search`, `ask`, `notes`, `export`, `sources`
  incl. the `github_*` and `agent_log` kinds and `/projects`, `facts`,
  `graph`, `coding-agents` `/tree`) and every MCP tool (and the MCP source
  import) builds one per request and reads only through it. The map
  (`GET /v1/graph`) reads pages and `[[link]]` rows through the page filters
  (both ends of a link), sources through the sources reach (whole scopes, or
  granted projects) and fact subjects through the facts filters, all before
  its cap (`livingbrain_pages::graph_rows_in`). Listing and search take
  `ScopeFilter`s **into the SQL** (`PageStore::list_in`, `search_in`), so a
  page outside the grant is never counted, ranked, limited, quoted or sent to
  a model; a read by slug skips scopes whose filter refuses the slug without
  reading; backlinks and citations are filtered through the plan; facts are
  filtered on the plaintext index before anything is chosen or decrypted
  (`relevant_where`).
- Members keep exactly their pre-ADR reach: their own space and the shared
  one (proved by the plan's unit test against `page_scopes_for` and by every
  pre-existing route test passing unchanged). A member's own notes still land
  in their own space whatever their token's subset, as before.
- Agent accounts authenticate with `lba_…` keys through
  `livingbrain_tokens::authenticate_any`, accepted **only** by the wiki/MCP
  bearer seam and `GET /v1/me`; every other route keeps `authenticate`,
  which refuses an agent key. A key is refused when revoked, expired, its
  account disabled or deleted, or its owner no longer a member. An agent's
  new pages and notes land in its owner's personal space (else the shared
  one) only where it holds `write`; its writes and every export are audited.
- Member-only (an agent key gets `403 api/agent-not-allowed`, documented
  rather than granted, because nothing in them is a resource a grant names):
  `/v1/coding-agents/queue`, `/process`, `/retry`, `/compile-fully` (a
  member's own compile queue; the workspace's model, budget and spend) and
  `/v1/model-jobs*` (a member's own runner). A member's `process` /
  `compile-fully` / `retry` compile only their own space, and only where the
  credential may **write** pages there: a `read:` token lists the queue but
  starts nothing. Session/PAT-only via `authenticate` (an agent key is a
  401): `/v1/me` writes and avatars, `/v1/avatars/{id}`, `/v1/settings/budgets`,
  `/v1/github/*` settings, `/v1/models*` incl. `/v1/models/jev*` (members
  read, owners/admins write), `/v1/usage`, `/v1/workspaces/*`, `/v1/tokens`.
- New modules (crate `livingbrain-accounts`): `access` (`/v1/access/grants`,
  `/v1/access/pages/{slug}`; it applies the three harness migrations as
  `access/0001–0003`, collected as venture migrations 0038–0040), `agents` (`/v1/agents…`), `audit` (`/v1/audit`). All
  session-only and same-origin for writes.
- Member removal (the venture's `MemberRemoval`): PATs revoked, owned agents
  disabled with keys revoked, grants naming the member and grants on their
  personal space deleted.
- A stored deny never removes an **owner's** access to the shared space
  either, so admins cannot lock owners out of it.
- Not yet wired: the Slack agent's answers (`livingbrain-pages::answers`)
  still use the channel/DM rule, not grants — grants reach the API, the web
  app and MCP. The Mac app shows nothing new yet; `GET /v1/me` is ready for
  its "connected as".

## Implementation notes (multiselect, batch routes)

- `POST /v1/access/grants/batch {grants:[…]}` creates up to 100 grants and
  `POST /v1/access/grants/batch/revoke {ids:[…]}` removes up to 100. Each
  entry is held to the single route's rule (who may create or delete a grant)
  **before anything is written**; one refusal refuses the batch (a field error
  names the entry, `grants[3]: …`; an id the caller may not remove answers the
  same 404 a single delete does). The rows and one audit line per grant are
  then one `batch_atomic`, so the grants and their audit trail commit together
  or not at all.
- `POST /v1/agents/batch {ids, disabled}` disables or enables up to 100 agent
  accounts under `PATCH /v1/agents/{id}`'s rule, atomically and audited per
  account; keys are untouched (a disabled account's keys stop verifying).
- The web app's grant form is a multiselect of who × spaces × folders × kinds
  with one permission and an optional expiry, previewed ("This creates 6
  grants") and confirmed above 10; its folder choices come from reads already
  filtered by the asker's access (`/v1/coding-agents/tree`, `/v1/pages`,
  `/v1/sources/projects`), no new endpoint. The grants and agent lists take
  row selection and bulk actions; a page's Share takes several people and
  agents at once.

## Phases

- **a. Members & invites** — Living Brain only.
- **b. Agent accounts** — needs the module-orgs release.
- **c. Grants + the authorization function wired everywhere** — needs the
  module-orgs release.
- **d. UI** — Settings → Members, Agent accounts, Access, Audit; a "who
  can see this" panel with Share on every wiki page; the Mac app's
  "connected as" from `GET /v1/me`.

## Notes for concurrent work

- **Coding-agents compiler (#165/#166).** Keep writing into the session
  owner's personal scope under `coding-agents--<agent>--…`. Folder grants
  (`personal:<owner>/coding-agents/claude/`) are how a teammate or an agent
  account is shown one agent's pages. Do not add a scope argument the
  client controls; take the job's scope as today. When a job is enqueued by
  an agent account, the enqueue route checks `write` on the target folder.
- **Temporal facts (#167).** Facts already carry a page scope and check
  `scope ∈ asker's scopes`. When phase c lands that check becomes
  `plan.can_read(kind=facts, scope, subject)`; keep subjects `/`-separated
  so a subject prefix is a folder.
- **Colonizer push with PATs.** PATs keep working unchanged. A pushing
  service should move to an **agent account key** (`lba_…`) so its writes
  are attributed to it and it can be scoped to the folders it pushes to.
- **Any new route**: get the asker from the existing `Service::asker`, then
  ask the plan. A route that reads `page_scopes` directly after phase c is a
  bug.
