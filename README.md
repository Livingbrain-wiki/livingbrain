# Living Brain

A teammate in your Slack that writes its own company wiki and keeps improving it.
[livingbrain.wiki](https://livingbrain.wiki) · a [Factory Zero](https://factory0.ventures) venture

**Status: planned. Nothing here runs yet.** This repository holds the plan as issues and, as it lands, the code.

## What it will be
- **Remembers:** turns Slack conversations into Markdown pages (people, projects, decisions), and every fact links to its source.
- **Evolves:** nightly passes merge duplicates, surface contradictions, refresh stale facts. A learning layer models each person.
- **Acts:** tools over MCP. Bigger jobs go to a [Colonizer](https://colonizer.dev) colony that comes back with a pull request.
- **Connects to coding agents:** an MCP server for Claude Code, Codex, Cursor and OpenCode; opt-in session learning with secret redaction.
- **Yours:** the wiki exports as plain Markdown and opens in Obsidian. Bring your own model, or use the default.

## Pricing (planned, open core, per workspace)
- **Community: free.** Self-hosted on your own Cloudflare account and model key, up to 5 people, all core features.
- **Teams: $5/mo.** Unlimited people plus SSO, admin and audit log, shared prompt library, per-channel policies. Self-hosted with a license key, or hosted with your own model key.
- **Crew: $9/mo.** Hosted, model included, up to 25 people, team features included.

## Stack
Rust, built as a [Cratefield](https://github.com/Cratefield/harness) venture: one Cloudflare Worker with D1, R2, KV and a
Durable Object. Email through [Owlpost](https://owlpost.pages.dev). Design reference: supermemory's
[Company Brain](https://github.com/supermemoryai/company-brain) (Apache-2.0); the design is ported, not the code.

## Plan
Three epics, drafted in `docs/issues/` (edit `specs.py`, regenerate, then file with `file.py`, which paces writes):

| Spec | Epic |
| :--- | :--- |
| `1-foundation.json` | The brain in Slack: events, permissions, the agent loop, bring your own model (11 issues) |
| `2-living-wiki.json` | The living wiki: pages, citations, search, nightly evolution, learning layer, export (8 issues) |
| `3-agents.json` | Agents, Colonizer, Owlpost and the 3D brain, plus waitlist and hosted billing (10 issues) |

Issues labelled `needs-harness` are blocked on Cratefield changes (tool calling, an OpenAI-compatible adapter,
embeddings and a vector index).

## License
Open core. Everything in this repository is [Apache-2.0](LICENSE), except the `ee/` directory (Teams features: SSO,
admin and audit log, shared prompt library, per-channel policies), which will ship under a commercial license and
needs a Teams key to run. Community self-hosting (up to 5 people) is free. See [NOTICE](NOTICE) for attributions.
