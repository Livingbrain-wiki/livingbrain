# Living Brain for macOS

A native menu bar app that runs `livingbrain logs sync` for you, so your
Claude Code and Codex sessions in opted-in repositories land in Living Brain
without opening a terminal — live, as each session goes quiet — plus a
workspace window for the wiki, Ask, synced sessions, usage and settings. SwiftUI,
macOS 14+, no third-party dependencies. It lives in the menu bar
(`LSUIElement`); opening the Living Brain window adds a Dock icon until the
window closes.

For syncing, the app is a thin shell over the CLI: every sync and opt-in is
a `livingbrain` subprocess. The CLI keeps deciding what may upload — a
session recorded outside an opted-in repository never leaves the machine,
and redaction still happens locally. The workspace window holds its own API
token in the keychain (see below); no token is ever logged or shown.

## The menu

- **Sync Now** (⌘R) — runs `livingbrain --json logs sync` and parses the report.
- **Last sync …** and a summary: uploaded · unchanged · skipped · notes written.
- An error line (the first line of the CLI's stderr) when a run failed.
- **Sign In…** — shown when the CLI says it is not logged in. Runs
  `livingbrain --json login`, opens the device-approval page in your browser,
  shows the code, and waits for approval (or **Cancel Sign-in**).
- **Synced Projects (n)** — the repositories in the CLI's opt-in list
  (`logs status --json`, falling back to `~/.config/livingbrain/logs-allow.json`),
  each with **Reveal in Finder** and **Remove** (`logs deny`).
- **Add Project…** — a folder picker, then `logs allow <path>`.
- **Open Living Brain** and **Agent Sessions** — open app.livingbrain.wiki.
- **Open Living Brain Window…** (⌘0) — the main window (below).
- **Pause Syncing** — stops live and scheduled sync; Sync Now still works.
- **Live Sync** — see below. On by default.
- **Sync Automatically Every 15 Min** — remembered across launches.
- **Notifications** — on upload and on error (see below). On by default.
- **Launch at Login** — via `SMAppService.mainApp`.
- **CLI** — shows which binary is used; **Choose CLI…** / **Use Default Location**.
- **Quit Living Brain** (⌘Q).

The icon is the `brain.head.profile` symbol with a small dot: none when idle,
a ring while syncing, a filled dot after an error.

## Live sync

The app watches `~/.claude/projects` and `~/.codex/sessions` with FSEvents.
An agent appends to its log on every turn, so when a session file has had
no change for two minutes the app runs
`livingbrain --json logs sync --session <file>…` for the files that went
quiet. The CLI still decides: a session outside an opted-in repository is
reported as skipped and nothing is sent. With a CLI that predates
`--session`, the app falls back to a full `logs sync`.

A session that resumes after going quiet is synced again when it next goes
quiet. Its note is keyed by the session id, so it is updated in place; the
ledger keys sources by content, so the longer record lands as a new source
next to the earlier one (exactly as a second manual `logs sync` would).

Notifications (`UNUserNotificationCenter`) are rate-limited: at most one
upload notification every 10 minutes (uploads in between are folded into
the next one), and the same error at most once every 30 minutes.

## The window

- **Recent Sessions** — every local session the CLI can read (a
  `logs sync --dry-run`, so nothing is sent): project, branch, agent, when,
  turns, tokens, and sync status (not opted in / pending / synced, from a
  small ledger of past syncs in
  `~/Library/Application Support/Living Brain/sessions-ledger.json`, which
  holds ids and times only). Selecting one shows the transcript of the
  **redacted record the CLI would send** (`livingbrain logs show <file>`),
  with tool names only. Actions: Open in Living Brain (the session's note,
  or the project's session list), Sync This Session, Reveal Log in Finder.
- **Projects & Sync** — every repository your sessions ran in, with an
  opt-in switch each (`logs allow` / `logs deny`), plus the pause, live-sync,
  schedule and notification switches.

### Workspace (signed in)

The window's **Workspace** section talks to the API directly over HTTPS
(`https://app.livingbrain.wiki` by default; change it under Settings) with
`Authorization: Bearer`. Sign in with **Sign In with Browser** (the same
device flow `livingbrain login` uses, client `livingbrain-cli`) or **Use the
CLI's Sign-in**, which copies the token the CLI keeps in the keychain
(service `wiki.livingbrain.cli`, account = the API URL; macOS asks first).
The app keeps its own copy under service `wiki.livingbrain.mac`. Tokens are
never logged or shown, and a 401 signs the app out.

- **Wiki** — the page list (`GET /v1/pages`), search with a project filter
  (`GET /v1/search`, ⌘K), pages rendered natively (headings, lists, quotes,
  code, `[[wikilinks]]` that navigate, backlinks), **Edit** (⌘E) and save with
  the version that was read (`PUT /v1/pages/{slug}` `base_version`); a 409
  offers *Load Their Version* or *Overwrite with Mine*. **New Note** (⌘N)
  posts `POST /v1/notes` with an optional project.
- **Ask** — `POST /v1/ask`, project-scoped, with numbered citations that
  open the cited page.
- **Agent Sessions** — the synced timeline per project
  (`GET /v1/sources/projects?kind=agent_log`, `GET /v1/sources?kind=agent_log`
  with paging), grouped by day; a session opens as its transcript
  (`GET /v1/sources/{id}`).
- **Usage** — `GET /v1/usage/daily?days=7|14|31` (workspace admins): totals,
  month-to-date spend against the budget, and Swift Charts of requests and
  tokens per day. Non-admins see why it is empty.
- **Settings** — server URL, sign in/out; models per role (`GET /v1/models`,
  `PUT`/`DELETE /v1/models/{role}`, *Discover* via `POST /v1/models/discover`,
  providers from the app's `providers.json`); API tokens (list, revoke).

What the API does not offer a bearer token, so the app links to the web
Settings instead: creating API tokens (`POST /v1/tokens` takes the web
session only, so a token can never mint a token), Slack connection status
(no endpoint reports it), and the workspace's name or switching workspaces
(`/v1/workspaces/me` is cookie-only and names one workspace). The current
workspace id is shown from the usage response when you are an admin.

Keyboard: ⌘K search, ⌘N new note, ⌘R sync, ⌘E edit, ⌘S save, ⌘1–⌘7 sections.

### Launch flags

- `--show-window` starts with the window open.
- `--destination wiki|ask|timeline|usage|sessions|projects|settings` picks the section.
- `--snapshot <file.png>` (with `--snapshot-delay <s>`) writes a PNG of the
  window, for docs and UI checks where screen capture is not permitted.
- `--api-fixtures <dir>` answers every API call from recorded JSON
  (`GET_v1_pages.json`, …) with no server or token; `--open-page <slug>`
  and `--select-first` pick something to show.

## Which CLI it runs

The first of these that is executable, unless you pick one under **CLI**:

1. `~/bin/livingbrain`
2. `/opt/homebrew/bin/livingbrain`
3. `/usr/local/bin/livingbrain`

The child gets your environment plus `LIVINGBRAIN_API_URL=https://app.livingbrain.wiki`
when that variable is not already set. To preview what a sync would send
without sending anything, run `livingbrain logs sync --dry-run` in a terminal.

## Build, run, install

Requires Xcode 15.3+ (Swift 5.10) on macOS 14 or later.

```sh
cd macos/LivingBrainBar
swift test                      # unit tests: CLI JSON, state machine, runner, debounce
scripts/build-app.sh            # → build/Living Brain.app
open "build/Living Brain.app"   # run it from the build folder
cp -R "build/Living Brain.app" /Applications/   # install
```

`scripts/build-app.sh` builds the release binary with SwiftPM, assembles the
bundle (`Info.plist` with `LSUIElement=1`, bundle id `wiki.livingbrain.bar`)
and signs it.

- **Signing:** ad-hoc (`codesign --sign -`) by default. Set
  `CODESIGN_IDENTITY="Developer ID Application: …"` to sign with a real
  identity (hardened runtime and a timestamp are added). An ad-hoc build
  runs on the machine that built it; to hand it to others, sign and notarize it.
- **Other knobs:** `UNIVERSAL=1` (arm64 + x86_64), `OUTPUT_DIR=…`,
  `APP_VERSION=…`, `BUILD_NUMBER=…`, `BUNDLE_ID=…`.
- **Launch at Login** registers whichever copy is running, so install to
  `/Applications` first and turn it on from there.

## Layout

```
Package.swift
Sources/LivingBrainKit/    # testable: CLI JSON models, SyncState, CLI locator/runner, opt-in file,
                           #   idle debouncer, notification throttle, FSEvents watcher, session ledger,
                           #   API client + models, keychain token store, Markdown blocks
Sources/LivingBrainBar/    # the SwiftUI menu bar extra, the window, and AppKit glue
Tests/LivingBrainKitTests/ # XCTest + recorded CLI JSON fixtures
scripts/build-app.sh
```

There is no macOS CI job: the repository's CI runs on Linux only. Run
`swift test` locally before sending a change here.
