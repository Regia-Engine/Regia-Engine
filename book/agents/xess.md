# XeSS Super Resolution setup and proof

XeSS is optional on Windows Vulkan (ADR-4656). The SDK is external and never belongs in a
project repository. Linux and Android keep the SDK-free stub.

1. Set `XESS_SDK` to the absolute XeSS SDK 3.0.2 root and `VULKAN_SDK` to the Vulkan SDK root.
   Build with `regia/xess` (or `regia_render/xess`). Bindgen writes only into Cargo OUT_DIR.
2. Put the SDK's unmodified `libxess.dll` beside the executable, or set `REGIA_XESS_PATH`
   to an absolute directory containing it. Ship the SDK's `LICENSE.txt` and
   `third-party-programs.txt`. `REGIA_XESS=0` disables the device hook.
3. Start the player on Vulkan. `render.get_capabilities` includes `upscalers`, the same
   availability and runtime rows as `render.get_upscalers`. The capability method is named
   `render.get_capabilities`, not `render.capabilities`.
4. Use `render.set_upscaler` with `backend: "xess"` and a quality preset. Native maps to AA;
   quality, balanced, performance and ultra_performance map to the same-named Intel presets.
   Query `vendor_frame.render_preferred` and `render_range`; do not assume the built-in ratios.
5. For fallback testing, `render.set_vendor_upscaler` with `vendor: "xess"` preserves the
   selection even if XeSS is unavailable. After a frame, inspect `render.get_upscalers`:
   `vendor_frame.active` must be true to count as XeSS evidence. A fallback is not SDK proof.
6. Capture matching native and XeSS views with `seeing.screenshot`; use `play.hitch_report`
   for timing and record GPU, driver, dimensions, mode, frame count and frame report.

The XeSS runtime row adds `binding_built`, `sdk_version`, `runtime_version`, `code`,
`instance_extensions`, and `device_extensions`. Bundle 3.0.2 contains SR DLL 2.0.2.68;
these versions must not be conflated. Devices created outside the XeSS hook are refused.
The adapter currently accepts RGBA16Float color/output, RG16Float or RGBA16Float XY motion, and Depth32Float
or R32Float depth, all single-sample full 2D views. Desktop profiles use the supported HDR
format; RGBA motion is converted to a cached RG16 target without filtering. RG11B10 mobile profiles fall back to the built-in upscaler.

The smoke test is opt-in and fails if explicitly enabled without a usable SDK/runtime/GPU:

```text
REGIA_XESS_SMOKE=1 REGIA_XESS_PATH=C:/work/regia/sdks/xess/bin cargo test -p regia_xess --features sdk --test gpu_smoke -- --nocapture
```

Set `XESS_SDK` and `VULKAN_SDK` for that build as described above. The test compares 48
jittered analytic frames against a supersampled native reference and a spatial baseline,
and checks the projection jitter sign against a deliberately flipped sign. It creates no
goldens. Its upload/submit/completion wall time is not a player GPU frame time.
On a machine with multiple GPUs, set `REGIA_XESS_GPU` to a case-sensitive adapter-name
substring for the analytic smoke (for example `RTX 3080`). It fails if no hardware adapter
matches. Use the renderer's `REGIA_GPU_NAME` selector for the frame smoke and player.
The renderer frame smoke also accepts `REGIA_XESS_EVIDENCE`, an absolute directory outside
the source tree, to save native and XeSS PNGs.

| Runtime code | Meaning |
|---|---|
| `disabled_by_env` | `REGIA_XESS=0` disabled the hook |
| `not_linked` | SDK binding was not built; availability reason is `sdk_not_bundled` |
| `runtime_not_found` | The absolute runtime search found no DLL |
| `not_vulkan`, `xess_needs_vulkan` | The device uses another API; the latter is the availability blocker |
| `unsupported_version` | The loaded SR ABI is unsupported or a development ABI |
| `device_not_prepared` | The device lacks the complete XeSS creation hook |
| `unsupported_device`, `unsupported` | The runtime rejects the GPU or operation |
| `needs_newer_driver` | The runtime requires a newer driver |
| `invalid_input` | Invalid sizes, formats, aliases, jitter, exposure or device identity |
| `out_of_gpu_memory` | Runtime allocation failed |
| `sdk_error` | Another Intel API error; logs include the API and numeric result |
| `backend_error` | Loader, Vulkan, synchronization or wgpu failure |
| `feature_not_found` | The frame attempted to use an already-released feature handle |

