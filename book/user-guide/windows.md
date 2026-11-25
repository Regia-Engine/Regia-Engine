# Windows

<!-- index: Windows -->
<!-- index: DX12 -->
<!-- index: Direct3D 12 -->
<!-- index: Cursor grab -->

The v0.2.17 Windows pass exercised Vulkan and DX12 on the owner's a test machine, including
native crash capture, deliberate device-loss exit and clean quits; the N150 laptop also had
a native pass. See the v0217 notes, `docs/releases/v0.1.28-windows-pass-v0217.md`.
That evidence belongs to its recorded source revision. **The v0.2.20 candidate still needs
its own native Windows acceptance**; its handoff, `docs/evidence/windows/v02020-acceptance-b.md`,
records the frozen source and exact commands. Windows is verified on the owner's machines
only (ADR-009); there are no hosted Windows runners.

## What a Windows package is

`regia package --platform windows` writes `dist/windows/<slug>-windows.zip`. On Linux the same command cross-compiles with cargo-xwin when [regia doctor](doctor.md) `--platform windows` is clean, and it does not run the exe on the Linux host. A dedicated server is `regia package --server --platform windows`. The procedure is [Package a game](../how-to/platforms/package-a-game.md). [Linux](linux.md) is the other desktop shipping target.

Vulkan is the default adapter. DX12 is in the same adapter list after the Vulkan entries. `REGIA_GPU_INDEX` or `REGIA_GPU_NAME` selects a DX12 adapter when the machine has one. A pass on a machine with no DX12 hardware adapter still records that run as `unreachable` (`BLOCKED`). The sections below are the acceptance history and the checks that do not need the Windows machine.

`CursorGrab::Locked` is requested as Confined on Windows, because that is the grab winit applies there. Confined stays Confined. Spec 11: only the primary window becomes an operating-system window (ADR-2252).

## What is checked without a Windows machine

- **Compile and lint.** The `windows-cross` CI job runs `cargo xwin clippy --workspace --all-targets
  --target x86_64-pc-windows-msvc -- -D warnings` on Linux (ADR-295). It is allowed to fail until it
  has been seen green once.
- **Shaders for DX12.** A Linux test writes every engine shader and permutation as HLSL Shader Model 6.0
  with naga and fails on any naga error; another keeps the three loops ADR-201 rewrote (16 and 12
  shadow taps, 16 parallax steps) free of `break` and `continue` so FXC can unroll them. A second
  check (ADR-367) covers every module of every engine crate, terrain, VFX, UI, 2D and the path tracer
  included: HLSL for Shader Model 5.1 (FXC) and 6.0 (DXC), the Direct3D 12 compute limits (32 KiB of
  workgroup memory, 1024 threads per group), and no `textureSample`, `textureSampleCompare` or
  `dpdx`/`fwidth` inside a loop of a fragment shader (FXC has to unroll such a loop). `cargo xtask
  check-shaders` runs it too. None of this proves FXC or DXC accepts the text.
- **The pass itself.** Its summary schema and verdict logic are unit tested against fixture JSON.

## Running native acceptance

On the Windows machine, with Rust (the `x86_64-pc-windows-msvc` host toolchain), the Visual Studio
Build Tools (C++ workload) and a GPU driver with Vulkan installed:

```
cargo xtask windows-pass
```

Allow time for a cold native build: compilation can take hours on a small Windows machine.
Use low build concurrency and distinguish completed builds from executed tests. In order it:

1. checks the toolchain and lists every GPU adapter with its backend and driver;
2. runs the Windows test subset: platform, native crash capture (the bundle, and a real fault in a
   child process), the deliberate exit, paths and Saved Games, adapter selection. One log per step in
   `tests/`. Each step is compiled first (`--no-run`, up to 3 hours) and then run (up to 30 minutes),
   so a slow cold build shows as `build_timeout` or `build_fail` rather than as a hung test (ADR-4050);
   `summary.json` records `build_secs` and `secs` per step;
3. builds `vfx_lab` in release and runs it per backend: a 3 s cold and a 3 s warm start on a fresh
   pipeline cache (the pipeline-compile gap, measured and not fixed), then a 60 s measured run
   (`--run-secs`) whose engine `summary.json` is read back. Vulkan goes through the default pick, so
   it tests ADR-201 itself. DX12 runs only when a variable can reach it; see below;
4. starts and quits the windowed sample five times (`--quits`) and checks that each exit code is 0.

Leave the machine alone while it runs and keep the screen awake: a run with a hidden window is
reported as invalid. The results land in `docs/evidence/windows/<UTC date>/`; `summary.json` is the
one file that matters. Send back that whole folder.

Options: `--out <dir>`, `--run-secs <n>`, `--quits <n>`, `--skip-tests`, `--skip-runs`.

### Reading a result (no Windows needed)

```
cargo xtask windows-pass --read docs/evidence/windows/<date>/summary.json
```

prints `PASS`, `BLOCKED` or `FAIL` with the reasons, and exits 0, 2 or 1. `BLOCKED` means nothing
failed but something could not run.

### Known limits of the pass

- **DX12 needs a DX12 hardware adapter.** The adapter list holds every adapter, Vulkan first and
  then DX12 (ADR-326), so the pass forces DX12 with `REGIA_GPU_INDEX` of a DX12 adapter
  (ADR-201). On a machine with no DX12 hardware adapter the pass records the DX12 run as
  `unreachable` (verdict `BLOCKED`, ADR-296).
