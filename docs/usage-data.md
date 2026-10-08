# Anonymous usage data

Living Brain is open-core and self-hostable, so the answer to "what do you
send, and when" has to be readable rather than reassuring. This page is that
answer for the usage batch: what it is meant to send is **a counted batch of
numbers** — how much Living Brain was used, never by whom. It is **on by
default**.

**Nothing in this repository sends it.** What you can check today is the batch
itself: `livingbrain telemetry status` prints the exact batch it would send —
every key, every number, verbatim — when you ask for it. The batch is **off
with one command or one environment variable**, and the environment variable
always wins over the stored preference.

The code is [`crates/livingbrain-telemetry`](../crates/livingbrain-telemetry);
[`usage.rs`](../crates/livingbrain-telemetry/src/usage.rs) is the whole of the
payload, and every name on this page is a name in that file.

## What is in a batch

One `UsageBatch` per on-period. The key set is fixed, and `UsageBatch::new` is
the constructor that stamps `payload_version` and starts the maps empty.
[`tests/anonymity.rs`](../crates/livingbrain-telemetry/tests/anonymity.rs)
asserts the serialised payload has **exactly** these nine top-level keys and no
others.

| Key | JSON type | Meaning | Allowed values |
| --- | --- | --- | --- |
| `payload_version` | integer | The wire version this build produces (`USAGE_PAYLOAD_VERSION`, currently `1`). A receiver checks it before reading anything else. | — |
| `usage_id` | string | A random id for this on-period, 16 characters of `[a-z0-9]`. | validated by `UsageId::new` |
| `version` | string | The sender's own crate version. | validated by `Version::new` |
| `platform` | string | Which host this was. | `linux`, `macos`, `windows`, `wasm`, `other` |
| `period_end_unix` | integer | When the on-period ends, in seconds since the Unix epoch. A timestamp, not an interval, so two batches can be ordered without trusting either sender's clock arithmetic. | — |
| `counters` | object | Magnitudes, keyed by `Count`. Value is an integer. | keys: `workspaces`, `pages`, `brain_turns`, `mcp_calls` |
| `latencies` | object | Bucketed durations, keyed by `Latency`. Value is a bucket string. | keys: `search` |
| `outcomes` | object | The last outcome of each unit of work, keyed by `OutcomeMetric`. Value is an `Outcome` string. | keys: `nightly_evolve`, `brain_turn`, `mcp_call` |
| `errors` | object | Failures, classified and counted, keyed by `ErrorKind`. Value is an integer. | keys: `config`, `upstream`, `timeout`, `decode`, `permission`, `internal` |

An empty map is an **absent fact**, not a zero: a receiver can tell "no search
ran this period" from "no search was measured", which a pre-filled zero in every
bucket could not. The maps are `BTreeMap`s, so the wire is sorted and a diff of
two batches is readable.

### The two string fields

Both are newtypes whose only constructor is a validator, and `UsageBatch` has
no `String` field at all. The fields themselves are `pub`, so this is a
**validator-based** guarantee rather than a sealed-constructor one: a caller that
writes a struct literal — `UsageBatch { payload_version: 999, .. }` — compiles
and serialises, and so does assigning to a field after `new`. What the types do
guarantee is that a value which could carry free text cannot be put in these two
fields through their constructors.

| Field | Rule | Refused |
| --- | --- | --- |
| `usage_id` | exactly 16 characters, `[a-z0-9]` only | uppercase, hyphens, a UUID, anything shorter or longer |
| `version` | `major.minor.patch`, each part one or more ASCII digits with no leading zero unless the number is exactly `0`; then optionally exactly one `-<word>.<number>`, where `word` is one of exactly four: `rc`, `alpha`, `beta`, `dev`. At most 32 characters, and no other character anywhere. | `main`, `v0.1.0`, `01.1.0`, `0.1`, `1.2.3.4`, `1.2.3-rc1`, `0.1.0-dirty`, `/home/alice/src/livingbrain`, `alice@example.com`, a 40-character commit hash (over the cap — which is why the cap is 32), and the fingerprint-shaped values below |

