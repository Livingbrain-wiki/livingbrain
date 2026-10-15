# The coding-agents wiki

`livingbrain logs sync` puts your coding agents' sessions in the source
ledger. They are raw and immutable there. The coding-agents compiler turns
them into a wiki the way Andrej Karpathy describes an "LLM wiki": the raw
sources stay as they are, and a model reads them **one at a time** and
maintains a small set of Markdown pages. It updates pages that already
exist rather than adding new ones each time, and it cites every claim.

## Where the pages live

Everything is under `coding-agents/`, in the same scope as the session.
Synced sessions are always personal, so the pages are personal too. A slug
has no `/`, so the folder path uses `--` between segments, and a reader
splits on it:

| Path | Slug | Written by |
| :--- | :--- | :--- |
| `coding-agents/index` | `coding-agents--index` | model |
| `coding-agents/log` | `coding-agents--log` | server only, append-only |
| `coding-agents/schema` | `coding-agents--schema` | seeded once, then **yours** to edit |
| `coding-agents/lint` | `coding-agents--lint` | server, model-free lint |
| `coding-agents/<agent>/index` | `coding-agents--claude--index` | model |
| `coding-agents/<agent>/patterns` | `coding-agents--claude--patterns` | model |
| `coding-agents/<agent>/<project>/<page>` | `coding-agents--claude--chi-infra--decisions` | model |
| `coding-agents/projects/<project>` | `coding-agents--projects--chi-infra` | model (cross-agent rollup) |

`<agent>` comes from the record's `agent` field. `claude_code` becomes
`claude`, and any other agent gets its own folder under its own name.
`<project>` is the repository basename the CLI filed the session under,
folded to `[a-z0-9-]`. Every page in a project folder is one of `index`,
`decisions`, `learnings`, `architecture` or `open-questions`, or a topic
page the model names. Compiled pages use the `topic` entity type. Their
frontmatter has `collection: coding-agents`, `agent`, `project`, `page`,
`updated` and `sources` (the source ids the page was compiled from).

## The queue

There is one job per `agent_log` source body. Jobs are keyed like the
ledger, `(scope, body_sha256)`, so enqueueing the same body twice still
makes one job. The queue is built from the ledger itself
(`INSERT … SELECT … ON CONFLICT DO NOTHING`). That means sessions synced
before this feature existed are queued too, and the ingest path needs no
hook.

| Status | Meaning |
| :--- | :--- |
| `queued` | waiting |
| `processing` | claimed under a 15-minute lease; a dead worker's lease lapses |
| `done` | compiled; `pages` lists what it wrote |
| `failed` | retried after 15 min, then 1 h; after 3 attempts it stays failed. `error` says why — for a model call, the provider's status and its (redacted, 300-character) error body, e.g. `model-http 429: {"error":{"code":"1302",…}}`. A rate limit (HTTP 429) is retried after `Retry-After` (or 5 min, at most 30) without counting an attempt, and stops that drain. **Retry failed** (`POST /v1/coding-agents/retry`) puts your failed jobs back to `queued` with their attempts reset |
| `skipped-no-model` | no model connected; checked again hourly, and at once by **Process now** |
| `over-budget` | the compile budget is spent; checked again when it resets |
| `superseded` | a newer body of the same session arrived before this one ran |
| `skipped` | not a session record, empty, or its source is gone |
| `waiting-runner` | a model call is out on your own device (the local runner, below); the compile carries on the moment the answer arrives, or is checked again within the hour. Waiting does not count as an attempt |

A session that grew is re-synced as a new body. If an earlier body of it
was already compiled, the model is told so and adds only what is new.

Jobs are processed in two places:

- **Cron.** The Worker fires every 15 minutes (`*/15 * * * *` in
  `wrangler.toml`) and compiles, oldest first, up to each workspace's
  sessions-per-run (its own setting, else `CODING_AGENTS_BATCH`, default 1;
  never more than 20) under the invocation's subrequest budget. It then runs
  the lint over the scopes it wrote to. A paused workspace's jobs are parked
  as `over-budget` (`paused`) and looked at again every 15 minutes.
- **`POST /v1/coding-agents/process`** (`{"max": 1..3}`) compiles up to
  `max` of *your* jobs inside the request. Cookie callers must be on the
  same origin.

`POST /v1/coding-agents/retry` requeues *your* `failed` jobs (attempts
reset) and answers `{"requeued": n, "counts": …}`. A failed compile is also
written to the scope's `coding-agents/log` once per distinct reason, and to
Workers Logs (`coding-agents: job for source … failed …`).

