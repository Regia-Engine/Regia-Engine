# Android

Regia Engine runs on Android phones (aarch64, API 29+, Vulkan). This page is the state of that port as
of batch v020 (v0.1.21) and the steps to re-run the real-device pass on your own phone. Numbers are from the
Pixel 6 (GrapheneOS, Mali-G78) on 2026-10-08 unless a row says otherwise.

## Build, install, attach

```sh
regia install-android --project <game>     # build the APK, install, launch, forward the agent port
```

`install-android` needs the Android SDK and NDK r27+, `cargo-ndk` and rustup-managed Android
targets (`regia doctor` checks them). It grants `INTERNET` to debug builds (GrapheneOS leaves it
off for adb installs), so `regia agent call ...` works over `adb forward` like on desktop.

Options: `--device <serial>` picks the phone when several are attached; `--abi <abi>` (repeatable)
builds only those ABIs (default: the device's primary ABI when `[platforms.android] abis` lists it);
`--no-build` installs the APK from the last `regia package`; `--no-logcat` returns after the launch
instead of streaming the app's logcat; `--attach` only forwards the agent port of the app that is
already running and writes `.regia/agent/endpoint.json` (no build, install or launch).

## What the engine does on Android

| Android event | Engine behavior |
|---|---|
| Window created / destroyed (`onStart`/`onStop` side) | `Resumed` / `Suspended`. Surfaces drop on suspend and are created again on resume; the `Device` and every GPU resource survive. The audio output stream is freed and rebuilt. |
| `onPause` (batch v020) | `ActivityState.paused`. The audio output gate pauses at once, instead of waiting for the window to be torn down. |
| `onDestroy` (batch v020) | `ActivityState.destroyed` writes `AppExit`, so the same clean exit as a desktop quit runs (ADR-188): final `summary.json`, pipeline cache, ordered GPU shutdown. winit does not forward `onDestroy`, so the generated `RegiaActivity` reports it. |
| Rotation / resize | The swapchain is reconfigured on the next frame; the safe area is re-read. |
| Memory pressure | `LowMemory` message (any state; it never changes `AppLifecycle.state`). The renderer frees idle GPU memory at the next frame. |

Files go to app storage: `user://` is the app's internal files directory and `logs://` is
`user://logs`, so logs, `summary.json`, partial summaries and crash bundles are there. Pull them with
`adb exec-out run-as <package> tar c -C files logs | tar x` (debug builds).

**Defaults.** Mobile High is the platform default preset. Textures are cooked to ASTC for Android
(ADR-144); BC textures are decoded on the CPU only for content that was not cooked for Android.
Several Mali driver bugs are worked around in the shaders (no texture arguments to functions,
no dynamic indexing of vectors; both have tests that fail on a regression).

## TalkBack

With the `accesskit` feature the same screen-reader bridge as on desktop attaches to the activity's
surface view and talks to TalkBack (ADR-3454). Build the game with that feature; it stays off by
default. `REGIA_ACCESSKIT=0` keeps the OS adapter off without a rebuild.

TalkBack connecting is the activation: the next update is the whole tree, and later frames send only
what changed. A double-tap runs through the same action path as a click (`click` on a button). The
bridge does not learn that TalkBack was turned off; it keeps sending until the process ends or an
agent calls `ui.a11y_platform_connect` with `false`. Window focus is not forwarded (TalkBack uses the
view). The Java delegate class is loaded from the crate, so the generated activity does not need a
new class.

TalkBack reads a control's label only, so the Android adapter adds each control's description to its
label: a toggle labelled `Invert Y` with the description `Move, gamepad` is read as "Invert Y, Move,
gamepad" (ADR-4932). Give controls that share a label a description that tells them apart.

**Touch size and scale.** A UI scaled to a 1920x1080 reference is about UI scale 1.0 on a landscape
phone, far below the screen's density: a 44 px control is about 20 dp there. Lay touch screens out with
`ScaleMode::ConstantPhysicalSize` (1 UI px = 1 dp) and let long lists scroll. The First Person
template's controls screen does this on Android (`ControlsLayout::Touch`), with an on-screen
**Controls** button. While that screen is open, Back closes it instead of the game. The `touch` profile
of `ui.a11y_audit` measures targets in dp, so it catches this on the device. Android's font size setting
(`Configuration.fontScale`) multiplies the game's own `text_scale` (`regia_platform::SystemTextScale`, read at
start, on resume and on every configuration change), within the 0.5 to 3.0 text scale range.

