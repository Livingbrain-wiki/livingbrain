# The Living Brain app

The web app: a **static, no-build** front end. Plain HTML, one stylesheet,
ES modules loaded with `<script type="module">`, no bundler, no npm
dependencies and no `package.json`. It is served as static assets from
Cloudflare and talks to the existing Rust Worker API over the session cookie.

## Run it locally

```sh
cd app
python3 -m http.server 8080
# then open http://localhost:8080/
```

`python3 -m http.server` serves the directory as-is, which is exactly how
Cloudflare serves it. The pages are static, so no other process is needed to
see them.

To talk to a local Worker, set the API base in each page's `<head>`:

```html
<meta name="lb-api" content="http://localhost:8787" />
```

## The API base URL

`app/assets/app.js` reads `<meta name="lb-api">`:

- **empty (the default)** — same origin. This is the hosted deployment, where
  the app and the Worker are one host.
- **an origin** — a local preview, or a white-label deployment whose Worker
  lives elsewhere. CORS must allow that origin; `livingbrain-venture` currently
  allows `https://livingbrain.wiki` and `https://api.livingbrain.wiki`
  (`crates/livingbrain-venture/src/lib.rs:58`).

## Endpoints the app calls

| Call | Where | Server-side? |
| :--- | :--- | :--- |
| `POST /v1/workspaces/email/start` | `index.html` | **yes** — always `202 {"status":"accepted"}` |
| `GET /v1/workspaces/slack/start` | `index.html` | **yes** |
| `GET /v1/workspaces/me` | every page | **yes** — `200` shows the signed-in home (and the header's "Signed in as …"), `401` the sign-in form |
| `POST /v1/workspaces/signout` | every page | **yes** — `204`, drops the session cookie; same-origin only |
| `GET /v1/models` | `settings.html` | **yes** |
| `PUT /v1/models/{role}` | `settings.html` | **yes** — via `LB.setting("models", role, connection)` |
| `DELETE /v1/models/{role}` | `settings.html` | **yes** — via `LB.setting("models", role, null)` |
| `PATCH /v1/models/{role}` | `settings.html` | **yes** — body `{model}`: the top "Main model" picker and a sheet's **Test** switch (or re-probe) a connection's model on the key already stored; via `LB.setting("model-switch", role, {model})` |
| `POST /v1/models/discover` | `settings.html` | **yes** — `{models:[{id, label?, context?, price?}], listed, note?}`: the provider's model list, fetched server-side with the key the form holds; nothing stored. A provider with no list of its own (a `404`/`405` from it, or a card that says so) answers its curated models from `assets/model-cards.json` with `listed: false` and a note; only a refused key (`401`/`403`) is an error |
| `GET /v1/models/mine` | `settings.html` | **yes** — `{runners:[{role, kind, agent, model, settings, last_seen_at}]}`: the member's own subscription runners. A `404` (a server without the routes yet) hides the runner choices |
| `PUT /v1/models/mine/{role}` / `DELETE` | `settings.html` | **yes** — `{kind:"local_runner", agent:"claude"\|"codex", model, settings}`; via `LB.setting("model-runner", role, value)`. No credential is ever sent: the member's own CLI on their own device does the call |
| `GET /v1/workspaces/members` | `settings.html` | **yes** — `[{user_id, name, display_name, avatar_url, timezone, is_admin, role}]`; `name` is the one everybody reads (the chosen one, else the sign-in's) |
| `PATCH /v1/me` | `settings.html` | **yes** — `{display_name}` (1–64 visible characters, trimmed) or `{display_name: null}` to go back to the sign-in name; per workspace. Session from this origin, or an unnarrowed bearer token |
| `PUT /v1/me/avatar` / `DELETE` | `settings.html` | **yes** — the raw image (`image/png`, `image/jpeg` or `image/webp`, ≤ 512 KB, ≤ 4096 px a side), checked by its bytes and stored without its metadata; the page crops it to a 256 px square on a canvas first |
| `PATCH /v1/workspaces/current` | `settings.html` | **yes** — `{name}`; owners and admins, session only |
| `PUT /v1/workspaces/current/avatar` / `DELETE` | `settings.html` | **yes** — the workspace's picture, same rules as yours; owners and admins |
| `GET /v1/avatars/{id}` | every page (`<img>`) | **yes** — the image, to members of the workspace whose row names it; `404` for anyone else. With no picture the app draws initials on a colour fixed by the id — no external avatar service is asked |
| `PATCH /v1/workspaces/members/{id}` | `settings.html` | **yes** — body `{role}`; owner/admin only, only an owner touches owners, `409` for the last owner |
| `DELETE /v1/workspaces/members/{id}` | `settings.html` | **yes** — remove (owner/admin) or leave (yourself); `409` for the last owner |
| `GET /v1/workspaces/invites` / `POST` / `DELETE /v1/workspaces/invites/{id}` | `settings.html` | **yes** — open invitations; `POST {email, role}` mails a 7-day link |
| `GET /v1/workspaces` / `POST /v1/workspaces/switch` | `settings.html` | **yes** — the workspaces your address belongs to, and switching the session to one |
| `GET /v1/tokens` | `settings.html` | **yes** |
| `POST /v1/tokens` | `settings.html` | **yes** — the only call that returns a token value |
| `DELETE /v1/tokens/{prefix}` | `settings.html` | **yes** |
| `GET /v1/pages?limit=N` | `wiki.html` | **yes** — `{pages:[{scope, slug, entity_type, version, updated_at, url}]}`, newest first; the home asks for 200, the server's cap |
| `GET /v1/pages/{slug}` | `wiki.html` | **yes** — `{slug, title, markdown, url, version, entity_type, backlinks, citations}`; `404` is an unknown slug, `401` is signed out |
| `PUT /v1/pages/{slug}` | `wiki.html` | **yes** — body `{markdown, base_version, title}`; `base_version: null` creates, a stale number is refused with `409`, and the response (post-redaction) is what the page then renders |
| `GET /v1/search?q=&limit=` | `wiki.html` | **yes** — `{results:[{title, url, snippet}]}`; the header's search box asks for 10 of the server's 25 cap |
| `POST /v1/ask` | `wiki.html` | **yes** — body `{question}`; `{answer, citations}` rendered in the ask dialog. A `202 {status:"pending", poll}` (your own runner is answering) is polled at `GET /v1/model-jobs/{id}` and the ask re-sent (`askWithRunner`) |
| `GET /v1/sources/projects` | `wiki.html` | **yes** — `{projects:[{project, sources}]}`; fills the project filter beside the search box and on Agent sessions, hidden when empty or on any failure. With a project chosen, search sends `&project=` |
| `GET /v1/sources?kind=agent_log&limit=50&project=&cursor=` | `wiki.html?view=sessions` | **yes** — `{sources:[{id, kind, scope, path, project, held, created_at}], next_cursor}`, newest first; "Load more" sends the cursor, and the chosen project is kept in the address (`&project=`) |
| `GET /v1/coding-agents/queue?limit=100` / `?source=` | `wiki.html?view=sessions` | **yes** — `{counts, model, budget, triage, jobs:[{source_id, status, error, pages, triage, estimate, can_compile_fully, …}]}`; the queue card above the sessions (with a triage line: skipped, summarised, compiled in full, tokens saved) and a status pill plus "Wrote …" links on each session, and the compile line above a transcript. Any failure hides the card and leaves the list |
| `POST /v1/coding-agents/process` | `wiki.html?view=sessions` | **yes** — body `{max: 1}`; the card's **Process now** compiles the next of your sessions and refreshes the list |
| `GET /v1/graph?project=&kind=&limit=` | `brain.html` | **yes** — `{nodes:[{id, kind, label, slug?, project?, agent?, created_at, updated_at, backlinks, facts, degree, url?}], edges:[{source, target, kind}], counts, projects, agents, limit, truncated}`: the asker's own readable pages, sources, projects and fact subjects (same scope and workspace rules as the listings; nothing unsealed). Filters apply before the limit (default 800, clamped to 2000); the page asks for 2000 |
| `POST /v1/coding-agents/retry` | `wiki.html?view=sessions` | **yes** — the card's **Retry failed** (shown when something failed) requeues your failed sessions with their attempts reset |
| `POST /v1/coding-agents/compile-fully` | `wiki.html?view=sessions` | **yes** — body `{source_id}`; **Compile fully** on a `skipped-trivial` or `summary-only` session compiles it in full (no triage, no compaction) and refreshes the list |

## What is NOT wired to a backend yet

These are shown on purpose, marked in the UI, and never sent:

- **Passkey sign-in.** Deferred with the follow-up identity work (ADR 0002).
  Rendered as a disabled control labelled "Not available yet".
- **Discord sign-in.** Issue #56. Also a disabled control; there is no OAuth
  route to link to, so the button is not an anchor to a 404.
- **Settings other than models** — proactivity, tools, automations, skills and
  the Colonizer connection. The page lists them in one "Coming soon" group with
  a line each and no controls at all; `LB.setting` still refuses to send them.
- **Audit.** The app sends `section`, `key` and `value` with every settings
  write so a server *can* audit it; no server records it yet.

## The app shell and navigation

Every page shares one shell (docs/ux-navigation.md has the audit and the
information architecture): a top bar — workspace switcher, **Search or ask**
(the ⌘K / Ctrl+K command palette, also `/`), theme, user menu — and a left
sidebar grouped **Brain** (Home, Ask the brain, Pages, Facts), **Sources**
(Agent sessions, Coding agents, GitHub, Imports), **Workspace** (Members,
Agent accounts, Access & audit) and **Settings** (Models, Usage & limits,
Integrations, API tokens, Account). On screens under 56rem the sidebar is a
drawer behind the ☰ button; on wider ones it collapses to an icon rail.

- `assets/nav.js` is the IA as data — the destinations, the settings tabs,
  which destination an address is (`activeId`), breadcrumbs, and the
  palette's rows — and pure, so `tests/nav.test.mjs` pins that every address
  published before the shell (`wiki.html?slug=`, `?view=sessions…`,
  `settings.html#tokens`, the API's `/wiki?slug=` citations) still lands on a
  destination.
- `assets/shell.js` builds the sidebar and the palette from it with
  `createElement`/`createElementNS` only, and wires the drawer, the collapse
  rail and the two menus. `app.js` mounts it in `boot()`; `wiki.js` tells it
  which view is showing with an `lb-view` event.
- `settings.html` shows one tab at a time, chosen by the hash; `#connect`,
  `#soon` and `#stack` land on Integrations.
- `wiki.html` gained `?view=ask[&q=]` (the one box: page hits as you type,
  Enter asks), `?view=coding-agents` (the #187 tree, moved off the Pages
  list), `?view=imports` (`GET /v1/sources?kind=import`, each readable in
  place) and `?view=new`.

## The Brain map

`brain.html` (Brain → Brain map) draws `GET /v1/graph` as a 3D graph in the
website brain's style. `assets/graph.js` is the pure part — shaping the
answer, the filters, the time slider, search, and the 3D force layout
(Barnes–Hut repulsion inside a two-hemisphere brain envelope: knowledge
left, sources right); `assets/brain-render.js` is the canvas renderer, ported
from the website's `assets/brain.js` (provenance in `assets/vendor/VENDOR.md`;
no third-party library, so the CSP stays `'self'`); `assets/brain.js` is the
page. It needs only a canvas 2D context — no WebGL — and "Flat" draws the
same nodes face-on; without a canvas the list under the stage is the map.
`prefers-reduced-motion` stops the spin, the pulses and the animated settling.

## The wiki page: wired end to end

`wiki.html` is a four-view shell (home, page, editor, new-page form) run by
`assets/wiki.js`, which `app.js` imports and dispatches for
`body[data-page="wiki"]`.

- **Home.** `GET /v1/pages?limit=200` fills Recent in server order
  (`updated_at` desc); a filter input narrows rows client-side on slug and
  type. **Pinned** pages are the fetched rows whose `scope/slug` appears in
  the browser's `localStorage` under `lb-wiki-pinned` — pins are
  per-browser, not per-account, and pinning/unpinning happens from the page
  view's Pin button or the pinned list's Unpin buttons. Zero pages shows an
  empty state that says where pages come from (imports, MCP `brain_note`,
  Slack) with a New page button.
- **Page.** `GET /v1/pages/{slug}` renders markdown, backlinks and citations
  through the shared renderer; headings get slug ids and a table of contents
  (h2/h3) mounts beside the page and in the small-screen drawer. A `404`
  says no page has this slug yet and points at New page.
- **Editor.** A Write/Preview toggle (the preview always re-renders the
  textarea) and a save that PUTs `{markdown, base_version, title}`. A clean
  save re-renders the view **from the response** — the server redacts
  secrets before storing, so if the returned markdown differs from what was
  sent the page says so. A `409` keeps the editor and the draft and offers
  two buttons on the notice: *Load their version* (re-GET into the editor)
  or *Overwrite with mine* (re-GET for a fresh `base_version`, then PUT
  again). New page validates the slug inline against the server's exact rule
  (`/^[a-z0-9-]{1,128}$/`) before anything is sent, and creates with
  `base_version: null`.
- **Ask.** A dialog posts the question to `POST /v1/ask` and renders the
  answer through the markdown renderer, with citations linked only when
  their URL is safe.
- **Search.** The header box debounces 200 ms and queries
  `GET /v1/search?q=&limit=10` from two characters; hits are listed in a
  listbox with `<mark>` highlights, ArrowUp/ArrowDown/Enter keyboard
  navigation (through `parseBrainUrl`, which recovers the slug from the
  result's `url`), `/` focuses the box from anywhere, and Escape closes.

**Signed out is a state, not an error.** These routes answer `401` without a
session, and the page treats that as its signed-out view: a sign-in link
instead of lists, and no Edit button. Every route above answers to the
session cookie as well as to a CLI bearer token (`SessionAuth` in
`crates/livingbrain-api`, wired by `livingbrain-venture`); a cookie-signed
write (`PUT /v1/pages/{slug}`, `POST /v1/ask`, `POST /v1/notes`) must come
from this origin — `sec-fetch-site: same-origin`, or an `Origin` naming this
host — or it is refused with `403`, which the app's own `fetch` always
satisfies.

The shapes above are pinned by `tests/wiki.test.mjs`, which boots the real
`wikiPage` against stub DOM and fetch — the home list and pins, the signed-out
paths, the TOC build, the create save (`base_version: null`), the redaction
notice, the `409` recovery buttons, search and ask.

## The settings choke point

Every settings change goes through **one** function, `LB.setting(section, key,
value)` in `assets/app.js` (`setSetting`). The pages never build a settings
request and never call `api()` for a settings route themselves. The request
shape is built by the pure `settingRequest` in `assets/settings.js`, so
`node --test` can pin it.

That gives the audit criterion two things: there is exactly one place a setting
is mutated (grep for `LB.setting(`), and the section, key and value travel with
the change so the server can attribute each write without parsing the URL.

## Rendering safety

The wiki body is user content. It is never turned into markup:

- `assets/markdown.js` returns **node descriptions**, not HTML strings. The only
  leaf kind is `text`.
- `assets/app.js` builds the DOM from those with `createElement` and
  `textContent`.
- There is **no `innerHTML`, `insertAdjacentHTML`, `outerHTML` or
  `document.write` anywhere in `app/`** — verify with
  `grep -rnE "\.(inner|outer)HTML *=|insertAdjacentHTML *\(|document\.write *\(" app/`
  (which prints nothing).
- Link URLs are checked against `http:`, `https:` and `mailto:` (or no scheme at
  all); anything else keeps its visible text and loses its `href`.
- `app/_headers` sets a CSP with no `unsafe-inline` and no `unsafe-eval`. The
  one inline script is the pre-paint theme bootstrap; its SHA-256 is in the CSP
  and must be regenerated if the script is edited (the command is in a comment
  in `_headers`).

## Tests

Node's built-in runner, no npm dependencies:

```sh
node --test "app/tests/**/*.test.mjs"
```

(`node --test app/tests/` — the bare-directory form — does not work on the
Node 24 build used here; the glob form does, and is what CI runs.)

The test files (`node --test` prints the current count):

- `tests/markdown.test.mjs` — the Markdown pipeline, the citation renderer and
  the backlinks renderer, imported from `assets/markdown.js` directly.
- `tests/settings.test.mjs` — the settings request shape, imported from
  `assets/settings.js` directly.
- `tests/runtime.test.mjs` and `tests/dom.test.mjs` — import `assets/app.js`
  itself. It guards its DOM boot, so Node loads the same file the pages load
  and exercises the real `api` wrapper, the real `LB.setting` choke point
  (including the bytes that reach the wire), and the real `mountNodes`.
- `tests/wiki.test.mjs` — the wiki page, booted for real: stub elements, a
  `location`, a `localStorage` and a recorded `fetch` are installed before
  `assets/app.js` is imported, so the import runs the same `boot()` the
  browser runs and every test then drives the actual `wikiPage` — the home
  list with its pins, the signed-out views, the TOC build, the create save
  (`base_version: null`), the redaction notice, the `409` recovery buttons,
  and search and ask, asserted through DOM state.
- `tests/providers.test.mjs` — the vendored catalog is well formed, and the
  sync script's parser and validator refuse what they should.
- `tests/models.test.mjs` — the Models section against the Rust handler's own
  role list and the catalog the server compiles in, form validation, the cards, disconnect through
  `LB.setting`, and the settings page's copy (no API paths or dead controls).
- `tests/nav.test.mjs` — the IA: every legacy address resolves, the settings
  tabs cover every section, breadcrumbs, palette rows, and the three pages
  carry one shell with the same ids and an untouched theme script.
- `tests/shell.test.mjs` — the shell on a stub DOM: the sidebar, the current
  item, the drawer and rail, the switcher, and the palette by keyboard.
- `tests/graph.test.mjs` — the map's shaping, filters before the time
  slider, search, tooltips, and the layout keeping every node inside its
  hemisphere; brain.html's shell and that nothing it loads leaves `'self'`.
- `tests/session.test.mjs` — the `/me` check and sign-out from
  `assets/session.js`: signed in, signed out and "could not tell", what each
  one shows, and the hidden-until-checked markup of `index.html`.

`dom.test.mjs` swaps `globalThis.document` for a recorder that offers only
`createElement`, `createTextNode`, `createDocumentFragment` and
`setAttribute`, so a future switch to `innerHTML` would throw rather than pass
quietly.

## Layout

| File | What it is |
| :--- | :--- |
| `index.html` | The signed-in home when there is a session; otherwise sign-in: email magic link, Slack, and the two options with no route yet |
| `settings.html` | Models (guided connect, one card per role), API tokens, Connect your coding agent, Coming soon, and a collapsed "Built with" |
| `wiki.html` | The four-view wiki shell: home (all pages, pins, filter), a page with its TOC, backlinks and citations, the editor, the new-page form, the ask dialog, header search and the small-screen drawer |
| `assets/app.css` | The website's tokens (`--bg`, `--bg2`, `--ink`, `--line`, `--accent`, oklch), fonts and components |
| `assets/wiki.css` | The wiki page's own styles, on the shared tokens: the search panel, drawer, page lists, editor toggle and touch-target sizes |
| `assets/app.js` | Browser glue: `$`, `$$`, `esc`, `api`, theme, `LB.setting`, sign-out, the settings tabs, mounting the shell |
| `assets/nav.js` | The information architecture as data: destinations, settings tabs, `activeId`, breadcrumbs, palette rows |
| `assets/shell.js` | The app shell: sidebar, drawer, collapse rail, workspace switcher, user menu, ⌘K palette |
| `brain.html` | The Brain map: the 3D graph of the asker's brain, its filters, time slider, side panel and list |
| `assets/graph.js` | The map's data shaping, filters, search and 3D force layout (pure) |
| `assets/brain-render.js` | The map's canvas renderer, ported from the website's brain |
| `assets/brain.js` | The Brain map page controller |
| `assets/brain.css` | The Brain map's own styles |
| `assets/wiki.js` | The wiki controller (`wikiPage`) and its pure helpers (`slugify`, `validSlug`, `parseBrainUrl`, `highlightTerms`); imported by `app.js`, dispatched for `body[data-page="wiki"]` |
| `assets/models.js` | The roles, the provider catalog as the picker uses it, connect-form validation, and the connected-model cards |
| `assets/providers.json` | The provider catalog: every provider Colonizer supports, vendored by `scripts/sync-providers`. The server compiles in the same file |
| `assets/session.js` | Who is signed in: the `/me` check, its three outcomes, and what each page shows for them |
| `assets/markdown.js` | Pure Markdown / citation / backlink renderers |
| `assets/settings.js` | Pure settings request builder |
| `assets/favicon.svg`, `assets/*.png` | The mark and the PWA icons, copied from the website |
| `assets/fonts/` | Bricolage Grotesque, Hanken Grotesk, JetBrains Mono: the website's self-hosted fonts (OFL, latin, variable) |
| `sw.js` | App-shell cache; minimal and defensive, never caches `/v1/*` |
| `manifest.webmanifest` | The white-label PWA manifest |
| `_headers` | CSP and security headers for Cloudflare Pages |

## Theming

The look is the livingbrain.wiki website's (Livingbrain-wiki/website,
`assets/livingbrain.css`): the same oklch tokens in both themes, the same
self-hosted fonts (Bricolage Grotesque for headings, Hanken Grotesk for text,
JetBrains Mono for labels), the same ring-and-node mark, header, cards, buttons
and fields. The fonts are served from `assets/fonts/`, so the CSP's
`font-src 'self'` covers them and nothing is fetched from a font CDN. When the
website's tokens change, copy them into the token block at the top of
`assets/app.css`; the pages use the shared classes only, never per-page styles.

Dark by default, light follows the system, and a choice is remembered in
`localStorage` under `lb-theme`. Changing it dispatches an `lb-theme` event on
`document`, which is what the toggle button listens to.

## The provider catalog

`assets/providers.json` lists every LLM provider a model can be connected
from: Colonizer's catalog (Colonizer-dev/harness `web/src/providerCatalog.ts`,
pinned to the commit recorded in the file) plus Living Brain's two built-ins,
Anthropic and OpenAI. Each entry names its base URL (with `${VAR}`
placeholders where the provider needs an id), its auth style (`bearer` or
`x-api-key`) and its wire (`anthropic` Messages or `openai` chat completions).
The server (`crates/livingbrain-models/src/catalog.rs`) reads the same file
with `include_str!`, so the picker cannot offer a provider the server would
refuse.

To move to a newer upstream:

```sh
scripts/sync-providers              # latest main
scripts/sync-providers --ref <sha>  # a pinned revision
```

CI runs `scripts/sync-providers --check --pinned`, which re-derives the file
from the commit it records and fails on any hand edit.

