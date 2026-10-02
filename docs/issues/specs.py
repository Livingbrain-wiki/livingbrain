#!/usr/bin/env python3
"""Writes the three epic specs (1-foundation.json, 2-living-wiki.json, 3-agents.json) that file.py files.
Edit here, run `python3 specs.py`, then file one spec at a time: `python3 file.py 1-foundation.json`."""
import json, pathlib

REPO = "Livingbrain-wiki/livingbrain"
LABELS = [
    ["epic", "5319e7", "Parent issue with sub-issues"],
    ["backend", "1d76db", "The Living Brain service (Rust, Cratefield venture)"],
    ["slack", "4a154b", "Slack app, events, replies"],
    ["wiki", "0e8a16", "The Markdown wiki, citations, evolution"],
    ["learning", "c5def5", "Per-person modeling and the learning layer"],
    ["agents", "fbca04", "Coding agents, MCP, Colonizer"],
    ["ui", "bfdadc", "Web app and the 3D graph"],
    ["email", "d93f0b", "Owlpost: digests and the brain's inbox"],
    ["security", "b60205", "Permissions, redaction, secrets"],
    ["needs-harness", "d4c5f9", "Blocked on a Cratefield harness change"],
]

STACK = (
    "**Stack.** A Rust Cargo workspace consumed as a Cratefield venture (`cratefield-core`, "
    "`cratefield-runtime-cloudflare`, modules from the harness repo by git until the 0.5 release round, "
    "[Cratefield/harness#464](https://github.com/Cratefield/harness/issues/464)), deployed as one Cloudflare Worker with D1, "
    "R2, KV and a venture-owned Durable Object, the same shape as `Owlpost-to/backend`. Modules only see ports "
    "(`Database`, `TextModel`, `Mailer`, `Realtime`, `Defer`, `Blob`, `KeyValue`, `Signer`, `Payments`); adapters are "
    "the only vendor-aware code. Email goes through **Owlpost** (Resend-compatible API, inbound agent inboxes)."
)
REF = (
    "**Reference design.** supermemory's open-sourced Company Brain "
    "([supermemoryai/company-brain](https://github.com/supermemoryai/company-brain), Apache-2.0, TypeScript on Workers). "
    "Port the design, not the code; where an idea is taken nearly verbatim, credit it in `NOTICE`."
)


def spec(epic, children):
    return {"repo": REPO, "labels_create": LABELS, "epic": epic, "children": children}


def child(key, title, labels, deps, body, ac):
    dep = "Depends on " + ", ".join("{{%s}}" % d for d in deps) + "." if deps else "No dependencies inside the epic."
    acc = "\n".join(f"- [ ] {a}" for a in ac)
    return {"key": key, "title": title, "labels": labels,
            "body": f"Part of {{{{epic}}}}. {dep}\n\n{body}\n\n**Acceptance criteria**\n{acc}"}


