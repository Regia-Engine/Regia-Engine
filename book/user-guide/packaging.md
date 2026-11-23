# Packaging

Packaging cooks your assets, builds the launcher and assembles a distributable. The pipeline is
`docs/specs/21-tooling-and-packaging.md`.

## Cook

```console
$ regia cook --project my_game --platform linux
$ regia cook --project my_game --platform android --quality fast
```

`cook` converts source assets to per-platform cooked data. `--quality fast` turns off pak lz4 compression;
`--clean` rebuilds from scratch. Cooked output is archived into `.fpak` files (`docs/formats/pak.md`),
the packed content archive the packaged build mounts.

### Texture quality

`[cook] texture_quality` in `Regia.toml` sets the quality every texture cooks at when its `.meta` names no
`quality` of its own:

```toml
[cook]
texture_quality = "draft"   # "final" (default) or "draft"
```

`"final"` cooks colour textures as BC7 on desktop and ASTC 4×4 on Android. `"draft"` cooks them at the low
quality (BC1 for opaque colour, BC3 with alpha, the 6×6 / 8×8 ASTC ladder): it encodes faster and writes smaller
paks while you iterate, and switching back recooks the affected sources. A texture whose `.meta` says
`quality: Some(high)` or `Some(low)` keeps it either way. Terrain layers keep their own fixed quality. Any other
value fails the cook and `regia package` with the message ``[cook] texture_quality `<value>` is not `final` or `draft` ``.

### Machine-readable output (`--json`)

`regia cook`, `regia package` and `regia run` take `--json` for CI, the editor and agents. Stdout then holds
exactly one JSON object on one line, printed when the command ends; every other line goes to stderr: the
cook summary (`cook: …`, `pak: …`), cargo's output and, for `run`, the game's own stdout. A failure still prints
one object, `{"ok": false, "error": {"code": …, "message": …}}`, after the usual `error:` line, and the exit
code is non-zero.

| Command | Keys of the object |
|---|---|
| `regia cook --json` | `ok`, `platform`, `quality`, `texture_quality`, `pak_dir`, `paks` (absolute paths), `cooked`, `skipped`, `imports_skipped`, `warnings` (count), `elapsed_ms` |
| `regia package --json` | `ok`, `artifact` (absolute path of the package: archive, APK/AAB or directory; the output directory for `--dry-run`), `platform`, `profile`, `server`, `dry_run`, `symbols` and `build_info` (paths, or `null` when absent) |
| `regia run --json` | `ok` (the game exited 0), `artifact` (the launcher binary cargo built), `artifact_exists`, `exit_code` (`null` when a signal ended the game), `package`, `profile` (`debug` or `release`), `level`, `cooked`, `cook_skipped` |

Error codes: `cook_failed`, `package_failed`, `run_failed`, and `server_platform_unsupported` (exit code 2) for
`regia package --server --platform android`. Arguments clap rejects (an unknown `--platform`) print clap's usage
error, not an object.

`--json` on `regia cook` with `--stats`, `--size-report` or `--lightmap-tiers`, and `regia cook cache … --json`,
print those reports as before (pretty-printed).

Paks count as fresh for `regia package` only when the last pack finished and was made from the
current project manifest (`Regia.toml`, or `Ferrite.toml` in a project that has only that one), the
current asset database, and compressed. `build/paks/<platform>/package-pack.stamp` records that; it
never ships. So the paks of `regia cook --quality fast` are packed again by `regia package`, while
those of `regia cook --quality final` (or `--ship`, GPU-ready meshes included) are staged as they are.
When the environment asks the package for GPU-ready meshes (`REGIA_PAK_GPU_MESHES=1`), paks with
cooked meshes are packed again (ADR-7612).

A pack fails with `chunk_asset_unwritten` when the chunk rules put assets in a chunk whose cooked
file is missing (a cook that skipped or failed them with only a warning): they would ship in no pak.
Fix the cook's warnings and cook again, or set `REGIA_ALLOW_UNWRITTEN_ASSETS=1` for a deliberate
partial pack. A cooked cache without `asset_manifest.bin` (no cook finished there) has no content to
name a build id after, so packing from it fails too.

## What ships in a package

A packaged build is self-contained: the game binary, its `content/*.fpak` archives, and everything it
reads through the VFS.

