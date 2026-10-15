# GitHub connection: record schema and pure core

The `livingbrain-github` crate is the pure half of the GitHub connection: it
verifies and reads webhook deliveries, models the REST and webhook objects,
maps them to deterministic records, plans a resumable backfill, and decides
rate-limit backoff. It does no I/O. The Worker's connector does the HTTP,
holds the installation token, runs the planner from its cron or queue, and
writes the ledger.

The backfill and ingest run only inside Living Brain, as a system process. No
agent, local script, one-off importer or CLI command fetches a repository's
history.

## Webhooks

Order of operations for a delivery:

1. `webhook::verify(secret, raw_body, X-Hub-Signature-256)`. Verify the exact
   bytes received. An unconfigured (empty) secret never verifies.
2. `webhook::Delivery::parse(X-GitHub-Event, X-GitHub-Delivery, body)`. Only
   the nine subscribed events pass. `X-GitHub-Delivery` is the replay key:
   store it and refuse a second delivery with the same id.
3. `webhook::parse_payload(event, body)` for the typed payload.

Subscribed events: `push`, `issues`, `issue_comment`, `pull_request`,
`pull_request_review`, `pull_request_review_comment`, `release`,
`installation`, `installation_repositories`. `ping` is refused as unsupported;
the connector must answer it itself, before this crate is involved.

## Record schema (version 1)

Every record is serialized with serde in struct declaration order, with no
maps, so the same GitHub state yields the same bytes. The body filed in the
ledger is that JSON. Its SHA-256 (`record::body_sha256`) is the identity that
(scope, sha256) dedupe uses. No field records when the crate ran.

### Commit: `kind: "commit"`, ledger kind `github_commit`

| field | type | notes |
|---|---|---|
| `schema` | int | `1` |
| `kind` | string | `"commit"` |
| `repo` | string | `owner/name` |
| `sha` | string | full commit sha |
| `origin_ref` | string | GitHub permalink |
| `message` | string | as written (free text; redacted by the ingest pipeline) |
| `author.login` | string or null | GitHub login |
| `author.name` | string or null | git display name. No address is ever stored |
| `authored_at` | string or null | git author date |
| `files[]` | array | `path`, `status`, `additions`, `deletions`, `patch` |
| `totals` | object | `files_changed`, `additions`, `deletions` (from GitHub `stats` when present) |

`patch` is `null` unless `MapOptions::include_small_diffs_max_bytes` is set
and that file's diff is at most that many bytes. The default stores no diff.

### Issue or pull request: `kind: "issue"` or `"pull_request"`

Ledger kinds `github_issue` and `github_pr`.

| field | type | notes |
|---|---|---|
| `schema`, `kind`, `repo`, `number`, `origin_ref` | | as above |
| `title`, `body` | string, string or null | |
| `state` | string | `open` or `closed` |
| `author` | string or null | login |
| `labels` | string[] | sorted |
| `created_at`, `updated_at`, `closed_at`, `merged_at` | string or null | GitHub timestamps, ISO-8601 UTC |
| `activity[]` | array | comments and reviews, sorted by time, then kind, then origin |

`activity[]` entries: `kind` (`comment` or `review`), `at`, `author`, `body`,
`state` (reviews only), `origin_ref`. Reviews with no `submitted_at` (still
pending) are dropped. Inline review comments are not folded in this version.

## Source parts

`record::commit_source` and `record::item_source` return the parts a
`SourceIngest` is built from:

- `kind`: `GithubKind` (wire names `github_commit`, `github_issue`, `github_pr`)
- `project`: the repository name, for example `EarthOS.world`
- `rel_path`: `github/<owner>/<repo>/commit/<sha>`, `.../issue/<n>` or `.../pr/<n>`
- `origin_ref`: the permalink
- `body`: canonical JSON; `body_sha256`: its digest

The caller adds the workspace, the importing member, and any author mapping
to a member (a GitHub login is not a member id).

## Backfill planner

`planner::plan(&state, budget)` returns at most `budget` requests per tick —
the tick's `planner::tick_budget` (400 unless `GITHUB_TICK_BUDGET` says
otherwise; `DEFAULT_BUDGET`, 45, is the Workers Free figure) within what the
invocation has left
(commit list page, issue list page, comments and reviews of queued items in
order, then commit details). The driver makes each call and calls
`planner::apply(state, &request, &response)`, which returns the new state and
the items now complete. The state is plain serde data and is persisted every
tick, so an interrupted tick resumes at the same cursor.

Listings: commits newest first; issues and pull requests via
`/issues?state=all&sort=updated&direction=asc&since=…`, so the watermark only
moves forward. After a full run, `BackfillState::next_run()` makes the next
incremental run: it starts at the saved `since` watermark and stops the commit
listing at the newest commit already seen.

## Rate limits

`rate::parse_rate_limit(headers)` reads `x-ratelimit-remaining`,
`x-ratelimit-reset` and `retry-after`. `rate::decide(status, rate, now)` returns
`Proceed`, `Wait { seconds }` or `Fail`. A 403 with no limit header is a
permission failure, not a wait.