`ui.a11y_platform_tree` and `ui.a11y_platform_connect` include `android`: `os`, `registered`,
`attached`, `status` (`pending`, `attached`, `disabled`, or `not_android` on a desktop build) and
`detail`. A JNI failure is logged and the bridge is disabled (`status` `disabled`, `adapter_error`
set); it does not panic. A shipping build still aborts if AccessKit unwraps internally after that
check.

## Frame pacing

On a phone each frame starts on a display vsync and stays on screen for the same number of vsyncs, so
frames arrive evenly and do not pile up in the swapchain (ADR-1942). The swap interval is chosen for
you: a 60 Hz panel with 20 ms frames runs at 30 fps, a 120 Hz panel with the same frames at 40. A rare
spike leaves it alone. `FramePacing::target_fps` still caps the rate, as a whole number of vsyncs.
Agents read and set all this with `pacing.get` and `pacing.set`
([Frame pacing](../agents/frame-pacing.md)); `pacing.trace` shows the last frames one by one. To return
to the old path, insert `FramePacingSettings { mode: PacingMode::Off, ..Default::default() }`.

## Thermal governor (batch v020)

Phones throttle hard: Mobile High on the Pixel 6 reached the OS's *severe* thermal status after
seven minutes and the frame time rose by a fifth. The engine now reads `PowerManager`'s thermal
status and its 10-second headroom forecast (`ThermalState`, polled every 2 s) and steps the render
scale down before the device gets there:

