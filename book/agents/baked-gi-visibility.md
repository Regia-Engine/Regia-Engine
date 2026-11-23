# Baked GI leak control in the lookup

A baked GI volume (`.fgiv`, see [Baked GI volumes](baked-gi.md)) lights a point from the eight
probes at the corners of its cell. Each probe also stores how far it sees in every direction (its
distance moments). The lookup uses them to drop a corner probe the point is hidden from: a probe
on the far side of a wall should not light the point.

The moments are coarse (4 × 4 or 8 × 8 directions per probe). Next to a thin wall, the two moment
taps around the direction to the point can disagree: one sees the wall in front of the point, the
other sees past it. Filtering the moments first (DDGI as published) averages them into one mean
far beyond the point with a large spread, so the point reads as visible and the far room's light
leaks onto the floor at the foot of the wall.

Since ADR-1105 the lookup does two things by default:

- **Per-tap visibility** (`per_tap`). It takes the visibility bound (Chebyshev, on the moments) at
  each of the four taps and then filters the four results, like percentage-closer filtering of a
  shadow map. A tap that sees the wall keeps its share of the probe hidden.
- **Light-bleeding reduction** (`leak_reduction`, default 0.25). The bound's tail from 0 to the
  amount maps to 0, and the rest is stretched back to 0..1. A probe that the moments find only
  partly visible counts for less. A point that every probe sees fully is not affected.

On open ground the new lookup only re-weights corners the moments already find partly hidden. The
repro test (`crates/regia_render/tests/baked_gi_thin_wall.rs`) holds the average change on open
ground under a light gradient below 2 %. A uniform light reads exactly as before, because the
corner weights are normalized.

What it does not fix: an enclosure smaller than one probe cell, which the moments cannot resolve
at all. Give such a room an interior brick ([Baked GI volumes](baked-gi.md), *Interiors*).

## Agent methods

- `render.get_baked_gi_visibility` returns `{ visibility_filter, leak_reduction,
  effective_leak_reduction, default, max_leak_reduction, filters, volume_installed }`.
  `visibility_filter` is `"per_tap"` or `"moments"`.
- `render.set_baked_gi_visibility` changes the settings on the renderer from the next frame.
  `preset: "legacy"` gives the lookup before ADR-1105 (`moments`, reduction 0), for a comparison.
  `preset: "default"` restores the defaults. `visibility_filter` and `leak_reduction` (0 to 0.95)
  set one field on top. It returns `{ before, after, note }`. A host's own
  `FrameRenderer::set_baked_gi_settings` call replaces it.
- `render.sample_baked_gi {points: [{p, n, v}], visibility_filter?, leak_reduction?}` (see
  [Baked GI quality](baked-gi-quality.md)) reads the installed volume's light at points through the
  CPU mirror of the shader's lookup. `visibility_filter` and `leak_reduction` override the
  renderer's settings for that call only, and `lookup` in the reply says which were used: call it
  once as is and once with `{visibility_filter: "moments", leak_reduction: 0}` to compare a point
  with the legacy lookup. Capsule occluders are not applied.

Errors: `invalid_params` (`F0002`) for a bad `preset` or `visibility_filter`, or an out-of-range
`leak_reduction`; `not_found` (`F0003`) when the app has no runtime renderer, or
`render.sample_baked_gi` finds no installed volume.

## Checking a leak

1. Find a bright patch on a dark floor beside a wall (`seeing.screenshot`).
2. Pick its world point with `world.pick` or `camera.raycast`, then call
   `render.sample_baked_gi {points: [{p, n: [0, 1, 0], v}]}` there, then again with
   `visibility_filter: "moments", leak_reduction: 0`, and compare `irradiance_over_pi`.
3. `render.set_baked_gi_visibility {preset: "legacy"}`, take the same screenshot, then
   `{preset: "default"}` and take it again.

## Host API

- `BakedGiSettings::visibility_filter` (`regia_render::baked_gi::GiVisibilityFilter`:
  `PerTap`, the default, or `Moments`) and `BakedGiSettings::leak_reduction` (default
  `DEFAULT_LEAK_REDUCTION`, 0.25; clamped to `MAX_LEAK_REDUCTION`, 0.95; a non-finite value turns it
  off).
- `BakedGiSettings::legacy_visibility()` is the default settings with the lookup before ADR-1105.
- `regia_render::sample_static_gi` follows the same settings, so a gameplay query sees what the
  frame shows.

The GPU cost is three more bound evaluations per corner (the four moment taps were already read).
The `.fgiv` file and the bake are unchanged.