## Storage & secrets

The `livingbrain-github-app` crate is the `github` module: the tables, the
sealed credentials and the store API the routes and the cron drain use. It
owns migrations `github/0001` (collected as
`crates/livingbrain-venture/migrations/0028_github_0001_init.sql`) and
`github/0002` (the shared app, collected as `0032_github_0002_shared_app.sql`). The routes
and the drain body (Part C) are described in "Routes and the drain" below.

### One app per workspace (the advanced option)

The default is the shared app ("The shared app" below). The per-workspace
app stays as **Advanced: use your own GitHub App**, for an enterprise or a
self-hosted deployment. The manifest flow is started by a workspace owner or admin and creates a
GitHub App on their own GitHub account. Its private key is that workspace's
credential, so `github_app` holds one row per workspace (`workspace_id` is
unique, `app_id` is the key). A leaked key reaches one tenant, and an
installation can be checked against the one app it may belong to. Webhooks
arrive at `POST /v1/github/webhook`, and the app is named by the delivery's
`X-GitHub-Hook-Installation-Target-ID` (see "Webhook intake" below).

### Tables

Every instant is `INTEGER` unix seconds (`BIGINT` on Postgres), because
expiry, pruning and leases are compared in SQL.

| table | holds |
|---|---|
| `github_app` | app id, slug, client id, and the private key, webhook secret and client secret, all sealed; `created_by` |
| `github_states` | single-use state tokens for the manifest, install, shared-install and verify round trips: the SHA-256 of the token, never the token; `subject` is the installation id a verify state may bind |
| `github_installations` | installation id, app id, `app_kind` (`workspace` or `shared`), the bound workspace (NULL until bound), the account login, type and id, `installer_id` (the GitHub user who installed it, from the signed `installation` `created` delivery), status `active` / `suspended` / `deleted`, `bound_by` |
| `github_repos` | per (installation, repo): workspace, full name, project, scope, `included`, `active`, backfill phase, cursor JSON, item and batch counters, `removed_at`, last webhook, last error |
| `github_deliveries` | (app id, delivery id) already accepted, kept 30 days |
| `github_queue` | work a verified delivery leaves for the drain: a reference (`kind`, `subject`), never GitHub content |

### Sealing

Each `github_app` row has its own 256-bit data key, wrapped by the KMS port
(`HARNESS_KEK_*`, the same key ring as the Slack bot token and the page
store). Each credential is sealed with XChaCha20-Poly1305 under that key with
its own random 24-byte nonce. The AAD is
`livingbrain-github-app/app-credential/v1 \0 <purpose> \0 <workspace_id> \0 <app_id>`,
where the purpose is `private-key`, `webhook-secret` or `client-secret`. Moving
a ciphertext to another column, another workspace or another app id makes it
fail to open (`KmsError::Tampered`).

Plaintext lives only in `Secret`, which is wiped on drop, prints as
`Secret([redacted])`, and is neither `Clone` nor `Serialize`. `expose()` is the
only way to read it. No route returns a credential, and none may be logged,
put in an error, or written to a queue row.

### Store API (for Part C)

All functions are `async`, take `&dyn Database` (and `&dyn Kms` where they
touch credentials), and return `Result<_, StoreError>`. `StoreError` is
`Store(DbError)`, `Kms(KmsError)`, `Forbidden` (not an owner or admin right
now) or `Invalid(&'static str)`. `now` is a `time::OffsetDateTime` from the
clock port.

State tokens (manifest and install):

```rust
create_state(db, now, StatePurpose::{Manifest|Install}, workspace_id, member_id, ttl_secs) -> String
consume_state(db, now, purpose, token, workspace_id, member_id) -> Option<StateProof>
prune_states(db, before) -> u64
```

`create_state` requires the member to be an owner or admin. `consume_state`
spends the token first (compare-and-set), then checks purpose, workspace,
member, expiry and the member's current role. Every refusal is `None`, and a
misused token is spent. `StateProof` cannot be built outside the crate, so the
two writes below can only be reached through a real round trip.

App credentials:

```rust
save_app(kms, db, now, StateProof /* Manifest */, &NewApp) -> SaveAppOutcome  // Saved | WorkspaceHasApp | AppIdTaken
app_summary(db, workspace_id) -> Option<AppSummary>          // no secrets; safe for a settings page
app_credentials(kms, db, workspace_id) -> Option<AppCredentials>  // internal: JWT, installation token, OAuth
webhook_secret(kms, db, app_id) -> Option<(workspace_id, Secret)> // to verify before the workspace is known
remove_app(db, now, workspace_id, member_id) -> bool          // owner/admin; installations deleted, repos stopped, queue dropped
```

Installations:

```rust
bind_installation(db, now, StateProof /* Install */, installation_id, &Account) -> BindOutcome
    // Bound | AlreadyThisWorkspace | BoundElsewhere | Refused | NoApp
observe_installation(db, now, app_id, installation_id, &Account) -> bool  // from a verified webhook; never binds
route_installation(db, app_id, installation_id) -> Option<Route { workspace_id, status }>
list_installations(db, workspace_id) -> Vec<Installation>
set_installation_status(db, now, app_id, installation_id, InstallationStatus) -> bool
```