- **Cooked assets** (`.gltf`/`.glb` meshes, textures, materials, …) are cooked into `content/*.fpak`
  and mounted automatically. The launcher finds its paks **relative to its own path**
  (`<exe dir>/content`), so the extracted package runs from any directory — no `--content` flag.
- **Raw assets** — anything under `assets/` that is not a cookable source (`about.txt`, config files,
  fonts, notes) — are copied verbatim next to the binary into `assets/`, where the `assets://` VFS
  mount resolves them.
- **Notices**: `THIRD_PARTY_NOTICES.md` is copied from the workspace root into the package root, with the
  workspace's own `LICENSE.md` when the game has one. Every package also gets the engine's MPL-2.0 notice
  with the source-location line (`REGIA-ENGINE-NOTICE.txt`), the MPL text (`REGIA-ENGINE-LICENSE.txt`) and
  `CREDITS.txt` with the default "Made with Regia Engine" line; `--no-engine-credit` or
  `[package] engine_credit = false` leaves the credit out ([Licensing](licensing.md), ADR-10225).
- **Software bill of materials**: `sbom.json` sits beside the notices. It records the resolved Cargo
  packages' names, versions and license identifiers (or license-file labels), sorted by name. Its
  `format_version` is `1`; missing license metadata is recorded as `unknown`. The list is the packages the
  package's platform resolves (`cargo metadata --filter-platform <target>`: the build's target triple, or the
  host's for a host build; every ABI for Android), so another platform's crates are not listed, and a host
  that only ever built this platform needs nothing more downloaded (`--frozen`).

## Package

```console
$ regia package --platform linux                              # → dist/linux/<slug>-linux.tar.gz
$ regia package --platform windows                            # → dist/windows/<slug>-windows.zip
$ regia package --platform android --format apk               # → dist/android/<slug>-debug.apk (Dev)
$ regia package --platform android --format apk --profile shipping  # → dist/android/<slug>-release.apk
$ regia package --platform android --format aab               # → dist/android/<slug>.aab (Play Store)
```

