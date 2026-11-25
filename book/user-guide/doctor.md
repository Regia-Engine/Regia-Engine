# Checking your machine with `regia doctor`

<!-- index: regia doctor -->
<!-- index: Doctor -->
<!-- index: verify-paks -->

`regia doctor` looks for everything a build needs and prints, for each problem, the command that fixes it.

```console
$ regia doctor                      # Rust, this host's desktop, the GPU probe, Android
$ regia doctor --platform android   # SDK, NDK, JDK, Gradle, adb, cargo-ndk, Rust targets
$ regia doctor --platform windows   # cross builds from Linux: cargo-xwin, MSVC target, clang-cl
$ regia doctor --platform all --json
```

## What each platform checks

| Platform | Checks |
|---|---|
| (every platform but `gpu`) | `rustc`, `cargo`, `rustup`, free disk space where `--project` builds |
| linux | Vulkan driver and devices, `spirv-val` |
| windows | On Windows: the Vulkan loader. From Linux: `cargo-xwin`, the `x86_64-pc-windows-msvc` target, `clang-cl` |
| android | SDK path (`ANDROID_HOME`), SDK platform and build tools, NDK r27 or newer, JDK 17+, Gradle, `adb` and attached devices, `cargo-ndk`, the Android Rust targets |
| deck | `cargo-zigbuild` and zig 0.14+ |
| macos | Xcode command-line tools and `spirv-val` (on the Mac only; testing only) |
| gpu | Every adapter and its ray-tracing support (information only) |

Each line is `ok`, `WARN` (some workflows need it), `FAIL` (this platform cannot build) or blank (a fact).
Run the `fix:` commands, then run the doctor again.

## Disk space

The doctor warns under 15 GiB free (`disk_low`) and fails under 2 GiB (`disk_full`) on the volume that holds the
project. A cold engine build and an Android package take many GiB: `cargo clean`, or set `CARGO_TARGET_DIR` on a
larger volume.

## Exit codes

`0` nothing failed (warnings are fine), `1` a check failed, `2` bad arguments. `--json` prints the same checks for
tools; the schema, every code and the pak-verification form are in [Toolchain doctor (agents)](../agents/doctor.md).

## Pak integrity (`--verify-paks`)

```console
$ regia doctor --verify-paks dist/linux/<slug>/content
$ regia doctor --verify-paks dist/linux/<slug>/content --json
```

This checks the whole-file content hash of every `.fpak` in that directory and runs no other doctor
check. A matching hash is `[ok]`. A pak with no content hash is `[--]` (unhashed) and is not a
failure. A mismatch is `[FAIL]`. One or more corrupt paks is a non-zero exit, with a count. `--json`
prints schema 1, `kind` `regia_doctor_paks`, `ok`, `corrupt`, and `paks`.

Related: [Installing Regia Engine](../start/install.md), [Linux](linux.md), [Android](android.md),
[Package a game](../how-to/platforms/package-a-game.md).