- **A step that skipped is not a pass.** A test step whose program prints `<program>: skipped ...` (the native
  crash driver is Linux only; `device_lost_exit` without a GPU) is recorded as `skipped` with its line, and the
  verdict is `BLOCKED` (ADR-5806).
- **Cold means a cold engine cache.** The operating system's own shader cache is not cleared.
- Frame times and compile times are reported, not judged against budgets.

## Paths

`user://` is `%APPDATA%\RegiaEngine\<exe>` and `cache://` is `%LOCALAPPDATA%\RegiaEngine\<exe>`.
`regia_platform::paths::saved_games_dir()` returns the `Saved Games` known folder for games that
keep saves there. The persistent pipeline cache is `cache://pipelines` (`%LOCALAPPDATA%`, never
roaming); it is Vulkan-only in wgpu 30, so on DX12 only the GPU driver's own shader cache helps a
second start. Feedback bundles, crash bundles and native-crash minidumps go to
`%LOCALAPPDATA%\RegiaEngine\<exe>\feedback`, not the roaming `%APPDATA%` (ADR-525). Panic text reports
stay in `logs://`. The agent method `platform.paths` prints all of these on the running machine.

## Swapchain, HDR and the DX12 workflow

The swapchain plan (ADR-525, `render.swapchain`, see [Swapchain](../agents/swapchain.md)) has three
rules for Windows:

- An HDR request on a display that is not in HDR mode (Windows HDR off) is configured as SDR,
  because DX12 lists HDR color spaces either way and Windows would tone-map the frame back down.
  The display is read again every 120 frames while HDR is requested, so switching Windows HDR on
  or off while the game runs reconfigures the swapchain.
- VRR on DX12 presents with tearing allowed (`Immediate`), the present that variable refresh needs on
  DXGI; without tearing support it uses `Mailbox`.
- The display's peak and minimum nits from DXGI fill the display calibration's defaults.

`.github/workflows/windows-dx12.yml` is defined for the owner's self-hosted runners and has not run:
the `policy` job (no GPU, `WINDOWS_RUNNER=true`) runs these rules' tests and compiles the FXC and
DXC paths; the `gpu` job (`WINDOWS_GPU_RUNNER=true`, a runner labeled `ferrite-gpu`) runs
`cargo xtask windows-pass` and keeps the evidence folder, crash bundles and minidumps as an artifact.

## Native crash capture

Native crash capture is opt-in on Windows: set `REGIA_NATIVE_CRASH=1` or
`FeedbackConfig::native_crashes_windows`. See the [Feedback](feedback.md) chapter. The pass turns it
on for its runs.

## File names

VFS paths (`user://`, `assets://`, ...) refuse names Windows cannot store, on every platform, so
content made on Linux works on Windows: the device names `CON`, `PRN`, `AUX`, `NUL`, `COM1`-`COM9`
and `LPT1`-`LPT9` (any case, with or without an extension), names ending in `.` or a space, and the
characters `< > " | ? *`. `regia_platform::open_path` shows a directory or file in the OS file
manager (`explorer` on Windows).

See also [macOS](macos.md), [Android](android.md) and `docs/06-testing-and-ci.md` § 8 for the
verification matrix.

## DX12 readiness (verified on Linux)

Everything in this section runs in the Linux test suite (ADR-326, ADR-327). Keep these compile and policy
results separate from a native Windows pass, and record the source revision for both.

- **Every shader translates to HLSL for both DX12 compilers.** The HLSL test writes every engine shader and permutation at
  Shader Model 6.0 (DXC: the `static-dxc` feature or a `dxcompiler.dll`) and at Shader Model 5.1 (FXC: what wgpu uses when no
  DXC is available, which is the default build on most machines). The 5.1 text is scanned for Shader Model 6 constructs that
  naga writes but FXC rejects (wave intrinsics, 16- and 64-bit integer types). The v0.1.27 gate runs of the Shader Model 6.0
  test found no failing shader, so no shader was changed. There is no allow-list.
- **DX12 can be forced.** The adapter list now holds the DX12 adapters after the Vulkan ones (the Vulkan numbering is
  unchanged), so `REGIA_GPU_INDEX=<n>` or `REGIA_GPU_NAME=<substring>` of a DX12 entry selects DX12 even when Vulkan works.
  The "DX12 cannot be forced" limit above no longer applies, and `cargo xtask windows-pass` runs its DX12 section instead of
  recording it as `unreachable`. The default pick is unchanged: Vulkan first, DX12 only without a Vulkan hardware adapter.
- **Resource Binding Tier from limits.** Each DX12 adapter in the list shows its D3D12 Resource Binding Tier, read from the
  per-stage limits wgpu derives from Microsoft's binding-tier table (Tier 3 is what bindless resource arrays need). wgpu does
  not expose Tier 1 adapters; a `Tier 1` reading means the limits were clamped below Tier 2.
- **Backend decisions in one place.** Which backends open per OS, the adapter order, the Vulkan-only pipeline cache, the
  DX12-only binding tier and the FXC rules are pure functions in `regia_render::backend_policy`, unit tested for Vulkan,
  DX12, Metal and GL.
- **Pipeline count per profile is pinned.** On DX12 the engine has no pipeline cache of its own (wgpu's is Vulkan only), so a
  cold start compiles every pipeline the profile warms. A test pins that count per shipped profile, so a jump in the number of
  shader variants fails CI.

Not proven here: that FXC or DXC accepts the HLSL, whether FXC unrolls the shadow and parallax loops once wgpu's loop bounding
wraps them, the binding tier a real DX12 adapter or WARP reports, and DX12 frame and compile times. The owner's
`windows-pass` run covers those.
