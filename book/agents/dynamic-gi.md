# Dynamic GI tiers

The dynamic GI is the GPU irradiance probe field behind the Depth lighting option
([Lighting option](gi-probes.md)): camera-relative cascades of probes that trace the terrain
heightfield and per-mesh distance fields every frame, with per-probe distance moments
(Chebyshev visibility) so light does not pass through closed walls. No bake is needed; the sun,
the sky and emissive meshes can change at any time.

A **tier** decides how large and how fast the field is when it runs (`specs/79`, ADR-1679).
Whether it runs at all is still the lighting option.

| Tier | Presets | Probes | Settled re-trace | Rays a frame | Probe buffers | Brick pool | Reach |
|---|---|---|---|---|---|---|---|
| `low` | Deck, Desktop Low | 4 cascades of 12 × 8 × 12 (4 608) | every 16 frames | 8 640 | 14.4 MB | 16 MB | 324 m |
| `medium` | none | 4 × 16 × 8 × 16 (8 192) | every 8 frames | 30 720 | 25.7 MB | 24 MB | 432 m |
| `high` | Desktop High | 4 × 20 × 12 × 20 (19 200) | every 8 frames | 72 000 | 60.1 MB | 32 MB | 549 m |
| `ultra` | Desktop Ultra | as high, 128 mesh bricks per cascade | every 8 frames | 72 000 | 60.1 MB | 64 MB | 549 m |

The settled re-trace is the finest cascade's: each coarser cascade re-traces half as often (up to eight
times less), and "Rays a frame" is that average with every probe settled and active.

`low` and `medium` keep 2 m between the finest probes (each coarser cascade three times wider);
`high` and `ultra` keep 1 m (each coarser cascade 3.8 times wider: 1, 3.8, 14.4 and 54.9 m), so a 3 to
6 m room holds finest probes (ADR-5001). Reach is the half width of the coarsest cascade: beyond it the
probes add no light.

A field whose traced scene stays unchanged stops updating once each finest probe has taken 512 updates
at the tier's re-trace rate (4096 frames on `high`, 8192 on `low`; at least 600). `settled_frames` in
`render.get_dynamic_gi` counts toward it. Before ADR-5001 the update stopped after 600 frames and a closed
room could keep a little of its start-up light.

## Closed rooms

A sealed room under the sun stays dark: under 0.5 % of the sunlit ground on the floor, walls and
ceiling of a 3 m room, with walls down to 10 cm (`specs/79` § 9.1, ADR-1949). Two rules keep it that
way, on every tier:

- The finest cascade holds full weight around the eye: 3 m below it and 4 m above it on `low` and
  `medium`, 3.5 m below and 4 m above on `high` and `ultra` (twelve 1 m layers, ADR-5290; half a cell
  more at each end, and a metre more headroom on `low` and `medium`, since ADR-5909). `finest_above_m`
  and `finest_below_m` in `render.get_dynamic_gi` report this.
- A probe's ray reads only its own cascade or finer where those hold the hit.

A surface beyond the span reads a coarser cascade. Without help the top of a taller room catches light
through a thin roof: a sealed 6 m room read 7 to 11 % on its ceiling and 5.5 to 7.7 % on its upper
walls. The `low` and `medium` tiers fix that with the **visibility blend** (ADR-2605): where a coarser
cascade's probes see a point less than three quarters as well as the finest cascade's (they stand outside
the room), the point keeps the finest cascade's light, and where the finest cascade sees the point a coarser
one may add at most 1.25 times its light. A sealed 6 m room then reads about 0.1 % on its ceiling on `low`. `high` and `ultra` turn it on too since ADR-5001
(with the probe seeding and running-average weighting); a host turns it off with
`DdgiSettings::visibility_blend`. The blend only reaches as high as the finest cascade's window: with ten
1 m layers (2 m over the eye) a 6 m room's ceiling on `high` read 2.2 %; with twelve (3.5 m over, ADR-5290)
it reads under 0.001 %. A host that builds its own layout sets `DdgiSettings::finest_headroom_m` (default
2 m) to give the window's spare height to the ceiling instead of the ground under the eye.

## Flat meshes

A mesh without thickness (a ground quad, a foliage card, a single-sided panel) is traced by the probes like
any other mesh since ADR-5716: rays from its front stop on it and take its bounce, rays from behind it meet a
back face. Before that its distance brick held non-finite values on the front side and the probes did not see
it from there (on a software rasterizer not at all, on an NVIDIA card only along steep rays), so a scene lit
through probes looked as if the floor quad were absent. If a ground under probe GI reads the sky's light from
below, check that the ground is in the frame's mesh list or an instance set and not opted out of the probes
(`renderer.pending_bakes` in `render.get_dynamic_gi` is 0 once its brick is baked).

