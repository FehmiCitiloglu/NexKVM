#!/usr/bin/env bash
set -euo pipefail

if [[ "$#" -lt 2 || "$#" -gt 3 ]]; then
  echo "Usage: $0 <nexkvm.app|archive.zip> <version> [architecture]" >&2
  exit 2
fi

ARTIFACT="$1"
VERSION="$2"
EXPECTED_ARCH="${3:-arm64}"
EXTRACT_DIR=""

fail() {
  echo "macOS package validation failed: $*" >&2
  exit 1
}

cleanup() {
  if [[ -n "$EXTRACT_DIR" ]]; then
    rm -rf "$EXTRACT_DIR"
  fi
}
trap cleanup EXIT

[[ "$(uname -s)" == "Darwin" ]] || fail "validation requires macOS"
[[ "$VERSION" =~ ^[0-9]+[.][0-9]+[.][0-9]+$ ]] \
  || fail "version must use the numeric X.Y.Z form"
[[ "$EXPECTED_ARCH" == "arm64" ]] \
  || fail "the supported release architecture is arm64"

plist_value() {
  /usr/libexec/PlistBuddy -c "Print :$2" "$1"
}

validate_binary() {
  local binary="$1"
  local label="$2"
  local architectures
  local description

  [[ -f "$binary" ]] || fail "$label is missing at $binary"
  [[ -x "$binary" ]] || fail "$label is not executable"

  architectures="$(/usr/bin/lipo -archs "$binary")" \
    || fail "lipo could not inspect $label"
  [[ "$architectures" == "$EXPECTED_ARCH" ]] \
    || fail "$label architectures are '$architectures', expected '$EXPECTED_ARCH'"

  description="$(/usr/bin/file -b "$binary")" \
    || fail "file could not inspect $label"
  [[ "$description" == *"Mach-O 64-bit executable"* ]] \
    || fail "$label is not a Mach-O executable: $description"
  [[ "$description" == *"$EXPECTED_ARCH"* ]] \
    || fail "$label file description does not contain $EXPECTED_ARCH: $description"
}

validate_app() {
  local app_dir="$1"
  local plist="$app_dir/Contents/Info.plist"
  local executable
  local short_version
  local bundle_version
  local local_network_description
  local bonjour_service

  [[ -d "$app_dir" ]] || fail "app bundle is missing at $app_dir"
  [[ -f "$plist" ]] || fail "Info.plist is missing"
  [[ -s "$app_dir/Contents/Resources/nexkvm.icns" ]] \
    || fail "nexkvm.icns is missing or empty"
  /usr/bin/plutil -lint "$plist" >/dev/null \
    || fail "Info.plist is not valid"

  executable="$(plist_value "$plist" CFBundleExecutable)" \
    || fail "CFBundleExecutable is missing"
  [[ "$executable" == "nexkvm-gui" ]] \
    || fail "CFBundleExecutable is '$executable', expected 'nexkvm-gui'"

  short_version="$(plist_value "$plist" CFBundleShortVersionString)" \
    || fail "CFBundleShortVersionString is missing"
  bundle_version="$(plist_value "$plist" CFBundleVersion)" \
    || fail "CFBundleVersion is missing"
  [[ "$short_version" == "$VERSION" ]] \
    || fail "short version is '$short_version', expected '$VERSION'"
  [[ "$bundle_version" == "$VERSION" ]] \
    || fail "bundle version is '$bundle_version', expected '$VERSION'"

  local_network_description="$(plist_value "$plist" NSLocalNetworkUsageDescription)" \
    || fail "NSLocalNetworkUsageDescription is missing"
  [[ -n "$local_network_description" ]] \
    || fail "NSLocalNetworkUsageDescription must not be empty"
  bonjour_service="$(plist_value "$plist" NSBonjourServices:0)" \
    || fail "NSBonjourServices must declare the NexKVM discovery service"
  [[ "$bonjour_service" == "_nexkvm._udp" ]] \
    || fail "unexpected Bonjour service '$bonjour_service'"

  validate_binary "$app_dir/Contents/MacOS/nexkvm-gui" "GUI executable"
  validate_binary "$app_dir/Contents/MacOS/nexkvm" "daemon executable"
  /usr/bin/codesign --verify --deep --strict --verbose=2 "$app_dir" \
    || fail "app bundle code signature is invalid"
}

case "$ARTIFACT" in
  *.app)
    validate_app "$ARTIFACT"
    ;;
  *.zip)
    [[ -s "$ARTIFACT" ]] || fail "archive is missing or empty at $ARTIFACT"
    EXTRACT_DIR="$(mktemp -d "${TMPDIR:-/tmp}/nexkvm-package-validate.XXXXXX")"
    /usr/bin/ditto -x -k "$ARTIFACT" "$EXTRACT_DIR"
    shopt -s nullglob
    apps=("$EXTRACT_DIR"/*.app)
    shopt -u nullglob
    [[ "${#apps[@]}" -eq 1 ]] \
      || fail "archive must contain exactly one top-level .app bundle"
    validate_app "${apps[0]}"
    ;;
  *)
    fail "artifact must be a .app bundle or .zip archive"
    ;;
esac

echo "Validated macOS $EXPECTED_ARCH artifact: $ARTIFACT"