Linux also builds an AppImage (`--format appimage`, or `formats = ["tar", "appimage"]` under
`[platforms.linux]` in `Regia.toml`; see [Linux AppImage](#linux-appimage)). `[platforms.<p>]
render_profile = "MobileHigh"` (any render profile preset: `MobileLow`, `MobileHigh`, `DesktopLow`,
`DesktopHigh`, `DesktopUltra`, `Deck`, or the offline `Cinematic`; or, since v0.7, a quality tier with no preset row:
`VeryLow`, the integrated-GPU floor, or `Medium`, ADR-8844) is written to the package's `build_info.json` as
`render_profile` (canonical spelling); Android packages carry that file in the APK assets. A name that is neither
fails the manifest with `render_profile_unknown` (`Low`, `High` and `Ultra` are not names: use `DesktopLow`,
`DesktopHigh`, `DesktopUltra`). A packaged tier starts as the tier, as Auto would apply it.

At startup the runtime reads that file beside the executable, even when launched from another
working directory; Android reads it from the APK assets root. `AutoQualityPlugin` (included in
`DefaultPlugins`) and `SettingsPlugin` apply the exact preset before `Startup`, including when
the game has no settings file. The order is **player settings > packaged profile > saved or
first-launch Auto detection > runtime default**. Per-setting player overrides still apply. A
package profile is never saved as a player choice. An explicit Auto-detect request replaces it
for this launch; on the next launch the package profile wins again unless the player saved a
preset. Games can still customize `RenderSettings` in their startup systems.

`regia package` also carries the optional `[boot]` table in `build_info.json`, including APK
assets. `BootMoviesPlugin` uses it when no loose `Regia.toml` exists. Explicit `BootMovies` or
plugin configuration wins over a loose manifest, which wins over package metadata. An empty
loose list deliberately disables the packaged movies. Add the media and boot plugins as described
in [Boot movies](../agents/video-boot-and-cues.md); the package does not install plugins for the
game. Named shipping profiles must include their boot media in the asset closure or `include`.

Missing metadata keeps the older startup behavior. Invalid metadata is logged and ignored; an
invalid loose manifest never silently falls back to a different packaged list. Read
`render.get_profile` or `quality.get_detected` → `configuration` for the configured profile's
origin, and `media.boot_state` → `configuration` for the boot list's origin (ADR-7893).

`<slug>` is the slugified project `id` from `Regia.toml`. `regia package` runs the cook
automatically when paks are missing or stale, builds the launcher, and lays out the package
(`content/*.fpak` next to the binary, the `.desktop` file and icon on Linux, the Gradle project on
Android). The default build profile is `Dev` (cargo `dev`, debug-signed); pass `--profile shipping`
for a release build (release-signed). `--out DIR` overrides the output directory. On Android,
`--abi <abi>` (repeatable) builds only the listed ABIs from `[platforms.android] abis`.

On desktop, `--pipeline-caches PATH` (default `<project>/pipeline-caches` when it exists) accepts
loose `.fpc` files, stored feedback ZIPs, or a directory containing either recursively and ships
the selected caches in `content/pipelines/`, so a tester whose GPU and driver
match skips the first-launch shader compile. `regia prepare` warms the same pipelines on the
player's machine before the first start (see [Instant load](../agents/instant-load.md)).
For harvested feedback, repeatable `--pipeline-cache-reference PATH` selects the current game's
shader/wgpu build, including a content salt. Without a reference, feedback caches must match
this engine's unsalted build. See [Pipeline cache harvest](../agents/pipeline-cache-harvest.md)
for limits, integrity checks and exact GPU/driver matching.

## Shipping builds and the agent server

The `regia` crate's default features are `client`, `agent`, `functional-tests`, `ui`, `quick` and
`procgen`, and packaging does not change a
crate's features, so a game that keeps the defaults links the agent-protocol server. A shipping
package built with a named profile **fails `ship_check`** if it contains the server (see "Build profiles" below); the plain `shipping` profile only warns. It
listens on loopback only and requires the per-run token, but a release should not carry
it: depend on `regia` with `default-features = false, features = ["ui"]` for the builds you ship,
in every crate of the build that depends on `regia` (game, launcher, Android crate). Cargo
unifies features, so one crate that keeps the defaults turns the server back on.

## Build profiles

A `[profiles.<name>]` table in `Regia.toml` ships only what its entry levels need:

```toml
[profiles.demo]
entry_levels = ["levels/main.level.ron"]
exclude = ["levels/debug*"]
```

`regia package --platform linux --profile demo --dry-run` cooks if needed and prints exactly the files
the package would hold (cooked assets with sizes, raw files, the totals and what was left out) without
building. Without `--dry-run` the package gets its own `content/base.fpak` with that closure only, and only
the raw files the levels reference. The closure follows each cooked asset's dependency list and the quoted
strings in raw files (`"materials/rock.material.ron"`, `"assets://levels/b.level.ron"`, an asset id). The
keys are `base`, `entry_levels`, `include`, `exclude`, `features`, `no_default_features`, `allow_agent` and
`target` (`"server"` makes it a dedicated-server profile; spec 21 § 4, spec 76 § 3). The closure scan is heuristic: an asset that code loads through a computed path is not
found, so list it in `include`. Each run restages from scratch (`content/` and `assets/` are cleared).

**This is not obfuscation.** Profiles shrink the package; they do not hide or encrypt anything, and anyone
can unpack a `.fpak`.

Every shipping package (built-in `shipping` or a profile) is checked: it fails with a stray `.fpak`
(anything but `content/base.fpak`) or editor content (a root-level `editor/`, `.regia/` or
`assets/editor/`, `*.editor.ron`, autosaves). Packages from a named profile also fail with the agent
server, editor code or the dev asset watcher in the binary, or those features in the game's `Cargo.toml`. A playtest profile can keep the agent
with `allow_agent = true`.

## Dedicated servers

`regia package --server --platform linux` (or `windows`) packages the game with no window, renderer or audio: the
game's `server` feature, a content filter that removes presentation assets, a default `server.toml`, an empty `bans.ron`
and a `README-server.txt`. The game declares the feature and builds `regia` without its defaults:

```toml
[dependencies]
regia = { path = "...", default-features = false }

[features]
default = ["client"]
client = ["regia/client", "regia/agent", "regia/ui"]
server = ["regia/server"]
```

Add `--dry-run` to see what is kept and what the filter removes. `regia server` runs the same build from the dev cache,
and `regia server --clients 2` adds two headless clients. `samples/dedicated_server` is a complete small example;
[Net dedicated server (agents)](../agents/net-dedicated-server.md) has the details.

## Android

```console
$ regia install-android --device <serial>
```

`install-android` packages, installs via adb, launches, and streams logcat. Android packaging is the
Gradle shim from `docs/specs/21` § 4: a debug-signed APK for dev, and (with signing keys in
`Regia.toml`) a release APK/AAB for stores. The Android surface is recreated on suspend and
resume — the packaged build is tested through the suspend/resume cycle every milestone.

### Android release signing

Release builds (`--profile shipping`) sign with a keystore configured under `[platforms.android]` in
`Regia.toml`. Secrets are `env:` references so they are never committed:

```toml
[platforms.android]
keystore = "env:REGIA_ANDROID_KEYSTORE"          # path to the .jks/.keystore
keystore_password = "env:REGIA_ANDROID_KEYSTORE_PASSWORD"
key_alias = "env:REGIA_ANDROID_KEY_ALIAS"
key_password = "env:REGIA_ANDROID_KEY_PASSWORD"  # defaults to the keystore password
```

With those set, build the Play-Store bundle (the lead's NDK run):

```console
$ REGIA_ANDROID_KEYSTORE=/secure/upload.jks \
  REGIA_ANDROID_KEYSTORE_PASSWORD=… \
  REGIA_ANDROID_KEY_ALIAS=upload \
  REGIA_ANDROID_KEY_PASSWORD=… \
  regia package --platform android --format aab --profile shipping --project samples/sim_bridge
```

Without a keystore, `--profile shipping` fails with an error: AGP 8 does not fall back to the debug
keystore for release builds, so the APK/AAB would be unsigned (and uninstallable). Build with
`--profile dev` for a debug-signed APK, or configure the keystore above.

## Crash reports and symbols

When a game panics, the platform writes a self-contained crash report to `logs://crash-<timestamp>.txt`
(panic message + backtrace, build identity, OS/GPU facts, the active level and frame count, and the
last 200 log lines). An opt-in `regia_crash::CrashReporter` (`regia::crash`, umbrella `crash`
feature) uploads pending reports — and a game-provided match log — to a configured endpoint on the
next start (retry/backoff, size cap, a consent flag; nothing is sent unless the game opts in).
`https://` endpoints need `regia_crash`'s `tls` feature (ureq + rustls); without it `https://` is
refused. A remote `http://` endpoint leaves uploads off unless the game sets `allow_cleartext`. See [Crash reports](../agents/crash-reports.md) for the
`diagnostics.crash_reports` method that lists/reads the local copies.

Shipping builds strip the shipped binary but keep the debug symbols separately: `--profile shipping`
writes `dist/<p>/symbols/` next to the package — on Linux `<name>.debug`, extracted with
`objcopy --only-keep-debug` from the (unstripped) build and address-resolvable, on Windows the
MSVC linker's `<name>.pdb` (stored as `symbols/<name>.pdb`; the Windows exe is not stripped because
MSVC writes all debug info into the `.pdb`, leaving nothing embedded in the exe). Symbolicate a
shipped backtrace on Linux with `addr2line -C -e symbols/<name>.debug -f <address>`; on Windows load
the `.pdb` in WinDbg or Visual Studio.

### Android symbols

Android shipping builds keep the unstripped libraries before Gradle runs (AGP strips the copy inside
the APK or AAB). `--profile shipping` copies each ABI's `lib<name>.so` from the cargo-ndk output into

```text
dist/android/symbols/
  symbols.json              # format_version 1: version, build_number, per library abi/file/build_id/size
  arm64-v8a/libmain.so      # unstripped
  x86_64/libmain.so
```

Keep this folder with the release it belongs to: `symbols.json` records each library's GNU build id,
which matches the build id of the library in the shipped APK (`readelf -n`). The step fails with
`android_symbols_missing` (an ABI has no library), `android_symbols_not_elf` or
`android_symbols_stripped` (no symbol table or debug info: keep `strip = "none"` in the shipping
profile). A library without a build id is recorded as `null` and warned about.

Symbolicate a native crash from the Android NDK (the `ndk-stack` and `llvm-addr2line` tools live in
`$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/<host>/bin`, and `ndk-stack` in the NDK root):

```console
$ adb logcat -d | ndk-stack -sym dist/android/symbols/arm64-v8a            # a tombstone or logcat crash
$ llvm-addr2line -C -f -e dist/android/symbols/arm64-v8a/libmain.so 0x1a2b3c   # one pc offset
```

`ndk-stack` picks the library by name from the `-sym` directory; use the `symbols/` of the release
that crashed (compare the `Build ID` line in the tombstone with `symbols.json`).

## Linux AppImage

```console
$ regia package --platform linux --format appimage       # → dist/linux/<slug>-<arch>.AppImage
```

The package is laid out as an AppDir and handed to `appimagetool`:

```text
<slug>.AppDir/
  AppRun                  # sh script, mode 0755: runs usr/bin/<binary>
  <slug>.desktop          # Type, Name, Exec=<binary>, Icon=<slug>, Categories=Game;
  <slug>.png              # the project icon (assets/branding/icon.png), required
  .DirIcon -> <slug>.png
  usr/bin/<binary>        # the game, with content/, assets/ and build_info.json beside it
```

Regia Engine does not bundle or download `appimagetool`. It uses the file named by
`REGIA_APPIMAGETOOL`, otherwise `appimagetool` on `PATH`; with neither the package fails before
the build starts with `appimagetool_missing`. Get the tool from the appimagetool releases page
(`appimagetool-<arch>.AppImage`), `chmod +x` it, and put it on `PATH` as `appimagetool`. Other
errors: `appimage_icon_missing` (no PNG icon) and `appimage_failed` (the tool exited with an error
or wrote no file). The tool runs with `APPIMAGE_EXTRACT_AND_RUN=1`, so FUSE is not needed to build.
An AppImage is for direct downloads; Steam depots keep the tarball layout (`--steam` needs no AppImage).

## Clean

```console
$ regia clean --dry-run            # list what the default level would remove
$ regia clean                      # the cook and asset-database caches
$ regia clean --cache              # also the shader and pipeline caches
$ regia clean --all                # also build/, dist/ and target/
$ regia clean --all --dry-run --json   # the plan as one JSON object (scripts, CI)
```

`--json` prints one object on stdout instead of the text lines, after the removal:
`{ "dry_run", "level": "default" | "cache" | "all", "removed", "paths": [{ "path", "kind": "dir" | "file" |
"symlink", "bytes" }], "bytes" }`, with project-relative `/`-separated paths. A failure prints no object; the
error and its code go to stderr as usual.

| Level | Removes (relative to the project root) |
|---|---|
| default | `.regia/cooked`, `.regia/assetdb` |
| `--cache` | the default set, `.regia/shader-cache`, `.regia/pipeline-cache` |
| `--all` | the `--cache` set, `build/`, `dist/`, `target/` |

Never removed: sources, `assets/`, `Regia.toml`, `pipeline-caches/` (the caches a package ships) and the rest of
`.regia/` (agent tokens, autosaves, review state). `regia clean` runs only in a project root (a directory with
`Regia.toml`; `--project DIR` selects another), checks the whole plan before the first deletion, refuses a target
whose parent resolves outside the project, and removes a symlink as the link only, never the folder it points to.

Exit codes: 0 (also when there is nothing to remove), 2 for `clean_not_a_project` and for
conflicting flags (`--cache` with `--all`), 1 for `clean_outside_project` (nothing is removed) and `clean_failed`.

## Windows shipping builds: no console window

A Windows program is either a console program, which opens a console window when it starts outside a
terminal, or a GUI program. A game should be a GUI program. New projects' game mains (the launcher,
the quick app, the net client) start with

```rust,ignore
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
```

so release and shipping builds are GUI programs and debug builds keep the console for logs. A project
made before v0.2.13 can add that line at the top of `launcher/src/main.rs` (after the `//!` doc
comment). The dedicated server of the net template stays a console program.

`regia package --platform windows --profile shipping` reads the staged exe's PE header: a console
exe is a warning on the plain `shipping` profile and fails a named `[profiles.<name>]` build profile
(ADR-2306).

## Windows icon and version info

Explorer, the taskbar and the exe's Properties > Details tab read two resources from the exe: an
icon and the version info. New projects embed both from a build script beside each game main:
`launcher/build.rs` (the `quick` template: `build.rs`; the net template: `client/build.rs` and
`server/build.rs`). It does
nothing on other targets. On a Windows target it reads `Regia.toml`:

```toml
[project]
name = "Night Train"            # ProductName and FileDescription
version = "1.4.2"               # FileVersion, ProductVersion and the binary file version
organization = "Example Studio" # CompanyName

[platforms.windows]
icon = "assets/branding/icon.png"
```

The icon is a PNG, square and 256 pixels or larger for the sharpest result. The build script turns
it into an `.ico` with 16, 32, 48 and 256 pixel images (a smaller or non-square PNG is scaled and
centred). Without `[platforms.windows] icon` it uses `assets/branding/icon.png` or `assets/icon.png`.
With none of these the exe gets a placeholder icon and release builds print a cargo warning. The
`winresource` crate compiles the resources. That needs the Windows SDK's `rc.exe` (installed with the
MSVC build tools) or, when cross-compiling from Linux, `llvm-rc` on `PATH` (set `RC_PATH` to use
another one). Without a resource compiler the build prints a warning and continues, and the exe has
no icon and no version info.

`regia package --platform windows` reads the staged exe's resources back. It warns when the icon group
(`RT_GROUP_ICON` with its `RT_ICON` images) or the version info (`RT_VERSION`) is missing, or when
the exe's `FileVersion` differs from `[project] version`. A named `[profiles.<name>]` shipping profile
fails instead (ADR-2645). An older project gets the build script with `regia upgrade-project
--write`. It adds `build.rs` and the `[build-dependencies]` it needs (`winresource`,
`png`, `toml`) and backs up the crate's `Cargo.toml` as `Cargo.toml.bak`. A crate that already has a
build script of its own is left alone: copy the resource code from a new project's `launcher/build.rs`
into it.

To check a build on Windows, open the exe's Properties > Details, or in PowerShell:

```text
(Get-Item .\dist\windows\<slug>\night_train.exe).VersionInfo | Format-List
```

## Linux shipping builds and the Steam runtime

Steam runs native Linux games, on desktops and on the Steam Deck, inside Steam Linux Runtime 3.0
("sniper", Debian 11, glibc 2.31). A binary linked against a newer distro's glibc does not start
there, so `--profile shipping` Linux packages link against glibc 2.31 by default:

```console
$ cargo install --locked cargo-zigbuild        # once; also put zig 0.14+ on PATH
$ regia package --platform linux --profile shipping           # glibc 2.31 (Steam runtime)
$ regia package --platform linux --profile shipping --glibc host   # this machine's glibc
```

After staging, the package step checks the binary and fails on a problem: the newest `GLIBC_x.y`
it imports (read from the binary's version-needed table) must not exceed the floor (`build_info.json` records `glibc_floor` and `glibc_max`),
and a shipping binary must not contain the build machine's repo, home or cargo-home path, nor the
dev asset watcher (do not enable `regia/dev-assets` in shipped builds). Libraries the runtime
lacks are reported as warnings: bundle them next to the binary (shipping builds link an `$ORIGIN`
rpath, which is also how `libsteam_api.so` is found).

### Build containers

cargo-zigbuild relinks the game against an older glibc's symbol list, but C and C++ dependencies
still compile with this machine's headers and `libstdc++`. `--container` builds the whole binary
inside a real older distribution instead (C-26, ADR-8835):

```console
$ regia package --platform linux --profile shipping --container steam-sniper   # glibc 2.31
$ regia package --platform linux --profile shipping --container debian12       # glibc 2.36
```

| Profile | Base image (pinned by digest) | glibc | Notes |
|---|---|---|---|
| `steam-sniper` | Steam Linux Runtime 3.0 "sniper" SDK, `3.0.20261110.268881` | 2.31 | C and C++ build with clang (`CC=clang`, `CXX=clang++`); the depot for Steam and the Steam Deck |
| `debian12` | Debian 12 "bookworm", `bookworm-20261104` | 2.36 | distributions from about 2023 on; not for `--steam` |

- **Container tool.** podman, else docker, found on `PATH` when the command runs;
  `REGIA_CONTAINER_TOOL=podman|docker` picks one. Without either the package fails with
  `container_tool_missing`.
- **Image.** The first run builds a toolchain image `localhost/regia-build-<profile>:<rustc
  version>-<digest>` from the pinned base (pulled once) with the same Rust version as this
  machine, and reuses it afterwards. The Containerfile is written to
  `<target>/container/<profile>/image/`. `--container-image <image>` runs in an image you
  prepared instead (it must have cargo); it must be pinned by digest (`<image>@sha256:<hex>`) or
  be a local image with a version tag (`localhost/<name>:<tag>`), never `latest`.
- **Mounts.** The cargo workspace is mounted read-only at its own path, so the build runs
  `cargo build --locked` (commit `Cargo.lock`). A path dependency outside the workspace needs
  `--container-mount <dir>` (repeatable, read-only). The target directory and cargo home are
  per profile, `<target>/container/<profile>/{target,cargo-home}`, so a container build never
  reuses artifacts linked against another glibc. docker runs the build as the workspace owner;
  rootless podman needs nothing.
- **Check.** `--glibc` is optional and must equal the profile's glibc. The staged binary passes the
  same floor check as every Linux package, read from its ELF version table (no binutils needed).

Error codes (`regia package --json` prints them as `error.code`): `container_platform_unsupported`,
`container_host_unsupported`, `container_tool_missing`, `container_tool_unknown`,
`container_image_unpinned`, `container_image_missing`, `container_glibc_conflict`,
`container_lockfile_missing`, `container_mount_missing`, `container_build_failed`; see
[Cook and package from the protocol](../agents/cook-package.md#build-containers).

Test the package inside the runtime itself before uploading. Download
`SteamLinuxRuntime_sniper.tar.xz` from
`https://repo.steampowered.com/steamrt-images-sniper/snapshots/latest-container-runtime-public-beta/`
(check `SHA256SUMS`), extract it, and run the game through it:

```console
$ SteamLinuxRuntime_sniper/run -- ./arena --exit-after-frames 600
```

`cargo test -p regia_cli --test package_e2e packaged_arena -- --ignored` packages `samples/arena`
this way, runs it from a clean directory, and captures the presented frame through the agent
protocol (it must not be black).

Proton: the Windows build prefers a Vulkan adapter, which Proton forwards to the host driver, so no
DXVK translation is involved. Ship the native Linux depot; Proton is the player's override.

## Steam packaging

`regia package --steam` additionally stages the Steam depot layout and steamcmd VDFs
(`steam_appid.txt`, the Steamworks redistributable, `steamcloud.md`, `app_build_<appid>.vdf`,
`depot_build_<depotid>.vdf`, and `game_actions_<appid>.vdf` when the project has
`config/input.ron`) under `<out>/steam`. Publish with `regia steam upload` — see
[Steam](steam.md).

## Steam Deck

The Deck runs the same native Linux package as any Steam Linux machine (glibc 2.31 floor, tested
in Steam Linux Runtime 3.0). Before uploading a build for it:

- **Controller configuration.** Ship a default Steam Input template (gamepad, with the D-pad and
  left stick on menu navigation, A confirm, B back) and let Steam show your glyphs. The engine's
  UI takes its navigation from the gamepad and keyboard, so either template works.
- **Launch option.** Set the Linux depot launch option to the binary; nothing else is needed.
  `SteamDeck=1` in the environment turns on the Deck's UI text floor
  (`UiDevicePolicy::detect`); `regia_steam` detects the Deck through Steam and selects the
  `Deck` render profile and the frame-cap setting.
- **Deck Verified checklist.** Run through the table in [Steam Deck](steam-deck.md): controller
  operation of every screen, text legibility, on-screen keyboard for text entry, default
  resolution, no mouse-only launcher or prompt, sleep and resume, quit.
- **Test on the device.** [Steam Deck](steam-deck.md) has the one-command measurement run and the
  sleep, cap and quit checks. Proton (the Windows build) is a fallback only; test the native build
  first.

## Lightmap seams

If the project ships baked lightmaps (`.flmap`), a shipping `regia package` reads the chart-seam figures stored in each
one and fails with `lightmap_seam_budget`, naming the entries, when one passes the budget. Set it in `Regia.toml`:

```toml
[lightmaps]
seam_budget = 0.25 # the default: the seam p95, as a relative luminance step
```

A project with no lightmaps never meets this. A file baked before format version 3 has no figures: a warning, not a
failure. See [Lightmaps](../agents/lightmaps.md).

## Smoke-test a package before shipping

The 60-second timer begins after startup and all recorded input steps complete; shader warmup is excluded.
`cargo xtask package-smoke` packages `samples/sim_bridge` and a generated third-person project at the dev and
shipping profiles, extracts each archive, and runs the game for 60 seconds with a scripted input recording. The
game exits itself (`REGIA_SMOKE_SECONDS`) so the session can write `summary.json`. The run fails when the JSON
log has an `ERROR` record, when playback never starts, when `summary.json` is missing or not a clean exit, or
when a shipping APK is over 150 MiB.

On Linux the games run under `xvfb` with lavapipe. `--android` also builds the sim_bridge APK and AAB (the
manifest ABIs, arm64-v8a and x86_64) and records their sizes. `--android-emulator` boots an x86_64 API 35 image
when `/dev/kvm` can be opened read-write, installs the dev APK, and fails on a crash or an error line in that
process's logcat. Without a usable KVM device the same command prints the emulator command lines and still
finishes the rest of the run. `--dry-run` prints those commands and does not boot.

```sh
cargo xtask package-smoke
cargo xtask package-smoke --android
cargo xtask package-smoke --android-emulator --dry-run
```

The report is `target/package-smoke/package-smoke.json` (`format_version` 1). It is a CI report, not a content
format. The size table is also printed.

Windows zip smoke runs on Windows:

```sh
cargo xtask package-smoke --windows
```

## When a package audit stops early

A shipping package check can fail in two different ways.

A **policy failure** means the audit finished. The package contains a forbidden string, a deny term, or a recognized secret signature. The report lists each hit as a path, a rule name, whether it was in the file name or the file bytes, and a byte offset. It does not include the matched text. Fix or review the file, or add a narrow allowance with a reason, then package again.

An **incomplete audit** means the checker stopped before it had looked at every selected byte. The package is not approved and it is not rejected for a specific finding. The report's `complete` field is false and `incomplete_reason` names the ceiling: the file is larger than the memory budget and a pattern cannot be streamed, a pak entry declares more bytes than the audit will decompress, the pak and a decoded entry would have to sit in memory together, a file changed while it was being read, or a byte, entry, or work limit was hit. Raise the matching limit in `[package_hygiene]` only when the package truly needs it. Leaving secret scanning on and raising nothing refuses files larger than 32 MiB, because those signatures are not split across a window.

```toml
[package_hygiene]
max_memory_bytes = 33554432
scan_window_bytes = 1048576
max_matcher_bytes = 8388608
max_work = 1099511627776
```

Project Settings writes these six numbers without changing forbidden strings or allowances. `regia package --audit-dir DIR --json` prints the same `complete` flag, the reason, the limits that were in force, and the peak memory and work the audit actually used.

On Linux that flag prints the commands and does not build. The owner's Windows a test machine is the host for that
run. Re-record the committed playback files with `scripts/write-package-smoke-recordings.sh`. See
[Package smoke](../agents/package-smoke.md).

## Reviewing package hygiene findings

Audit an extracted, immutable staging directory before publishing:

```sh
regia package --platform linux --project . --audit-dir dist/linux/player --json
```

Configure `[package_hygiene]` in `Regia.toml`; the [package hygiene guide](../agents/package-hygiene.md)
lists policy rules and error codes. Auditing reads files without rewriting them or starting a cook.
Findings contain a scrubbed `path`, an opaque `location_id`, a rule ID, source and byte offset.
They never include matched snippets. Sensitive filenames are scrubbed in size-limit, IO and
pak errors as well as completed reports; JSON and terminal output use the same safe data.

For example, two synthetic private filenames can both appear as `[redacted]`, with
`location-1` and `location-2` identifying them separately. These IDs are traversal ordinals,
not encoded or hashed filenames. Preserve the staging tree while reviewing; IDs can change
when entries change. Pak entries retain their asset IDs instead of manifest paths.

An authorized local tool can rerun `regia_cli::package_hygiene::audit_tree_with_locations`
to resolve an ID to its original normalized path through an explicit callback. Keep that
callback's values local; they are sensitive and must not be copied into shared logs or agent
responses. Normal CLI/agent reports have no reveal switch or embedded mapping. Invalid names
are not revealed. Use the original path, not `[redacted]` or the ID, for a reviewed allowance.
Redaction recognizes configured values and supported secret shapes; add project rules for
private names it cannot infer. See the guide's synthetic examples and local review contract.
