#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
INFO_PLIST="$ROOT/packaging/macos/Info.plist"
PACKAGE_SCRIPT="$ROOT/scripts/package-macos.sh"
VALIDATOR="$ROOT/scripts/validate-macos-package.sh"
ICON_RENDER_SOURCE="$ROOT/scripts/render-macos-icon.swift"
ICON_BUILDER_SOURCE="$ROOT/scripts/build-macos-icns.swift"
VERSION="9.8.7"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "SKIP: macOS package validation requires Darwin."
  exit 0
fi

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

plist_value() {
  /usr/libexec/PlistBuddy -c "Print :$2" "$1"
}

bash -n "$PACKAGE_SCRIPT"
bash -n "$VALIDATOR"
/usr/bin/plutil -lint "$INFO_PLIST" >/dev/null

[[ "$(plist_value "$INFO_PLIST" CFBundleExecutable)" == "nexkvm-gui" ]] \
  || fail "Info.plist must launch nexkvm-gui"
[[ -n "$(plist_value "$INFO_PLIST" NSLocalNetworkUsageDescription)" ]] \
  || fail "Info.plist must describe local-network access"

release_output="${TMPDIR:-/tmp}/nexkvm-release-preflight.$$.log"
if env -u APPLE_CODESIGN_IDENTITY -u APPLE_NOTARY_PROFILE \
  NEXKVM_RELEASE=1 NEXKVM_VERSION="$VERSION" \
  bash "$PACKAGE_SCRIPT" >"$release_output" 2>&1; then
  fail "release mode accepted missing signing and notarization inputs"
fi
grep -F "requires APPLE_CODESIGN_IDENTITY" "$release_output" >/dev/null \
  || fail "release preflight did not explain the missing signing identity"

if env -u APPLE_NOTARY_PROFILE \
  APPLE_CODESIGN_IDENTITY="Developer ID Application: Fixture" \
  NEXKVM_RELEASE=1 NEXKVM_VERSION="$VERSION" \
  bash "$PACKAGE_SCRIPT" >"$release_output" 2>&1; then
  fail "release mode accepted a missing notarization profile"
fi
grep -F "requires APPLE_NOTARY_PROFILE" "$release_output" >/dev/null \
  || fail "release preflight did not explain the missing notarization profile"
rm -f "$release_output"

TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/nexkvm-package-test.XXXXXX")"
trap 'rm -rf "$TMP_DIR"' EXIT

ICONSET_DIR="$TMP_DIR/nexkvm.iconset"
ICON_RENDERER="$TMP_DIR/render-macos-icon"
ICON_BUILDER="$TMP_DIR/build-macos-icns"
mkdir -p "$ICONSET_DIR"
mkdir -p "$TMP_DIR/swift-module-cache"
xcrun swiftc -warnings-as-errors -module-cache-path "$TMP_DIR/swift-module-cache" \
  "$ICON_RENDER_SOURCE" -o "$ICON_RENDERER"
xcrun swiftc -warnings-as-errors -module-cache-path "$TMP_DIR/swift-module-cache" \
  "$ICON_BUILDER_SOURCE" -o "$ICON_BUILDER"
for size in 16 32 128 256 512; do
  "$ICON_RENDERER" "$ROOT/packaging/assets/nexkvm-logo.png" \
    "$ICONSET_DIR/icon_${size}x${size}.png" "$size"
  "$ICON_RENDERER" "$ROOT/packaging/assets/nexkvm-logo.png" \
    "$ICONSET_DIR/icon_${size}x${size}@2x.png" "$((size * 2))"
done
grep -F "hasAlpha: yes" \
  <(sips -g hasAlpha "$ICONSET_DIR/icon_512x512@2x.png") >/dev/null \
  || fail "rendered app icons must preserve an alpha channel"
