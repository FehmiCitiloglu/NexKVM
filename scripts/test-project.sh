#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LOCAL_ROOT="${NEXKVM_LOCAL_TEST_ROOT:-$ROOT/target/local-test}"
CONFIG_HOME="$LOCAL_ROOT/config"
CONFIG_DIR="$CONFIG_HOME/nexkvm"
CONFIG_FILE="$CONFIG_DIR/config.toml"
TRUST_FILE="$CONFIG_DIR/trust.json"
SIM_FILE="${NEXKVM_SIM_FILE:-$ROOT/tools/sim/local-workspace.toml}"

cd "$ROOT"

section() {
  printf '\n==> %s\n' "$1"
}

run() {
  printf '+ %s\n' "$*"
  "$@"
}

section "Prepare isolated local config"
mkdir -p "$CONFIG_DIR"
cat > "$CONFIG_FILE" <<'EOF'
[device]
name = "nexkvm-local-test"

[network]
listen_port = 47654
enable_discovery = false
transports = ["tcp"]

[security]
require_pairing = true
trust_on_reconnect = true

[telemetry]
level = "debug"
json = false

[plugins]
enabled = false
allowed = []

[workspace]
unified_desktop = true
allow_remote_app_launch = false
global_search = true
shared_memory = true
memory_max_entries = 1000

[collaboration]
shared_cursor = true
pair_programming = true
allow_control_requests = true
allow_delegated_control = false
remote_teaching = true
max_participants = 8
default_control_lease_millis = 300000
EOF
printf '[]\n' > "$TRUST_FILE"
printf 'Config: %s\n' "$CONFIG_FILE"
printf 'Trust store: %s\n' "$TRUST_FILE"

section "Format, lint, and test workspace"
run cargo fmt --all -- --check
run cargo clippy --workspace --all-targets --all-features
run cargo test --workspace --all-features

section "Build workspace and nexkvm release binary"
run cargo build --workspace --all-targets --all-features
run cargo build -p nexkvm --release

section "Build desktop package / GUI launcher artifact"
case "$(uname -s)" in
  Darwin)
    if [[ "${NEXKVM_SKIP_PACKAGE:-0}" == "1" ]]; then
      echo "Skipping macOS app bundle because NEXKVM_SKIP_PACKAGE=1"
    else
      run bash "$ROOT/scripts/package-macos.sh"
    fi
    ;;
  Linux)
    echo "Linux release binary built at target/release/nexkvm."
    echo "Set NEXKVM_RUN_LINUX_PACKAGING=1 to also build .deb/.rpm/AppImage artifacts."
    if [[ "${NEXKVM_RUN_LINUX_PACKAGING:-0}" == "1" ]]; then
      run bash "$ROOT/scripts/package-linux.sh"
    fi
    ;;
  MINGW*|MSYS*|CYGWIN*)
    echo "Windows release binary built. Run scripts/package-windows.ps1 from PowerShell to build the installer."
    ;;
  *)
    echo "No package script for this OS; release binary is available at target/release/nexkvm."
    ;;
esac

section "Smoke-test nexkvm CLI with isolated config"
export XDG_CONFIG_HOME="$CONFIG_HOME"
run "$ROOT/target/release/nexkvm" config-path
run "$ROOT/target/release/nexkvm" doctor
run "$ROOT/target/release/nexkvm" protocol
run "$ROOT/target/release/nexkvm" devices
run "$ROOT/target/release/nexkvm" simulate "$SIM_FILE"

section "Done"
printf 'Local test config stayed isolated under: %s\n' "$LOCAL_ROOT"
