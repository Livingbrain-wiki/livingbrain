# Deploying Living Brain

The Worker is deployed by `.github/workflows/deploy.yml`. This page is the
runbook: the environments, the one-time setup they need, and what to do when
a deploy is wrong. Nothing here is a secret, and no secret belongs in this
repository — every key lives in Cloudflare or in GitHub's secret store.

## Environments

Wrangler does not inherit bindings or variables into an environment, so
`crates/livingbrain-venture/wrangler.toml` repeats the whole set in each
`[env.*]` block. The top-level config is local development and is never
deployed.

| | local | staging | production |
| --- | --- | --- | --- |
| Worker name | `livingbrain` | `livingbrain-staging` | `livingbrain` |
| `ENV` | unset (`development`) | `staging` | `production` |
| D1 database | simulated by `--local` | `livingbrain-staging` | `livingbrain` |
| R2 bucket | simulated by `--local` | `livingbrain-blobs-staging` | `livingbrain-blobs` |
| KV namespace | simulated by `--local` | a per-environment namespace | a per-environment namespace |
| Deployed by | nothing — `wrangler dev` | every push, `main` or `v*` tag | a `v*` tag, after approval |
| Public host | `127.0.0.1:8787` | `staging-api.livingbrain.wiki` (Worker API and the `app/` web app, one origin) | `api.livingbrain.wiki` (custom domain, `[env.production]`) |

`ENV` is not decoration. The harness reads it twice: it names the
environment to the production readiness gate at boot, and it binds every
cookie the Worker signs as `{HARNESS_VENTURE}|{ENV}`, so a session minted in
staging never verifies in production. It must match the `ENV` the minting
side uses, which is why the smoke test passes it explicitly.

## Staging resources (created 2026-10-08)

D1 `livingbrain-staging` (`f3a32632-ca97-4038-b094-2a9e1d7f413d`), R2
`livingbrain-blobs-staging`, KV `KV-staging`
(`333ff8d934b04e87a0c6cb85f9daa572`) and the rate-limit namespace `4002`. The
staging Worker serves the static app from `app/` (minus `app/.assetsignore`)
on the same custom domain as the API. Mail leaves from
`no-reply@send.livingbrain.wiki`, the domain verified in Owlpost.

Staging secrets: `HARNESS_SECRET`, `HARNESS_KEK_CURRENT` + `HARNESS_KEK_V1`
(the page and Slack-token key ring; without it `/v1/pages/mcp` is not mounted
and the Slack install answers 503), `MODEL_KEYS_SECRET`, `OWLPOST_API_KEY`,
`ADMIN_TOKEN`, and for Slack `WORKSPACES_SLACK_CLIENT_SECRET` and
`WORKSPACES_SLACK_SIGNING_SECRET`.

## Production

Public host `api.livingbrain.wiki`, a custom domain routed in
`[env.production]`; nothing reaches it without a human.

```sh
# Resources, once each; the ids go into the matching [env.production] lines.
npx wrangler d1 create livingbrain
npx wrangler r2 bucket create livingbrain-blobs
npx wrangler kv namespace create KV-production
```

Paste the ids into `[env.production.d1_databases]` and
`[env.production.kv_namespaces]` — both are placeholders in `wrangler.toml`
until this is done. The route is `api.livingbrain.wiki` with
`custom_domain = true`, which requires the `livingbrain.wiki` zone on the
same Cloudflare account.

### Secrets — each set with `wrangler secret put NAME --env production`

```sh
npx wrangler secret put HARNESS_SECRET --env production                   # signs sessions and confirm links
npx wrangler secret put HARNESS_KEK_CURRENT --env production              # seals page bodies and Slack tokens; never staging's value
npx wrangler secret put MODEL_KEYS_SECRET --env production                # wraps the BYOK model-key ciphertext; never staging's value
npx wrangler secret put WORKSPACES_SLACK_CLIENT_SECRET --env production   # the production Slack app, not the staging one
npx wrangler secret put WORKSPACES_SLACK_SIGNING_SECRET --env production  # verifies Slack's request signatures
npx wrangler secret put OWLPOST_API_KEY --env production                  # sends the mail
npx wrangler secret put TURNSTILE_SECRET --env production                 # captcha gate; absent = joins refused, fail-closed
npx wrangler secret put ADMIN_TOKEN --env production                      # the waitlist CSV export
```

