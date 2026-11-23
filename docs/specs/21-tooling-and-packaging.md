# Spec 21 — Tooling (`regia_cli`) and packaging

## 1. `regia` CLI

```
regia new <name> [--template blank|first_person|third_person|top_down|quick|net_game] [--path DIR]
regia editor [PROJECT_DIR] [--headless]  # runs the regia-editor binary on the project; --headless: no window (specs/23 § 2, ADR-2412)
regia run [--release] [--dev] [--level L] [--json]   # desktop dev run of launcher; --dev = live asset reload (specs/12 § 8, ADR-126); --level = start in L (REGIA_START_LEVEL, opened after the game's Startup)
regia preview <asset> [--exit-after-frames N] # windowed standalone asset viewer (M4, specs/12 § 9)
regia cook --platform <linux|windows|android> [--quality fast|final] [--clean] [--json]     # macos: no cook target yet (ADR-009, ADR-264)
regia cook ... [--ship | --gpu-meshes] | --pack-report   # cooked packs (ADR-1594): GPU-ready desktop meshes, cooked-pack check + build/paks/<p>/cooked_pack.json; --pack-report re-checks existing paks
regia cook ... [--shared-cache PATH [--read-only] | --no-shared-cache]   # shared cook cache (ADR-2612): fetch other machines' cooked outputs, publish this cook's
regia cook cache prune --max-size SIZE | stats [--json] [--shared-cache PATH]   # trim the shared cook cache (least recently used first) or print its totals and the last cook's summary
regia package --platform <p> [--profile dev|shipping] [--format apk|aab|dir|tar|appimage|zip|app] [--out DIR] [--json]
regia install-android [--device ID]   # package + adb install + launch + logcat
regia doctor [--platform p]           # checks toolchains and prints fixes
regia prepare --cache-dir DIR [--shipped-dir DIR] [--salt S] [--profile NAME] [--threads N] [--timeout-secs N] [--no-nice] [--force | --check | --status]   # headless shader warm + stamp; stdout is JSON lines (exit 2 from --check when a prepare is still needed)
regia clean [--cache|--all] [--dry-run] [--json]   # default: cook + asset-db caches; --cache adds shader/pipeline caches; --all adds build/, dist/, target/ (ADR-2631)
regia upgrade-project                 # migrates project files to the current engine version
regia mcp [--project DIR]   # (--attach is accepted but ignored; --headless and --http are planned) MCP server for AI agents (specs/23)
regia agent call <method> [json]      # one-off agent-protocol call (scripts, CI)
regia gen run <name> | verify         # PLANNED, not built; run / verify generators (specs/23 § 6)
regia steam upload --branch <b> [--live]   # steamcmd upload of a --steam package (specs/24)
```
- Pure Rust; uses `std::process::Command` for cargo, gradle, adb, codesign. Human output by default,
  `--json` for CI/editor integration. As built (ADR-7217): `regia cook` (a plain cook too, not only its
  `--stats`/`--size-report`/`--lightmap-tiers` reports), `regia package` and `regia run` print exactly one
  compact JSON object on one stdout line at the end (`ok`; `artifact` for package and run; `ok: false` with
  `error: {code, message}` on failure) and send every other line, child processes' stdout included, to stderr
  (`book/user-guide/packaging.md` § Machine-readable output). `regia prepare` is the exception: its stdout is always JSON lines
  (a launcher progress bar); human text stays on stderr. The editor calls the same library functions (`regia_cli` is a
  thin `main.rs` over a `regia_cli::commands` module — move that module into a library crate
  `regia_tools` if the editor needs it, via ADR).

