# Temporal quality (agents)

The temporal upscaler (TAAU, the default `TemporalUpscaler`) blends each frame with a reprojected
history. When the history is trusted where it should not be, the image ghosts: a moving object
leaves a trail of its color on the background it uncovers, or small bright features (stars, a
moon's limb, distant lights) fade while the camera turns. `render.temporal_quality` measures this
on built-in synthetic moving-camera sequences, so an agent can check a change to the resolve, or
try other history-rejection constants, without a GPU. Guide topic:
`agent.guide {"topic": "temporal_quality"}`. Design and measurements: ADR-1151.

The numbers come from a CPU copy of `shaders/taau.wgsl` (`regia_render::taau_quality`): the
same neighborhood clip, cubic history fetch, motion test and weights. A unit test checks that its
constants match the shader source. It is a reference model, not a capture of the GPU: the history
stays in `f32`, and each scene is flat layers that translate on screen. A turning camera moves
distant layers by the same amount, and parallax is a near layer moving faster than the far one.
The GPU resolve has its own tests on the same kind of sequence (`crates/regia_render/src/taau_tests.rs`).

## Method

- `render.temporal_quality { scenario?, render_scale?, frames?, sky_motion?, tuning? }` runs one
  scenario, or all five, and returns `{ model: "cpu_reference", width, height, render_scale,
  frames, warmup, sky_motion, tuning, shipped, scenarios: [{ name, ghost_leak,
  disoccluded_pixels, rms_error, peak_ratio, frames_measured }] }`. Read-only; permission `read`;
  needs no renderer or window.
  - `scenario`: one name from the table below (default: all).
  - `render_scale`: render over output resolution, 0.25 to 1 (default 1). Below 1 the resolve
    upscales, as on the Deck (0.7) and the Desktop Low (0.75) and mobile presets.
  - `frames`: frames per sequence, 2 to 64 (default 24). The first third, at most 8, are warm-up
    and are not measured.
  - `sky_motion`: `false` reproduces the velocity pass before ADR-1151, which wrote zero motion on
    the sky.
  - `tuning`: `{ history_weight, variance_gamma, fractional_penalty, velocity_trust_px,
    velocity_reject_px, wide_motion_test }`, each optional, applied over the shipped values. The
    model only: the shader does not read them. `shipped` in the answer is `true` when the tuning
    is the shipped one.

```json
{"method": "render.temporal_quality", "params": {}}
{"method": "render.temporal_quality", "params": {"scenario": "moving_object", "render_scale": 0.67}}
{"method": "render.temporal_quality", "params": {"tuning": {"variance_gamma": 1.0}}}
```

Errors: `unknown_scenario` (the name is not one of the five; the hint lists them) and the
generic invalid-parameters code `F0002` for a value out of range or of the wrong type.

## Scenarios

| Name | What moves | What it catches |
|---|---|---|
| `parallax_pan` | The camera pans: a textured wall moves 1 px/frame, a mid-gray box in front of it 3 px/frame | Disocclusion trails behind a near object |
| `moving_object` | The box crosses the wall at 3 px/frame under a held camera | The classic moving-object trail |
| `tracked_object` | The camera follows the box (still on screen); the wall moves 3 px/frame | Trails at the edges of a tracked character |
| `bright_object` | As `parallax_pan`, with an HDR box (luminance ~3.6) over a dark wall | Bright trails and over-bright edges |
| `stars_turning` | The camera turns at 3 px/frame over a star sky (depth 1) and a ground band | Sky features fading during a turn |

The box's gray lies inside the wall's color range, so the neighborhood clip alone cannot reject
its trail: the motion test has to.

## Reading the numbers

- `ghost_leak`: the share of the foreground's color (as a fraction of the foreground-to-background
  difference) left on background the foreground uncovered in the last six frames. Its baseline is
  the same sequence resolved without the foreground, and the same statistic on background that was
  never covered is subtracted, so the ordinary softness of the accumulation and of the upscale does
  not count. About 0.02 or less is no visible trail; 0.1 is a trail you can see.
- `disoccluded_pixels`: how many uncovered pixels the leak averaged (0 for `stars_turning`, which
  has no foreground).
- `rms_error`: RMS luminance error against an unjittered 4 × 4 supersampled ground truth.
- `peak_ratio`: output over ground truth on the brightest pixels (ground-truth luminance at least
  half the frame's peak, away from the left and right 8 px, where new content enters). 1 is exact.
  A still star field reads about 0.5: the resolve's inverse-luminance blend (the firefly guard,
  Karis 2014) dims sub-pixel HDR points even at rest. Under motion the number should stay at the
  still value.

Measured on the shipped resolve (numpy port of the model, 96 × 48, 24 frames; the Rust unit tests
assert bounds around these):

| Scenario | `ghost_leak` at 1.0 | at 0.67 | `peak_ratio` |
|---|---|---|---|
| `parallax_pan` | 0.017 | 0.013 (0.104 before ADR-1151) | 1.00 |
| `moving_object` | 0.008 | 0.017 (0.151 before) | 1.00 |
| `tracked_object` | 0.013 | 0.015 (0.071 before) | 1.00 |
| `bright_object` | 0.000 | 0.000 | 1.00 |
| `stars_turning` | — | — | 0.51 (0.06 with `sky_motion: false`; a still sky reads 0.50) |

## Typical loop

1. `render.temporal_quality {}` on a branch that touches `taau.wgsl` or `velocity.wgsl`, and
   compare with the numbers above. Then the same with `{"render_scale": 0.67}`.
2. To explore a change to the history rejection, pass `tuning` and compare `ghost_leak` (trails),
   `rms_error` (overall accuracy, which drops when the history is rejected too often) and
   `peak_ratio`. Only a constant that improves the model and the GPU tests belongs in the shader.
3. On the live game, confirm with `seeing.screenshot` while the camera moves (TAA history is
   per view; `render.camera_cut` drops it).
