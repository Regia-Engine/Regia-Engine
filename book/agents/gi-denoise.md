# GI denoise

A noisy per-pixel GI signal (a screen-space gather, a stochastic probe lookup, later a per-pixel
ray tracer) needs a spatio-temporal filter before it is shown: one sample per pixel has an error
about as large as the signal itself, and it changes every frame. `regia_render::gi_denoise`
is that filter (ADR-864). This page explains what it does, how its quality is scored, and how an
agent checks it with `seeing.gi_denoise_check`.

The filter is not yet wired into the frame: it ships as a CPU reference (`GiDenoiser`), a GPU
twin (`GiDenoiseGpu`, `shaders/gi_denoise.wgsl`) held to the reference by a test, the scores and
the regression scenarios. Feeding it the screen-space bounce is the next step.

## What the filter does

Three passes per frame, the same on the CPU and the GPU:

1. **Temporal accumulation with history validation.** Each pixel follows its motion vector into
   the previous frame and reads the history there (bilinearly). A history sample is used only if
   its depth is within `depth_tolerance` (10 %) of the pixel's, its normal within
   `normal_tolerance` (cosine 0.9), and it is on screen. Otherwise the pixel starts over: that is
   what keeps a moving object's light from smearing onto the wall it uncovers. The new sample
   blends in with weight `max(1/n, alpha_color)` for `n` frames of history (a running mean while
   young, an exponential average once settled). The first two luminance moments accumulate too.
   **Anti-lag:** per 8×8 square, the mean of this frame's input is compared with the mean of the
   reprojected history. When they differ by more than `antilag_sigma` (5) standard errors of the
   input's mean, the change is the lighting, not noise: the square takes the new frame in with at
   least the relative size of the change and its history is shortened to match. A light that
   quadruples settles in 2 frames instead of 9; on still, unchanging light the squares almost
   never fire.
2. **Variance.** A settled pixel's noise is the variance of its accumulated luminance. A pixel with
   fewer than four frames of history estimates it from its neighbours on the same surface instead.
3. **Edge-avoiding à-trous passes.** Five passes of a 5×5 kernel with tap spacing 1, 2, 4, 8 and
   16 pixels. Each tap is weighted by depth, normal and luminance; the luminance weight is scaled by
   the pixel's standard deviation, so a noisy pixel blurs wide and a converged one keeps its
   edges.

`GiDenoiser::reset` drops the history (call it on a camera cut); a resize drops it too. NaN,
infinite and negative input is treated as 0. Pixels without a surface (depth 0, NaN or infinite)
output 0.

## Scoring: the synthetic scenarios

The scores need a ground truth, so the regression tests and the agent method use deterministic
synthetic sequences (`gi_denoise::scenario`): a wall with a smooth light gradient and a hard
shadow band, a floor with a 90° crease, and a bright box in front at half the depth. The noisy
input is the truth times unit exponential noise per pixel (one path-traced bounce sample has about
this spread). Every run is bit-reproducible.

| Scenario | What moves | What it checks |
|---|---|---|
| `static` | nothing | convergence, flicker, bias, leak |
| `pan` | the camera, `pan_px` per frame | reprojection; a new column enters every frame |
| `disocclusion` | the box, `box_speed` per frame | history rejection behind the box |
| `light_step` | the light: 25 % → 100 % at `step_frame` | lag |

The scores, all relative to the truth's mean luminance:

| Score | Meaning | Good |
|---|---|---|
| `rel_rmse` (`raw_rel_rmse`) | RMS error against the truth after (before) filtering | low; `noise_reduction` = raw / filtered |
| `max_rel_rmse` | the worst scored frame | low |
| `flicker` (`raw_flicker`) | mean frame-to-frame change, read through the motion vectors | near 0 on a still view |
| `bias` | mean output / mean truth − 1 | near 0; the luminance weight makes it slightly negative |
| `edge_leak` | the box band's mean output at `box_irradiance` minus the same run at `box_irradiance / 8`, divided by the band's truth luminance | 0: no light crosses the depth edge |
| `shadow_edge_error` | mean absolute error within 2 px of a shadow edge | low; compare `raw_shadow_edge_error` |
| `disoccluded_rel_rmse` | error on pixels with no history this frame | low (spatial filter only) |
| `lag_frames` | frames after the light change until the mean is within 10 % | small |
| `mean_history` | average history length | near `max_history` on a still view |

