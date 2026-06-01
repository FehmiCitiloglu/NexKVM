#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIST="$ROOT/target/package"
APP_DIR="$DIST/coklu.app"
VERSION="${COKLU_VERSION:-0.1.0}"
ARCHIVE="$DIST/coklu-macos-universal-${VERSION}.zip"
LOGO_PNG="$ROOT/packaging/assets/coklu-logo.png"
ICONSET_DIR="$DIST/coklu.iconset"
ICNS_OUT="$APP_DIR/Contents/Resources/coklu.icns"

mkdir -p "$DIST"

echo "Building release binary..."
cargo build -p coklu --release

echo "Creating app bundle..."
rm -rf "$APP_DIR"
mkdir -p "$APP_DIR/Contents/MacOS" "$APP_DIR/Contents/Resources"
cp "$ROOT/packaging/macos/Info.plist" "$APP_DIR/Contents/Info.plist"
cp "$ROOT/target/release/coklu" "$APP_DIR/Contents/MacOS/coklu"
chmod +x "$APP_DIR/Contents/MacOS/coklu"

if [[ ! -f "$LOGO_PNG" ]]; then
  echo "Logo not found at $LOGO_PNG"
  exit 1
fi

echo "Generating app icon (.icns) from logo..."
rm -rf "$ICONSET_DIR"
mkdir -p "$ICONSET_DIR"
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" "$LOGO_PNG" --out "$ICONSET_DIR/icon_${size}x${size}.png" >/dev/null
  sips -z "$((size * 2))" "$((size * 2))" "$LOGO_PNG" --out "$ICONSET_DIR/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET_DIR" -o "$ICNS_OUT"
rm -rf "$ICONSET_DIR"

if [[ -n "${APPLE_CODESIGN_IDENTITY:-}" ]]; then
  echo "Signing app bundle with identity: $APPLE_CODESIGN_IDENTITY"
  codesign --force --deep --timestamp --options runtime --sign "$APPLE_CODESIGN_IDENTITY" "$APP_DIR"
else
  echo "APPLE_CODESIGN_IDENTITY not set; building unsigned bundle"
fi

/usr/bin/ditto -c -k --sequesterRsrc --keepParent "$APP_DIR" "$ARCHIVE"
echo "Created archive: $ARCHIVE"

if [[ -n "${APPLE_NOTARY_PROFILE:-}" ]]; then
  echo "Submitting archive to Apple notarization service..."
  xcrun notarytool submit "$ARCHIVE" --keychain-profile "$APPLE_NOTARY_PROFILE" --wait
  echo "Stapling notarization ticket..."
  xcrun stapler staple "$APP_DIR"
  /usr/bin/ditto -c -k --sequesterRsrc --keepParent "$APP_DIR" "$ARCHIVE"
  echo "Notarized archive ready: $ARCHIVE"
else
  echo "APPLE_NOTARY_PROFILE not set; skipping notarization"
fi
