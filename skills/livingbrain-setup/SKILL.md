---
name: livingbrain-setup
description: Set up, connect or onboard a team or company to Living Brain (livingbrain.wiki). Use when someone asks to set up Living Brain for a team, create or choose its workspace, invite members, connect GitHub organisations, sync Claude Code or Codex sessions with the Mac app, connect MCP for Claude Code, Codex or Claude connectors, or import notes, ChatGPT or Claude exports. Splits the work into steps the agent runs (CLI, MCP config, verification) and steps a human must click (sign-in, workspace, GitHub App install as owner or admin, Mac app install and project allowlist, invites).
---

# Set up Living Brain for a team

Living Brain is a team wiki that an LLM writes and keeps current from the sources
you connect: coding-agent sessions, GitHub repositories, Markdown notes and
chat exports. Agents read it over MCP. This runbook takes one company from
nothing to a connected workspace.

Product: `https://app.livingbrain.wiki` (web app and API).
MCP: `https://mcp.livingbrain.wiki/v1/pages/mcp`.
Facts here were checked against the code on `origin/main`; `reference.md`
lists the sources and the places where the docs and the code disagree.

Each step is marked **[AGENT]** (you run it) or **[HUMAN]** (a person clicks
it). You cannot sign in for someone, approve their browser page, or install
a GitHub App for them. Ask, and wait for the answer before a dependent step.

## Rules for every step

1. **One company = one workspace.** Never put two companies' code, sessions,
   notes or GitHub organisations in one workspace. A person can belong to
   several workspaces, so check which one is current before every sign-in,
   sync or import.
2. **Never handle a secret in chat.** Do not paste, print or echo a token. A
   CLI sign-in lives in the OS keychain. A bearer token belongs in the
   `LIVINGBRAIN_TOKEN` environment variable or the keychain, never in a
   message, a commit or shell history.
3. **No backfills by hand.** GitHub history and compiles run in the Worker
   cron. Do not script a GitHub fetch, a re-import loop or a manual compile.
4. **Nothing is deleted by an agent.** There are no delete tools. Pages are
   versioned and sources are append-only. Removing a member, a GitHub
   installation, a repository or a token is a human action in the web app.
5. **Shared versus personal.** A repository, import or page filed as `shared`
   serves the whole workspace. Personal scope serves only its writer. A
   team's company material goes to `shared`.
6. **Use production** (`app.livingbrain.wiki`) unless the user names staging.

---

## 0. Preflight [AGENT]

Ask the user for these. Do not guess them:

- The company name and the workspace name to use.
- The GitHub organisations and repositories to include, and whether the
  person setting up is an owner or admin of each organisation.
- The Mac users who will sync sessions, and the repositories they opt in.
- Any notes folder or chat export to import.
- The people to invite and their roles (section 9).

Check the local tools:

```sh
command -v livingbrain || echo "livingbrain CLI missing"
command -v claude || echo "Claude Code missing"
command -v codex || echo "Codex missing"
```

If the CLI is missing, go to section 2. Tell the user which route you chose.

**[HUMAN]** Confirm they can open `https://app.livingbrain.wiki` and sign in
with the team's email.

## 1. Choose the workspace [HUMAN, then AGENT check]

The header's workspace switcher (`#ws-switch`) lists the workspaces you belong to.

- If the company already has a workspace, select it. Do not create a second.
- If not, use **New workspace** in the switcher menu, or the **New workspace**
  field under **Your workspaces** in Settings → Account. Name it after the
  company. The creator becomes its owner.

Record the workspace's name and id. Section 3 checks them.

## 2. Install the CLI [AGENT, with a HUMAN choice]

The CLI is `livingbrain`. Two routes are documented:

- **Mac app (recommended on a Mac).** **[HUMAN]** downloads `LivingBrain.dmg`
  from `https://github.com/Livingbrain-wiki/livingbrain/releases` (tags
  `macos-v*`), checks it against the `.sha256` beside it, and drags the app to
  Applications. The app contains the CLI at
  `Living Brain.app/Contents/Resources/livingbrain`. Until the builds are
  notarised, macOS blocks the first open: right-click the app and choose Open.
- **From a checkout.** `cargo install --path crates/livingbrain-cli`
  (binary: `livingbrain`).

**[AGENT]** Check it: `livingbrain --version && livingbrain --help`.