At the default settings (64×48, one sample per pixel): `static` 1.27 → 0.076 error and 1.0 →
0.012 flicker, bias −1.5 %, leak 0; `pan` 0.14 (worst frame 0.17); disoccluded pixels 0.25;
`light_step` lag 2 frames (9 with `antilag_sigma` 0). The tests in `crates/regia_render/tests/gi_denoise.rs` hold these
with margin.

## `seeing.gi_denoise_check`

```json
{"method": "seeing.gi_denoise_check", "params": {}}
```

runs all four scenarios at the defaults. Parameters, all optional:

- `scenario`: `"static"`, `"pan"`, `"disocclusion"`, `"light_step"`, `"all"` (default), or an
  array of them.
- `settings`: any of `alpha_color`, `alpha_moments`, `max_history`, `depth_tolerance`,
  `normal_tolerance`, `atrous_iterations` (0–6), `sigma_depth`, `depth_epsilon`, `sigma_normal`,
  `sigma_luminance`, `variance_radius` (0–4), `spatial_variance_frames`,
  `history_from_first_pass`, `antilag_sigma` (0 turns anti-lag off), `antilag_block` (2–32).
  Unset fields keep the defaults; the result echoes the settings used.
- `backend`: `"cpu"` (default, the reference) or `"gpu"` (the compute twin on the app's render
  device, so the scores are the target GPU's own; it blocks on one readback per frame, so do not
  run it during a hitch measurement).
- `width` (16–512), `height` (12–512), `frames`, `warmup`, `spp` (1–64), `seed`, `pan_px`,
  `box_speed`, `step_frame`, `box_irradiance`: scenario overrides. `width × height × frames` is
  capped at 1 048 576 (the call runs on the app's thread).
- `blue_noise`: `true` draws the scenario's noise from the [spatiotemporal blue-noise
  table](blue-noise.md) instead of the hash.

The result is `{ backend, settings, scenarios: [...], note }` with one entry per scenario carrying
the scores above.

Try a change before coding it: compare `flicker` and `lag_frames` with
`{"settings": {"alpha_color": 0.1, "alpha_moments": 0.1, "antilag_sigma": 0}}` against
`{"settings": {"antilag_sigma": 0}}` (flicker halves, lag doubles from 9 to 19 frames); with
anti-lag on, both follow the light step in 2 frames. A filter change that raises `edge_leak` above zero or `shadow_edge_error` well above
the defaults is a regression even if `rel_rmse` drops.

### Error codes

In `error.data.error_code` (the JSON-RPC error is `F0002`, invalid parameters):

| Code | When |
|---|---|
| `gi_denoise_unknown_scenario` | `scenario` names no scenario |
| `gi_denoise_unknown_setting` | `settings` has a key the filter does not know |
| `gi_denoise_invalid_setting` | a setting or scenario field is out of range; the message names it |
| `gi_denoise_too_large` | `width × height × frames` above 1 048 576 |
| `gi_denoise_gpu` | `backend: "gpu"` in an app without a render device (JSON-RPC `F0003`), or the GPU run failed |

The Rust API returns the same codes from `GiDenoiseError::code()`, plus
`gi_denoise_size_mismatch` (an input buffer is not `width × height` long),
`gi_denoise_empty_frame` (a zero-sized frame).

## GPU and CPU

`GiDenoiseGpu::denoise` takes the same input as `GiDenoiser::denoise` and returns the same output.
It uploads the frame, runs the passes in one compute pass, and reads the result back (blocking);
the history stays on the GPU. It holds 160 bytes per pixel plus 16 bytes per 2×2 anti-lag
square (`GiDenoiseGpu::gpu_bytes`: input, history, temporal and the two ping-pong buffers, then
one `vec4` for each 2×2 square). A 64×48 frame is 503 808 bytes.
`crates/regia_render/tests/gi_denoise_gpu.rs` runs both on every scenario and requires the GPU
to stay within 0.1 % mean and 2 % per pixel of the CPU (the drivers' `exp` and `pow` differ in
the last bits), with identical history lengths. Those runs turn anti-lag off: it is a threshold
test, and GPU division and square root are not correctly rounded, so a square exactly at the
threshold may decide differently. A separate clean light step checks anti-lag itself.

## Path-traced check

`crates/regia_pt/tests/gi_denoise_reference.rs` feeds sixteen one-sample path-traced frames of
the Cornell box (indirect diffuse divided by the first-hit albedo) through the CPU denoiser and
compares the result with a 512-sample trace. It needs a GPU (the path tracer) and prints the raw,
plain-average and denoised errors, the bias and the flicker.
