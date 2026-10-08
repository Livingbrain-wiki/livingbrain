# Logging and telemetry: how to operate it

Three streams. Only one has a preference that is on by default, and only one is
intended to send anything off the machine.

| Stream | What it is | Default | Leaves the machine? |
| --- | --- | --- | --- |
| **Anonymous usage batch** | One counted `UsageBatch` per on-period: how much Living Brain was used, never by whom. | **on** (`Telemetry::default()` is `On`) | intended: **yes**, to the usage endpoint. **Nothing in this repository sends it.** |
| **Live-map heartbeat** | One `Heartbeat` while the map is switched on: this install exists, at this size. | **off** (`LiveMap::disabled()`) | intended: **yes**, to the heartbeat endpoint, and only after you switch it on. No sender exists here. |
| **Structured event log** | One JSON line per brain turn, nightly-evolve change, MCP call and colony hand-off — a **format and a validated type**, not yet wired to a writer. | **no writer** | **no** — such a line would stay on the machine that wrote it |

The first is the one to turn off, and it is one command or one environment
variable away:

```
livingbrain telemetry off      # for good: the stored preference
LIVINGBRAIN_TELEMETRY=0        # for a run
```

[usage-data.md](usage-data.md) is the reference for it: every field of the batch,
every vocabulary, both off switches, and where the preference is stored in the OS
keychain. The second is off by default and emits nothing at all until someone
switches it on; [telemetry.md](telemetry.md) has the map, the event schema and
the `x-trace-id` contract. The third is not something to point your tail at yet —
nothing in this repository constructs an `Event` outside its tests, so there is
no file to tail.

`livingbrain telemetry` has three subcommands — `on`, `off` and `status` — and
all three honour `--json`, need no token and open no socket, so they all work
while logged out.

## Reading the event log