Repositories (the workspace always comes from the binding):

```rust
upsert_repos(db, now, workspace_id, installation_id, &[RepoRef]) -> u64   // only under a bound, active installation
remove_repo(db, now, workspace_id, installation_id, repo_id) -> bool
list_repos(db, workspace_id) -> Vec<Repo>
get_repo(db, workspace_id, installation_id, repo_id) -> Option<Repo>
set_repo_settings(db, now, workspace_id, member_id, installation_id, repo_id, &RepoSettings) -> bool  // owner/admin
note_webhook(db, now, workspace_id, installation_id, repo_id)
```

A new repository is `pending`, included, scope `shared`, and its project is
the repository name. A scope is `shared` or `user:<member id>` for a current
member.

Deliveries and the queue:

```rust
record_delivery(db, now, app_id, delivery_id, event) -> bool   // false = replay
prune_deliveries(db, now) -> u64
enqueue(db, now, workspace_id, installation_id, &WorkItem) -> Option<String>
queued_count(db, workspace_id) -> i64
```

Drain only. Each of these takes a `&Drain` (see below):

```rust
drain::repos_due(db, &drain, limit) -> Vec<Repo>
drain::save_backfill_progress(db, &drain, now, ws, installation_id, repo_id, &BackfillProgress) -> bool
drain::record_repo_error(db, &drain, now, ws, installation_id, repo_id, error)
drain::claim(db, &drain, now, limit, lease_secs) -> Vec<Claimed>
drain::complete(db, &drain, &Claimed) -> bool
drain::fail(db, &drain, now, &Claimed, error, retry_after_secs) -> bool  // dead after 5 attempts
```

The cursor is opaque JSON of at most 64 KiB. It is meant to hold
`livingbrain_github::planner::BackfillState`. Progress lands only on a
repository that is still active.

### Expected flows

- **Manifest.** `create_state(Manifest)` → redirect to GitHub with `state` →
  callback with `code` + `state` → `consume_state` → the conversion call
  (`POST /app-manifests/{code}/conversions`) → `save_app`.
- **Install.** `create_state(Install)` → `github.com/apps/<slug>/installations/new?state=…`
  → setup URL with `installation_id` + `state` → `consume_state` → **check
  with GitHub** (`GET /app/installations/{id}` signed with this workspace's
  app JWT) that the id is an installation of this app → `bind_installation`
  with the account from that response → `upsert_repos` with the
  installation's repositories.
- **Webhook.** `webhook_secret(app_id)` → `livingbrain_github::webhook::verify`
  → `record_delivery` (stop on `false`) → `observe_installation` and
  `route_installation` → on `active`: `enqueue` / `upsert_repos` /
  `remove_repo` / `note_webhook`; installation lifecycle events go to
  `set_installation_status`. A route that is `None` or not `active` accepts
  the delivery and does nothing.
- **Cron.** `GithubApp::scheduled` mints the `Drain`, prunes expired states
  and old deliveries, and (Part C) claims queue items and advances
  `repos_due` within the invocation budget.

### The drain is the only importer

Backfill and ingest run only inside the Worker's scheduled handler. No CLI
command, local script, agent or admin route runs an import. This is enforced
in code, not just stated:

- Every progress write and every queue claim takes a `Drain`. Its only
  constructor is private to `src/drain.rs`, and it is called only from
  `drain::run`, which is the body of `Module::scheduled`.
- The test `only_the_scheduled_drain_mints_a_drain` fails if any other file
  in the crate names the constructor.
- The test `the_cli_cannot_reach_the_github_store` fails if `livingbrain-cli`
  depends on the GitHub crates.
- An admin can include or exclude a repository, but there is no "run the
  backfill now" call. A new or re-included repository is picked up by the
  next cron tick.

### Threat model

