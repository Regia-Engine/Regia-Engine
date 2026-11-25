# Package a game

<!-- index: Package -->
<!-- index: Cook -->
<!-- index: AppImage -->
<!-- index: APK -->
<!-- index: Android App Bundle -->
<!-- index: Dedicated server -->
<!-- index: Package parity -->
<!-- index: regia package parity -->
<!-- index: Build container -->
<!-- index: Container image | see: Build container -->
<!-- index: glibc -->

This is the path from a project that already runs in the editor to a file you can hand to someone else. The reference for flags, profiles, and error codes is [Packaging](../../user-guide/packaging.md). Install the tools ([Installing Regia Engine](../../start/install.md)), then confirm the host with [regia doctor](../../user-guide/doctor.md).

You need a project directory that contains `Regia.toml`. Commands below assume the shell is in that directory. `<slug>` is the slug of the project `id`.

## 1. Check the host

```sh
regia doctor --platform linux
```

Fix every `FAIL` line and run it again. Warnings are allowed. Add `--platform windows` before a Windows cross package (cargo-xwin, the MSVC target, clang-cl) and `--platform android` before an APK or AAB (SDK, NDK r27 or newer, JDK 17 or newer, Gradle, adb, cargo-ndk, the Rust Android targets). `--platform deck` checks cargo-zigbuild and zig 0.14 or newer, which a shipping Linux build also uses.

## 2. Say where the engine source is

A package writes the MPL-2.0 notice, the license text, third-party notices, and `CREDITS.txt`. If you changed an engine file and you are shipping that change, set `engine_source_url`. The credit line is on by default and is not a license condition:

```sh
regia package --platform linux --no-engine-credit
```

or `[package] engine_credit = false`. The license page is [Licensing](../../user-guide/licensing.md).

## 3. Package Linux

```sh
regia package --platform linux --profile shipping
```

`regia package` cooks when the paks are missing or stale, builds the launcher, and writes `dist/linux/<slug>-linux.tar.gz`. A dev iteration can use the default profile (`dev`) or `--format dir`, which leaves the staged directory and skips the archive. Shipping links glibc 2.31 (Steam Linux Runtime, which is also what a [Steam Deck](../../user-guide/steam-deck.md) runs). Pass `--glibc host` only when every player has this machine's glibc or newer.

For a direct download as one file, after `appimagetool` is on `PATH`:

```sh
regia package --platform linux --profile shipping --format appimage
```

That fails before the build with `appimagetool_missing` when the tool is absent, and with `appimage_icon_missing` when the project has no PNG icon at `assets/branding/icon.png`.

To compile C and C++ dependencies inside the same old distribution, instead of relinking with cargo-zigbuild alone:

```sh
regia package --platform linux --profile shipping --container steam-sniper
```

`debian12` is the other profile (glibc 2.36) and is not the Steam runtime. You need podman or docker. The result is still a tar, a directory, or an AppImage.

## 4. Package Windows

On Windows, or on Linux once `regia doctor --platform windows` is clean:

```sh
regia package --platform windows --profile shipping
```

The artifact is `dist/windows/<slug>-windows.zip`. A Linux host does not run the exe it just cross-compiled. A named shipping profile fails a console-subsystem exe; new projects mark release builds as GUI programs. See [Windows](../../user-guide/windows.md).

## 5. Package Android

```sh
regia package --platform android --format apk --profile dev
regia package --platform android --format aab --profile shipping
```

A dev APK is debug-signed. A shipping APK or AAB needs the keystore environment variables on [Packaging](../../user-guide/packaging.md). Without them the shipping build fails, because a release build is not given the debug keystore. Install onto a phone with `regia install-android`. The phone steps are [Android device bring-up](../../user-guide/android-bringup.md).

An Android package is not Google Play services. Those services are parked in this version. The browser [product viewer](../../agents/web-player.md) is a different program and is not this APK.

## 6. Package a dedicated server

Linux or Windows only:

```sh
regia package --server --platform linux --dry-run
regia package --server --platform linux
```

The dry run prints what the content filter keeps. The package has no window, renderer, or audio. The game crate needs a `server` feature, as [Packaging](../../user-guide/packaging.md) shows. `--server --platform android` (and macOS, which is not a package platform) exits 2 with `server_platform_unsupported`.

`--format app` is rejected on every platform. macOS is testing-only and has no app-bundle package (ADR-264).

## 7. Check the bytes

Hygiene rejects staged files that match your rules or the built-in secret patterns. It does not compare a dev run with a packaged run. That comparison is parity, in the next step.

