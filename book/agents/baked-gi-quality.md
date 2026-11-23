# Baked GI quality

The baked GI lookup (`regia_render::baked_gi`, `shaders/baked_gi.wgsl`) has three quality switches on
`BakedGiSettings` (ADR-861). All three are on by default, and each can be turned off on its own to compare:

| Switch | What it does | Where it matters |
|---|---|---|
| `deringing` | Some probes see light from a narrow solid angle: a window, a sunlit patch, an emitter, a sky body. There the L1 irradiance is evaluated as a non-negative lobe with the same mean and first moment over the sphere. The old evaluation was the linear form clamped at 0, which reads light beside and behind the source and adds energy through the clamp. Light as smooth as a hemisphere (the sky, the ground) evaluates exactly as before. | Surfaces beside or facing away from a small bright source. |
| `leak_guard` | Refines the ADR-366 wall split in a lookup cell with wall codes. An edge with a rejected probe at either end (under the floor, inside geometry) joins nothing, and its code does not move the wall's position: that segment crossed the floor or the solid, not the wall. Where every wall along an axis sits on such edges, the wall's position is unknown, and a receiver whose normal points along the axis takes the side its normal faces. | Inner walls and floors of rooms whose probe grid has a layer under the floor. |
| `sun_relight` | The sun-bounce component of a probe lit from below follows the sun's elevation: sun light bounced off level ground scales with `sin h`. The factor is `(max(sin h_now, 0) + 0.02) / (max(sin h_baked, 0) + 0.02)`. A probe takes it in proportion to how much of its L1 points down and how directional it is. Light from the side or above keeps the baked factor. Needs `follow_sun`. | Time of day after the bake, on terrain. |

`sample_static_gi` is the CPU mirror with the sun as baked. `sample_static_gi_relit` takes the frame's
`sun_relight(volume, settings, lights)`, which is what the renderer applies.

## Agent surface

`render.get_gi_mode` returns, under `baked`:

- `lookup: { deringing, leak_guard, sun_relight }`: the switches in use.
- `sun_relight`: the ground factor of the last frame. It is 1 when the sun stands where it was baked, when the
  relight is off, and before the first frame that used the volume. It is below 1 for a lower sun and above 1
  for a higher one.

`render.sample_baked_gi` looks the installed volume up on the CPU at given points: the shader's mirror,
with the renderer's settings, the last frame's light factors and its sun relight. Each of `deringing`,
`leak_guard` and `sun_relight` can be overridden for that call only, so one call with a switch on and one with
it off show what it changes at a point:

```json
{"method": "render.sample_baked_gi", "params": {
  "points": [{"p": [12.0, 0.05, -3.0], "n": [0, 1, 0]}, {"p": [12.4, 1.2, -3.0], "n": [-1, 0, 0]}],
  "leak_guard": false}}
```

Parameters:

- `points`: 1 to 1024 receivers `{ p, n, v }`. `n` defaults to up and `v` (toward the viewer) to `n`; both are
  normalized.
- `deringing`, `leak_guard`, `sun_relight`: optional booleans. They default to the renderer's settings. The sun
  relight comes from the last frame, so `sun_relight: true` has no effect while the renderer has it off or before a
  frame used the volume.
- `visibility_filter` (`"per_tap"` or `"moments"`) and `leak_reduction` (`0..=0.95`): optional overrides of
  `BakedGiSettings::visibility_filter` and `::leak_reduction` for this call (ADR-1105,
  [Baked GI visibility](baked-gi-visibility.md)).
- `zh3`: optional boolean override of `BakedGiSettings::zh3`: the ZH3 (`true`) or L1 (`false`) evaluation of the
  SH (ADR-1377, [GI sampling](gi-sampling.md)).

The result is `{ samples, lookup, scales, sun_relight, from_frame }`:

- `samples`: one per point, `{ irradiance_over_pi, components: { sun_bounce, sky, fill, emissive }, coverage,
  visibility }`.
- `lookup`: the switches this call used.
- `scales` and `sun_relight`: the light factors and the ground factor applied.
- `from_frame`: whether those came from the last frame. When it is `false`, no frame has used the volume yet,
  so the baked light levels (every factor 1) and the baked sun are used.

Errors:

- `F0003` (not found): no runtime renderer, or no volume installed.
- `F0002` (invalid params): bad points, a non-boolean switch, an unknown `visibility_filter` or a
  `leak_reduction` outside `0..=0.95`.

`regia bake-gi --sample` prints the same lookup with the default switches and the baked sun.

To see the effect on a whole frame instead, use the GI validation route ([GI validation](gi-validation.md)):

1. Capture `seeing.compare` against `seeing.reference_trace` with `aov: "indirect_diffuse"`.
2. Have the host call `FrameRenderer::set_baked_gi_settings` with the switch off.
3. Capture again and compare the two results.

The switches themselves are host settings, like the biases: no agent method sets them for the renderer.

## The analytic check

`crates/regia_pt/tests/baked_gi_quality.rs` runs on the CPU only: the CPU probe tracer bakes and the CPU mirror
looks up, so no GPU is needed. It bakes three scenes with exact answers and looks each one up twice, with the
switches off (baseline) and on (quality):

