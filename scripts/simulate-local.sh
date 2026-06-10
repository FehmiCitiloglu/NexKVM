#!/usr/bin/env sh
set -eu

cargo run -p nexkvm -- simulate "${1:-tools/sim/local-workspace.toml}"
