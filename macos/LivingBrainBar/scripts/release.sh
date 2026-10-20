#!/usr/bin/env bash
# Build a downloadable "Living Brain.app" with the CLI inside it, packed in a DMG.
#
#   scripts/release.sh 0.1.0
#       → build/release/Living Brain.app   (universal: arm64 + x86_64)
#       → build/release/LivingBrain.dmg
#       → build/release/LivingBrain.dmg.sha256
#
# Steps: build `livingbrain` (crates/livingbrain-cli) for aarch64-apple-darwin
# and x86_64-apple-darwin and `lipo` them into one binary; build the app
# universal via scripts/build-app.sh; copy the CLI to
# `Contents/Resources/livingbrain` (the app prefers it over ~/bin,
# /opt/homebrew/bin and /usr/local/bin unless a CLI is chosen under Settings);
# sign; pack the DMG with an /Applications link.
#
# Signing: ad-hoc by default, which is NOT notarized — Gatekeeper asks on first
# open (see README "Install from the DMG"). For a notarized build set:
#   CODESIGN_IDENTITY="Developer ID Application: …"   sign with hardened runtime
#   NOTARY_PROFILE=<keychain profile>                  notarytool submit + staple
#     (create one with `xcrun notarytool store-credentials <profile> …`)
#
# Other knobs: BUILD_NUMBER (default 1), OUTPUT_DIR (default build/release),
# CARGO_TARGET_DIR (shared cargo target dir; default <repo>/target).
set -euo pipefail

version="${1:-}"
if [[ -z "$version" || ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "usage: $0 <version>   (e.g. 0.1.0)" >&2
  exit 64
fi

here="$(cd "$(dirname "$0")/.." && pwd)"
repo="$(cd "$here/../.." && pwd)"
out_dir="${OUTPUT_DIR:-$here/build/release}"
app_name="Living Brain"
app="$out_dir/$app_name.app"
dmg="$out_dir/LivingBrain.dmg"
identity="${CODESIGN_IDENTITY:--}"
target_dir="${CARGO_TARGET_DIR:-$repo/target}"
export CARGO_TARGET_DIR="$target_dir"

mkdir -p "$out_dir"

echo "==> livingbrain CLI (aarch64 + x86_64)"
triples=(aarch64-apple-darwin x86_64-apple-darwin)
if command -v rustup >/dev/null; then
  (cd "$repo" && rustup target add "${triples[@]}" >/dev/null)
fi
cli_slices=()
for triple in "${triples[@]}"; do
  (cd "$repo" && MACOSX_DEPLOYMENT_TARGET=14.0 \
    cargo build --locked --release -p livingbrain-cli --bin livingbrain --target "$triple")
  cli_slices+=("$target_dir/$triple/release/livingbrain")
done
cli="$out_dir/livingbrain"
lipo -create -output "$cli" "${cli_slices[@]}"
lipo -info "$cli"

echo "==> app (universal)"
UNIVERSAL=1 APP_VERSION="$version" BUILD_NUMBER="${BUILD_NUMBER:-1}" \
  OUTPUT_DIR="$out_dir" CODESIGN_IDENTITY="-" BUNDLE_CLI=0 "$here/scripts/build-app.sh"

echo "==> bundling the CLI"
install -m 0755 "$cli" "$app/Contents/Resources/livingbrain"
rm -f "$cli"

echo "==> codesign (${identity/#-/ad-hoc})"
sign_flags=(--force --sign "$identity")
if [[ "$identity" != "-" ]]; then
  sign_flags+=(--options runtime --timestamp)
fi
# Inside out: the nested CLI first, then the bundle (which seals Resources).
codesign "${sign_flags[@]}" "$app/Contents/Resources/livingbrain"
codesign "${sign_flags[@]}" "$app"
codesign --verify --deep --strict "$app"

echo "==> $dmg"
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
cp -R "$app" "$stage/"
ln -s /Applications "$stage/Applications"
rm -f "$dmg"
hdiutil create -volname "$app_name" -srcfolder "$stage" -fs HFS+ -format UDZO -ov "$dmg" >/dev/null
if [[ "$identity" != "-" ]]; then
  codesign --force --sign "$identity" --timestamp "$dmg"
fi

if [[ -n "${NOTARY_PROFILE:-}" ]]; then
  if [[ "$identity" == "-" ]]; then
    echo "NOTARY_PROFILE needs CODESIGN_IDENTITY (a Developer ID); not notarizing" >&2
    exit 1
  fi
  echo "==> notarize ($NOTARY_PROFILE)"
  xcrun notarytool submit "$dmg" --keychain-profile "$NOTARY_PROFILE" --wait
  xcrun stapler staple "$dmg"
fi

(cd "$out_dir" && shasum -a 256 "$(basename "$dmg")" > "$(basename "$dmg").sha256")

echo
echo "app:    $app"
echo "dmg:    $dmg ($(du -h "$dmg" | cut -f1 | tr -d ' '))"
echo "sha256: $dmg.sha256 ($(cut -d' ' -f1 < "$dmg.sha256"))"
[[ "$identity" == "-" ]] && echo "note:   ad-hoc signed, not notarized — Gatekeeper asks on first open"
# The release API refuses a short SHA as --target, so the suggested command
# names the full commit. Short ids are fine for display; this one is not.
full_sha="$(git -C "$repo" rev-parse HEAD)"
echo "publish: gh release create macos-v$version $(printf '%q' "$dmg") $(printf '%q' "$dmg.sha256") --target $full_sha --title \"Living Brain for Mac $version\" --latest --generate-notes"
exit 0
