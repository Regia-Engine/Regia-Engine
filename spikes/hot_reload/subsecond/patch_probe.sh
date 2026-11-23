#!/usr/bin/env bash
# Edits game_system's body while `dx serve --hot-patch` runs patch_host, and measures save → first
# output of the new body. Usage: patch_probe.sh <dx log file> [rounds]
# Each round sets MARKER to "r<N>" and the increment to N (so `value` jumps prove the new body runs
# and that state was kept). Restores the source at the end.
set -uo pipefail
cd "$(dirname "$0")"
log="$1"
rounds="${2:-5}"
src=patch_host/src/main.rs
[ "${3:-}" = lib ] && src=patch_game/src/lib.rs
orig="$(cat "$src")"
for n in $(seq 2 $((rounds + 1))); do
    sed -i -E "s/const MARKER: &str = \"[^\"]*\";/const MARKER: \&str = \"r$n\";/; s/g\.value \+= [0-9]+;/g.value += $n;/" "$src"
    t0=$(date +%s%3N)
    until grep -q "marker=r$n\$" "$log"; do
        sleep 0.05
        if [ $(( $(date +%s%3N) - t0 )) -gt 120000 ]; then echo "round $n: TIMEOUT"; break; fi
    done
    line="$(grep -m1 "marker=r$n\$" "$log")"
    t1=$(echo "$line" | grep -oE 'PATCHPROBE [0-9]+' | grep -oE '[0-9]+')
    echo "round $n: save->visible $((t1 - t0)) ms | $(echo "$line" | grep -oE 'frame=.*')"
    sleep 2
done
printf '%s\n' "$orig" > "$src"
grep -E 'Hot-patching|hotpatch|patch-applied|Full rebuild|error' "$log" | tail -n $((rounds * 3))
