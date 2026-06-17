#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIST="$ROOT/target/package"
APP_DIR="$DIST/nexkvm.app"
VERSION="${NEXKVM_VERSION:-0.1.0}"
ARCHIVE="$DIST/nexkvm-macos-universal-${VERSION}.zip"
LOGO_PNG="$ROOT/packaging/assets/nexkvm-logo.png"
ICONSET_DIR="$DIST/nexkvm.iconset"
ICNS_OUT="$APP_DIR/Contents/Resources/nexkvm.icns"
ENTITLEMENTS="$ROOT/packaging/macos/nexkvm.entitlements"
RELEASE="${NEXKVM_RELEASE:-0}"

if [[ "$RELEASE" == "1" ]]; then
  if [[ -z "${APPLE_CODESIGN_IDENTITY:-}" ]]; then
    echo "NEXKVM_RELEASE=1 requires APPLE_CODESIGN_IDENTITY"
    exit 1
  fi
  if [[ -z "${APPLE_NOTARY_PROFILE:-}" ]]; then
    echo "NEXKVM_RELEASE=1 requires APPLE_NOTARY_PROFILE"
    exit 1
  fi
fi

mkdir -p "$DIST"

echo "Building release binary..."
cargo build -p nexkvm --release

echo "Creating app bundle..."
rm -rf "$APP_DIR"
mkdir -p "$APP_DIR/Contents/MacOS" "$APP_DIR/Contents/Resources"
cp "$ROOT/packaging/macos/Info.plist" "$APP_DIR/Contents/Info.plist"
cp "$ROOT/target/release/nexkvm" "$APP_DIR/Contents/MacOS/nexkvm"
chmod +x "$APP_DIR/Contents/MacOS/nexkvm"

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
  codesign --force --deep --timestamp --options runtime --entitlements "$ENTITLEMENTS" --sign "$APPLE_CODESIGN_IDENTITY" "$APP_DIR"
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

if [[ "$RELEASE" == "1" ]]; then
  echo "Validating signed and notarized app..."
  codesign --verify --deep --strict --verbose=2 "$APP_DIR"
  codesign -dvvv --entitlements :- "$APP_DIR"
  xcrun stapler validate "$APP_DIR"
  spctl -a -vv "$APP_DIR"
fi