A swaying plant (`material.set_sway`, `FoliageSway::adopt`) reflects the mean of its base color map for the
probes whether it is placed in the frame's mesh list or drawn as an instance set (ADR-5716). Before that a placed
swaying plant reflected its base color factor alone, usually white, and the ground around it took more bounce
light than around the same plant in a set.

## Frame edges and creases

The frame reads the probes through a half-resolution screen lookup, and each lookup texel takes a surface normal
from the depth buffer around its pixel. Since ADR-5796 a texel at the frame's first row or column takes no
neighbor from outside the frame, and a texel at a crease does not mix the two surfaces' neighbors. Before that
a capture from an NVIDIA card could show single bright texels along the frame's top and left edge (a software
rasterizer did not), and a capture on the `low` and `medium` tiers single bright or dark texels along the creases
near the camera. If you compare frames across that change, expect those texels to differ and nothing else.

## Openings

A wall that stands between two probe planes takes its light from the nearest probe's estimate of the light at the
wall. Since ADR-5909 that estimate weighs light that came through an opening (a doorway, a window, a tunnel's
mouth, the gap over a trench's rim) from the opening itself, so the wall gets the share of it that the wall sees
and not the share the probe saw. Before that a room lit through a door read too bright on `low` and `medium`
(1.13 times a path-traced reference where the room's walls stood a metre from the probe planes), and a tunnel's
and a shaft's walls a few percent too dark on every tier. If you compare captures across that change, expect
interiors lit through one opening to get darker on the 2 m tiers and tunnels to get a little brighter. Nothing in
the protocol changed: no setting, no new field.

The same change widens the finest cascade's window by half a cell at each end (its blend margin is a quarter
cell where it was three quarters) and places `low` and `medium` a metre higher: a ceiling or a shaft wall 4 m
over the camera on `low` now reads the 2 m probes where it read the 6 m cascade from 2 m up. `finest_above_m` and
`finest_below_m` in `render.get_dynamic_gi` report the window (4 m over and 3 m under the eye on `low` and
`medium`, 4 m and 3.5 m on `high` and `ultra`).

## After a fast camera stops

New probes start over when the volumes scroll. On `low` and `medium` a probe that starts over is
**seeded** (ADR-2605, `DdgiSettings::seed_updates`, 4 on those tiers): from the probe the scroll
stepped past, or, when it leaves geometry, from the next coarser cascade. Its first updates average
with that seed instead of one frame of its own rays, so the light is back soon after the camera stops.
`seed_updates` in each tier's entry of `render.get_dynamic_gi` says which tiers do it.

Your own `DdgiSettings` get the same rules when the coarsest cascade is wider than 3 m. A layout of
1 and 3 m cascades does not leak, so it keeps the older placement and ray read. `room_rule` in the
budget (`renderer.budget` for your layout) says which applies.

## Settings

- `RenderProfile::gi_tier` is the preset's tier (the table above).
- `RenderSettings::dynamic_gi_tier: Option<DynamicGiTier>` is the player's choice; `None` keeps
  the preset's. `RenderSettings::effective_dynamic_gi_tier()` resolves it.
- `GameSettings::graphics.gi_quality: Option<GiQuality>` (`Low`, `Medium`, `High`, `Ultra`) is
  the value a game's options menu binds. It persists with the other graphics settings (an older
  settings file without it loads as `None`) and is applied live to
  `RenderSettings::dynamic_gi_tier`; while it stays `None` a host's own value is left alone.
- A host without the runtime calls `FrameRenderer::set_dynamic_gi_tier(tier)`. It writes the
  tier's sizes into the probe settings and leaves biases, hysteresis and emitters alone; calling it
  again with the same tier does nothing, so `set_ddgi_settings` after it is kept.
- `FrameRenderer::dynamic_gi_status()` returns the tier, whether the probes ran, the budget, the
  settling and the last measured GPU time.
- `DdgiSettings::normal_bias` and `view_bias` move the point the probes' visibility test looks at
  (0.1 and 0.3 probe spacings off the surface). They do not move where the probes are weighted
  (ADR-5374; on every layout since ADR-5506, with or without `visibility_blend`): a surface
  takes its light at its own position, so raising them does not push light off a surface and
  the indirect light does not follow the view direction; the irradiance texels next to a
  coordinate axis hold that axis's exact lobe.