| threat | defence |
|---|---|
| Database dump or backup leak | Credentials are sealed under a KMS-wrapped key. State tokens are stored only as hashes. The queue holds references, not content. |
| Row or column swap inside the database | The AAD binds purpose, workspace and app id. A swapped ciphertext fails to open, and the failure is reported as `Tampered`, never as "no app". |
| A member (not owner or admin) connects GitHub or rebinds it | `create_state`, `consume_state`, `save_app`, `bind_installation`, `set_repo_settings` and `remove_app` re-read the role from `workspace_members` at the moment of the write. A demotion mid-flow voids the flow. |
| CSRF or a forged callback | The state is single-use, expires (15 min by default, 1 h at most), and is bound to workspace, member and purpose. It is spent before it is checked, so a misused token cannot be retried. |
| Installation hijack: binding another tenant's installation | The bind is one compare-and-set (`workspace_id IS NULL`). A row held under another app is refused. A deleted installation is never bound. Part C must confirm the installation with GitHub before binding, because the setup URL's `installation_id` is attacker-controlled. |
| An admin pre-binding an installation id of another workspace's app (skipping the GitHub check) | No data is exposed: GitHub will not mint a token for an installation of another app, and that app's webhooks do not route to the row. The other workspace's bind is refused, though, so this is a denial of service. The GitHub check in Part C is what closes it. |
| One app's webhook speaking for another app's installation | `observe_installation`, `route_installation` and `set_installation_status` all match on the app id whose secret verified the delivery. |
| Claiming another organization's installation of the public shared app (a spoofed `installation_id` on the setup URL, or an installation made on GitHub directly) | The setup callback binds nothing for the shared app. The bind needs a verify state minted for that installation id, and a GitHub user token whose `GET /user/installations` lists it and whose `GET /user` id is the installer GitHub named in the signed `installation` `created` delivery (or the personal account itself). An organization member who can see the installation but did not install it is refused (`not-installer`). |
| The shared app's keys | Worker secrets only; read per request into wiped `Secret`s; never in a response, log, error, row or queue item. The setup script converts the manifest code on the operator's machine and pipes the keys straight into `wrangler secret put`. |
| One app's delivery speaking for the other kind | Every installation read and write matches app id **and** `app_kind`; a workspace app can never be saved under the shared app id. |
| Replay of a captured delivery | `record_delivery` is insert-if-absent on (app id, delivery id). After the 30-day retention a replay is accepted again. That only costs a refetch, because work items are references and the drain re-reads current state from GitHub. |
| A webhook filing data into another workspace | `upsert_repos` and `enqueue` take the workspace from the installation binding (`INSERT … SELECT … WHERE workspace_id = ? AND status = 'active'`), so a mismatched pair writes nothing. |
| Suspended or deleted installation keeps syncing | Suspension stops the repositories, and the claim skips the queue. Deletion is terminal and drops the queue. `repos_due` and `claim` also join on installation status. |
| Secrets in logs or errors | `Secret` has no `Display`, and its `Debug` is redacted. `StoreError` carries scrubbed `DbError`s and fixed `KmsError` texts. Stored error text is limited to 300 characters with control characters stripped, and the drain must never pass a header, token or body. |
| Two cron runs at once | Queue claims are a compare-and-set on the lease, and completion needs the lease token, so an expired holder cannot complete an item that another run now holds. A repository's pass holds a lease in its cursor, and every cursor write is a compare-and-set on the cursor its writer read, so two runs never pass one repository. |

Part C closes the four items Part B left open: the install callback verifies
the installation with GitHub before `bind_installation`; installation tokens
are minted and cached in memory only; `ping` is answered after the signature
verifies; and the drain runs inside `ctx.scheduled`.

## Routes and the drain (Part C)

The routes are mounted at `/v1/github`. A cookie-authenticated write must pass
the same-origin check (`livingbrain_tokens::same_origin`); a bearer write does
not need it. Every write is owner or admin only, and the role is re-read by the
store at the moment of the write.

| route | who | what |
|---|---|---|
| `GET /v1/github` | any member | `configured`, `can_manage`, `shared {configured, slug, html_url}`, `advanced {configured}`, the workspace's own app (id, slug, client id, `html_url`, `install_url`; no secrets), installations (id, `app_kind`, `manage_url`, account, status, `bound_by`), repositories (settings, `backfill` state, items, batches, started and finished, `waiting_until`, `last_webhook_at`, `last_error`), queue `{queued, dead}` |
| `POST /v1/github/app/manifest` | owner/admin | body `{"owner":"user"}` or `{"owner":"org","org":"<login>"}`. Mints a manifest state and answers `{action, method:"POST", field:"manifest", manifest, manifest_json, expires_in}`. The UI posts a form with one field, `manifest` (= `manifest_json`), to `action`. 409 if the workspace already has an app |
| `GET /v1/github/app/callback?code&state` | the member who started it (cookie) | spends the state, calls `POST /app-manifests/{code}/conversions`, seals the credentials with `save_app`, mints an install state and redirects to `https://github.com/apps/<slug>/installations/new?state=…` |
| `POST /v1/github/app/install` | owner/admin | answers `{url}`, a fresh install link for the workspace's own app (to add another account, or to retry an install) |
| `POST /v1/github/shared/install` | owner/admin | answers `{url}`: the shared app's install link with a fresh shared-install state. 503 `github/shared-not-configured` when the shared secrets are unset |
| `GET /v1/github/install/callback?installation_id&setup_action&state` | the member who started it (cookie) | the setup URL of both kinds of app. The state's purpose picks the flow: a workspace app's installation is confirmed and bound here; a shared one is confirmed and goes on to the OAuth hop. With no state, see "The shared app" |
| `GET /v1/github/oauth/callback?code&state` | the member who started it (cookie) | the shared app's OAuth hop: proves the installer, binds, lists repositories, redirects to the settings page |
| `POST /v1/github/shared/verify` | owner/admin | body `{"installation_id": n}`: a fresh OAuth hop for an unclaimed shared installation (the retry after `installer-unknown`). Binds nothing itself; 404 when there is nothing unclaimed with that id |
| `PATCH /v1/github/repos/{repo_id}` | owner/admin | body: any of `included` (bool), `scope` (`shared` or `user:<member id>`), `project`. Answers the repository |
| `DELETE /v1/github/app` | owner/admin | forgets the app (`remove_app`) and answers `{removed, next_step:{message,url}}`: the app still exists on GitHub and must be deleted there |
| `POST /v1/github/webhook` | GitHub | see "Webhook intake". `/v1/github/webhook/{app_id}` is accepted too |

