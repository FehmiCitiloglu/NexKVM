#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT_DIR"

shopt -s nullglob
workflow_files=(.github/workflows/*.yml .github/workflows/*.yaml)
if (( ${#workflow_files[@]} == 0 )); then
  echo "No GitHub Actions workflows found" >&2
  exit 1
fi

awk '
  BEGIN {
    approved["actions/checkout@34e114876b0b11c390a56381ad16ebd13914f8d5"] = 1
    approved["dtolnay/rust-toolchain@4be7066ada62dd38de10e7b70166bc74ed198c30"] = 1
    approved["Swatinem/rust-cache@c19371144df3bb44fab255c43d04cbc2ab54d1c4"] = 1
    approved["taiki-e/install-action@1725d1806acf0cd664ecc8dd0fcff6d896453dcf"] = 1
    approved["actions/upload-artifact@ea165f8d65b6e75b540449e92b4886f43607fa02"] = 1
    approved["actions/download-artifact@d3f86a106a0bac45b974a628896c90dbdf5c8093"] = 1
    approved["anchore/sbom-action@e22c389904149dbc22b58101806040fa8d37a610"] = 1
    approved["softprops/action-gh-release@3bb12739c298aeb8a4eeaf626c5b8d85266b0e65"] = 1
  }
  /uses:[[:space:]]*/ {
    action = $0
    sub(/^.*uses:[[:space:]]*/, "", action)
    sub(/[[:space:]#].*$/, "", action)
    if (action ~ /^\.\//) {
      next
    }
    count = split(action, parts, "@")
    ref = parts[count]
    if (length(ref) != 40 || ref ~ /[^0-9a-f]/) {
      printf "%s:%d: action is not pinned to a full commit SHA: %s\n", FILENAME, FNR, action > "/dev/stderr"
      failed = 1
    } else if (!(action in approved)) {
      printf "%s:%d: action pin is not in the reviewed official-pin allowlist: %s\n", FILENAME, FNR, action > "/dev/stderr"
      failed = 1
    }
  }
  END { exit failed }
' "${workflow_files[@]}" || {
  echo "GitHub Actions workflow policy failed" >&2
  exit 1
}

awk '
  function finish_install_action() {
    if (install_action && !tool_configured) {
      printf "%s:%d: pinned taiki-e/install-action must declare tool: cargo-deny@0.20.2\n", action_file, action_line > "/dev/stderr"
      failed = 1
    }
    install_action = 0
    tool_configured = 0
  }
  FNR == 1 { finish_install_action() }
  /^[[:space:]]*-[[:space:]]+(name:|uses:|run:)/ && install_action {
    finish_install_action()
  }
  /uses:[[:space:]]*taiki-e\/install-action@/ {
    finish_install_action()
    install_action = 1
    action_file = FILENAME
    action_line = FNR
    next
  }
  install_action && /^[[:space:]]*tool:[[:space:]]*cargo-deny@0[.]20[.]2([[:space:]#]|$)/ {
    tool_configured = 1
  }
  END {
    finish_install_action()
    exit failed
  }
' "${workflow_files[@]}" || {
  echo "GitHub Actions install-action configuration policy failed" >&2
  exit 1
}

awk '
  function finish_toolchain_action() {
    if (toolchain_action && !toolchain_configured) {
      printf "%s:%d: pinned dtolnay/rust-toolchain must declare an approved explicit toolchain\n", action_file, action_line > "/dev/stderr"
      failed = 1
    }
    toolchain_action = 0
    toolchain_configured = 0
  }
  FNR == 1 { finish_toolchain_action() }
  /^[[:space:]]*-[[:space:]]+(name:|uses:|run:)/ && toolchain_action {
    finish_toolchain_action()
  }
  /uses:[[:space:]]*dtolnay\/rust-toolchain@/ {
    finish_toolchain_action()
    toolchain_action = 1
    action_file = FILENAME
    action_line = FNR
    next
  }
  toolchain_action && /^[[:space:]]*toolchain:[[:space:]]*(1[.]88[.]0|nightly-2026-07-15)([[:space:]#]|$)/ {
    toolchain_configured = 1
  }
  END {
    finish_toolchain_action()
    exit failed
  }
' "${workflow_files[@]}" || {
  echo "GitHub Actions Rust toolchain configuration policy failed" >&2
  exit 1
}

while IFS=: read -r workflow line_number command; do
  if [[ "$command" != *"cargo +nightly-2026-07-15 install cargo-fuzz --version 0.13.2 --locked"* ]]; then
    echo "$workflow:$line_number: cargo-fuzz install must pin nightly, version 0.13.2, and --locked" >&2
    exit 1
  fi
done < <(grep -nH "install cargo-fuzz" "${workflow_files[@]}")

while IFS=: read -r workflow line_number command; do
  if [[ "$command" != *"cargo +nightly-2026-07-15 fuzz run protocol_decode"* ]]; then
    echo "$workflow:$line_number: protocol fuzzing must select nightly-2026-07-15 explicitly" >&2
    exit 1
  fi
done < <(grep -nH "fuzz run protocol_decode" "${workflow_files[@]}")

echo "GitHub Actions are pinned to full commit SHAs"