| Scene | Exact answer | Targets |
|---|---|---|
| `open_ground` | An infinite Lambertian ground (ρ = 0.5) under a uniform sky and a sun. A receiver facing `n` (`c = n·up`) reads `L(1+c)/2 + ρL(1−c)/2` of sky and `ρ E sin h/π · (1−c)/2` of sun bounce. Baked at 60°, looked up at 60°, 25° and 80°. | `sun_relight` |
| `emissive_panel` | A 2 × 2 m one-sided emitter over a black floor. Lambert's polygon formula on the panel, clipped to the receiver's hemisphere. | `deringing` |
| `sealed_room` | A closed room with 10 cm walls on the open ground, with probes 0.78 m apart, some under the floor. The inside is black. | `leak_guard` |

Two more rows are measured but not targeted: the sealed room with an INTERIOR brick over its inside, with the
brick's faces on the inner walls or inset 0.12 m from them (see "INTERIOR bricks on the walls" below). They only have
to be no worse with the switches on.

The test prints the table, and writes it to `$REGIA_GI_TABLE/baked_gi_quality.md` when that variable is set. It
fails when any metric of any scene gets worse. The one exception is the panel's energy ratio; the test file explains
why. It also fails when the moved-sun ground error does not drop by at least half (and below 5 %), when the panel's
error does not drop, or when the sealed room leaks more than 5 % of the outdoor light.

```text
cargo test -p regia_pt --test baked_gi_quality -- --nocapture
REGIA_GI_TABLE=/tmp/gi-table cargo test -p regia_pt --test baked_gi_quality -- --nocapture
```

The metrics, per scene:

- **rel RMSE**: RMSE over the mean exact value. For the sealed room it is over the open ground's value.
- **energy**: mean estimate over mean exact value.
- **max error**: the largest error, on the same scale as rel RMSE.
- **light in the dark**: the mean estimate where the exact value is black: behind the panel, or inside the room.

### The prototype measurement

Before the Rust code was written, every change was measured in a numerical prototype of the bake and the lookup.
The prototype is outside the repository: it is single-channel, with its own one-bounce tracer.

| Scene | Metric | Baseline | Quality |
|---|---|---|---|
| open ground, sun 60° → 25° | rel RMSE | 0.295 | 0.006 |
| open ground, sun 60° → 80° | rel RMSE | 0.066 | 0.004 |
| open ground, sun 60° → 25° | energy | 1.189 | 1.004 |
| room with a door, sun moved (8 cases) | rel RMSE | 0.07–0.43 | 0.06–0.22 (better in all 8) |
| emissive panel | rel RMSE | 0.523 | 0.450 |
| emissive panel | light behind the source | 0.030 | 0.024 |
| emissive panel | max error | 4.37 | 3.90 |
| emissive panel | energy | 1.013 | 0.981 (see below) |
| sealed room, 10 cm walls, 0.78 m probes | mean leak | 0.104 | 0.000 |
| sealed room, 10 cm walls, 0.78 m probes | max leak | 1.25 | 0.000 |

Five other room layouts (wall thickness 0.1–0.4 m, probe spacing 0.8–2 m) did not leak at baseline, and they
still do not. The room's outdoor receivers did not
change. The first run of the Rust test replaces this table with the engine's own numbers.

The panel's energy moved from 1.3 % over to 1.9 % under, for this reason:

- De-ringing gives every probe its exact mean over the sphere. The unit test
  `deringing_keeps_the_mean_and_first_moment_and_never_goes_negative` shows this.
- The clamped linear form adds about 9 % at |r1| = 1.8·r0.
- Near a small source the interpolation between probes falls short by about 2 %, and that surplus more than
  made up for it.

## What was measured and not changed

- **Windowing** (scaling the L1 band down, fixed or just enough to keep the linear form non-negative) was worse on
  every panel metric. Example: rel RMSE 0.523 → 0.64–0.81, light behind the source 0.030 → 0.12–0.30. A narrower
  lobe is the fix; a flatter one is not. The non-linear L1 reconstruction from the Frostbite talk (Hazel's) lowered
  the error but doubled the light behind the source, because its constant term never reaches 0.
- **Normal and view bias**: a sweep of (0.1–0.4, 0.3–0.6) on the open ground changed nothing measurable. The
  defaults stay.
- **Interior/exterior blending**: a band over the last cell inside an INTERIOR brick, gated by how much of the
  terrain layer's cell sees the receiver, did not lower the error or the seam at a doorway. The doorway's error comes
  from the coarse outdoor layer, not from the switch. The ADR-281 rule stays: no blend.
- **INTERIOR bricks on the walls**: an INTERIOR brick whose faces lie on the room's inner wall faces puts probes on
  the wall surfaces. In the prototype, the floor next to the walls of the sealed room then read 40 % of the outdoor
  light. With the brick inset by 0.12 m (a sixth of its 0.69 m cell), it read none. This is a placement concern for
  `auto_bricks` (an ADR-861 follow-up), not a lookup one.

## Limits

- The sun relight models ground bounce only. A sunlit wall's bounce keeps the baked factor when the sun's azimuth
  moves. A bake with several sun positions would cover that, and it would change the format.
- De-ringing works per channel on the summed SH, after the corners are blended. A cell whose corners point their
  light in different directions gets a smaller summed L1 and stays linear.