The two callbacks are browser navigations from github.com. They carry the
session cookie (`SameSite=Lax`) and always answer `302` to a fixed path,
`/settings.html?github=<outcome>[&reason=<reason>]#github`, never to a URL read
from the request. Outcomes: `installed` (with `reason=repos` when the
repository list could not be read, or `reason=suspended`), `requested` (an
organization member asked an owner to approve; no state is spent), `created`
(the app was saved but no install state could be minted), and `error` with one
of `bad-request`, `signed-out`, `not-configured`, `state` (unknown, expired,
reused, another member's or another purpose's), `conversion`, `app-exists`,
`app-id-taken`, `forbidden`, `store`, `no-app`, `github`, `not-confirmed`,
`bound-elsewhere`, `refused`, and for the shared app `unclaimed` (an install
made on GitHub directly, with no state), `not-verified` (the OAuth hop was
declined, or GitHub does not list the installation for that GitHub user),
`not-installer` (the proven GitHub user is not the one who installed it) and
`installer-unknown` (the `installation` delivery naming the installer has not
arrived yet; the address also carries `installation=<id>` and the page offers
**Finish connecting**, which calls `POST /v1/github/shared/verify`).

There is no route that runs a backfill, a sync or an import. Including a
repository makes it due for the next cron tick.

### The manifest

`name` is `Living Brain (<workspace name>) <4 hex>`, at most 34 characters (the
GitHub limit, and the suffix avoids GitHub's global name collisions). `url` is
the web app's origin (`WORKSPACES_REDIRECT_BASE`, `https://app.livingbrain.wiki`
in production). `hook_attributes.url` is `<origin>/v1/github/webhook` and
`active` is true. `redirect_url` is `<origin>/v1/github/app/callback` and
`setup_url` is `<origin>/v1/github/install/callback`. `setup_on_update` is
false, `public` is false, and `request_oauth_on_install` is false. The default
permissions are metadata, contents, issues and pull requests, all read. The
default events are `push`, `issues`, `issue_comment`, `pull_request`,
`pull_request_review`, `pull_request_review_comment` and `release`.
Installation events are always sent to an app.

### Confirming an installation

The setup URL's `installation_id` is attacker-controlled. The callback mints an
RS256 app JWT from the unsealed private key (`cratefield-push-auth`'s
`Rs256Signer`; `iat` = now − 60 s, `exp` = now + 9 min, `iss` = the app id). It
calls `GET /app/installations/{id}` and binds only when GitHub answers 200 for
that id **and** that app id. The account it binds comes from GitHub's answer,
not from the request. A 404 (another app's installation, or an id that does
not exist) is `not-confirmed`, and no row is written. The repository list is
then read with an installation token (`POST
/app/installations/{id}/access_tokens`, then `GET /installation/repositories`,
at most 10 pages) and stored with `upsert_repos`.

Installation tokens live in a per-isolate in-memory cache for 50 minutes (they
are valid for 60). They are never stored, logged or returned, and they are
dropped when GitHub answers 401 or the app is removed.

### Webhook intake

The manifest has to name the hook URL before GitHub assigns an app id, so the
URL carries none. GitHub names the app on every delivery with
`X-GitHub-Hook-Installation-Target-Type: integration` and
`X-GitHub-Hook-Installation-Target-ID: <app id>`, which GitHub documents as
"the resource where the webhook was created". That header only selects which
stored secret the body must verify under. A forged header gains nothing,
because the body still needs a valid HMAC under that app's secret. Updating the
hook URL after creation (`PATCH /app/hook/config`) would cost another call and
add a failure mode for no added security, so it is not done.

The steps, in order:

1. Refuse a body over 1 MiB with 413 (`MAX_WEBHOOK_BYTES`, which is also the
   module's body ceiling).
2. Read the app id from the path or the header. If both are present and
   disagree, there is no app.
3. Look up `webhook_secret(app_id)`.
4. Run `livingbrain_github::webhook::verify` over the raw bytes.
5. `ping` answers 200.
6. Run `Delivery::parse`. An event this connection does not act on answers
   200 `ignored`.
7. `record_delivery`. A replay answers 200 `replay`.
8. Apply the delivery.

Anything that does not verify (an unknown app, a missing or bad signature, the
wrong target type) answers the same 401.

The delivery is applied as follows:

- `installation`: `observe_installation` with the payload's account (it never
  binds). `deleted`, `suspend` and `unsuspend` go to `set_installation_status`.
  `created` under an installation that is already bound upserts its
  repositories.
