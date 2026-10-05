# 0003 — Per-scope envelope encryption, blind-index search and crypto-shredding

Status: Accepted

## Context

Issue #43 asks for page bodies encrypted at rest, a search that does not read
the bodies, and a way to make a scope's content permanently unreadable. Three
facts shaped the answer:

- **The module may only name Cratefield crates.** A module naming a vendor SDK
  fails CI's wasm-safety check, so the custodian is the harness's `Kms` port.
- **The issue says AES-256-GCM. We ship XChaCha20-Poly1305**, the cipher
  `cratefield-kms` itself seals with, so the one cipher in the graph: a random
  96-bit GCM nonce is a birthday bound a service cannot reason about comfortably
  and a random 192-bit XChaCha nonce is not, and a Worker isolate has no AES-NI
  where ChaCha20 is a straight-line loop.
- **A Worker isolate is long-lived and there is no way to flush it.** A cached
  data key would make a crypto-shred a promise about the *next* write rather than
  about now.

## Decision

**One data key per scope, held only wrapped.** `scope_keys(scope, key_version,
kms_provider, kms_key_ref, wrapped_dek, created_at, retired_at)` holds a 32-byte
DEK wrapped by the `Kms` port, base64 in D1. The plaintext DEK is never stored,
logged or cached across calls: every read unwraps what it needs and drops it.
Versions are monotonic, never reused, and created lazily on the scope's first
write, so a scope nobody writes costs nothing.

**The envelope is `lbe1 | key_version u32 BE | nonce(24) | XChaCha20-Poly1305`
and the AAD is `livingbrain-pages/body/v1\0{scope}\0{body_key}\0{key_version}`.**
The key version is in the clear because a reader has to know which key to ask the
KMS for — and it is also the last field of the AAD, so editing the header
invalidates the tag and `open` rejects a header that disagrees with the DEK it
unwrapped. Scope, body key and version are all bound, so a ciphertext cannot be
moved between scopes, between versions of a page or onto another key version, and
the opaque bytes in R2 have nothing else binding them.

**Search reads the blind index, never a body.** `page_terms(scope, term, slug)`
holds one row per distinct token of a page's *current* body, where `term` is
`hex(HMAC-SHA256(HMAC-SHA256(dek, "livingbrain-pages/blind-index/v1"), token))`.
A search MACs the query's tokens under every **live** key version of the asker's
scopes and intersects the slugs they match; the scope predicate is always bound,
never derived from the query. A scope with no live key contributes nothing, and
asking for one does not create one.

**What the blind index leaks, stated plainly.** Inside one scope: whether two
pages share a token, how often a token occurs — a frequency side channel no
amount of keying removes — and which pages mention a term. Across scopes:
nothing, because the index key is derived from a per-scope DEK, so the same word
produces an unrelated MAC elsewhere. Lengths are not hidden: a page with many
distinct tokens has more rows. Slugs and `page_links` targets are in clear too —
identifiers, not body text — and a shred takes the links with the terms.

**Rotation is online, and finishing it is a separate, idempotent pass.**
`rotate_scope_key` creates the next version and retires nothing, so old bodies
keep reading under the version they were written with. `reencrypt_scope` re-seals
every body whose recorded `page_versions.key_version` is not the active one,
re-derives the index for the heads it re-sealed, then retires the superseded
versions. Every read succeeds throughout; re-running rewrites nothing.

**Retirement is one guarded statement, and only after a grace window.** A body
that fetched the old key before a rotation may commit after it, so the column,
not the envelope header, is the truth: `write` records `key_version` in the same
batch as the body, re-encryption updates it after each put, and retirement is
`UPDATE scope_keys SET wrapped_dek = NULL, retired_at = ? … AND NOT EXISTS
(SELECT 1 FROM page_versions WHERE scope = ? AND key_version = ?)` as one
statement, so a version any row still names cannot be retired, and only once it has
been superseded for 15 minutes — longer than any Worker request's wall time, with
the wait reported as `ReencryptReport::pending` until it passes.
`rotate_scope_key` refuses with `RotationPending` while a superseded version is
still live, capping live versions per scope at two and a search at two unwraps.

**Crypto-shredding is destroying the key.** `forget_scope` NULLs `wrapped_dek` for
every version of a scope and deletes its `page_terms` and `page_links` rows, in
one batch. The pages, versions and blobs stay: the ciphertext is not what a
reader can get at, the key was. A shredded scope is recorded distinctly from a
retired one — `retired_at` is set by a rotation and NULL by a shred — so a later
write cannot quietly key it again, and `rotate_scope_key` re-checks that in the
same statement that inserts the new version. The shred is immediate in D1 and R2
and **not** in their history: D1 Time Travel and retained R2 object versions keep
the wrapped DEK and the ciphertext until those expire, and a backup taken before
a shred still has the wrapped key. It is complete once the wrapped DEK is gone
from every backup as well; we publish no number for those windows, because the
retention is a property of the operator's Cloudflare settings.

**Customer-managed keys are the choice of `Kms` provider.** Nothing else changes:
`WorkerSecretKms` for the hosted path, `LocalFileKms` for self-hosting, an
HSM-backed provider when one exists — and `kms_provider` and `kms_key_ref` travel
with every wrapped key, so changing custodian is a re-wrap, not a migration.

**Honest limits.** A hosted deployment reads the plaintext to write the wiki. This
protects data **at rest** and **per scope**: against a leaked R2 bucket, a leaked
D1 dump, a mis-scoped query, and one customer's data leaking into another's. It
does not protect data from the service, which holds the key — self-hosting with a
`LocalFileKms` the operator controls is the path to that — and it is no defence
against a compromised Worker holding the key material either.

**Not in this change.** Vector namespaces: there is no vector search yet (#18), and
encrypting an embedding index is a separate decision. Source storage: no source
store exists yet. The on-device cache (#35, #40) is what would let a client read
pages without the service ever holding the key; this is the server half of that.
The scope taxonomy — `channel:` versus `user:` — is #13; scope validation is
unchanged.

## Consequences

- A read is now a KMS unwrap. That is the price of shredding that takes effect
  immediately across isolates, and the latency test
  (`crates/livingbrain-pages/tests/latency.rs`) is the budget that keeps it from
  becoming N unwraps. It runs an in-process KMS, so it budgets the crypto, index
  and SQL, not a custodian's round trip.
- `PageStore::new` takes a `Kms`; every caller must supply one and there is no
  default, because "no key custodian" must never compile. A body in R2 is
  `application/octet-stream` starting with `lbe1`, so anything reading a body
  object directly has to go through `PageStore::read`.
- `scope_keys` and `page_terms` are declared `PersonalDataSet::unreachable`: both
  hold a page's words only in a keyed form, so an erasure is the scope-level key
  destruction rather than a row delete.