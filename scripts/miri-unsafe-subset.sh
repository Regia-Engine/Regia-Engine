#!/usr/bin/env bash
# Miri over the tests of the `unsafe` that is plain Rust memory work (docs/06-testing-and-ci.md § 14,
# ADR-2086, ADR-2726). Needs a nightly toolchain with miri and rust-src, and no GPU. The Android group
# needs the NDK, for build scripts only: nothing is linked.
# Usage: scripts/miri-unsafe-subset.sh [group...]   (host platform render media dlss windows android; no group: all)
set -u
cd "$(dirname "$0")/.."
TOOLCHAIN=${MIRI_TOOLCHAIN:-nightly}
WANTED=" $* "
want() {
    [ "$WANTED" = "  " ] || [[ "$WANTED" == *" $1 "* ]]
}
failed=0
miri() {
    echo "=== cargo miri test $*"
    if ! cargo "+$TOOLCHAIN" miri test "$@"; then
        echo "FAILED: cargo miri test $*"
        failed=1
    fi
}

want host && {
# Host: the allocator, the thread clock's out parameter, and the library-load helpers.
miri -p regia_core --lib -- memory::tagging
miri -p regia_core --test unsafe_miri
miri -p regia_editor --lib -- \
    hot_reload::load::tests::the_abi_hash_copy \
    hot_reload::load::tests::an_unaligned_export_table \
    hot_reload::load::tests::a_null_slot \
    extension::load::tests::the_entry_box \
    extension::load::tests::the_stamp_copy
# The stamp scan and refusals read their fixtures from the temp directory, so isolation is off.
MIRIFLAGS="${MIRIFLAGS:-} -Zmiri-disable-isolation" miri -p regia_editor --lib -- extension::load::tests::a_file_with_no_stamp \
    extension::load::tests::each_differing_field \
    extension::load::tests::a_truncated_file

}
want platform && {
# Host: code built for the tests of every host. The probe's addresses travel as integers, as in the
# signal handler, hence permissive provenance.
miri -p regia_platform --lib -- wide::tests hid::tests sleep::tests::live_clocks
MIRIFLAGS="${MIRIFLAGS:-} -Zmiri-permissive-provenance" miri -p regia_platform --lib -- \
    native_crash::memory_probe::tests::readable_memory \
    native_crash::memory_probe::tests::stale_bytes \
    native_crash::memory_probe::tests::a_probe_with_no_pipe

}
want render && {
# No `unsafe` of their own: what runs on the bytes the `unsafe` hands over, and on bytes cast for the GPU.
miri -p regia_asset --lib -- pak::tests::round_trip_uncompressed pak::tests::payload_slices \
    pak::tests::owned_payload pak::tests::clearing_and_refilling pak::tests::hostile_size
miri -p regia_render --lib -- pipeline_cache::tests::every_proper_prefix pipeline_cache::tests::round_trips \
    pipeline_cache::tests::header_key scratch::tests::gpu_objects_land mesh_storage::tests::push_slice \
    cooked_model::tests::gpu_ready_mesh_loads

}
want media && {
# The wrapper around `unsafe-libopus`, up to where the library stops Miri: state creation and destroy
# pass only with Stacked Borrows off, and a decode does not pass at all (docs/06 § 14).
MIRIFLAGS="${MIRIFLAGS:-} -Zmiri-disable-stacked-borrows" miri -p regia_media --features opus --test opus_boundary

}
want dlss && {
# NGX's extension table, on the check stub. Miri links nothing, so the build needs no SDK. `names`
# leaks its copies on purpose (the instance keeps them), and so does the test's table.
REGIA_DLSS_CHECK_STUB=1 MIRIFLAGS="${MIRIFLAGS:-} -Zmiri-ignore-leaks" miri -p regia_dlss --features sdk --lib -- table_tests

}
want windows && {
# Windows, interpreted on any host: Miri needs no Windows machine and links nothing.
export CARGO_FEATURE_PURE=1 # blake3's build script would otherwise look for a Windows assembler
miri -p regia_platform --lib --target x86_64-pc-windows-gnu -- \
    native_crash::windows::tests hid::windows::tests platform::windows::miri_tests wide::tests

}
want android && {
# Android, interpreted on any host. The compilers are for C++ build scripts of dependencies.
if [ -n "${ANDROID_NDK_HOME:-}" ]; then
    BIN=$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin
    export CC_aarch64_linux_android=$BIN/aarch64-linux-android24-clang
    export CXX_aarch64_linux_android=$BIN/aarch64-linux-android24-clang++
    export AR_aarch64_linux_android=$BIN/llvm-ar
    miri -p regia_platform --lib --target aarch64-linux-android -- platform::android::vsync::tests
else
    echo "FAILED: set ANDROID_NDK_HOME: the Android group did not run"
    failed=1
fi
}

exit "$failed"
