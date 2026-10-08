# Telemetry: the live map, the event log, `x-trace-id`, OTLP

[`livingbrain-telemetry`](../crates/livingbrain-telemetry) is three things that
share one idea: **what leaves is a closed set of types, and the types are the
promise**. No constructor in the crate takes content as input, and there is no
field anywhere that can hold it. (The string fields on `Event` are `pub` and
validated rather than sealed — see
[The two guarantees](#the-two-guarantees) — so "nothing takes content as input"
is a statement about the API a caller is meant to use, and the id validators are
what back it.)

| Piece | Default | Leaves the machine? |
| --- | --- | --- |
| Anonymous counted usage ([usage-data.md](usage-data.md)) | **on** (`Telemetry::default()` is `On`) | intended: yes, once per on-period. **Nothing in this repository sends it.** |
| The live map of installs | **off** (`LiveMap::disabled()`; `LiveMap::default()` is disabled) | only after you switch it on — and no sender exists here |
| The structured event log | **no writer**: a format and a validated type, nothing writes it | **no** — it would stay local |

The first is documented on its own page because it is the only one whose
preference defaults to on. This page covers the other two plus the trace id and
the operational half. [logging.md](logging.md) covers operating all three, and
holds the list of what is not built yet.

Read the third row carefully: the event log is not a stream that is switched on.
It is a versioned line format and the Rust type that produces it. Nothing in
this repository constructs an `Event` outside tests, and `Event::to_json_line()`
has no non-test caller, so no such file exists to tail.

## The live map

**Off until someone switches it on.** A disabled instance emits nothing at all:
no id exists, there is nothing to send and there is nothing to forget.
`LiveMap::disabled()` is the whole of an install that never opted in, and
`LiveMap::heartbeat` — the only send path — returns `None`. A caller that
retries does not start sending.

### Turning it on

`LiveMap::enable` mints nothing itself: the id is the caller's, because this
crate holds no RNG and no id source. The caller passes one, and it is validated
exactly like a usage id — sixteen lowercase characters of `[a-z0-9]` — because
the threat is the same: an id that carried anything derived from the machine or
its owner would turn a pin on a map into a fingerprint. An `InstallId` is not a
workspace id, a page id, a Slack team id or a user, and it is dropped rather than
kept "in case it is wanted again", because a kept id is a kept position on a map.

Re-enabling is refused with `AlreadyEnabled`. Re-enabling with a second id would
leave the first one on the map forever with no way to send the `online: false`
beat that retires it, so it is refused rather than papered over.

### The beat

`Heartbeat` has five fields and no more:

| Key | JSON type | Meaning |
| --- | --- | --- |
| `install_id` | string | The random install id, 16 lowercase characters |
| `version` | string | The sender's own version (validated) |
| `platform` | string | `linux`, `macos`, `windows`, `wasm`, `other` |
| `pages` | integer | Page count as a bucket: `0` for 0–9, `1` for 10–99, `2` for 100 or more |
| `online` | boolean | Whether this install is currently on the map |

The first beat and the periodic beats are the same shape. Real output:

```json
{"install_id":"k3m9q7w2x5b8n4c1","version":"0.1.0","platform":"linux","pages":2,"online":true}
```

When to beat is the caller's scheduling decision; `LiveMap` has no interval and
no last-sent timestamp, and the answer to "is it on" does not depend on whether
a beat happened to fire. `pages` is a bucket rather than a count, the top bucket
is open-ended rather than stopping at some ceiling — a hard ceiling would only be
a place for an off-by-one to leak an exact count — and `PagesBucket::of` is the
only way to make one, so the count a caller measured cannot reach the wire from
anywhere else.

Beats go to `LIVE_MAP_ENDPOINT`, a constant rather than a setting, as with the
usage endpoint:

```
https://telemetry.livingbrain.wiki/v1/telemetry/heartbeat
```

As with everything in this crate, the transport is the caller's: there is no HTTP
client here.

### Turning it off

`LiveMap::disable` returns the one `online: false` beat that must reach the map
so an install is not left showing as permanently online, and then **forgets the
id**. The id is not kept in a file for later: switching it back on mints a new
one, so a forgotten id cannot be resumed and cannot be reconstructed from
elsewhere. Switching off something that was never switched on sends **nothing at
all** — there is nothing to take off the map, and sending an empty beat would be
inventing traffic.

The three descriptive fields of the retraction beat are neutral placeholders:
`version` is `"0.0.0"`, `platform` is `"other"`, `pages` is `0`. `disable` takes
no version, platform or page count to put in them, and that is deliberate: a
retraction is not a description of an install, and a beat that claimed a version
on the way out would be a second claim about an install that is leaving the map.
Real output:

```json
{"install_id":"k3m9q7w2x5b8n4c1","version":"0.0.0","platform":"other","pages":0,"online":false}
```

`0.0.0` rather than `0` because `Version`'s grammar is `major.minor.patch` and
`0` is not a version under it. The value has to pass the same validator as any
other version, so it is a `const` that a compile-time assertion checks against
the grammar: if the grammar is ever tightened under this constant, the crate
stops compiling rather than becoming a `disable()` that quietly sends nothing.

### The CLI switch is read-only for the map

`telemetry on` and `telemetry off` are switches for **anonymous usage data only**:
they write exactly one key, `telemetry.usage`. `status` shows the live map, and
only shows it:

```
anonymous usage data: on  (stored on, LIVINGBRAIN_TELEMETRY unset)
live map:              off — no install id exists, so nothing is sent
sent to:               https://telemetry.livingbrain.wiki/v1/telemetry/events
```

The `live map:` line reads `opted in — no heartbeat from this command, which
sends nothing` when `telemetry.live-map` is stored `on`. `status` never writes
that key, and there is no `telemetry live-map on` flag to imply a switch that is
not wired. The write side of the opt-in belongs to whoever mints the install id
and sends the beat: an id created for a status display would have to be kept, and
a kept id is a kept position on a map. The same is true in `--json`, where
`live_map.sends_heartbeat` is `false` because `LiveMap::heartbeat` is asked and
returns `None` — that is the crate's answer, not an assumption.

Where the map's setting would live, and the full command surface, are on
[usage-data.md](usage-data.md#where-the-preference-is-stored).

### The 25 km cell is derived server-side

**The client never sends a coordinate.** The 25 km cell is worked out by the
server from the address the request came from, which means the coarse location
is derived from something the operator's network already knows and the client
never volunteers. A client that already knows where it is does not get to say so.

`Heartbeat` deliberately has no latitude and longitude field, and it must not
gain one. It does not have a `cell` field either: naming one on the wire would
imply the client computed it. The reasoning is that a coordinate is an address
and an address is a person, and adding the field would be the single change that
breaks the promise this feature is made of — a coordinate in the payload would be
free text in a struct that promises to carry none.

The guarantee is asserted by shape from outside the crate:
`tests/live_map.rs` checks the serialised key set is exactly
`["install_id", "online", "pages", "platform", "version"]` and asserts that
`lat`, `lon`, `latitude`, `longitude`, `geo`, `cell`, `ip`, `address`, `host`
and `hostname` are absent. Checked by key, not by substring: `lat` is inside
`platform`, and a substring check that fires on a real field teaches the next
reader to ignore it.

## The structured event log

**A format and a validated type, ready to be written — not yet wired to a
writer.** One JSON line per brain turn, nightly-evolve change, MCP call and
colony hand-off, plus `operational` for work that is not about anyone's content.
Nothing in this repository produces those lines outside its tests: there is no
sink, no file appender and no rotation, and `Event::to_json_line()` has zero
non-test callers. What exists is the contract they would be written to.

The lines are versioned against [`docs/brain-events.schema.json`](brain-events.schema.json)
(`EVENT_SCHEMA_VERSION`, currently `1`) — adding a kind or a field is a bump of
that number, not an edit in place, because a log that outlives a release is read
by something that was not built with it.

`Event::to_json_line` is the only way to get a line out, and `Event::schema`
describes the same fields to a reader that a schema file or a dashboard would
want. Both are generated from the same types that serialise the data, so the
schema cannot drift from the lines; `docs/brain-events.schema.json` is checked
against it.

| Key | JSON type | Meaning | Allowed values |
| --- | --- | --- | --- |
| `schema_version` | integer | The event schema version this build writes | `1` |
| `trace_id` | string | Opaque id carried across Slack, Worker, MCP and CLI | non-empty, ≤128 chars, `[A-Za-z0-9._:-]` |
| `kind` | string | What happened | `brain_turn`, `nightly_evolve`, `mcp_call`, `colony_handoff`, `operational` |
| `at_unix` | integer | When, in seconds since the Unix epoch | — |
| `outcome` | string | Whether it worked, and if it was attempted at all | `success`, `failure`, `skipped` |
| `duration_ms` | integer | How long it took, in milliseconds. **Not bucketed**, unlike the usage payload — this line stays on the machine that wrote it and is read by whoever is debugging that machine, so bucketing here would cost the reader the thing they opened the log for. | — |
| `subject` | object or null | `{ kind, id }` — what it was about, by opaque id. Never the content. | `kind` is one of `page`, `message`, `workspace`, `tool` |
| `error_kind` | string or null | How it failed | `config`, `upstream`, `timeout`, `decode`, `permission`, `internal` |
| `tool` | string or null | The MCP tool called | `brain_search`, `brain_page`, `brain_note` |
| `colonizer_colony` | string or null | Opaque colony id for a hand-off | non-empty, ≤128 chars, `[A-Za-z0-9._:-]` |

Four real lines, produced by running the crate:

```json
{"schema_version":1,"trace_id":"01JABCDEF.123-4","kind":"mcp_call","at_unix":1700000000,"outcome":"success","duration_ms":88,"subject":{"kind":"page","id":"01JABCDEF.123-4"},"error_kind":null,"tool":"brain_search","colonizer_colony":null}
{"schema_version":1,"trace_id":"01JABCDEF.123-4","kind":"brain_turn","at_unix":1700000000,"outcome":"success","duration_ms":420,"subject":null,"error_kind":null,"tool":null,"colonizer_colony":null}
{"schema_version":1,"trace_id":"trace-1","kind":"colony_handoff","at_unix":1700000004,"outcome":"success","duration_ms":1800,"subject":null,"error_kind":null,"tool":null,"colonizer_colony":"colony-9"}
{"schema_version":1,"trace_id":"trace-2","kind":"nightly_evolve","at_unix":1700000900,"outcome":"failure","duration_ms":9200,"subject":{"kind":"workspace","id":"01JWS.99"},"error_kind":"upstream","tool":null,"colonizer_colony":null}
```

Note the first two share a `trace_id`: one MCP call and the brain turn it served
are one trace. Note also what is absent — no question, no answer, no page
title, no slug. [logging.md](logging.md#reading-the-event-log) has the shell
recipes for reading lines like these.

### The two guarantees

**Content is referenced by page or message id and never copied into the line.**
There is no field named for content: a `Subject` is a kind and an opaque id, and
the reader of the log is expected to fetch the page by id if it wants the page.
The three strings on an event — the trace id, the subject id and the colony id —
are validated as opaque ids by `validate_opaque_id`, because a `String` field
with no rule on it would be a back door around every closed enum in the crate.

This is not a style preference. An event log is the artefact most likely to be
shipped somewhere — to an aggregator, a support engineer, a bug report — and the
moment a page body is allowed into a line, that text has left the machine in a
form nobody chose to send it. `Event` has no `Deserialize`, so a line that
arrived from elsewhere cannot be read back into a typed event and re-serialised
into something the compiler thinks was vetted.

**What is structural and what is only validated — both matter, neither is
enough.** There is no content *field* on `Event`, so the struct shape discourages
one; that part is real. But the three id strings are `pub`, the type is not
`#[non_exhaustive]`, and `Subject.id` is a plain `String`. From another crate
this compiles and serialises:

```rust
let mut event = Event::new(EventKind::BrainTurn, "trace-1", 1)?;
event.trace_id = "the quarterly plan is to migrate the ingest path".into();
// …or, equivalently, on a Subject: subject.id = "page body".into();
```

`events.rs` says so on the field itself — "the field is public so a reader can
read it, and assigning to it directly is the one way to bypass the validator — so
do not". **The validators are the real guarantee; the struct shape only
discourages.** What they stop is free text with a space or an `@` in it:
`Subject::new` refuses `"a page body"` and `alice@example.com`, `Event::new`
refuses `"trace 1"`.

**The honest limit, stated rather than softened.** Any string matching
`[A-Za-z0-9._:-]` of at most 128 characters passes all three validators —
**including a hyphenated Slack bot token**. Reproduced against the crate:
`Event::new(EventKind::BrainTurn, "<slack-token-shaped id>", 1)` — the
placeholder stands for a real hyphenated Slack bot token, kept out of this file
so a secret scan of the source bytes never matches it — returns `Ok`, and
`to_json_line()` emits that token verbatim as `trace_id`. The
same holds for `Subject::new` and `Event::with_colony`. This is not a rare edge
case to be waved at; it is what the charset is.

Why it is tolerable today, and what would have to change for it not to be:

- **No writer exists.** Nothing constructs or writes an `Event` outside tests,
  so there is no log for a leaked id to land in. The limitation is documented
  *before* it can hurt, which is the only good time to document one.
- **The discipline a caller must hold** is: pass ids **only** from its `IdGen`,
  never from content. Nothing enforces that from outside the crate.
- **The upstream mitigation is designed but not wired.**
  [`livingbrain-redact`](../crates/livingbrain-redact) is the intended redactor
  for ingest paths and would sit *before* this line. It is currently a
  dependency of no other crate in the workspace, is called from no ingest path,
  and is exercised only by its own tests — so from this repository alone the
  upstream half of the defence cannot be verified.
  [logging.md](logging.md#redaction) describes it.

The usage batch is not affected by any of this, for reasons stated on
[usage-data.md](usage-data.md#the-two-ids-and-the-one-thing-that-gets-through).

### Trace ids across components

One turn is followed from the message that caused it to the page it produced by
a single opaque id. It is minted where the work starts and carried across
**Slack → Worker → MCP/CLI → Colonizer**: the Worker puts it on the response, the
MCP server and the CLI pass it through on the calls they make, and a colony
hand-off records it alongside `colonizer_colony`. Every line the turn produces
carries the same `trace_id`, which is what makes `grep trace_id` across a log,
or a trace query in your backend, one operation rather than a correlation
effort.

**The header carry is not plumbed anywhere.** Nothing in this repository reads
or writes an `x-trace-id` header: there is no propagation in the Worker, no
header on the MCP server's requests and none in the CLI's client. What is
described here is the contract the event log is built for; the plumbing that
moves the value between those components is a later piece of work.

## Operational logs and OTLP

**This half is specified, not built.** The plan is that a self-hoster points
`OTEL_EXPORTER_OTLP_ENDPOINT` — plus headers for the ones that need them, e.g.
`OTEL_EXPORTER_OTLP_HEADERS` — at whatever backend they already run, and
Living Brain exports over OTLP:

- **Metrics** — brain turns, latency, queue depth, nightly-evolve duration,
  search p95 and token spend. These are counts and durations from the running
  system, which is a different thing from the anonymous usage batch: they are
  about your instance's health, not a report to us.
- **Traces** — one span tree per brain turn, so a slow turn is a trace you can
  read rather than a number you can only compare.
- **Logs** — the operational lines, alongside the structured event log.

The same redaction is intended to apply to all three: no message text and no
page body would reach a span attribute or a log line, and `livingbrain-redact`
is meant to run on the ingest paths upstream of anything being written — though,
as above, nothing calls it yet.

Why OTLP is the choice: it is the one standard that covers all of them, so a
self-hoster points one variable at whichever they already run — Grafana Cloud,
self-hosted Grafana with Prometheus and Loki and Tempo, Datadog, Honeycomb —
rather than integrating twice.

**Not landed in this repository:** there is no OTLP exporter, no span creation
and no metrics instrumentation in the Worker yet, and
`OTEL_EXPORTER_OTLP_ENDPOINT` is read nowhere in the repository — it is the
agreed configuration surface and nothing reads it today. The dashboard in
[`docs/grafana/livingbrain.json`](grafana/livingbrain.json) is written against
the metric names above and will be empty until the exporter is wired in; that is
stated in the dashboard's own description so nobody debugs an empty panel. The
consolidated list of what is planned but absent is on
[logging.md](logging.md#what-is-not-in-this-repository-yet).

## Related

- [usage-data.md](usage-data.md) — the anonymous counted batch, on by default
- [logging.md](logging.md) — operating all three, the redaction rules, and what
  is not built yet
- [brain-events.schema.json](brain-events.schema.json) — the versioned schema