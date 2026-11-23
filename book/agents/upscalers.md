# Upscalers and quality modes (agents)

A TAA frame renders below the display size and a temporal upscaler brings it up to the display.
Regia Engine ships three upscalers: **TAAU** (the default on every preset), the **TSR**-grade upscaler and an
**FSR 3.1** port. All three run on every platform Regia Engine builds for. **DLSS** runs in builds with the
`dlss` feature and NVIDIA's SDK linked, on NVIDIA GPUs over Vulkan, when its runtime library ships with the
game (see [DLSS](#dlss)). **XeSS** runs on Windows when the `xess` feature linked the XeSS SDK and the
runtime library is present. Either vendor is refused only when that row's blockers are non-empty. The
upscaler only runs on TAA frames (`current.temporal`). Any other frame keeps the profile's AA and its spatial
upscaler.

The **quality mode** sets the render size from the display size. It uses the usual per-axis ratios:

| Mode | Ratio | Scale (permille) | 2560×1440 renders | 3840×2160 renders | Dynamic resolution range |
|---|---|---|---|---|---|
| `native` | 1.0 | 1000 | 2560×1440 | 3840×2160 | 667–1000 |
| `quality` | 1.5 | 667 | 1707×960 | 2560×1440 | 588–667 |
| `balanced` | 1.7 | 588 | 1506×847 | 2259×1271 | 500–588 |
| `performance` | 2.0 | 500 | 1280×720 | 1920×1080 | 333–500 |
| `ultra_performance` | 3.0 | 333 | 853×480 | 1280×720 | 333 |

To get each axis, the display size is divided by the ratio and rounded to the nearest integer, with halves
rounding up. The ratio is used as an exact fraction (3/2, 17/10, ...), so the result is exact. No preset sets
a mode (`quality: null`). Without a mode the profile's own `render_scale` applies, as it always has. With a
mode, dynamic resolution moves inside the mode's range (`DynResConfig::within_quality`). At the top of that
range the frame is exactly the size in the table.

## Methods

- `render.get_upscalers` returns `{ current, upscalers, qualities, adapter, vendor_frame, reactive_mask }`.
  - `current` is `{ backend, quality, render_scale, temporal }`: the profile's built-in upscaler (`taau`, `tsr` or
    `fsr3`), the mode (`null` when the frame uses the profile field), that profile's `render_scale`,
    and whether the profile runs TAA. Setting a mode does not change `render_scale` (Desktop High
    stays `1.0`). The frame's scale is the mode's permille ÷ 1000 while `quality` is set (Performance
    `0.5`, Balanced `0.588`, Ultra Performance `0.333`) and `render_scale` while `quality` is null.
    `vendor_frame.selected` names the vendor tried before this built-in fallback.
  - `upscalers` holds one `{ backend, vendor, available, reason, blockers, unavailable_code, inputs, runtime }`
    per upscaler, in the order `taau`, `tsr`, `fsr3`, `dlss`, `xess`. `reason` is the first entry of `blockers`
    (`null` when the upscaler is available). `unavailable_code` is the one code that says what to fix (see
    [Why a vendor cannot run](#why-a-vendor-cannot-run)).
  - `inputs` is the backend's input contract (see [Input contract](#input-contract)).
  - `runtime` says where the backend's code comes from. A built-in has `{ "state": "compiled_in" }`. A
    vendor row has `{ state, env_var, library_names, found, searched, refused_relative }`: this build's
    binding state (see [Runtime states](#runtime-states)), the environment variable that names an extra
    directory, the library file names it looks for on this OS (`<version>` stands for a numeric suffix),
    the file it found (or `null`), the directories it looked in, and the variable's value if it was refused
    for being a relative path. The search only lists directories. It never loads a library and never looks
    in the working directory. The DLSS row also has `binding_built` (true when this build compiled the NGX
    sources, including the check stub),
    `sdk_version` (`none` for the stub, `check-stub` for `REGIA_DLSS_CHECK_STUB=1`, a dotted suffix such as
    `310.9.1` from `libnvidia-ngx-dlss.so.<version>` on Linux, `windows` when the file is `nvngx_dlss.dll`,
    or `unknown` when a linked build finds neither name), `code` (NGX's or the binding's last answer, see [DLSS](#dlss)) and
    the `instance_extensions` and `device_extensions` device creation added for it (the lists of the device
    the hooks made last: diagnostics only). Availability asks NGX about this device, and an empty
    requirement list means those extensions are enabled. Without the DLSS binding that ask does not
    run, and an empty recorded list is `needs_dx12_or_vulkan_extension`.
  - `resolve` says where a built-in resolve differs from the reference algorithm it ports. It is `null` on
    every row but `fsr3`, which has `{ reference, differences }`: the SDK it ports (`"AMD FidelityFX SDK
    v1.1.4, FSR 3.1 upscaler"`) and one `{ id, adr, summary }` per change to what the resolve outputs
    (for example `nearest_sample_restart`, ADR-7678). Regia Engine's FSR 3.1 is that SDK's algorithm with
    these changes, not the SDK unchanged.
  - `qualities` is the table above as `{ quality, ratio, permille, dynamic_range_permille,
    render_1440p, render_2160p }`.
  - `adapter` is `{ name, vendor, device_type, backend }` for the GPU the check looked at, or `null` when
    there is no render device.
  - `vendor_frame` is what the frame's vendor slot did: `{ selected, active, fallback, fallback_backend,
    reason, frames, fallbacks, runtime, render_range, render_preferred }`. `selected` is the vendor chosen
    (`dlss`, `xess` or `null`). `active` is true when the last TAA frame ran it. Otherwise `fallback` is the
    error code of why the built-in upscaler ran instead (see [Errors](#errors)), `fallback_backend` names that
    built-in, and `reason` is the runtime's own reason when the runtime is the cause (see [Reasons](#reasons)).
    `frames` and `fallbacks` count frames. `runtime` is the binding's state, `render_range` is `{ min, max }`,
    the render sizes the vendor accepts, and `render_preferred` is the size it asks for at the frame's mode.
    It is `null` without a runtime renderer.
  - `reactive_mask` is what fed the reactive mask on the last frame: `{ sources, present, readers, layers:
    { installed, placement, writes_mask } }`. `sources` lists the passes that wrote it (`oit`, or empty when
    the frame had none) and `present` is true when it had one. `readers` are the passes that read it (`taau`,
    `tsr`, `fsr3`, `vendor`, `frame_gen`). `layers` counts the installed translucent layers (VFX particles,
    trails, decals) and says where they draw: `after_resolve`, so they never write the mask (see
    [Particles and the reactive mask](#particles-and-the-reactive-mask)). It is `null` without a runtime
    renderer.
  - Read-only.
- `render.set_upscaler {"backend": "tsr", "quality": "quality"}` sets the upscaler, the mode, or both,
  from the next frame.
  - `backend` is `taau`, `tsr`, `fsr3`, `dlss` or `xess`. Case, `-`, `_` and spaces are ignored. A vendor
    that is available here is selected on the renderer and runs before the profile's built-in upscaler. A
    built-in clears that selection. An available vendor requires `RuntimeRenderer`; without it the call
    returns `F0003` and leaves the profile unchanged.
  - `quality` is a mode name. `"custom"` or `null` goes back to the profile's `render_scale`. Leaving
    `quality` out keeps the current mode.
  - Every parameter is checked before anything changes: a refused call changes neither field.
  - Switching upscalers drops the history of the one switched to. The switch is not persisted.
  - Returns `{ before, after, note }`. `before` and `after` are the `render.get_upscalers` state,
    including `vendor_frame`. `vendor_frame` is `null` when the app has no runtime renderer. A
    vendor this call selects is `after.vendor_frame.selected`; a built-in clears it. A
    quality-only call leaves the selection in place and still reports it.
  - Mutating; permission `edit_scene`.

```json
{"method": "render.set_upscaler", "params": {"backend": "fsr3", "quality": "performance"}}
```

- `render.reference_upscale {"filter": "edge_adaptive", "quality": "performance", "frames": 32}` runs the
  software reference upscaler on the CPU on a synthetic scene and returns its error against an 8×8
  supersampled truth. It goes through the same boundary a vendor upscaler plugs into, so it checks the
  jitter, motion-vector and reactive-mask conventions without a GPU.
  - Parameters, all optional: `filter` (`bilinear` or `edge_adaptive`), `quality` (sets the render size),
    `display` (`[w, h]`, 8 to 128), `frames` (1 to 64), `motion_px` (`[x, y]` display pixels per frame,
    -8 to 8), `motion_vectors` (`correct`, `zero` or `negated`), `reactive` (boolean), `sharpness` (0 to 1).
  - Returns `{ check, render, first_frame_rmse, final_rmse, spatial_only_rmse, accumulation_gain, note }`.
    `accumulation_gain` is `spatial_only_rmse ÷ final_rmse`: above 1 means the jitter and the history beat
    the spatial filter alone. With `motion_vectors: "negated"` or `"zero"` on a moving scene the gain drops,
    which is what a sign mistake looks like.
  - Read-only. It needs no renderer.
- `render.upscaler_jitter {"quality": "performance", "display": [2560, 1440], "frames": 8}` lists a
  Halton(2, 3) sequence whose phase count is `8 × ratio²`, rounded: 8, 18, 23, 32 and 72 frames for the
  five modes. This list repeats after `phase_count` frames. The frame's projection does not. It calls
  the same Halton on the unbounded index since the last camera cut, so frames 1 through `phase_count`
  match and the next frame of this list is index 1 while the projection uses index `phase_count + 1`.
  FSR reads the phase count only to pace lock decay.
  - Parameters, all optional: `quality` (default `quality`) or `render` (`[w, h]`, not both), `display`
    (`[w, h]`, default `[2560, 1440]`, axes 1 to 16384; `render` may not be larger), `frames` (1 to 256,
    default 8).
  - Returns `{ quality, display, render, ratio, sequence, phase_count, offsets, note }`. Each offset is
    `{ frame, halton_index, render_pixels, ndc }`. `render_pixels` is y down and is the offset the
    projection was moved by: what DLSS and XeSS are given. `ndc` is y up and is what is added to the
    projection.
  - Read-only. It needs no renderer.

```json
{"method": "render.upscaler_jitter", "params": {"quality": "ultra_performance", "frames": 72}}
```

- `render.set_vendor_upscaler {"vendor": "dlss"}` selects a vendor upscaler (`dlss`, `xess`, or `none` to
  clear it) on the runtime renderer, even where it cannot run. Unlike `render.set_upscaler` it never refuses:
  use it to check the fallback. Every TAA frame then tries the vendor. When the vendor refuses or fails, that
  frame runs the built-in upscaler, and `vendor_frame.fallback` in `render.get_upscalers` says why.
  - Returns `{ before, after, runtime, available, reason, blockers, unavailable_code, fallback_backend }`:
    `vendor_frame` before and after, the vendor's runtime state, whether it can run on this device with the
    same reasons `render.get_upscalers` lists, and the built-in upscaler that runs when it cannot. All of
    those are `null` for `none`.
  - Errors: `unknown_upscaler` for a name that is not a vendor upscaler. Without a runtime renderer, the
    `F0003` not-found error.
  - Mutating; permission `edit_scene`. Not persisted.

```json
{"method": "render.set_vendor_upscaler", "params": {"vendor": "dlss"}}
```

## Input contract

Each backend declares what it reads. The frame always passes Regia Engine's own conventions, and a backend
converts them into its own.

| Field | Values | Meaning |
|---|---|---|
| `depth`, `motion`, `reactive`, `transparency` | `required`, `optional`, `unused` | Whether the backend reads the input. A frame without a required input is refused |
| `motion_units` | `uv`, `render_pixels` | Units of a motion vector |
| `motion_direction` | `to_previous`, `to_current` | Regia Engine's velocity target is `previous − current` |
| `jitter_units` | `ndc`, `render_pixels` | Units of the jitter. One render pixel is 2 ÷ width in NDC, and NDC y is up |
| `jitter_sign` | `projection`, `sample` | The offset the projection moved by, or its negation |
| `vulkan_device_hook` | boolean | The runtime needs Vulkan extensions enabled at device creation |
| `max_ratio` | number | The largest display ÷ render ratio per axis |

The built-ins use Regia Engine's conventions (`uv`, `to_previous`, `ndc`, `projection`). The vendor rows use
render pixels with the same direction and sign. That is how the public integration guides describe them;
the vendor work confirms each sign against the SDK before it ships (ADR-510, ADR-1150).

A frame with [order-independent transparency](oit.md) feeds `reactive` from its composite: the covered share of each pixel, capped at
0.9. TAAU scales its history weight by one minus the mask; TSR, FSR 3 and the vendor upscalers read it as their reactive input. Other
frames pass no mask, and each backend runs as it did.

### Particles and the reactive mask

Particles, trails and decals need no reactive mask. The frame draws its translucent layers after the
temporal resolve, into a copy of the resolve's output-sized result; the resolve itself stays next frame's
history. A particle is never in an upscaler's input or history, so smoke, sparks and fire moving over a
moving background leave no trail behind them under TAAU, TSR, FSR 3.1 or a vendor upscaler, and the
background under them is resolved as if they were not there. A mask from the particles would only lower the
history weight of that background. `reactive_mask.layers.placement` reports this (`after_resolve`) and
`reactive_mask.sources` never lists the layers.

The check is `cargo test -p regia_vfx --test reactive_upscaler`: a bright spark moving 3.7 px per frame over a
striped wall while the camera pans, through each upscaler at render scale 0.67, against the same frames
without the spark. Eight frames after the spark passes, its old footprint matches the spark-free frame
within one 8-bit level, and so does every pixel away from it (ADR-3101).

Frame generation is the one reader that sees the particles: it interpolates the presented image, and a
particle has no motion vector of its own. Its interpolator already lowers its trust in the game vector where
the mask is set. A mask from the VFX layers for frame generation is designed in ADR-3101 and not built yet;
until then a generated frame moves a particle with the surface behind it.

## Errors

| `data.error` | JSON-RPC / `data.code` | When | Also in `data` |
|---|---|---|---|
| `upscaler_unavailable` | `-32004` / `F0013` | The upscaler cannot run here: that backend's blockers are non-empty (a missing SDK link, the GPU, the API, an extension, or the runtime). DLSS and XeSS are accepted when their blockers are empty | `backend`, `reason`, `blockers` |
| `unknown_upscaler` | `-32602` / `F0002` | `backend` is not one of the five names | `backend` |
| `unknown_quality` | `-32602` / `F0002` | `quality` is not a mode name, `custom` or `null` | `quality` |
| `unknown_filter` | `-32602` / `F0002` | `render.reference_upscale`: `filter` is not `bilinear` or `edge_adaptive` | `filter` |
| `unknown_motion_vectors` | `-32602` / `F0002` | `render.reference_upscale`: `motion_vectors` is not `correct`, `zero` or `negated` | `motion_vectors` |
| `out_of_range` | `-32602` / `F0002` | A number or size is outside the range given above | `param`, `range` |

`render.reference_upscale` can also return an upscaler error code on `F0002`. These are the codes any
backend behind the boundary reports. The frame falls back to a built-in upscaler when a backend fails:

| Code | When |
|---|---|
| `invalid_size` | A size is zero, or the render size is larger than the display |
| `unsupported_ratio` | The display ÷ render ratio is beyond the backend's `max_ratio` |
| `missing_input` | A required input is missing |
| `size_mismatch` | An image is not the size the frame says |
| `backend_failed` | The backend's runtime failed |
| `runtime_unavailable` | A vendor runtime is not usable (its reason is one of the runtime reasons below) |
| `render_size_out_of_range` | The render size is outside the range the vendor feature was created for |
| `invalid_jitter` | The jitter is more than half a render pixel: it was not given in NDC |
| `backend_disabled` | The backend failed 3 times in a row and stays off until it is re-enabled |

## Reasons

| Reason | Meaning |
|---|---|
| `sdk_not_bundled` | This build has no runtime for the vendor's SDK (vendor features without their external SDK) |
| `needs_nvidia_adapter` | DLSS runs only on NVIDIA GPUs |
| `needs_dx12_or_vulkan` | The device runs on GL, Metal or WebGPU |
| `needs_dx12_or_vulkan_extension` | Vulkan, and this device does not enable the extensions the vendor asked for. DLSS asks NGX for this device, and an empty requirement list counts as enabled. Without that binding, an empty recorded hook list is this reason |
| `needs_hardware_adapter` | A software rasterizer such as lavapipe |
| `needs_desktop_gpu` | A mobile GPU (Adreno, Mali, PowerVR, Apple) |
| `no_render_device` | There is no render device (a headless tool, or before the first frame) |
| `runtime_not_found` | The vendor binding is built in, but its library was not found |
| `runtime_not_initialized` | The vendor binding is built in and its library was found, but no render device has started it yet |
| `runtime_reports_unsupported` | The vendor library loaded and says the feature cannot run on this GPU |
| `needs_newer_driver` | The vendor library loaded and asks for a newer GPU driver |
| `runtime_init_failed` | The vendor library was found but did not start, and neither the GPU nor the driver is why: a Vulkan or wgpu failure, or an error during init |
| `xess_needs_vulkan` | This XeSS adapter implements Vulkan; the selected device uses another API |
| `dlss_needs_vulkan` | The DLSS binding drives NGX through Vulkan only; the selected NVIDIA device runs on DX12. Pick the Vulkan adapter (`REGIA_GPU_INDEX`) |

## Why a vendor cannot run

`unavailable_code` on a `render.get_upscalers` row and on the `render.set_vendor_upscaler` result is the one
code to act on. A GPU that can never run the vendor comes first, then the runtime's state, then the device.
A device without the vendor's Vulkan extensions is what a build with no SDK looks like, so on Vulkan with no
SDK the code is `sdk_not_bundled` where `reason` is `needs_dx12_or_vulkan_extension`.

| Situation | `unavailable_code` | DLSS `runtime.code` |
|---|---|---|
| No SDK in this build (also Android and macOS, where NGX is not linked even if `DLSS_SDK` is set) | `sdk_not_bundled` | `null` (`runtime.state` is `not_bundled`; the agent does not report `not_linked`) |
| The runtime library is not there | `runtime_not_found` | `null` |
| A GPU that is not NVIDIA | `needs_nvidia_adapter` | `null` |
| An NVIDIA GPU DLSS does not support, such as a GTX 1080 | `runtime_reports_unsupported` | `feature_not_supported` (refused at device creation; measured on a GTX 1080, driver 580.173.02) or `unavailable` |
| The driver is too old | `needs_newer_driver` | `needs_newer_driver` |
| The runtime failed to start | `runtime_init_failed` | the failure's name, such as `vulkan_error` |

The DLSS row's `runtime.state` and `unavailable_code` follow the last answer NGX gave for a device in this
process: after the GTX 1080's refusal they read `reports_unsupported` and `runtime_reports_unsupported`, the
same state the frame's `vendor_frame` fell back on (ADR-4937).

Selecting a vendor that cannot run never breaks a frame. `render.set_upscaler` refuses it and changes nothing.
`render.set_vendor_upscaler` keeps the selection: every TAA frame runs the built-in upscaler, pixel for pixel
what it renders with no vendor selected, and `vendor_frame` shows `fallback`, `fallback_backend` and `reason`.

## Render size under a vendor

A vendor runtime asks for its own render sizes: the size it prefers at a display size and mode, and the
smallest and largest it accepts. While a vendor runs, the frame follows that answer instead of the mode's
fixed scale (`render_preferred`, `render_range`):

- Under a quality mode, the mode's own scale renders exactly the preferred size. Another scale, from dynamic
  resolution, multiplies the preferred size by `scale ÷ mode scale`, so a step never jumps.
- With no mode, the frame keeps the size its `render_scale` asks for.
- Either way the size stays inside `render_range`. A runtime whose smallest and largest size are equal holds
  that one size at every scale.

`DynResConfig::within_preferred` gives dynamic resolution the matching range. A frame with no vendor selected,
or a vendor that cannot run, renders the size it always did. Frames a vendor does not run (MSAA, or no TAA)
are also unchanged.

## Runtime states

| `runtime.state` | Meaning |
|---|---|
| `compiled_in` | A built-in upscaler: nothing to load |
| `not_bundled` | This build has no binding for the vendor (XeSS without the `xess` feature and external SDK; DLSS without its feature and SDK) |
| `not_found` | The binding is built in, but the library was not found |
| `found` | The binding is built in and the library was found; no render device has started it yet |
| `reports_unsupported` | The library loaded and reports the feature unusable here |
| `needs_newer_driver` | The library loaded and wants a newer driver |
| `init_failed` | The library was found but did not start, and neither the GPU nor the driver is why |
| `ready` | Loaded and usable |

All codes are lowercase ASCII letters, digits and `_`, and they are stable.

## Judging an upscaler

Use the same view and the same mode for each upscaler you compare. Take a `seeing.screenshot` with each one.
For cost, read the GPU `taau` interval in `play.hitch_report`: it times the velocity pass and the upscale,
whichever upscaler runs. For motion, use the spec 27 § 2.3 chart (`regia_render/tests/taa_motion.rs`,
`do27_chase --motion`). Judge the result by eye against the reference plates, not by test counts.

## DLSS

DLSS Super Resolution runs through NVIDIA's NGX on wgpu's Vulkan backend (ADR-2002). It is off unless a game
is built with it:

The default build uses the DLSS stub: calls return `not_linked`, and it names no vendor library path.

1. Build with the `dlss` feature (`regia`, `regia_render` or `regia_dylib`) and `DLSS_SDK` set to an absolute
   path to NVIDIA's DLSS SDK checkout (v310.9.1). The SDK is never committed. Without `DLSS_SDK` the feature
   still builds, and DLSS reports `not_bundled`.
2. Ship the runtime library next to the executable: `libnvidia-ngx-dlss.so.<version>` on Linux,
   `nvngx_dlss.dll` on Windows. `REGIA_DLSS_PATH` names another directory, which must be an absolute path.
3. On an NVIDIA GPU over Vulkan, device creation then adds the extensions NGX asks for, and the first TAA
   frame with DLSS selected starts NGX. `REGIA_DLSS=0` keeps DLSS out of device creation.

Every step fails soft. A missing library, a GPU that is not NVIDIA, an old driver, a render size outside the
feature's range or an NGX error each leaves the frame on the built-in upscaler, with the reason in
`vendor_frame.fallback` and the DLSS row's `runtime.code`. After 3 failures in a row the vendor turns itself
off (`backend_disabled`) until it is selected again. The binding's codes are:

| `runtime.code` | Meaning |
|---|---|
| `null` | No problem so far |
| `disabled_by_env` | `REGIA_DLSS=0` |
| `not_linked` | `DlssError` from the stub when something calls the unbound NGX API. The agent status does not use it: a build that did not link leaves `runtime.code` null and `runtime.state` `not_bundled`, including Android and macOS |
| `not_vulkan` | The device is not on Vulkan |
| `unavailable` | NGX started and reports DLSS unavailable on this GPU. A GPU NGX refuses already at device creation shows `feature_not_supported` instead (a GTX 1080 does, with the runtime present); a missing runtime can show as `unable_to_initialize_feature` |
| `needs_newer_driver` | NGX asks for a newer driver |
| `device_without_ngx_extensions` | The device (or its instance) lacks an extension NGX asks for on this device's GPU, checked per device |
| `vulkan_error`, `wgpu_error` | A Vulkan or wgpu call failed during instance or device creation (a missing Vulkan loader is `wgpu_error`) |
| `poisoned` | A previous NGX call panicked and left the NGX lock poisoned |
| `unsupported_format` | An input texture format NGX cannot take |
| an NGX failure name | `feature_not_supported`, `platform_error`, `unable_to_initialize_feature`, `invalid_parameter` (also a parent texture smaller than `render_size`, an output parent smaller than the display size, or an exposure that is not 1×1), `out_of_gpu_memory`, `denied`, `not_implemented` and the other NGX results, snake_case |

What DLSS is given:

- The frame's HDR colour, standard depth (near 0, far 1) and motion vectors at the render size. The motion is passed with a
  render-size scale, so it reaches DLSS in render pixels toward the previous frame.
- The frame's jitter, in render pixels with y down: the offset the projection was moved by.
- The reactive mask, as DLSS's current-colour bias, when the frame has one.
- The exposure, as a 1×1 texture, when the frame passes one. Creating the feature with auto exposure does not drop this texture: a size other than 1×1 still fails the call.
- A frame time of 16.667 ms on the render frame's vendor path (`UpscaleParams::new`). That path does not pass the measured frame delta, so every such frame is one sixtieth of a second.
- The quality mode the host set, or the one nearest the frame's render scale. While the live feature accepts
  the render size it keeps its mode, so dynamic resolution does not recreate it. Native is DLAA.
- A history reset on the first frame, on a camera cut and on a size change.
- A render size from DLSS's own answer (see [Render size under a vendor](#render-size-under-a-vendor)).

The reactive mask has been read through the code path only: no run has shown DLSS reading it on a GPU.

On an RTX 5090 (driver 615.71.09, SDK 310.9.1) an analytic test scene upscaled at Quality (640×360 to
960×540) has an RMSE against the supersampled native image of 0.039, where bilinear gives 0.097. At
Performance the numbers are 0.041 and 0.124. The flipped jitter sign gives 0.146 and 0.206. In the frame, at
two-thirds scale, DLSS lands 0.25 8-bit levels from the native frame on average, and TAAU lands 0.71
(`regia_render/tests/vendor_upscaler_frame.rs`). These RTX 5090 RMSE and 0.25-level figures were measured
while `FeatureFlags::REGIA.depth_inverted` was still `true`, and have not been remeasured since the flag was
corrected to `false`.

XeSS uses the optional Windows Vulkan adapter (ADR-4656). See [XeSS setup and proof](xess.md).
The adapter consumes color, depth, motion, jitter, exposure and reset; reactive and transparency masks
are currently unused. Native maps to AA, and other quality presets map by name. Render sizes come from
XeSS because its preset ratios differ from the built-in table.

## Conformance checklist for a vendor binding

A new binding implements `VendorRuntimeApi` and `VendorEvaluate<GpuTarget>`, and must receive exactly what the
frame gives every other vendor. `regia_render/tests/vendor_conformance.rs` runs the frame's vendor slot on any
GPU with a scripted binding that records every input and upscales with a known filter. Set
`REGIA_VENDOR_CONFORMANCE` to `dlss` or `xess` to record the real binding instead.

| Item | What the binding must receive |
|---|---|
| `render_size` | The frame's render size in the frame values, the evaluate values and the inputs |
| `display_size` | The frame's display size, and an output of that size |
| `jitter_units` | The frame's NDC jitter converted by the binding's contract: render pixels, under half a pixel |
| `jitter_sign` | The offset the projection moved by, y down: `(ndc.x × width ÷ 2, −ndc.y × height ÷ 2)` |
| `motion_scale` | The render size as the factor from UV to render pixels, toward the previous frame, unjittered |
| `exposure` | Post exposure ÷ pre-exposure, in HDR colour, with no auto exposure |
| `reset` | True on the first evaluate, on a camera cut and on a new display size; false otherwise |
| `depth_convention` | Single-sample `Depth32Float` and a creation flag that says standard depth: near is smaller |
| `reactive_mask` | `R8Unorm` at the render size when the frame has one, absent otherwise |
| `transparency_mask` | Absent: no frame passes one yet |