## 2. Project manifest `Regia.toml`
```toml
[project]
name = "My Game"
id = "com.example.mygame"           # reverse-DNS; used for Android applicationId, macOS bundle id, user data dir
version = "1.0.0"
build_number = 1                     # Android versionCode, macOS CFBundleVersion
engine = "0.1"
startup_level = "levels/main_menu.level.ron"
organization = "Example Studio"

[platforms.android]
min_sdk = 29
target_sdk = 35
orientation = "sensor_landscape"
abis = ["arm64-v8a"]                 # add "x86_64" for emulator builds
permissions = []                     # e.g. "android.permission.INTERNET"
icon = "assets/branding/icon.png"
adaptive_icon_foreground = "assets/branding/icon_fg.png"
keystore = "env:REGIA_ANDROID_KEYSTORE"   # never a committed path to a key; env or file outside repo
render_profile = "MobileHigh"
cargo_package = "android"            # cargo package of the Android cdylib (lib name "main"), ADR-012

[platforms.windows]
icon = "assets/branding/icon.png"
render_profile = "DesktopHigh"        # written to the package's build_info.json (all platforms, ADR-2631)

[platforms.macos]                    # deferred (ADR-009): parsed and ignored until the macOS port
icon = "assets/branding/icon.png"
sign_identity = "env:REGIA_MACOS_IDENTITY"   # optional
notarize = false

[platforms.linux]
formats = ["tar", "appimage"]         # tar | dir | appimage (appimagetool on PATH or REGIA_APPIMAGETOOL, ADR-2631)
render_profile = "DesktopHigh"

[cook]
chunks = { base = ["**"] }
texture_quality = "final"

[runtime]                            # optional, all keys default off (ADR-7070, docs/formats/regia-toml-runtime.md)
pause_on_focus_loss = false          # pause gameplay time while the window is out of focus
keep_screen_on = false               # Android FLAG_KEEP_SCREEN_ON while the game runs
```
As built (ADR-7217): `texture_quality` is `"final"` (default) or `"draft"`; the CLI manifest refuses any other
value and the cook reads it (`regia_asset_pipeline::ProjectTextureQuality`). `draft` cooks every texture whose
`.meta` names no `quality` at the low quality (BC1/BC3, the low ASTC ladder) instead of the high one (BC7, ASTC
4×4); the cache keys on it only when it is `draft`, so existing caches stay valid. Terrain layers keep their fixed
quality.

## 3. Templates (`crates/regia_templates/templates/`, ADR-1485)
Each template is a directory with `{{name}}`, `{{id}}`, `{{crate_name}}` placeholders; `regia new`
copies and substitutes. Contents follow `02-architecture.md` § 10, plus a `README.md` for the user,
`.gitignore`, starter assets with permissive licences (CC0) and a starter level.
Templates are tested in CI: `regia new` → `regia package --platform linux` → run headless smoke.
The shipped ids are `blank`, `first_person`, `third_person`, `top_down` (spec 91: overhead camera,
screen-relative movement, F5/F9 quicksave) `quick` and `net_game` (alias `net-game`); `regia_templates::TEMPLATE_IDS` lists all six.

## 4. Packaging per platform

### Common steps
1. `regia cook --platform P --quality final` → paks.
2. `cargo build --profile shipping -p launcher --target T` (or `-p android` via cargo-ndk).
3. Assemble output layout, write `build_info.json` (version, git hash if any, engine version, date).
4. Shipping profile: LTO fat, codegen-units 1, `panic = abort`, `debug = "line-tables-only"`,
   `strip = "none"` (cargo does not strip). `regia package` extracts a symbolication artifact into
   `dist/<p>/symbols/` and strips the shipped binary: Linux runs `objcopy --only-keep-debug` to keep
   `<name>.debug` (address-resolvable — symbolicate with `addr2line -C -e symbols/<name>.debug -f <addr>`)
   then `objcopy --strip-all`; Windows copies the MSVC linker's `<name>.pdb` to `symbols/<name>.pdb`
   (the exe is not stripped — MSVC writes all debug info into the `.pdb`, so there is nothing embedded
   in the exe to strip; `strip = "none"` is correct). A missing artifact fails the package.
   `split-debuginfo = "packed"` is deliberately not used: after `strip = "symbols"` the packed
   `.dwp`'s `DW_AT_low_pc` is unresolved, so `addr2line` cannot resolve addresses (rustc 1.99;
   ADR-032). **Android** shipping builds copy the unstripped `lib<name>.so` of each ABI out of the
   cargo-ndk output into `dist/android/symbols/<abi>/` before Gradle runs (AGP strips the copy in
   the APK), with `symbols.json` (format_version 1: version, build number, per ABI the file, GNU
   build id and size); symbolicate with `ndk-stack -sym dist/android/symbols/<abi>` or
   `llvm-addr2line -C -f -e dist/android/symbols/<abi>/lib<name>.so <pc>` (ADR-2631).
   `[platforms.<p>] render_profile` (a render profile preset name) is written to the package's
   `build_info.json` as `render_profile`.
   Every template's root `Cargo.toml` defines this `[profile.shipping]` with the engine workspace's
   values. A project made before the templates carried it gets the same profile through `--config
   profile.shipping.<key>=…` on the cargo (and cargo-ndk) build; a project that defines the table keeps
   its own (ADR-5627).

