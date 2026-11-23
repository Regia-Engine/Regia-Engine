#!/usr/bin/env bash
# ThreadSanitizer over the workspace's threaded tests (docs/06-testing-and-ci.md § 14, ADR-2726).
# Needs a nightly toolchain with rust-src, and no GPU. A report is a finding: triage it as § 14 says.
# Usage: scripts/tsan-subset.sh [log directory] [group...]   (no group: all of them)
set -u
cd "$(dirname "$0")/.."
LOGS=${1:-target/tsan-logs}
mkdir -p "$LOGS"
LOGS=$(cd "$LOGS" && pwd)
shift || true
WANTED=" $* "
# `crash-handler` defines its own `pthread_create`, which hides every thread from the sanitizer unless
# this cfg makes it call the sanitizer's interceptor: without it, the runtime faults on the first thread.
export RUSTFLAGS="-Zsanitizer=thread --cfg sanitizer_compat" RUSTDOCFLAGS="-Zsanitizer=thread --cfg sanitizer_compat"
export TSAN_OPTIONS="suppressions=$PWD/docs/ci/tsan-suppressions.txt halt_on_error=0 second_deadlock_stack=1 report_signal_unsafe=0 exitcode=66"
TOOLCHAIN=${TSAN_TOOLCHAIN:-nightly}
failed=0
run() {
    local name=$1
    shift
    if [ "$WANTED" != "  " ] && [[ "$WANTED" != *" $name "* ]]; then
        return
    fi
    echo "=== $name: cargo test $*"
    cargo "+$TOOLCHAIN" test -Zbuild-std --target x86_64-unknown-linux-gnu --no-fail-fast "$@" >"$LOGS/$name.log" 2>&1
    local status=$?
    grep -E "^(test result|error)|WARNING: ThreadSanitizer" "$LOGS/$name.log"
    if [ "$status" -ne 0 ]; then
        echo "$name FAILED with $status; the log is $LOGS/$name.log"
        failed=1
    fi
}
run core -p regia_core --lib --test scope_steal --test par_chunk_spans --test residency_edges
run telemetry -p regia_telemetry --lib --test trace --test trace_observer --test json_layer --test phase_mark --test summary_exit
run schedule_ecs -p regia_ecs --lib
run schedule_app -p regia_app --lib --test loop_and_time --test two_clocks --test states --test task_pool_sizing
run asset -p regia_asset --lib --test hot_reload_edges --test edge_cases --test content_store
run platform -p regia_platform --lib -- deliberate_exit exit:: console_ctrl gpu_worker_registry background
run agent_jobs -p regia_agent --lib -- jobs::
exit "$failed"