- A probe also holds the light at the surface behind it (ADR-5457; every layout since ADR-5506): per
  axis, when a flat surface lies between 0.15 and 1.05 spacings behind the probe, the axis texels
  of odd parity hold the irradiance at that surface, and a wall between two probe planes reads
  it, not the light half a cell in front of it. `DdgiReadback::irradiance(slot, x, y)` returns
  the probe's own lobe at the even axis texels ((2, 2) is +z) and the surface's at the odd ones.
  Nothing to set: no new option, no extra memory.
- Probe noise (every layout, ADR-5457): a settled probe's shading copy lags its history by one
  update. A held view changes about a third less from frame to frame; a light change reaches
  the frame one probe update later.
- Since ADR-5506 (nothing to set): a new probe traces every frame for its first sixteen updates
  (eight before), so `render.get_dynamic_gi`'s timing reads higher for longer after a scroll or
  a tier change; the light between two probes is blended as its cube root, which reads less
  light between a bright probe and a dark one (the first metre inside a doorway or a tunnel's
  mouth); a probe ray's hit reflects as the material's dielectric does in the frame (a little
  less than its albedo head-on, more at a grazing angle), from the material's `roughness` and
  `metallic` factors (roughness and metallic maps are not read by the probes); and a probe ray
  hits a mesh where it crosses its surface, so a door, a window or a tunnel mouth lets in the
  light of its full width (it read half a brick voxel narrower on each side).
- Since ADR-5552 (nothing to set, no extra memory): a probe also holds the light in the creases around it.
  For each face and each side across it, a border texel of its irradiance tile holds the irradiance on the
  face's surface 0.08 spacings from the surface beside it, and a lookup scales the face's light toward that
  crease. A wall's foot over a floor in shadow reads darker, a wall beside a sunlit strip brighter, where
  both used to read the light a quarter spacing (or half a cell) away. The frame takes every crease; the
  probes' own bounce takes a crease half a cell away only where it adds light. `DdgiReadback::crease_irradiance(slot,
  face, side)` returns a crease texel (the face's own light where the probe has no crease there). The probe
  update costs about an eighth more (`render.get_dynamic_gi`'s timing): 0.55 ms at 1440p on an RTX 5080.

## Agent methods

### render.get_dynamic_gi

```json
{ "tier": "low", "setting": "default", "preset_tier": "low", "lighting": "balanced",
  "probes": false, "available": true,
  "tiers": [{ "name": "low", "probes": 4608, "rays_per_frame": 8640, "probe_bytes": 14432256,
              "sdf_pool_bytes": 16777216, "reach_m": 324.0, "finest_above_m": 4.0,
              "finest_below_m": 3.0, "room_rule": true, "screen_bytes": 16588800,
              "seed_updates": 4, "visibility_blend": true, "screen_max": [960, 540],
              "recent_weight": 3, "front_weights": true, "edge_visibility": true,
              "young_budget": 0, "young_settle_frames": 16 }, "..."],
  "renderer": { "tier": "low", "custom": false, "active": false, "enabled_override": null,
                "budget": { "probes": 4608, "...": "..." }, "settled_frames": null,
                "pending_bakes": 0, "emitters": 0, "young_budget": 0, "gpu_ms": 0.0,
                "gpu_stages_ms": { "trace": 0.0, "relocate": 0.0, "irradiance": 0.0,
                                   "distance": 0.0, "lookup": 0.0 },
                "pyramid": { "full_texels": 0, "full_rebuilds": 0, "regional_rebuilds": 0,
                             "texels_reduced": 0, "last_texels": 0 } },
  "persisted": "default" }
