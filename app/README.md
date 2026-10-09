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
| `POST /v1/models/discover` | `settings.html` | **yes** — the provider's model list, fetched server-side with the key the form holds; nothing stored |
| `GET /v1/tokens` | `settings.html` | **yes** |
| `POST /v1/tokens` | `settings.html` | **yes** — the only call that returns a token value |
| `DELETE /v1/tokens/{prefix}` | `settings.html` | **yes** |
| `GET /v1/pages/{slug}` | `wiki.html` | **no route yet** |
| `PUT /v1/pages/{slug}` | `wiki.html` | **no route yet** |

## What is NOT wired to a backend yet

These are shown on purpose, marked in the UI, and never sent:

- **Passkey sign-in.** Deferred with the follow-up identity work (ADR 0002).
  Rendered as a disabled control labelled "Not available yet".
- **Discord sign-in.** Issue #56. Also a disabled control; there is no OAuth
  route to link to, so the button is not an anchor to a 404.
- **Pages read/write.** `GET /v1/pages/{slug}` and `PUT /v1/pages/{slug}` are
  what `wiki.html` speaks, but no module mounts them. The page therefore shows
  its error notice; the conflict handling (a `409` on a stale `base_version`)
  is written and tested against the shape the store will use.
- **Citations and backlinks on a page.** The wiki page renders both, and the
  `GET /v1/pages/{slug}` contract includes them, but no route returns them yet.
- **Settings other than models** — proactivity, tools, automations, skills and
  the Colonizer connection. The page lists them in one "Coming soon" group with
  a line each and no controls at all; `LB.setting` still refuses to send them.
- **Audit.** The app sends `section`, `key` and `value` with every settings
  write so a server *can* audit it; no server records it yet.

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
- `tests/providers.test.mjs` — the vendored catalog is well formed, and the
  sync script's parser and validator refuse what they should.
- `tests/models.test.mjs` — the Models section against the Rust handler's own
  role list and the catalog the server compiles in, form validation, the cards, disconnect through
  `LB.setting`, and the settings page's copy (no API paths or dead controls).
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
| `wiki.html` | Reader and editor: Markdown, backlinks, citations, `base_version` saves |
| `assets/app.css` | The website's tokens (`--bg`, `--bg2`, `--ink`, `--line`, `--accent`, oklch), fonts and components |
| `assets/app.js` | Browser glue: `$`, `$$`, `esc`, `api`, theme, `LB.setting`, sign-out |
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