The version grammar is deliberately tighter than semver, because a looser
"digits, dots and hyphens" rule is still free text: a person's checkout and this
crate's released versions are the same characters in the same order. Verified
against the crate:

| Value | Result |
| --- | --- |
| `0.1.0` | accepted |
| `1.2.3-rc.1`, `0.2.0-beta.3`, `1.0.0-alpha.1`, `1.0.0-dev.12` | accepted |
| `2026.10.08` | **refused** — `08` has a leading zero |
| `1.2.3-rc1` | refused — a prerelease suffix is `-rc.N`, not `-rc1` |
| `0.1.0-dirty` | refused — `dirty` is not one of the four words |
| `v0.1.0` | refused — `v` is not part of the grammar |
| `0.1.0-3-gabc1234-dirty` | refused — a second hyphen group: this is `git describe` |
| `2026.10.08-alice-laptop` | refused — a build stamped with a person's name |
| `1jane-doe-macbook-2026` | refused — the same, spelled differently |
| `1workspace-acme-corp` | refused — a build stamped with a workspace |

The last four are refused **because** they carry somebody's machine onto the
wire while looking like a version number, not because they are untidy. Semver
is looser in exactly the two places that matter here: it accepts any
dot-separated identifier as a prerelease tag and any dot-separated identifier
as build metadata, and either can be a name.

### How a batch is filled

`UsageBatch` exposes four `&mut self` methods, so a caller counts rather than
sets and never has to read the batch first:

| Method | Effect |
| --- | --- |
| `count_by(Count, u64)` | adds to a counter, creating it at zero first. **Saturating**, not `+=`: a debug build panics on overflow and a release build wraps, so a plain add would make the count a reader sees depend on how the binary was compiled. A counter that reaches `u64::MAX` is already nonsense, and pinning it there is the honest reading of "more than we can count". |
| `observe_latency(Latency, Duration)` | buckets the duration immediately, so no caller can pass a number that would later be recognised |
| `record_outcome(OutcomeMetric, Outcome)` | sets the outcome of a unit of work |
| `count_error(ErrorKind, u64)` | adds to a classified failure count |

## The closed vocabularies

Every non-numeric value in the payload is a variant of one of these. The wire
string is the same one `as_str()` returns, and `tests/anonymity.rs` asserts the
two agree for every variant, so a rename on one side is a failing test rather
than a silently new vocabulary word.

**`platform`**

| Variant | Wire string |
| --- | --- |
| `Platform::Linux` | `linux` |
| `Platform::Macos` | `macos` |
| `Platform::Windows` | `windows` |
| `Platform::Wasm` | `wasm` |
| `Platform::Other` | `other` |

`other` is a real bucket rather than an `Unknown`: an unrecognised platform is
a fact about a platform, and dropping it would make the total number of
installs disagree with the number of known platforms.

**`Count` — the keys of `counters`**: `Count::Workspaces` → `workspaces`,
`Count::Pages` → `pages`, `Count::BrainTurns` → `brain_turns`, `Count::McpCalls`
→ `mcp_calls`.

**`Latency` — the keys of `latencies`**: `Latency::Search` → `search`. One
variant today, and that is the point: measuring a new latency is a deliberate
addition to the schema.

**`LatencyBucket` — the values in `latencies`**

| Variant | Wire string | Duration |
| --- | --- | --- |
| `LatencyBucket::Under50ms` | `lt_50ms` | under 50 ms |
| `LatencyBucket::Under250ms` | `lt_250ms` | 50 ms up to but not including 250 ms |
| `LatencyBucket::Under1s` | `lt_1s` | 250 ms up to but not including 1 s |
| `LatencyBucket::Under5s` | `lt_5s` | 1 s up to but not including 5 s |
| `LatencyBucket::Over5s` | `gt_5s` | 5 s and over |

The convention is **half-open, lower bound inclusive**: a bucket covers
`[lower, next lower)`, so 50 ms is `lt_250ms` and 250 ms is `lt_1s`. No gap, no
double count. The boundaries are chosen so the interesting one — under a second,
which is whether an interaction feels instant — is a boundary, and so that a
slow call cannot be told from a fast one by the shape of its bucket.

