# Connect an app (SupportGenius, EarthOS Aura)

A connected app gets **five tools** over HTTP and nothing else. This page is
the setup and the contract; the design is
[ADR 0005](adr/0005-connected-apps.md).

| Tool | What it does |
|---|---|
| `brain_answer_public` | Answers from the pages you published to customers, verbatim, with citations; `null` when nothing covers the question. |
| `brain_memory_recall` | A subject's private memories (newest 20, 500 characters each). |
| `brain_memory_note` | Keeps one thing the subject chose to keep. |
| `brain_memory_forget` | Deletes one memory. |
| `brain_memory_forget_all` | Deletes everything kept for the subject. |

No other route answers to the app's credential: it can't read your shared
pages, search, ask, export or write notes.

## 1. Publish what customers may see

Customer-safe answers come from pages **you publish**, and only from them.
A workspace owner publishes a page of the shared space:

```sh
livingbrain login                       # as a workspace owner
livingbrain public publish refund-policy --url https://example.com/refunds
livingbrain public list
livingbrain public unpublish refund-policy
```

`--url` is where a citation links to (it must be http or https); without it the
citation points at the page in the web app. Publishing copies a cleaned
snapshot (secrets, personal data and instruction-like text are screened out);
edit the internal page and publish again to update. Unpublishing takes effect on
the next question. Personal and channel pages can't be published.

## 2. Connect EarthOS

```sh
livingbrain apps issue earthos
```

This connects the app and prints a credential **once**. Put it in EarthOS's
environment as a secret:

| EarthOS variable | Value |
|---|---|
| `LIVINGBRAIN_URL` | `https://app.livingbrain.wiki` (production), `https://staging-api.livingbrain.wiki` (staging) |
| `LIVINGBRAIN_TOKEN` | the credential `apps issue` printed (`lba_…`) |

EarthOS appends `/v1/tools/<name>` itself, so the URL has no path and no
trailing slash needed. Check it:

```sh
curl -sS -X POST "$LIVINGBRAIN_URL/v1/tools/brain_answer_public" \
  -H "Authorization: Bearer $LIVINGBRAIN_TOKEN" -H 'Content-Type: application/json' \
  -d '{"question":"How long do I have to request a refund?"}'
```

Connect SupportGenius the same way with `livingbrain apps issue supportgenius`.

The credential is issued under the owner who runs the command: if that person
is removed from the workspace, the app's account is disabled with them (hand
it to a long-lived owner, or issue it as one).

### Rotate, revoke, expire

```sh
livingbrain apps list                          # prefixes, last used, never a secret
livingbrain apps issue earthos                 # a second key; switch the app over …
livingbrain apps revoke earthos --key lba_…    # … then revoke the old one
livingbrain apps revoke earthos                # every key, now
livingbrain apps issue earthos --expires-in-days 90
```

A revoked key is refused on its next request.

## 3. Members' data

`subject` is the app's own name for a member (EarthOS sends `user:<id>`). Each
subject's memories are private to that subject, under that app, in your
workspace; another subject, app or workspace reads nothing of them. Memories
are sealed at rest. To answer a data-subject request without the app:

```sh
livingbrain apps export earthos user:42
livingbrain apps forget earthos user:42
```

## The wire contract

`POST {LIVINGBRAIN_URL}/v1/tools/<name>`, JSON body, `Authorization: Bearer
<credential>`. Answers are small (well under 256 KiB). Errors are
`application/problem+json`; `401` for a missing, wrong or revoked credential,
`403` for a tool the credential isn't granted, `422` for an input that doesn't
fit (the text isn't echoed back).

```jsonc
// brain_answer_public
{ "question": "How long do I have to request a refund?" }
{ "answer": "Customers can request a refund within 30 days of purchase.",
  "citations": [{ "title": "Refund policy", "url": "https://example.com/refunds", "scope": "public" }] }
{ "answer": null, "citations": [] }              // nothing covers it

// brain_memory_recall
{ "subject": "user:42" }
{ "memories": [{ "text": "Likes sunrise walks", "id": "mem_01J…", "created_at": "2026-10-20T…" }] }

// brain_memory_note — text up to 2,000 characters; source is the conversation id
{ "subject": "user:42", "text": "Allergic to peanuts", "source": "conv_123" }
{ "id": "mem_01J…" }

// brain_memory_forget / brain_memory_forget_all
{ "subject": "user:42", "id": "mem_01J…" }       { "forgotten": 1 }
{ "subject": "user:42" }                          { "forgotten": 3 }
```

A note that looks like an instruction to a model is refused (`422`); a secret or
personal data in a note is redacted before it is stored.
