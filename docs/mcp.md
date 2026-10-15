# MCP

One endpoint, four tools, and every read scoped to the person asking.

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

## Tools

| tool | what it does |
|---|---|
| `brain_search(query, limit?)` | pages you may read holding **every** word of the query |
| `brain_page(slug)` | one page's Markdown, by slug |
| `brain_context_for(repo, task)` | a brief: the pages that mention the task, full text, cited |
| `brain_note(text, title?)` | writes to your own private memory, redacted first |

Every page a tool quotes comes back as a numbered citation — title, `scope/slug`
ref and URL — and `initialize` tells the model to cite the `[n]` markers.

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

**Not implemented yet:** the browser authorization-code + PKCE flow, with
dynamic client registration, that `claude mcp add` uses for zero-config OAuth.
The metadata advertises the `device_code` grant only, so a client that only
speaks the redirect flow will not complete a login on its own.