**`OutcomeMetric` — the keys of `outcomes`**: `OutcomeMetric::NightlyEvolve` →
`nightly_evolve`, `OutcomeMetric::BrainTurn` → `brain_turn`,
`OutcomeMetric::McpCall` → `mcp_call`.

**`Outcome` — the values in `outcomes`**: `Outcome::Success` → `success`,
`Outcome::Failure` → `failure`, `Outcome::Skipped` → `skipped`. An `Outcome`,
not a number, because "how many brain turns failed" is a fact about the software
and "which page failed" is a fact about a person.

**`ErrorKind` — the keys of `errors`**: `ErrorKind::Config` → `config`,
`ErrorKind::Upstream` → `upstream`, `ErrorKind::Timeout` → `timeout`,
`ErrorKind::Decode` → `decode`, `ErrorKind::Permission` → `permission`,
`ErrorKind::Internal` → `internal`. Closed so a new kind is a code change someone
reviewed: an open error taxonomy is where "the model returned an error mentioning
the user's question" would arrive.

## A batch on the wire

Produced by running the crate against a fully populated batch — every key used,
every vocabulary word exercised once:

```json
{"payload_version":1,"usage_id":"k3m9q7w2x5b8n4c1","version":"0.1.0","platform":"linux","period_end_unix":1700000000,"counters":{"workspaces":3,"pages":3,"brain_turns":3,"mcp_calls":3},"latencies":{"search":"lt_250ms"},"outcomes":{"nightly_evolve":"success","brain_turn":"success","mcp_call":"success"},"errors":{"config":1,"upstream":1,"timeout":1,"decode":1,"permission":1,"internal":1}}
```

## What can never be in the payload

This is a claim about the type system, not about a filter, and it is worth
stating precisely because a claim like it is worth nothing as a comment.

**Every scalar is an integer or a word from a closed compile-time vocabulary.**
The four maps are keyed by `Count`, `Latency`, `OutcomeMetric` and `ErrorKind`
and valued by `u64`, `LatencyBucket` and `Outcome` — all `Copy` enums with no
data in their variants. `tests/anonymity.rs` walks the serialised payload of a
fully-populated batch to every depth and asserts that every number is a `u64` or
`i64`, every string is either `usage_id`, `version` or a member of a vocabulary
built by iterating the enums themselves, and every map key is a vocabulary word
too — so a counter cannot be filed under a name somebody typed.

**Everything measured is a count or a duration bucket, never a raw
measurement.** `observe_latency` calls `bucket_latency` before it stores
anything, so the exact figure is not available to be sent — and the open-ended
top bucket is deliberate: the exact figure is the interesting part, and this
payload is not where it can go.

**Adding a new measurement means adding an enum variant**, which is a reviewable
code change. There is no `&str` field on `UsageBatch` a caller could fill with a
sentence, and there is no sanitising step that could get the answer wrong.
`the_only_string_fields_are_the_two_validated_ids` in `tests/anonymity.rs` pins
every field to its concrete type by assignment (`let _: u64 =
batch.period_end_unix;` and so on), so changing what a field holds is a change a
compiler sees; adding a free-text field is a change a reviewer sees.

### The two ids, and the one thing that gets through

A charset cannot tell a ULID from a token. Any string matching
`[A-Za-z0-9._:-]` of at most 128 characters passes the opaque-id validators used
by the event log — and **hyphens are in that charset**, so a real, hyphenated
Slack bot token fits it. Reproduced against the crate:
`Event::new(BrainTurn, "xoxb-1111-2222-abcdefghijklmnopqrstuvwx", 1)` returns
`Ok`, and the token lands verbatim in the emitted line. The same is true of
`Subject::new` and of `Event::with_colony`.

