#!/usr/bin/env sh
set -eu

cargo bench -p nexkvm-network --bench latency_suite
