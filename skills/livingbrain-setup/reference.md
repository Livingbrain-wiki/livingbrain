# Living Brain setup: reference

Companion to [SKILL.md](SKILL.md). Everything here was read from
`Livingbrain-wiki/livingbrain` at `origin/main`. Where the docs and the code
disagree, the code is followed and the disagreement is listed in section 7.

## 1. Endpoints

| Use | Address |
| :--- | :--- |
| Web app, API, CLI default | `https://app.livingbrain.wiki` (`DEFAULT_API_URL` in `crates/livingbrain-cli/src/main.rs`) |
| MCP (Streamable HTTP, POST only, no trailing slash) | `https://mcp.livingbrain.wiki/v1/pages/mcp` (`docs/mcp.md`) |
| OAuth metadata | `https://mcp.livingbrain.wiki/.well-known/oauth-protected-resource`, and `/.well-known/oauth-authorization-server` |
| Current identity (bearer or session) | `GET /v1/me` on the API host. Answers `workspace`, `principal`, `credential` (kind and prefix, never the secret). Source: `crates/livingbrain-tokens/src/me.rs` |
| Import one file | `POST /v1/pages/sources` (`docs/import.md`) |
| Create a token (web session only) | `POST /v1/tokens`. A token cannot mint a token |
| Create a workspace | `POST /v1/workspaces`. The creator is the owner |
| GitHub shared-app install link | `POST /v1/github/shared/install` (`app/assets/github.js`) |
| Retry failed compile jobs | `POST /v1/coding-agents/retry` (`docs/coding-agents.md`) |

Staging exists at `staging-api.livingbrain.wiki`. Use production unless told.

## 2. CLI commands (`livingbrain`, crate `livingbrain-cli` 0.1.2)

Top level, from `crates/livingbrain-cli/src/main.rs`: `login`, `logout`, `ask`,
`search`, `note`, `page`, `export`, `mcp`, `import`, `sources`, `logs`,
`jobs`, `telemetry`, `about`. Global flags: `--json`, `--api-url`
(env `LIVINGBRAIN_API_URL`).

| Command | Notes |
| :--- | :--- |
| `livingbrain login` | RFC 8628 device flow. Prints `Open <url> and enter the code: <code>` to stderr, polls, then stores the token in the keychain. Re-running replaces the stored token. The approval page picks the workspace. |
| `livingbrain logout` | Forgets the keychain token. |
| `livingbrain search [--project P] [--limit N] <query…>` | Query is required. |
| `livingbrain sources [--kind K] [--project P] [--limit N] [--cursor C] [--projects]` | `--projects` lists projects instead. |
| `livingbrain import markdown <dir> [--shared] [--yes] [--exclude GLOB]… [--max-bytes N]` | Default scope is personal. Shows a preview, then `Upload N files to …? [y/N]`. `--yes` skips the prompt; do not use it without the human's say-so. |
| `livingbrain import chatgpt <export.zip>` and `import claude <export.zip>` | Check `--help` for the scope flag. The zip is read in memory. |
| `livingbrain logs allow [path] [--detail full\|messages\|summary]` | Default path is the current directory. A new path defaults to `full`. |
| `livingbrain logs deny [path]` | |
| `livingbrain logs status` | Opted-in paths and detail levels. |
| `livingbrain logs sync [--dry-run] [--session FILE]… [--claude-dir D] [--codex-dir D]` | Uploads opted-in sessions. `--dry-run` sends nothing. |
| `livingbrain logs show <file> [--json]` | Prints the redacted record that sync would send, offline. |
| `livingbrain mcp` | Stdio MCP server, for clients that want a local process. |

There is no `whoami` or `me` command. The CLI never prints its token or its
workspace. To learn the workspace behind a bearer token, call `GET /v1/me`
with that token. To learn which workspace the CLI's keychain token belongs to,
use the Mac app's Settings → Workspace after **Use the CLI's Sign-in**, or the
approval page when you ran `login`.

Environment: `LIVINGBRAIN_TOKEN` overrides the keychain for one invocation and is
never persisted (`crates/livingbrain-cli/src/auth.rs`).

Install: from the Mac app's bundle (`Living Brain.app/Contents/Resources/livingbrain`),
from a Homebrew path the Mac app also searches (`/opt/homebrew/bin/livingbrain`),
or from a checkout with `cargo install --path crates/livingbrain-cli`. No
Homebrew formula or hosted CLI download is documented.

## 3. MCP

Tools (`docs/mcp.md`, `crates/livingbrain-mcp`):

