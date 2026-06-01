#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIST="$ROOT/target/package"
APPDIR="$DIST/AppDir"
VERSION="${COKLU_VERSION:-0.1.0}"

mkdir -p "$DIST"

echo "Building release binary..."
cargo build -p coklu --release

echo "Installing packaging helpers..."
cargo install --locked cargo-deb --version 2.11.2
cargo install --locked cargo-generate-rpm --version 0.15.2

echo "Building .deb package..."
cargo deb --manifest-path "$ROOT/apps/desktop/Cargo.toml" --no-build --output "$DIST/coklu_${VERSION}_amd64.deb"

echo "Building .rpm package..."
cargo generate-rpm --manifest-path "$ROOT/apps/desktop/Cargo.toml" --target x86_64-unknown-linux-gnu
cp "$ROOT/target/generate-rpm/coklu-${VERSION}-1.x86_64.rpm" "$DIST/coklu_${VERSION}_x86_64.rpm"

echo "Building AppImage..."
rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications"
cp "$ROOT/target/release/coklu" "$APPDIR/usr/bin/coklu"
cp "$ROOT/packaging/linux/coklu.desktop" "$APPDIR/usr/share/applications/coklu.desktop"

cat > "$APPDIR/AppRun" <<'EOF'
#!/usr/bin/env bash
HERE="$(dirname "$(readlink -f "$0")")"
exec "$HERE/usr/bin/coklu" "$@"
EOF
chmod +x "$APPDIR/AppRun"

# 1x1 transparent PNG for AppImage icon requirements.
base64 -d > "$APPDIR/coklu.png" <<'EOF'
iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAJUbS6QAAAAASUVORK5CYII=
EOF
ln -sf usr/share/applications/coklu.desktop "$APPDIR/coklu.desktop"
ln -sf coklu.png "$APPDIR/.DirIcon"

APPIMAGETOOL="$DIST/appimagetool.AppImage"
if [[ ! -x "$APPIMAGETOOL" ]]; then
  curl -L -o "$APPIMAGETOOL" "https://github.com/AppImage/AppImageKit/releases/download/continuous/appimagetool-x86_64.AppImage"
  chmod +x "$APPIMAGETOOL"
fi
ARCH=x86_64 "$APPIMAGETOOL" "$APPDIR" "$DIST/coklu-${VERSION}-x86_64.AppImage"

echo "Linux packages ready in $DIST"
