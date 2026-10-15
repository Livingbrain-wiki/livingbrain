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
- **Run Brain Jobs with My Subscription** — see [Brain jobs on this Mac](#brain-jobs-on-this-mac).
  While on, the lines under it say what the runner is doing ("Running a
  compile job with Claude Code…", "Last job done 2 min ago · 1,234 tokens",
  or the last error).
- **Brain Jobs Agent: …** — Claude Code or Codex, the model (Claude Code's
  `sonnet`/`opus`/`haiku`/`fable` aliases; Codex's `gpt-5.6-sol`/`-terra`/`-luna`,
  `gpt-5.5`, `gpt-5.4-mini`), and whether each agent is installed and signed in.
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

## Brain jobs on this Mac

Anthropic and OpenAI only allow a Claude Pro/Max or ChatGPT Plus/Pro
subscription inside their own clients, so Living Brain never holds those
credentials. Instead, **Run brain jobs with my subscription** (menu, or
Settings → Brain jobs on this Mac) lets your own Claude Code or Codex on this
Mac answer your brain's model jobs — compiling your synced sessions into the
coding-agents wiki, and your Ask questions:

- On: the app sends `PUT /v1/models/mine/main {kind: "local_runner", agent, model}`
  so the server hands *your* jobs to this device, and keeps one
  `livingbrain --json jobs run --agent <claude|codex>` child running. The CLI
  claims a job, runs the agent with tools off and a read-only sandbox (never a
  permission bypass), answers it, and prints one JSON event per line, which
  is all the app reads. It restarts the child with backoff (5 s doubling to
  5 min) if it exits.
- Off: the child stops and the app sends `DELETE /v1/models/mine/main`, so
  your jobs go back to the workspace's model instead of waiting for a device
  that is not answering.
- **Pause Syncing** also stops the child but keeps the route: jobs wait and
  run when you resume. Quitting stops the child the same way.
- `livingbrain --json jobs agents` says which agent is installed and signed in
  (no email, no org). The app never reads or sends the agents' own
  credentials, and never logs a prompt or an answer.

Changing the model here updates the server route; a model id that is not in
the list can be typed as a custom one in Settings. Headless machines run the
same thing with `livingbrain jobs run`.

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
  **redacted record the CLI would send** (`livingbrain logs show <file>`):
  messages, reasoning summaries, interruptions, and each tool call with its
  command or patch and its clipped output (at the repository's detail level;
  `full` for a repository not opted in). Actions: Open in Living Brain (the session's note,
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
(no endpoint reports it), and switching workspaces (a token belongs to one
workspace; switch in the web app and sign in here again). Settings →
Workspace shows the workspace and who the token is connected as, from the
bearer-readable `GET /v1/me`.

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

The one you pick under **CLI**, if it is executable. Otherwise the first of
these that is:

1. the copy inside the app, `Living Brain.app/Contents/Resources/livingbrain`
   (release builds from `scripts/release.sh` ship one; a plain
   `scripts/build-app.sh` build does not)
2. `~/bin/livingbrain`
3. `/opt/homebrew/bin/livingbrain`
4. `/usr/local/bin/livingbrain`

The child gets your environment plus `LIVINGBRAIN_API_URL=https://app.livingbrain.wiki`
when that variable is not already set. To preview what a sync would send
without sending anything, run `livingbrain logs sync --dry-run` in a terminal.

## Install from the DMG

Download `LivingBrain.dmg` from the
[releases](https://github.com/Livingbrain-wiki/livingbrain/releases) (tags
`macos-v*`), check it against the `.sha256` next to it
(`shasum -a 256 LivingBrain.dmg`), open it and drag **Living Brain** to
**Applications**. One universal build runs on Apple silicon and Intel, macOS 14
or later, and carries its own `livingbrain` CLI.

Until releases are notarized, macOS blocks the first open ("Apple could not
verify…"). Either:

- in Finder, right-click (or Control-click) **Living Brain** in Applications →
  **Open** → **Open**; or
- open it once, then **System Settings → Privacy & Security**, scroll to
  *"Living Brain" was blocked…* → **Open Anyway**, and confirm.

macOS remembers the choice; later opens are normal.

## Build a release

```sh
cd macos/LivingBrainBar
scripts/release.sh 0.1.0
# → build/release/Living Brain.app, LivingBrain.dmg, LivingBrain.dmg.sha256
```

`release.sh` builds the CLI for `aarch64-apple-darwin` and
`x86_64-apple-darwin` (adding the targets with rustup if needed), `lipo`s them
into one binary at `Contents/Resources/livingbrain`, builds the app universal
via `build-app.sh` with `CFBundleShortVersionString` = the version (and
`CFBundleVersion` = `BUILD_NUMBER`, default 1), signs the CLI and then the
bundle, and packs a DMG (volume "Living Brain", with an `/Applications` link)
plus its SHA-256.

- **Ad-hoc by default** — not notarized, so users see the first-open steps above.
- **Signed and notarized** — set `CODESIGN_IDENTITY="Developer ID Application: …"`
  (hardened runtime + timestamp, DMG signed too) and `NOTARY_PROFILE=<profile>`
  (a `xcrun notarytool store-credentials` keychain profile): the DMG is
  submitted with `notarytool --wait` and stapled.
- `CARGO_TARGET_DIR` and `OUTPUT_DIR` are honoured.

Publish under a `macos-v<version>` tag (`gh release create macos-v0.1.0 …`),
never a bare `v*` tag: those trigger the production deploy workflow.

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
                           #   API client + models, keychain token store, Markdown blocks,
                           #   the brain-jobs runner (jobs run/agents, events, activity)
Sources/LivingBrainBar/    # the SwiftUI menu bar extra, the window, and AppKit glue
Tests/LivingBrainKitTests/ # XCTest + recorded CLI JSON fixtures
scripts/build-app.sh
scripts/release.sh
```

There is no macOS CI job: the repository's CI runs on Linux only. Run
`swift test` locally before sending a change here.
