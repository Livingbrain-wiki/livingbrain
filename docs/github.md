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

`planner::plan(&state, DEFAULT_BUDGET)` returns at most 45 requests per tick
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
owns migration `github/0001` (collected as
`crates/livingbrain-venture/migrations/0028_github_0001_init.sql`). The routes
and the drain body (Part C) are described in "Routes and the drain" below.

### One app per workspace

The manifest flow is started by a workspace owner or admin and creates a
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
| `github_states` | single-use state tokens for the manifest and install round trips: the SHA-256 of the token, never the token |
| `github_installations` | installation id, app id, the bound workspace (NULL until bound), the account login and type, status `active` / `suspended` / `deleted`, `bound_by` |
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
| Replay of a captured delivery | `record_delivery` is insert-if-absent on (app id, delivery id). After the 30-day retention a replay is accepted again. That only costs a refetch, because work items are references and the drain re-reads current state from GitHub. |
| A webhook filing data into another workspace | `upsert_repos` and `enqueue` take the workspace from the installation binding (`INSERT … SELECT … WHERE workspace_id = ? AND status = 'active'`), so a mismatched pair writes nothing. |
| Suspended or deleted installation keeps syncing | Suspension stops the repositories, and the claim skips the queue. Deletion is terminal and drops the queue. `repos_due` and `claim` also join on installation status. |
| Secrets in logs or errors | `Secret` has no `Display`, and its `Debug` is redacted. `StoreError` carries scrubbed `DbError`s and fixed `KmsError` texts. Stored error text is limited to 300 characters with control characters stripped, and the drain must never pass a header, token or body. |
| Two cron runs at once | Queue claims are a compare-and-set on the lease, and completion needs the lease token, so an expired holder cannot complete an item that another run now holds. |

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
| `GET /v1/github` | any member | `configured`, `can_manage`, the app (id, slug, client id, `html_url`, `install_url`; no secrets), installations (id, account, status, `bound_by`), repositories (settings, `backfill` state, items, batches, started and finished, `waiting_until`, `last_webhook_at`, `last_error`), queue `{queued, dead}` |
| `POST /v1/github/app/manifest` | owner/admin | body `{"owner":"user"}` or `{"owner":"org","org":"<login>"}`. Mints a manifest state and answers `{action, method:"POST", field:"manifest", manifest, manifest_json, expires_in}`. The UI posts a form with one field, `manifest` (= `manifest_json`), to `action`. 409 if the workspace already has an app |
| `GET /v1/github/app/callback?code&state` | the member who started it (cookie) | spends the state, calls `POST /app-manifests/{code}/conversions`, seals the credentials with `save_app`, mints an install state and redirects to `https://github.com/apps/<slug>/installations/new?state=…` |
| `POST /v1/github/app/install` | owner/admin | answers `{url}`, a fresh install link (to add another account, or to retry an install) |
| `GET /v1/github/install/callback?installation_id&setup_action&state` | the member who started it (cookie) | spends the state, confirms the installation with GitHub, binds it, lists its repositories, redirects to the settings page |
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
`bound-elsewhere`, `refused`.

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

- **Budget.** A tick may make at most `min(planner::DEFAULT_BUDGET,
  ctx.scheduled remaining)` GitHub calls, and it stops at the scheduled
  deadline. A token mint counts as a call; a cached token does not. The venture
  mounts the module **last**, so it inherits whatever subrequests the earlier
  modules left in the invocation (45 in total, split in order).
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
  lowercased). It is filed with no project if the ledger refuses the name.
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

### Connecting a workspace (owner click-path)

These steps use World-360's `EarthOS.world` as the example. They assume Part D
has built the settings UI on the routes above.

1. Sign in to `https://app.livingbrain.wiki` as an owner or admin of the
   workspace. Open Settings, then GitHub.
2. Choose **Create the GitHub App**. For World-360, pick the organization owner
   `EarthOS-world` (or whichever GitHub org owns the repository). Living Brain
   calls `POST /v1/github/app/manifest` and posts the manifest form to GitHub.
3. On GitHub's "Create GitHub App" page, the name, permissions (read only) and
   events are filled in. Click **Create GitHub App for EarthOS-world**. GitHub
   creates the app and sends you back to Living Brain, which stores its
   credentials sealed and forwards you to the app's install page.
4. On the install page, pick the account (`EarthOS-world`), choose **Only
   select repositories**, select `EarthOS.world`, and click **Install**. GitHub
   sends you back to Living Brain, which confirms the installation with GitHub,
   binds it to the workspace and lists the repository.
5. Back on Settings → GitHub, the repository shows as pending, shared, project
   `earthos.world`. Change the scope or project here if needed. The backfill
   starts on the next cron tick (every 15 minutes) and shows its progress on
   this page. Nothing needs to be clicked to start it, and nothing can.

To disconnect, choose **Remove**. Living Brain forgets the credentials, and the
answer links to the app on GitHub, which must be deleted there (Advanced, then
Delete GitHub App).

### Deployment

No new secret or binding is needed. The app credentials live sealed in D1 under
the existing `HARNESS_KEK_*` ring. The tables are the existing `github/0001`
migration. `WORKSPACES_REDIRECT_BASE` already names each environment's
origin, so a staging app points its webhook at the staging Worker.