## 3. Sign the CLI in to the right workspace [AGENT starts, HUMAN approves]

**[AGENT]** Start the device login. It prints a URL and a short code, then waits:

```sh
livingbrain login
```

**[HUMAN]** Open the URL, sign in if asked, and enter the code. The approval
page names the workspace the token is issued for. If it is the wrong one, use
the page's workspace picker, then approve. The token goes to the OS keychain.
Nothing is printed.

**[AGENT]** Confirm the CLI is signed in. The CLI does not expose its token, so
check with a command that needs one:

```sh
livingbrain --json sources --limit 1 >/dev/null && echo "CLI signed in"
```

A non-zero exit with an authorisation error means it is not signed in, or the
token was revoked. Run `livingbrain login` again.

To see which workspace a bearer token belongs to, use `GET /v1/me`. It returns
the workspace and the principal, never the secret:

```sh
curl -fsS -H "Authorization: Bearer ${LIVINGBRAIN_TOKEN}" \
  https://app.livingbrain.wiki/v1/me \
  | jq '{workspace: .workspace.name, workspace_id: .workspace.id, who: .principal.name, role: .principal.role, credential: .credential.kind}'
```

Compare `workspace.name` with section 1. If they differ, stop and sign in to
the correct workspace first. The CLI's sign-in is bound to one workspace, and
the Mac app's sync uses that sign-in, so sessions go where the CLI points.

## 4. Connect GitHub, per organisation [HUMAN installs, AGENT verifies]

**[HUMAN]** An owner or admin of the target workspace:

1. Opens **Settings** (`https://app.livingbrain.wiki/settings.html`) and scrolls
   to the **GitHub** section (`#github`). The page has no "Integrations"
   heading; this section is the GitHub connection.
2. Clicks **Install Living Brain on GitHub** (`#github-shared-install`).
3. On GitHub, picks the organisation or account and its repositories (read
   only), then installs.
4. Authorises Living Brain once when GitHub asks. This proves the installer is
   the person signed in here.
5. Back in Settings → GitHub, under **Repositories**, sets **Included** on each
   repository to read, and its scope to **Shared** for team repositories.
   Nothing is read until a repository is included.

Rules the product enforces:

- A GitHub organisation binds to exactly one workspace. A second workspace
  cannot take it (section 11).
- Only an owner or admin can connect or change GitHub.
- Each included repository files under its own name as a project.
- To add or remove repositories, or to disconnect, use **Manage on GitHub**.
  Do not do this by API.

**[AGENT]** After the human confirms, check that projects appear:

```sh
livingbrain --json sources --projects
```

Backfill runs in the cron and can take several cycles for a large repository.
An empty result right after install is not an error. Check again later. Do not
start a backfill yourself.

## 5. Sync Mac coding-agent sessions [HUMAN installs and allows, AGENT verifies]

The Mac app syncs Claude Code (`~/.claude/projects`) and Codex
(`~/.codex/sessions`) sessions, and only from repositories the user has
allowed. Secrets are redacted on the Mac before upload.

**[HUMAN]** On each developer's Mac:

1. Install the Mac app (section 2). In its window's **Workspace** section, use
   **Sign In with Browser** and approve for the team workspace, or **Use the
   CLI's Sign-in** if the CLI is already signed in to the right workspace.
2. Open **Projects & Sync** in the window's sidebar and add each repository
   with **Add Project…**. A new path uses detail level `full`, which includes
   tool inputs and outputs. For a sensitive repository, the terminal sets a
   lighter level: `livingbrain logs allow <path> --detail messages` (messages
   and tool names only) or `--detail summary` (each message cut to 2,000 characters).
3. Leave **Live Sync** on, or press **Sync Now** (⌘R).

**Several companies on one Mac (Mac app 0.1.4 or later).** Sessions go to the
default workspace (the CLI's sign-in) unless a repository is routed elsewhere:

- **[HUMAN]** Sign the Mac app in to each workspace (**Settings → Workspaces →
  Sign In**). In **Projects & Sync**, pick each repository's workspace
  (**Automatic**, **Default**, or a named workspace). Pinning a parent folder
  overrides every repository inside it, so pin the repository itself.
- **[AGENT]** The same from the terminal:
  `livingbrain logs allow <repo> --workspace <workspace-id>` or
  `livingbrain logs workspace <repo> <workspace-id|default|auto>`. Rules can
  route by git remote owner or folder name, in `~/.config/livingbrain/logs-allow.json`:
  `"routes": [{"remote_owner": "Acme-Org", "workspace": "<id>"}]` (the file
  becomes version 2; update every `livingbrain` CLI on that Mac).
- A session whose workspace can't be decided (no remote, unknown host, missing
  sign-in, keychain denied) is **held**, never sent to another workspace;
  `livingbrain logs status --json` shows why (`skipped_unroutable`,
  `skipped_no_token`, `skipped_keychain_denied`). Sessions already synced stay
  where they went.

