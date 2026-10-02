<p align="center">
  <img src="https://raw.githubusercontent.com/Livingbrain-wiki/.github/main/assets/org-banner.png" alt="livingbrain.wiki. A living brain for your crew." width="100%">
</p>

<p align="center">
  <b>A teammate in your Slack that writes its own company wiki, and keeps improving it.</b><br>
  Every fact links back to the message it came from. Plain Markdown you own.
</p>

<p align="center">
  <img src="https://img.shields.io/badge/STATUS-PLANNED-4BE3A9?style=flat-square&labelColor=0E1719" alt="Status: planned">
  <img src="https://img.shields.io/badge/LANGUAGE-RUST-E8EFF0?style=flat-square&labelColor=0E1719" alt="Language: Rust">
  <img src="https://img.shields.io/badge/RUNTIME-CLOUDFLARE%20WORKERS-E8EFF0?style=flat-square&labelColor=0E1719" alt="Runtime: Cloudflare Workers">
  <img src="https://img.shields.io/badge/HARNESS-CRATEFIELD-E8EFF0?style=flat-square&labelColor=0E1719" alt="Harness: Cratefield">
  <img src="https://img.shields.io/badge/EMAIL-OWLPOST-E8EFF0?style=flat-square&labelColor=0E1719" alt="Email: Owlpost">
  <img src="https://img.shields.io/badge/LICENSE-APACHE--2.0%20%2B%20EE-4BE3A9?style=flat-square&labelColor=0E1719" alt="License: Apache-2.0 core, ee/ commercial">
</p>

<p align="center">
  <a href="https://livingbrain.wiki">livingbrain.wiki</a>
  &nbsp;·&nbsp;
  <a href="https://github.com/Livingbrain-wiki/livingbrain/issues">The plan, as issues</a>
  &nbsp;·&nbsp;
  <a href="docs/issues/">Issue specs</a>
  &nbsp;·&nbsp;
  <a href="https://livingbrain.wiki/llms.txt">llms.txt</a>
  &nbsp;·&nbsp;
  <a href="https://factory0.ventures">Factory Zero</a>
</p>

> **Planned. Nothing here runs yet.** The plan is three epics and 29 issues in the
> [issue tracker](https://github.com/Livingbrain-wiki/livingbrain/issues); code lands issue by issue.
> Early access is a waitlist at [livingbrain.wiki](https://livingbrain.wiki).

---

## What it will be

| | |
| :--- | :--- |
| **Remembers** | Turns Slack conversations into Markdown pages for people, projects, decisions and customers. Every fact links to its source message. |
| **Evolves** | Nightly passes merge duplicates, surface contradictions and refresh stale facts. A learning layer models how each person works. |
| **Acts** | Tools over MCP. Bigger jobs go to a [Colonizer](https://colonizer.dev) colony that comes back with a pull request. |
| **Coding agents** | An MCP server for Claude Code, Codex, Cursor and OpenCode. Opt-in session learning, with secrets redacted first. |
| **Private by design** | It reads only with the asker's own Slack access: public channels, the channels you're in, your DMs. |
| **Yours** | The wiki exports as plain Markdown and opens in Obsidian. Bring your own model, or use the default. |

## How it will work

```mermaid
flowchart LR
  S["Slack<br/>threads, DMs"] --> B
  A["Coding agents<br/>over MCP"] <--> B
  M["Email<br/>via Owlpost"] --> B
  B["<b>Living Brain</b><br/>Rust Worker<br/>Cratefield"] --> W[("Markdown wiki<br/>pages + citations")]
  W --> E["Nightly evolve<br/>merge · reconcile · refresh"] --> W
  B --> C["Colonizer colony<br/>microVM → pull request"] --> W
  W --> G["3D graph<br/>web app"]
```

## Pricing (planned, open core, per workspace)

| Plan | Price | What you get |
| :--- | :--- | :--- |
| **Community** | Free | Self-hosted on your own Cloudflare account and model key, up to 5 people, all core features |
| **Teams** | $5/mo | Unlimited people, plus SSO, admin and audit log, shared prompt library, per-channel policies. Self-hosted with a license key, or hosted with your own model key |
| **Crew** | $9/mo | Hosted, model included, up to 25 people, team features included |

## Stack

Rust, built as a [Cratefield](https://github.com/Cratefield/harness) venture: one Cloudflare Worker with D1, R2, KV and a
Durable Object. Modules only see ports; adapters are the only vendor-aware code. Email goes through
[Owlpost](https://owlpost.pages.dev). The design reference is supermemory's
[Company Brain](https://github.com/supermemoryai/company-brain) (Apache-2.0); the design is ported, not the code.

## The plan

Three epics, drafted in [`docs/issues/`](docs/issues/). Edit `specs.py`, regenerate, then file with `file.py`, which
paces its writes.

| Epic | Spec | Scope |
| :--- | :--- | :--- |
| [#1](https://github.com/Livingbrain-wiki/livingbrain/issues/1) | `1-foundation.json` | The brain in Slack: events, permissions, the agent loop, bring your own model (11 issues) |
| [#13](https://github.com/Livingbrain-wiki/livingbrain/issues/13) | `2-living-wiki.json` | The living wiki: pages, citations, search, nightly evolution, learning layer, export (8 issues) |
| [#22](https://github.com/Livingbrain-wiki/livingbrain/issues/22) | `3-agents.json` | Agents, Colonizer, Owlpost and the 3D brain, plus waitlist and billing (10 issues) |

Issues labelled `needs-harness` are blocked on Cratefield changes: tool calling, an OpenAI-compatible adapter,
embeddings and a vector index.

**Get involved.** Each issue is written so one person or one coding agent can finish it in one pull request. Pick one
whose dependencies are closed.

## Repositories

| Repo | What it is |
| :--- | :--- |
| **livingbrain** | This repo: the product, open core. The plan as issues, and the code as it lands |
| **[website](https://github.com/Livingbrain-wiki/website)** | [livingbrain.wiki](https://livingbrain.wiki): static HTML, no build step, Cloudflare Pages |

## License

Open core. Everything in this repository is [Apache-2.0](LICENSE), except the `ee/` directory (Teams features: SSO,
admin and audit log, shared prompt library, per-channel policies), which will ship under a commercial license and
needs a Teams key to run. Community self-hosting (up to 5 people) is free. See [NOTICE](NOTICE) for attributions.

<p align="center">
  <br>
  <a href="https://livingbrain.wiki"><b>livingbrain.wiki</b></a>
  &nbsp;·&nbsp;
  a <a href="https://factory0.ventures">Factory Zero</a> venture
</p>

<p align="center">
  <sub>Listens. Writes. Evolves.</sub>
</p>
