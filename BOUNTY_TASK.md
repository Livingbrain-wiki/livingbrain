# Bounty Objective: Open-core plans, Teams license and hosted billing
Target: Livingbrain-wiki/livingbrain Issue #32
URL: https://github.com/Livingbrain-wiki/livingbrain/issues/32

## Issue Description
Part of #22. Depends on #30.

- Plans (per workspace, decided 2026-10-03 after a cost model of Cloudflare, DeepSeek and Stripe): Community free (self-hosted, up to 5 people); Teams self-hosted license $5/mo (unlimited people, team features, white-label); Teams hosted $9/mo or $90/yr (bring your own LLM, 25 people included, 10 GB); Crew hosted $15/mo or $150/yr ($5/month DeepSeek credit or your own LLM, 25 people included, 25 GB); extra people on hosted plans +$5/mo per 25; annual billing is the default. Usage on hosted plans: writing $1/M tokens (nightly evolve included), reading unlimited, reasoning per question $0.001-$0.25, extra storage $0.25/GB-month, drawn from credit first; no usage fee on the customer's own LLM key. Margin targets: Teams hosted about 60-75%, Crew about 45-55%. Cost guards: keep page bodies in R2 not D1 (D1 storage $0.75/GB vs R2 $0.015/GB), prefer 384-dim embeddings unless #48 shows a quality loss, run nightly passes in DeepSeek off-peak hours, no api.honcho.dev on hosted plans (build the learning layer in-house, #54).
- Self-hosted license keys: signed offline-verifiable keys (harness `Signer`), checked at startup and daily; an expired key degrades to Community limits, never deletes data.
- Hard usage caps per plan; overage pauses rather than bills by surprise.

**Acceptance criteria**
- [ ] A workspace over its cap is paused with a clear message, not charged.
- [ ] Model cost per workspace is visible to the owner.
- [ ] Community self-hosted never calls Stripe; over 5 people it asks for a Teams key instead of breaking.
- [ ] A lapsed license or subscription never deletes or locks the wiki export.

## Instructions for Kuro (Autonomous Solver):
1. Use skills: `systematic-debugging` and `code-review-and-quality`.
2. Inspect the repository structure, locate relevant files, and understand the bug/feature.
3. Apply the minimal, robust fix or implementation.
4. Run any existing test suites or linters in the workspace.
5. Ensure the fix adheres to the project's existing code style.
6. When done, output a summary of files modified and verification results.
