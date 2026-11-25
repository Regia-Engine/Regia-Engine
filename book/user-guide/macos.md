# macOS (testing only)

<!-- index: macOS -->
<!-- index: Metal -->
<!-- index: Cursor grab -->

Regia Engine builds and runs its test suite on macOS with Metal. This is a bring-up for testing
(ADR-264, which amends ADR-009), not a supported release target: there is no macOS release build,
packaging, signing or notarization, and macOS tests never gate a merge.

There is no dedicated-server package. `regia package --server` is Linux and Windows, and any other
platform exits 2 with `server_platform_unsupported`. `--format app` is rejected on every platform:
the macOS app-bundle format is not built. An iOS release is parked and is not this testing build.
DLSS is never linked on macOS. The graphics backend here is Metal.

`CursorGrab::Confined` is not available, so a Confined request is sent as Locked and logged once
per process. Locked stays Locked. Spec 11: only the primary window becomes an operating-system
window (ADR-2252).

Install and day-to-day development on Linux are [Linux](linux.md) and
[Installing Regia Engine](../start/install.md). `regia doctor --platform macos` runs on the Mac
(Xcode command-line tools and `spirv-val`).

State recorded for v0.1.22, on an Apple M5 (32 GB, macOS 26.6, Metal 4) with Homebrew `rustc` 1.99.1:
the whole workspace builds, and 4979 of 5006 test cases pass (472 test binaries, 38 ignored).

## What is macOS-specific

- **Platform services** live in `regia_platform::platform::macos` (`crates/regia_platform/src/platform/macos.rs`):
  opening a URL through `/usr/bin/open` (not waited for), the locale from the environment and then
  the user's `AppleLanguages` and `AppleLocale` defaults (GUI launches carry no `LANG`), OS information from `sw_vers` and
  `sysctl`, and `_exit` for the clean-exit watchdog. The Linux module, which used Wayland and X11
  window attributes, no longer compiles for macOS, and `with_app_id` does nothing there.
- **Graphics backend.** `instance_backends()` is Metal on macOS. wgpu's Vulkan backend needs a loader
  a Mac does not have.
- **Metal workarounds**, none of which change Linux, Windows or Android output:
  - The exposure histogram uses one workgroup array instead of 64 scalar workgroup variables
    (`HIST_WORKGROUP_ARRAY`), because Metal allows 31 threadgroup arguments.
  - The frame GPU timer is off where `TIMESTAMP_QUERY_INSIDE_ENCODERS` is missing: Apple GPUs sample
    timestamps at pass boundaries only. Frame timing reports the `fallback_no_timestamps` notice.
  - The terrain impostor shader no longer needs `SHADER_EARLY_DEPTH_TEST`.
- **Pipeline cache.** wgpu 30 has no pipeline cache on Metal, so every launch compiles its pipelines
  (62 boot pipelines took 485 ms and 48 game pipelines 377 ms in `boot_probe` on the M5).
- **Paths.** `host_platform()` is `macos`, so dev cooks land in `.regia/cooked/macos`. The source
  watcher reports changed paths under the root it was given (FSEvents reports real paths; `/var` is
  `/private/var`), and the soak memory probe reads `ps -o rss=`.

## Build and test

```sh
cargo check --workspace
cargo test --workspace
```

Install Rust with Homebrew or rustup; the repository pins 1.99.0 and the Homebrew 1.99.1 compiler
works. Install SPIRV-Tools (`spirv-val`) if you want the SPIR-V validation tests to run.

## Known failures

The 27 failing cases are not gated, and none is hidden or re-blessed:

- Golden images that differ on Metal. Goldens are lavapipe images; they are not re-blessed for Metal.
- Tests that encode a Vulkan capability set: timestamps inside encoders, allocator reports,
  `MULTI_DRAW_INDIRECT_COUNT`, forced early depth tests.
- Tests that need `spirv-val`, which is not installed on the test Mac (8 tests fail with "No such
  file").
- The editor hot-reload fixture test `hot_fixture_edits_keep_reflected_state`: the baseline load is
  refused with "engine changed". Hot reload is not verified on macOS.

## Not verified on macOS

Frame-rate budgets, Vulkan SPIR-V validation, native crash capture (`native_crash.rs` is Linux and
Windows only), SpaceMouse (the `hid` backend has no macOS implementation), Steam, audio devices,
the Android and Windows builds from a Mac.

For the rest of the platform story see [Android](android.md) and [Steam Deck](steam-deck.md); the
CI and device matrix is in `docs/06-testing-and-ci.md` § 8.
