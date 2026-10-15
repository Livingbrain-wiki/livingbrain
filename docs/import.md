# Import

`livingbrain import` brings data in from elsewhere, one **source** per file or
conversation. A source is a file that came in from outside, as opposed to a
page, which the brain wrote. Three formats today: a directory of notes
(`markdown`), and the chat vendors' own data exports (`chatgpt`, `claude`).

## Use it

```
livingbrain import markdown ~/vault
livingbrain import markdown ~/vault --exclude 'drafts/**' --shared
livingbrain import markdown ~/vault --max-bytes 262144 --json
```

| flag | what it does |
|---|---|
| `--shared` | import into the shared brain instead of your personal one (default `personal`) |
| `--yes` | upload without the confirmation prompt |
| `--exclude <glob>` | skip files matching a glob; repeatable, e.g. `--exclude 'drafts/**'` |
| `--max-bytes <n>` | skip files over this size (default and maximum 1 MiB, redaction's own cap) |

`--json` prints one object: `root`, `scope`, `files`, `created`, `unchanged`,
`redacted`, `skipped` (`not_markdown`, `too_large`, `binary`) and a `sources`
array of `{path, id, created}`. The preview always goes to stderr, so stdout
stays one object.

## Nothing leaves the machine before you say yes

The command walks the directory, reads and redacts every candidate **locally**,
prints a preview (file count, bytes, what was skipped, how many secrets are
about to be redacted, which brain the files would land in) and only then asks:

```
Upload 412 files to your personal brain? [y/N]
```

Only `y` or `yes` proceeds — anything else, and a closed stdin, is a no. The
token is read from the keychain and the socket opened *after* that answer, so
answering `n` leaves the machine exactly as it found it: a declined import
exits non-zero, uploads nothing, and makes no request at all. A run that finds
nothing to import is a successful no-op, not an error.

## What is uploaded, and what is skipped

The walk honours the vault's own `.gitignore` (whether or not it is a git
checkout) and skips dotfiles and dot-directories, which is what takes a vault's
own bookkeeping — `.obsidian/`, `.trash/` — out of the walk. Of what is left, a
file is a candidate only if it ends `.md`, `.markdown` or `.txt`; it is skipped
when it is over `--max-bytes`, or when its bytes are not UTF-8 text or contain a
NUL. The preview counts each of those reasons instead of swallowing them. What
goes up is one POST per file: the path **relative to the root**, `/`-separated,
and the body. A path that is not relative to the root is an error, never a
fallback that would put a local absolute path on the wire.

## Redaction happens on both sides

Redaction is local and not optional. `livingbrain-redact` — the same library the
server runs on every ingest path — runs over the body *and* over the relative
path before the socket opens, so a secret in a vault never crosses the wire; a
filename carries an API key just as well as a body does. The server redacts both
again on arrival, whatever the client did: the client does not hold that decision.

## Scope and idempotency

The request names `personal` or `shared`. The server folds that with the
workspace into a page scope and checks the result against the asker's own
grant: a caller cannot name a scope that is not theirs, because the string they
send is not the scope it becomes.

The ledger's primary key is `(scope, body_sha256)` — the SHA-256 of the
**redacted** body. The same document imported twice is one source; the second
import reads the first one's row back rather than sealing a second copy, so
re-running over an unchanged vault creates nothing new. The same body arriving
at two different paths is **one** source, stored under the path the first import
used — which is why the answer reports the **stored** path: a deduplicated
upload answers `200` with the path already on file, not the one this call sent.
A body is sealed under the scope's active data key at a blob key carrying its
own id, and the row records the key version that opens it, so a source written
before a rotation still reads after one and forgetting a scope makes every body
it sealed unreadable.

Each row also carries the `[[wiki-links]]` the file names, parsed leniently
Obsidian-style: `[[My Note]]`, `[[My Note|an alias]]`, `[[folder/Other#heading]]`
and `![[an embed]]` all count, and a target need not be a slug a page could
carry. Aliases and headings are stripped, duplicates dropped in first-seen
order, targets over 512 bytes ignored, the list capped at 256.

## The wire: `POST /v1/pages/sources`

The credential is the MCP endpoint's bearer token; a missing or rejected one
gets the same 401 and `WWW-Authenticate` challenge. `path` and `body` are
required, `scope` defaults to `personal`, `kind` to `import`; anything else in
the object is ignored. A path must be relative, at most 1024 bytes, and carry no
`..` segment, backslash or NUL.

```json
{"kind": "import", "path": "notes/one.md", "body": "# One", "scope": "personal"}
```