# ---------------------------------------------------------------- Epic 1
e1 = spec(
    {"title": "Epic 1: The brain in Slack. A Cratefield venture that listens, answers with sources and acts",
     "labels": ["epic", "backend", "slack"],
     "body": (
        "Living Brain is a teammate in Slack that keeps its own company wiki and keeps improving it "
        "(see [livingbrain.wiki](https://livingbrain.wiki)). This epic builds the core: a Slack app on a Cratefield "
        "Worker that receives events, decides whether to speak, answers using only what the asker is allowed to see, "
        "cites its sources, and runs tools. No wiki yet (Epic 2) and no coding agents yet (Epic 3).\n\n"
        f"{STACK}\n\n{REF}\n\n"
        "**Models.** The default managed model is DeepSeek (Anthropic-compatible endpoint), with "
        "bring-your-own-model per workspace. The Cratefield `TextModel` port has no tools, streaming or embeddings in v1 "
        "(`core/src/ports/text_model.rs`), so the harness issues come first.\n\n"
        "**Children**\n{{harness_tools}} {{harness_openai}} {{scaffold}} {{tenancy}} {{slack_events}} {{conversation_do}} "
        "{{permissions}} {{agent_loop}} {{byok}} {{litellm}} {{mcp_tools}} {{deploy}}")},
    [
        child("harness_tools", "Harness: tool calling on the TextModel port", ["needs-harness", "backend"], [],
              "**Why.** The brain's answers chain tool calls (search memory, read a PR, open an issue). `TextModel` v1 completes "
              "one prompt with no tools.\n\n**Proposed approach** (to be filed upstream in Cratefield/harness, tracked here)\n"
              "- Extend `Prompt` with `tools: Vec<ToolSpec>` (name, description, JSON schema) and `Completion` with "
              "`tool_calls`; `Turn` gains tool-result content. Non-breaking, as the port docs already anticipate.\n"
              "- Implement in `adapter-anthropic` and `adapter-workers-ai`; adapters that cannot do tools return a typed "
              "`Unsupported` instead of silently dropping them.\n- A `run_tool_loop` helper with a max-step budget.",
              ["Anthropic adapter round-trips a two-step tool call in a test.",
               "An adapter without tool support fails loudly with a typed error.",
               "Step budget enforced; the loop cannot run unbounded."]),
        child("harness_openai", "Harness: adapter-openai-compatible (DeepSeek, OpenRouter, LiteLLM, self-hosted)", ["needs-harness", "backend"], ["harness_tools"],
              "**Why.** One adapter covers the default DeepSeek model, OpenRouter, LiteLLM gateways and customer "
              "self-hosted endpoints (vLLM, Ollama behind a tunnel).\n\n**Proposed approach**\n"
              "- `adapter-openai-compatible`: base URL, key, model id sent verbatim; Chat Completions with tools and JSON mode.\n"
              "- Optional Anthropic-messages mode for endpoints such as `api.deepseek.com/anthropic`.\n"
              "- Report provider token usage in `Completion` so cost can be metered.",
              ["Works against DeepSeek and OpenRouter in a recorded-fixture test.",
               "Model id is passed through unchanged; no silent remapping.",
               "Token usage populated whenever the provider returns it."]),
        child("scaffold", "Venture scaffold: workspace, Worker, D1, CI", ["backend"], [],
              "- Cargo workspace `crates/livingbrain-*` plus `crates/livingbrain-venture` (the Worker), generated with the "
              "Cratefield venture CLI.\n- D1 `livingbrain`, R2 `livingbrain-blobs`, KV, `wrangler.toml`, `worker-build`.\n"
              "- CI: fmt, clippy, tests, the Cratefield parity matrix (SQLite + Postgres) for every module.\n"
              "- `/__health`, `NOTICE` crediting Company Brain, README in the Factory Zero style (what is built vs planned).",
              ["`wrangler dev` serves `/__health`.", "CI green on an empty module.", "README states plainly that nothing is live yet."]),
        child("tenancy", "Workspaces, Slack sign-in and members", ["backend", "security"], ["scaffold"],
              "- One tenant per Slack workspace (`TenantDatabases` if a per-workspace D1 is warranted; decide and record in an ADR).\n"
              "- Sign in with Slack through `auth-oidc` (Slack supports OIDC). The first person to sign in owns the workspace.\n"
              "- Members table mirrors Slack users (id, name, timezone, is_admin) and is refreshed on `user_change`.",
              ["Sign in with Slack works end to end under `wrangler dev` with a tunnel.",
               "A user from workspace A can never read workspace B (test).", "Owner is set exactly once."]),
        child("slack_events", "Slack app: manifest, install, event intake with signature checks", ["slack", "backend", "security"], ["tenancy"],
              "- Slack app manifest generated with the deployment's URLs (as Company Brain's `/setup` does).\n"
              "- OAuth install per workspace; bot token encrypted with the harness `kms`/`secrets` crates, never logged.\n"
              "- `POST /slack/events`: verify `X-Slack-Signature` + timestamp (5 min window), ack within 3 s, hand work to "
              "`Defer`. Idempotent on `event_id`.\n- Handles `app_mention`, `message.channels`, `message.groups`, `message.im`, `member_joined_channel`.",
              ["A bad or stale signature is rejected (test).", "Retries with the same `event_id` are processed once.",
               "The 3 s ack holds even when the model is slow."]),
        child("conversation_do", "Per-conversation Durable Object for multiplayer concurrency", ["backend", "slack"], ["slack_events"],
              "Several people can talk in one thread at once. Company Brain solves this with one Durable Object per conversation.\n\n"
              "- A venture-owned `#[durable_object]` keyed by `team:channel:thread_ts` serialises turns, keeps a short working "
              "context and coalesces messages that arrive mid-turn.\n- Reuse the harness `Realtime` pattern (the DO class lives in the venture).",
              ["Two messages in the same thread never produce interleaved replies.",
               "A message arriving mid-turn is folded into the next turn, not dropped."]),
        child("permissions", "Read-with-the-asker's-access permission model", ["security", "backend"], ["tenancy"],
              "The brain must never leak what the asker could not see.\n\n"
              "| Asked in | May draw on |\n|---|---|\n| Public channel | Shared memory |\n"
              "| Private channel | That channel's memory + shared |\n| DM | Personal memory + every private channel the asker is in + shared |\n\n"
              "- Every memory item carries a scope (`shared`, `channel:<id>`, `user:<id>`). All reads go through one "
              "`scopes_for(asker, location)` function; nothing queries memory without it.\n"
              "- Writes to external tools run under the asker's own connection; borrowing a teammate's connection needs their "
              "approve/deny card.",
              ["Property test: no answer cites an item outside `scopes_for`.",
               "Leaving a private channel removes its memory from that user's DMs within one sync."]),
        child("agent_loop", "The turn: triage, answer with citations, speak-up policy", ["backend", "slack"], ["harness_tools", "conversation_do", "permissions"],
              "- **Triage** on a cheap tier (`ModelTier`): reply, react, or stay silent. Proactivity level per org and per channel.\n"
              "- **Answer** on the main tier with the tool loop; every factual claim carries a citation chip linking to the "
              "Slack permalink (later: the wiki page).\n- Model profile per role (triage / main / research) stored per workspace.",
              ["With proactivity off, the brain only answers when mentioned.",
               "Every answer that states a fact has at least one citation, or says it does not know.",
               "Triage cost per message is recorded."]),
        child("byok", "Bring your own model, with a capability check", ["backend", "security"], ["harness_openai", "tenancy"],
              "- Per workspace and per role: provider key or custom OpenAI-compatible endpoint (base URL + key + model).\n"
              "- On connect, probe: tool calling, JSON output, context size. Show \"works\" or what is missing.\n"
              "- Custom URLs: HTTPS only; reject private, loopback and link-local addresses (SSRF).\n"
              "- No silent fallback to the managed model; fallback is an explicit opt-in.\n- Keys encrypted, shown as last 4 only.",
              ["A URL resolving to 10.0.0.0/8 or 169.254.0.0/16 is refused.",
               "A model without tool calling is marked \"answers only\".", "Keys never appear in logs or API responses."]),
        child("litellm", "LiteLLM gateway as a first-class model provider (BYOK and hosted virtual keys)", ["backend", "security"], ["harness_openai", "byok"],
              "Filed directly as #42; see the issue for the full body. Customer LiteLLM gateway as a connection type (model list + capability probe), hosted plans on our LiteLLM with one budgeted virtual key per workspace, spend sync, SSRF rules.",
              ["A hosted workspace that exhausts its virtual-key budget is paused, never billed over."]),
        child("mcp_tools", "Tools over MCP with per-user connections", ["backend", "agents", "security"], ["agent_loop"],
              "- Remote MCP servers (GitHub, Linear, Notion, Google) connected per user via OAuth; tokens encrypted.\n"
              "- Per-connection and per-workspace `disabled_tools` (the same pattern HarnessRouter uses).\n"
              "- Approval cards in Slack for actions that write.",
              ["A write action never runs without the asker's connection or an approved borrow.",
               "A disabled tool is never offered to the model."]),
        child("deploy", "Deploy to api.livingbrain.wiki and an internal pilot", ["backend"], ["agent_loop", "byok"],
              "- Production Worker on `api.livingbrain.wiki`; secrets via `wrangler secret`.\n"
              "- Pilot in one internal Slack workspace for two weeks; record cost per workspace per day (model, D1, email).",
              ["The brain answers a question in the pilot workspace with a citation.",
               "Daily cost per workspace is visible and below the $9/month plan's budget."]),
    ])

