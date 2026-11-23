#!/bin/sh
# test-costs link shim (written by `cargo xtask test-costs`): runs the real linker and appends one
# tab-separated line per link to $REGIA_TEST_COSTS_LINK_LOG: start_ns, end_ns, exit code, output path.
# With $REGIA_TEST_COSTS_ARGS_DIR set it also keeps each link's arguments (one per line) in
# <dir>/<output name>.args, so a link can be replayed with other linker options (ADR-2986).
out=
prev=
for a in "$@"; do
  if [ "$prev" = "-o" ]; then out=$a; fi
  case "$a" in
    @*) if [ -z "$out" ] && [ -r "${a#@}" ]; then out=$(sed -n '/^"\{0,1\}-o"\{0,1\}$/{n;p;q}' "${a#@}" | tr -d '"'); fi ;;
  esac
  prev=$a
done
if [ -n "${REGIA_TEST_COSTS_ARGS_DIR:-}" ] && [ -n "$out" ]; then
  mkdir -p "$REGIA_TEST_COSTS_ARGS_DIR" && printf '%s\n' "$@" > "$REGIA_TEST_COSTS_ARGS_DIR/${out##*/}.args"
fi
s=$(date +%s%N)
"${REGIA_TEST_COSTS_REAL_LINKER:-cc}" "$@"
rc=$?
e=$(date +%s%N)
if [ -n "${REGIA_TEST_COSTS_LINK_LOG:-}" ]; then
  printf '%s\t%s\t%s\t%s\n' "$s" "$e" "$rc" "$out" >> "$REGIA_TEST_COSTS_LINK_LOG"
fi
exit $rc