`201` when the row was created, `200` when the body was already there:

```json
{"id": "01J…", "kind": "import", "scope": "personal", "path": "notes/one.md",
 "sha256": "…", "wikilinks": ["Other Note"], "redacted": 1, "created": true,
 "created_at": "2026-10-08T12:00:00Z"}
```

Every refusal is an RFC 9457 problem document (`application/problem+json`) of
`type`, `title`, `status` and `detail`; the CLI prints the detail, so the reader
sees the server's sentence rather than the JSON.

| status | problem | when |
|---|---|---|
| `400` | `sources/bad-body` | not JSON, or `path`/`body` missing or not a string |
| `400` | `sources/invalid-path` | not a relative name inside a vault |
| `400` | `sources/unknown-scope` | a `scope` that is not `personal` or `shared` |
| `400` | `sources/unknown-kind` | a `kind` that is not `import` or `agent_log` |
| `401` | — | no bearer token, or one that names nobody |
| `403` | `sources/not-your-scope` | the folded scope is not in the asker's grant |
| `413` | `sources/too-large` | the body is over redaction's cap |
| `422` | `sources/not-redactable` | redaction refused the body; nothing was stored |
| `500` | — | a store failure; the detail stays server-side |

## Chat exports: `import chatgpt` and `import claude`

```
livingbrain import chatgpt ~/Downloads/conversations.zip
livingbrain import claude ~/Downloads/claude-export.zip --keyword migration
livingbrain import chatgpt conversations.json --since 2026-01-01
```

The argument is the vendor's export zip — read in memory, straight out of the
archive, never extracted to disk — or a bare `conversations.json`. Get the
exports from ChatGPT (Settings → Data controls → Export data; the download
link arrives by email) and Claude (Settings → Privacy → Export data).

| flag | what it does |
|---|---|
| `--shared` | import into the shared brain instead of your personal one (default `personal`) |
| `--yes` | upload without the confirmation prompt |
| `--since <date>` / `--until <date>` | keep conversations created on or after / before `YYYY-MM-DD` |
| `--keyword <text>` | keep conversations whose title or messages contain the text, case-insensitively; repeatable, and any match keeps a conversation |
| `--id <id>` | keep the conversation with this id; repeatable |

Both formats normalise into one conversation record. A branched chat — an
edited or regenerated answer — keeps only the live branch: ChatGPT's
`current_node` path up the `mapping` tree, Claude's branch whose leaf is the
latest message. The system prompt, messages hidden from the conversation and
abandoned siblings never leave the machine, and a parent chain that loops is
cut rather than walked forever. The parsers are versioned — ChatGPT's and
Claude's are both `1` today — and the number is recorded in every body as
`parser-version`.

Each conversation renders to one deterministic Markdown source at
`chatgpt/<conversation-id>` or `claude/<conversation-id>`: a header with the
title, source, parser version, conversation id and creation date, then one
section per message whose heading is its citation key —

```
## conversation:<conversation-id>#<message-id> (user · 2026-04-15T09:41:00Z · gpt-4o)
```

— which is the anchor any page generated from this source must cite.
Non-text parts (image pointers) and file attachments become `Attachments:`
references. A body that would pass the wire cap splits at a message boundary
into `…/part-2`, `…/part-3`, …, each repeating the header; a single message too
big for its section is truncated with a visible `[truncated]` marker.

The order of the work is `markdown`'s: parse, select, render, redact — all
locally — then the preview (the selected conversations oldest first, then the
per-class redaction summary, e.g. `redaction: email: 2, openai_key: 1`), then
the one `y/N` question, and only then the token and the socket; answering `n`
makes no request at all. `--json` prints one object: `source`, `export`,
`scope`, `conversations`, `files`, `created`, `unchanged`, `redacted`,
`redaction` (counts per class) and a `sources` array of `{path, id, created}`.
An unchanged export re-imports byte-identically, so the ledger's
`(scope, sha256)` key reads rows back: the second run creates nothing.

## Not built here

- Turning sources into pages — extraction is a later issue (#77). `wikilinks`
  are stored for the importer that will resolve them; nothing reads them yet.
- An import UI in the web app (`app/`). The app exists; it has no import surface,
  and the path to a source today is this CLI and this route.
- Listing, reading back or deleting sources over HTTP. The route answers `POST`
  only; `SourceStore::get`/`open` exist with no endpoint mounted over them.
- A dry run. The preview is a preview, not a run: answering `n` is the way to
  stop.