- `installation_repositories`: `upsert_repos` for the added repositories and
  `remove_repo` for the removed ones.
- Content events: `note_webhook`, then a reference is enqueued. `issues` and
  `issue_comment` enqueue `item`/`<issue number>`. `pull_request`,
  `pull_request_review` and `pull_request_review_comment` enqueue
  `item`/`<pr number>`. `push` enqueues `push` (nothing for a deleted branch).
  `release` is only noted, because schema version 1 has no release record.
- Nothing happens for an installation that is unbound, not active, or bound
  under another app.

The webhook never calls GitHub. A store failure after `record_delivery`
answers 500. GitHub's redelivery of that id then reads as a replay. The daily
reopen below catches up whatever was missed.

### The drain

`GithubApp::scheduled` → `drain::run` (housekeeping, then mints the `Drain`) →
`sync::run`. Without the key ring, or without a blob, id or defer port, only
the housekeeping runs.

- **Budget.** A tick may make at most `min(planner::tick_budget(
  GITHUB_TICK_BUDGET), ctx.scheduled remaining)` GitHub calls — 400 by
  default, so twelve ticks an hour stay under an installation's 5,000/h —
  and it stops at the scheduled deadline. A token mint counts as a call; a
  cached token does not. The venture mounts the module **last**, so it
  inherits whatever steps and time the earlier modules left in the
  invocation (2,000 steps and 12 minutes in total on Workers Paid, split in
  order; docs/deploy.md, "Scheduled limits").
- **Repository lease.** The cron fires every five minutes and a run may
  last fifteen, so two runs can be in the drain at once. A pass holds its
  repository under a 15-minute lease kept in the cursor (`lease_until`),
  taken with a compare-and-set on the cursor `repos_due` read and given
  back by the save, which is a compare-and-set on the leased cursor. A
  leased repository is not due, and a push to it is retried once the lease
  ends, so no two runs pass one repository or overwrite each other's cursor.
- **Daily reopen.** A repository that finished more than 24 hours ago goes back
  to `running`. Its stored cursor is the planner's `next_run`, so this is an
  incremental pass.
- **Queue first**, with at most half the tick when backfills are due. Items are
  claimed one at a time, and only while at least 4 calls remain. `item`
  re-reads the issue, its comments and (for a pull request) its reviews, then
  files it. `push` makes a finished repository due (incremental), or sets
  `rerun` on a running one so its next pass starts as soon as this one ends.
  `push` makes no call. A failed item is retried by the store's `fail` with a
  growing delay and is parked dead after its fifth attempt.
- **Backfill.** `repos_due` (oldest `updated_at` first), skipping repositories
  whose `wait_until` has not passed. Each repository gets `ceil(left / repos
  left)` calls, at least 4. A pass makes one planned request at a time:
  listings and commit details go through `planner::apply`. A listing is held
  back while its queue holds 200 entries, which keeps the cursor far below the
  64 KiB cap.
- **Items as units.** An issue or pull request is read with all of its
  activity in one tick: the issue (unless the listing just returned it), up to
  10 pages of comments and up to 3 pages of reviews. It is then mapped
  (`record::map_item`), filed, and removed from the planner's queue. Nothing
  GitHub said is kept between ticks except the cursor. A unit that runs out of
  budget is dropped and retried. If it was the first work of its tick, it is
  filed with what was read instead, so one huge thread cannot stall a
  repository.
- **Cursor** (`backfill_cursor`): `{v, state: BackfillState, rerun,
  wait_until, failures, etag_commits, etag_issues}`. It holds no content. A
  first listing page is requested with `If-None-Match`, and a 304 costs no
  quota.
- **Limits.** Every answer goes through `rate::decide`. A wait (exhausted
  quota, `retry-after`, a 429, a 5xx, a network failure, or a 2xx that leaves
  under 10 requests) stores `wait_until` and holds every repository of that
  installation for the rest of the tick. Any other non-success is a failure:
  it is recorded in `last_error` as a status and a fixed text, and backs the
  repository off for 5 minutes, doubling up to 6 hours. A 404, 410 or 422 on
  one item or commit removes it from the queue. A 409 (an empty repository)
  and a 410 (issues disabled) are empty listings.
- **Done.** When the planner state is done, the cursor becomes `next_run()`.
  The repository is marked done, unless `rerun` was set, in which case it stays
  running for the incremental pass.

### Filing

Each record goes through `livingbrain_pages::Ingestor` as a `SourceIngest`:

- `kind`: `GithubCommit`, `GithubIssue` or `GithubPr`.
- `workspace`: the binding's workspace.
- `scope`: `page_scope(workspace, Shared)`, or the member's own scope for a
  repository set to `user:<member id>`.
- `project`: the repository's project setting (default: the repository name,
  in its original case, for example `EarthOS.world`). It is filed with no
  project if the ledger refuses the name.
