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
| `failed` | retried after 15 min, then 1 h; after 3 attempts it stays failed |
| `skipped-no-model` | no model connected; checked again hourly, and at once by **Process now** |
| `over-budget` | the compile budget is spent; checked again when it resets |
| `superseded` | a newer body of the same session arrived before this one ran |
| `skipped` | not a session record, empty, or its source is gone |

A session that grew is re-synced as a new body. If an earlier body of it
was already compiled, the model is told so and adds only what is new.

Jobs are processed in two places:

- **Cron.** The Worker fires every 15 minutes (`*/15 * * * *` in
  `wrangler.toml`) and compiles `CODING_AGENTS_BATCH` jobs (default 1)
  across all workspaces, oldest first. It then runs the lint over the scopes
  it wrote to.
- **`POST /v1/coding-agents/process`** (`{"max": 1..3}`) compiles up to
  `max` of *your* jobs inside the request. Cookie callers must be on the
  same origin.

`GET /v1/coding-agents/queue?limit=&source=` returns your jobs, a count per
status, whether a model is connected, and the budget with what has been
spent. Both routes only return jobs in your own scopes.

## The model

The model is your workspace's own, connected under **Settings → Models**,
on the **Main** job slot. There is no managed fallback: with nothing
connected, jobs park as `skipped-no-model` and the status route reports
`model.connected: false`. Each call goes through
`livingbrain_models::invoke::chat`, which re-runs the SSRF guard, decrypts
the key for that one request only, and records the call's tokens in the
usage ledger.

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
- **Per-job cap.** `CODING_AGENTS_JOB_TOKENS` (default 80 000) caps one
  job's tokens, in and out. A transcript longer than
  `CODING_AGENTS_MAX_CHUNKS` × `CODING_AGENTS_CHUNK_CHARS` (4 × 32 000
  characters) is cut, and the log says so.
- **Per-workspace budget.** `CODING_AGENTS_DAILY_TOKENS` (default 400 000)
  and `CODING_AGENTS_MONTHLY_TOKENS` (default 4 000 000) are counted in
  `coding_agent_spend`. A workspace whose usage ledger is over
  `USAGE_BUDGET_CENTS_MONTHLY` also compiles nothing.
- **Scope.** A job carries its source's scope, and compiled pages are
  written there and nowhere else. Neither a client nor a model can name a
  scope.

## In the app

- **Wiki home**: the compiled pages are a `coding-agents/` folder (agent →
  project, plus `projects/` for the rollups) instead of rows under Recent.
  A compiled page's meta line shows its path.
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