```

- `tier` is the tier in effect, `setting` the player's (`"default"` when none), `preset_tier`
  the profile's.
- `lighting` and `probes`: the lighting option in effect and whether it runs the probes.
  `available` is false on Android, where the probes never run.
- `renderer` is null without a runtime renderer. `custom` is true when the probe settings no
  longer match the tier (a host set its own sizes). `settled_frames` is null while the probes are
  off. `gpu_ms` and `gpu_stages_ms` are from the last timed frame: turn GPU timing on first, or
  they stay 0.
- `renderer.pyramid` counts the rebuilds of the terrain heightfield's max pyramid the probe rays
  march (ADR-7659). `full_texels` is the whole pyramid (every level) for the current map. A runtime
  height edit (`terrain.patch_heights`, DO27 contract § 7.4) re-reduces only the texels its changed
  samples reach (`regional_rebuilds`); the first build, a new map, holes, a host's
  `update_heights` or a frame the probes skipped rebuild everything (`full_rebuilds`).
  `texels_reduced` sums both kinds, `last_texels` is the last rebuild's. A crater costs well under
  1 % of `full_texels`. The counters only move on frames the probes update.
- `persisted` is `GameSettings.graphics.gi_quality` (`"default"` when unset), null without
  `GameSettings`.
- Per tier (ADR-2605): `screen_bytes` and `screen_max` are the screen lookup region (66.4 MB on `high`
  and `ultra`, 16.6 MB on `low`, 29.5 MB on `medium`; a frame too large for it at half resolution takes
  a quarter-resolution lookup), `seed_updates` the seed weight of a probe that starts over (0 = no
  seeding), `visibility_blend` whether a point stays on the finest cascade where a coarser one cannot
  see it, `recent_weight` how a young probe's running average weighs its later updates (3 on
  every tier since ADR-5001, so the light settles sooner after a fast camera stops; 1 means equal weights).
- Per tier (ADR-5566, ADR-5716): `front_weights` is true on every tier. The lookup weighs a probe by which
  side of the surface a point lies on it stands: a probe behind that surface's plane counts a thousandth (the
  probe outside a door no longer lights the door wall's inner face through the door; the probe just inside a
  room's open front no longer lights the ends of the room's walls). `edge_visibility` is true on `low` and
  `medium`, whose 2 m probes stand on both sides of a house wall: a probe past the surface's edge, with open
  air where the surface would lie behind it (the probe outside the door against the room's ceiling), counts
  only where it sees the point without doubt. If a room reads brighter near a door or window than you expect,
  read these fields first: `false` where the tier sets `true` means a host replaced the tier's settings
  (`renderer.custom` is then true). A host layout sets `DdgiSettings::front_weights` and
  `DdgiSettings::edge_visibility` itself; they cost no ray, probe or byte.

### render.set_dynamic_gi

```json
{ "tier": "low", "enabled": true }
```

- `tier`: `"low"`, `"medium"`, `"high"`, `"ultra"` or `"default"` (the preset's tier).
- `enabled`: `true` runs the probes (the `depth` lighting option), `false` the `balanced`
  option, `null` the preset's choice. It goes through `render.set_gi_mode`, so it also writes the
  persisted lighting option.
- `young_budget`: the most young probes the runtime renderer's probe update traces in one frame
  (`0` or `null` = off, the default on every tier; see [Young-probe budget](#young-probe-budget)). It is a
  setting of the renderer's probes, not of the tier: a tier change keeps it, and it does not make
  `renderer.custom` true.
- With `GameSettings`, `tier` is also written to `graphics.gi_quality` (`"default"` clears it).
- At least one of the three. Returns `{ before, after, note }`.

The change applies from the next frame. A tier with a different probe count recreates the field,
which settles again in about 8 frames (about 30 after a camera cut). Before a capture or a
comparison, `wait_until` `renderer.settled_frames` is above 8.

### Error codes

| Code | When |
|---|---|
| `F0002` | `tier` is not a string, `enabled` is not a boolean or null, or neither was given. |
| `F0003` | The app has no `RenderSettings` (no runtime renderer). |
| `unknown_gi_tier` | `tier` names no tier. |
| `dynamic_gi_unavailable` | `enabled: true` on a platform where the probes do not run (Android). |
| `dynamic_gi_no_renderer` | `young_budget` on an app without a runtime renderer (nothing was changed). |

## Young-probe budget

A new probe traces every frame for its first sixteen updates. A start-up, a light cut (the sun turns
more than about 3°, or the sun's or the sky's light jumps by a quarter) or a far camera jump makes every
probe new at once, so those sixteen frames trace the whole field. On a large field that is a visible
hitch: High's 1 m grid on the Steam Deck spent about 62 ms a frame in the probe update for sixteen
frames, where Low's 2 m grid spends 23 ms (ADR-5938).

`young_budget` caps how many of those young probes one frame traces (ADR-7870). Past the cap a young
probe waits for a later frame, in this order:

1. nearest to the eye first (in rings one finest probe spacing wide),
2. then probes in view before probes behind the camera,
3. then the one that has waited longest.

Settled probes keep their own re-trace cadence and do not count against the cap. The choice is
exact and the same on every run: the GPU finds the cap-th probe in that order with four counting passes.

What it costs is time to settle. With `young_budget` B, a field of P probes that restarted has taken
every probe's sixteen updates within `young_settle_frames` = 16 · P / B + 16 frames (16 without a
budget). Until then the far probes keep their old light (after a light cut) or are not read yet (after
a start-up or a jump), and the update freeze waits for the restart before it stops the probes. Both
numbers are in each `budget` of `render.get_dynamic_gi`:

```json
{ "renderer": { "young_budget": 3072,
                "budget": { "probes": 19200, "young_budget": 3072, "young_settle_frames": 116, "...": "..." } } }
