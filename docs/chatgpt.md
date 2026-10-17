# ChatGPT

ChatGPT reads the brain the way Claude's connector does: as a custom MCP
server, over OAuth, with no token to copy. Deep research and company
knowledge answers come out of it cited.

**Connector URL:** `https://mcp.livingbrain.wiki/v1/pages/mcp`

## Add the connector

1. In ChatGPT, open **Settings → Apps → Connectors** (older UI:
   **Settings → Connectors → Advanced**, with **Developer mode** on) and
   add a custom MCP server.
2. Paste the URL, set authentication to **OAuth**, and create it.
3. ChatGPT opens a Living Brain page in your browser. Sign in (the usual
   email link), pick the workspace if you belong to several, and
   **Approve**.

A workspace admin may have to allow custom connectors first (Workspace
settings → Apps). They are available on Business, Enterprise and Edu plans;
on Pro, the connector still serves deep research's `search` and `fetch`
reads. Plus and Free plans cannot add custom connectors — a Custom GPT
with Actions (below) is the fallback there.

## What ChatGPT registers as, and what it holds

ChatGPT registers itself: by default it presents a Client ID Metadata
Document (its `client_id` is `https://chatgpt.com/oauth/client.json`); when
a deployment cannot accept that, dynamic client registration is the
fallback. Either way the consent page names it **ChatGPT** as a verified
app, and that is where you pick the workspace.

The connection appears in **Settings → API tokens** as
`ChatGPT connector · <name>`: MCP-only (no settings, token or write
routes), never an admin, an access token that lasts an hour with a
rotating 30-day refresh, and revocable there — revoking disconnects
ChatGPT at once.

## The tools it answers from

Deep research and company knowledge use the pair ChatGPT's research agent
expects, beside the five `brain_*` tools every MCP client gets
([mcp.md](mcp.md)):

| tool | what it does |
|---|---|
| `search(query)` | pages you may read, best match first, as `{results: [{id, title, url}]}` rows to hand to `fetch` |
| `fetch(id)` | one page's Markdown by the `id` a search returned: `{id, title, text, url, metadata}` |

An `id` is `<scope>:<slug>`, and each `url` is the page's address on
`app.livingbrain.wiki`, so a citation in a ChatGPT answer opens the page.
Both read only what you may read: a scope you were not granted is *not
found*, never a refusal.

## Custom GPT with Actions (plans without custom connectors)

There is no public "Living Brain" GPT: a deployment operator creates the
GPT and points its Actions at that deployment.

1. In the GPT's configuration, under Actions, import
   [`openapi/chatgpt-actions.yaml`](openapi/chatgpt-actions.yaml).
2. Set Authentication to **OAuth** with the deployment's predefined
   Actions **Client ID** and **Client Secret**, Authorization URL
   `https://app.livingbrain.wiki/v1/oauth/authorize`, Token URL
   `https://app.livingbrain.wiki/v1/oauth/token`, scope empty. Token
   exchange method **Default (POST request)**; a Basic authorization
   header is also accepted. No PKCE is needed — the client secret stands
   in for it.
3. Copy the callback URL ChatGPT shows
   (`https://chat.openai.com/aip/<g-id>/oauth/callback` or
   `https://chatgpt.com/aip/<g-id>/oauth/callback`) and have the operator
   add it to `OAUTH_ACTIONS_REDIRECT_URIS`.

The operator's side is three secrets and an optional name (see
[deploy.md](deploy.md)): `OAUTH_ACTIONS_CLIENT_ID`,
`OAUTH_ACTIONS_CLIENT_SECRET_SHA256` (the hex SHA-256 of the client
secret, set with `wrangler secret put`) and `OAUTH_ACTIONS_REDIRECT_URIS`
(space-separated), with `OAUTH_ACTIONS_CLIENT_NAME` defaulting to "Living
Brain GPT".

Signing in, picking the workspace and approving happen exactly as the
connector's do. An Actions token is read-only and limited to four routes:
`GET /v1/search`, `GET /v1/pages/{slug}`, `GET /v1/facts` and `POST
/v1/ask` (an ask uses the workspace's connected model).
