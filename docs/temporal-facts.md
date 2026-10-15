# Temporal facts: how the brain remembers change over time

**Status: format v1, stable.** Compilers, extractors and agents that write to
the brain emit facts in exactly this shape. Additive changes only; anything
that would change the meaning of an existing field gets a new `format`
number.

A page says what something *is*. A fact says what was true *when*. The brain
keeps both, and never answers "what is my shoe size?" by overwriting the old
answer: it keeps the old value with the dates it was true, and says which one
holds now.

```text
2025  note: "my shoe size is 40"      -> fact A: shoe_size(person/nick) = 40 EU, valid 2025..
2026  note: "my shoe size is now 41"  -> fact B: shoe_size(person/nick) = 41 EU, valid 2026..
                                         fact A is closed: valid 2025..2026, superseded by B
ask "what's my shoe size?"            -> 41 EU (was 40 EU 2025..2026)
ask as_of 2025                        -> 40 EU
```

The model is bi-temporal, the way Graphiti/Zep and most temporal knowledge
graphs are: **valid time** (when the claim is true in the world) is kept
apart from **observation time** (when the brain learned it). Both are on
every fact, and neither is ever rewritten to mean the other.

## 1. A fact

| Field | Type | Who sets it | Meaning |
| :--- | :--- | :--- | :--- |
| `id` | string | server | `fact-<ULID>`. Stable; everything cites by it. |
| `subject` | string | writer | The entity, as `<entity_type>/<slug>`: `person/nick`, `project/checkout`, `system/postgres`. `entity_type` is one of the page entity types (`person`, `project`, `decision`, `customer`, `system`, `glossary`, `radar`); `slug` follows the page slug rule (lowercase ascii letters, digits, `-`, 1..=128). The subject is the same key the entity's page has, so a page and its facts meet without a lookup table. `me` is accepted on write and resolves to the writer's own person subject (`person/<user id, slugified>`). |
| `attribute` | string | writer | The predicate, normalised: lowercase `snake_case`, `[a-z0-9_]`, 1..=64 chars, e.g. `shoe_size`, `employer`, `uses_tool`, `lives_in`. The server normalises `Shoe Size` and `shoe-size` to `shoe_size`; emitters should send the normal form. |
| `value` | string | writer | The object, as text: `41`, `Acme GmbH`, `ripgrep`. Not interpreted. A number stays a string so `41` and `41.0` are the writer's choice, not a float's. |
| `unit` | string? | writer | The unit or system of the value, when it has one: `EU`, `kg`, `EUR/month`. Part of the value's identity: `41 EU` and `41 US` are different values. |
| `valid_from` | partial date? | writer | When it became true in the world. Omitted means "not known": the server uses the observation time (precision `second`). |
| `valid_to` | partial date? | writer, then server | When it stopped being true, **exclusive**. Omitted and nothing later = still true (open-ended, rendered `now`). The server sets it when a later fact supersedes this one. |
| `observed_at` | RFC 3339 | writer or server | When the brain learned it — normally the time of the source message. Defaults to the time of the write. Never in the future. |
| `source` | string? | writer | The citation: a page (`page:<scope>/<slug>`), a source (`source:<id>` or `source:<id>#<message-id>`), a note (`note:<scope>/<slug>`), a conversation (`conversation:<id>#<message-id>`). Notes fill it in themselves. |
| `quote` | string? | writer | The exact words the fact was read from, redacted. What makes a fact arguable. |
| `confidence` | number? | writer | 0..=1, the writer's belief that the extraction is right. Default `1.0` for a person's own write, the compiler's number otherwise. |
| `cardinality` | `one` \| `many`? | writer or registry | Whether the attribute holds one value at a time (`shoe_size`, `employer`) or several (`uses_tool`, `speaks`). See §3. |
| `status` | enum | server | `current`, `superseded`, `retracted` or `disputed`. See §2. |
| `supersedes` | fact id? | server | The fact this one replaced. |
| `superseded_by` | fact id? | server | The fact that replaced this one. |
| `scope` | `personal` \| `shared` \| `channel` | server | Which memory it is in. Facts are scoped exactly like pages (§6). |

### Partial dates

A date is as precise as the writer knows, and says so:

| Written | Precision | Starts at (UTC) |
| :--- | :--- | :--- |
| `2026` | `year` | `2026-01-01T00:00:00Z` |
| `2026-10` | `month` | `2026-10-01T00:00:00Z` |
| `2026-10-15` | `day` | `2026-10-15T00:00:00Z` |
| `2026-10-15T09:12:00Z` | `second` | that instant |