# ---------------------------------------------------------------- Epic 2
e2 = spec(
    {"title": "Epic 2: The living wiki. Markdown pages with sources that the brain keeps improving",
     "labels": ["epic", "wiki", "learning"],
     "body": (
        "The memory is not a black-box vector store: it is a Markdown wiki the customer can read, edit and export "
        "(Karpathy's LLM-wiki pattern: raw sources, compiled pages, an index, and lint passes). The brain writes it from "
        "conversations, links every fact to its source, and on a schedule merges, reconciles and refreshes it. A learning "
        "layer models each person so answers and briefs improve every week.\n\n"
        f"{STACK}\n\n"
        "**Learning layer and licensing.** Honcho's server is AGPL-3.0; its SDKs are Apache-2.0. Start on the hosted "
        "`api.honcho.dev` through the SDK, never embed or modify the server, and rebuild the parts we keep as a Cratefield "
        "module before scale.\n\n"
        "**Children**\n{{harness_embed}} {{pages}} {{extract}} {{citations}} {{search}} {{evolve}} {{learning}} {{export}}")},
    [
        child("harness_embed", "Harness: Embeddings and VectorIndex ports (Workers AI + Vectorize, pgvector natively)", ["needs-harness", "backend"], [],
              "- `Embedder` port (batch embed text → `Vec<f32>`), adapters: `adapter-workers-ai`, `adapter-openai-compatible`.\n"
              "- `VectorIndex` port (upsert, delete, query with metadata filter), adapters: Cloudflare Vectorize; pgvector on `runtime-native`.\n"
              "- Metadata filters must support the scope field so permission filtering happens inside the query.",
              ["Parity test: same results shape on Vectorize and pgvector.", "Scope filter applied in the index query, not after."]),
        child("pages", "Page store: entities, Markdown, links, history", ["wiki", "backend"], [],
              "- Page = entity (person, project, decision, customer, system, glossary) with Markdown body in R2 and metadata in D1: "
              "slug, type, scope, `[[backlinks]]`, version history.\n- Typed frontmatter schema per entity type (as in chi-brain's `schemas/`).\n"
              "- Human edits are first-class and win over machine edits until reconciled.",
              ["Every page version is recoverable.", "A human edit is never silently overwritten by the brain."]),
        child("extract", "Listen → Write: turn conversations into page updates", ["wiki", "backend"], ["pages"],
              "- Batch new messages per channel (cron + `Defer`), extract facts, decisions, owners, and propose page diffs.\n"
              "- Each fact keeps its source: Slack permalink, author, timestamp, scope.\n- Skip noise (reactions, \"thanks\", bots).",
              ["A decision stated in a thread appears on the right project page with its permalink.",
               "Facts from a private channel never land on a shared page."]),
        child("citations", "Citations everywhere: fact → source, answer → page", ["wiki", "backend", "security"], ["extract"],
              "- Inline citation markers in page Markdown resolve to sources the reader is allowed to see; others render as "
              "\"restricted source\".\n- Answers in Slack cite wiki pages, and pages cite messages.",
              ["Clicking a citation as a user without access shows no content from it."]),
        child("search", "Hybrid search over pages and sources", ["wiki", "backend"], ["harness_embed", "pages"],
              "- Embeddings per page section + full-text (D1 FTS5); rank by both with recency.\n- Exposed as a tool to the agent loop and to MCP clients (Epic 3).",
              ["Search respects scopes (test with a private-channel fact).", "p95 latency recorded."]),
        child("evolve", "Evolve: nightly merge, reconcile, refresh (the wiki lints itself)", ["wiki", "learning"], ["extract", "search"],
              "Cron jobs (`Module::scheduled`):\n- **Merge** duplicate pages (same entity, different names).\n"
              "- **Reconcile** contradictions: open a \"needs a human\" card in Slack with both sources instead of guessing.\n"
              "- **Refresh** stale facts (age + newer conflicting mention), rewrite summaries, fix broken backlinks.\n"
              "- A changelog page per night: what the brain changed and why.",
              ["Every automated change is in the nightly changelog with a reason.",
               "Contradictions are surfaced, never silently resolved."]),
        child("learning", "Learning layer: per-person profiles and what works", ["learning", "backend"], ["extract"],
              "- Feed messages (and, in Epic 3, agent sessions) to Honcho via the Apache-2.0 SDK against `api.honcho.dev`; "
              "store returned peer representations per user and scope.\n- Use them to tailor answers and briefs (\"prefers small PRs\").\n"
              "- An ADR on replacing Honcho with an in-house Cratefield module (profiles + periodic \"dreaming\" passes).",
              ["Users can view and delete their own profile.", "No AGPL server code is embedded or modified."]),
        child("export", "Export and sync: Markdown zip and Obsidian-compatible vault", ["wiki"], ["pages"],
              "- One-click export of the whole wiki (respecting the exporter's scopes) as an Obsidian vault.\n- Optional one-way git sync to a customer repo.",
              ["An export opens in Obsidian with working backlinks.", "Export contains only what the exporter may see."]),
    ])