| Tool | Does |
| :--- | :--- |
| `brain_search(query?, limit?)` | Pages you may read holding every word of the query; with no query, the most recently updated. |
| `brain_page(slug)` | One page's Markdown. |
| `brain_context_for(repo, task)` | A cited brief of the pages that mention the task. |
| `brain_note(text, title?, facts?)` | Writes to your private memory, redacted first. |
| `brain_facts(subject?, attribute?, as_of?)` | Current or as-of facts, with history. |

Client commands:

- Claude Code: `claude mcp add --transport http livingbrain https://mcp.livingbrain.wiki/v1/pages/mcp`
  (no header, OAuth). Token variant: `--header "Authorization: Bearer <token>"`.
  Flags checked with `claude mcp add --help`: `--transport`, `--scope`,
  `-H/--header`, `--client-id`, `--callback-port`.
- Codex: `codex mcp add livingbrain --url <endpoint> --bearer-token-env-var LIVINGBRAIN_TOKEN`
  (flags checked with `codex mcp add --help`). Without the env flag, Codex
  sends no token.
- Cursor and OpenCode: `cursor mcp add livingbrain --url <endpoint>`,
  `opencode mcp add livingbrain --url <endpoint>` (from `docs/mcp.md`; not run here).
- Claude on the web or Desktop: Settings → Connectors → Add custom connector,
  then the browser approval page. The app is shown as **unverified** unless
  its address is recognised as Claude; an unverified app reaches only the MCP
  tools.

Tokens: **Settings → API tokens** (`app/settings.html#tokens`). Scope choices in
`app/assets/tokens.js`: **Full read access** (no scopes) and **Shared only**
(`shared`). The token is shown once. The database stores only its SHA-256.

## 4. Web screens and element ids

Settings page: `app/settings.html`. Sections in page order: Models (`#models`),
API tokens (`#tokens`), Workspace (`#workspace`), Members (`#members`), Usage &
limits (`#limits`), Account (`#account`, which holds **Your workspaces** and the
**New workspace** form), Agent accounts, Access, Audit log, Connect your coding
agent (`#connect`), **GitHub** (`#github`), Discord (`#discord`, hidden unless
Discord is configured).

| Element | Id |
| :--- | :--- |
| Workspace switcher | `#ws-switch`, `#ws-button`, `#ws-menu` |
| GitHub shared install button | `#github-shared-install` (label "Install Living Brain on GitHub") |
| GitHub retry | `#github-retry` ("Finish connecting") |
| GitHub installations list | `#github-installs` |
| GitHub repositories table | `#github-repos`, columns Repository, Project, Scope, Included, Backfill, Last webhook, Last error |
| Advanced: own GitHub App | `#github-advanced` (enterprise or self-hosted; not the default) |
| Member invite | `#invite-email`, `#invite-role`, `#send-invite` |
| Roles | Member, Admin, Owner (`app/assets/members.js`). Only an owner can make an owner. |
| Imports page | `wiki.html?view=imports` (`.md`/`.txt`, 1 MiB each) |
| Agent sessions | `wiki.html?view=sessions` |

