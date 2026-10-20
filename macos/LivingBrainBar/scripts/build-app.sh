#!/usr/bin/env bash
# Build "Living Brain.app" from the SwiftPM package.
#
#   scripts/build-app.sh                 # → build/Living Brain.app, ad-hoc signed
#   CODESIGN_IDENTITY="Developer ID Application: …" scripts/build-app.sh
#   UNIVERSAL=1 scripts/build-app.sh     # arm64 + x86_64
#   OUTPUT_DIR=/some/dir scripts/build-app.sh
#   BUNDLE_CLI=0 scripts/build-app.sh   # app only, no livingbrain CLI inside
#
# The app bundles the `livingbrain` CLI at Contents/Resources/livingbrain, built
# from the same checkout as the app, so the app and its CLI always agree on the
# opt-in file format. The app prefers this copy over any CLI on the PATH.
set -euo pipefail

here="$(cd "$(dirname "$0")/.." && pwd)"
cd "$here"

app_name="Living Brain"
exe_name="LivingBrainBar"
bundle_id="${BUNDLE_ID:-wiki.livingbrain.bar}"
version="${APP_VERSION:-0.1.0}"
build_number="${BUILD_NUMBER:-1}"
out_dir="${OUTPUT_DIR:-$here/build}"
app="$out_dir/$app_name.app"

arch_flags=()
if [[ "${UNIVERSAL:-0}" == "1" ]]; then
  arch_flags=(--arch arm64 --arch x86_64)
fi

echo "==> swift build -c release ${arch_flags[*]:-}"
swift build -c release --product "$exe_name" ${arch_flags[@]+"${arch_flags[@]}"}
bin_dir="$(swift build -c release --show-bin-path ${arch_flags[@]+"${arch_flags[@]}"})"

echo "==> assembling $app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$bin_dir/$exe_name" "$app/Contents/MacOS/$exe_name"
# SwiftPM resource bundles, if any target ever grows one.
for bundle in "$bin_dir"/*.bundle; do
  [[ -e "$bundle" ]] && cp -R "$bundle" "$app/Contents/Resources/"
done
# The CLI from this same checkout (host architecture unless UNIVERSAL=1 is
# handled by scripts/release.sh, which builds both slices itself and passes
# BUNDLE_CLI=0 here so the CLI is not built twice).
if [[ "${BUNDLE_CLI:-1}" == "1" ]]; then
  repo="$(cd "$here/../.." && pwd)"
  target_dir="${CARGO_TARGET_DIR:-$repo/target}"
  echo "==> livingbrain CLI from this checkout"
  (cd "$repo" && cargo build --locked --release -p livingbrain-cli --bin livingbrain)
  install -m 0755 "$target_dir/release/livingbrain" "$app/Contents/Resources/livingbrain"
fi
if [[ -f "$here/Resources/AppIcon.icns" ]]; then
  cp "$here/Resources/AppIcon.icns" "$app/Contents/Resources/AppIcon.icns"
fi

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>$app_name</string>
  <key>CFBundleDisplayName</key><string>$app_name</string>
  <key>CFBundleExecutable</key><string>$exe_name</string>
  <key>CFBundleIdentifier</key><string>$bundle_id</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$build_number</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleDevelopmentRegion</key><string>en</string>
  <key>LSMinimumSystemVersion</key><string>14.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.productivity</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSPrincipalClass</key><string>NSApplication</string>
  <key>NSHumanReadableCopyright</key><string>Living Brain</string>
</dict>
</plist>
PLIST
plutil -lint "$app/Contents/Info.plist" >/dev/null

identity="${CODESIGN_IDENTITY:--}"
echo "==> codesign (${identity/#-/ad-hoc})"
sign_flags=(--force --sign "$identity")
if [[ "$identity" != "-" ]]; then
  sign_flags+=(--options runtime --timestamp)
fi
# Inside out: the nested CLI first, then the bundle (which seals Resources).
if [[ -f "$app/Contents/Resources/livingbrain" ]]; then
  codesign "${sign_flags[@]}" "$app/Contents/Resources/livingbrain"
fi
codesign "${sign_flags[@]}" "$app"
codesign --verify --strict "$app"

echo "==> built $app"