| Level | Asked for when | Render scale (× the preset's) |
|---|---|---|
| 0 | status none/light and headroom under 0.8 | 1.00 |
| 1 | status moderate, or headroom ≥ 0.8 | 0.85 |
| 2 | status severe, or headroom ≥ 0.92 | 0.70 |
| 3 | status critical or worse, or headroom ≥ 1.0 | 0.55 (never under 0.5) |

It steps down at once and back up one level after 30 s of being cooler. The scale the player or the
preset asked for stays the base: if anything else changes the render scale while the governor is
holding it down, that value becomes the new base. Hosts turn the policy off with
`ThermalGovernor { enabled: false, ..default() }` (a resource, changeable at run time; the field
takes effect on the next frame and restores the base scale). On desktop the platform reports no
thermal state and the governor does nothing. `render.get_profile` shows `thermal` (see the
[agent page](../agents/render-profile.md)). The factors and the 0.8 / 0.92 / 1.0 thresholds are a first
guess; the 20-minute pass below measures them.

## Crash capture

The panic hook writes a bundle to `logs://` on Android as on desktop. A *native* crash (a driver
SIGSEGV) cannot use the desktop crash monitor (ADR-158: Android forbids the ptrace-based dump and
the monitor re-exec), so the crashing process writes a small record itself (ADR-328): the next
launch logs `native crash record from the last run: SIGSEGV ...`, writes a
`logs://crash-<time>-native.txt` report with the stack, and, with `FeedbackPlugin`, a crash bundle
(no minidump). See [Crash reports](../agents/crash-reports.md#android-native-crash-records). The
engine also logs why the earlier run ended, read from Android's `ApplicationExitInfo` (API 30+):
`previous run ended: native-crash (status 11) ...` as a warning. The OS tombstone itself is in
`adb logcat -b crash` (and `adb bugreport`); the pass script below saves the Regia Engine log lines, and
`--crash-test` checks the record on the device.

## Re-running the Pixel 6 pass

Plug the phone in (USB debugging on), keep it unlocked or use `--screen-suspend`, and run from the
repo root:

```sh
cargo xtask android-pass --package <app id> --project <game> \
    [--device <serial>] [--thermal-minutes 20] [--screen-suspend] \
    [--do27-chase <aarch64 do27_chase> --do27-content <content dir> --do27-map bore_twelve]
```

It installs the app, then runs, in order: the lifecycle check (50 suspend/resume cycles and 10
rotations, PSS leak check), the Mobile Low and Mobile High presets (frame and GPU p50/p99 with per-pass
GPU times, memory report), the DO27 map if you pass the binary and content (build the binary with
`cargo ndk -t arm64-v8a build --release -p lookdev --bin do27_chase`), and a 20-minute Mobile High
run sampled every 15 s. Everything is written to `target/android-pass/<timestamp>/`:
`summary.txt` (read this first), `lifecycle.json`, `presets.jsonl`, `thermal.tsv`
(elapsed, frame p50/p99, GPU p50, render scale, thermal status, skin and SoC °C, RSS),
`do27-*.txt` and `logcat-regia.txt`. Send that folder back. In `thermal.tsv`, a falling
`render_scale` column with `thermal_status` staying at 0–2 is the governor working; status 3 for
long stretches means the thresholds need to be lower.

Tuning options: `--thermal-preset` is the render preset of the thermal run (default `mobile_high`;
`mobile_low` on a phone whose GPU times out at Mobile High), `--sample-secs` the seconds between thermal
samples (default 15) and `--window-secs` how long each `play.hitch_report` window lasts, for the presets and
the thermal samples (default 8). `--crash-test` runs only the native crash record check, and
`--crash-delay-secs` (default 8) is how long after startup its worker thread faults.

### Budget verdicts

A run ends with one verdict per budget instead of a number to compare by hand (ADR-1942): the
`budget ...` lines of `summary.txt`, and `budgets.json` and `budgets.tsv` beside it.

| Budget | Limit (reference tier) | Read from |
|---|---|---|
| `frame_p99` | 16.6 ms at Mobile High: the frame's work, per frame the larger of CPU and GPU frame time | `play.hitch_report` `work.p99_ms`, in `presets.jsonl` |
| `presented_p99` | 1.05 × the refresh interval of the panel (unit `x refresh`; the summary line gives ms and Hz) | `play.hitch_report` `p99_ms` and `pacing.get` `refresh_hz`, in `presets.jsonl` |
| `cpu_main_p99` | 8 ms | `presets.jsonl` |
| `longest_frame` | 50 ms | `presets.jsonl` |
| `memory_pss` | 700 MB | `dumpsys meminfo` after the preset's window, in `presets.jsonl` |
| `cold_start` | 5000 ms (median, first engine frame) | `startup.tsv` |
| `thermal_status` | status 2 at worst | `thermal.tsv` |
| `thermal_p50_growth` | 25 % from the first sample to the last | `thermal.tsv` |
| `pacing_late` | 5 % of the frames in the preset's window | `pacing.get`, in `presets.jsonl` |

Each row is `pass`, `fail`, or `not_measured` when a step was skipped or the phone reported nothing.
A frame shown on a 60 Hz panel is on screen for at least 16.67 ms however little it cost, so
`frame_p99` reads what the frame cost and `presented_p99` checks that the frames keep their display
slot (ADR-4947). A run from a build without `work` leaves `frame_p99` not measured.
A phone's GPU memory is system memory, so the GPU allocator's free space counts in PSS: on Android the
engine asks wgpu for small allocator blocks (4-32 MiB, ADR-5423), and `seeing.memory_report`'s
`reserved_bytes` minus `allocated_bytes` is the part of PSS the allocator holds free.
The limits are `budgets/android-reference.ron`; a low-end phone uses `--budgets budgets/android-low.ron`
(Mobile Low at 30 fps). `docs/06-testing-and-ci.md` § 4 is where the numbers come from.

These budgets are not a gate yet: a failed row is reported and the pass keeps its own verdict. With
`--enforce-budgets` a row that fails or was not measured fails the pass.

To judge a finished run again, for instance against a changed budget file:

```sh
cargo xtask android-budgets --run target/android-pass/<timestamp> [--budgets <file>]
```

It rewrites `budgets.json` and `budgets.tsv` in that folder and exits 1 when a row fails.

## Gamepads

Pads connect through `InputManager` (polled every 30 frames). Buttons arrive as key events: the
`BUTTON_*` family maps to the pad buttons; the D-pad arrives as arrow keys. The left
stick works; the right stick, hat axes and analog triggers are not reachable through winit 0.30 and
need the Java-shim observer hook (not built). Device check: a physical pad's D-pad on the Pixel.

## Known limits

- The emulator (gfxstream) is for checking behavior, not speed. The per-frame surface capability
  query that froze it after the first frame is gone (batch v020), so interactive checks work there.
- Mobile High is 60–67 ms on the Pixel 6 at 2400×1080 and is GPU-bound (the opaque forward pass is
  56 ms). The lite forward shader for the Mobile tiers is in this tree (ADR-1874,
  [Mobile forward path](../agents/mobile-forward.md)); that figure was measured before it, and the
  fused post and pooled object uniforms from the same work are not confirmed here; see the lane report.
- macOS and the Steam Deck are separate pages and passes.

## Package size, texture formats and lifecycle checks (v0.1.28)

- **Package size.** `regia package --platform android` prints a size report on stderr after
  the build. stdout still prints only the archive path. `regia apk-size <apk|aab>` prints the
  same report for any built archive: stored and extracted bytes per category (native libraries
  per ABI, paks, other assets, dex, resources, manifest, signature) and the largest entries.
  `--json` gives the machine form, `--top N` sets how many entries to list, and `--budget-mb 150`
  exits 1 when the archive is larger. For the native code inside the archive, `cargo xtask binary-size` splits the `.so` by crate (see [Binary size](binary-size.md)). Two problems are warned about. `pak_compressed` means a
  `.fpak` was stored deflated: the generated Gradle project marks paks `noCompress` so the
  runtime can map them. `unexpected_abi` means the archive has native libraries for an ABI that
  `[platforms.android] abis` does not list.
- **Texture formats.** `regia cook --platform android --stats` ends with the cooked textures
  per format: ASTC by block size, BC, uncompressed. It warns
  `textures_cooked_for_other_platform` when BC textures are in the Android cache: phones decode
  BC to RGBA8 on the CPU. `--stats --json` prints only that report as JSON.
- **Lifecycle without a phone.** `lifecycle.drill { "cycles": 50 }` runs suspend/resume cycles
  on a desktop through the same transitions a home press takes. The surface audit
  (`lifecycle.get`) checks that every surface is dropped on suspend and recreated on resume. On a
  phone, `cargo xtask android-lifecycle` reads the audit's `regia surface audit:` logcat line
  and fails the run on a violation. See [Mobile lifecycle, thermal and touch](../agents/mobile-lifecycle.md).
- **Thermal hooks.** `ThermalLevelChanged` is written on every governor level change. A game reads it to
  shed its own load. `ThermalGovernor::max_fps` adds an optional frame cap per level; it is off by
  default. `thermal.set_override` forces a reading to test both.
- **Touch controls.** `regia_input::TouchControls` maps screen zones (button, floating stick)
  and gestures to actions, without UI widgets.

## ETC2 for GPUs without ASTC (ADR-1379)

The Android cook writes ASTC. For a GPU without ASTC (some older or low-end parts; ETC2 is part of OpenGL ES 3.0
and most Vulkan Android drivers expose it) the asset pipeline can also write ETC2 / EAC:

| Usage | ETC2 / EAC format | Bits a texel |
|---|---|---|
| sRGB colour, UI | ETC2 RGB8 sRGB, or ETC2 RGBA8 sRGB when the base level has alpha | 4 / 8 |
| linear data (ORM) | ETC2 RGB8 or RGBA8 | 4 / 8 |
| one-channel mask | EAC R11 | 4 |
| normal map | EAC RG11 (X, Y; the shader rebuilds Z, as for BC5) | 8 |

The renderer uploads these as they are (`compressed_texture.rs` maps every ETC2 and EAC format). From code:
`regia_asset_pipeline::cook_texture_rgba8_android(width, height, rgba, usage, quality, AndroidTextureFormat::Etc2)`.
A whole project switches with `Regia.toml` (ADR-2063):

```toml
[cook.textures.android]
format = "etc2"              # or "astc" (the default)
astc_profile = "per_texture" # "high" (4x4 everywhere) or "low" (6x6 / 8x8) for an ASTC cook
```

`regia cook --size-report` compares every image as BC, ASTC and ETC2 before you switch, and a device that cannot
sample what was cooked decodes it to RGBA8 on the CPU instead of drawing the slot white; see
[Android texture compression](../agents/texture-compression.md).

**Size report.** Compare the two families on your own textures before choosing:

```console
$ cargo run --release -p regia_asset_pipeline --example etc2_survey -- assets/textures/*.png
$ cargo run --release -p regia_asset_pipeline --example etc2_survey -- --json --low assets/textures/*.png
```

Each row gives the full mip chain's bytes as ASTC (the block the cook picks; `--low` for the larger blocks) and as
ETC2 / EAC, with the level-0 PSNR of both, then the totals with the RGBA8 size. ETC2 RGB8 is half the size of
ASTC 4×4 on opaque colour, at a lower quality on detailed content; with alpha, and for normal maps, both are 8 bits
a texel. `regia cook --stats` reports the ETC2 family of a cooked cache.

## Device pass from a host that does not build (ADR-1749)

The phones may sit on a machine that only runs `adb` (the owner's desktop). Build the APK and the two host
tools on a build machine, copy them over, and run the pass with the prebuilt pieces:

```sh
# build machine (repo root; Android SDK, NDK r27+, JDK 17, cargo-ndk)
cargo build -p regia_cli -p xtask
cargo run -p regia_cli -- package --platform android --abi arm64-v8a --project samples/arena
#   -> samples/arena/dist/android/com.regiaengine.samples.arena-debug.apk, target/debug/{regia,xtask}

# phone host (repo checkout for samples/arena/Regia.toml; adb on PATH; phone on USB, USB debugging on)
adb devices -l                     # pick the USB serial (a phone may also show up over wireless adb)
xtask android-pass --package com.regiaengine.samples.arena --project samples/arena \
    --device <serial> --apk <arena debug apk> --regia <prebuilt regia> \
    --screen-suspend --thermal-minutes 10 --uninstall --out <results dir>
```

On top of the steps above the pass now runs:

| Step | What it checks | Files |
|---|---|---|
| startup (`--start-runs 3`) | three cold starts (`am force-stop`, `am start -W`) and three warm starts (suspend, `am start -W`): `TotalTime` and the engine's first presented frame after the system's `START` line | `startup.tsv` |
| screenshots | `screencap` after each preset and after the thermal run | `screen-*.png` |
| touch | a finger held 6 s low in the middle of the screen (`input touchscreen swipe x y x y 6000`; clear of the system's one-time "Viewing full screen" notice, in case a phone shows it anyway) shows in `input.touch_controls` and is gone after release | `checks.json` |
| trim memory | `am send-trim-memory <package> RUNNING_CRITICAL` reaches the engine as `regia lifecycle: LowMemory` and the app keeps its pid | `checks.json` |
| ASTC | `render.get_capabilities` reports ASTC; `assets.cooked_pack` shows no non-ASTC texture and no CPU decode | `checks.json` |
| surface audit | `lifecycle.get` after the lifecycle cycles: every surface dropped on suspend and recreated on resume | `checks.json` |
| gamepads | the gamepads `dumpsys input` lists (none attached is reported, not failed) | `checks.json` |
| back button | `KEYCODE_BACK` suspends the app (Android 12+ keeps a root activity alive) or ends it cleanly, with no crash, and a relaunch renders; when another window has input focus before the key (a system notice or dialog), the line names it (`focus before the key <window>`) | `summary.txt` |
| crash scan | every line of the whole run (`logcat -b main,system,crash`, streamed from the start) through the lifecycle tool's crash attribution | `logcat-all.txt`, `crash-scan.txt` |

The pass keeps the screen on while the phone is plugged in (`settings put global stay_on_while_plugged_in 7`) and
marks the system's one-time "Viewing full screen" notice as seen (`settings put secure immersive_mode_confirmations
confirmed`; otherwise the notice can take input focus from the app part way through the pass, and the back key goes to
it). It puts both old values back at the end; if it is interrupted, or the phone drops off adb before the end, it prints
the commands that restore them. `--uninstall` removes the app at the end, also after a failed step; the summary's
`uninstall:` line reads `pm list packages` and says `UNKNOWN` when adb no longer reaches the phone. The debug APK shows over the lock screen, so a PIN-locked phone needs
nobody to unlock it. The pass wakes the phone and asks it to drop the keyguard; when `dumpsys window` still reports
`isKeyguardShowing=true` (a PIN or pattern lock) it switches to `--screen-suspend` by itself and says so in
`summary.txt` (HOME on the lock screen stops backgrounding the app part way through the lifecycle cycles).
With the screen key, a phone sometimes wakes itself right after `KEYCODE_SLEEP` and the screen never goes off; when
`Suspended` has not arrived after 4 s and `dumpsys power` reads `mWakefulness=Awake`, the cycle sends the key again (at
most twice) and prints a line saying so. The cycle still fails unless the app reports `Suspended`.

An adb transport reset (a USB blip, or the phone's wireless adb transport flapping) ends the streamed logcat and
closes the forwarded agent socket while the app keeps running. The pass restarts `adb logcat` from the last complete
line's time (`-T`, so `logcat-all.txt` keeps every line; `summary.txt` counts the restarts), and the thermal run
re-attaches the agent once per failed sample (`agent reconnects` in the thermal line); a second failure in a row
ends the run.

**ASTC on the phone.** The arena APK carries no textures, so the ASTC step only proves the device exposes ASTC and
that nothing is decoded on the CPU. To see an ASTC map drawn, build the device compression test for the phone and run
it from `/data/local/tmp` (it skips on an adapter without ASTC):

```sh
# build machine
cargo ndk -t arm64-v8a -P 29 test -p regia_render --test device_compression --no-run
#   -> target/aarch64-linux-android/debug/deps/device_compression-<hash>
# phone host
adb -s <serial> shell mkdir -p /data/local/tmp/regia-test
adb -s <serial> push device_compression-<hash> /data/local/tmp/regia-test/device_compression
adb -s <serial> shell 'cd /data/local/tmp/regia-test && ./device_compression --test-threads 1'
adb -s <serial> shell rm -rf /data/local/tmp/regia-test
```