# ---------------------------------------------------------------- Epic 3
e3 = spec(
    {"title": "Epic 3: Agents, Colonizer, Owlpost and the 3D brain",
     "labels": ["epic", "agents", "ui", "email"],
     "body": (
        "Everything around the core: coding agents connect over MCP in both directions, Colonizer colonies do the bigger "
        "jobs, Owlpost sends digests and gives the brain an inbox, and the web app shows the brain as a live 3D graph. "
        "Also billing and the waitlist.\n\n"
        f"{STACK}\n\n"
        "**Children**\n{{waitlist}} {{mcp_server}} {{session_ingest}} {{prompt_library}} {{colonizer}} {{owlpost_digests}} "
        "{{owlpost_inbox}} {{web_app}} {{graph3d}} {{billing}}")},
    [
        child("waitlist", "Waitlist at api.livingbrain.wiki with Owlpost confirmation mail", ["backend", "email"], [],
              "- Cratefield `waitlist` module (as in `cratefield-backend`), product `livingbrain`, Turnstile captcha.\n"
              "- Double opt-in mail through Owlpost (`adapter-owlpost`, or `adapter-resend` pointed at Owlpost's Resend-compatible API until it lands).\n- Wire the website's forms to it.",
              ["Joining from livingbrain.wiki sends one confirmation email.", "CSV export behind the admin token."]),
        child("mcp_server", "MCP server for coding agents (Claude Code, Codex, Cursor, OpenCode)", ["agents", "backend", "security"], [],
              "- Remote MCP at `mcp.livingbrain.wiki`, OAuth per user, tools: `brain_search`, `brain_page`, `brain_context_for(repo, task)`, `brain_note`.\n"
              "- Same `scopes_for` permission model as Slack.\n- One-line connect docs per agent.",
              ["`claude mcp add` + a question returns a cited answer.", "An agent never sees pages outside its user's scopes."]),
        child("session_ingest", "Agent sessions → wiki (opt-in) with secret redaction", ["agents", "learning", "security"], ["mcp_server"],
              "- Opt-in per user: agent prompts, decisions and outcomes posted via `brain_note` or a session-upload endpoint.\n"
              "- Redact secrets before storage (key patterns, entropy check, `.env`-style lines); never store source code unless the workspace allows it.\n"
              "- Feeds the learning layer (Epic 2): which prompts lead to merged PRs, where agents get stuck.",
              ["A fake API key in a session is redacted before it reaches D1/R2 (test).", "Opt-out deletes that user's ingested sessions."]),
        child("prompt_library", "Team prompt library that learns what works", ["agents", "learning"], ["session_ingest"],
              "- Versioned prompts per workspace, suggested in Slack and over MCP.\n- Score by outcome (merged PR, rework, abandon) from session ingest and Colonizer results.",
              ["Prompts show usage and outcome counts.", "Scores only use outcomes the viewer may see."]),
        child("colonizer", "Colonizer: launch a colony from Slack, brief it, learn from it", ["agents", "backend"], ["mcp_server"],
              "- `@livingbrain fix #142` → `POST /api/sessions` on the configured Colonizer mothership "
              "(`{repo, issue, instructions, autopilot, model_tier}`, bearer token stored encrypted).\n"
              "- Instructions include `brain_context_for(repo, issue)`; the colony can call the MCP server for more.\n"
              "- Progress posted to the Slack thread; on finish, the PR and the colony's learnings (`pr.md`, dead ends) become wiki updates.",
              ["A colony started from Slack posts its PR link back to the thread.",
               "The colony never receives Slack or wiki credentials, only scoped MCP access."]),
        child("owlpost_digests", "Digests by email through Owlpost", ["email"], ["waitlist"],
              "- Scheduled daily/weekly digests per user or channel: what changed in the wiki, open contradictions, colonies finished.\n"
              "- Sent via Owlpost `POST /v1/emails`; one-click unsubscribe (RFC 8058) as Owlpost already enforces.",
              ["Unsubscribe works in one click.", "Digest only contains what the recipient may see."]),
        child("owlpost_inbox", "The brain's own inbox (Owlpost inbound)", ["email", "wiki", "security"], ["owlpost_digests"],
              "- `brain@<workspace>.livingbrain.wiki` via Owlpost agent inboxes; forwarded mail becomes a source for the wiki.\n"
              "- Owlpost's screening (DMARC/SPF+DKIM, lookalike domains, prompt-injection hold) runs before ingestion.",
              ["Mail held by screening never reaches the wiki until released.", "Forwarded mail appears as a cited source."]),
        child("web_app", "Web app: sign in with Slack, settings, wiki reader/editor", ["ui"], [],
              "- Static front end on Cloudflare (same no-build style as the venture sites), talking to the API.\n"
              "- Settings: models/BYOK, proactivity, tools, automations, skills, Colonizer connection.\n- Wiki reader and Markdown editor with backlinks and citations.",
              ["Works at 360 px wide.", "All settings changes are audited."]),
        child("graph3d", "The 3D brain: live graph of every entity", ["ui", "wiki"], ["web_app"],
              "- `3d-force-graph` (three.js) from a CDN: pages as nodes coloured by type, links as edges, colonies as orbiting capsules.\n"
              "- Click a node → side panel with its Markdown and sources. Live updates via the `Realtime` port.\n"
              "- 2D SVG fallback when WebGL is missing or reduced motion is set.",
              ["Smooth with 5,000 nodes on a mid-range laptop.", "Graph only shows nodes the viewer may see."]),
        child("billing", "Open-core plans, Teams license and hosted billing", ["backend"], ["web_app"],
              "- Plans (per workspace): Community free (self-hosted, up to 5 people, all core features); Teams $5/mo (unlimited people + SSO, admin/audit log, shared prompt library, per-channel policies; self-hosted license key or hosted BYOK); Crew $9/mo (hosted, model included, up to 25 people). No free hosted plan. Stripe via `adapter-stripe` / `control-plane-billing`.\n"
              "- Self-hosted license keys: signed offline-verifiable keys (harness `Signer`), checked at startup and daily; an expired key degrades to Community limits, never deletes data.\n"
              "- Hard usage caps per plan; overage pauses rather than bills by surprise.",
              ["A workspace over its cap is paused with a clear message, not charged.",
               "Model cost per workspace is visible to the owner.",
               "Community self-hosted never calls Stripe; over 5 people it asks for a Teams key instead of breaking.",
               "A lapsed license or subscription never deletes or locks the wiki export."]),
    ])

