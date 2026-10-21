# MCP

One endpoint, seven tools, and every read scoped to the person asking.

**`https://mcp.livingbrain.wiki/v1/pages/mcp`** — Streamable HTTP, POST only,
no trailing slash.

## Connect

```
claude mcp add --transport http livingbrain https://mcp.livingbrain.wiki/v1/pages/mcp
codex mcp add livingbrain --url https://mcp.livingbrain.wiki/v1/pages/mcp
cursor mcp add livingbrain --url https://mcp.livingbrain.wiki/v1/pages/mcp
opencode mcp add livingbrain --url https://mcp.livingbrain.wiki/v1/pages/mcp
```

Then ask it something about a repository. `claude` calls `brain_context_for`
and answers with a cited brief.

Setting up a whole team (workspace, GitHub, Mac sync, connectors, imports)?
The agent skill in [`skills/livingbrain-setup/`](../skills/livingbrain-setup/SKILL.md) walks through it.

## Connect from Claude (connectors)

Claude on the web and Claude Desktop connect to the same endpoint through a
custom connector, with no token to copy:

1. In Claude, open **Settings → Connectors → Add custom connector**.
2. Enter **`https://mcp.livingbrain.wiki/v1/pages/mcp`** and add it.
3. Claude opens a Living Brain page in your browser. Sign in there (the usual
   email link), then **Approve**. The page names the app asking and the
   workspace the connection is for; if you belong to several workspaces, pick
   the one you want from the menu first.
4. Claude returns to the conversation with the connector connected.

An app whose address Living Brain does not recognise as Claude or ChatGPT is
shown on the Approve page as **unverified**, with its address in full, and it
can only reach the MCP tools. A connected app can never change settings,
manage tokens or reach the write routes.

The connection is listed in **Settings → API tokens** as
`Claude connector · <name>` or `ChatGPT connector · <name>` (or
`Connected app · <name>` when unverified). Revoking it there disconnects Claude at once: its
access and its renewal both stop. It also renews itself while Claude uses it,
and ends after 30 days without use.

ChatGPT connects the same endpoint the same way: [chatgpt.md](chatgpt.md).

## Tools

| tool | what it does |
|---|---|
| `brain_search(query?, limit?, include_sessions?)` | pages you may read holding **every** word of the query; if none does, the pages holding enough of its words; if neither, the answer says there is **no confident match**. With no query, the most recently updated pages you may read |
| `brain_page(slug)` | one page's Markdown, by slug |
| `brain_context_for(repo, task, include_sessions?)` | a brief: the sections of the pages that mention the task, cited, under a cap of about 2,000 tokens, with the facts it is about |
| `brain_note(text, title?, facts?)` | writes to your own private memory, redacted first; `facts` are temporal facts the note states ([format](temporal-facts.md)) |
| `brain_facts(subject?, attribute?, as_of?)` | what is true about an entity now, or as of a date, with the history of earlier values ([format](temporal-facts.md)) |
| `search(query)` | `brain_search`'s read in the shape ChatGPT's deep research expects: `{results: [{id, title, url}]}` rows to hand to `fetch` |
| `fetch(id)` | one page's Markdown by a `search` result's `<scope>:<slug>` id: `{id, title, text, url, metadata}` |

Every page a tool quotes comes back as a numbered citation — title, `scope/slug`
ref and URL — and `initialize` tells the model to cite the `[n]` markers.

### How a search matches

A search first looks for pages holding **every** word of the query (stopwords
such as "what" or "the" are ignored, and plurals fold). Only when no page does
does it fall back to pages holding **some** of the words, and only those whose
share of the query's weight is at least half: a word is weighed by how rare it
is in your brain, so a page sharing only a common word with the question is not
returned. When neither finds a page, the answer says there is **no confident
match** (`"match": "none"` in the structured result) rather than the nearest
page. A result's `match` is `all`, `any` or `none`, and its `workspace` names
the workspace it came from.

Coding-agent **session transcripts** (`agent-log-*` pages) are left out of
`brain_search`, `brain_context_for`, `search` and `POST /v1/ask` unless the
caller passes `include_sessions: true` (or `include_sessions=true` on
`GET /v1/search`). A transcript is a long log of one session, not a page a
person wrote, so it does not answer by default.

A brief quotes only the sections of each page that mention the task's words,
packed under the cap; a page with no room is not cited. Pages are never inlined
whole.

## What an agent can see

Whatever its user's `scopes_for(user, Location::Dm, …)` grant — the same rule
Slack gets — and nothing else. No tool takes a scope argument. A page you were
not granted reads as *not found*, not as a refusal, because "you cannot see
this" is itself an answer about somebody else's page. A page scope is
`{shared|channel|user}-{hash of the workspace and the scope}`: a page has no
tenant column, so two workspaces' pages are kept apart by the scope string.

## Logging in

Pass a personal access token from Settings → API tokens:

```
claude mcp add --transport http livingbrain https://mcp.livingbrain.wiki/v1/pages/mcp \
  --header "Authorization: Bearer <token>"
```

Or run `livingbrain login`, which uses the device authorization grant
(RFC 8628): it prints a short code, and you approve it in a browser where the
session cookie you are already signed in with is the proof. The token is
returned once; the API stores only a SHA-256 hash, and a page outside the
token's scope subset reads as *not found*.

A client that reads the metadata finds both halves itself:
`/.well-known/oauth-protected-resource` names this endpoint and the
authorization server, and `/.well-known/oauth-authorization-server` names the
device and token endpoints. The 401's RFC 9728 challenge points at the first.

The browser authorization-code + PKCE flow, with dynamic client registration
(the zero-config OAuth `claude mcp add` and the connectors above use), runs on
this same authorization server; the metadata advertises it beside the
`device_code` grant.
