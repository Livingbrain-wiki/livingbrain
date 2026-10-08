#!/usr/bin/env bash
# The smoke test a deploy is only allowed to pass on. Two checks, in order:
# the harness health route, then — when HARNESS_SECRET is set — one
# authenticated read against the seeded smoke-test workspace.
#
#   scripts/deploy/smoke.sh https://staging-api.livingbrain.wiki
#
# Production runs with HARNESS_SECRET unset, so it gets the health check
# only: nothing seeds production and nothing signs a cookie with the
# production key. Staging sets it, and the cookie is minted here on the
# runner by the `mint_session` example — the signing key never leaves it.
#
# Any failure exits non-zero with a line saying what failed, which is what
# stops the pipeline before production and triggers the rollback step.
set -euo pipefail

if [ $# -ne 1 ]; then
  echo "usage: $(basename "$0") BASE_URL" >&2
  exit 2
fi

script_dir=$(cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(cd -- "$script_dir/../.." && pwd)

base_url=${1%/}
health_url="$base_url/__health"
me_url="$base_url/v1/workspaces/me"
# The workspace scripts/deploy/seed-staging.sql creates, and the member in
# it. `/v1/workspaces/me` answers with the workspace it resolved, so this is
# the assertion that the cookie, the schema and the binding all line up.
expected_workspace=T0SMOKETEST

body=$(mktemp)
trap 'rm -f "$body"' EXIT

fail() {
  echo "SMOKE FAILED: $*" >&2
  exit 1
}

# A freshly deployed Worker can take a few seconds to answer, so the health
# check retries before it is believed.
echo "smoke: GET $health_url"
code=$(curl -sS -o "$body" -w '%{http_code}' --max-time 30 \
  --retry 5 --retry-delay 3 --retry-connrefused "$health_url") \
  || fail "GET $health_url did not answer"
[ "$code" = "200" ] || fail "GET $health_url answered $code, expected 200"

if [ -z "${HARNESS_SECRET:-}" ]; then
  echo "smoke: /__health is 200; HARNESS_SECRET is unset, so the authenticated read is skipped"
  exit 0
fi

echo "smoke: minting a session cookie for $expected_workspace"
cookie=$(cd -- "$repo_root" && \
  HARNESS_SECRET="$HARNESS_SECRET" ENV="${ENV:-staging}" \
  cargo run -q -p livingbrain-workspaces --example mint_session) \
  || fail "could not mint a session cookie"

echo "smoke: GET $me_url"
code=$(curl -sS -o "$body" -w '%{http_code}' --max-time 30 \
  --cookie "__Host-lb_session=$cookie" "$me_url") \
  || fail "GET $me_url did not answer"
if [ "$code" != "200" ]; then
  cat "$body" >&2
  fail "GET $me_url answered $code, expected 200 (a 401 means the cookie did not verify: is HARNESS_SECRET the deployed one, and ENV the deployed environment?)"
fi

# jq is present on a GitHub runner but not everywhere, so parse with python3
# and print the body rather than silently reading nothing.
python3 - "$expected_workspace" "$body" <<'PY' || fail "the authenticated read did not resolve $expected_workspace"
import json, sys

expected, path = sys.argv[1], sys.argv[2]
with open(path, encoding="utf-8") as handle:
    body = json.load(handle)
actual = body.get("workspace", {}).get("id")
if actual != expected:
    print(f"body was {json.dumps(body)}", file=sys.stderr)
    sys.exit(f"workspace.id is {actual!r}, expected {expected!r}")
print(f"smoke: the authenticated read resolved workspace {actual}")
PY

echo "smoke: ok"