- `origin_ref`: the GitHub permalink.
- `rel_path`: `github/<owner>/<repo>/commit/<sha>`, `/issue/<n>` or `/pr/<n>`.
- `body`: the canonical JSON.
- `imported_by`: the member who bound the installation (`brain:github` if that
  id was erased).
- `author`: none, because a GitHub login is not a member id.

The pipeline redacts, dedupes on `(scope, sha256)` and seals. The same GitHub
state produces the same bytes, so a re-read files nothing. A changed issue
files a new source. A record redaction will not take (too large, or refused) is
skipped. A ledger failure stops the pass without saving its cursor, so the next
tick re-reads it and dedupe absorbs whatever had already landed.

The composition roots the module's blob view on the pages key space
(`PagesRebind` now includes `github`), so the drain's source bodies are the same
physical `pages/…` objects every other ledger route reads.

### The shared app

One operator-owned GitHub App, **Living Brain**, in the `Livingbrain-wiki`
organization, serves every workspace. It is public, so any organization can
install it, and it asks for the same read-only permissions and events as a
workspace's own app (`scripts/github-shared-app/manifest.json`; a test keeps
the two in step).

**Credentials are Worker secrets, never rows.** `GITHUB_APP_ID`,
`GITHUB_APP_SLUG`, `GITHUB_APP_CLIENT_ID`, `GITHUB_APP_PRIVATE_KEY` (the PEM),
`GITHUB_WEBHOOK_SECRET` and `GITHUB_APP_CLIENT_SECRET`. They are read per
request from the config port into wiped-on-drop `Secret`s, and never
returned, logged or stored. All six must be set and well formed; with any one
missing, shared mode is off: the settings page says "GitHub isn't configured
on this deployment", `POST /v1/github/shared/install` answers 503, a delivery
naming the shared id is an unknown app (401), and the advanced flow is
unaffected. The client id and secret exist for the OAuth hop below.

**Why an OAuth hop.** A workspace's own app is private, so only its owner
can install it, and "GitHub confirms this is an installation of our app" is
enough to bind. A public app can be installed by anyone, and GitHub warns
that the setup URL's `installation_id` can be spoofed. So the shared flow
proves *who* is finishing it, as GitHub recommends, with a user token:

1. Settings → GitHub → **Install Living Brain on GitHub** (owner or admin;
   cookie same-origin, or bearer) → `POST /v1/github/shared/install` mints a
   `shared-install` state (single use, 15 minutes, bound to workspace and
   member) → the browser *navigates* (a link, not a form) to
   `https://github.com/apps/<GITHUB_APP_SLUG>/installations/new?state=…`.
2. The person installs it on an account and picks repositories. GitHub
   sends the signed `installation` `created` delivery, whose `sender` is the
   GitHub user who installed it: the webhook records the installation
   **unbound**, with that `installer_id`.
