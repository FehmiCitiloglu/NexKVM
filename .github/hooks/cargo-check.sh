#!/usr/bin/env bash
# PostToolUse hook: after a Rust file is edited, auto-format it and run clippy.
# Reads the hook JSON from stdin; only acts when a .rs file was written/edited.
# Exit 0 = success (non-blocking), exit 2 = blocking error (lint failures).
set -uo pipefail

input="$(cat)"

# Extract the edited file path from the hook payload (best-effort, no jq dependency).
file_path="$(printf '%s' "$input" \
  | grep -oE '"(filePath|file_path|path)"[[:space:]]*:[[:space:]]*"[^"]+"' \
  | head -n1 \
  | sed -E 's/.*:[[:space:]]*"([^"]+)"/\1/')"

# Only act on Rust source files.
case "$file_path" in
  *.rs) ;;
  *) exit 0 ;;
esac

# Bail out quietly if cargo isn't available.
command -v cargo >/dev/null 2>&1 || exit 0

# Format the whole workspace (fast, deterministic).
cargo fmt --all >/dev/null 2>&1 || true

# Run clippy; surface failures as a blocking error so the agent fixes them.
clippy_output="$(cargo clippy --all-targets --all-features 2>&1)"
clippy_exit=$?

if [ "$clippy_exit" -ne 0 ]; then
  printf '{"decision":"block","systemMessage":%s}\n' \
    "$(printf '%s' "clippy reported issues:\n${clippy_output}" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))' 2>/dev/null || printf '"clippy reported issues"')"
  exit 2
fi

exit 0