The written form is stored and shown as written; the start instant is what
sorts and compares. A range `[valid_from, valid_to)` runs from the start of
`valid_from` to the start of `valid_to`, so `2025..2026` is all of 2025.

## 2. Status and supersession

Status is about the **belief**; the validity range is about the **world**.
A fact can be `current` (nobody has replaced it) and still not valid today
(its `valid_to` is in the past).

| Status | Meaning |
| :--- | :--- |
| `current` | The latest belief for its subject+attribute (or, for a `many` attribute, for that value). |
| `superseded` | A later fact replaced it. Its `valid_to` is the successor's `valid_from` (or its own stated `valid_to`, if earlier), and `superseded_by` names the successor. Kept forever: it is the history. |
| `retracted` | Somebody said it was never true (`POST /v1/facts/{id}/retract`). Not used to answer; shown in history struck through. |
| `disputed` | Two facts claim different values for overlapping validity and nothing orders them. Both are kept, both are shown, neither is preferred; a person settles it by retracting one. |

**New information never overwrites.** For a `one` attribute, every write
re-resolves that subject+attribute's timeline in its scope, deterministically:

1. Retracted facts take no part.
2. A fact whose value+unit equals a live fact that is valid at the new
   fact's `valid_from` is a **corroboration**: no new fact is stored, the
   write answers with the existing one (`outcome: "corroborated"`).
3. The rest are ordered by the start of `valid_from`.
4. Facts with the **same** `valid_from` start and different values are
   `disputed` — nothing says which came first.
5. Each fact's effective `valid_to` is the earlier of its own stated
   `valid_to` and the next fact's `valid_from`; it is `superseded` by that
   next fact, which `supersedes` it.
6. A stated `valid_to` that runs **past** the next fact's start, on a fact
   observed **after** that next fact, is a conflict: the newer observation
   says the old value still held. Both are `disputed`.
7. The last fact in the order is `current`.

Backfilling works the same way: learning today that the shoe size was 39 in
2024 inserts a fact that ends where the 40 begins, `superseded` by it,
without touching the current 41.

## 3. Cardinality

A `one` attribute has one value at a time; a new value ends the old one. A
`many` attribute (`uses_tool`, `speaks`, `member_of`) accumulates: each
distinct value has its own timeline, a new value ends nothing, and a value
ends only by a stated `valid_to` or a retraction.

The cardinality is a property of the attribute, decided by the first of:

1. the `cardinality` stored on the attribute's existing facts in that scope
   (a write that disagrees is refused with `422 facts/cardinality-mismatch`);
2. the `cardinality` field of the write;
3. the built-in registry: `many` for `uses_tool`, `uses`, `speaks`,
   `knows`, `member_of`, `works_on`, `owns`, `skill`, `skills`, `likes`,
   `dislikes`, `tag`, `tags`, `alias`, `aliases`, `depends_on`, `reports`,
   `contributor`, `contributors`; `one` for everything else.

## 4. Writing facts

### JSON an emitter sends (`format: 1`)

```json
{
  "format": 1,
  "facts": [
    {
      "subject": "person/nick",
      "attribute": "shoe_size",
      "value": "41",
      "unit": "EU",
      "valid_from": "2026",
      "observed_at": "2026-10-15T09:12:00Z",
      "source": "conversation:c-123#m-9",
      "quote": "my shoe size is now 41",
      "confidence": 0.9
    }
  ],
  "shared": false
}
```

