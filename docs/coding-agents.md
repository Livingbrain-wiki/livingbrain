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
folded to `[a-z0-9-]`. That fold is the **canonical project key** everywhere
(`livingbrain_pages::project_key`): `EarthOS.world` (a repository, a GitHub
item) and `earthos-world` (a local clone, this folder) are one project. The
sources filter (`GET /v1/sources?project=`), its projects facet (one entry
per key, `{project, key, sources}`, named by the repository's own spelling),
the page listing (`GET /v1/pages?project=` — the project's compiled pages
under every agent) and the brain map's project node all match on the key and
show the original spelling. Every page in a project folder is one of `index`,
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
| `skipped-trivial` | triage found nothing to compile (an empty session, a greeting); `error` says why (`empty`, `trivial`, `jev:skip`). No model call. **Compile fully** compiles it anyway |
| `summary-only` | triage logged the session in one model-free line instead of compiling it (`failing-loop`, `no-changes`, `jev:summary`). No model call. **Compile fully** compiles it anyway |

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
status, whether a model is connected, the budget with what has been spent,
and a `triage` block: whether triage and compaction are on, the strictness,
whether a classifier (Jev) is wired, how many sessions were skipped,
summarised and compiled in full, and the estimated input tokens without and
with triage and compaction (`tokens_raw`, `tokens_sent`, `tokens_saved`).
Each job carries its `triage` (`decision`, `reason`, `forced`), its
`estimate` (`raw`, `sent`) and `can_compile_fully`. `GET
/v1/coding-agents/tree` returns your synced sessions grouped by agent and
project — `{agents: [{agent, folder, sessions, compiled, projects:
[{project, sessions, compiled, statuses, latest_at}]}], truncated}`, a session
counted once (its newest body), compiled when any of its bodies compiled,
`folder` the wiki folder its pages land in. All these routes only return
jobs in your own scopes.

`POST /v1/coding-agents/compile-fully` (`{"source_id"}`) is **Compile
fully**: it puts one of *your* `skipped-trivial` or `summary-only` jobs back
to `queued` with no triage and no compaction, and compiles it in the
request. Someone else's job, or one that triage did not hold back, is a 404.

## Triage and compaction

Most of a coding-agent session is tool output, and many sessions teach the
wiki nothing. Before a session costs a model call, the compiler decides how
much of a compile it deserves, then cuts what it sends down to what matters:

```
session → triage (skip | summary | full, + topic pages) → compaction → compile
```

**Triage** has three outcomes:

- **skip** → `skipped-trivial`: an empty session, or a greeting with no tool
  call. Nothing is written.
- **summary** → `summary-only`: a session that changed no file and went round
  a failing loop (one call failing three or more times, most calls failing),
  or a short lookup that changed nothing. The server appends one line to
  `coding-agents/log` saying what the session did (counts, the failing
  command, "no files changed").
- **full** → the normal compile, on the compacted transcript.

A session that changed a file (an edit, a write, a patch, a commit or a push)
is always **full**, whatever else says otherwise. A version-1 record, which
names its tools and nothing else, is never judged "changed nothing".

**Strictness** (`lenient`, `balanced`, `strict`) sets how readily a session
is held back: `lenient` skips only empty sessions and summarises only
failing loops; `strict` summarises any session that changed nothing and said
little (under 4 000 characters from the agent).

**Jev.** When the deployment has a `TYPESAFE_API_KEY`, the compiler also asks
Jev — TypeSafe's System One, a purpose-trained classifier at
`api.typesafe.ai`, called over HTTP from the Worker through the harness's
`Classifier` port (`cratefield-adapter-typesafe`, model pinned) — **once per
session**, in one request:

- a `choice` between skip, summary and full;
- for each of the session's own topic pages (up to 24), whether the session
  bears on it — only those it picks are shown to the model, beside the
  fixed pages (index, decisions, learnings, …);
- for each bulky tool output (up to 32), whether it is needed word for word —
  kept whole when it is sure, cut to its error lines when it is sure it is
  not.

Jev may always *raise* the rule's decision; it may *lower* it only when its
confidence passes the strictness's bar (0.9 / 0.75 / 0.6), and never below
full for a session that changed files. A slow, failing or unwired Jev
leaves the rule's decision. What Jev reads is a redacted digest, at most
24 000 characters: the counts, the person's and the agent's words cut short,
and one line per tool call (tool, file, failed or not, the command's head) —
never a tool's output. It costs $0.042 per million input tokens (output is
free), billed to the operator's TypeSafe account, and the data is hosted in
the US.

**Compaction** shrinks what the model is sent, never the source:

- every user message whole, and the agent's own text (decisions live there);
- every tool call's name, file, status and exit code, and its command;
- from each output, the error lines first, then `file:line` locations and
  warnings, and the last two lines (test totals, the end of a build); a run
  of same-shaped lines collapses to one with a count;
- a read (a file, a search, a listing, a page) keeps how much it read, not
  what — the source has it;
- a patch becomes its file and hunk headers and `+N −M lines`;
- the second and later copies of an identical call become one line ("same
  call as in turn 2, again, failed with exit 1 (#7)");
- a subagent (record schema 3) becomes its outline: what it was asked, one
  line per call of its own that changed or failed something, and its answer
  — at most 1 500 characters, where the uncompacted renderer cut the whole
  subagent to one 600-character JSON line. Its edits count as the session's
  changes for triage;
- no other compacted call is longer than the uncompacted renderer's own cap
  (600 characters).

**Nothing is lost.** The source stays in the ledger untouched, and every page
still cites it. A wrong triage call is undone by **Compile fully** — no
triage, no compaction. A compile that waits on your own runner resumes on the
plan it started with (stored on the job), so its chunks line up.

**Accounting.** Each job records an estimate of the input tokens the compile
would have sent without triage and compaction (`est_tokens_raw`) and with
them (`est_tokens_sent`): the transcript chunks plus the fixed prompt and the
conventions per call. The pages shown per call are left out on both sides,
so the saving it reports is a floor.

## The model

The model is your workspace's own, connected under **Settings → Models**,
on the **Main** job slot. There is no managed fallback: with nothing
connected, jobs park as `skipped-no-model` and the status route reports
`model.connected: false`. Each call goes through
`livingbrain_models::invoke::chat`, which re-runs the SSRF guard, decrypts
the key for that one request only, and records the call's tokens in the
usage ledger.

**Streaming.** A call asks the provider to stream its answer
(`"stream": true`; on the OpenAI wire also `stream_options.include_usage`).
A forced-thinking model such as Z.AI's GLM-5.3 can reason for more than
100 s before the first answer byte, and Z.AI's Cloudflare front cuts a
reply that has sent nothing for 100 s with `524`; a streamed reply sends its
thinking as it goes, so the connection never sits idle. In the Worker the
stream is read as it arrives (`livingbrain_models::stream`, opened by the
venture's `model_stream::FetchStreaming` over `worker::Fetch`): a call fails
as `model-http: the model stream stalled` after 90 s with no bytes, and as
a deadline past the call's own timeout (at most 15 minutes). The thinking is
dropped and only the answer, the usage and the stop reason are kept; an
`error` event mid-stream fails the call with its status and body, like a
refusal. A provider whose card in `app/assets/model-cards.json` sets
`"stream": false` is called unstreamed. When Living Brain moves to
`cratefield-core` 0.9, the venture's seam is replaced by the harness's
`HttpClient::send_streaming` — the steps are in the header of
`crates/livingbrain-models/src/deadline.rs`.

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
workspace to 2 and 3. The table is `usage_budgets` (usage migrations 0002
and 0003); NULL means "not set here".

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
| `coding_agents.triage` | `CODING_AGENTS_TRIAGE` (`on`/`off`) | on | on / off |
| `coding_agents.compaction` | `CODING_AGENTS_COMPACTION` (`on`/`off`) | on | on / off |
| `coding_agents.triage_strictness` | `CODING_AGENTS_TRIAGE_STRICTNESS` | `balanced` | `lenient` / `balanced` / `strict` |
| `ask.context_chars` | `ASK_CONTEXT_CHARS_MAX` | 200 000 | 2 000 – 1 000 000 |
| `ask.max_output_tokens` | — | 1 024 | 128 – 8 192 |
| `spend.monthly_cents` | `USAGE_BUDGET_CENTS_MONTHLY` | 300 | 0 – 100 000 000 |

**Hard ceilings** (bold) hold over the setting *and* the env var: they
protect the Worker, not the bill. One cron invocation has a fixed wall and
the Workers subrequest limit (the venture keeps 45 of 50; a compile spends
8, 9 with Jev), and a compile — or a "Process now" request — has to finish inside it, so
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
  and **Process now**, and a triage line: how many sessions were skipped,
  summarised and compiled in full, and the tokens saved. Each session shows
  its compile status and links to the pages it wrote; a skipped or
  summarised one has **Compile fully**. Settings → Usage & limits has the
  token savers: triage on/off, compaction on/off, and the strictness. The transcript view shows the same above the session.
- **Mac app**: not yet. It is a follow-up.

## Lint

The lint pass is cheap and uses no model. It writes `coding-agents--lint`
with orphans (pages nothing links to), broken links to collection pages
that do not exist, and pages nothing has compiled into for 60 days.
Checking for contradictions needs a model and is **not built yet**.