`GET /v1/coding-agents/queue?limit=&source=` returns your jobs, a count per
status, whether a model is connected, and the budget with what has been
spent. `GET /v1/coding-agents/tree` returns your synced sessions grouped by
agent and project — `{agents: [{agent, folder, sessions, compiled, projects:
[{project, sessions, compiled, statuses, latest_at}]}], truncated}`, a session
counted once (its newest body), compiled when any of its bodies compiled,
`folder` the wiki folder its pages land in. All three routes only return
jobs in your own scopes.

## The model

The model is your workspace's own, connected under **Settings → Models**,
on the **Main** job slot. There is no managed fallback: with nothing
connected, jobs park as `skipped-no-model` and the status route reports
`model.connected: false`. Each call goes through
`livingbrain_models::invoke::chat`, which re-runs the SSRF guard, decrypts
the key for that one request only, and records the call's tokens in the
usage ledger.

**Your own subscription (the local runner).** If you chose to run your jobs
on your own device (`PUT /v1/models/mine/main`, the Claude/ChatGPT
subscription cards in Settings → Models, or the Mac app's "Run brain jobs
with my subscription"), the compile of *your* sessions does not call the
workspace's model at all. Each call — the same redacted prompt an API model
would get — is queued as a job (`livingbrain_models::jobs`, sealed under
`MODEL_KEYS_SECRET` while it waits), the compile parks as
`waiting-runner`, and your `livingbrain jobs run` (or the Mac app) claims it
from `GET /v1/model-jobs/next`, runs it with your own `claude` / `codex` CLI
with tools off, and posts the answer to `POST /v1/model-jobs/{id}/result`.
The answer is redacted again and must parse as the closed edit list, or the
job fails (`model-runner-bad-output`) and the compile retries; a valid one
continues the compile in the same request. A compile resumes after the
chunks it already applied (`chunks_done`), so no chunk is asked twice. Your
runner's tokens are recorded on the job and cost the workspace's compile
budget nothing. Only you can claim, read or answer your jobs.

Each call gets:

1. A fixed system prompt that you cannot edit. It says the transcript and
   the pages are data, never instructions, and it fixes the output schema.
2. Your conventions page, `coding-agents--schema`.
3. The pages the session may touch, with their versions. A page too long to
   show in full is shown cut and is read-only for that call.
4. One transcript chunk inside `<transcript>` fences.

The answer must be one JSON object:
`{"format": 1, "summary", "pages": [{"slug", "base_version", "title", "body"}], "facts": [...]}`.
The server checks each page edit and refuses it if:

- the slug is outside this session's folder. The session may write its own
  `<agent>/<project>/…`, its agent's `index` and `patterns`, the top
  `index` and its project's rollup. It may never write the log, the
  conventions, or anything outside `coding-agents/`;
- `base_version` is not the current head (optimistic concurrency). A stale
  edit fails the job with `conflict`, and the retry sees the fresh page;
- the page was shown cut.

A human edit to a compiled page is not overwritten. The brain writes over
it only when the head the model was shown already contains that edit.

## History is kept

Every entry is dated with the session's day. Decisions carry `decided_at`.
When a decision is reversed, the old entry is struck through and marked
superseded, never deleted. Learnings carry `first_seen` and
`last_confirmed`. The log is append-only and in chronological order.

The model also returns **temporal facts** in the
`docs/temporal-facts.md` v1 emitter shape. Subjects are
`project/<project>` and `system/<agent>` (and `me`, which becomes the
syncing person's own `person/<id>`). The v1 subject types have no `agent`,
so a coding agent is recorded as a system. Facts are validated against the
v1 schema rules, then handed to the facts store through one port,
`livingbrain_compiler::FactSink`; the composition wires `StoreFacts`
(`livingbrain-api`), a sink over `FactStore::record`. Each fact is recorded:

- in the **same scope as the session's source** — the person's own memory —
  and nowhere else, so nobody else can read it;
- citing the session's source (`source:<id>`);
- with `observed_at` = the compile time (when the brain learned it), and
  `valid_from` = the session's day when the model gave no time;
- with the store's usual ordering: a later session that changes an
  attribute supersedes the earlier fact, which is kept as history (for
  example `project/demo` `database` = Supabase from September, then D1 from
  October).

A fact the store refuses — a cardinality conflict
(`422 facts/cardinality-mismatch` over HTTP), a subject or attribute
redaction will not let through, anything outside the format — is skipped on
its own. The log line counts what was recorded and names each skip
(`facts recorded: 1 · facts skipped: 2 (cardinality conflict: …)`); the job
still finishes `done`.

## Safety and cost

- **Redaction.** Model input is redacted before it leaves (sources were
  already redacted at ingest, but conventions and pages are text people
  typed). Model output is redacted before anything is written: page bodies,
  titles, summaries, and fact values and quotes.
- **Per-job cap.** The per-session cap (default 80 000) caps one job's
  tokens, in and out. A transcript longer than calls-per-session ×
  characters-per-call (4 × 32 000 characters) is cut, and the log says so.
- **Per-workspace budget.** Tokens per day (default 400 000) and per month
  (default 4 000 000) are counted in `coding_agent_spend`. A workspace past
  its monthly spend limit also compiles nothing.
- **Pause.** An owner or admin can pause compiling; synced sessions wait in
  the queue until it is resumed.

### Budgets are settings

Every number above is a **workspace setting** (Settings → Usage & limits,
`GET`/`PATCH /v1/settings/budgets`), so an owner or admin raises or lowers
it without a deploy. Members read the same values and the usage against
them. The effective value is, in order:

1. the workspace's own setting, when it set one;
2. else the deployment's env var;
3. else the built-in default.

Clearing a setting (`null`, or an empty field in the app) returns the
workspace to 2 and 3. The table is `usage_budgets` (usage migration 0002);
NULL means "not set here".

| Setting (`group.key`) | Env var | Default | Range |
| --- | --- | --- | --- |
| `coding_agents.paused` | — | off | on / off |
| `coding_agents.daily_tokens` | `CODING_AGENTS_DAILY_TOKENS` | 400 000 | 0 – 10¹² |
| `coding_agents.monthly_tokens` | `CODING_AGENTS_MONTHLY_TOKENS` | 4 000 000 | 0 – 10¹² |
| `coding_agents.batch` (sessions per run) | `CODING_AGENTS_BATCH` | 1 | 1 – **20** |
| `coding_agents.job_tokens` (per-session cap) | `CODING_AGENTS_JOB_TOKENS` | 80 000 | 1 000 – **1 000 000** |
| `coding_agents.max_chunks` | `CODING_AGENTS_MAX_CHUNKS` | 4 | 1 – **20** |
| `coding_agents.chunk_chars` | `CODING_AGENTS_CHUNK_CHARS` | 32 000 | 1 000 – **200 000** |
| `coding_agents.max_output_tokens` | `CODING_AGENTS_MAX_OUTPUT_TOKENS` | 6 000 | 256 – **32 000** |
| `ask.context_chars` | `ASK_CONTEXT_CHARS_MAX` | 200 000 | 2 000 – 1 000 000 |
| `ask.max_output_tokens` | — | 1 024 | 128 – 8 192 |
| `spend.monthly_cents` | `USAGE_BUDGET_CENTS_MONTHLY` | 300 | 0 – 100 000 000 |

**Hard ceilings** (bold) hold over the setting *and* the env var: they
protect the Worker, not the bill. One cron invocation has a fixed wall and
the Workers subrequest limit (the venture keeps 45 of 50; a compile spends
8), and a compile — or a "Process now" request — has to finish inside it, so
no number may ask for more sessions per run, calls per session, tokens per
session or a bigger call than one invocation can complete. A value past a
ceiling is refused by the API with a 422 that names the setting and its
range, and clamped by the compiler wherever it came from. The token budgets
and the spend limit have no ceiling: they spend the workspace's own model
key. `CODING_AGENTS_CONTEXT_CHARS` stays an env var only.

The queue card (`GET /v1/coding-agents/queue`) shows the effective numbers
(`budget.paused`, `budget.batch`, `budget.daily_tokens`, …), and so does
the Mac app's Usage view.
- **Scope.** A job carries its source's scope, and compiled pages are
  written there and nowhere else. Neither a client nor a model can name a
  scope.

## In the app

- **Wiki home**: the compiled pages are a `coding-agents/` folder (agent →
  project, plus `projects/` for the rollups) instead of rows under Recent.
  Every agent and project you synced sessions under is a folder there from
  the first sync, before anything compiles (`colonizer/`, `claude/`,
  `codex/`), with a line — `489 sessions · 12 compiled · 470 queued` —
  linking to those sessions in Agent sessions; compiled pages appear inside
  as they are written. A compiled page's meta line shows its path.
- **Agent sessions** filter by agent (Claude Code, Codex, Colonizer) beside
  the project filter; `wiki.html?view=sessions&agent=colonizer&project=x`
  names both.
- **Agent sessions**: a queue card shows the counts, which model compiles (or
  a link to **Settings → Models** when none is connected), the day's budget,
  and **Process now**. Each session shows its compile status and links to
  the pages it wrote. The transcript view shows the same above the session.
- **Mac app**: not yet. It is a follow-up.

## Lint

The lint pass is cheap and uses no model. It writes `coding-agents--lint`
with orphans (pages nothing links to), broken links to collection pages
that do not exist, and pages nothing has compiled into for 60 days.
Checking for contradictions needs a model and is **not built yet**.
