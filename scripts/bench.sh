#!/usr/bin/env sh
set -eu

cargo bench -p coklu-network --bench latency_suite
