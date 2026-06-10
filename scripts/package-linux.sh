#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIST="$ROOT/target/package"
APPDIR="$DIST/AppDir"
VERSION="${NEXKVM_VERSION:-0.1.0}"
LOGO_PNG="$ROOT/packaging/assets/nexkvm-logo.png"

mkdir -p "$DIST"

echo "Building release binary..."
cargo build -p nexkvm --release

echo "Installing packaging helpers..."
cargo install --locked cargo-deb --version 2.11.2
cargo install --locked cargo-generate-rpm --version 0.15.2

echo "Building .deb package..."
cargo deb --manifest-path "$ROOT/apps/desktop/Cargo.toml" --no-build --output "$DIST/nexkvm_${VERSION}_amd64.deb"

echo "Building .rpm package..."
cargo generate-rpm --manifest-path "$ROOT/apps/desktop/Cargo.toml" --target x86_64-unknown-linux-gnu
cp "$ROOT/target/generate-rpm/nexkvm-${VERSION}-1.x86_64.rpm" "$DIST/nexkvm_${VERSION}_x86_64.rpm"

echo "Building AppImage..."
rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications" "$APPDIR/usr/share/icons/hicolor/512x512/apps"
cp "$ROOT/target/release/nexkvm" "$APPDIR/usr/bin/nexkvm"
cp "$ROOT/packaging/linux/nexkvm.desktop" "$APPDIR/usr/share/applications/nexkvm.desktop"
cp "$LOGO_PNG" "$APPDIR/usr/share/icons/hicolor/512x512/apps/nexkvm.png"

cat > "$APPDIR/AppRun" <<'EOF'
#!/usr/bin/env bash
HERE="$(dirname "$(readlink -f "$0")")"
exec "$HERE/usr/bin/nexkvm" "$@"
EOF
chmod +x "$APPDIR/AppRun"

ln -sf usr/share/applications/nexkvm.desktop "$APPDIR/nexkvm.desktop"
ln -sf usr/share/icons/hicolor/512x512/apps/nexkvm.png "$APPDIR/.DirIcon"

APPIMAGETOOL="$DIST/appimagetool.AppImage"
if [[ ! -x "$APPIMAGETOOL" ]]; then
  curl -L -o "$APPIMAGETOOL" "https://github.com/AppImage/AppImageKit/releases/download/continuous/appimagetool-x86_64.AppImage"
  chmod +x "$APPIMAGETOOL"
fi
ARCH=x86_64 "$APPIMAGETOOL" "$APPDIR" "$DIST/nexkvm-${VERSION}-x86_64.AppImage"

echo "Linux packages ready in $DIST"