All codes use lowercase ASCII, digits and underscores. Missing or failed XeSS leaves the
built-in upscaler active; three consecutive evaluate failures disable the vendor until
reselected. Initialization may compile pipelines and block; this lane does not claim a
hitch-free first selection. This implementation does not use frame generation, low latency,
DX12, responsive masks or a Linux XeSS runtime.

When both external SDK features are built and both runtimes are found, the XeSS Vulkan
creation hook runs first. A vendor whose requirements were not enabled reports unavailable.
Set `REGIA_XESS=0` before launching to use the DLSS creation hook instead; changing this
variable after device creation does not retrofit the device's features or extensions.

For an isolated player proof, build `cargo build -p sim_bridge --example xess_capture
--features regia/xess` and launch the example in a new run directory. Set
`REGIA_PIPELINE_CACHE_DIR` to an absolute path inside that directory. It uses static procedural discs and a ground plane, local VFS mounts/logs/agent discovery, in-memory settings, muted audio, and a
960×540 logical window with immediate presentation. Use `render.set_profile` with
`preset: "Deck"` and `post: true` (TAA and RGBA16 HDR), then compare `taau`/`native` with `xess`/`quality`.
`quit.request` ends the capture. The example adds no simulation or content-format changes.

The November 8 lane remains partial: a the min-spec Windows laptop player capture executes XeSS with zero fallbacks,
but the expanded analytic smoke fails repeated-context equivalence when frames are queued.
`REGIA_XESS_SMOKE_SERIAL=1` adds a GPU completion wait after each test submission and passes
all unchanged assertions; this is a diagnostic, not a production workaround or a default-path
pass. See `docs/evidence/xess/2026-11-08/README.md` for source revisions and measured results.

## Determinism diagnostics (November 9)

The smoke's opt-in variables isolate why equally fed runs can differ. None changes an assertion:

| Variable | Effect |
|---|---|
| `REGIA_XESS_SMOKE_VIEWS=fresh\|retain\|persistent` | new views each frame (default), keep every frame's views until the run ends, or reuse one set |
| `REGIA_XESS_SMOKE_INFLIGHT=<n>` | at most `n` unfinished submissions (`1` waits for every frame) |
| `REGIA_XESS_SMOKE_SEQUENCES=<n>` | repeat the 48-frame reset sequence `n` times in the same context and print the differences |
| `REGIA_XESS_SMOKE_TRACE=1` | print a hash of every frame's output (waits for each frame) |
| `REGIA_XESS_SMOKE_ORDER=performance` | run the Performance mode before Quality |


XeSS reads parts of its temporary storage on a reset frame before writing them. With SDK
allocations, identical inputs then diverged at the very first frame of a later context (17 of 20
runs failed on the min-spec Windows laptop, Intel N150, driver 101.7088, SR 2.0.2). `regia_xess` now hands XeSS
zero-initialized temporary heaps of its own (`xessGetProperties` sizes, device-local
host-visible memory, freed with the context); `Context::zeroed_temp_heaps()` reports it and the
smoke asserts it. With them 50 of 51 runs pass every unchanged assertion and repeated reset
sequences in one context are identical. One divergent run remains unexplained. Where no suitable
memory type exists, or the SDK refuses the heaps, initialization logs a warning and falls back
to SDK allocations. See ADR-4907 and `docs/evidence/xess/2026-11-09/`.

## Frames in flight (November 14)

The one unexplained divergence above was a race: XeSS SR keeps state written while an execute is recorded in a
fixed number of slots, and corrupts queued frames when more than 10 executes are outstanding on the GPU. The smoke
submits 48 frames without waiting, so a release build (fast on the CPU) diverged in every Performance-first run on the
N150, while `REGIA_XESS_SMOKE_INFLIGHT=1` to `10` passed and `11` and above failed. `regia_xess` now paces each context
itself: before recording an execute it waits until the execute four evaluations back has finished on the GPU. Nothing to
configure; a real-time frame loop is rarely that far ahead. To check a build, run the smoke with
`REGIA_XESS_SMOKE_ORDER=performance` from a release build (`cargo test --release`) several times: every run must pass.
See ADR-7695 and `docs/evidence/xess/2026-11-14/`.

## Zeroed heaps without host-visible image memory (v0.5.1)

The heaps above were zeroed by the CPU, which needs device-local memory the host can see and the heap's resources may
use. An NVIDIA card without Resizable BAR (the RTX a test machine) has such memory only in its small BAR window, and that
memory takes buffers and linear images, not the optimal-tiled images of XeSS's texture heap. So XeSS ran on its own
allocations there and the smoke's zeroed-heap assertion failed (ADR-7718). `regia_xess` now tries, per heap and in this
order (ADR-7820):