Never reuse a staging secret value in production, and never rotate a KEK
casually: rotating `HARNESS_KEK_CURRENT` or `MODEL_KEYS_SECRET` orphans the
ciphertext already sealed under it.

### The go-ahead gate

Production deploys run only on `v*` tags and wait for approval from the
GitHub environment `production`'s required reviewers — that approval is the
owner's go-ahead. `PRODUCTION_URL` must be set to
`https://api.livingbrain.wiki` for the health smoke. The smoke is
health-only here — production is never seeded, and its signing key never
reaches a runner, unlike staging's authenticated read — and a deploy that
fails it is rolled back with `scripts/deploy/rollback.sh production`.

### Pilot

The pilot is two weeks on one internal Slack workspace. Install the Slack app
to the workspace through the install flow (`/v1/workspaces/slack/install`),
sign in, and connect the workspace's model key at `PUT /v1/models/{role}` —
BYO key; the catalog includes DeepSeek.

Day 1, mention the bot with a question in a channel: the reply must answer
with a citation link into the wiki — the issue's first acceptance criterion.
Record the date and a screenshot or link in the pilot log.

Each day, as a workspace admin with a personal access token:

```sh
curl -sH "Authorization: Bearer $LIVINGBRAIN_TOKEN" "https://api.livingbrain.wiki/v1/usage/daily?days=14"
```

It returns per-day counts (model requests/tokens, D1 queries, emails sent),
attributed cost, month-to-date, and `status` — `ok` or `over` against
`USAGE_BUDGET_CENTS_MONTHLY`, 300 cents by default: the $3.00 infra share of
the $9/mo hosted Teams plan at a ~2/3 margin (pricing v3 in
[origin-and-plan.md](origin-and-plan.md)). Model keys are BYO, so provider
token spend is the workspace's own; the ledger records the tokens anyway, for
credit accounting.

