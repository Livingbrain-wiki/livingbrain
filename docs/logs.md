# Agent logs

`livingbrain logs sync` reads the session logs your local coding agents
already wrote — Claude Code under `~/.claude/projects`, Codex under
`~/.codex/sessions` — and uploads the sessions whose repository you have
opted in, one normalised **source** per session. Nothing is read that you did
not already have on disk; nothing is sent that you did not allow.

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

| flag | what it does |
|---|---|
| `--claude-dir <dir>` | read Claude Code sessions from here instead of `~/.claude/projects` |
| `--codex-dir <dir>` | read Codex sessions from here instead of `~/.codex/sessions` |
| `--dry-run` | parse, normalise and report, but never touch the network |

`--json` prints one object: `scanned`, `uploaded`, `unchanged`, `skipped`,
`unreadable` (a file that could not be read at all), `unparsed` (read, but no
session in it), `redacted`, `dry_run` and a `sessions` array of
`{agent, session_id, repo, models, turns, tool_calls, totals, truncated,
status}` — the status one of `uploaded`, `unchanged`, `skipped`
(`reason: "repo not opted in"`) or `would_upload` under `--dry-run`.

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