| Method (`HeapZeroing`) | Code | When |
|---|---|---|
| `HostWrite` | `host_write` | a device-local, host-visible, coherent memory type takes the heap: the CPU writes zeros before `xessVKInit` (Intel GPUs, NVIDIA with Resizable BAR) |
| `DriverZeroed` | `driver_zeroed` | the device enabled `VK_EXT_zero_initialize_device_memory` (`Runtime::request_device` enables it where the GPU offers it): the heap is allocated with the zero-initialize flag |
| `DeviceFill` | `device_fill` | any device-local type that transfer buffers share with the heap: a buffer bound over the whole heap is filled with zeros at the start of the first `evaluate`'s commands, behind a barrier to every later command. That first command buffer must be submitted (a failed record keeps the fill for the next one) |

`Context::temp_heap_zeroing()` returns `[buffer heap, texture heap]`; the smoke prints it. The diagnostic
`REGIA_XESS_HEAP_ZEROING=host|driver|fill` allows only that method (unknown values warn and allow all); a heap the
method cannot serve falls back to SDK allocations with a warning, as before. Vulkan does not define what an image
created with an undefined layout reads from memory zeroed by any of these methods; the `regia_xess` lib test
`optimal_images_over_filled_memory_read_the_fill` measures it on the card it runs on, and the smoke's repeat and
equivalence assertions remain the proof.

The agent's XeSS runtime row (`render.get_upscalers`, `render.get_capabilities`) adds `temp_heaps`: the two codes of
the last initialized feature, `["sdk_allocated", "sdk_allocated"]` when XeSS took its own allocations, `null` before
the first feature.

## DX12 selection in XeSS and DLSS builds (v0.5.1)

The XeSS and DLSS creation hooks open a Vulkan-only instance. When `REGIA_GPU_INDEX` or `REGIA_GPU_NAME` selects a
DX12 adapter of the full listing (`regia_render::list_adapters` order: Vulkan adapters first, then DX12), the engine now skips both
hooks and keeps the full instance, so the variables force DX12 as in any other build (ADR-7820). The vendor then
reports `xess_needs_vulkan` / `dlss_needs_vulkan` for that device and the built-in upscaler runs. `REGIA_GPU_UUID` and
`REGIA_GPU_PCI` select by bus, where the Vulkan adapter comes first, so they keep the hooks. On a one-GPU NVIDIA
Windows host the DX12 row is `REGIA_GPU_INDEX=1 REGIA_GPU_STRICT=1`: the device opens on DX12 and the vendor falls
back (a test that requires the vendor to run fails there by design).

## Packaging the runtime (opt-in, v0.5.1)

Nothing from the SDK is in the repository or in a default package. Two opt-ins stage Intel's unmodified
`bin/libxess.dll` beside the executable with the SDK's `LICENSE.txt` and `third-party-programs.txt`, renamed
`INTEL-XESS-LICENSE.txt` and `INTEL-XESS-THIRD-PARTY-PROGRAMS.txt` (ADR-7820):

```text
REGIA_PACKAGE_XESS=1 XESS_SDK=C:/sdks/xess regia package --platform windows --project <game>
XESS_SDK=/abs/xess cargo xtask demo-package --xess-sdk /abs/xess
```

`regia package` stages the files for a Windows client package only (the game must enable `regia/xess` itself); any
other value than `0`/`1`, a server, Linux or Android package, a relative path, a missing or empty file, or a
`libxess.dll` that resolves to another library fails with `xess_package_invalid`. `demo-package --xess-sdk` builds the
Windows bundle with `regia/xess` (with `XESS_SDK` set to the same path; `--from-windows` binaries must have been built
that way), adds the XeSS notices section to that bundle's `licenses/THIRD_PARTY_NOTICES.md`, records `"xess": true`
for the target in `demo-package.json`, and `--verify` then requires the three files and the section in the bundle and
the depot (`demo_package_xess`; a bundle without the opt-in must hold none of them). `cargo xtask notices` lists the
XeSS obligations in the engine notices. Shipping the runtime stays the owner's decision (ADR-7695).

| Code | Meaning |
|---|---|
| `xess_package_invalid` | invalid opt-in value, target, SDK path or file, or staging IO failed |
| `demo_package_xess` | a demo bundle or depot lacks a staged XeSS file or its notices section, or holds one without the opt-in |