### Build profiles
`regia package --profile <name>` takes `dev`, `shipping` or a `[profiles.<name>]` table of `Regia.toml`:
```toml
[profiles.demo]
base = "shipping"                       # cargo profile: "shipping" (default) or "dev"
entry_levels = ["levels/main.level.ron"]  # required; paths under assets/
include = ["ui/**"]                     # extra globs (`*`, `?`, `**`) shipped beyond the closure
exclude = ["levels/debug*"]             # removed; the closure walk does not follow into them
features = []                           # cargo --features for the game package
no_default_features = false             # cargo --no-default-features
allow_agent = false                     # playtest builds only
target = "server"                       # optional: a dedicated-server profile (spec 76 § 3)
```
The package ships the **dependency closure** of the entry levels: cooked assets through the dependency list in each cooked
header; raw files (levels, UI layouts, other `assets://` data) through the **root resolver the chunk cook shares**
(`regia_asset_pipeline::closure_roots`, ADR-8477). Roots are the entry levels, every top-level `ui/*.ui.ron` (loaded at
start), the `*.procgen/` bakes of every level reached, and the `include` globs, which are also how content loaded only by
name (a level opened from a menu, a texture picked in code) is declared as an explicit dynamic root. UI layouts are read
through their typed image/mask fields; levels through their decoded RON string literals (escapes and `\u{..}` decoded), a
string naming a raw file, a cooked asset path, an asset id or a level id; unknown text formats through the legacy generic
quoted-string scan, labeled `fallback` and reported as incomplete typed coverage; binary files are not scanned. A malformed
level or layout (`content_root_malformed`), an unreadable file (`content_root_unreadable`), a missing entry level
(`content_root_missing`) or a raw file over 256 MiB (`content_root_limit`) fails the package. `[project] startup_level` and
chunk `entry` globs go through the same resolver, so equivalent profile and chunk declarations ship the same cooked ids;
`--dry-run`, `regia assets chunks --json` (`roots`) and `regia assets why` (`root`) show why each root exists (`kind`,
`declared_by`, `via`, `boot`). The profile gets its own pak (`build/profiles/<name>/paks/<platform>/base.fpak`) and only
its raw files are copied. `--dry-run` cooks if needed, prints the closure file by file with sizes, the totals and what was left
out, then stops. **Nothing is obfuscated or encrypted**: a profile decides which files ship, and anyone can unpack a `.fpak`.
Built-in `dev` and `shipping` ship every asset, as before.

**Cook diet** (ADR-8461, as built; book `agents/cook-diet.md`): `[content.diet]` (and `[profiles.<name>.diet]`, whose keys
replace) holds `texture_caps = [{ glob, max_edge }]` (the first match wins; `default_max_edge`, default 4096, for the rest),
`max_triangles` (default 1 500 000), `variants = [{ glob, pick = "first" | "seeded" }]` with `seed`, and `path_aliases =
[{ from, to }]`. The cook applies the texture caps per source (keyed in the cache). A named profile's pak applies the triangle
ceiling (the finest LOD of the mesh with the most triangles at its current level drops until the total fits; never a last
LOD) and the variant picks, and writes `diet-report.json` beside the pak (`assets.diet_report`). Without a diet table
nothing changes.

