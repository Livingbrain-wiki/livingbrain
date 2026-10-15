# Web app navigation: audit and information architecture

This note records how the web app (`app/`) was navigated before the
app-shell redesign, what was confusing about it, and the information
architecture (IA) that replaced it. The URL table at the end is the contract:
every address that worked before still works, because pages, the Mac app
(`macos/LivingBrainBar`) and the API's own citation links (`/wiki?slug=…`)
point at them.

## 1. Audit: what existed before

### Documents

| Document | `data-page` | What it held |
| :--- | :--- | :--- |
| `index.html` | `signin` | Signed out: the sign-in form (email link, Slack, passkey/Discord placeholders). Signed in: a "home" with the workspace card, sign-out and three "next steps" (open the wiki, connect a model, connect a coding agent). It had **no navigation at all** besides the brand link. |
| `wiki.html` | `wiki` | A single-page shell of seven views, chosen by the query string. |
| `settings.html` | `settings` | One long scrolling page of seven sections with a sticky in-page section nav. |
| `/device…` (server) | — | The device-grant pages (`livingbrain login`), rendered by `crates/livingbrain-venture/src/device_pages.rs` inside the app's header and stylesheet. They link back to `/index.html` and `/settings.html#tokens`. |
| `/v1/workspaces/email/verify` (server) | — | The magic-link confirmation page (`livingbrain-workspaces`), linking to `/index.html`. |

### `wiki.html` views

