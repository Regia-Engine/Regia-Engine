# Toolchain doctor (agents)

`regia doctor` checks the toolchains a platform needs and prints a fix for each problem. `--json` prints the same
checks as one JSON document, for an agent that sets up a machine or explains a failed build (ADR-1379).

```console
$ regia doctor                       # Rust, this host's desktop, Android (warnings only), the GPU probe
$ regia doctor --platform windows    # cross-build tools on Linux; the Vulkan loader on Windows
$ regia doctor --platform deck --json
$ regia doctor --platform all --json
```

`--platform` takes `linux`, `windows`, `android`, `macos`, `deck`, `gpu` or `all`. Any other value is an error
that lists these. The exit code is `0` when no check fails (warnings do not count), `1` when a check fails, and `2` when the arguments
are wrong (an unknown `--platform`; clap's own usage errors also exit 2), so `regia doctor && regia package ...`
stops. Tool checks run in `--project` (default `.`), so a `rust-toolchain.toml` there picks the
toolchain whose version and targets are reported.

## The JSON document (schema 1)

```json
{
  "schema": 1,
  "kind": "regia_doctor",
  "host": "linux",
  "ok": false,
  "sections": ["rust", "windows", "gpu"],
  "failed": ["cargo_xwin_missing"],
  "warnings": 1,
  "checks": [
    {"section": "rust", "name": "rustc", "status": "ok", "code": "ok", "detail": "rustc 1.99.0 (...)", "fix": null},
    {"section": "windows", "name": "cargo-xwin", "status": "fail", "code": "cargo_xwin_missing",
     "detail": "not found; ...", "fix": "cargo install cargo-xwin --locked"}
  ]
}
```

`status` is `ok`, `warn`, `fail` or `info`. Every `warn` and `fail` has a `fix`. `info` is never a problem (the GPU
ray-tracing probe, macOS off the Mac).

## Codes

Codes are lowercase ASCII, digits and `_`, and stable: a script may match on them.

| Section | Code | Status | Meaning |
|---|---|---|---|
| any | `ok` | ok / info | Found and usable, or a fact. |
| rust | `rustc_missing`, `cargo_missing` | fail | The tool did not run. |
| rust, windows, android | `rustup_missing` | warn (rust) / fail (a target check) | No rustup: distro rustc builds desktop targets, cross targets need rustup. |
| linux | `vulkan_no_device` | fail | `vulkaninfo` lists no device. |
| linux | `vulkaninfo_missing` | warn | A driver manifest is installed, `vulkaninfo` is not. |
| linux | `vulkan_missing` | fail | No `vulkaninfo` and no driver manifest. |
| linux, macos | `spirv_val_missing` | warn | The SPIR-V validation tests need `spirv-val`. |
| windows (on Windows) | `vulkan_loader_missing` | warn | No `vulkan-1.dll`; Regia Engine falls back to Direct3D 12. |
| linux, windows, macos (the host's own section) | `editor_launcher_missing` | info | The Regia Editor is not in this desktop's app launcher: `regia editor install-launcher` adds it ([Editor launcher](editor-launcher.md), ADR-20000). |
| windows (cross) | `cargo_xwin_missing` | fail | Windows packages cross-build with cargo-xwin. |
| windows (cross) | `windows_target_missing` | fail | `rustup target add x86_64-pc-windows-msvc`. |
| windows (cross) | `clang_cl_missing` | warn | cargo-xwin compiles C/C++ dependencies with clang-cl and lld-link. |
| deck | `deck_needs_linux` | fail | The Deck bundle builds on Linux only (or use `xtask dist --from`). |
| deck | `cargo_zigbuild_missing` | fail | Builds against glibc 2.31 need cargo-zigbuild. |
| deck | `zig_missing`, `zig_too_old` | fail | cargo-zigbuild needs zig 0.14 or newer. |
| macos (off the Mac) | `macos_testing_only` | info | macOS is testing only (ADR-264); run the doctor on the Mac. |
| macos (on the Mac) | `xcode_clt_missing` | fail | `xcode-select --install`. |
| android | `android_sdk_missing`, `android_ndk_missing` | fail | SDK not found / no NDK r27+. |
| android | `jdk_missing`, `gradle_missing`, `adb_missing` | fail | The packaging tools. |
| android | `cargo_ndk_missing` | fail | Android builds run through cargo-ndk. |
| rust | `disk_low` | warn | Under 15 GiB free on the volume holding `--project`; a cold build wants that much. |
| rust | `disk_full` | fail | Under 2 GiB free there. Run `cargo clean` or set `CARGO_TARGET_DIR` on a larger volume. |
| android | `android_platform_missing`, `android_build_tools_missing` | fail | The SDK has no `platforms/` or `build-tools/` entry: `sdkmanager "platforms;android-35"` / `"build-tools;35.0.0"`. |
| android | `android_ndk_too_old` | fail | The NDK is older than r27. |
| android | `android_target_missing` | fail | `rustup target add aarch64-linux-android` (or `x86_64-linux-android`). |

`--verify-paks DIR` checks the integrity of every `.fpak` in `DIR` (exit 1 when one is corrupt). With `--json` it prints

```json
{"schema": 1, "kind": "regia_doctor_paks", "ok": true, "corrupt": 0,
 "paks": [{"path": "content/base.fpak", "status": "ok", "detail": null}]}
```

where `status` is `ok`, `unhashed` (no content hash to check) or `corrupt` (`detail` says why).

## Exit codes

| Code | Meaning |
|---|---|
| 0 | No check failed (or every pak is intact). |
| 1 | At least one check failed, a pak is corrupt, or the directory has no paks. The failed codes are in the error line and in `failed`. |
| 2 | Usage error: unknown `--platform`, or an argument clap rejects. |

The text and JSON reports go to stdout, the error line to stderr, so `regia doctor --json | jq` stays valid on failure.

Related: [Installing Regia Engine](../start/install.md), [Test bundles](../user-guide/test-bundles.md),
[Android](../user-guide/android.md).
