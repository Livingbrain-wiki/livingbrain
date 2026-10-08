#!/usr/bin/env bash
# Rolls a deployed environment back to the previous Worker version.
#
#   scripts/deploy/rollback.sh staging
#   scripts/deploy/rollback.sh staging <version-id>
#
# With no version id, wrangler rolls back to the previous version, which is
# what a failed smoke test wants. With one, it rolls back to that version,
# which is what a human wants after `wrangler deployments list --env <env>`.
#
# The deploy workflow calls this automatically when a deploy succeeds and its
# smoke test then fails, so staging is never left on a build that could not
# read its own database. A rollback does NOT undo D1 migrations — they are
# forward-only, which is why they must stay backward-compatible for one
# release (docs/deploy.md).
#
# Needs CLOUDFLARE_API_TOKEN and CLOUDFLARE_ACCOUNT_ID in the environment
# (the workflow exports them from repository secrets) and the wrangler
# version the workflow pins.
set -euo pipefail

if [ $# -lt 1 ] || [ $# -gt 2 ]; then
  echo "usage: $(basename "$0") ENV [VERSION_ID]" >&2
  exit 2
fi

env_name=$1
version_id=${2:-}

# Only the two environments this repository deploys. Without this an
# arbitrary first argument reaches `wrangler --env`, where it would name an
# environment that does not exist.
case "$env_name" in
  staging | production) ;;
  *)
    echo "$(basename "$0"): ENV must be staging or production, not '$env_name'" >&2
    exit 2
    ;;
esac

# The version .github/workflows/deploy.yml pins, read from the environment
# when the workflow ran this so the two cannot drift; npx, so no
# package.json or node_modules lands in the repository.
wrangler=(npx --yes "wrangler@${WRANGLER_VERSION:-4.147.0}")

script_dir=$(cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(cd -- "$script_dir/../.." && pwd)
config_dir="$repo_root/crates/livingbrain-venture"

if [ -n "$version_id" ]; then
  echo "rollback: rolling $env_name back to version $version_id"
  "${wrangler[@]}" rollback "$version_id" \
    --env "$env_name" --cwd "$config_dir" \
    --message "rolled back by scripts/deploy/rollback.sh" --yes
else
  echo "rollback: rolling $env_name back to the previous version"
  "${wrangler[@]}" rollback \
    --env "$env_name" --cwd "$config_dir" \
    --message "rolled back by scripts/deploy/rollback.sh" --yes
fi

echo "rollback: $env_name is on the version above; re-run scripts/deploy/smoke.sh to confirm"
