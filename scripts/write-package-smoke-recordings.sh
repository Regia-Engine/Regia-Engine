#!/bin/sh
# Re-record the committed package-smoke .frec files (ADR-3968).
# Run from anywhere. The recordings are 32 steps at seed 1; the 60 second
# smoke length is REGIA_SMOKE_SECONDS, not the recording.
set -eu
cd "$(dirname "$0")/.."
exec cargo xtask package-smoke --write-recordings
