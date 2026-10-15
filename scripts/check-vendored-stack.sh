#!/usr/bin/env bash
#
# check-vendored-stack.sh -- fail CI when a vendored copy of the Factory Zero
# registry entry for this venture (FZ-018) no longer matches the registry.
#
# The product's "Built with" list is served from a *vendored* copy of
# https://factory0.ventures/stack.json: nothing fetches at runtime, so the copy
# can only go stale silently. This script is the check that stops that -- it
# compares the vendored FZ-018 object against the published registry and exits
# non-zero on any drift.
#
# Usage:
#   scripts/check-vendored-stack.sh            # check the published registry
#   STACK_URL=file:///tmp/x.json scripts/check-vendored-stack.sh   # offline
#
# Environment:
#   STACK_URL   Registry to compare against. Defaults to the published URL.
#               Point it at a local file (or any http(s) URL) to run offline.
#   STACK_VENTURE  Venture id to compare. Defaults to FZ-018.
#
# The comparison is semantic, not byte-for-byte: the vendored files carry extra
# top-level keys of their own (`source`, `subprocessors`), and the registry is
# free to add fields. Only the venture's identity fields and its `uses` array
# are compared, entry by entry, field by field.
#
# Network failure is a HARD failure. This check exists to prove the vendored
# copy matches; a fetch we could not complete proves nothing, and a drift check
# that silently passes when the network is down is worse than no check at all.

set -euo pipefail

STACK_URL="${STACK_URL:-https://factory0.ventures/stack.json}"
STACK_VENTURE="${STACK_VENTURE:-FZ-018}"

# The vendored copies. Both are checked against the registry, and against each
# other, so the browser copy and the copy compiled into the binary can never
# quietly disagree.
VENDORED=(
  "crates/livingbrain-stack/src/stack.json"
  "app/assets/stack.json"
)

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

if [ "${1:-}" = "--help" ] || [ "${1:-}" = "-h" ]; then
  sed -n '2,28p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
  exit 0
fi

for path in "${VENDORED[@]}"; do
  if [ ! -f "$path" ]; then
    echo "::error::missing vendored stack copy at $path"
    echo "the vendored copy of the $STACK_VENTURE registry entry is gone; restore it from $STACK_URL"
    exit 1
  fi
done

registry="$(mktemp -t stack-registry.XXXXXX.json)"
trap 'rm -f "$registry"' EXIT

# A bare local path is a copy of the published registry, so it stands in for the
# published URL: `source` must name what the registry actually is, not the
# scratch file a test happens to point at.
published_url="https://factory0.ventures/stack.json"
fetch_url="$STACK_URL"
source_url="$STACK_URL"
case "$STACK_URL" in
  http://*|https://*) ;;
  # A `file://` URL is the documented offline form as well as a bare path, so
  # it takes the same branch: curl reads it, and `source` must still name the
  # published registry rather than the local copy.
  file://*) source_url="$published_url" ;;
  *)
    if [ -f "$STACK_URL" ]; then
      fetch_url="file://$(cd "$(dirname "$STACK_URL")" && pwd)/$(basename "$STACK_URL")"
      source_url="$published_url"
    fi
    ;;
esac

if ! curl -fsSL --max-time 30 --retry 2 -o "$registry" "$fetch_url"; then
  echo "::error::could not fetch the registry at $STACK_URL"
  echo "the vendored copy cannot be verified without it, so this check fails rather than passing blind"
  exit 1
fi

VENDORED_LIST="${VENDORED[*]}" python3 - "$registry" "$source_url" "$STACK_VENTURE" <<'PY'
import json, os, sys

registry_path, registry_url, venture_id = sys.argv[1], sys.argv[2], sys.argv[3]
vendored_paths = os.environ["VENDORED_LIST"].split()
errors = []

def fail(path, msg):
    errors.append(f"{path}: {msg}")


def load(path):
    try:
        with open(path, encoding="utf-8") as fh:
            return json.load(fh)
    except FileNotFoundError:
        fail(path, "file does not exist")
    except json.JSONDecodeError as exc:
        fail(path, f"is not valid JSON ({exc})")
    return None