**When there is one.** Nothing in this repository writes an event log today —
`Event::to_json_line()` has no non-test caller and there is no sink, buffer or
rotation. What follows is how to read the lines *once* a caller writes them,
using the format and the type as they stand. The full field table, including the
closed vocabularies for `kind`, `outcome`, `error_kind` and `tool`, is on
[telemetry.md](telemetry.md#the-structured-event-log); the fixtures in
[`docs/telemetry/`](telemetry/) are checked against it on every build.

Each line is one JSON object, one line, no embedded newline — a log that needs a
second line to be read is a log a tool has to parse, and the whole point is to
be tailed. One produced by running the crate:

```json
{"schema_version":1,"trace_id":"01JABCDEF.123-4","kind":"mcp_call","at_unix":1700000000,"outcome":"success","duration_ms":88,"subject":{"kind":"page","id":"01JABCDEF.123-4"},"error_kind":null,"tool":"brain_search","colonizer_colony":null}
```

A slow brain turn:

```sh
grep '"kind":"brain_turn"' brain-events.jsonl | jq -c 'select(.duration_ms > 2000)'
```

A night that failed upstream:

```sh
jq -c 'select(.kind=="nightly_evolve" and .error_kind=="upstream")' brain-events.jsonl
```

The schema version is on every line, so a log that outlives a release is
readable by whatever did not build it. `duration_ms` is an exact figure rather
than a bucket, because this line stays local: bucketing here would cost the
reader the thing they opened the log for.

### One turn is one grep

`trace_id` is one opaque id for one brain turn, carried across **Slack → Worker
→ MCP/CLI → Colonizer**, so:

```sh
grep '01JABCDEF.123-4' brain-events.jsonl
```

gives the MCP call and the brain turn it served, in order, and you can follow it
from the message that caused it to the page id it produced. The id is validated
as opaque: non-empty, at most 128 characters, drawn from `[A-Za-z0-9._:-]`. The
full contract, and the fact that the `x-trace-id` header carry between those
components is not plumbed anywhere, are on
[telemetry.md](telemetry.md#trace-ids-across-components).

## Redaction

**Message text and page bodies are not carried by an event line.** `Event` has no
field named for content — a `Subject` is a kind plus an opaque id — and `Event`
has no `Deserialize`, so a line that arrived from elsewhere cannot be read back
into a typed event and re-serialised into something the compiler thinks was
vetted. Content is **referenced by page or message id**; a reader who wants the
page fetches it by id.

Be precise about which half of that is structural. There is no content *field*,
so the struct shape discourages one — but the three id strings are `pub`, the
type is not `#[non_exhaustive]`, and `Subject.id` is a plain `String`, so from
another crate `e.trace_id = "a whole sentence".into()` compiles and serialises.
**The validators are the real guarantee; the struct shape only discourages.**
`events.rs` says so on the field itself ("assigning to it directly is the one way
to bypass the validator — so do not").

The honest limit, stated in
[`tests/anonymity.rs`](../crates/livingbrain-telemetry/tests/anonymity.rs): the
charset stops free text and nothing more. **Any** string matching
`[A-Za-z0-9._:-]` of at most 128 characters passes — hyphens are in that set, so
a real hyphenated Slack bot token passes, and
`Event::new(EventKind::BrainTurn, "<slack-token-shaped id>", 1)` — the
placeholder standing for a real hyphenated Slack bot token, which is not printed
here because a secret scan of the source bytes must never match it —
returns `Ok` and emits that token verbatim. What holds today is that no writer
exists, so no line is written; what a caller must hold is that ids come **only**
from its `IdGen`, never from content; and the usage batch is not affected,
because `UsageId::new` and `Version::new` both refuse it
([usage-data.md](usage-data.md#the-two-ids-and-the-one-thing-that-gets-through)).

[`livingbrain-redact`](../crates/livingbrain-redact) is the intended shared
redactor for the ingest paths, so that the thing that decides what is stored is
one piece of code rather than two that can disagree. Its `redact` function
returns the clean text and one `Finding` per removal, and a `Finding` carries a
class and a byte span and nothing else — no matched text, no prefix, no length,
no hash — which is what makes it safe to put in a log. **Nothing calls it yet:**
it is a dependency of no other crate in the workspace and no ingest path runs
through it; only its own tests exercise it. Until that changes, the upstream half
of this defence is designed rather than wired, and no documentation claim should
lean on it as though it were running.

## What is not in this repository yet

Stated briefly, because the plan and the code differ here and an operator
planning a rollout needs to know which is which.

- **OTLP is configured by endpoint and headers, but the Worker-side exporter
  wiring is not landed.** `OTEL_EXPORTER_OTLP_ENDPOINT` is the agreed surface and
  is read nowhere in this repository. There is no span creation and no metrics
  instrumentation in the Worker. The dashboard in
  [`docs/grafana/livingbrain.json`](grafana/livingbrain.json) is written against
  the intended metric names and will read empty until that lands. See
  [telemetry.md](telemetry.md#operational-logs-and-otlp) for the plan.
- **The `x-trace-id` header carry is not plumbed anywhere.** The trace id is a
  contract on the event line; nothing reads or writes the header between Slack,
  the Worker, the MCP server and the CLI yet.
- **The Teams audit log is tracked separately** as
  [#92](https://github.com/Livingbrain-wiki/livingbrain/issues/92). It lives in
  the `ee/` tree, which does not exist in this repository yet.
- **`livingbrain report` is not built.** There is no command that reads a usage
  batch back to you. `livingbrain telemetry status`, which prints the batch, is
  the whole of the reporting surface today.
- **Nothing writes an event log.** The type, the line format, the schema and the
  fixtures are here; the writer is not. `Event::to_json_line()` has no non-test
  caller.
- **The live map cannot be switched from the CLI.** `livingbrain telemetry`
  exists with `on`, `off` and `status`, but `on`/`off` write only
  `telemetry.usage`. `status` shows the live map's stored setting and its
  endpoint and is otherwise read-only; the write side of that opt-in belongs to
  whatever mints the install id and sends the beat.
- **The crate sends nothing itself.** No HTTP client, no queue, no retry, no
  timer: the endpoints are constants and the transport and schedule are the
  caller's. It generates no randomness, holds no clock and reads no
  configuration, so it has two dependencies, `serde` and `serde_json`. The CLI's
  `telemetry` command sends nothing either — nothing in that path constructs a
  `ureq` client.
- **There is no sender, so there is no period.** `Telemetry::notice` returns a
  `String`; the only place it is printed is `livingbrain telemetry status`, on
  **stdout**, when you run it. The intended design — print once per on-period on
  stderr, before sending — is not built, and nothing is sent on a period by
  anything in this repository.

### The network trap, and what it is worth

`no_connection_is_opened_by_on_off_or_status` in
[`crates/livingbrain-cli/tests/telemetry.rs`](../crates/livingbrain-cli/tests/telemetry.rs)
is a **guard against a sender being added**, not a demonstration that a dial was
ever in play. It points the `LIVINGBRAIN_API_URL` **environment variable** — not
the `--api-url` flag — and all six proxy variables, with `NO_PROXY`/`no_proxy`
blanked, which is what makes the redirect land on the trap's own `127.0.0.1`
rather than exempting it, at one `TcpListener` the test owns; `on`, `off` and
`status` fail the test if it ever accepts a connection. The trap is sound rather
than vacuous — running `livingbrain login` through the same setup lands one
accepted connection in it — but what it guards today is a path that has nothing
to dial with. Its value is that the day someone wires a transport into that
command, the test fails rather than the claim quietly becoming false.

## Pointers

- [usage-data.md](usage-data.md) — the anonymous usage contract, exhaustive
- [telemetry.md](telemetry.md) — the live map, the event log, `x-trace-id`, OTLP
- [brain-events.schema.json](brain-events.schema.json) — the versioned event
  schema
- [grafana/livingbrain.json](grafana/livingbrain.json) — a ready-to-import
  dashboard
- [origin-and-plan.md](origin-and-plan.md) — why this was decided, and what else
  was