Mac app (`macos/LivingBrainBar/README.md`): menu bar and window. Window
sections include **Projects & Sync** (per-repository opt-in, pause, live sync,
schedule, notifications), **Recent Sessions**, and **Settings → Workspace**
(**Sign In with Browser**, **Use the CLI's Sign-in**, **Add Workspace…**).

## 5. GitHub connection rules (`docs/github.md`, `app/assets/github.js`)

- Default app: the shared "Living Brain" GitHub App. An owner or admin installs
  it from Settings → GitHub. The install is confirmed through an OAuth hop that
  proves the installer.
- Read only: commits, issues, pull requests, reviews. Nothing is read until a
  repository is included. Subscribed webhook events are `push`, `issues`,
  `issue_comment`, `pull_request`, `pull_request_review`,
  `pull_request_review_comment`, `release`, `installation`,
  `installation_repositories`.
- Each organisation binds to one workspace. Error keys and their strings
  (`app/assets/github.js`):
  - `bound-elsewhere`: "That installation is already connected to another workspace."
  - `unclaimed`: "That installation was made on GitHub directly, so it is not connected. Use Install Living Brain on GitHub here, as an owner or admin."
  - `not-verified`: "GitHub did not confirm that your GitHub account can reach that installation…"
  - `not-installer`: "Only the GitHub user who installed Living Brain on that account can connect it…"
  - `installer-unknown`: "GitHub has not yet told Living Brain who installed the app. Wait a few seconds, then choose Finish connecting."
  - `forbidden`: "Only a workspace owner or admin can connect GitHub."
  - `not-confirmed`: "GitHub does not show that installation for this app…"
  - `refused`: "Living Brain refused that installation."
- Backfill and compile run only inside the Worker cron. Each tick has a budget
  (`GITHUB_TICK_BUDGET`, default 400 requests), so a large repository takes
  several ticks.
- Repositories file under their own project name. A session from the same
  repository lands beside its commits and pull requests.

## 6. Session sync and compiling (`docs/logs.md`, `docs/coding-agents.md`)

- Opt-in lives in `$XDG_CONFIG_HOME/livingbrain/logs-allow.json` (else
  `~/.config/livingbrain/logs-allow.json`). A session is opted in when its
  working directory sits inside an allowed path. A session that runs in more
  than one directory is sent at the most restrictive level among them.
- Notes are written to the personal scope, filed under the project named after
  the repository's directory.
- The compile queue is drained every 5 minutes by the cron. Statuses worth
  knowing: `queued`, `processing`, `done`, `failed` (3 attempts), `skipped-no-model`
  (no model connected; checked hourly), `over-budget`, `waiting-runner`,
  `skipped-trivial`, `summary-only`.
- Compiled pages live under `coding-agents/` and show under **Pages → Projects**.

## 7. Where the docs and the code disagree, or the brief did not match

1. **OAuth for connectors.** `docs/mcp.md` says the browser authorization-code
   flow with PKCE and dynamic registration is "not implemented yet" and that
   `claude mcp add` gets no zero-config OAuth. The code says otherwise:
   `crates/livingbrain-tokens/src/handlers.rs` advertises `authorization_code`,
   `refresh_token` and the device grant, with `code_challenge_methods_supported:
   ["S256"]`, and `crates/livingbrain-tokens/src/oauth/mod.rs` serves
   `/v1/oauth/authorize`, `/v1/oauth/register` and `/v1/oauth/token`. The skill
   follows the code. The stale doc line should be fixed separately.
2. **"Settings → Integrations".** The settings page has no "Integrations"
   heading. GitHub is the `#github` section, and the JS comments call it
   "Integrations". The skill says "the GitHub section".
3. **Product status.** `README.md` on `origin/main` says "Planned. Nothing is
   live yet" and "Nothing is deployed". The brief says the app is live at
   `app.livingbrain.wiki`, and `docs/mcp.md` describes the live endpoint. The
   skill follows the brief. The README's status badges are stale.
4. **Cron timing.** The brief says pages appear "within one cron cycle". That
   holds for a session or a small import. A large GitHub backfill spans several
   ticks, so the skill says "a cycle or two" and "several for large backfills".
5. **Per-repository workspace routing** shipped in PR #236 and Mac app 0.1.4
   (`logs allow --workspace`, `logs workspace`, `routes` in the version-2
   opt-in file; undecidable sessions are held). See `docs/logs.md`.
6. **Member role `viewer`.** `app/assets/members.js` labels a `viewer` role, but
   the invite form offers only Member, Admin and Owner. The skill lists only the
   three invitable roles.
7. **Default import scope is personal.** The CLI's `import` defaults to personal.
   Team material needs `--shared`. The skill says so.
8. **Discord and Slack** are not in this skill. Discord is hidden until the
   server says it is configured, and Slack sign-in is hidden until configured.
   The brief said not to instruct either unless the Integrations page shows it.
9. **Mac app sign-in labels.** `macos/LivingBrainBar/README.md` mentions a
   **Sign In…** menu item shown when the CLI is not signed in, and
   **Sign In with Browser** in the window's Workspace section. The skill names the
   window control.
10. **No CLI workspace readout.** The CLI does not print the workspace of its
    token. Checking it needs `GET /v1/me` with a bearer token, or a look at the
    Mac app. The skill checks sign-in with `livingbrain --json sources --limit 1`.

## 8. Source files read

- `docs/mcp.md`, `docs/logs.md`, `docs/import.md`, `docs/github.md`,
  `docs/coding-agents.md`, `README.md`, `macos/LivingBrainBar/README.md`
- `crates/livingbrain-cli/src/main.rs`, `src/auth.rs`, `src/import.rs`
- `crates/livingbrain-tokens/src/me.rs`, `src/handlers.rs`, `src/device.rs`,
  `src/oauth/mod.rs`
- `crates/livingbrain-workspaces/src/handlers.rs` (workspace creation as owner)
- `crates/livingbrain-workspaces/src/members.rs` (last-owner rule)
- `app/settings.html`, `app/assets/github.js`, `app/assets/members.js`,
  `app/assets/tokens.js`, `app/assets/imports.js`, `app/assets/device.js`,
  `app/assets/shell.js`, `app/assets/nav.js`