"$ICON_BUILDER" "$ICONSET_DIR" "$TMP_DIR/nexkvm.icns"
"$ICON_BUILDER" "$ICONSET_DIR" "$TMP_DIR/nexkvm-second.icns"
[[ -s "$TMP_DIR/nexkvm.icns" ]] || fail "ICNS builder did not produce an app icon"
cmp "$TMP_DIR/nexkvm.icns" "$TMP_DIR/nexkvm-second.icns" >/dev/null \
  || fail "ICNS generation must be deterministic"

EXTRACTED_ICONSET="$TMP_DIR/extracted.iconset"
iconutil -c iconset "$TMP_DIR/nexkvm.icns" -o "$EXTRACTED_ICONSET"
for size in 16 32 128 256 512; do
  [[ -s "$EXTRACTED_ICONSET/icon_${size}x${size}.png" ]] \
    || fail "ICNS is missing the ${size}x${size} representation"
  [[ -s "$EXTRACTED_ICONSET/icon_${size}x${size}@2x.png" ]] \
    || fail "ICNS is missing the ${size}x${size}@2x representation"
done

APP_DIR="$TMP_DIR/nexkvm.app"
mkdir -p "$APP_DIR/Contents/MacOS" "$APP_DIR/Contents/Resources"
cp "$TMP_DIR/nexkvm.icns" "$APP_DIR/Contents/Resources/nexkvm.icns"
cp "$INFO_PLIST" "$APP_DIR/Contents/Info.plist"
/usr/bin/plutil -replace CFBundleShortVersionString -string "$VERSION" \
  "$APP_DIR/Contents/Info.plist"
/usr/bin/plutil -replace CFBundleVersion -string "$VERSION" \
  "$APP_DIR/Contents/Info.plist"

printf '%s\n' 'int main(void) { return 0; }' \
  | xcrun clang -arch arm64 -x c - -o "$APP_DIR/Contents/MacOS/nexkvm-gui"
cp "$APP_DIR/Contents/MacOS/nexkvm-gui" "$APP_DIR/Contents/MacOS/nexkvm"
chmod +x "$APP_DIR/Contents/MacOS/nexkvm-gui" "$APP_DIR/Contents/MacOS/nexkvm"

for executable in nexkvm nexkvm-gui; do
  codesign --force --timestamp=none --sign - "$APP_DIR/Contents/MacOS/$executable"
done
codesign --force --timestamp=none --sign - "$APP_DIR"

bash "$VALIDATOR" "$APP_DIR" "$VERSION" arm64

ARCHIVE="$TMP_DIR/nexkvm-macos-arm64-$VERSION.zip"
/usr/bin/ditto -c -k --sequesterRsrc --keepParent "$APP_DIR" "$ARCHIVE"
bash "$VALIDATOR" "$ARCHIVE" "$VERSION" arm64

TAMPERED_APP="$TMP_DIR/tampered.app"
/usr/bin/ditto "$APP_DIR" "$TAMPERED_APP"
printf 'tampered' >> "$TAMPERED_APP/Contents/Resources/nexkvm.icns"
if bash "$VALIDATOR" "$TAMPERED_APP" "$VERSION" arm64 >/dev/null 2>&1; then
  fail "validator accepted an app with a broken code signature"
fi

if bash "$VALIDATOR" "$APP_DIR" "9.8.8" arm64 >/dev/null 2>&1; then
  fail "validator accepted an incorrect plist version"
fi

/usr/bin/plutil -replace CFBundleExecutable -string nexkvm \
  "$APP_DIR/Contents/Info.plist"
if bash "$VALIDATOR" "$APP_DIR" "$VERSION" arm64 >/dev/null 2>&1; then
  fail "validator accepted nexkvm as the main executable"
fi
/usr/bin/plutil -replace CFBundleExecutable -string nexkvm-gui \
  "$APP_DIR/Contents/Info.plist"

/usr/bin/lipo /usr/bin/true -thin x86_64 \
  -output "$APP_DIR/Contents/MacOS/nexkvm"
if bash "$VALIDATOR" "$APP_DIR" "$VERSION" arm64 >/dev/null 2>&1; then
  fail "validator accepted a non-arm64 daemon"
fi

echo "PASS: macOS package metadata and artifact validation"
