# Water bodies

Streams, rivers, lakes and the sea, drawn by the runtime frame (ADR-1399). A water body is an
entity with a `regia_terrain::WaterBody`:

- `WaterShape::Plane { center, half_extent, flow }` is a level rectangle (a lake, a pond, the
  sea). The terrain clips it, so make it larger than the water.
- `WaterShape::Ribbon { points }` follows a centerline, source first. Each `RibbonPoint` has the
  surface position (its `y` is the water surface, so the ribbon slopes where the points fall), the
  width bank to bank, the centerline depth and an optional speed. Without a speed the flow comes
  from Manning's equation: the local slope, a rectangular section of `width × depth`, and
  `manning_n` (0.035 by default).

Add `regia_terrain::WaterPlugin`. It draws every body (up to 16, in entity order) from the first
frame after the render device exists, through the frame renderer's translucent layer, so the
water is in `seeing.screenshot`, `render.out` and any host that renders the frame. Nothing here
needs a `Terrain`: the water refracts and reflects whatever the frame drew (terrain, meshes,
scattered stones).

The shoreline plane of a `Terrain` (`terrain.water`, [terrain water](terrain-water.md)) is still
shaded by the offscreen terrain renderer. `WaterPlugin` also keeps it as a runtime body, unless
`water.set { terrain_planes: false }`. The terrain-water-planes paragraph below says how that
body follows the plane.

## What the water does

- **Flow.** The ripples are advected by the surface velocity with two flow-map layers half a
  cycle apart (1.6 s cycle). The velocity follows the ribbon's tangent and drops to zero at the
  banks. Ripples stretch along fast water into streaks, and faster water is choppier.
- **Determinism.** Everything animated is a pure function of the water clock's tick, which
  `WaterPlugin` advances once per `FixedUpdate`. The same tick renders the same bytes on the same
  GPU and driver, at any frame rate. The seconds are reduced in 64-bit before the shader sees them,
  so a 30-day session animates as smoothly as the first minute.
- **Through the water.** The bed shows through by Beer–Lambert absorption along the refracted
  view path plus the light's way down, with in-scatter from suspended matter. On High and Ultra
  the view ray is refracted by Snell's law to the bed (screen-space refraction), and the bed gets
  caustics (thin bright lines from the ripples, faded with depth and distance).
- **Reflection.** On High and Ultra a screen-space ray march finds what the surface reflects.
  When it misses, the frame's own sky in that direction is used where it is on screen, and an
  analytic sky (from the ambient light) otherwise. The sun glint is GGX, with the ripple detail
  that the mip chain averaged away added to the roughness, so distant water glitters instead of
  aliasing. A short screen-space march toward the sun shades glints in bank and rock shadow.
- **Wet banks.** Ground within `wet_height` above the water and `wet_width` beside a ribbon is
  darkened (`wet_darkening`), with a little sky sheen.
- **Standing waves.** Each `WaterObstacle` (a rock at the waterline) bends the flow around it,
  raises pile-up rings upstream and a V-shaped wake downstream at the wavelength of the waves that
  stand still against the flow, `2π v² / g`, clamped to 0.15..=3 m (the value `water.probe` reports
  as `standing_wavelength_m`; a 6 m/s flow is 3 m, not the unclamped 23 m), with whitewater at its
  upstream face. The nearest 32
  (High) or 64 (Ultra) obstacles to the camera are evaluated.
- **Ice.** An optional `IceShelf` freezes the shallowest water and a band along a ribbon's banks,
  with a ragged edge, where the flow is slow.

## Tiers

The tier follows the render preset unless `water.set { tier }` forces one.

| Tier | Presets | What it draws |
|---|---|---|
| `low` | Steam Deck, Mobile Low/High, Desktop Low | Blended over the frame: absorption from the depth buffer, sky and sun reflection, wet banks, ice, one ripple scale. No scene copy, no rays, no obstacles, no caustics. |
| `high` | Desktop High | Composited over a copy of the frame: refraction, 24-step reflection march with 3 refinements, 10-step sun shadow, 32 obstacles, two ripple scales, caustics. |
| `ultra` | Desktop Ultra | High with a 48-step march, 6 refinements, a 16-step sun shadow and 64 obstacles. |

