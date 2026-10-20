# 0005 — Connected apps: the public scope, app memories and app tools

Status: Accepted (owner decisions of 2026-10-20). Implements issues #51
(SupportGenius: customer-safe answers) and #146 (EarthOS Aura: per-member
memory and public answers) together, because they are one contract: five
tools over `POST /v1/tools/<name>`.

## Context

Two sister ventures want the brain without ever seeing internal memory:

- **SupportGenius** answers customers' questions and needs the company's
  *customer-safe* knowledge, cited.
- **EarthOS Aura** answers its members and needs that same public knowledge,
  plus a private memory per member that the member can read, add to and erase.

Both already ship a client (`lib/aura/livingbrain.ts` in EarthOS): `POST
{base}/v1/tools/<name>` with a JSON body and `Authorization: Bearer
<scoped credential>`, a 3 second timeout, responses capped at 256 KiB. What
existed here (ADR 0001-0004): workspaces, per-scope encryption (ADR 0003),
agent accounts with `lba_` keys and grants (ADR 0004), the audit log, the page
store, facts. What did not: a customer-safe scope, any way to publish into it,
the `/v1/tools` routes, or a credential for an *app* rather than a person.

## Decision

### 1. Tenancy: an app is a connection inside a workspace

Each connected app is a **connection** in one workspace. An app's end members
("subjects") are not workspace members; they map to `Scope::User` with the id
`<app>:<subject>` (EarthOS sends `user:<id>`, so its scope id is
`earthos:user:<id>`). The page scope is `page_scope(workspace, scope)`, a hash
that folds the workspace, the kind and the id together, so:

- the same subject string under another app is another scope;
- the same app and subject in another workspace is another scope;
- an app name cannot contain `:` (`[a-z][a-z0-9-]{1,31}`), so `<app>:<subject>`
  always splits at its first colon and one app's prefix can never be spelled
  by another's subject;
- nothing a client sends selects a scope by itself: the app comes from the
  **credential's grants**, never from the request.

### 2. The `public` scope

`Scope::Public` (`public`) is a new scope class beside `shared`, `channel:` and
`user:`. The access model never grants it: `scopes_for` cannot return it and
the token vocabulary refuses it, so no ordinary read, search, ask, export or
MCP call can reach a page in it. Only the public-answer path names it.

A page becomes public **only** through an explicit publish by a workspace
**owner** (their whole credential — not a narrowed token, an admin or an
agent), audited as `page.publish` / `page.unpublish`:

- the source is always a page of the `shared` space; personal and channel
  pages cannot be named, and a session log is refused;