Honesty notes: D1 cost has no per-workspace attribution yet, so while the
pilot is single-workspace, read account-level D1 from the Cloudflare
dashboard once a day and note it in the log; the `d1_queries` column is
recorded for when attribution lands. Email costs 0 during the pilot (inside
the mail provider's free tier); sends are counted.

| date | answer with citation | usage status | D1 (dashboard) | notes |
| --- | --- | --- | --- | --- |
| 2026-10-13 (example) | yes — reply linked the page | `ok` | $0.02 | day 1: criterion met |
| 2026-10-14 | — | `ok` | $0.02 | |
| 2026-10-15 | — | `ok` | $0.03 | |
| 2026-10-16 | — | `ok` | $0.02 | |

Success: two weeks of daily `status` `ok` and the day-1 citation check.

## One-time setup

The owner does this once, per environment. Every id in `wrangler.toml` is a
placeholder until then, and a deploy with a placeholder id fails at
`wrangler d1 migrations apply` rather than silently doing nothing.

```sh
# Resources, once each.
npx wrangler d1 create livingbrain-staging
npx wrangler d1 create livingbrain
npx wrangler r2 bucket create livingbrain-blobs-staging
npx wrangler r2 bucket create livingbrain-blobs
npx wrangler kv namespace create KV-staging
npx wrangler kv namespace create KV-production
```

Paste the printed ids into the matching `database_id` and `id` lines in
`wrangler.toml` and commit that change. Secrets are per environment and go in
Cloudflare, not in the file — `npx wrangler secret put HARNESS_SECRET --env
staging` (at least 32 bytes), then the same for
`WORKSPACES_SLACK_CLIENT_SECRET` and `OWLPOST_API_KEY`, and the same three
for `--env production` with different values.

GitHub repository **secrets** (Settings → Secrets and variables → Actions):

- `CLOUDFLARE_API_TOKEN` — an API token with Workers Scripts (Edit), D1
  (Edit), Workers KV (Edit) and Workers R2 (Edit) on this account.
- `CLOUDFLARE_ACCOUNT_ID` — the account id.
- `STAGING_HARNESS_SECRET` — the **staging** `HARNESS_SECRET`, and nothing
  else. It is the only reason the smoke test can make an authenticated read.
  Never put the production value here: a runner that holds the production
  signing key can mint a valid production session.

GitHub repository **variables**: `STAGING_URL` and `PRODUCTION_URL`, the two
Workers' public origins (e.g. `https://staging-api.livingbrain.wiki`).

GitHub **environment** `production`: create it and add the owner under
**Required reviewers**. That approval is the entire production gate — a tag
cannot deploy unattended, and a human cannot skip the staging smoke test by
pushing a tag, because the production job needs staging to be green. The
`staging` environment needs no reviewers; it exists so deployments are listed
and named.

## How a deploy flows

1. **Pull request** — the `dry-run` job builds the Worker and runs
   `wrangler deploy --dry-run` against the top-level config and both
   environments. It needs no secrets, so it runs on fork pull requests, and
   it only proves the config parses and the bundle builds.
2. **Merge to `main`** — the `staging` job applies the D1 migrations to the
   remote staging database, deploys, seeds `scripts/deploy/seed-staging.sql`
   and smoke tests. A failure rolls staging back.
3. **Push a `v*` tag** — the `staging` job runs once more (every push runs
   it; the production job needs it), then `production` waits for a human to
   approve the `production` environment, then migrates, deploys and health
   checks.

A failing smoke test fails the job, and a failed staging job means the
`production` job never starts.

## The smoke test

`scripts/deploy/smoke.sh BASE_URL` is the only thing standing between a build
and a live environment. It does `GET /__health` and expects 200, retrying for
about fifteen seconds because a fresh deploy can take a moment to answer.
When `HARNESS_SECRET` is set — staging only — it then mints a
`workspaces.session` cookie with `cargo run -p livingbrain-workspaces
--example mint_session` and makes one authenticated read,
`GET /v1/workspaces/me`, expecting 200 and `workspace.id == T0SMOKETEST`, the
workspace the seed SQL creates. Production runs with `HARNESS_SECRET` unset
and gets the health check only: production is never seeded, and the
production signing key never reaches a runner. Any failure exits non-zero
with a line naming what failed, so the log says whether the Worker was down
or the cookie did not verify.

## Rollback

Automatic: if a deploy succeeds and its smoke test then fails, the workflow
runs `scripts/deploy/rollback.sh <env>`, which rolls that environment back to
the previous version. The job still fails — the rollback is in addition to
the failure, not instead of it. Manual, when staging was green and production
is not:

```sh
npx wrangler deployments list --env production   # find the version to return to
scripts/deploy/rollback.sh production <version-id>
```

A rollback does **not** undo D1 migrations. They are forward-only, so the only
safe rollback is a Worker that still understands the schema it finds — which
is the migration rule below.

## Migrations

Forward-only, and backward-compatible for one release. A migration that
breaks the previous Worker cannot be rolled back to, so **expand** first — add
the new column (nullable, or with a default) or the new table in one release,
deploy it, and the old Worker keeps running — and **contract** a release
later, once the code that used it is gone from every environment. Within one
release, never rename a column, change a column's type in place, or make a
NOT NULL column that has no default. Editing a migration that has already
been applied does nothing at all: `wrangler d1 migrations apply` only runs the
files it has not already recorded in `d1_migrations`.

## Later

Keep Shipping is planned to run the deploys for the site and the Worker
instead of this workflow (see [origin-and-plan.md](origin-and-plan.md)). When
it does, this page is what it has to reproduce: the environment split, the
migrations before the deploy, the smoke test, and the approval in front of
production.
