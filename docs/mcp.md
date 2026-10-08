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

## Until issue #72 lands

There is no authorization server yet. The bearer value **is** your session
cookie value, verified through `workspaces`' own `caller`, so a user pastes the
cookie into their agent's config. The 401 carries an RFC 9728 challenge
naming `/.well-known/oauth-protected-resource`, which 404s until the OAuth
server lands. #72 replaces that one implementation of `BearerAuth`; the
endpoint and the tools do not change.