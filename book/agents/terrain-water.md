# Terrain water

A terrain can carry one shoreline water plane (`Terrain::water`, a `WaterPlane`). It is a flat
surface at a fixed `level` with small ripples, colored by its depth over the terrain. There is
no simulation. ADR-1237 describes the shading model.

**Where it is drawn.** The offscreen terrain renderer draws the plane when it is given one:
`render_offscreen`, `samples/terrain_lab` and its DO27 evidence shots. The look-dev harness
builds its terrain without water. In the runtime frame (`TerrainSharedDraw`), an app with
`WaterPlugin` draws the plane as a water body over the whole map at the plane's level
(ADR-1620): it shows in `water.state` with `terrain` set to this terrain, and
[water](water.md#swell-surface-and-ray-queries) explains its look and how to add a swell to it.
The calls below read and edit the plane itself; the runtime body follows each edit on the next
frame.

## Methods

- `terrain.water { at?, time? }` reads the first terrain's plane.
  - `water`: the plane's fields, or `null` when the terrain has none.
  - `at`: up to 64 points `[[x, z], ...]` in meters. Each gets a `probes` entry:
    - `surface_height`: the rippled surface at `time` seconds (default 0).
    - `ground_height`: the terrain's sim surface (bilinear heights). Holes are not applied.
    - `depth`: `level − ground_height`. Negative on dry land.
    - `wet`: whether the plane is drawn there (`depth >= −0.05`).
    - `slope`: the bed's rise over run.
    - `shore_distance`: horizontal meters to the waterline, `depth / slope`. The slope is at
      least 0.02.
    - `foam`: the foam band's coverage before the noise mottle, 0 to 0.8.
    - `opacity_from_above`: the water's coverage seen straight down, Fresnel not included.
  - `entity`: the terrain read (the first in query order). `terrains`: how many exist.
- `terrain.set_water { water?, enabled? }` edits the first terrain's plane.
  - `water` can hold any subset of the fields below. They apply over the plane in effect, or
    over the defaults when there is none.
  - `enabled: false` removes the plane.
  - The new plane is validated before it replaces the old one. On a refusal nothing changes.
  - Returns `before` and `after`.

| Field | Meaning | Default |
|---|---|---|
| `level` | World Y of the still surface, meters | 4.0 |
| `shallow_color`, `deep_color` | Body color at no depth and from `max_depth` on (linear RGB, 0..1) | teal, dark blue |
| `foam_color` | Foam color (linear RGB, 0..1) | light grey |
| `foam_width` | Width of the foam band, horizontal meters from the waterline | 1.6 |
| `max_depth` | Path length at which the body color reaches `deep_color`, meters. Body opacity is 95 % of `alpha` at half of it | 12.0 |
| `alpha` | Opacity of deep water (0..1) | 0.75 |
| `wave_amplitude` | Ripple amplitude, meters | 0.05 |
| `wave_frequency` | Ripple wave number, radians per meter | 0.9 |

## Errors

- `F0003`: the world has no `Terrain`.
- `F0002`:
  - an unknown field, or a value that fails validation (colors outside 0..1, a negative width,
    `alpha` outside 0..1, a non-finite level);
  - more than 64 points, or a point that is not `[x, z]`;
  - `enabled: false` together with `water`.

## How it looks

- **Reflection.** The surface reflects the sky by Fresnel. Looking straight down, about 2 %
  of what shows is reflected sky. At grazing angles almost all of it is. The reflected sky is
  the frame's uniform sky light, the same light the terrain is lit with. There is no
  screen-space reflection yet.
- **Transmission.** The view ray bends into the water (Snell's law). The bed shows through
  in proportion to the water crossed along that bent ray, so shallow water is clear and deep
  water shows its own color. The offscreen renderer also refracts the picture: the bed shows
  from where the bent ray meets it (screen-space refraction). Where something above the water
  stands in front of that spot, the water shows what is straight behind it instead.
  `TerrainGpu::set_water_refraction(false)` turns the refraction off.
- **Waterline.** The water's opacity fades in from zero over the last 0.2 m of depth, so the
  shoreline shows no hard edge.
- **Foam.** The band is `foam_width` meters wide measured across the beach, whatever the
  beach's slope.
- **Ripples.** The ripple normals shade every pixel. The water grid has 64 cells per side
  over the whole map, so it drops a ripple it cannot resolve: on a large map the drawn surface
  is flat and only the shading ripples.
- **From below.** Past the critical angle (about 48.6° from straight up) the surface is a
  mirror of the water body. Inside that cone it shows the sky.

## Checking it

1. Call `terrain.water { at: [[x, z]] }` at a beach point. `wet` should be true and `depth`
   small, and `shore_distance` should be about `depth / slope`.
2. Raise `max_depth` with `terrain.set_water`, then probe the same point again.
   `opacity_from_above` should drop: the same water is clearer.
3. Edits change the `Terrain` component only. An offscreen `TerrainGpu` keeps the plane it
   was built with until the host passes the new plane to `TerrainGpu::set_water`; the runtime
   water body (with `WaterPlugin`) follows on the next frame.
4. Look at the pictures in `samples/terrain_lab`: compare a frame looking
   straight down with one at a grazing angle. The grazing water should take the sky's color.

## Caustics and sun shafts (`water.set_caustics`, `water.caustics_stats`)

Light focused by the ripples falls on whatever lies below a water surface, and under a surface
the sun makes shafts in the water (ADR-2432). Both are **off by default**: a body has
`look.caustics.enabled = false`, no pass is built and default frames are byte-identical.

The setting is `WaterCaustics` on the water look: `enabled`, `strength` (0..=4, 1 is physical),
`scale` (pattern size against the ripple tile), `speed` (the pattern's change; the first enabled
body drives the frame's tile), `chromatic` (0..=1 colour spread of the focus), `shaft_steps` (0 for
no shafts, else 8..=16) and `tier_cost` (`high` or `ultra`). It is saved with the look and left
out of a save while it is the default; older files load unchanged.

`water.set_caustics { entity?, body?, enabled?, strength?, scale?, speed?, chromatic?,
shaft_steps?, tier_cost? }` patches those fields on one body (`entity`, alias `body`, default the
first). It returns `{ entity, before, caustics, tier, active, compiled_out }`: `active` says
whether the pass draws at the effective tier, `compiled_out` is true on Low, which never draws it.

`water.caustics_stats { depth? }` reports `{ tier, compiled_out, enabled_bodies, active_bodies,
passes: { bodies, tile_size, tile_passes, shaft_passes, shaft_texels, shaft_steps, shafts_drawn },
tile }`. `tile` is the tile read back from the GPU and summarized at `depth` metres (default 1):
`mean_density` stays near 1 (the focusing moves light, it does not add any) and `max_density` is the
focus. It is `null` until a tile pass has run, and the read blocks until the GPU is done.

| Code | Raised by |
|---|---|
| `F0002` | `water.set_caustics` on an unknown or out-of-range field, a `tier_cost` of `low`, or `shaft_steps` outside 0 and 8..=16; `water.caustics_stats` on a `depth` outside 0..=20. |
| `F0003` | `water.set_caustics` without `WaterPlugin` or without the body; `water.caustics_stats` with no water renderer. |

Checking it:

1. `water.set { tier: "high" }`, then `water.set_caustics { enabled: true }` and wait a frame
   (`wait_until` on `water.caustics_stats.passes.tile_passes >= 1`). With the sun above the horizon
   the tile exists: `tile.max_density` above 1.5 at 1 m, `tile.mean_density` near 1.
2. Take a screenshot looking down at a shallow bed, enable, take another: bright lines move over
   the bed and fade with depth (to the plain lighting by about 6 m). Pick a body with
   `normal_strength` 0 and nothing changes: calm water focuses nothing.
3. Dive under (`water.set { underwater: true }`, camera below the surface) with `shaft_steps: 12`:
   `passes.shafts_drawn` is true and bright shafts run toward the surface. `water.set { tier: "low" }`
   makes `compiled_out` true and both effects vanish.

The pattern is sharpest at about a quarter of the tile length below the surface (the tile is
`look.ripple_scale` times `caustics.scale`): a 1.5 m stream tile focuses near 0.4 m, the sea's 18 m tile
near 5 m, where the fade has all but ended it, so use a smaller `scale` for shallow beds under big
ripples.

Limits: the pattern follows the ripple map, not a swell's or ocean's long waves; frames that only
blend (no scene copy) draw no receiver caustics.

### Shafts with occluders, accumulation and the view from above (ADR-3251)

- **Occluders.** With a sun shadow map bound, a pier, a hull or a cave roof cuts the shafts:
  `passes.shadow_bound` is true. The engine frame hands its sun cascade over once its hookup is in
  (the frame patch named in ADR-3251); a host can bind its own map with `WaterRenderer::set_sun_shadow`.
  No map bound draws exactly what it did before.
- **Accumulation.** `water.set_caustics { shaft_accumulate: true }` averages the half-resolution march
  over up to 16 frames (`passes.resolves`, `passes.history_frames`), reprojected as the camera turns and
  restarted on a camera cut (`passes.history_resets`). Off by default; two more half-resolution targets.
- **From above.** `water.set_caustics { shafts_from_above: true }` draws a plane's shafts with the eye
  above its surface, behind the surface only (`passes.from_above`). Off by default.

Checking it: dive under a body with shafts next to a pier (`passes.shadow_bound` true): a dark slab
runs down from the pier's shadow on the surface along the refracted sun; with `shaft_accumulate` on and
the camera still, `passes.history_frames` reaches 16 and the shafts stop crawling; a camera cut (a
teleport the frame renderer detects) brings `history_frames` back to 1. Above the water with `shafts_from_above` on, shafts show
in the water in front of the camera and nowhere above it.