| View | Reached by | Notes |
| :--- | :--- | :--- |
| home | `wiki.html` | All pages (recent 200), pinned pages (per browser), a filter, **and** the compiled `coding-agents/` folder tree (#187) above them. |
| page | `wiki.html?slug=<slug>` (also `/wiki?slug=…&scope=…`, the API's citation URL) | Markdown, facts card, backlinks, citations, TOC (side on desktop, drawer on phones), Edit / Ask about this page / Pin / New page. |
| editor | `?slug=<slug>&mode=edit`, or Edit | Write/Preview, optimistic-concurrency save. |
| new page | the "New page" buttons only | No address of its own. |
| sessions | `?view=sessions[&agent=][&project=]` | Agent sessions with agent and project selects, the compile-queue card. |
| transcript | `?view=sessions&source=<id>` | One session's transcript. Shares the `sessions` view name. |
| facts | `?view=facts` | Every temporal fact as of a date. |

### `settings.html` sections

Models (`#models`), API tokens (`#tokens`), Members (`#members`, which also
held "Your workspaces" and switching), Usage & limits (`#limits`), Connect
your coding agent (`#connect`), Coming soon (`#soon`) and a collapsed "Built
with" (`#stack`). Sections were all on one page; the section nav highlighted
whichever scrolled into view.

### Navigation elements

* **Header, `wiki.html`:** brand → `wiki.html`; a search box with a project
  select; links Wiki · Agent sessions · Facts · Settings · Sign in; "Signed
  in as" + Sign out; Theme; and on phones a Menu drawer that repeated the
  links and carried the page TOC.
* **Header, `settings.html`:** brand → `wiki.html`; links Wiki · Settings ·
  Sign in (no Agent sessions, no Facts); "Signed in as" + Sign out; Theme.
* **Header, `index.html`:** brand → `index.html`; Theme. Nothing else.
* **In-page links:** the home's three next-step buttons; sessions ↔
  transcript ("← All sessions"); coding-agents folders → sessions filtered by
  agent/project; page backlinks/citations; search results.
* **Keyboard:** `/` focused the wiki search box (wiki only).

### What made it confusing

1. **Three different headers.** Each document had its own link set; from
   Settings there was no way to reach Agent sessions or Facts, and the home
   page had no links at all.
2. **The brand went to different places** (`index.html` on one page,
   `wiki.html` on the others), so "home" meant two things.
3. **Mixed levels in one row.** "Wiki" (a place), "Agent sessions" (a
   source), "Facts" (a lens on the wiki) and "Settings" (configuration) sat
   side by side with no grouping.
4. **The wiki home did two jobs:** the curated pages and the machine-compiled
   `coding-agents/` tree, which could be hundreds of folders above the
   human pages.
5. **Search and Ask were separate** (a header box and a per-page dialog), and
   search only existed on `wiki.html`.
6. **Settings was a scroll of seven unrelated things** — workspace
   membership, personal tokens, the workspace's models and budgets — under
   one heading, with "Your workspaces" (switching) hidden inside Members.
7. **No location cues:** no breadcrumbs, a static "Company wiki" eyebrow on
   every wiki view, and no current-item state outside the header links.
8. **Nothing for "where is my data from"**: imports and (soon) GitHub had no
   place, and agent sessions were a peer of "Wiki".

## 2. The new information architecture

One app shell on every signed-in page: a **top bar** and a **left sidebar**.

```
┌ top bar ─────────────────────────────────────────────────────────────────┐
│ ☰  ◉ livingbrain.wiki   [Acme ▾]   [ Search or ask…            ⌘K ]  ◐  (A) │
├ sidebar ────────┬ main ──────────────────────────────────────────────────┤
│ BRAIN           │ Breadcrumbs                                             │
│  Home           │ EYEBROW (the group)                                     │
│  Ask the brain  │ Page title                                              │
│  Pages          │ Lede                                                    │
│  Facts          │ …                                                       │
│  Brain map      │                                                         │
│ SOURCES         │                                                         │
│  Agent sessions │                                                         │
│  Coding agents  │                                                         │
│  GitHub    soon │                                                         │
│  Imports        │                                                         │
│ WORKSPACE       │                                                         │
│  Members        │                                                         │
│  Agent accounts soon                                                      │
│  Access & audit soon                                                      │
│ SETTINGS        │                                                         │
│  Models         │                                                         │
│  Usage & limits │                                                         │
│  Integrations   │                                                         │
│  API tokens     │                                                         │
│  Account        │                                                         │
│ « Collapse      │                                                         │
└─────────────────┴─────────────────────────────────────────────────────────┘
```

* **Brain** — what the brain knows.
  * **Home** (`index.html`): the workspace overview and next steps; the
    sign-in form when signed out.
  * **Ask the brain** (`wiki.html?view=ask[&q=]`): one box. Typing lists the
    matching pages (the old header search, same keys); pressing Enter without
    choosing a page asks the brain and shows the answer with its citations.
    `?q=` pre-fills and asks.
  * **Pages** (`wiki.html`): **Projects first** — one card per project
    (canonical key, the repository's spelling: `EarthOS.world`), with its
    compiled sections (Overview, Decisions, Learnings, Architecture, Open
    questions, topics) — then pinned and recent pages by title, then the
    **Agents** section (each agent's own `patterns`/`index` pages), the
    filter and New page (`wiki.html?view=new`). Coding-agent session logs
    (`session` pages) are sources: the list asks the server to leave them out
    (`GET /v1/pages?exclude_type=session`) unless **Show session logs** is on
    (remembered per browser).
  * **Project** (`wiki.html?view=project&project=<key>`): the project's wiki
    by section, each with the agents that wrote it ("from Claude, Codex and
    Colonizer", each linking to that agent's page), its topics, the
    cross-agent rollup, recent sessions (by their note titles), GitHub items
    and facts. `&section=<page>` reads that section from every agent in turn
    (`EarthOS.world — Learnings`). Compiled pages are never shown by slug:
    `EarthOS.world — Learnings`, `Codex — Patterns`. The per-agent split
    stays at Sources → Coding agents.
  * **Facts** (`wiki.html?view=facts`).
  * **Brain map** (`brain.html`): the 3D graph of the asker's own pages,
    sources, projects and fact subjects (`GET /v1/graph`), in the website
    brain's style — knowledge in the left hemisphere, sources in the right.
    Orbit, zoom and pan; hover for a tooltip; pick a node to read it in a
    side panel; filter by kind, project and agent; search to focus; a time
    slider replays the brain growing; "Flat" shows it face-on; reduced
    motion stops the spin and the pulses; a list holds the same nodes for
    anyone the canvas does not serve.
* **Sources** — where the knowledge came from.
  * **Agent sessions** (`wiki.html?view=sessions`, filters and transcript
    unchanged).
  * **Coding agents** (`wiki.html?view=coding-agents`): the #187 folder tree,
    moved out of the wiki home with its behaviour intact.
  * **GitHub** — a reserved slot, shown as "Soon" until the GitHub
    connection's UI (Part D) lands.
  * **Imports** (`wiki.html?view=imports`): the imported sources
    (`GET /v1/sources?kind=import`), each readable in place.
* **Workspace** — who is in it and what they may do.
  * **Members** (`settings.html#members`).
  * **Agent accounts** and **Access & audit** — reserved slots, "Soon", for
    #180 (agent accounts, access grants, the audit log).
* **Settings** — how it runs.
  * **Models** (`settings.html#models`), **Usage & limits** (`#limits`),
    **Integrations** (`#integrations`: connect your coding agent, MCP, CLI,
    what is coming, "Built with"), **API tokens** (`#tokens`), **Account**
    (`#account`: who you are, your workspaces and switching, theme, sign out).

`settings.html` now shows **one tab at a time**: the hash picks the tab, so
each sidebar item is a page of its own with its own title, and the old
anchors map onto the tab that holds them (`#connect`, `#soon` and `#stack`
→ Integrations).

### The top bar

* **Workspace switcher** — the current workspace's name; opening it lists
  your workspaces (`GET /v1/workspaces`) and switches with the same request
  the Account tab sends.
* **Search or ask (⌘K / Ctrl+K, or `/`)** — the command palette, on every
  page: jump to any sidebar destination, find pages (`GET /v1/search`), or
  "Ask the brain" with the typed question. Arrow keys move, Enter opens,
  Escape closes.
* **Theme** — unchanged (dark default, light follows the system until
  chosen).
* **User menu** — "Signed in as …", Account, Sign out. Signed out, a
  "Sign in" link instead.

### Layout and behaviour

* **Desktop (≥ 56rem):** sidebar 15rem, collapsible to a 3.5rem icon rail
  (remembered per browser under `lb-nav-collapsed`).
* **Tablet and phone (< 56rem):** the sidebar is an off-canvas drawer opened
  by the ☰ button; Escape, the scrim or following a link closes it, and focus
  returns to the button. The page TOC on phones stays in its own "On this
  page" drawer.
* **Consistent page headers:** breadcrumb (page and transcript views), a
  mono eyebrow naming the group, the H1, a lede.
* **Breadcrumbs** on a page: `Pages / <title>`; for a compiled page
  `Pages / <Project> / <Section> / <Agent>` (`Pages / Agents / <Agent>` for an
  agent's own page); for a session log or a transcript `Agent sessions /
  <session>`.
* **Accessibility:** landmarks (`header`, `nav` "Main", `main`, `footer`), a
  skip link, `aria-current="page"` on the current item, the palette as a
  modal `dialog` with a combobox + listbox, the drawer with `aria-expanded`
  and focus return, visible focus rings, and nothing reachable only by hover.
* **Reduced motion:** the drawer and palette do not animate.

## 3. URL contract

| Address (before) | Now |
| :--- | :--- |
| `index.html`, `/` | Home (same document) |
| `wiki.html` | Pages |
| `wiki.html?slug=<s>` / `/wiki?slug=<s>&scope=<x>` | The page (same; a compiled page is under Pages, a session log under Agent sessions) |
| — | `wiki.html?view=project&project=<key>[&section=<page>]` — a project (new) |
| `wiki.html?slug=<s>&mode=edit` | The editor (same) |
| `wiki.html?view=sessions[&agent=][&project=]` | Agent sessions (same) |
| `wiki.html?view=sessions&source=<id>` | The transcript (same) |
| `wiki.html?view=facts` | Facts (same) |
| `settings.html` | Models (the first tab) |
| `settings.html#models` / `#tokens` / `#members` / `#limits` | That tab |
| `settings.html#connect` / `#soon` / `#stack` | Integrations |
| — (new) | `wiki.html?view=ask[&q=]`, `?view=coding-agents`, `?view=imports`, `?view=new`, `settings.html#integrations`, `#account`, `brain.html` |

Nothing moved, so the Mac app's links (`wiki.html?slug=`,
`wiki.html?view=sessions&project=`, `settings.html`) need no change.
`app/tests/nav.test.mjs` pins this table: every old address resolves to a
sidebar destination.
