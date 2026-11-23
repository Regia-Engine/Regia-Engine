#!/usr/bin/env bash
# Builds and runs the headless host with the library path the editor launcher would set
# (specs/20 § 1: std is a dylib; the engine dylib lives in target/debug/deps).
# Usage: ./run.sh [host args...]   e.g. ./run.sh --mode salted --edit layout --sim inproc --gens 20
set -euo pipefail
cd "$(dirname "$0")"
cargo build -q -p host
sysroot="$(rustc --print sysroot)"
host_triple="$(rustc -vV | sed -n 's/^host: //p')"
export LD_LIBRARY_PATH="$sysroot/lib/rustlib/$host_triple/lib:$PWD/target/debug/deps${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
exec ./target/debug/host "$@"