**[AGENT]** Verify from the terminal. Nothing is sent:

```sh
cd /path/to/repo
livingbrain logs status
livingbrain --json logs sync --dry-run | jq '{scanned, uploaded, skipped, dry_run}'
```

`skipped` with `reason: "repo not opted in"` means the repository is not on
the allowlist. Ask the human to add it under **Projects & Sync**. Run
`livingbrain logs allow <path>` yourself only when the human has named the
path and the detail level. Then run `livingbrain logs sync` once. Do not loop.

A session counts only if its working directory is inside an allowed path. A
session started outside the repository is skipped on purpose.

## 6. Connect MCP for coding agents [AGENT, one HUMAN token step]

Every client uses one endpoint, `https://mcp.livingbrain.wiki/v1/pages/mcp`
(Streamable HTTP, POST, no trailing slash). Its tools are `brain_search`,
`brain_page`, `brain_context_for`, `brain_note` and `brain_facts`. Each read is
scoped to the person asking. A page they cannot read comes back as not found.

### 6a. Claude Code

Preferred: OAuth, with no token to store.

```sh
claude mcp add --transport http livingbrain https://mcp.livingbrain.wiki/v1/pages/mcp
```

Inside Claude Code, run `/mcp`, choose `livingbrain`, and authenticate in the
browser page. The human approves for the right workspace (section 3's rule).

Fallback for a headless agent, with a token: **[HUMAN]** creates one at
**Settings → API tokens** (`#tokens`), named after the agent. Pick **Shared
only** for bots and CI, or **Full read access** for a person's own agent. The
token is shown once. The human puts it in `LIVINGBRAIN_TOKEN` (shell profile or
secret store). **[AGENT]** then adds the server with a reference to the variable:

```sh
claude mcp add --transport http --scope user livingbrain \
  https://mcp.livingbrain.wiki/v1/pages/mcp \
  --header 'Authorization: Bearer ${LIVINGBRAIN_TOKEN}'
claude mcp get livingbrain
```

Check that `claude mcp get` shows the variable and not a literal token. If it
shows a literal token, remove the entry and tell the human the token reached a
config file. They must revoke it and create another.

### 6b. Codex

```sh
codex mcp add livingbrain --url https://mcp.livingbrain.wiki/v1/pages/mcp \
  --bearer-token-env-var LIVINGBRAIN_TOKEN
```

Codex reads `LIVINGBRAIN_TOKEN` from its environment, so no token is written to
its config.

### 6c. Claude on the web or Claude Desktop (connectors) [HUMAN]

1. Claude **Settings → Connectors → Add custom connector**.
2. Enter `https://mcp.livingbrain.wiki/v1/pages/mcp` and add it.
3. Sign in on the Living Brain page that opens. Pick the workspace from the
   menu if you belong to several, then **Approve**.

A connector reaches only the MCP tools. It appears in **Settings → API tokens**
as `Claude connector · <name>`. Revoking it there disconnects Claude at once.

## 7. Verify MCP [AGENT]

Start a new agent session, since tool lists load at start. Then call
`brain_search` with the repository name, or `brain_context_for` with the
repository and a task. Or from the terminal, using the CLI's sign-in:

```sh
livingbrain --json search --limit 3 "<repository name>"
```

An empty result on day one is normal. Section 10 gives the timing.

## 8. Imports [AGENT, with the human's folder path]

An import becomes a source, one per file or conversation. It goes to the
**personal** brain unless you pass `--shared`. For team material, pass `--shared`.

```sh
livingbrain import markdown ~/path/to/vault --shared --exclude 'drafts/**'
livingbrain import chatgpt ~/Downloads/export.zip     # check --help for --shared
livingbrain import claude ~/Downloads/export.zip
```

`import markdown` reads and redacts every file locally, prints a preview (file
count, bytes, skipped files, secrets to redact, target brain), then asks
`Upload N files to the shared brain? [y/N]`. **Only the human answers that
prompt.** Relay the preview. If they say no, nothing is sent.

Files over 1 MiB, and binary or non-Markdown files, are skipped with a reason.
The web **Imports** page (`wiki.html?view=imports`) takes `.md` and `.txt` files
up to 1 MiB each, for a human who prefers the browser.

Imports are compiled by the cron. Do not compile by hand.

## 9. Invite people and set roles [HUMAN]

**[HUMAN]** An owner or admin opens **Settings → Members** (`#members`), types
the email under **Invite**, picks a role, and sends.

| Role | Can |
| :--- | :--- |
| **Member** | Read and write the shared wiki. |
| **Admin** | Also invite, remove and re-role members, and connect GitHub. |
| **Owner** | Everything, including making other owners. |

Give the minimum role. The server refuses to remove or demote the last owner,
so name a second owner before you hand over.

Each new member signs in and makes their own CLI or token. Never share one
person's token.

## 10. Verify the setup [AGENT]

These are safe to repeat:

```sh
livingbrain --json sources --limit 3          # CLI signed in, data readable
livingbrain logs status                       # which repositories are opted in
livingbrain --json logs sync --dry-run | jq '{scanned, uploaded, skipped}'
livingbrain --json search --limit 3 "<repository name>"
```

Timing: a source is ingested as soon as it arrives. The wiki page for it is
compiled by the cron, which runs every 5 minutes. A page usually appears within
one or two cycles. Large GitHub backfills and big import batches take several
cycles. Say so to the human, and do not call it broken before then.

Report to the human: the workspace name, the GitHub organisations and their
included repositories, the projects with synced sessions, the members invited
and their roles, and what is still open.

## 11. Troubleshooting

| Symptom | Likely cause | Do this |
| :--- | :--- | :--- |
| `401` from the CLI, `/v1/me` or MCP | The token is for another workspace, revoked or expired. | Run `livingbrain login` again and pick the right workspace on the approval page. For an MCP token, create one in the target workspace and revoke the old one. For a connector, remove and re-add it. |
| `/v1/me` shows the wrong `workspace.name` | The credential belongs to another workspace. | As above. Do not continue until the name matches section 1. |
| GitHub: "That installation is already connected to another workspace." (`bound-elsewhere`) | Each GitHub organisation binds to one workspace. | An owner of the workspace that has it disconnects it under **Settings → GitHub → Manage on GitHub**. Or use a different organisation. Do not move it by script. |
| GitHub: "That installation was made on GitHub directly, so it is not connected." (`unclaimed`) | The app was installed on GitHub without starting here. | As an owner or admin, click **Install Living Brain on GitHub** in Settings → GitHub. |
| GitHub: "GitHub has not yet told Living Brain who installed the app." (`installer-unknown`) | GitHub has not yet sent the installer. | Wait a few seconds, then click **Finish connecting** (`#github-retry`). |
| GitHub: "Only the GitHub user who installed Living Brain on that account can connect it." (`not-installer`) | A different GitHub user installed it. | That person finishes in Settings, or installs it again themselves. |
| GitHub: "Only a workspace owner or admin can connect GitHub." (`forbidden`) | The person is a member. | An owner or admin does it. |
| GitHub installed, repository list empty | Nothing is included yet, or the first ingest has not run. | Include a repository in Settings → GitHub. Wait several cron cycles. |
| Nothing syncing from a Mac | The repo is not opted in, the CLI is signed in to another workspace, or the session started outside the repo. | `livingbrain logs status`. Check **Projects & Sync**. Check the CLI sign-in (section 3). |
| Sessions synced, no wiki pages | Compiling needs a model. Jobs show `skipped-no-model`. | Check **Settings → Models**. With nothing connected, the managed model answers. |
| Compile job `failed` | A provider error, for example a rate limit. | Failed jobs retry after 15 minutes, then after 1 hour, and stop after 3 attempts. A human uses **Retry failed** if the UI offers it, after fixing the cause. |
| `Upload N files? [y/N]` declined | The human said no. | Stop. Nothing was sent. Do not add `--yes` unless the human asks. |
| A second workspace is missing the sessions | Sessions go to the CLI token's workspace. | Sign the CLI in to that workspace before syncing. |

Commands, endpoints, screens and sources: [reference.md](reference.md).
