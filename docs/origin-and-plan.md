# Living Brain: where it came from and how we build it

This is the short record of why Living Brain exists, what was decided and when,
and how it is built. The plan itself lives in the
[issue tracker](https://github.com/Livingbrain-wiki/livingbrain/issues); this page
explains the shape of it. When two decisions disagree, **the later one wins**, and
the table below says which one that is.

All times are the founder's local time (WITA, UTC+8). Everything below happened on
2026-10-03 unless a row says otherwise.

## Origin

On 2026-09-26 supermemory open-sourced **Company Brain**
([supermemoryai/company-brain](https://github.com/supermemoryai/company-brain),
Apache-2.0), the Slack teammate they had run as a paid product and then
discontinued. It is TypeScript on Cloudflare Workers, Durable Objects and D1, with
memory on supermemory's paid API.

Early on 2026-10-03 the founder asked whether Factory Zero could offer the same
thing "in a Rust harness version", as cheaply as possible. The answer became a new
venture, **FZ-018 Living Brain**, built on what Factory Zero already has:

- **Cratefield** is the Rust harness on Cloudflare Workers. Living Brain is a
  Cratefield venture, not a port of Company Brain's code. We port the design and
  credit it in [`NOTICE`](../NOTICE).
- **Owlpost** handles email: confirmations, digests and the brain's own inbox.
- **Colonizer** does the real work in microVMs: "fix #142" comes back as a pull
  request.
- **A funded DeepSeek account** makes DeepSeek the default managed model.

Three ideas turned "a Slack bot with memory" into "a living brain":

1. **Karpathy's LLM wiki.** Memory is not a black-box vector store. It is a
   Markdown wiki the team can read, edit and export: raw sources, compiled pages,
   an index, and lint passes. Every fact links to its source.
2. **Honcho-style per-person modelling.** The brain learns how each person works
   and briefs them, and their coding agents, the way they like.
3. **A 3D graph** of every entity, so you can see the brain grow.

The name **livingbrain.wiki** was bought on Spaceship that night.

## Decisions, in order

| When (2026-10-03, WITA) | Decision | Status |
| :--- | :--- | :--- |
| 00:35 | Build a cheap Rust version of Company Brain as a new venture | Holds |
| 00:40 | Use Cratefield, Owlpost and Colonizer; customers can bring their own LLM | Holds |
| 00:52 | DeepSeek is the default managed model | Holds |
| 00:57 | Honcho's server is AGPL-3.0 and its SDKs are Apache-2.0. Never embed or modify the server | Holds |
| 01:06 | Name: livingbrain.wiki | Holds |
| 01:18 | Coding agents connect in both directions: prompts and decisions in, context out | Holds |
| 01:44 | Self-hosted is free and hosted is paid (open core) | Holds |
| 01:46 | Apache-2.0 core with commercial code in `ee/`; the repo is public | Holds |
| 01:51 | "Not only in Slack", then "just MCP or the CLI": one Rust binary, no chat adapters beyond Slack | **Superseded at 04:19** |
| 02:00 | LiteLLM is a first-class model provider (#42) | Holds |
| 02:26 | Logging and telemetry the Colonizer way (#44) | Holds |
| 02:40 | Storage (GB) is part of the plans; aggregate coding agents' logs (#45) | Holds |
| 02:56 | Gmail, Outlook and IMAP connectors, with encryption per scope (#43, #46) | Holds |
| 03:01 | A2A interfaces are not needed | Holds (dropped) |
| 03:05 | The site states the advantages (fewer tokens) with no numbers until an open benchmark exists (#48) | Holds |
| 03:14 | Import ChatGPT and Claude history (#49) | Holds |
| 03:20 | Screen everything the brain reads and serves with PromptDecode (#50); SupportGenius gets customer-safe answers (#51) | Holds |
| 03:31 | Crew includes $3 of DeepSeek credit; usage pricing: writing $1/M tokens, reading unlimited, reasoning per question | Credit and plan prices **superseded at 05:30**; usage prices hold |
| 03:44 | Connect Grafana and read logs; export OpenTelemetry (#52) | Holds |
| 04:10 | Jev as the fast yes/no judge (#53) | Holds |
| 04:13 | Adopt Hindsight's memory ideas: typed facts, mental models, four-way recall (#54) | Holds |
| **04:19** | **Coding agents, the CLI and the app come first. Slack and Discord are equal team-chat options. WhatsApp and Telegram come later** (#55 to #57) | **Latest surface decision** |
| 04:31 | Discord permissions follow Discord's own "View Channel" (#56) | Holds |
| 04:43 | Dreaming also reads arXiv, news and releases about the team's stack (#58) | Holds |
| 04:48 | Prices confirmed; tool calling filed upstream (Cratefield/harness#665) | Prices **superseded at 05:30** |
| 05:22 | "The app" is a white-label PWA plus an open API and SDKs (#40, #59) | Holds |
| **05:30** | **Pricing v3**, after a cost model of Cloudflare, DeepSeek and payment fees (below). Do not call api.honcho.dev on hosted plans; build the learning layer in-house | **Latest pricing decision** |
| 05:30 | "Coding agents" are "coding harnesses", Hermes included (#38) | Holds |
| 17:20 | No company exists, so billing goes through **Polar as Merchant of Record**, behind Cratefield's `Payments` port (`adapter-polar`, Cratefield/harness#690). Annual billing is the default because of the fixed fee | Holds |
| 2026-10-04 | Show "Built with" in the product (#61); branded email through Owlpost (#63) | Holds |

### Resolved contradictions

- **Surfaces.** An earlier note said "Slack first, then MCP or CLI only, no
  Teams, Discord or WhatsApp". The founder later said "I don't need Slack so much,
  Discord also", and the agreed order became: coding harnesses over MCP, the CLI and
  the app first, then team chat with Slack and Discord as equals, then WhatsApp and
  Telegram. Microsoft Teams is not planned. Slack is not required, so workspaces
  and sign-in must not depend on it.
- **Pricing.** Plans went from Community / Teams $5 / Crew $9 with $3 of credit to
  the v3 table below. v3 holds.
- **Honcho.** The first plan used api.honcho.dev through the Apache SDK. The cost
  model showed about $2 per workspace per month for ingestion, which wipes out the
  margin. The learning layer is now built in-house as a Cratefield-style module
  that reimplements the ideas (peer representations, background "dreaming"), with
  no Honcho code.
- **Payments.** Early issues said Stripe. Polar is the seller of record until a
  company exists; switching to `adapter-stripe` later is only a composition change.

## Product

A brain for your team that writes its own company wiki, and keeps improving it.

- **Listen.** Team chat (Slack, Discord), mail (the Owlpost brain inbox first, then
  Gmail, Outlook and IMAP), coding agents' session logs, ChatGPT and Claude
  exports, git, monitoring and logs, support tickets, and the research radar.
  Every source is stored once, screened and redacted, with its scope.
- **Write.** Extraction turns sources into typed facts and observations, and
  those into Markdown pages for people, projects, decisions, customers and
  systems. Every claim cites its source, and citations resolve only for people
  allowed to see them.
- **Evolve.** Nightly passes merge duplicates, surface contradictions for a person
  to settle (never resolved silently), refresh stale facts, update standing
  answers and write a changelog. Dreaming also scans papers and releases.
- **Learn.** The in-house learning layer models each person and tailors answers
  and agent briefs.
- **Ask anywhere.** MCP for every coding harness, the `livingbrain` CLI (one
  static Rust binary with a local cache), the white-label PWA, the API and SDKs,
  and team chat. All of them go through one permission function, `scopes_for`.
- **Act.** Tools over MCP under the asker's own connection, and Colonizer colonies
  for real work.
- **See it.** A live 3D graph in the app and from `livingbrain view`.

## Architecture on Cratefield

- **One Cloudflare Worker** (`crates/livingbrain-venture`) with D1 for indexes and
  metadata, R2 for page bodies, sources and logs, KV, and venture-owned Durable
  Objects (one per conversation). Modules see only Cratefield ports (`Database`,
  `TextModel`, `Embedder`, `VectorIndex`, `Classifier`, `Mailer`, `Payments`,
  `Signer`, `Defer`, `Blob`, `KeyValue`, `Realtime`); adapters are the only
  vendor-aware code.
- **Models:** `adapter-openai-compatible` (DeepSeek, OpenRouter, LiteLLM, or
  self-hosted), with tool calling on `TextModel` (Cratefield/harness#665),
  `Embedder` and `VectorIndex` (Workers AI and Vectorize), and the `Classifier`
  port for the Jev judge (`adapter-typesafe`) with an LLM fallback.
- **Cratefield modules we reuse:** `auth-magic-link`, `auth-passkeys`, `auth-oidc`,
  `module-device-auth` (CLI login), `module-orgs`, `module-waitlist`,
  `module-telemetry`, `module-webhooks`, `module-privacy` (subject access and
  erasure), `module-changelog`, `kms` and `secrets`, `adapter-github-app`,
  `adapter-cloudflare-saas` (white-label domains), `adapter-webpush`,
  `adapter-polar`, `adapter-owlpost` and `mail-templates`, and the
  `module-error-reporting` reporter (Cratefield/harness#692).
- **Tenancy:** one database, isolation enforced by the module (ADR 0001). A
  workspace is our own id. Slack and Discord are linked connections.
- **Encryption:** one data key per scope, wrapped by a workspace key in KMS;
  blind-index keyword search; crypto-shredding for "forget" and erasure (#43).
- **Cost guards** (from the pricing model): page bodies and logs in R2, not D1;
  384-dimension embeddings unless the benchmark shows a loss; nightly work in
  DeepSeek's off-peak hours; cheap yes/no decisions on the judge model.

## What we reuse from the portfolio

| Venture | Role in Living Brain |
| :--- | :--- |
| **Cratefield** | The harness, ports, adapters and modules listed above |
| **Owlpost** | All email: waitlist confirmation, digests, the brain's own inbox with screening, branded templates |
| **Colonizer** | Runs real work in microVMs from any surface and returns PRs and files; its learnings flow back into the wiki |
| **PromptDecode** | Its engine screens every way in (chat, mail, imports, agent logs, git, fetched papers) and every way out (MCP, CLI) for hidden text |
| **SupportGenius** | Customer-safe answers from published pages, routing by ownership, and the destination for Living Brain's own error reports |
| **Keep Shipping** | Planned deploys for the site and Worker |
| **Polar** (third party) | Merchant of Record for the hosted plans |

## Pricing (planned, per workspace)

| Plan | Price | Includes |
| :--- | :--- | :--- |
| **Community** | Free | Self-hosted on your own Cloudflare account and LLM key, up to 5 people, all core features |
| **Teams, self-hosted** | $5/mo | License key: unlimited people, SSO, admin and audit log, shared prompt library, per-channel policies, white-label app |
| **Teams, hosted** | $9/mo or $90/yr | Your own LLM, 25 people included, 10 GB |
| **Crew, hosted** | $15/mo or $150/yr | $5 of DeepSeek credit a month (or your own LLM), 25 people included, 25 GB |

Extra people on hosted plans cost +$5/mo per 25. Annual billing is the default.
Hosted usage: writing $1 per million tokens (the nightly evolve is included),
reading unlimited, reasoning per question from $0.001 (Minimal) to $0.25 (Max),
and extra storage $0.25 per GB-month, drawn from credit first. There is no usage fee
on your own LLM key. Over a cap, a workspace pauses; it is never billed by
surprise. A lapsed plan never deletes data or locks the export.

## Licence

Apache-2.0 for everything in this repository except `ee/` (SSO, admin and audit
log, shared prompt library, per-channel policies), which ships under a commercial
licence and needs a Teams key. Self-hosting Community, up to 5 people, is free.
Attributions are in [`NOTICE`](../NOTICE): Company Brain (design), Hindsight (MIT,
ideas and redaction patterns). No AGPL code is used.

## How the work is done

Each issue is one pull request under about 1,500 lines, with acceptance tests and
a `## Depends on` section listing only real blockers. Colonizer colonies on the
omarchy mothership pick up issues whose dependencies are closed. Issues labelled
`needs-human` need an owner decision or an owner-only action (accounts, legal
text, production) and are never dispatched.

## Open questions for the owner

- Polar's fee is 5% plus 50¢ per charge. Do Teams hosted at $9/mo and the $5
  self-hosted licence still hold on monthly billing, or should monthly be dropped?
- Crew's default model is DeepSeek. Some buyers will not send company data to a
  China-based provider. Should the hosted default, or its disclosure, change?
- Should Community self-hosters be able to plug in api.honcho.dev with their own
  key, or is the in-house learning layer the only option?