def find_venture(doc, path):
    """Normalise a document to the venture object plus its `uses` array.

    Three shapes are in play: the registry itself (a `ventures` array), a
    vendored copy that is the bare venture object, and a vendored copy that
    wraps the identity in `venture` and keeps `uses` (and the registry's
    `statuses` glossary) beside it. Accept all three so the check does not care
    which shape a vendored copy happens to use."""
    if not isinstance(doc, dict):
        fail(path, "top level is not a JSON object")
        return None
    bare = {"id": doc.get("id"), **{f: doc.get(f) for f in ("name", "site", "page")}}
    if doc.get("id") == venture_id:
        return {**bare, "uses": doc.get("uses", [])}
    wrapped = doc.get("venture")
    if isinstance(wrapped, dict) and wrapped.get("id") == venture_id:
        uses = doc.get("uses", wrapped.get("uses", []))
        return {**wrapped, "uses": uses}
    for venture in doc.get("ventures", []):
        if isinstance(venture, dict) and venture.get("id") == venture_id:
            return {**venture, "uses": venture.get("uses", [])}
    fail(path, f"holds no {venture_id} venture object")
    return None


registry_doc = load(registry_path)
if registry_doc is None:
    print("\n".join(f"::error::{e}" for e in errors))
    sys.exit(1)

registry_venture = find_venture(registry_doc, registry_url)
if registry_venture is None:
    print("\n".join(f"::error::{e}" for e in errors))
    sys.exit(1)

registry_uses = registry_venture.get("uses", [])
if not isinstance(registry_uses, list) or not registry_uses:
    print(f"::error::{registry_url}: {venture_id} has no `uses` array")
    sys.exit(1)


def describe(entry):
    """Entries repeat a product across roles, so name the role too."""
    if not isinstance(entry, dict):
        return repr(entry)
    return f"{entry.get('name', '?')} ({entry.get('role', '?')})"


def compare_uses(local, remote, path):
    for index in range(max(len(local), len(remote))):
        if index >= len(local):
            fail(path, f"uses[{index}] missing: registry has {describe(remote[index])}")
            continue
        if index >= len(remote):
            fail(path, f"uses[{index}] not in the registry: {describe(local[index])}")
            continue
        got, want = local[index], remote[index]
        for field in sorted(set(got) | set(want)):
            if got.get(field) != want.get(field):
                fail(path, f"uses[{index}] {describe(want)} field `{field}`:\n"
                           f"      vendored: {json.dumps(got.get(field), ensure_ascii=False)}\n"
                           f"      registry: {json.dumps(want.get(field), ensure_ascii=False)}")


def compare_venture(local, doc, path):
    for field in ("id", "name", "site", "page"):
        if local.get(field) != registry_venture.get(field):
            fail(path, f"`{field}`:\n"
                       f"      vendored: {json.dumps(local.get(field), ensure_ascii=False)}\n"
                       f"      registry: {json.dumps(registry_venture.get(field), ensure_ascii=False)}")
    # `source` and `subprocessors` are the vendored copy's own metadata, so they
    # live on the document, beside the venture rather than inside it.
    source = doc.get("source", local.get("source"))
    if source != registry_url:
        fail(path, f"`source` is {json.dumps(source, ensure_ascii=False)}, expected the registry URL fetched "
                   f"({json.dumps(registry_url)})")
    subprocessors = doc.get("subprocessors", local.get("subprocessors"))
    if not isinstance(subprocessors, str) or not subprocessors.startswith("https://"):
        fail(path, f"`subprocessors` is {json.dumps(subprocessors, ensure_ascii=False)}, expected an absolute https URL")
    compare_uses(local.get("uses", []), registry_uses, path)


parsed = []
for path in vendored_paths:
    doc = load(path)
    if doc is None:
        continue
    venture = find_venture(doc, path)
    if venture is not None:
        compare_venture(venture, doc, path)
        parsed.append((path, venture))

# The two copies ship the same list; a divergence is a bug whichever one drifts.
if len(parsed) == 2:
    first_path, first = parsed[0]
    second_path, second = parsed[1]
    compare_uses(second.get("uses", []), first.get("uses", []), f"{second_path} vs {first_path}")

if errors:
    print("\n".join(f"::error::vendored stack drift -- {e}" for e in errors))
    print(f"::error::run `curl -sSL {registry_url}` and update the vendored copies: "
          + ", ".join(vendored_paths))
    sys.exit(1)

print(f"{venture_id} ({registry_venture['name']}) matches {registry_url}: "
      f"{len(registry_uses)} uses entries agree across {', '.join(vendored_paths)}.")
PY