- the page is passed through the same inbound screen every note passes (hidden
  text, injection payloads, secrets and personal data, #50) and the *cleaned*
  text is copied into the workspace's public scope — a **snapshot**: editing
  the internal page publishes nothing until the owner publishes again;
- the `public_pages` ledger is the authority: a page is public while it has a
  row (who, when, which version, the title and the link a citation shows).
  Unpublishing deletes the row and takes effect on the next question.

### 3. `brain_answer_public` is extractive

No model is called. The question is looked up in the public scope (the same
blind-index search every read uses), the passages of the best-matching
published pages are scored against the question's terms (the page title and the
passage's heading count, but a passage must itself contain a term), and the
top passage(s) are returned verbatim, at most two and 1,200 characters, with
`citations: [{title, url, scope: "public"}]`. A passage must cover at least 60%
of the question's terms; below that the answer is `null` with no citations —
"I don't know" is a feature. The cost is a handful of reads and no network
call, so it is far inside the client's 3 second timeout.

### 4. The credential

A connected app is an **agent account** (ADR 0004) named `app:<name>`, owned by
the owner who connected it, with five *allow* grants of kind `tools` in the
space `app:<name>` — one per tool — and **nothing else**: no page, source or
fact grant. Its key is an ordinary `lba_…` key, hashed at rest, shown once.

- Authorization is checked on every call by reading the grants: the account
  must hold tool grants for exactly one `app:` space, covering the tool asked.
  Two apps on one credential, an unknown tool, no grant, or a key cut to a
  scope subset are all refused (fail closed).
- Revoking is immediate: a revoked key, a disabled account, deleted grants or a
  removed owner is a 401/403 on the next request.
- Because it holds no page grant, the same key reaches nothing on `/v1/pages`,
  `/v1/search`, `/v1/notes`, `/v1/export`, `/v1/facts` or MCP.

Issue and revoke are owner routes with a CLI (see "Operating it"). A PAT from
`livingbrain login`, un-narrowed, of an owner, is enough; the key is returned
by the issuing call only. (ADR 0004 says agent keys are minted from a session.
This is the one deliberate exception: the CLI has no cookie, and the owner's
whole-credential PAT is the same person with the same authority.)

### 5. The tools

All `POST /v1/tools/<name>`, JSON in and out, every answer far below 256 KiB:

| Tool | Input | Answer |
|---|---|---|
| `brain_answer_public` | `{question}` (≤ 1,000 chars) | `{answer \| null, citations: [{title, url, scope: "public"}]}` |
| `brain_memory_recall` | `{subject}` | `{memories: [{text, id, created_at}]}`, newest 20, 500 chars each |
| `brain_memory_note` | `{subject, text, source}` | `{id}` |
| `brain_memory_forget` | `{subject, id}` | `{forgotten: n}` |
| `brain_memory_forget_all` | `{subject}` | `{forgotten: n}` |

`id` and `created_at` in a recall are additions to the contract the client
already reads (`{memories: [{text}]}`); they are what a memory page needs to
call `brain_memory_forget`.

A memory is a **row**, not a page (`app_memories`): a page is versioned and a
version is forever, and the whole point of forgetting is that it is not. The
text and its source label are sealed under the subject's scope key (ADR 0003),
AAD-bound to the scope, the id and the key version, and move with a key
rotation. A note passes the inbound screen first (an instruction to a model is
refused with a 422 that does not echo it; a secret or personal data is
redacted). Limits: 2,000 characters a note (a recall returns the first 500), a
source label of 200, 1,000 memories a subject.

The module is `app-tools`, served at `/v1/tools/…`: the harness already has a
`tools` module (the MCP client, which has no routes yet) and mounts a module at
`/v1/<name>`, so the Worker re-addresses the one path before routing.

Every authorized call is one audit line (`tool.<name>`, the app's account as
actor, `<app>:<subject>` as resource); a refused one is `tool.denied`. The
memory text is never written to a log.

### 6. Privacy: export and erasure

- `brain_memory_forget` is a `DELETE` of one row. `brain_memory_forget_all`
  deletes every memory of the subject and anything derived into the same scope
  (temporal facts and the tidy passes over them), in one atomic batch.
- **`forget_all` does not shred the scope key**, though `PageStore::forget_scope`
  would: a shredded scope can never be written again, and a member who clears
  their memory must be able to start a new one. With the rows gone nothing
  sealed under the key is left to read. (D1 Time Travel keeps the rows until it
  expires, as ADR 0003 says for everything.)
- A **workspace erasure** (ADR 0004, "Deleting a workspace") does shred: it
  finds every subject scope by the rows' own `workspace_id` and shreds each,
  plus the public scope, with the member spaces.
- The owner can **export** a subject (`GET /v1/apps/<app>/subjects/<subject>`)
  and **erase** one (`DELETE …`), both audited, to answer a data-subject
  request without going through the app.

## Not in this change

#51 also proposed `brain_route` (who owns a topic), `brain_note_ticket`
(resolved tickets back into an internal `support` scope), and a
"label on the page in git" way to publish. The owner's decision for this change
is the five tools above, so those remain open on #51. Publishing is by the
owner's explicit action only.

## Operating it

```text
livingbrain login                          # as a workspace owner
livingbrain public publish <slug> --url <https://… the page's public address>
livingbrain apps issue earthos             # prints the credential once
livingbrain apps list
livingbrain apps revoke earthos            # all keys, immediately
livingbrain apps revoke earthos --key lba_… # one key
livingbrain apps export earthos user:42
livingbrain apps forget earthos user:42
```

See [docs/connect-apps.md](../connect-apps.md) for the client side.

## Threat model

| Threat | Control |
|---|---|
| A customer-facing app reads an internal page | `public` is never in `scopes_for` or the token vocabulary; the answer path reads only the public scope, and only slugs with a ledger row; a page enters it only by an owner's publish from the shared space; the test seeds internal, personal and channel pages and asserts none is returned or paraphrased. |
| A secret or injection payload in an internal page reaches customers | Publishing runs the inbound screen and copies the cleaned text. |
| An app reads another app's, or another workspace's, subject | The scope is derived from the credential's app and workspace; the client sends neither. |
| A subject reads another subject | The subject is hashed into the scope; a table of near-miss spellings returns nothing. |
| A leaked app key does more than five tools | It holds no page, source or fact grant; the tools check their grants on every call; revoke is immediate. |
| A note carries an instruction to a model | The inbound screen refuses it (422) before anything is stored; a note never triggers a tool. |
| Memory text in logs | Not written: the audit line has the tool and `<app>:<subject>`; errors never echo input. |
| Erasure leaves text behind | Hard delete, plus the derived tables of the scope; a workspace erasure shreds the key. |
