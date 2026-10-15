#!/usr/bin/env bash
# Build "Living Brain.app" from the SwiftPM package.
#
#   scripts/build-app.sh                 # → build/Living Brain.app, ad-hoc signed
#   CODESIGN_IDENTITY="Developer ID Application: …" scripts/build-app.sh
#   UNIVERSAL=1 scripts/build-app.sh     # arm64 + x86_64
#   OUTPUT_DIR=/some/dir scripts/build-app.sh
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
codesign "${sign_flags[@]}" "$app"
codesign --verify --strict "$app"

echo "==> built $app"