```

Every tier ships with the budget off (`0`). Set it with `render.set_dynamic_gi { "young_budget": 3072 }`
or `DdgiSettings::young_budget`; clear it with `0` or `null`. To compare captures, wait until
`renderer.settled_frames` is above `young_settle_frames` after a cut.

## Measuring the cost

`cargo test -p regia_render --test dynamic_gi -- --ignored --nocapture tier_cost` renders a sunlit
room at each tier, settled, at the tier's real re-trace rate, and prints the probe update's GPU
time per stage (on a device with timestamp queries) and the frame's CPU time. On lavapipe it is a
correctness run; budgets come from a real GPU.

## A 0.1 m wall on a 0.4 m grid

`render.get_dynamic_gi` reports `normal_bias` in units of probe spacing for the selected tier,
for each tier, and on `renderer` for its live settings. The existing default is 0.1; the existing
minimum front-face clearance is 0.12 spacings. Neither default changes (ADR-3947). The `before`
and `after` states returned by `render.set_dynamic_gi` include the same reporting fields.

`thin_wall_leak_fraction` is a reference measurement from the lavapipe fixture
`a_tenth_meter_shared_wall_stays_under_the_leak_bound`, not live-scene telemetry. It divides the
closed dark room's floor luma by the neighboring lit room's floor luma. The rooms share a 0.1 m
wall, with 24 × 16 × 16 probes on a 0.4 m grid. It checks the wall on a probe plane and halfway
between planes, with 36 frames for each lit reference and 64 for each dark view. The reported
reference is the largest fraction across those alignments. Spec 27 § 2.2 requires at most 0.005
(0.5%). Run:

```sh
cargo test -p regia_render --test ddgi_thin_wall -- --nocapture --test-threads=1
```

The fixture first verifies a probes-off, zero-ambient direct-light control, then converges the
default probe field for fixed frame counts. Its volume covers the room surfaces, and it disables
static screen-result reuse so the scored image includes the convergence updates. The recorded reference
fraction is 0.0 (zero measured dark-floor pixels). No new DDGI setting is required.

Shrinking probe spacing also shrinks the field's coverage. Keep the ceiling and other bounce
surfaces inside it: a probe update hit outside the field can read the open-sky fallback, which
can light a closed room even when the ray hit its wall correctly.

## Screen-space gather step

The screen-space bounce (not the probe field) gathers at a texel step of 1 (native), 2 (half
resolution, Desktop High at the 0.67 budget) or 4 (quarter). Step 4 is off unless a host turns
it on (`ScreenSpaceGiSettings::quarter_gather`, ADR-3964). It is not a preset field. The gather
at step 2 or 4 is the bounce alone, lifted by the same joint bilateral upsample.

### render.get_ssgi_gather

```json
{ "quarter": false, "step": null }
```

- `quarter`: the override. `false` by default.
- `step`: `4` while the override is on, `null` while the preset (or `FrameRenderer::set_ssgi_half_res`)
  chooses 1 or 2.

### render.set_ssgi_gather

```json
{ "step": 2 }
```

- `step`: `4` gathers at quarter resolution. `1` and `2` both clear the override; they do not
  force native or half. Any other integer is refused.
- Returns `{ before, after }`, each `{ quarter, step }`.
- Applies on the next gather. An unfrozen held view that continues compositing a half-resolution
  gather refreshes it when the step changes. A view reusing a native composite, or a converged
  temporal view showing frozen output, waits for movement or `FrameRenderer::camera_cut`.
  The separate integration cache-key patch removes this caveat; it is not applied in this lane.

The default stays off. `render.set_ssgi_gather { "step": 4 }` is how a host turns quarter on
after reading the lavapipe scores (`crates/regia_pt/tests/ssgi_quarter_gather.rs`).

### Error codes

| Code | When |
|---|---|
| `F0002` | `step` is missing, null, or not a non-negative integer. |
| `F0003` | The app has no `RenderSettings` (no runtime renderer). |
| `ssgi_gather_bad_step` | `step` is a non-negative integer other than 1, 2 or 4. |