**That limitation is not about this payload.** A Slack token is 39 characters,
not 16, so `UsageId::new` refuses it; and it is not `major.minor.patch`, so
`Version::new` refuses it. Both refusals are asserted in `tests/anonymity.rs`,
which is where "the token cannot reach the wire" is checked from outside the
crate. Why the event-log half is tolerable today, and what would make it matter,
is stated on [telemetry.md](telemetry.md#the-two-guarantees); the redaction
layer designed to sit upstream of it is on
[logging.md](logging.md#redaction).

## Turning it off

Two switches, both named verbatim in the announcement:

```
livingbrain telemetry off      # for good: the stored preference
LIVINGBRAIN_TELEMETRY=0        # for a run
```

Both exist today. `Telemetry::resolve` decides, and the rules are exact:

| Environment value | Result |
| --- | --- |
| `0`, `false`, `off`, `no` | forced **off**, whatever the stored preference says |
| `1`, `true`, `on`, `yes` | forced **on**, whatever the stored preference says |
| anything else, unset, or empty | the stored preference, unchanged |

Matching is case-insensitive and surrounding whitespace is ignored, so `OFF`,
`" Yes "` and ` No ` all work. **An unrecognised value is ignored, not guessed
at** — a typo must not silently switch telemetry on or off, so `garbage`
returns the stored preference in both directions.

**The environment always wins over the stored preference.** A variable set for
one run is a deliberate act, and it should beat a preference written months ago.

The default is `Telemetry::On`, and that is the part a reader of the licence
deserves to notice: Living Brain sends counted usage out of the box. The way
that is made honest is that the default is visible.

### Where the preference is stored

The **OS keychain**, under the same service name the CLI's own auth uses —
`wiki.livingbrain.cli` — with two keys:

| Key | Holds | Default when absent |
| --- | --- | --- |
| `telemetry.usage` | the anonymous usage preference, `on` or `off` | `on` (`Telemetry::default`) |
| `telemetry.live-map` | the live-map preference, read-only from this command | `off` |

The keychain rather than a dotfile, and the reason is not that these values are
secret — they are not, they are two words — but that **it is the only place the
CLI already knows how to persist anything**. A dotfile would be a *config file*,
and this CLI has no config-file mechanism, no XDG lookup, no precedence rules and
no migration story. Inventing one here for a two-word preference would add all
of that, and a second thing to forget to wipe. The keychain gives a per-user,
per-machine store that is wiped with the token and honours the same
`LIVINGBRAIN_TEST_KEYRING=mock` seam that `auth` installs before any command
runs, so the telemetry command needs no new test hook at all.

**No dotfile is written.** `the_preference_is_never_written_to_a_file` in
[`crates/livingbrain-cli/tests/telemetry.rs`](../crates/livingbrain-cli/tests/telemetry.rs)
runs `telemetry off` in a scratch working directory with a scratch `$HOME` and
asserts that the string `telemetry.usage` appears nowhere under either tree
afterwards.

## `livingbrain telemetry on | off | status`

`on`, `off` and `status` are registered subcommands of the `livingbrain` CLI in
[`crates/livingbrain-cli/src/telemetry.rs`](../crates/livingbrain-cli/src/telemetry.rs),
every one of them honours the global `--json` flag, and none needs a token or
opens a socket, so all three work while logged out. `logout` qualifies too, since
it only deletes one keychain entry.

`livingbrain telemetry --help`:

```
Turn anonymous usage data on or off, and show what would be sent

Usage: livingbrain telemetry [OPTIONS] <COMMAND>

Commands:
  on      Turn anonymous usage data on
  off     Turn anonymous usage data off
  status  Show what telemetry would send, and where it would go
  help    Print this message or the help of the given subcommand(s)

Options:
      --json               Print a machine-readable JSON result (accepted by every command)
      --api-url <API_URL>  Living Brain API base URL [env: LIVINGBRAIN_API_URL=] [default: https://api.livingbrain.wiki]
  -h, --help               Print help
```

`livingbrain telemetry status`:

```
anonymous usage data: on  (stored on, LIVINGBRAIN_TELEMETRY unset)
live map:              off — no install id exists, so nothing is sent
sent to:               https://telemetry.livingbrain.wiki/v1/telemetry/events

Living Brain sends anonymous usage data once per period: counts, bucketed durations and outcomes, with a random id and nothing that names a workspace, a person or a page.

the period ending at unix 1791504000 (the next UTC midnight)

turn it off for good:  livingbrain telemetry off
turn it off for a run: LIVINGBRAIN_TELEMETRY=0

exactly what this batch says, and nothing else does:
{
  "payload_version": 1,
  "usage_id": "deeb11daaa8cf1ef",
  "version": "0.1.0",
  "platform": "linux",
  "period_end_unix": 1791504000,
  "counters": {},
  "latencies": {},
  "outcomes": {},
  "errors": {}
}
```

`status` prints the next batch **verbatim** — the same nine keys, serialised by
the same `Serialize` impl the sender would use, not a rendering of one. Every
map is empty, and that is on purpose: the command counts nothing, so inventing a
plausible `3` for `counters.pages` would be a self-hoster looking at a number
and believing their install reported three pages. While the preference resolves
to off, the same batch is still printed under the line `the batch that would go,
and nothing else would:`, with no announcement around it.

The `live map:` line reads `opted in — no heartbeat from this command, which
sends nothing` when `telemetry.live-map` is stored `on`, and the live map is
otherwise not switchable from here — see
[telemetry.md](telemetry.md#the-cli-switch-is-read-only-for-the-map).

Every one of these commands is machine-readable too. `livingbrain telemetry
status --json` prints one object with the two halves side by side — `usage`
(stored, resolved, the raw environment value, `env_var`, `endpoint`, and
`next_batch`) and `live_map` (stored, `sends_heartbeat`, `endpoint`):

```json
{"live_map":{"endpoint":"https://telemetry.livingbrain.wiki/v1/telemetry/heartbeat","sends_heartbeat":false,"stored":"off"},"usage":{"endpoint":"https://telemetry.livingbrain.wiki/v1/telemetry/events","env":null,"env_var":"LIVINGBRAIN_TELEMETRY","next_batch":{"counters":{},"errors":{},"latencies":{},"outcomes":{},"payload_version":1,"period_end_unix":1791504000,"platform":"linux","usage_id":"69d84b09690b4552","version":"0.1.0"},"resolved":"on","stored":"on"}}
```

`livingbrain telemetry off`:

```
Anonymous usage data is off. Nothing is collected and nothing is sent.
turn it back on:       livingbrain telemetry on
turn it on for a run:  LIVINGBRAIN_TELEMETRY=1
```

`on` and `off` report the **resolved** value, not the stored one, so a run with
`LIVINGBRAIN_TELEMETRY=0` cannot look like it worked:

```
Anonymous usage data is on.
turn it off for good:  livingbrain telemetry off
turn it off for a run: LIVINGBRAIN_TELEMETRY=0

note: LIVINGBRAIN_TELEMETRY is set to "0", so it is off for this run whatever the stored preference says.
```

#### How the id is minted

**There is no period state in this repository.** `status` mints a **fresh**
`usage_id` on every invocation, by hashing the constant label
`"livingbrain-usage-id"` through a freshly constructed
`std::collections::hash_map::RandomState` and formatting the result as sixteen
lowercase hex characters. Because each `RandomState` draws a different key from
the operating system, three runs produce three different ids — checked, not
assumed:

```
$ for i in 1 2 3; do livingbrain telemetry status --json | jq -r .usage.next_batch.usage_id; done
0f30b22a0231d9f3
936597588ea62025
f8ba212265fdc1cf
```

So "the same id for every batch in the period" is the **contract the type
intends** (`UsageId`'s doc comment says the id is shared by every batch in a
period) and is what a sender with real periods would do — but nothing here
implements a period, and nothing persists an id. `cratefield_core::IdGen` is not
used by this command or by this crate at all; in the workspace it is used by
`livingbrain-models` to mint encryption nonces, and by the pages and workspaces
crates for their own ids. The id is display-only: `status` shows it and sends
nothing.

### The command sends nothing

`on`, `off` and `status` are the switch and the local view. The transport
belongs to whoever sends. Nothing in the `telemetry` command path constructs a
`ureq` client at all: `main.rs` dispatches `Command::Telemetry` to
`telemetry::run` without going through `client(&cli)`, which is what every
network-touching command does.

`no_connection_is_opened_by_on_off_or_status` in
[`crates/livingbrain-cli/tests/telemetry.rs`](../crates/livingbrain-cli/tests/telemetry.rs)
is the regression guard for that. It sets the `LIVINGBRAIN_API_URL`
**environment variable** — not the `--api-url` flag — plus all six proxy
variables (`ALL_PROXY`, `HTTP_PROXY`, `HTTPS_PROXY` and their lowercase
spellings) at one `TcpListener` the test owns, and blanks `NO_PROXY`/`no_proxy`.
Blanking the two `NO_PROXY` variables is the load-bearing part of the redirect:
an inherited `NO_PROXY` exempts the trap's own host by name, and the trap is on
`127.0.0.1`, so it would answer every request itself and the test would prove
nothing. The test then runs `status`, `off` and `on`, in the normal and the
`LIVINGBRAIN_TELEMETRY=0` configurations, and fails if that listener ever accepts
a connection — not if a request arrives, but if a socket is opened at all.

Be precise about what that is worth: it is a guard against a **sender being
added**, not a demonstration that a dial was ever in play. The trap is a sound
one rather than a vacuous one — running `livingbrain login` through the same
setup does land one accepted connection in it — but today it guards a path that
has nothing to dial with. Its value is that the day someone wires a transport
into this command, the test fails rather than the claim quietly becoming false.

## The announcement

`Telemetry::notice(&batch, body)` builds it: both off switches verbatim, then
the batch exactly as it will be sent, pretty. `body` is the sender's own line
about the period (when it started, what it covered), included verbatim, so a
caller cannot make the announcement say something the batch does not show. Its
shape is the block reproduced under
[`livingbrain telemetry status`](#livingbrain-telemetry-on--off--status) above,
which is `notice` output verbatim with an empty batch; with a populated batch
the trailing object is the same nine keys with every value filled in.

**`notice` returns a `String` and writes nothing.** It is a pure function; there
is no `eprintln!`, no `println!` and no file handle anywhere in
`livingbrain-telemetry` — `usage.rs` and `events.rs` between them contain no
output call at all. Its doc comment calls it "the one announcement, printed once
per on-period", and that is the **intended behaviour of a future sender**, not
present tense.

**Where it is printed today: `livingbrain telemetry status`, on stdout.**
`render_status` calls `print!("{}", Telemetry::notice(batch, &body()))`, which
is stdout, and it does so only because you ran `status`. The same command's
`--json` form puts the batch under `usage.next_batch` instead. Nothing else
prints the notice, and nothing prints it without being asked.

The intended behaviour of the future sender — **not built** — is to print this
same string once per on-period, on **stderr**, immediately before posting the
batch, so it cannot be confused with command output. **No such sender exists in
this repository**, and nothing prints the notice once per period by anything.

## Where it goes

`USAGE_ENDPOINT`, a constant rather than a setting, because the question a
self-hoster asks first is *where does my data go* and the answer should be one
line they can read:

```
https://telemetry.livingbrain.wiki/v1/telemetry/events
```

Counts are aggregated in the project's own store. There is no third-party
analytics anywhere in the payload or the dependency graph: no segment, no
mixpanel, no posthog, no vendor SDK that could see a batch.

**What this crate does not do, honestly.** It does not send anything. There is
no HTTP client, no queue, no retry and no timer in
`livingbrain-telemetry`: the endpoint is a constant, and the transport, the
schedule, the storage of the preference and the minting of the random id all
belong to the caller. The crate also generates no randomness, holds no clock and
reads no configuration, so it pulls in no dependency that could do any of those
behind its back. It has two dependencies: `serde` and `serde_json`.

## Related

- [telemetry.md](telemetry.md) — the opt-in live map, the structured event log,
  `x-trace-id`, OTLP and the dashboard
- [logging.md](logging.md) — which stream leaves the machine, how to read the
  event log, and the redaction rules