# ---------------------------------------------------------------- Epic 4
e4 = spec(
    {"title": "Epic 4: Everywhere via MCP and the CLI. One blazing-fast Rust binary",
     "labels": ["epic", "agents", "backend"],
     "body": (
        "Living Brain is not only a Slack bot. Outside Slack, every surface reaches the same brain through two doors: "
        "**MCP** (Claude Code, Codex, Cursor, OpenCode, Claude Desktop and any other MCP client) and the **`livingbrain` CLI**. "
        "Both are one Rust binary: a single static executable, instant startup, a local cache so search answers before the network does.\n\n"
        f"{STACK}\n\n"
        "**Rule.** MCP and the CLI are clients of the same API and the same `scopes_for` permission model. They never hold "
        "their own memory or their own agent loop; the local cache only ever holds pages the signed-in user may see.\n\n"
        "**Children**\n{{cli}} {{local_cache}} {{local_mcp}} {{claude_code}} {{agent_plugins}} {{perf}} {{pwa}} {{view3d}}")},
    [
        child("cli", "`livingbrain` CLI: one static Rust binary", ["agents", "backend"], [],
              "- Commands: `login` (device flow), `ask`, `search`, `note`, `page`, `export`, `mcp`.\n"
              "- Static binary per target (macOS arm64/x64, Linux musl x64/arm64, Windows) via cargo-dist; Homebrew tap, `curl | sh`, npm shim.\n"
              "- Pipes well: `git log -5 | livingbrain note --project api`; `--json` on every command for scripts and agents.\n"
              "- Tokens in the OS keychain, never a dotfile.",
              ["`livingbrain ask` returns a cited answer.", "Every command supports `--json`.", "No token is ever written to disk in plain text."]),
        child("local_cache", "Local cache: instant search, offline reads", ["agents", "wiki", "security"], ["cli"],
              "- Sync the pages the user may see into a local SQLite (FTS5) cache, incrementally via a change feed; encrypted at rest with a key from the OS keychain.\n"
              "- `search` and `page` answer from the cache first, then refresh; `ask` uses the cache as context and the server for the model.\n"
              "- Logging out or losing access purges the cache.",
              ["Search on a warm cache answers without a network call.", "Revoked access purges local pages on next sync (test)."]),
        child("local_mcp", "MCP from the same binary: stdio and streamable HTTP", ["agents", "backend"], ["cli", "local_cache"],
              "- `livingbrain mcp` serves the brain over stdio (and `--http` for clients that want streamable HTTP), backed by the local cache "
              "and the API. Same tools as the remote MCP server (Epic 3): `brain_search`, `brain_page`, `brain_context_for`, `brain_note`.\n"
              "- Works in any MCP client: Claude Code, Codex, Cursor, OpenCode, Claude Desktop.",
              ["The MCP Inspector lists and calls every tool.", "Tool results never include pages outside the user's scopes."]),
        child("claude_code", "Claude Code: first-class plugin (MCP + skills + slash commands + hooks)", ["agents", "learning", "security"], ["local_mcp"],
              "One install gives Claude Code the whole brain:\n"
              "- The `livingbrain mcp` server, preconfigured.\n- **Skills**: `brain-context` (pull conventions, owners and past decisions before "
              "coding), `brain-note` (record a decision with its reason).\n- **Slash commands**: `/brain ask`, `/brain note`, `/brain colony` (start a Colonizer colony).\n"
              "- **Hooks (opt-in)**: at session start, inject the brain's brief for this repo; at session end, send a redacted summary back "
              "(feeds the learning layer and the prompt library).\n- Packaged as a Claude Code plugin in the Agent Plugins layout (`plugin.json`, `mcp.json`, `skills/`).",
              ["`/brain ask` returns a cited answer.", "The session-end hook redacts a planted fake key before upload (test).", "Hooks are off until the user opts in."]),
        child("agent_plugins", "Same package for Codex, Cursor, OpenCode, Claude Desktop", ["agents"], ["claude_code"],
              "- Reuse the plugin's MCP config and skills; `livingbrain mcp install <agent>` writes the right config (Codex `config.toml`, Cursor `mcp.json`, OpenCode, Claude Desktop).\n"
              "- One docs page per agent with a one-line install.",
              ["Each listed client can call `brain_search` after `livingbrain mcp install`."]),
        child("perf", "Speed budget and benchmarks in CI", ["agents", "backend"], ["cli", "local_cache", "local_mcp"],
              "- Budgets: CLI cold start, warm-cache `search`, MCP `brain_search` round trip, binary size. Set them from the first measurements, then hold them.\n"
              "- A benchmark job in CI (criterion + hyperfine) that fails on regression; publish the numbers in the README only once measured.",
              ["CI fails when a budget regresses.", "No speed claim appears on the site before it is measured."]),
        child("pwa", "Easy to use: the web app as an installable, offline-first PWA", ["ui", "pwa"], ["local_cache"],
              "Filed directly as #40; see the issue for the full body. Manifest + service worker, offline reads of permitted pages, one-tap sign-in, one search-or-ask box, share target, web push.",
              ["Installs on iOS, Android and desktop.", "Logging out clears every cache."]),
        child("view3d", "`livingbrain view`: the 3D brain, locally, from the CLI", ["agents", "ui"], ["cli", "local_cache"],
              "Filed directly as #41; see the issue for the full body. Loopback-only local server with a one-time token, same renderer as the site, offline from the cache.",
              ["Binds to 127.0.0.1 only and rejects requests without the token."]),
    ])

here = pathlib.Path(__file__).parent
for name, s in [("1-foundation.json", e1), ("2-living-wiki.json", e2), ("3-agents.json", e3), ("4-everywhere.json", e4)]:
    (here / name).write_text(json.dumps(s, indent=1) + "\n")
    print(name, 1 + len(s["children"]), "issues")
