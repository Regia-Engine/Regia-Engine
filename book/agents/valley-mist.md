# Valley mist

A mist that lies on the terrain: it follows the ground height, pools in hollows, thins on open
ground and ridges, drifts, and is lit by the sun through the same froxel fog as everything else
(ADR-1394). Any game can use it; it needs a heightfield but not a particular terrain system.

## What it does

The froxel fog (`agents/volumetric-fog.md`) already holds height fog and boxes and spheres of
extra density. The valley mist adds one more medium to the same extinction:

```text
above   = y − ground(x, z)
hollow  = smoothstep(0, hollow_depth_m, concavity(x, z))
pool    = open_ground_fraction + (1 − open_ground_fraction) · hollow
k       = 0.5 + 0.5 · hollow                       (hollows are deeper)
profile = exp(−above / (scale_height_m · k)) · (1 − smoothstep(0.6·top, top, above)),  top = max_height_m · k
σ(p)    = density · pool · profile · noise(p, time_s)
```

`ground` and `concavity` come from a small grid of the terrain (`MistHeightGrid`): the height, and
how far (metres) the ground lies below the mean ground in a window of `concavity_radius_m`. A
bowl has a large concavity, a ridge none, so the mist pools in the bowl without any hand-placed
volumes. `noise` is three octaves of value noise (one on the reduced tier) that scrolls with
`wind_m_per_s · time_s`; `noise_strength` 0 is smooth, 1 leaves gaps.

Light: the sun's in-scatter in a mist froxel is shadowed by the sun's cascade shadow map, so a
ridge between the sun and a valley shades the mist behind it and leaves the sunlit banks
beside it: those are the shafts, and they are longest for a low sun (9 to 12 degrees). On the
full tier a short march toward the sun (`sun_steps`, default 4) also dims the sunlight by the
mist it crosses, so a low sun lights the bank tops and leaves the hollows darker. The fog's
phase function is forward scattering (`anisotropy`), which makes the shafts brightest when you
look toward the sun.

Determinism: the mist is a function of the terrain grid, the sample position and `time_s`. Set
`time_s` from the frame number (`ValleyMist::time_for_frame(frame, fps)`, or `frame` and `fps` in
`render.set_valley_mist`) and the same frame renders the same mist on every run. While the mist
is on, the fog draws without its history blend, so a frame does not depend on the frames before it.

## Tiers

| Profile | Tier | Cost |
|---|---|---|
| `volumetric_fog` on (Desktop High, Ultra) | full: 3 octaves, `sun_steps` march | the froxel pass, 40 × 24 × 16 froxels × 4 taps, plus the full-screen blend |
| `volumetric_fog` off (Low, Deck, Mobile) | reduced: 1 octave, no sun march | the same froxel pass and blend; set `reduced_on_low_tiers` false to leave those tiers without mist |

The frame runs the fog pass for the mist on a reduced tier even though the profile has no
volumetric fog. Measured costs are in ADR-1394 once the owner's cards have run them.

## From a program

```rust,wrap
# use regia::prelude::*;
# let world: &mut World = todo!();
# let terrain: &regia_terrain::Terrain = todo!();
# let frame: u64 = 0;
use std::sync::Arc;
use regia_render::ValleyMist;

let mut mist = ValleyMist::default();
mist.grid = regia_terrain::mist_grid_from_map(&terrain.map, 32.0, 300.0).map(Arc::new);
mist.time_s = ValleyMist::time_for_frame(frame, 60.0);
world.insert_resource(mist);
```

Without a grid the mist lies on flat ground at `flat_ground_y`. Hosts that build their own
renderer call `Renderer::set_valley_mist`.

## Agent surface

- `render.get_valley_mist`: `{ enabled, density, scale_height_m, max_height_m, open_ground_fraction,
  hollow_depth_m, noise_scale_m, noise_strength, wind_m_per_s, time_s, sun_steps, reduced_on_low_tiers,
  flat_ground_y, grid, active, tier_with_volumetric_fog, tier_without_volumetric_fog }`;
  `{ enabled: false }` when the app has no `ValleyMist`.
  The two `tier_*` fields are what-if tiers for the resource as it is (what a profile with and without
  `volumetric_fog` would draw); they do not say what the last frame drew. Compare two captures to see it.
  A headless app's captures draw the mist like a window does (ADR-1453).
- `render.set_valley_mist`: creates the mist from the defaults or changes the given fields and
  answers `{ before, after }`. `enabled: false` removes it. `terrain: true` samples the first
  `Terrain` into the grid (`grid_cell_m`, default 32; `concavity_radius_m`, default 300);
  `terrain: false` drops the grid. `frame` with `fps` (default 60) sets `time_s`.

Errors: `F0002` for a wrong type, a number that is not finite as `f32`, a `wind_m_per_s` that is not two numbers
or an `fps` that is not positive; `F0003` for `terrain: true` when the world has no `Terrain`.

To check it: set the mist with `terrain: true`, put the camera low over a hollow with the sun at
10 degrees (`TimeOfDay` or the directional light), take `seeing.screenshot`, step frames and
take another: the banks move slowly; `render.out` of the same frame twice gives the same hash.
