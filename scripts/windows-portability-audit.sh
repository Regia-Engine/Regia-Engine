#!/usr/bin/env bash
# Static Windows-portability audit (ADR-524). Needs no toolchain: greps the workspace for Unix-only
# APIs and path assumptions, and fails on a file that uses one without a cfg gate in the same file.
# Usage: scripts/windows-portability-audit.sh [--list]   (--list prints every hit, gated or not)
set -euo pipefail
cd "$(dirname "$0")/.."

# Reads of /proc and /sys are not flagged: they fail with NotFound on Windows and every caller maps that to None.
PATTERN='std::os::(unix|fd|linux)|PermissionsExt|libc::|"xdg-open"|Command::new\("(sh|bash|cp|mv|rm|ls|ln|chmod|which)"\)'
# A file with any of these is treated as gated for the whole file (platform modules, test code).
GATE='cfg\((any\()?(unix|target_os *= *"(linux|android|macos)"|not\(windows\)|not\(any\(windows|all\(unix|not\(target_os *= *"windows")'

list=0; [ "${1:-}" = "--list" ] && list=1
fail=0
while IFS= read -r file; do
  case "$file" in */tests/*|*/benches/*|*/examples/*|xtask/*) continue;; esac
  hits=$(grep -nE "$PATTERN" "$file" | grep -vE '^\s*[0-9]+:\s*//' || true)
  [ -z "$hits" ] && continue
  if grep -qE "$GATE" "$file" || [[ "$file" == */platform/linux.rs || "$file" == */procfs.rs || "$file" == */hid/linux.rs || "$file" == */platform/windows.rs || "$file" == */procfs.rs || "$file" == */hid/linux.rs || "$file" == */platform/macos.rs || "$file" == */platform/android/* ]]; then
    [ $list -eq 1 ] && printf 'gated    %s\n' "$file"
  else
    printf 'UNGATED  %s\n%s\n' "$file" "$hits" | sed '2,$s/^/    /'
    fail=1
  fi
done < <(git ls-files 'crates/**/*.rs' | sort)
exit $fail
