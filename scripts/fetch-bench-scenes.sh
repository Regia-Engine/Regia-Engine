#!/usr/bin/env bash
# Fetch the public benchmark scenes (never committed): every file is pinned to one commit and
# checked against its SHA-256, and LICENSES.txt is written beside them. See docs/benchmark-scenes.md.
#   scripts/fetch-bench-scenes.sh [scene ids...] [--assets DIR] [--restricted]
set -euo pipefail
cd "$(dirname "$0")/.."
exec cargo run --release -p regia_cli --bin regia -- bench --fetch "$@"
