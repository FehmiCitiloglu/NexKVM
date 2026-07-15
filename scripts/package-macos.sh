#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIST="$ROOT/target/package"
APP_DIR="$DIST/nexkvm.app"
VERSION="${NEXKVM_VERSION:-0.1.0}"
TARGET_TRIPLE="aarch64-apple-darwin"
ARCHITECTURE="arm64"
TARGET_ROOT="${CARGO_TARGET_DIR:-$ROOT/target}"
if [[ "$TARGET_ROOT" != /* ]]; then
  TARGET_ROOT="$ROOT/$TARGET_ROOT"
fi
BINARY_DIR="$TARGET_ROOT/$TARGET_TRIPLE/release"
ARCHIVE="$DIST/nexkvm-macos-${ARCHITECTURE}-${VERSION}.zip"
LOGO_PNG="$ROOT/packaging/assets/nexkvm-logo.png"
ICONSET_DIR="$DIST/nexkvm.iconset"
ICON_RENDERER="$DIST/render-macos-icon"
ICON_RENDER_SOURCE="$ROOT/scripts/render-macos-icon.swift"
ICON_BUILDER="$DIST/build-macos-icns"
ICON_BUILDER_SOURCE="$ROOT/scripts/build-macos-icns.swift"
SWIFT_MODULE_CACHE="$DIST/swift-module-cache"
ICNS_OUT="$APP_DIR/Contents/Resources/nexkvm.icns"
ENTITLEMENTS="$ROOT/packaging/macos/nexkvm.entitlements"
VALIDATOR="$ROOT/scripts/validate-macos-package.sh"
RELEASE="${NEXKVM_RELEASE:-0}"
NOTARY_RESULT="$DIST/notary-result.json"

fail() {
  echo "macOS packaging failed: $*" >&2
  exit 1
}

cleanup() {
  rm -rf "$ICONSET_DIR"
  rm -rf "$SWIFT_MODULE_CACHE"
  rm -f "$ICON_RENDERER"
  rm -f "$ICON_BUILDER"
}
trap cleanup EXIT

[[ "$(uname -s)" == "Darwin" ]] || fail "this script requires macOS"
[[ "$VERSION" =~ ^[0-9]+[.][0-9]+[.][0-9]+$ ]] \
  || fail "NEXKVM_VERSION must use the numeric X.Y.Z form"
[[ "$RELEASE" == "0" || "$RELEASE" == "1" ]] \
  || fail "NEXKVM_RELEASE must be 0 or 1"

if [[ "$RELEASE" == "1" ]]; then
  [[ -n "${NEXKVM_VERSION:-}" ]] \
    || fail "NEXKVM_RELEASE=1 requires an explicit NEXKVM_VERSION"
  [[ -n "${APPLE_CODESIGN_IDENTITY:-}" ]] \
    || fail "NEXKVM_RELEASE=1 requires APPLE_CODESIGN_IDENTITY"
  [[ -n "${APPLE_NOTARY_PROFILE:-}" ]] \
    || fail "NEXKVM_RELEASE=1 requires APPLE_NOTARY_PROFILE"
  if ! security find-identity -v -p codesigning \
    | grep -F -- "$APPLE_CODESIGN_IDENTITY" >/dev/null; then
    fail "APPLE_CODESIGN_IDENTITY is not available in the active keychains"
  fi
fi

cd "$ROOT"
mkdir -p "$DIST"

echo "Building arm64 GUI and daemon release binaries..."
cargo build --locked -p nexkvm -p nexkvm-gui --release --target "$TARGET_TRIPLE"

echo "Creating app bundle..."
rm -rf "$APP_DIR"
mkdir -p "$APP_DIR/Contents/MacOS" "$APP_DIR/Contents/Resources"
cp "$ROOT/packaging/macos/Info.plist" "$APP_DIR/Contents/Info.plist"
/usr/bin/plutil -replace CFBundleShortVersionString -string "$VERSION" \
  "$APP_DIR/Contents/Info.plist"
/usr/bin/plutil -replace CFBundleVersion -string "$VERSION" \
  "$APP_DIR/Contents/Info.plist"
/usr/bin/plutil -lint "$APP_DIR/Contents/Info.plist" >/dev/null
cp "$BINARY_DIR/nexkvm-gui" "$APP_DIR/Contents/MacOS/nexkvm-gui"
cp "$BINARY_DIR/nexkvm" "$APP_DIR/Contents/MacOS/nexkvm"
chmod +x "$APP_DIR/Contents/MacOS/nexkvm-gui" "$APP_DIR/Contents/MacOS/nexkvm"

if [[ ! -f "$LOGO_PNG" ]]; then
  echo "Logo not found at $LOGO_PNG"
  exit 1
fi

echo "Generating app icon (.icns) from logo..."
rm -rf "$ICONSET_DIR"
mkdir -p "$ICONSET_DIR"
mkdir -p "$SWIFT_MODULE_CACHE"
xcrun swiftc -warnings-as-errors -module-cache-path "$SWIFT_MODULE_CACHE" \
  "$ICON_RENDER_SOURCE" -o "$ICON_RENDERER"
xcrun swiftc -warnings-as-errors -module-cache-path "$SWIFT_MODULE_CACHE" \
  "$ICON_BUILDER_SOURCE" -o "$ICON_BUILDER"
for size in 16 32 128 256 512; do
  "$ICON_RENDERER" "$LOGO_PNG" "$ICONSET_DIR/icon_${size}x${size}.png" "$size"
  "$ICON_RENDERER" "$LOGO_PNG" "$ICONSET_DIR/icon_${size}x${size}@2x.png" \
    "$((size * 2))"
done
"$ICON_BUILDER" "$ICONSET_DIR" "$ICNS_OUT"

if [[ "$RELEASE" == "1" ]]; then
  echo "Signing app bundle with identity: $APPLE_CODESIGN_IDENTITY"
  for executable in nexkvm nexkvm-gui; do
    codesign --force --timestamp --options runtime --entitlements "$ENTITLEMENTS" \
      --sign "$APPLE_CODESIGN_IDENTITY" "$APP_DIR/Contents/MacOS/$executable"
  done
  codesign --force --timestamp --options runtime --entitlements "$ENTITLEMENTS" \
    --sign "$APPLE_CODESIGN_IDENTITY" "$APP_DIR"
else
  echo "Signing app bundle ad-hoc for local development."
  for executable in nexkvm nexkvm-gui; do
    codesign --force --timestamp=none --options runtime --entitlements "$ENTITLEMENTS" \
      --sign - "$APP_DIR/Contents/MacOS/$executable"
  done
  codesign --force --timestamp=none --options runtime --entitlements "$ENTITLEMENTS" \
    --sign - "$APP_DIR"
  echo "Ad-hoc signed bundles are for local development only and may trigger Gatekeeper warnings."
fi

bash "$VALIDATOR" "$APP_DIR" "$VERSION" "$ARCHITECTURE"

create_archive() {
  rm -f "$ARCHIVE"
  /usr/bin/ditto -c -k --sequesterRsrc --keepParent "$APP_DIR" "$ARCHIVE"
}

create_archive

if [[ "$RELEASE" == "1" ]]; then
  signing_details="$(codesign -dvvv "$APP_DIR" 2>&1)"
  [[ "$signing_details" == *"Authority=Developer ID Application:"* ]] \
    || fail "release bundle is not signed with a Developer ID Application certificate"
  [[ "$signing_details" == *"(runtime)"* ]] \
    || fail "release bundle is missing the hardened runtime flag"
  [[ "$signing_details" == *"Timestamp="* ]] \
    || fail "release bundle is missing a trusted signing timestamp"

  notary_args=(--keychain-profile "$APPLE_NOTARY_PROFILE")
  if [[ -n "${APPLE_NOTARY_KEYCHAIN:-}" ]]; then
    notary_args+=(--keychain "$APPLE_NOTARY_KEYCHAIN")
  fi
  echo "Submitting archive to Apple notarization service..."
  xcrun notarytool submit "$ARCHIVE" "${notary_args[@]}" --wait \
    --output-format json > "$NOTARY_RESULT"
  notary_status="$(/usr/bin/plutil -extract status raw "$NOTARY_RESULT")"
  [[ "$notary_status" == "Accepted" ]] \
    || fail "Apple notarization status is '$notary_status', expected 'Accepted'"
  echo "Stapling notarization ticket..."
  xcrun stapler staple "$APP_DIR"
  codesign --verify --deep --strict --verbose=2 "$APP_DIR"
  xcrun stapler validate "$APP_DIR"
  spctl --assess --type execute --verbose=2 "$APP_DIR"
  create_archive
fi

bash "$VALIDATOR" "$ARCHIVE" "$VERSION" "$ARCHITECTURE"
echo "Created macOS $ARCHITECTURE archive: $ARCHIVE"