A frame without an HDR target (MSAA, or no post stack) always blends, in display space.

## Methods

- `water.state` reads everything:
  - `plugin`: whether `WaterSettings` is in the app. When it is absent, `plugin` is `false` and the settings fields (`enabled`, `tier`, `forced_tier`, `debug`, `terrain_planes`, `shore`, `ocean_draw`, `underwater`, `ring_culling`, `ring_snap`, `gpu_timing`) are `null`. `renderer` is a boolean (`false` when the app has no water renderer). `bodies` still lists every `WaterBody` (an empty array when there are none). `clock` is `null` without a `WaterClock`, `sea_wind` is `null` without `SeaWind`, and `last_frame` is `null` when the renderer has not drawn.
  - `enabled`, `tier` (in effect), `forced_tier` (`null`: follows the preset), `debug`.
  - `clock`: `tick`, `step_seconds`, `seconds`, `running`.
  - `renderer`: whether the GPU side exists yet. `last_frame`: what it drew (`bodies`,
    `vertices`, `triangles`, `obstacles` evaluated, `frames_drawn`, `composite`, `hdr`, `tier`,
    `tick`).
  - `bodies`: per body, `entity`, `shape` (a plane's `center`, `half_extent` and `flow`; a
    ribbon's `points`, `length_m`, `drop_m`, `width_m` and `speed_m_s` as [min, max]),
    `obstacles` (count), `ice`, `manning_n`, `look`, `swell` (below; `null` without one) and
    `terrain` (the terrain entity of a shoreline plane's runtime body, else `null`), and
    `shore` (its baked shore field's summary, else `null`; see "Shore").
  - `terrain_planes`: whether each terrain's shoreline plane gets a runtime body; `shore`: whether
    shore fields are used.
  - In `last_frame`, the swell's draw: `swell_bodies`, `swell_waves` (drawn, all bodies),
    `swell_waves_dropped` (the 128-wave frame table was full), `ring_cell_m`, `ring_half_cells`,
    `rings`, `ring_vertices`, `ring_triangles` and `ring_origin`; the shore fields bound:
    `shore_fields`, `shore_fields_dropped`, `shore_texels`.
- `water.probe { at }` takes up to 64 points `[[x, z], ...]`. Each answer names the first body
  (entity order) that holds the point, or `body: null`, and gives `surface_height`, `flow`
  (m/s along x and z), `speed`, `lateral` (meters from a ribbon's centerline, positive to the right
  looking downstream), `half_width` (`null` on a plane), `depth` (the authored centerline depth;
  `null` on a plane), `distance_along`, `froude` (`v / √(g·depth)`) and `standing_wavelength_m`.
- `water.set` takes any of:
  - `enabled` (boolean), `tier` (`auto | low | high | ultra`), `debug` (below);
  - `running` (boolean, holds the clock) and `tick` (sets it);
  - `terrain_planes` (boolean): runtime bodies for the terrains' shoreline planes (default on);
  - `shore` (boolean): use the baked shore fields (default on; see "Shore");
  - `queries_follow_tier` (boolean, default off): water queries read the surface the tier draws
    instead of the shared CPU mirror (single-player only; see [FFT ocean](water-ocean.md));
  - `look` (a patch of the fields below), `ice` (a patch of `width`, `max_depth`, `color`,
    `clarity` over the current or default shelf, or `false` to remove it) and `swell` (a patch of
    `waves`, `crest_foam`, `foam_onset` over the current or default swell, or `false` to remove
    it) for one body: `entity`, or the first body.
  Everything is validated before anything changes; a refusal changes nothing. Returns `before`
  and `after` (both `water.state`).
- `water.floaters { entity? }` lists the floating bodies (see "Floating bodies").

| Look field | Meaning | Clear stream |
|---|---|---|
| `absorption` | Beer–Lambert absorption per channel, 1/m | 0.3, 0.1, 0.09 |
| `scattering` | Scattering by suspended matter, 1/m | 0.05 |
| `scatter_color` | Albedo of that matter (linear) | 0.28, 0.3, 0.24 |
| `roughness` | Surface roughness, 0.02..1 | 0.06 |
| `normal_strength` | Ripple slope scale; 0 is a mirror | 1 |
| `ripple_scale` | Ripple tile length, m | 1.5 |
| `refraction` | Screen-space refraction strength, 0..1 | 0.6 |
| `wet_height`, `wet_width`, `wet_darkening` | Wet band height (m), width beside a ribbon (m), darkening | 0.12, 0.6, 0.45 |
| `foam` | Whitewater at obstacles, 0..4 | 1 |
| `glint` | Sun glint scale | 1 |

`WaterLook::lake()` and `WaterLook::ocean()` are the other presets.

### Debug views

`water.set { debug }`: `off`, `transmitted` (the bed after absorption plus in-scatter),
`reflected` (what the surface reflects), `normal` (`n · 0.5 + 0.5`), `fresnel`, `masks`
(whitewater in red, ice in green). Scalar views are scaled to the horizon sky's brightness so
they survive the exposure.

### Errors

- `F0003`: `water.set` without `WaterPlugin`, with an `entity` that has no `WaterBody`, or with
  a `look`, `ice`, `swell`, `ocean` or `sea` edit and no body in the world; `water.floaters` with an `entity` that has no
  `Floater`.
- Bad values (an unknown tier, debug view or look field, a look out of range, a negative or
  fractional `tick`) are invalid parameters (`F0002`), as are an empty `water.set`, an `entity`
  without a body edit, a bad `at` point, a zero ray `direction` and a `max_distance` outside
  0..=100 000 m.
- `F0001`: internal; the state could not be serialized. Nothing changes.

## Swell, surface and ray queries

A plane body may carry a **swell** (ADR-1620): a list of trochoidal waves, each with a
`wavelength` (m, 0.25..2000), a crest-to-trough `height` (m, 0..40), a travel `direction`
`[x, z]`, a `steepness` (0..1: 0 is a sine, 1 the sharpest crest; default 0.5) and a `phase`
(radians, -10 000..10 000, default 0). Each wave runs at its deep-water speed `√(g λ / 2π)` (a 100 m wave: 12.5 m/s, an 8 s
period). A swell also has `crest_foam` (0..4, default 1) and `foam_onset` (0..1, default 0.15):
foam where the surface compresses toward a crest. The sum over the waves of
`steepness · (2π / wavelength) · height / 2` must stay at or below 1, or crests would fold over
themselves; `water.set` refuses more and changes nothing. A ribbon cannot carry a swell.

`water.set { swell: { waves: [ { wavelength: 40, height: 1.2, direction: [1, 0.3], steepness: 0.7 },
{ wavelength: 13, height: 0.4, direction: [0.2, 1] } ] } }` puts two waves on the first body.
`water.set { swell: { open_sea: { direction: [1, 0.3], height: 2, wavelength: 45, count: 16 } } }`
makes a deterministic spread instead (`count` a whole number 1..32, default 16; `1.9` is refused, ADR-1769; heights scaled to that
root-sum-square `height`; wavelengths 0.55 to 1.6 times `wavelength`, headings within ±35°, a fold
sum of 0.6 where the waves are steep enough); `crest_foam` and `foam_onset` beside `open_sea`
patch over it. From a host, `Swell::open_sea(direction, height, wavelength, count)` does the same.

The surface is a pure function of the water clock's tick, written once in Rust and mirrored in
the shader, so the same tick renders the same image and answers the same queries:

- `water.surface { at: [[x, z], ...] }` (up to 64 points): per point, the highest wet surface
  over it among every body (ties: the lower entity id, as `water.state` lists them; see "Shore" below), its `height`, unit `normal`, `velocity` (the swell's orbital
  motion plus the plane's drift or a ribbon's flow, m/s), `crest_foam` (0..1, before the look's
  `foam` scale), `rest` (the point on the still plane that moved there) and `miss_m` (what the
  fixed eight-step inverse left; well under a millimeter on a valid swell). It always uses every
  wave, whatever the tier draws. A level plane answers its level, a ribbon its sloped surface.
- `water.raycast { origin, direction, max_distance }` (default 1000 m, at most 100 km): the
  nearest crossing with any body: `hit`, `body`, `distance`, `point`, `normal`, `from_below`
  (the ray started under the water), `step_m` (the march step: the ray is clipped to the body's
  box and marched in 128 equal steps, then bisected 24 times, so a crest thinner than one step can
  be missed on a long grazing ray), `velocity` and `crest_foam`. `hit: false` when there is none.

**Drawing.** A swell plane is drawn on a camera-centered ring grid instead of the 8 × 8 plane
grid: an inner square whose cells span about 6 pixels straight below the camera (a power of two in
meters, so it changes only when the camera height doubles or halves), then rings whose cells
double, out to the far side of the body. A narrower field of view gets more cells per ring, up to
the tier's cap (Low 16, High 32, Ultra 48 across the half square), so a scoped view keeps detail
farther out. Each wave is faded out of the vertex displacement where the grid has fewer than four
vertices per wavelength (and fully gone at two); the shading normal keeps it until it is smaller
than a pixel. The tiers draw at most 8 (Low), 16 (High) or 32 (Ultra) waves per body, the first
ones in the list, so list the largest first. Near the camera the drawn vertices lie on the
surface `water.surface` answers within 1 mm.

**Terrain water planes.** Each terrain's shoreline water plane (`terrain.water`, ADR-1237) is
also drawn in the runtime frame: `WaterPlugin` keeps a plane body over the whole map at the
plane's level, tagged with its terrain (`terrain` in `water.state`). Its look comes from the
plane: extinction from `max_depth`, absorption from `shallow_color`, in-scatter from
`deep_color`, the wet band from `foam_width`, ripples from `wave_amplitude` and
`wave_frequency`. Edit it with `water.set { entity, look | swell }` like any body; a change to
the terrain's plane resets its shape and look and keeps its swell. `water.set { terrain_planes:
false }` removes these bodies.

## Shore

A plane body can carry a **shore field** (spec 87, ADR-1828): per texel over the plane, the water
depth and the signed horizontal distance to the shoreline (positive in the water), baked from the
bed under it. Each terrain's water plane gets one automatically (baked when the plane changes, and
again at most every 32 frames after the terrain is deformed); a host bakes one for its own plane
with `ShoreField::bake_terrain(map, level, min, max)` (or `ShoreField::bake` with any bed
function) and inserts `WaterShore { field }` on the body's entity. Ribbons never use one.

With a field:

- **Shoaling and breaking.** Each swell wave grows and shortens as linear wave theory says over the
  local depth (its height times the shoaling coefficient, 1 in deep water), and never stands
  taller than 0.78 of the depth; on land it is gone. The drawn swell and `water.surface` use the
  same depth, so near the camera the vertices still lie on the queried surface within 1 mm.
- **Foam.** `breaking` (0..1) rises where a wave nears its breaking height; `shore_foam` (0..1)
  fills the look's `wet_width` band out from the shoreline. High and Ultra draw both into the
  whitewater; Low binds no field (its swell is unshoaled, as it already draws fewer waves).
- **Dry land.** A point whose bed is above the level is not water: `water.surface` answers no body
  there and `water.raycast` passes through, so a hilltop under a terrain's map-wide plane is dry.
- **Answers.** `water.surface` adds `depth_m`, `shore_m` (distance to the shoreline; a ribbon
  reports its bank distance and authored depth), `breaking` and `shore_foam`. `water.raycast`
  adds `depth_m`, `breaking` and `shore_foam` on the hit, and does not include `shore_m`
  (spec 87 § 6).

`water.state` reports each plane's `shore`: `null` or `{ size, cell_m, level, wet_fraction,
max_depth_m, shoreline_texels }`, its `last_frame` the fields bound (`shore_fields`, at most 4 per
frame; `shore_fields_dropped`, `shore_texels`). `water.set { shore: false }` ignores every field
in the frame and the queries (the bakes are kept); `true` uses them again. A field is up to 1024
texels a side (the cell grows on a larger plane, at least 0.25 m). Its bytes (`RGSH`, version 1)
are a cache: a different version is refused and baked again.

## Floating bodies

Give a dynamic rigid body a `Floater` (`regia_terrain::water_body::float`): spheres in the body's
frame (`Floater::sphere(radius)`, or `Floater::box_points(half_extents, [nx, ny, nz])` to fill a
box with spheres of the same total volume). `box_points` may build more than 64 spheres, up to
4096, so the allocation stays bounded; `Floater::validate` rejects any count outside 1..=64,
and each step applies only the first 64. Set `fluid_density` (1000 kg/m³ fresh, 1025 sea),
`linear_drag` (N·s/m against the water's velocity) and `angular_drag`. Add
`regia_framework::buoyancy::BuoyancyPlugin` beside the physics plugin. Every fixed step, before
the physics step, each applied sphere's part under the water surface pushes up with its displaced
weight at the sphere (a tilted hull rights itself). A sample whose `miss` is NaN, or greater than
half the sphere's radius, contributes nothing that step: the choppy-wave inverse missed the center.
Drag pulls the body toward the water's motion:
the swell's orbital velocity, a plane's drift, a stream's flow. The plugin owns the floater's
`ExternalForce` while it is in the water (put a host force on a jointed body instead) and removes
the force it wrote the step the body leaves the water; a body that stays dry keeps whatever force
the host gave it. Same steps, same bits.

`water.floaters { entity? }` lists them in entity order: `points`, `volume_m3`, `fluid_density`,
the drags and `state` (the last step's `force`, `torque`, `submerged_volume_m3`,
`submerged_fraction`, `wet_points` and the water `tick`; `null` before the first step or without
the plugin). `F0003` when `entity` has no `Floater`.

## Water extras

Spec `docs/specs/100-water-extras.md`, ADR-2342. Five additions, **all off by default** (a project
that sets nothing draws what it drew before); each has a switch in `water.set` and a field in
`water.state`.

**Underwater view.** `water.set { underwater: true }`. When the eye is under a plane body's
surface (the swell included; an ocean plane answers its FFT surface), `last_frame.underwater` is
true and `last_frame.underwater_depth_m` says how far under. The scene is fogged by the water
column (Beer-Lambert along the ray to the scene or to the surface above, plus the column's
scattered light) and the surface is drawn from below: a window shows the sky or the scene above,
refracted, and beyond the critical angle the surface mirrors the water. Caustics and sun shafts are
an opt-in pass (`water.set_caustics`, [terrain water](terrain-water.md)); ribbons are not fogged. Off (the default) draws nothing from below. The top-level `underwater` in
`water.state` is the switch.

**Ring culling.** `water.set { ring_culling: true }`. The swell ring grid is drawn as index ranges;
the ranges outside the camera's frustum are left out. The picture does not change (over an FFT
ocean too: the boxes take the peaks of the tick's ocean query frame, which culling builds once
per tick when it is on). `last_frame.ring_chunks`,
`ring_chunks_culled` and `ring_triangles_drawn` (against `ring_triangles`) show the saving.

**Per-ring snap.** `water.set { ring_snap: true }`. Each ring of the grid moves in steps of its own
size, so the far rings stop shimmering as the camera moves. `last_frame.ring_origin` is still the
grid's center. A grid with fewer than 8 cells across the half of its inner square does not snap
its rings.

**Sea state from the scene wind.** A plane body with a `WeatherSea { fetch, waves }` component gets
its swell from the sea-state table for the wind in `SeaWind`; on a body with an FFT ocean the
component sets the ocean's `wind_speed` (1 m/s steps, 0.5..40) and `wind_direction` (5 degree
steps) instead and leaves its swell alone. With `regia_framework::sea_weather::SeaWeatherPlugin`
added, `SeaWind` follows the scene's mean surface wind (`SceneWind`, written by the weather or held
by `render.set_foliage_wind`-style manual control); `SceneWindMode::Off` leaves it as the host set
it. `water.set { entity?, sea: { fetch?, waves? } }` adds or patches the component (`fetch`
0.1..1, default 1; `waves` 1..32, default 12), `sea: false` removes it; the body then owns its
swell or ocean wind until the component is removed. `water.state` shows `sea_wind` and, per body,
`sea: { drives, fetch, waves, wind_m_s, force, height_m, wavelength_m }` (`drives` is `swell` or
`ocean_wind`; the Beaufort force, the significant height and the peak wavelength for the current
wind). Errors: `invalid_params` for a fetch or wave count out of range or a ribbon body.

**GPU time.** `water.set { gpu_timing: N }` times the water draw on the GPU every N frames
(0 stops); `last_frame.gpu_ms` is the last timing, `gpu_ms_pixels` the target it covered (the
viewport, each side at most 2048), `gpu_ms_samples` how many ran and `gpu_ms_supported` whether
the device can time passes at all (lavapipe cannot: `gpu_ms` stays 0). A timing draws the water
once more into a private target and waits for the GPU, so use it for a measurement, then set it
back to 0; a composite tier is timed as a blend (no scene copy).

Example: with the camera under a sea, turn the extras on, let a few frames run, read the state
and a screenshot, then switch everything back off.

```json
{ "method": "water.set", "params": { "underwater": true, "ring_culling": true, "gpu_timing": 30 } }
{ "method": "play.step", "params": { "frames": 60 } }
{ "method": "water.state", "params": {} }
{ "method": "seeing.screenshot", "params": {} }
{ "method": "water.set", "params": { "underwater": false, "ring_culling": false, "gpu_timing": 0 } }
```

With the eye 1.3 m under a plane at y = 3, `water.state` answers `last_frame.underwater: true`,
`underwater_depth_m` near 1.3 and `ring_chunks_culled` above 0 when part of the grid is behind
the eye; on a card with timestamps `gpu_ms` is above 0 after 30 frames (on lavapipe
`gpu_ms_supported` is false and `gpu_ms` stays 0). A sea that follows the wind:

```json
{ "method": "water.set", "params": { "sea": { "fetch": 1.0, "waves": 12 } } }
{ "method": "water.state", "params": {} }
```

With `SeaWeatherPlugin` added and a 20 m/s scene wind, the body's `sea` reads `force: 8` and
`height_m` near 8.5 (`drives: "ocean_wind"` on a body with an FFT ocean, whose `ocean.wind_speed`
becomes 20).

## Recipes

**Float a crate and check it.** Spawn a dynamic box with `Floater::box_points([0.5, 0.5, 0.5],
[4, 4, 4])` and a mass of 500 kg over a lake, run a few seconds, then `water.floaters`:
`submerged_fraction` settles near 0.5 (half the water's density) and `force` near the weight.

**Check a coast.** With a terrain water plane, `water.state` shows its `shore`; `water.surface`
along a line from the sea up the beach gives `depth_m` falling to 0, `shore_m` falling to 0,
`breaking` rising where the swell nears its breaking height, then no body on the sand.

**Put a sea under a coast and check it.** `water.set { entity, swell: { waves: [...] } }`, then
`water.set { running: false, tick: 600 }`; `water.surface { at }` along the shore gives the
heights; `seeing.screenshot` twice at tick 600 hashes the same; `water.raycast` from the camera
toward a buoy point finds where the eye ray meets the water.

**Check the stream reads as moving.** `water.set { running: false, tick: 600 }`, take
`seeing.screenshot`, then `water.set { tick: 615 }` and take another. The water pixels differ, the
banks do not. Two screenshots at the same tick have the same hash.

**Find why water looks wrong.** Switch `debug` through `transmitted`, `reflected` and `fresnel`.
A milky surface with a clear `transmitted` view is the reflection (or the sky it falls back to);
a green or murky `transmitted` view is the absorption and scattering.

**Place standing waves.** Probe the rock's position with `water.probe`: `standing_wavelength_m`
is the crest spacing the obstacle will raise, and `froude` above 1 marks fast, shallow water.

## Known limits

- Underwater: caustics and shafts are off unless `water.set_caustics` turns them on, ribbons are not fogged, and the fog ends at the body's rectangle.
- Height fog and aerial perspective are not applied to the water's own reflection and glint (what
  it refracts and reflects from the frame is already fogged).
- Effects particles draw over the water, also where they are under it.
- Screen-space reflection and refraction only know what is on screen; off-screen reflections fall
  back to the sky.
- A sharp bend in a ribbon can overlap the inner bank's triangles; keep the point spacing well
  under the radius of the bend.
- Swell: without a shore field the waves are deep-water waves everywhere; with one they shoal and
  cap but do not turn toward the shore (no refraction over the bed) and do not run up the beach.
  The water's GPU time is measured by a second draw into a private target, not inside the
  frame's own pass.
- Floaters do not disturb the water (no wakes or ripples from them), and each sphere is judged by
  the surface over its center.
