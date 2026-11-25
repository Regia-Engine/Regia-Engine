# Dynamic resolution

<!-- index: Dynamic resolution -->
<!-- index: Upscale quality -->

`regia_render::DynResController` chooses an internal render scale in permille (`1000` is
native, `500` is half) from GPU time. It is a pure function of the samples you give it: it does
not read the device, it does not change `RenderSettings`, and it does not write the settings
file. Apply the permille it returns to the frame you are about to draw, and keep that value in
the controller. Writing `render_scale` on every step fsyncs on the main thread.

DO27 can adopt this controller or keep its own. Either way the decision keys on GPU time against
the wall frame. A frame whose GPU time is well under the wall time holds or raises the scale,
even when the GPU time itself is over the frame budget. The bug this closes is a CPU-bound scene
(about 31 ms of GPU time inside a 66 ms wall frame, near 15 fps) whose scale fell from 1000 to
500 because the controller only compared GPU time with the budget.

The Android thermal governor is a different policy. It scales for heat, not for frame time. This
page does not change it.

## What one frame means

Pass two clocks, both in milliseconds:

- `gpu_ms` is [`GpuTimings::frame_ms`](../agents/renderer.md): the GPU's busy time for the frame
  encoder. Timestamps land two frames late. Until the first one arrives the value is `0`, and the
  controller ignores `0`.
- `frame_ms` is the wall interval between presented frames (the interval the performance overlay
  graphs). `FrameCpuTimings::total_ms` covers `render_into` only; pass the present interval here.

`classify_bound` classifies that pair:

| GPU time / wall time | Bound |
|---|---|
| strictly under 60% | `CpuBound` |
| strictly over 90% | `GpuBound` |
| from 60% through 90%, or a non-finite / non-positive input | `Balanced` |

The 90% line is the same "GPU-bound" idea the performance overlay prints. The 60% line is
stricter: a frame that is only somewhat busy on the GPU is `Balanced`, and a balanced frame does
not step the scale down.

## When the scale moves

The controller keeps a rolling median of the accepted `gpu_ms` samples
(`DynResConfig::window_frames`, default 30). It does not move until that window is full, so one
spike cannot change the scale. Defaults:

| Knob | Default |
|---|---|
| `budget_ms` | 16.6 ms of GPU time |
| `min_permille` / `max_permille` | 500 / 1000 |
| `step_down_permille` / `step_up_permille` | 50 / 25 |
| `window_frames` | 30 |
| `hysteresis` | 0.1 (a deadband of ±10% around the budget) |
| `cooldown_frames` | 60 accepted samples between changes |

On a full window, after the cooldown:

- **Down** when the median is strictly over `budget_ms * (1 + hysteresis)` and the current frame
  is `GpuBound`. A `CpuBound` or `Balanced` frame never steps down.
- **Up** when the median is strictly under `budget_ms * (1 - hysteresis)`, whatever the bound is.
  A scale that was pushed down comes back up once the GPU work drops, including while the frame
  is still CPU-bound.
- **Hold** inside the deadband, on the edges, and while the cooldown is running.

After a change, the next `cooldown_frames` accepted samples keep the new permille. The sample
after those may change again. A non-finite or non-positive sample is dropped whole: it does not
enter the median, it does not tick the cooldown, and it does not change the last bound. The same
inputs always produce the same permille trace.

`min_permille` greater than `max_permille` is clamped down to `max_permille` (the range becomes
that one value). A non-finite or non-positive budget becomes 16.6 ms. `hysteresis` is clamped to 0.0..=0.99 (non-finite becomes 0.1). A window of 0 becomes 1,
and a window above 4096 is capped. The starting permille is clamped into the sanitized range.

## Calling it

```rust
use regia_render::{DynResConfig, DynResController, FrameQuality};

fn start() -> DynResController {
    DynResController::new(DynResConfig::default(), 1_000)
}

fn apply(
    controller: &mut DynResController,
    quality: &mut FrameQuality,
    gpu_ms: f32,
    frame_ms: f32,
) {
    let permille = controller.update(gpu_ms, frame_ms);
    let capped = permille.min(u32::from(u16::MAX));
    quality.render_scale_permille = u16::try_from(capped).unwrap_or(u16::MAX);
}
```

`FrameQuality::render_scale_permille` is a `u16`. The default range 500..=1000 fits. A host that
raises `max_permille` past 65535 has to clamp before that assignment; the controller itself
returns a `u32`.

Read `permille()`, `bound()` (`FrameBound::label` is `"cpu"`, `"gpu"` or `"balanced"`) and
`median_gpu_ms()` for a HUD or a log. This crate does not emit a telemetry event. A later
telemetry row can use the names `dynres_permille`, `dynres_bound` and `dynres_median_gpu_ms`.
`median_gpu_ms()` is `0` until the first accepted sample.

The owner's case, as a unit test in `dynamic_res`: GPU 31 ms, wall 66 ms, budget 16.6 ms, three
minutes at about 15 fps, permille stays 1000. A GPU-bound overload steps down by 50 to 500 and
stops there, then steps up by 25 when the load drops.

## Inside an upscale quality mode

When `RenderProfile::upscale_quality` is set, the frame's top size is that mode's render size,
not the profile's `render_scale`. Build the controller's range from the mode so a step cannot
leave it:

```rust
use regia_render::{DynResConfig, DynResController, UpscaleQuality};

fn controller_for(mode: UpscaleQuality) -> DynResController {
    let config = DynResConfig::default().within_quality(mode);
    DynResController::new(config, mode.permille())
}
```

`UpscaleQuality::permille_range` is Native 667–1000, Quality 588–667, Balanced 500–588,
Performance 333–500. Ultra Performance holds 333. A vendor runtime that publishes its own range
uses `DynResConfig::within_preferred` instead. The quality modes and the present modes are in
[Quality and upscalers](../how-to/picture/quality-and-upscalers.md).
