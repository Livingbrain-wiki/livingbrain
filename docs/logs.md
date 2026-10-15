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
livingbrain logs allow          # opt this repository in
livingbrain logs sync           # read local logs, upload this repo's sessions
livingbrain logs sync --dry-run # report what would go, send nothing
livingbrain logs status
livingbrain logs deny
```

| command | what it does |
|---|---|
| `logs allow [path]` | allow sessions whose log records a working directory inside `path` (default: the current directory) |
| `logs deny [path]` | revoke a path `logs allow` added |
| `logs status` | print what is opted in, and where that is recorded |
| `logs sync` | read both agents' logs and upload the opted-in sessions |
| `logs show <file>` | print the redacted record `logs sync` would send for one session file, opted in or not; never touches the network |

| flag | what it does |
|---|---|
| `--claude-dir <dir>` | read Claude Code sessions from here instead of `~/.claude/projects` |
| `--codex-dir <dir>` | read Codex sessions from here instead of `~/.codex/sessions` |
| `--dry-run` | parse, normalise and report, but never touch the network |
| `--session <file>` | `logs sync` only: scan just this session file (repeatable). It must sit under one of the two trees, and its repo must still be opted in |

`--json` prints one object: `scanned`, `uploaded`, `unchanged`, `skipped`,
`unreadable` (a file that could not be read at all), `unparsed` (read, but no
session in it), `redacted`, `dry_run` and a `sessions` array of
`{agent, session_id, repo, models, turns, tool_calls, totals, truncated,
status, branch, started_at, ended_at, file, note?}` (`file` is the local path
of the log; it is printed, never sent) — the status one of `uploaded`, `unchanged`, `skipped`
(`reason: "repo not opted in"`) or `would_upload` under `--dry-run`. Every
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

The note is a compact summary of the session: agent, session id, repository,
branch, first and last timestamp, models, turn and tool-call counts (tool
names only), token and cost totals, the id of the source it summarises, and
the conversation's text excerpted — each turn cut to 400 characters, and the
excerpt to 12 KiB, with a line saying how many turns were left out. It is
built from the same home-elided record as the source and redacted on this
machine before it is sent; the notes route redacts again on arrival.

Notes are idempotent too. Each is keyed by agent and session —
`agent-log-<agent>-<session-id>` — so a session that grew updates its one
page in place, and a re-sync of an unchanged session sends the same note,
which the server recognises and does not write (`status: "unchanged"`). A
note the server refuses — say, because someone edited that page by hand —
is reported as `failed` with the reason; the session's source is in the
ledger regardless, and the next sync tries the note again.

## Opt-in is per repository

The opt-in is a small JSON file in the CLI's config directory
(`$XDG_CONFIG_HOME/livingbrain/logs-allow.json`, else
`~/.config/livingbrain/logs-allow.json`), listing absolute paths. A session
counts as opted in when the working directory its log records sits **inside**
an allowed path — so allowing a repository's root covers its subdirectories,
which is what a monorepo needs and what running `logs allow` at the root
means. A path that does not exist on this machine is allowed all the same: a
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

## What a session becomes

Both formats normalise into one record, versioned with `schema_version` and a
per-collector `collector_version`: the agent, session id, repository, branch,
models, first and last timestamp, the turns (`role`, timestamp, text truncated
to 2 000 characters, tool calls), totals (tokens in/out, cache read/write,
reasoning, cost when the log reports one), and a `truncated` flag. Only tool
**names** travel: a tool call's input object and its result body are your
files and your commands, dropped at parse time — there is no field in the
record they could hide in.

Two formats, two rules worth knowing:

- **Claude Code** streams one assistant message as several JSONL lines that
  repeat the same `message.id`, the same usage and the same `costUSD`. All
  three are counted once per id — five blocks are one request.
- **Codex** writes `token_count` events whose `total_token_usage` is
  cumulative, so the **last** one is the session's totals; summing them would
  multiply every turn in. The `<user_instructions>` and
  `<environment_context>` blocks every session opens with are boilerplate,
  not conversation, and are skipped.

## Redaction happens on both sides

Redaction is local and not optional. Before anything serialises, the real home
prefix becomes `~` — in the recorded repository path and in every turn's text —
including Claude Code's dash-encoded project-dir form of directories under
`$HOME` (`-home-alice-…`), which shows up precisely in text that never wrote
`$HOME` out. The whole body then goes through `livingbrain-redact`, the same
library the server runs on every ingest path, and the redacted bytes are what
the socket sends. A session that grows past the body cap (three quarters of
redaction's own 1 MiB input cap) loses its trailing turns and says so with
`truncated`.

## Idempotency

Sessions land as `kind: "agent_log"` in the source ledger, filed as
`<agent> session <id>`, in your personal memory — there is no `--shared` for
logs. The ledger's key is `(scope, sha256 of the redacted body)`, and a record
serialises deterministically: struct order is wire order, and nothing in it is
read from the clock or the environment. The same session therefore always
yields the same bytes, and re-running `logs sync` reads rows back (`200`,
`unchanged`) instead of storing copies.

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

`?kind=&project=&limit=&cursor=`, bearer-authenticated exactly like
`GET /v1/sources/{id}`. `limit` defaults to 50 and is clamped to 100; a
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
