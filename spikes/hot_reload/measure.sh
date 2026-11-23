#!/usr/bin/env bash
# Linux: sizes and exported dynamic symbol counts of the spike's dylibs, and the top crates by
# exported symbols in the engine dylib. Run after ./matrix.sh (needs the built libraries).
set -uo pipefail
cd "$(dirname "$0")"
report() {
    local f="$1"
    [ -f "$f" ] || return 0
    printf '%-55s %12s bytes  %7s exported\n' "$f" "$(stat -c %s "$f")" "$(nm -D --defined-only "$f" | wc -l)"
}
report target/debug/deps/libspike_engine_dylib.so
report target/debug/deps/libspike_editor_dylib.so
report target/debug/deps/libsim_dylib.so
report target/debug/libgame_dylib.so
report "$(ls -1 .hot/*.so 2>/dev/null | tail -1)"
report target/debug/host
report target/debug/editor_host
echo "--- engine dylib: exported symbols by crate (top 25) ---"
nm -D --defined-only -C target/debug/deps/libspike_engine_dylib.so \
    | cut -d' ' -f3- \
    | sed -E 's/^<//; s/^(impl )//; s/^&?(mut )?//; s/^<?//' \
    | grep -oE '^[a-zA-Z_][a-zA-Z0-9_]*' \
    | sort | uniq -c | sort -rn | head -25