`ship_check` (shipping builds, every platform) fails a package that contains a **stray pak** (any `.fpak` other than
`content/base.fpak`), **editor content** (anything under `editor/` or `.regia/`, `*.editor.ron`, `*.autosave[.ron]`), or the
**agent server**, the editor or the dev asset watcher in the binary (desktop: byte markers in the staged binary; an
explicit `agent` / `dev-assets` / `editor` feature on `regia` in the game's `Cargo.toml`). The binary-marker and
`Cargo.toml` feature findings fail a named build profile and only warn on the plain `shipping` profile (ADR-604: games on
`regia`'s default features shipped that way before profiles existed). `allow_agent = true` on a named
profile permits the agent server for playtest builds. A game that links `regia` with its default features links the agent
server, so shipping games use `default-features = false` (see ADR for build profiles).

Every package also ships the cooked shader-permutation list as `content/permutations.json` beside the paks (ADR-2026,
`docs/formats/permutations.md`), and a profile over its permutation budget fails a shipping package.

### Boot movies
An optional `[boot]` table lists startup movies (spec 50 § 2.7, ADR-2410); absent means none, and the keys are additive
(no manifest version bump). `regia_framework::boot_movies::BootMoviesPlugin` reads it:
```toml
[boot]
movies = ["movies/studio.video.ron", "movies/intro.webm"]  # project paths, played in order before the first level
skippable_after = 1.0                                      # seconds of each movie before skip_video works
```

### Shipping-binary checks
Shipping builds pass `--remap-path-prefix` for `$HOME`, the cargo home, the workspace and the target
dir (as one `--config target.<triple>.rustflags=[…]`, which cargo joins with the config files' and
cargo-xwin's / cargo-zigbuild's flags) and bake `REGIA_GIT_HASH` / `REGIA_BUILD_VERSION` /
`REGIA_BUILD_PROFILE` (crash reports). After staging, `regia package` fails the package when the
binary contains the build machine's workspace, home or cargo-home path, or the dev asset watcher
(`dev-assets`; `regia_render::shader_reload::engine_shader_dir` is `None` without that feature, so no
source path is embedded). Linux additionally checks the glibc floor (the newest `GLIBC_x.y` in the
binary's version-needed table, read from the ELF bytes since ADR-8835, must be ≤ the floor) and reports `NEEDED` libraries the Steam runtime lacks (ADR-148).
Windows additionally reads the staged exe's PE header (`Subsystem` of the optional header): a console
program (`IMAGE_SUBSYSTEM_WINDOWS_CUI`) warns on the plain `shipping` profile and fails a named build
profile, because Windows opens a console window beside the game (ADR-2306). The templates' launchers,
the quick app and the net client carry
`#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]`; the dedicated server
stays a console program.

### Linux
`dist/linux/<Name>/{<name> (binary), content/*.fpak, lib/ (none expected), <name>.desktop, icon.png}` →
tar.gz. Shipping packages are linked against **glibc 2.31**, the baseline of Steam Linux Runtime 3.0
("sniper", Debian 11, which Steam and the Steam Deck use for native Linux games): `regia package`
builds with `cargo zigbuild --target <arch>-unknown-linux-gnu.2.31` (zig supplies the old glibc's
symbol stubs; `--glibc VERSION|host` overrides, dev packages default to `host`) and links an
`$ORIGIN` rpath so `libsteam_api.so` and any bundled library load from the binary's directory. X11,
Wayland and the Vulkan loader are `dlopen`ed; ALSA, udev and D-Bus are linked and provided by the
runtime. The package is verified by running it inside the runtime (`SteamLinuxRuntime_sniper/run --
./<name>`), see the packaging guide. This replaces building inside the sniper SDK container (ADR-148):
same baseline, no container engine needed on the build machine.

**Build containers** (`--container steam-sniper|debian12`, C-26, ADR-8835, amends ADR-148): where C or
C++ dependencies must also come from the old distribution, the binary builds inside a container instead
of with cargo-zigbuild. `steam-sniper` is the Steam Linux Runtime 3.0 SDK (glibc 2.31, `CC=clang`,
`CXX=clang++`), `debian12` Debian 12 (glibc 2.36). The base image is pinned by digest; the first run
builds `localhost/regia-build-<profile>:<rustc>-<digest12>` from it with the host's Rust version
(`--container-image` runs a prepared image, pinned by digest or a local version tag, never `latest`).
podman, else docker (`REGIA_CONTAINER_TOOL`), is found at run time (`container_tool_missing`
otherwise). The workspace and `--container-mount` directories are mounted read-only at their host
paths (`cargo build --locked`); the target directory and cargo home are
`<target>/container/<profile>/{target,cargo-home}`. The glibc floor is the profile's; `--glibc` must
match it and `--steam` needs `steam-sniper` (`container_glibc_conflict`). Error codes are listed on the
packaging guide page.

**AppImage** (`--format appimage` or `formats = ["tar", "appimage"]`): the staged package becomes an AppDir
(`AppRun` 0755, `<slug>.desktop` with `Type`, `Name`, `Exec=<bin>`, `Icon=<slug>`, `Categories=Game;`,
`<slug>.png`, `.DirIcon`, `usr/bin/<bin>` with `content/`, `assets/` and `build_info.json` beside it) and
`appimagetool` writes `<slug>-<arch>.AppImage`. The tool comes from `REGIA_APPIMAGETOOL` or `PATH`; it is
never downloaded, and a missing tool fails before the build with `appimagetool_missing`. An icon is required
(`appimage_icon_missing`).

### Windows
`dist/windows/<Name>/{<Name>.exe, content/*.fpak}` → zip. Exe icon + version info via `winresource`
(ADR-2645): every game executable crate of a template (`launcher/build.rs`, the quick crate's `build.rs`, the net game's
`client/build.rs` and `server/build.rs`, ADR-5617) has the same build script. It does nothing unless `CARGO_CFG_TARGET_OS` is `windows`. Then it embeds
a multi-size ICO (16/32/48/256, PNG-compressed entries), written from `[platforms.windows] icon` (else
`assets/branding/icon.png`, `assets/icon.png`, else a generated placeholder), and a VERSIONINFO: ProductName and
FileDescription from `[project] name`, CompanyName from `organization`, and FileVersion, ProductVersion and the fixed file
version from `[project] version`. With no resource compiler (`rc.exe`; `llvm-rc` on a cross build) it prints a cargo
warning and the build continues. `winresource`, `png` and `toml` are build-dependencies of the generated crates only.
`regia upgrade-project --write` adds the script to a game executable crate that has no build script (an edited or custom
one is never replaced). After staging, `regia package` reads the exe's resource directory (`ship_check::check_windows_resources`):
it needs `RT_GROUP_ICON` with its `RT_ICON` images and an `RT_VERSION` whose FileVersion matches `[project] version`. It
warns on dev and plain shipping packages and fails a named build profile. `#![windows_subsystem = "windows"]` in release builds
(the templates' `cfg_attr`, checked by `ship_check`; see Shipping-binary checks, ADR-2306).
Built on Windows (MSVC) in CI; cross-compiling from Linux is optional (cargo-xwin), not required.
Proton: the Windows build picks a Vulkan adapter first (`gpu_select::enumerate`; DX12 only when no
Vulkan adapter exists), and under Proton Wine's `winevulkan` forwards Vulkan to the host driver, so
DXVK (D3D9–11) is never involved and VKD3D-Proton (D3D12) only in that no-Vulkan fallback. The native
Linux depot remains the supported Linux/Deck path; Proton is the players' override.

### macOS (deferred — ADR-009; not built in M16, design kept for the port)
`.app` bundle: `Contents/MacOS/<name>`, `Contents/Resources/{content/*.fpak, AppIcon.icns}`,
`Contents/Info.plist` (bundle id, version, `NSHighResolutionCapable`, `LSMinimumSystemVersion` 12.0,
`NSSupportsAutomaticGraphicsSwitching`). Universal binary (`lipo` arm64 + x86_64) by default. Optional
`codesign --options runtime` + notarisation (`xcrun notarytool`) when identity configured. `.icns`
generated from PNG in Rust (write the ICNS container directly; it's a simple format). Built on macOS.

### Android
1. `cargo ndk -t arm64-v8a [-t x86_64] --platform <min_sdk> -o build/android/app/src/main/jniLibs build --profile shipping -p android`.
2. Generate/refresh Gradle project `build/android/` from the engine's template (kept in
   `crates/regia_cli/templates/android/`): `settings.gradle.kts`, `app/build.gradle.kts` (AGP version
   pinned, `androidx.games:games-activity` dependency matching the `android-activity` crate's
   GameActivity version, `noCompress += "fpak"`), `AndroidManifest.xml` (activity
   `com.google.androidgamesdk.GameActivity` subclass `RegiaActivity` — a ~10-line Java/Kotlin file,
   the only non-Rust code), icons (mipmap densities generated from PNG by the CLI), orientation,
   permissions, `android:hasCode` as needed, `appCategory="game"`, `isGame="true"`.
3. Copy paks and `permutations.json` into `app/src/main/assets/content/`.
4. `./gradlew assembleRelease` (APK) or `bundleRelease` (AAB), signing from env-provided keystore;
   debug keystore for dev builds.
5. `regia install-android`: `adb install -r`, `adb shell am start`, stream `adb logcat` filtered by pid.

Pinned in the template (ADR-013; `asset-delivery` 2.2.2 joins them for projects with store asset packs, ADR-1876): AGP 8.13.2, Gradle 8.14.3, `games-activity` 4.4.0, `appcompat` 1.7.0, Kotlin
BOM 1.8.22. No Gradle wrapper jar is committed; the CLI finds Gradle as `REGIA_GRADLE` → `gradle` on PATH →
a cached distribution (≥ 8.13). M1 builds debug APKs only (`assembleDebug`, AGP debug keystore); every
manifest gets the engine permissions (`VIBRATE`) plus the project's.

`regia doctor --platform android` verifies: rustup + targets (`aarch64-linux-android`,
`x86_64-linux-android`), `cargo-ndk`, `ANDROID_HOME` with platform + build-tools, NDK r27+ (`ANDROID_NDK_HOME`),
JDK 17, `adb` reachable.

### Engine test bundles (`cargo xtask dist`, ADR-868)
Repository tooling, not a game package: `cargo xtask dist --target windows|deck` bundles engine binaries
(default `vfx_lab`) for a Windows PC or a Steam Deck without a Rust toolchain there.
- Layout: `target/dist/<target>/regia-<target>-<version>/{<bins>, licenses/{LICENSE, THIRD_PARTY_NOTICES.md},
  content/pipelines/*.fpc (optional), README.txt, dist-manifest.json}`, plus a stored zip (Windows) or ustar tar
  (Deck) of that folder, and `symbols/<crate>.pdb` beside it.
- Build: Windows uses cargo-xwin off Windows and cargo on Windows. The Deck uses
  `cargo zigbuild --target x86_64-unknown-linux-gnu.2.31` and runs on Linux only.
- `dist-manifest.json` (schema 1) lists every file with its size, BLAKE3 hash and mode, the binary checks and the
  run-time folders (user, logs, crash dumps, cache, pipeline cache, shipped pipeline caches).
- Binary checks: a Windows exe's PE imports must be Windows DLLs, the Visual C++ runtime (reported), `vulkan-1.dll`
  or bundled DLLs. A Deck binary's glibc must be at most 2.31; libraries outside the Steam runtime are reported.
- `--dry-run` plans without writing, and `--verify` checks a folder or archive against its manifest.

The MSVC PDB carries the crate name: cargo replaces hyphens with underscores, so `my-game.exe` has `my_game.pdb`.
Both `regia package` and `dist` look for that name first.

## 5. Crash reporting (Tier 1 minimum)
- Panic hook writes `logs://crash-<timestamp>.txt` (panic message, backtrace, build_info, OS info, last
  200 log lines). On next launch, the game can query `regia_platform::crash::pending()` to show or
  send the pending reports. Sending is **opt-in crash and match-log upload (off by default)**: the
  `regia_crash` reporter (`regia::crash`, umbrella `crash` feature) POSTs pending crash reports and
  a game-provided match log to a configured endpoint only when the game sets an endpoint and gives
  consent. Production endpoints are `https://`, enabled by `regia_crash`'s `tls` feature (ureq +
  rustls); without it only `http://` works and `https://` is refused.
- Desktop native crash minidumps: Tier 2.
- Symbolication: Linux `symbols/<name>.debug`, Windows `symbols/<name>.pdb`, Android
  `symbols/<abi>/lib<name>.so` + `symbols.json` (§ 4 Common steps); see the packaging guide.

## 6. Acceptance (M16)
- From a clean clone, CI produces: Linux tar.gz, Windows zip (built and smoke-tested in the owner's
  Windows VM, `06` § 8), Android APK + AAB
  for `samples/sim_bridge` (repeated for `samples/third_person` in M20), each launching and running a
  60-second automated smoke test (scripted
  input playback) with zero errors in the log. Android smoke test on emulator (x86_64) in CI plus a
  manual run on the physical reference device recorded in progress.md.
- Shipping build sizes recorded; Android APK for the sample < 150 MB.

### Configurable package hygiene (ADR-8062, C-09)

Every profile validates the optional `[package_hygiene]` table in `Regia.toml` through a
bounded, redacting reader before general manifest decoding. Shipping builds enforce it on
staged desktop/server packages and Android assets, native
libraries and AAB asset packs before archiving. The table configures literal strings,
Rust regex patterns, word/substring/regex deny terms, reviewed rule/path exceptions and
resource limits. Secret signatures are enabled by default. Dev builds retain developer
strings. `regia package --platform P --audit-dir DIR --json` audits an extracted directory
without building or writing, for any profile. `agent.package {audit_dir, confirm: true}`
is the same operation with a project-relative path and the existing job/permission rules.
The [agent guide](../../book/agents/package-hygiene.md) specifies every field, default,
matching rule, finding and error. Configurable rules may use explicit ids. An allowance
stores the BLAKE3 digest of the rule it was reviewed against, so reordering rules does not
move the waiver and a changed body becomes finding `allowance_obsolete`. Legacy positional
lists still load; `Policy::migrated` freezes their current index into those ids and does not
retarget a digest onto a different row. `hygiene.policy_digest` is the effective policy
digest and contains no rule text. `Policy::effective` is the same model for a later editor
policy job. The public Rust surface lives in `regia_cli::package_hygiene`. This is project
configuration, not a frozen content format under specs/12 § 4a, so it does not take the v0.6
format bump. Defaults are unchanged: empty project rules, secret signatures on, email
scanning off. No frozen runtime or cooked-content schema changes.

### Audit memory budget (ADR-8244)

The same table also sets `max_memory_bytes`, `scan_window_bytes`, `max_matcher_bytes` and
`max_work`. Literals and bounded patterns stream through a window whose overlap is one byte
shorter than the widest streamed pattern, so a match that crosses a window edge keeps the
offset of a contiguous scan. Unbounded patterns, including the default secret signatures, run
on the whole input when it fits in `max_memory_bytes` and otherwise fail closed with
`incomplete_reason` `unbounded_regex`; they are not split. Matcher cost is reserved before
each rule is compiled and a rule past `max_matcher_bytes` is not compiled. A mapped pak is
not charged. An owned pak buffer and a decoded entry are not retained together past the
memory budget. A declared entry larger than that budget fails before decompression. A file
whose size or modification time changes during the audit fails closed. Reports carry
`complete`, `incomplete_reason`, `limits`, `peak_memory_bytes` and `work`. A finished scan
with findings is a policy failure (`complete` true). `complete` false means the audit did
not check every selected byte. Project Settings reads and writes the six ceilings without
rewriting the rest of `[package_hygiene]`. No frozen format changes.

### Packaged startup configuration (v0.5.1, ADR-7893)

`build_info.json` carries the selected platform's optional `render_profile` and the project's
optional `[boot]` object. Desktop reads it beside the executable, independently of the current
working directory; Android reads it from the APK assets root. The exact render preset is applied
before Startup: player settings > package > saved or first-launch Auto > runtime default.
Explicit Auto-detect replaces it for this launch. Package defaults are never persisted as player
settings. Explicit BootMovies/plugin config wins over a loose manifest, then packaged boot data;
an empty loose list disables the packaged list. Invalid metadata warns without blocking startup.
The existing profile/quality/boot agent status methods report the configuration origin. This is
an additive build-report change (ADR-1594), not a frozen asset format change.

### Engine licence notice and credit (ADR-10225)

Every package (Linux, Windows, Android, dedicated server) carries, beside `THIRD_PARTY_NOTICES.md`:
`REGIA-ENGINE-NOTICE.txt` (the MPL-2.0 notice, the engine version and the engine source location, MPL § 3.2:
`[package] engine_source_url`, else the engine's setting, the public repository
`https://github.com/Regia-Engine/Regia-Engine` (ADR-10254); a `shipping` package warns while a project still sets the
old placeholder `<release host>`, the "not set" value), `REGIA-ENGINE-LICENSE.txt` (the MPL text) and
`CREDITS.txt` (the project's own `CREDITS.txt`, then "Made with Regia Engine" unless `--no-engine-credit` or
`[package] engine_credit = false`). The credit is a default, not a licence condition; the notice and the licence
text are always written. The engine's `LICENSE` is not copied as such; a game workspace's own `LICENSE.md` is.

### Stage preparation (ADR-8427, C-10)

Shipping preparation, before the content audit, rewrites absolute build paths in text
assets (`.json`, `.ron`, `.gltf`, `.txt`), drops internal notes under `content` unless
`[package] legal_notices` keeps them, rejects files outside `[package] stage_allow` when
that key is set, and refuses a shipped binary that carries `regia-dev-feature`,
`regia-cheat-hook`, or accepts `--regia-dev-probe` / `--regia-cheat-probe`. `[package_hygiene]
path_scrub` is `rewrite` (default), `reject` or `off`. Binary files are not rewritten.
`--audit-dir` reports the same rule IDs and does not write, delete or execute. Dev
packages skip preparation and the content audit. Rule IDs: `build_path`, `dev_flag`,
`stage_allow`, `internal_notes`. Completed rewrites and drops are report `actions`.

### Executable host-string gate (ADR-10354)

Every staged executable (detected by magic: ELF, PE, Mach-O, WebAssembly) is scanned for
private build-machine strings: the build farm's host names, the user and directory roots of
this network (`/home/`, `/media/`, `/srv/`, `/models/`), LAN addresses (`192.168.`) and
tailnet names. A hit is rule `host_string` (source `content`, the binary's byte offset;
never the matched text or a filename that could itself carry a host). The source of every
hit is removed instead where possible: engine data files name no private host, the
workspace's `.cargo/config.toml` remaps build-machine directory roots out of `file!()` and
panic locations, and `env!("CARGO_MANIFEST_DIR")` stays out of runtime code (rustc does not
remap `env!` values). Reviewed false positives are exact `[[allow]]` entries (staged path,
exact printable run, reason) in a TOML file named by the optional `[package]
host_strings_allow` key of `Regia.toml` (project-relative; no default exceptions). The
standalone check over any built executable is
`regia_cli::package_hygiene::check_executable_strings`, run by `cargo xtask host-strings`
(default: the release `hello_window` sample, built on demand; CI runs it).

### Package parity (ADR-8428, C-12)

`regia package parity` diffs a dev access trace and a package access trace
along one scripted path (`--script`, or two already written traces). Exit 1
means a dev open is missing from the package. Never-touched package files and
cooked equivalents are reported; cooked equivalents count as present. Drift
is reported and fails only with `--strict-drift`. `--classify` reports how a
traced process touched files in an extracted tree. The agent method is
`package.parity`. No frozen format changes.

### Named-profile dependency integrity (ADR-8360, CT-24)

A profile's `exclude` still stops traversal, but excluding a required cooked dependency now
fails shipping with `content_dependency_missing` and its root-to-edge path. The existing
`optional_references` registry tag can identify a known target as deliberately optional.
Both registry edges and cooked-header edges are checked; missing cooked files and unknown IDs
are distinct diagnostics. A corrupt registry is `content_registry_invalid`, even with cook
validation `Off` or `Warn`. This adds no profile keys or frozen-format fields.
### Editor policy workflow (ADR-8399)

Project Settings and `editor.package_hygiene_set` share one commit. The patch may replace
`rules`, replace `allow` (JSON key `allowances`), set the positive integer ceilings
including `max_entries` and `max_findings`, and set `scan_names`, `scan_secrets` and
`scan_emails`. Those flags are the target policy. There is no new TOML key: `Policy`
denies unknown fields. Validation runs before the write. An invalid or duplicate rule id,
an allowance without a reason and a relative scope, or a ceiling that puts the scan window
above the memory budget leaves the file bytes unchanged. A successful save preserves keys
the patch does not model and records undo. `save: false` previews and does not record undo.
The editor fills each allowance `definition` with the ADR-8247 digest and does not link
`regia_cli` or scan file bytes itself.

`editor.package_hygiene_audit` starts `regia package --audit-dir` on a project-relative
directory through the existing job service (`hygiene_audit:<n>`). It requires `confirm: true`
and the build permission, does not build a package, and does not publish a staging folder.
Cancel drops the generation so a late process cannot publish a clean result. Status copies
at most 128 scrubbed findings into Problems (opaque `location_id`, rule id, source, offset).
`receipt.status` is `absent` unless the CLI JSON already carries a receipt object under
`receipt`, `hygiene.receipt`, `report.receipt` or `report.hygiene.receipt`. A staging result
is not a final-artifact receipt. The first Done status of a generation emits one
`hygiene_review` host event for `play.wait_until` and `play.expect`. No frozen format
changes, and the defaults are unchanged.

### Final-artifact audit and receipts (ADR-8348, CT-16)

After the archive (or dir format) is produced, a shipping package audits the generated
artifact itself: ZIP/APK/AAB, tar, tar.gz and nested `.fpak` containers are read in place up
to `max_container_depth` (default 3), with the same entry, byte, memory and work budgets,
and nothing is extracted. Duplicate and traversal names, corrupt or ambiguous records and
over-limit content fail closed; encrypted entries and unsupported containers are listed as
uninspected and refused under `strict_containers = true`. A pass writes
`<artifact>.hygiene-receipt.json` beside the artifact (schema `regia.package_hygiene.receipt`
version 1), bound to the artifact's BLAKE3, the policy digest, the container settings and the
tool version; the artifact itself is never modified. `regia package --json` reports it as
`final_audit`. `regia package --audit-artifact FILE [--verify-receipt RECEIPT]` and
`agent.package {audit_artifact}` audit or verify without building or writing. The receipt is a
tool output, not a frozen content format.