JSON Schema for one fact (the array is `facts`, at most 50 per request):

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "$id": "https://livingbrain.wiki/schemas/temporal-fact.v1.json",
  "type": "object",
  "required": ["subject", "attribute", "value"],
  "additionalProperties": false,
  "properties": {
    "subject":     { "type": "string", "pattern": "^(me|(person|project|decision|customer|system|glossary|radar)/[a-z0-9-]{1,128})$" },
    "attribute":   { "type": "string", "maxLength": 64 },
    "value":       { "type": "string", "minLength": 1, "maxLength": 2000 },
    "unit":        { "type": "string", "maxLength": 32 },
    "valid_from":  { "type": "string", "pattern": "^\\d{4}(-\\d{2}(-\\d{2}(T\\d{2}:\\d{2}:\\d{2}(\\.\\d+)?(Z|[+-]\\d{2}:\\d{2}))?)?)?$" },
    "valid_to":    { "type": "string", "pattern": "^\\d{4}(-\\d{2}(-\\d{2}(T\\d{2}:\\d{2}:\\d{2}(\\.\\d+)?(Z|[+-]\\d{2}:\\d{2}))?)?)?$" },
    "observed_at": { "type": "string", "format": "date-time" },
    "source":      { "type": "string", "maxLength": 512 },
    "quote":       { "type": "string", "maxLength": 2000 },
    "confidence":  { "type": "number", "minimum": 0, "maximum": 1 },
    "cardinality": { "enum": ["one", "many"] }
  }
}
```

Rules for a compiler or a model emitting facts:

- **One fact per claim.** "Nick moved to Berlin and wears 41" is two facts.
- **Resolve the subject** to the entity's page subject; use `me` only when
  writing for the person who is speaking.
- **Normalise the attribute** to a short stable `snake_case` key. Prefer an
  existing attribute over a near-synonym (`employer`, not `works_at` and
  `company` on alternate days). Units go in `unit`, not in `value`.
- **Date it as precisely as the text does**, and no more: "this year" in a
  2026 message is `2026`, "since October" is `2026-10`, "on the 15th" is
  `2026-10-15`. Do not invent a day. Omit `valid_from` when the text gives
  no time; never put the message time in `valid_from` unless the text says
  the change happened then ("I just switched to...").
- **`observed_at` is the message's time**, not the extraction run's.
- **Do not resolve conflicts yourself.** Emit what the source says; the
  server orders, supersedes and disputes. Never emit `status`, `supersedes`
  or `superseded_by`.
- **Quote** the sentence and **cite** the message. A fact with no source is
  allowed but ranks below one with a source.
- Redaction runs on every text field before storage; secrets do not belong
  in a fact.

### Routes

- `POST /v1/facts` — body as above; bearer, or the web app's session cookie
  (a cookie write must pass the same-origin check, like every cookie write).
  Answers `201` with `{ "facts": [ { "id", "outcome": "created" | "corroborated", "fact": {...} } ] }`.
- `POST /v1/facts/{id}/retract` — marks one fact `retracted` and re-resolves
  its timeline. Same auth.
- `GET /v1/facts?subject=&attribute=&as_of=` — see §5.
- `POST /v1/notes` accepts an optional `facts` array (same shape, no
  `format`); each fact's `source` defaults to the note (`note:<scope>/<slug>`)
  and its `quote` to nothing.
- The MCP `brain_note` tool accepts the same `facts` array; `brain_facts`
  reads them (§5).

Facts land in the writer's **personal** scope unless `shared: true`, which
writes to the workspace's shared memory and needs a credential that may read
it.

## 5. Reading facts

`GET /v1/facts?subject=person/nick&attribute=shoe_size&as_of=2025`

- `subject` and `attribute` are optional filters (`subject=me` works);
  without either the answer is every fact the asker may read, newest first,
  capped at 500.
- `as_of` (a partial date, default **now**) chooses which facts count as
  true: with a partial date it means **the last instant of that period** —
  `as_of=2025` is "as of the end of 2025".

```json
{
  "as_of": "2025",
  "as_of_instant": "2025-12-31T23:59:59Z",
  "keys": [
    {
      "subject": "person/nick",
      "attribute": "shoe_size",
      "cardinality": "one",
      "current": [ { "id": "fact-01...", "value": "40", "unit": "EU", "valid_from": "2025", "valid_to": "2026", "status": "superseded", ... } ],
      "history": [ { ...41..., "status": "current" }, { ...40... } ],
      "line": "shoe_size(person/nick) = 40 EU [valid 2025..2026, observed 2025-03-02, source note:user-…/note-…] (later 41 EU 2026..now)"
    }
  ]
}
```

In `GET /v1/facts` a line cites each fact by its own `source`; in a brief
(`ask`, `brain_context_for`, `brain_facts`) sources are numbered `F1`, `F2`,
… and listed in the answer's `citations` as `{title: "F1", source, quote}`.

`current` is **the facts valid at `as_of`** (for a `one` attribute: at most
one, unless disputed — then all disputed ones), whatever their status; a
newer observation wins when two could answer. `history` is every fact for
the key, newest `valid_from` first, retracted ones included.

`/v1/search` answers a `facts` array beside `results` (the timelines `q` is
about, as of now), `/v1/ask` puts the fact lines at the top of its brief and
answers `facts` and `as_of` beside `answer`, and the page view
(`GET /v1/pages/{slug}`) carries `subject`, `facts` and `facts_markdown`.
Which timelines a question is about is decided on the plaintext index alone
(attribute words, subject slug, `my`/`me` for the asker's own person) before
anything is decrypted. `ask` takes an `as_of` field (`livingbrain ask --as-of 2025 …`
on the CLI). Without one, a question that names a time — "in 2025", "as of
2025-06", "in June 2025", "last month" — is asked as of the end of that time.

## 6. Scope, privacy, storage

Facts are stored in the `facts` table of the pages module (migration
`0006_facts`), under the same page scopes as pages: `user-…` (personal),
`channel-…`, `shared-…`, each folded with the workspace id, so two
workspaces never share a row. A read only ever names the scopes the
credential grants (`page_scopes`), and no route accepts a scope from the
client; a narrowed token reads only its narrowed scopes.

The `value`, `unit`, `quote` and `source` are **sealed** with the scope's
data key (XChaCha20-Poly1305, the same envelope as page bodies, AAD bound to
the scope, the fact id and the key version) and stored as base64 in the row.
The `subject` and `attribute` are plaintext, like page slugs, so a timeline
can be selected without decrypting. Forgetting a scope deletes its fact rows
and destroys the key; key rotation re-seals them with the bodies.

## 7. Rendering

### On an entity page (Markdown, deterministic)

The page for `person/nick` shows its facts below the body, generated — never
stored in the body, so a page edit cannot desynchronise them:

```markdown
## Facts

