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
