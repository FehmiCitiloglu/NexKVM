#!/usr/bin/env sh
set -eu

cargo run -p coklu -- simulate "${1:-tools/sim/local-workspace.toml}"