### Current

| Attribute | Value | Valid | Observed | Source |
| :--- | :--- | :--- | :--- | :--- |
| shoe_size | 41 EU | 2026 – now | 2026-10-15 | note:user-3f…/note-91c… |

### History

- **2026 – now** · shoe_size = 41 EU · observed 2026-10-15 · current
- **2025 – 2026** · shoe_size = 40 EU · observed 2025-03-02 · superseded by fact-01J…
```

Order: Current sorted by attribute; History newest `valid_from` first, then
newest `observed_at`. Retracted entries render as `~~…~~ · retracted`.

### For a model (one line per subject+attribute)

```text
shoe_size(person/nick) = 41 EU [valid 2026..now, observed 2026-10-15, source F1] (was 40 EU 2025..2026)
```

- `attribute(subject) = value unit` — the answer as of the asked time.
- `[valid FROM..TO, observed DATE, source Fn]` — `TO` is `now` when open;
  `Fn` indexes the brief's fact citations.
- `(was …)` — earlier values, newest first, each with its range; `(later
  …)` — values after `as_of` when answering historically.
- `{disputed: A | B}` replaces the value when the timeline is disputed.
- `many` attributes list their values valid at that time: `uses_tool(person/nick) = {ripgrep, jj} [...]`.

The brief that `ask` builds starts with this convention, so the model reads
it as an instruction:

> Facts below are temporal. Each line is `attribute(subject) = value
> [valid FROM..TO, observed DATE, source]`. Prefer the value valid now (or at
> the time the question asks about). When a value changed and the change is
> relevant, say so ("41, was 40 until 2026"). Treat `disputed` as unsettled
> and say so. Cite the source.

### In the web app

A page whose subject has facts shows a **Facts** card under the body — the
Current table and the History list, with an "As of" date picker that re-reads
`GET /v1/facts?subject=…&as_of=…`. `wiki.html?view=facts` lists every
timeline the reader may see, with the same picker. Both are built from DOM
nodes and `textContent` only.

### Decision and learning pages

A decision page may carry `decided_at`, `valid_from`, `valid_to`,
`supersedes` and `superseded_by` in its frontmatter (partial dates for the
first three, page slugs for the last two), so a decision that was reversed
reads as history, not as a contradiction.

## 8. The shoe-size example, end to end

(`person/u0nick` is the asker's own subject: `me` resolves to `person/` plus
the user id, lowercased, with anything outside `[a-z0-9-]` turned into `-`.)

1. 2025-03-02, the note `POST /v1/notes` `{"body": "my shoe size is 40",
   "facts": [{"subject": "me", "attribute": "shoe_size", "value": "40",
   "unit": "EU", "valid_from": "2025"}]}` stores fact A:
   `shoe_size(person/u0nick) = 40 EU`, valid `2025..`, `current`,
   source `note:user-…/note-…`.
2. 2026-10-15, the note `{"body": "my shoe size is now 41", "facts":
   [{"subject": "me", "attribute": "shoe_size", "value": "41", "unit": "EU",
   "valid_from": "2026"}]}` stores fact B, valid `2026..`, `current`,
   `supersedes: A`; fact A becomes valid `2025..2026`, `superseded`,
   `superseded_by: B`.
3. `ask "what's my shoe size?"` — the brief carries
   `shoe_size(person/u0nick) = 41 EU [valid 2026..now, observed 2026-10-15, source F1] (was 40 EU 2025..2026)`,
   so the answer is "41 EU (it was 40 until 2026)".
4. `ask {"question": "what's my shoe size?", "as_of": "2025"}` (or "what
   was my shoe size in 2025?") — the brief carries
   `shoe_size(person/u0nick) = 40 EU [valid 2025..2026, observed 2025-03-02, source F1] (later 41 EU 2026..now)`,
   so the answer is 40 EU.