3. GitHub redirects to the setup URL. The callback spends the state,
   confirms the installation with an app JWT from the shared key
   (`GET /app/installations/{id}`: this app, this id; the account comes from
   GitHub's answer), records it under the shared app, and — unless it is
   already bound to this workspace — mints a `verify` state whose subject is
   that installation id and redirects to
   `https://github.com/login/oauth/authorize?client_id=…&redirect_uri=<origin>/v1/github/oauth/callback&state=…`.
   An installation bound to another workspace is refused here
   (`bound-elsewhere`).
4. GitHub returns to `/v1/github/oauth/callback?code&state`. The callback
   spends the verify state, exchanges the code (client id + secret) for a
   user token, and with it reads `GET /user/installations` (the installation
   must be listed, under the shared app id) and `GET /user` (the GitHub user
   id). The token is then dropped: never cached, stored, logged or returned.
5. `bind_shared_installation` binds in one compare-and-set: the row is the
   shared app's, unbound, not deleted, the member is still an owner or
   admin, the verify state names this installation, and the proven GitHub
   user **is the recorded installer** — or, while the installer is not yet
   known, the personal account the app is installed on. Then the
   repositories are listed with an installation token (shared key) and
   upserted, and the browser lands on the settings page.

The installer check is stricter than GitHub's `/user/installations` alone,
which also lists an organization's installation for members who can reach
one of its repositories. Checking organization ownership instead would need
the Members permission, which this read-only app does not ask for. The
consequence: when an organization member *requests* the app and an owner
approves it on GitHub, the approving owner is the installer and must finish
in Living Brain.

`setup_on_update` is true for the shared app. A visit to the setup URL
**without our state** (an install or a repository change made on github.com
directly) never binds: for an installation already bound to the caller's
workspace (owner or admin) it re-reads the repositories; otherwise it answers
`unclaimed`. Such an installation is recorded by its webhooks but shown to
no one; to connect it, its installer uses the button in Settings → GitHub
(GitHub then offers the existing installation's configuration). Repository
additions and removals also arrive as `installation_repositories` deliveries.

**Webhooks.** One URL, `POST /v1/github/webhook`. When
`X-GitHub-Hook-Installation-Target-ID` equals `GITHUB_APP_ID`, the body must
verify under `GITHUB_WEBHOOK_SECRET` and the delivery acts only on
installations held under the shared app; any other id is looked up among
the workspaces' own apps as before. One id is never tried against both
secrets. Signature first, then replay protection, exactly as in "Webhook
intake".

**The drain** picks the key per installation: the shared private key for an
installation held under the shared app (and only while `GITHUB_APP_ID` still
names that app), the workspace's unsealed key for its own app's
installations. Installation tokens stay memory-only.

**Disconnecting** is done on GitHub: each account's **Manage on GitHub**
link opens the installation's settings, where it can be reconfigured or
uninstalled. Uninstalling sends `installation` `deleted`, which stops its
repositories and drops its queue.

#### Creating the shared app (operator, once per environment)

Run on the operator's machine, signed in to GitHub as an owner of
`Livingbrain-wiki` and to Cloudflare with `npx wrangler login`:

```sh
python3 scripts/github-shared-app/setup.py --env production   # app "Living Brain"
python3 scripts/github-shared-app/setup.py --env staging      # app "Living Brain Staging"
```

The script serves one local page, `http://127.0.0.1:8765/`, whose form posts
the manifest to `https://github.com/organizations/Livingbrain-wiki/settings/apps/new`
(name, `url` = the origin, hook `<origin>/v1/github/webhook`, setup URL
`<origin>/v1/github/install/callback`, OAuth callback
`<origin>/v1/github/oauth/callback`, `public: true`, read-only permissions and
events). The manifest's `redirect_url` is that local server, so GitHub's
one-time conversion `code` comes back to the operator's machine, not to the
Worker. The script checks its state, converts the code, and pipes each of
the six values into `wrangler secret put NAME --env <env>` over stdin: no key
is shown in the browser or the terminal. If `wrangler` fails, what is left
goes to a `0600` file in a fresh `0700` directory with the commands to finish
by hand; delete it afterwards. `--print-manifest` shows the manifest only;
`--name` picks another name if GitHub reports "Living Brain" as taken.

The Worker cannot set its own secrets, and showing converted keys in a
browser page would put them in history and screen recordings, so the
conversion runs locally. Each `wrangler secret put` publishes a new Worker
version with the secret, so the Install button appears without a code
deploy — but the code that reads these secrets must be deployed first.

### Connecting a workspace (owner click-path)

These steps use the World-360 organization and its `EarthOS.world` repository
as the example. The settings UI is in Settings → Integrations → GitHub (the
`#github` section of `settings.html`, `app/assets/github.js`).

1. Sign in to `https://app.livingbrain.wiki` as an owner or admin of the
   workspace. Open Settings → Integrations, then GitHub.
2. Choose **Install Living Brain on GitHub**. GitHub opens the Living Brain
   app's install page.
3. Pick **World-360** (you must be an owner of it; a member's install becomes
   an approval request, and then the approving owner finishes these steps),
   choose **Only select repositories**, select `EarthOS.world`, and click
   **Install**.
4. GitHub asks you to **Authorize Living Brain** once. This only proves to
   Living Brain that you are the GitHub user who just installed it; the
   token is used for two reads and dropped.
5. Back in Settings → GitHub, World-360 is listed under Accounts and the
   repository shows as pending, shared, project `EarthOS.world` (the
   repository's own name, in its own case, so agent sessions from the same
   repository file under the same project). Change the scope or project here
   if needed. The backfill starts on the next cron tick (every 5 minutes)
   and shows its progress on this page. Nothing needs to be clicked to start
   it, and nothing can.

To add or remove repositories, or to disconnect, use **Manage on GitHub**
beside the account. If the page says GitHub has not yet told Living Brain who
installed the app, wait a few seconds and choose **Finish connecting**.

**Advanced: use your own GitHub App.** Under the collapsible section, choose
**Create the GitHub App** and the owner (your account or an organization).
GitHub's "Create GitHub App" page opens with the read-only permissions and
events filled in; create it, install it on the account, and Living Brain
confirms the installation and binds it. **Add another account** installs
your app elsewhere; **Remove** forgets its credentials, and the answer links
to the app on GitHub, which must be deleted there (Advanced, then Delete
GitHub App). Both kinds can be connected to one workspace at once.

The GitHub source filter is under Sources → GitHub. It lists commits, issues
and pull requests by kind and project, each linked to its page on github.com.

### Deployment

- Migration `github/0002` (`0032_github_0002_shared_app.sql`) adds
  `app_kind`, `account_id` and `installer_id` to `github_installations` and
  `subject` to `github_states`. Existing rows read as `workspace`, which is
  what they are. Apply it before the code that reads the new columns.
- The shared app needs the six `GITHUB_*` Worker secrets above, per
  environment, set by `scripts/github-shared-app/setup.py`. Without them,
  shared mode is off and everything else works as before.
- A workspace's own app credentials live sealed in D1 under the existing
  `HARNESS_KEK_*` ring. `WORKSPACES_REDIRECT_BASE` names each environment's
  origin, so a staging app points its webhook at the staging Worker; staging
  has its own shared app ("Living Brain Staging").