```sh
regia package --platform linux --audit-dir dist/linux/<slug> --json
```

Exit 0 is clean. Exit 1 is a finding. Point `--audit-dir` at the extracted or staged tree, or pass `--audit-artifact` a finished zip, tar.gz, APK, or AAB. The rule file is [Package hygiene](../../user-guide/package-hygiene.md).

Then check pak hashes. With `--verify-paks`, doctor runs that check and skips the toolchain checks:

```sh
regia doctor --verify-paks dist/linux/<slug>/content
```

A corrupt pak is a non-zero exit. A pak with no content hash is reported as unhashed and is not a failure by itself.

## 8. Compare a dev run with the package

`regia package parity` (ADR-8428) is a separate command from `regia package`. It is attached before clap, so `--help` on `regia package` does not list it. `regia package parity --help` does.

Give it two traces, or two programs that it runs with `REGIA_ASSET_TRACE` set. There is no `regia play` subcommand. The dev side is a program you already run, such as `regia run` (that command opens a window). The package side is the game binary in the extracted tree. The tool does not play the game itself.

```sh
regia package parity \
  --dev-trace dev.log --dev-root . \
  --package-trace pkg.log --package-root dist/linux/<slug>

regia package parity \
  --dev-command 'regia run' \
  --package-command './my_game' \
  --dev-root . \
  --package-root dist/linux/<slug> \
  --json
```

`--script FILE` checks that the file exists and sets `REGIA_PARITY_SCRIPT` on each child. No engine system reads that variable, so the file does not drive input by itself. Use the two-trace form when you already have logs.

| Exit | Meaning |
|---|---|
| 0 | Every dev open is accounted for in the package |
| 1 | A dev open is missing. Drift is reported and does not fail unless you pass `--strict-drift` |
| 2 | Usage error, `parity_script_missing` (the `--script` file is not there), or `parity_run_failed` |

`parity_script_missing` and `parity_run_failed` are command exits, not agent-protocol error codes. `--report-md` and `--report-json` write the same report to files. The report lists `missing`, `drift`, `dieted`, `cooked`, and `never_touched`. `never_touched` is informational.

`--classify` reads an strace open trace instead of an asset trace:

```sh
regia package parity --classify --trace run.strace --root dist/linux/<slug>
```

Exit 0 prints the report. Exit 2 is a usage error or an unreadable trace, root, or manifest. `--json` and `--markdown` select the form. Repeat `--trace` to pass more than one file, or pass a directory of traces.

The trace format is [Asset access trace](../../agents/asset-access-trace.md). The agent method
`package.parity` is the same comparison without launching a process
([Package parity](../../agents/package-parity.md)). Its error codes are `parity_bad_params`,
`parity_not_a_directory`, and `parity_trace_missing`. It does not take `--strict-drift`.

## Finished check

You are done for one platform when all of the following are true:

1. `regia doctor` for that platform exits 0.
2. The artifact exists: `dist/linux/<slug>-linux.tar.gz`, `dist/windows/<slug>-windows.zip`, `dist/android/<slug>-debug.apk` or the shipping APK, or `dist/android/<slug>.aab`. A `--format dir` run is done when the staged directory is complete and you did not need an archive.
3. The staged tree (extract the archive if you shipped one) contains the game binary, `content/*.fpak`, `build_info.json`, `sbom.json`, `THIRD_PARTY_NOTICES.md`, `REGIA-ENGINE-NOTICE.txt`, `REGIA-ENGINE-LICENSE.txt`, and `CREDITS.txt`.
4. `regia doctor --verify-paks` on `content/` reports every pak intact.
5. `regia package --audit-dir` on that tree exits 0.
6. If you care that a scripted play opens the same assets in the package as in the dev tree, `regia package parity` exits 0 on that script. Skip this step only when you have no scripted path yet. Do not treat a skipped parity run as a passed one.

A shipping profile also refuses editor content, a stray `.fpak` beside `content/base.fpak`, and, for a named profile, the agent server inside the binary. Read the error and the [build profiles](../../user-guide/packaging.md#build-profiles) section. Profiles shrink the package. They do not encrypt it.

The bill of materials you archive beside a release, beyond the `sbom.json` inside the package, is [Bill of materials](../../user-guide/sbom.md). Executable size is [Binary size](../../user-guide/binary-size.md). Engine test bundles are not a game package; they are [Test bundles](../../user-guide/test-bundles.md).
