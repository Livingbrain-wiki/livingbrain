# Agent logs

`livingbrain logs sync` reads the session logs your local coding agents
already wrote — Claude Code under `~/.claude/projects`, Codex under
`~/.codex/sessions` — and uploads the sessions whose repository you have
opted in, one normalised **source** per session — and beside each one a short
**note**, a searchable page filed under the repository's name, so the session
turns up in `livingbrain search` and `livingbrain ask`. Nothing is read that
you did not already have on disk; nothing is sent that you did not allow.

## Use it

```
cd ~/work/demo
livingbrain logs allow          # opt this repository in (detail: full)
livingbrain logs allow --detail messages  # …or send messages only, no tool I/O
livingbrain logs sync           # read local logs, upload this repo's sessions
livingbrain logs sync --dry-run # report what would go, send nothing
livingbrain logs status
livingbrain logs deny
```

| command | what it does |
|---|---|
| `logs allow [path] [--detail full\|messages\|summary] [--workspace <id>]` | allow sessions whose log records a working directory inside `path` (default: the current directory), at a detail level (default `full` for a new path; an allowed path keeps its level unless `--detail` names one). `--workspace` sends them to that workspace; see [Workspaces](#workspaces-one-mac-several-workspaces) |
| `logs deny [path]` | revoke a path `logs allow` added |
| `logs workspace <path> <id\|default\|auto>` | send an allowed path's sessions to a workspace; `default` for the default workspace even when a route would pick another; `auto` to clear the choice so the routes decide. Refused for a path that is not opted in |
| `logs status` | print what is opted in, at which detail level, into which workspace, and the routes that send the rest |
| `logs sync` | read both agents' logs and upload the opted-in sessions |
| `logs show <file>` | print the redacted record `logs sync` would send for one session file, opted in or not; never touches the network. `--json` gives `{file, opted_in, redacted, body, record}`: `body` is the exact text, `record` its parse (`null` if redaction left it unparseable) |

| flag | what it does |
|---|---|
| `--claude-dir <dir>` | read Claude Code sessions from here instead of `~/.claude/projects` |
| `--codex-dir <dir>` | read Codex sessions from here instead of `~/.codex/sessions` |
| `--dry-run` | parse, normalise and report, but never touch the network (nor the local sync ledger) |
| `--resync` | `logs sync` only: ignore the local sync ledger and send every opted-in session again (see [The local sync ledger](#the-local-sync-ledger)) |
| `--session <file>` | `logs sync` only: scan just this session file (repeatable). It must sit under one of the two trees, and its repo must still be opted in |

`--json` prints one object: `scanned`, `uploaded`, `unchanged`, `skipped`,
`skipped_mixed_workspace`, `skipped_no_token` (see [Workspaces](#workspaces-one-mac-several-workspaces)),
`unreadable` (a file that could not be read at all), `unparsed` (read, but no
session in it), `redacted`, `dry_run` and a `sessions` array of
`{agent, session_id, repo, models, turns, tool_calls, totals, truncated,
detail, status, workspace, branch, started_at, ended_at, file, note?}` (`file` is the local path
of the log; it is printed, never sent) — the status one of `uploaded`, `unchanged`, `skipped`
(`reason: "repo not opted in"`), `skipped_no_token` (`reason: "not signed in to workspace <id>"`),
`skipped_mixed_workspace`, `skipped_unroutable`, `skipped_keychain_denied`, or `would_upload` under `--dry-run`. Every
opted-in session also carries `note`: `{slug, project, status, id?, url?,
error?}`, the status one of `written`, `unchanged`, `failed` or `would_write`
under `--dry-run` (`null` for a session that recorded no repository), and the
top level counts them in `notes`, e.g. `{"written": 2}`.

## Find a session again

A source in the ledger is not yet a page, and `search` and `ask` read pages.
So once a session's source is in the ledger — uploaded, or already there —
`logs sync` also writes it a note through `POST /v1/notes`, in your personal
scope, filed under the **project** named after the repository's directory
(`~/work/demo` files under `demo`). Filter on it:

```
livingbrain search --project demo "migration"
livingbrain ask --project demo "what did we decide about the migration?"
```

The project filter runs before the result limit, so `--project` answers with
up to `--limit` of that project's pages however many other pages match too.

The note is the session, readable: a header (agent, session id, repository,
branch, first and last timestamp, models, message and tool-call counts, token
and cost totals, detail level, the id of the source and a link to its full
transcript), then the conversation in order — every user and assistant
message, reasoning summaries quoted, events (an interruption, a compaction)
as a line, and each tool call as a compact block: its name, file and outcome
(`ok`/`error`, exit code), its command or patch in a fenced block, and its
result in another, clipped to head and tail.

A page must fit the notes route's 64 KiB request limit, so the note is held
to 48 KiB (measured as the JSON-escaped text it travels as). A long session
is first quoted less — each message, input and result clipped harder, down
to commands without results — and if it still does not fit, the page keeps
the opening turns and the most recent ones, with a line saying how many sit
between and linking the full transcript. It is built from the same
home-elided, redacted record as the source and redacted again on this
machine before it is sent; the notes route redacts again on arrival.

## Read the whole transcript

The web app's **Agent sessions** view (`wiki.html?view=sessions`) lists your
sessions, one row per session (a re-sync after an upgrade files a new source
beside the old one; the newest is the one listed). Each opens its transcript
at `wiki.html?view=sessions&source=<id>`, read from `GET /v1/sources/{id}` —
which returns the body to its owner under the same rules as the listing, by
bearer token or by the web app's session cookie — and drawn turn by turn:
messages in full (the agent's as markdown, the person's as typed), reasoning
summaries folded, tool calls with their commands, patches and results, long
results folded. Nothing is drawn with `innerHTML`: every node is built
element by element, so nothing in a transcript can become markup.

A subagent is a card inside the call that launched it — `🤖 name · model ·
duration · tokens · status`, its task, its final answer — indented on a rule,
with its other turns drawn only when **Show steps** is pressed (a subagent can
run hundreds). The session's header names its main model and its subagents,
and a turn names its model where it changed.

The Mac app's Recent Sessions view shows the same record, built locally by
`livingbrain logs show <file>`, subagents nested the same way.

Notes are idempotent too. Each is keyed by agent and session —
`agent-log-<agent>-<session-id>` — so a session that grew updates its one
page in place, and a re-sync of an unchanged session sends the same note,
which the server recognises and does not write (`status: "unchanged"`). A
note the server refuses — say, because someone edited that page by hand —
is reported as `failed` with the reason; the session's source is in the
ledger regardless, and the next sync tries the note again.

A note is a **session** page (entity type `session`), not a wiki page: it is a
source the coding-agents wiki is compiled from, so the web app lists it under
Sources → Agent sessions and keeps it out of Pages (search and `ask` still
find and cite it). Its title names the session the way a person would:
`EarthOS.world · Claude Code · 2026-10-15 · Fix the login redirect loop` —
the project, the agent, the day it started and the first thing asked, cut to
60 characters. A client that sends only an id-shaped title (`colonizer
session a15f…`) gets one the server reads from the note itself; for a
Colonizer colony that is the issue it worked on, else its pull request, else
its outcome (`EarthOS.world · Colony a15fba01 · #123 Fix auth redirect`).
Notes synced before this were decisions titled by id: the notes module's
cron moves them to `session` and renames them, a batch every 5 minutes, and
a session's next sync does the same for its own page.

## Workspaces: one Mac, several workspaces

A Mac can be signed in to several workspaces. By default every session goes to
the default workspace (the one `livingbrain login` signs in to). To send a
repository's sessions elsewhere, give its root a workspace, or write a route
that picks one by the repository's name or remote:

```sh
livingbrain logs allow ~/Documents/GitHub/chi-web --workspace <chi-workspace-id>
livingbrain logs workspace ~/Documents/GitHub/side-project default
livingbrain logs workspace ~/Documents/GitHub/chi-web auto   # back to the routes
livingbrain logs status            # each root's explicit choice and where it sends
```

The choices and the routes live in the same opt-in file:

```json
{
  "version": 2,
  "allowed": [
    "/Users/nick/Documents/GitHub/chi-web",
    "/Users/nick/Documents/GitHub/chi-backend",
    "/Users/nick/Documents/GitHub/side-project"
  ],
  "workspace": {
    "/Users/nick/Documents/GitHub/side-project": "default"
  },
  "routes": [
    {"remote_owner": "CHI-Ecosystem", "workspace": "<chi-workspace-id>"},
    {"folder_glob": "chi-*", "workspace": "<chi-workspace-id>"}
  ]
}
```

That is the example "CHI repos go to the CHI workspace". `side-project` names
`default` explicitly, so no route applies to it. Routes are edited in the file
(there is no command for them yet). `logs status` shows each route, each root's
explicit choice, where its sessions currently go, and why.

**Version.** A file that names a workspace or a route is written as `"version": 2`;
a file without routing stays `"version": 1`, and a version-1 file that names a
workspace or a route is refused. Two configured workspace ids that would share a
`LIVINGBRAIN_TOKEN_*` variable (`a-b` and `a_b`, say) are refused, and never saved. Every CLI that reads the file must be
at least this version: an older build refuses a version-2 file outright rather
than sending everything to the default. Unknown top-level keys are refused in
either version, so a misspelt `routes` is an error, not a silent default.

**Precedence.** For each session's working directory, in order:

1. the workspace of the nearest enclosing allowed root that names one (`default`
   counts, and stops the routes). A nested checkout follows its parent by
   lookup, not by copy: changing the parent's workspace moves every checkout
   inside it that names none of its own;
   after `logs deny <parent>`, a nested root that names none of its own falls
   back to routes or the default, again by lookup;
2. otherwise the **first** matching route, judged on that repository's own facts:
   its git top level, folder name (`folder_glob`, with `*` and `?`) and `origin`
   remote (`remote_owner`). `origin` is used; with no `origin`, the only remote.
   The URL is read as configured (`git config --get`), so an `insteadOf` rule
   cannot change the owner or host a route sees. The owner comparison is
   case-insensitive. A `remote_owner` route reads
   `github.com` unless it names a `host` (for GitHub Enterprise); a remote on any
   other host is not decided by that route;
3. otherwise the default workspace.

**Folders of checkouts.** `status` judges each allowed root as the sessions under
it will be judged. A root that is not itself a checkout, or that holds checkouts,
has no single answer: it reports `route_reason: "per-repository"`, lists each
checkout inside with its own workspace (`repos`), and warns that pinning the
folder would override every route inside it. An explicit choice on the folder
still covers everything inside, and is reported as such.

**Held, never defaulted.** When routes exist and a route needs a fact that cannot
be read, the session is **held**, not sent to the default. The facts that cannot be
read: the directory is not a git checkout (or no longer exists), git fails or
refuses the repository (its own safety checks are not weakened), the repository
has no remote or several remotes with no `origin`, or the remote is on a host
the route does not name (an ssh alias, a GitLab remote, another GHE host). Each
such session reports `skipped_unroutable` with the reason:

```json
"skipped_unroutable": 2
```

Git is asked read-only, with the caller's repository and config overrides
(`GIT_DIR`, `GIT_WORK_TREE`, `GIT_CONFIG`, `GIT_CONFIG_GLOBAL` and the like)
removed and a five-second limit per call; a call that times out is an unknown
fact. A repository is probed once per run, per working directory.

A session whose working directories resolve to different workspaces is never
sent: it is reported as `skipped_mixed_workspace`. A session with any held
directory is held as a whole.

**Tokens.** Each workspace is signed in separately. The CLI reads a routed
workspace's token from the same keychain item the Mac app writes for it: service
`wiki.livingbrain.mac`, account `<API URL>#<workspace id>` (the API URL without a
trailing slash). For tests and CI, `LIVINGBRAIN_TOKEN_<WORKSPACE>` (the id
upper-cased, other characters as `_`) takes its place, unless two configured
workspaces would share that variable name, in which case neither uses it. The
default workspace keeps using the CLI's own token, as before.

**Missing sign-in or keychain refusal.** A routed session whose workspace has no
token is held: not uploaded, never sent under another token, and not marked as
synced, so it goes up on the first sync after the sign-in. A workspace whose
keychain item cannot be read is held the same way, and only its own sessions:

```json
"skipped_no_token": [{"workspace": "<chi-workspace-id>", "sessions": 2}],
"skipped_keychain_denied": [{"workspace": "<other-id>", "sessions": 1}]
```

The Mac app shows these as "Sign in to <workspace> to sync 2 sessions", and as a
Keychain message for the denied case. Other workspaces keep syncing.

**Already synced.** The server's ledger is per workspace: every upload carries
its workspace's token, so a session that moves to another workspace is written
there once, and its earlier copy stays where it was. The Mac app's local record
of what it synced is keyed the same way, per (session, workspace).

## Opt-in is per repository

The opt-in is a small JSON file in the CLI's config directory
(`$XDG_CONFIG_HOME/livingbrain/logs-allow.json`, else
`~/.config/livingbrain/logs-allow.json`), listing absolute paths. A session
counts as opted in when the working directory its log records sits **inside**
an allowed path — so allowing a repository's root covers its subdirectories,
which is what a monorepo needs and what running `logs allow` at the root
means.

Each allowed path also has a **detail level**, kept beside it in the same
file (`"detail": {"/abs/path": "messages"}`); a path with no level — every
entry written before levels existed — is `full`:

| level | what a session's record carries |
|---|---|
| `full` | every user and assistant message in full, reasoning summaries, events, and each tool call with its input and its clipped result |
| `messages` | every message in full and each tool call's name — no tool input or output, no reasoning |
| `summary` | each message cut to 2 000 characters and tool names only — what every record carried before levels existed |

When a session's directories fall under several allowed paths, each
directory takes the level of the most specific path it sits in, and the
session is sent at the most restrictive of those. A level this build does not
know makes the file unreadable rather than guessed at; a build that predates
levels ignores them and sends `summary`. A path that does not exist on this machine is allowed all the same: a
checkout need not be mounted here to be opted in. A symlinked checkout
matches too: a directory counts when either its recorded form or the path it
really is sits inside an allowed root.

A session may run in more than one directory, and the decision covers all of
them: a session whose log shows a working directory outside every allowed
path is skipped, however opted-in its first repo was. Fail closed, on
purpose — half a session is still a leak.

A corrupt opt-in file, or one written by a newer version of the CLI, is an
error rather than an empty allowlist: silently reading it as empty would sync
exactly the sessions you meant to keep off the wire.

## Colonizer colonies

Colonizer runs each agent in a microVM, and the guest writes its transcripts
as `/workspace`, so the path a session records names no repository. The
colony's repository is in the Colonizer registry instead, so `logs sync` reads
it there: `~/.local/share/colonizer/sessions.json` (or `--colonizer-dir`, when
it exists) maps each colony id to `repo`, `Owner/name`.

A colony is judged as if it ran in the virtual root
`colonizer://<owner>/<name>`, and it is opted in with an entry of that form:

```
livingbrain logs allow colonizer://CHI-Ecosystem --workspace ws-chi
livingbrain logs allow colonizer://Colonizer-dev/tool --workspace ws-fz
```

An organisation's entry covers each repository in it, and names match
case-insensitively, as GitHub's do. A colony whose repository is not opted in is
not read at all. The project is the repository's basename, and the source's
`origin_ref` is the colony id. `--max-sessions N` uploads at most N new
colony sessions per run, oldest first; sessions already in the ledger do not
count, so repeated runs work through the backlog. Once the cap is met, the
remaining transcripts are not read or parsed at all: they are counted from
their metadata as `skipped_max_sessions` (`workspace: null`, since nothing
about them was read). `--dry-run` reports the
counts per workspace and project. A colony routes like any repository: it is
never sent to the default workspace on a route that cannot judge it.

## What a session becomes

Both formats normalise into one record, versioned with `schema_version` (3)
and a per-collector `collector_version` (3): the agent, the `detail` level it
was built at, session id, repository, branch, models, first and last
timestamp, the turns, totals (tokens in/out, cache read/write, reasoning, cost
when the log reports one — the main agent's own; each subagent carries its
own), `truncated`, `omitted_turns` and `omitted_subagent_turns`.

The turns are the conversation in order. Each has a `role`, a timestamp, its
`text` and its `tool_calls`:

- `user` — what the person typed, with the agents' own machinery taken out:
  Claude Code's `<system-reminder>` blocks, slash-command wrappers (a
  `/review src/lib.rs` stays, `<local-command-stdout>` goes), `!` shell
  output, task notifications and meta lines; Codex's `developer` messages,
  `<permissions instructions>`, `<environment_context>`, `<user_instructions>`,
  AGENTS.md dumps and the like. A user line that was only machinery is no turn.
- `assistant` — the agent's text, the `model` that wrote it (named only where
  it differs from the list's previous assistant turn, so a session on one
  model names it once), and the tool calls of that step, each
  `{name, format?, path?, input?, output?, status?, exit_code?, subagent?}`: `input` is
  the command (`exec_command`'s `cmd`, `Bash`'s `command`), the patch
  (`apply_patch`), the diff of an `Edit` under its `path`, or the arguments as
  compact JSON; `format` (`shell`, `diff`, `json`, `text`) says how to show
  it. `output` is the result — clipped to its first and last 2 KiB around a
  `… N bytes omitted …` line — with `status` (`ok`, `error`, `interrupted`)
  and `exit_code` when the log records them. An input is clipped the same way
  at 16 KiB.
- `reasoning` — a reasoning summary, when the log carries one in the clear
  (Codex's `summary`, Claude Code's visible thinking). Encrypted reasoning is
  never read.
- `event` — something that happened rather than something said: an
  interruption (Codex's `<turn_aborted>`, Claude Code's `[Request interrupted
  by user]`), a context compaction, or the turns the size cap removed.

A message is kept whole up to 256 KiB (beyond that, its first and last
128 KiB); what bounds a session is the body cap below. Under `messages` and
`summary`, the tool-call objects carry `name` alone.

A version-1 record (from a build before this one) reads the same way: its
tool calls are names, it has no `reasoning` or `event` turns, and its text was
cut to 2 000 characters. A version-2 record has no `subagent`, no turn
`model` and no `omitted_subagent_turns`; it reads the same way too.

### Subagents

A tool call that launched a subagent carries the subagent's whole session
in `subagent`, every field always present, in this order:

```json
{"name": "Agent", "format": "text", "input": "Find every logger in src/",
 "output": "Two loggers: src/a.rs and src/b.rs.", "status": "ok",
 "subagent": {
   "id": "a1111", "name": "Explore", "description": "Survey logging",
   "models": ["claude-haiku-4-5"],
   "started_at": "2026-10-02T10:00:06.500Z", "ended_at": "2026-10-02T10:01:30.000Z",
   "status": "completed",
   "totals": {"input_tokens": 180, "output_tokens": 45, "cache_read_tokens": 0,
              "cache_creation_tokens": 0, "reasoning_tokens": 0, "cost_usd": null},
   "turns": [ …the same turn shape, recursively: its own tool calls and subagents… ],
   "truncated": false, "omitted_turns": 0}}
```

`name` is the kind of agent (Claude Code's `subagent_type`, Codex's agent
role, Colonizer's subagent), `description` what the launch said it was for,
`status` one of `completed`, `failed`, `stopped`, `running` (or `null` when
the log does not say). The call's `output` is the subagent's last words, as
a foreground call's result already is — a background launch's receipt is
replaced by them. The detail level, home elision, redaction and the clips all
apply inside a subagent exactly as outside it.

- **Claude Code** writes each subagent's transcript beside the session:
  `~/.claude/projects/<dir>/<session>/subagents/agent-<id>.jsonl`, and its
  launch in `agent-<id>.meta.json` (`agentType`, `description`, the
  launching `toolUseId`, `parentAgentId` for one a subagent launched). The
  transcript nests in the call with that id — the session's own, or the
  parent subagent's — and the status is the last `<task-notification>` for
  that call (else the call's result). The subagent's working directories
  count for the opt-in like the session's own.
- **Codex** writes a spawned agent's rollout as a file of its own, whose
  `session_meta` names `source.subagent.thread_spawn` (`parent_thread_id`,
  `agent_path`, `agent_role`, `agent_nickname`). It nests in the
  `spawn_agent` call whose result names that path, and is no longer synced as
  a session of its own. Codex's own helpers (`source.subagent.other`) are
  still sessions of their own.

Two formats, two rules worth knowing:

- **Claude Code** streams one assistant message as several JSONL lines that
  repeat the same `message.id`, the same usage and the same `costUSD`. All
  three are counted once per id — five blocks are one request. A tool's
  result arrives on a later user line and is paired with its call by id.
- **Codex** writes `token_count` events whose `total_token_usage` is
  cumulative, so the **last** one is the session's totals; summing them would
  multiply every turn in. A call's output is paired with it by `call_id`.

## Redaction happens on both sides

Redaction is local and not optional. Before anything serialises, the real
home prefix becomes `~` — in the recorded repository path and in every string
a turn carries (text, tool paths, inputs, outputs) — including Claude Code's
dash-encoded project-dir form of directories under `$HOME` (`-home-alice-…`).
Every one of those strings then goes through `livingbrain-redact`, the same
library the server runs on every ingest path, **value by value, before the
record is serialised**: redacting the serialised JSON instead can let a match
straddle an escape (an address after a `\n` read as `nname@host`) and leave
bytes that no longer parse. The serialised body is checked once more as a
whole, the way the server will read it, and the redacted bytes are what the
socket sends. A secret in a tool's output is redacted like one in a message.

## Size limits

| what | limit |
|---|---|
| one tool result in the record | first and last 2 KiB |
| one tool input in the record | first and last 8 KiB (16 KiB) |
| one message in the record | 256 KiB (first and last 128 KiB beyond) |
| the record's body | 768 KiB (three quarters of redaction's 1 MiB input cap, which the server applies to the whole body on arrival) and 992 KiB once JSON-escaped (what `GET /v1/sources/{id}` answers it inside, under the edge's 1 MiB response buffer) |
| the note | 48 KiB JSON-escaped (the notes route takes 64 KiB) |

A session past the body cap loses its subagents' detail first: subagent by
subagent, oldest first, each loses its inner turns from the oldest on — never
its header or its final answer — until the body fits; the removed run becomes
one `event` turn in the subagent, which says so in its own `truncated` and
`omitted_turns`, and the record counts them in `omitted_subagent_turns`.
Still past the cap, the session keeps its opening turn and its most recent
ones: the oldest turns after the opening are removed whole, an `event` turn
says how many, `truncated` is `true` and `omitted_turns` counts them. One session is
one source either way, so the ledger's `(scope, sha256)` key still holds.

## Idempotency

Sessions land as `kind: "agent_log"` in the source ledger, filed as
`<agent> session <id>`, in your personal memory — there is no `--shared` for
logs. The ledger's key is `(scope, sha256 of the redacted body)`, and a record
serialises deterministically: struct order is wire order, and nothing in it is
read from the clock or the environment. The same session therefore always
yields the same bytes, and re-running `logs sync` reads rows back (`200`,
`unchanged`) instead of storing copies.

A session whose record changed — it grew, its repository's detail level
changed, or a CLI upgrade changed the schema — yields different bytes, so the
next sync uploads it as a new source (`201`) and rewrites its note in place
(same slug, a new version). The server's ledger is still the dedupe of record. The
local sync ledger (below) only spares the re-reading and re-sending of sessions
that have not changed since the server accepted them.

## The local sync ledger

`logs sync` keeps a record of what it has sent in `logs-synced.json`, beside
the opt-in file (`$XDG_CONFIG_HOME/livingbrain`, else `~/.config/livingbrain`).
An entry is written only after the server has the session's source and its
note (created or already there), and is keyed by the canonical file path and
the API URL. It holds the file's size and mtime, the level it was sent at, the
working directories it was opted in on, its workspace, and the routing (the
workspaces, routes and default workspace) it was sent under.

On the next run, a file whose size and mtime are unchanged, under the same API
URL and the same routing, is skipped **before it is read**: it is counted in
`unchanged` ("already in the ledger") and nothing is sent for it. A file that
changed, a new file, or any session under a changed route or API URL is read
and sent as before. A session whose repository was revoked is also skipped,
since nothing would be sent for it. `--resync` ignores the ledger for the run
and sends everything opted in again; the server answers what it already has
as `unchanged`.

The file is written atomically (a temp file, then a rename) and is private to
its owner on unix. A corrupt, unreadable or other-version file is treated as
empty and replaced on the next save: the sessions it held are sent again and
the server dedupes them. Deleting the file has the same effect. `--dry-run`
never writes it.

## Projects, and listing what was synced

Each upload also names a **project**: the basename of the session's
repository (`~/code/livingbrain` files as `livingbrain`), cut to the server's
project rule — ASCII letters, digits, `.`, `_` and `-`, starting with a letter
or digit, at most 64 bytes; any other character becomes `-`. A session that
recorded no repository names none. Only the basename travels in this field;
the full home-elided path is already inside the record. The server redacts the
project like any other free text and drops it, rather than storing it, if
redaction finds anything in it.

The project is a column of the ledger row, not part of the body, so it does
not change the `(scope, sha256)` key: a session synced before projects
existed is filed under its project the next time `logs sync` runs (`200`,
`unchanged` — the existing row gains the project; a row that already has one
keeps it).

List what is there, newest first, without opening a single body:

```sh
livingbrain sources --kind agent_log                 # every synced session
livingbrain sources --kind agent_log --project demo  # one repository's
livingbrain sources --projects                       # the projects, with counts
livingbrain sources --cursor <next_cursor>           # the next page
```

The same lists back the web app's **Agent sessions** view
(`wiki.html?view=sessions`) and the project filter beside the wiki's search
box. The listing is strictly yours: your personal sources, plus the
workspace's shared ones — never another member's, never another workspace's.

### `GET /v1/sources`

`?kind=&project=&agent=&limit=&cursor=`, bearer-authenticated exactly like
`GET /v1/sources/{id}`. `agent` narrows to the sessions one agent wrote
(`claude_code`, `codex`, `colonizer`: the first word of the session's
`path`). `limit` defaults to 50 and is clamped to 100; a
`kind` or `project` outside its rule is a `400 validation-failed`, not an
empty list. Rows are newest first (ULID order); `next_cursor` is the last
id of a full page, `null` on the last one.

```json
{
  "sources": [
    {
      "id": "01JA7Q8ZK3T5V9X2B4N6M8P0RC",
      "kind": "agent_log",
      "scope": "personal",
      "path": "claude-code session 3f2a9c1e-5b7d-4e8a-9c1f-2a4b6d8e0f1a",
      "project": "livingbrain",
      "held": false,
      "created_at": "2026-10-15T04:32:12.801Z"
    }
  ],
  "next_cursor": null
}
```

### `GET /v1/sources/projects`

`?kind=` optional. The distinct projects your sources are filed under,
alphabetically, with a count each — the names `GET /v1/sources?project=` can
find something under, and no others (at most 500).

```json
{ "projects": [ { "project": "demo", "sources": 2 }, { "project": "livingbrain", "sources": 14 } ] }
```
