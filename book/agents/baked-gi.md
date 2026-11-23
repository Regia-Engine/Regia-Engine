# Baked GI volumes

A map can ship its indirect light baked by the path tracer: a `.fgiv` volume made by
`regia bake-gi` (format: `docs/formats/baked-gi.md`). The volume holds probes at fixed
positions. Each probe stores four light components as L1 spherical harmonics: sun bounce, sky,
sky-body fill and emissive. It also stores distance moments for visibility. The probes sit on a
terrain-following layer, with optional fine detail bricks around set pieces.

At runtime the bake replaces the GPU probe update (DDGI). It gives path-traced bounce, sky and
fill light at a fixed cost: one half-resolution lookup pass, about one sample per four pixels. There
is no settling time, so the first frame after a load or a camera cut is final.

## What changes on a baked map

| | Without a bake | With a bake |
|---|---|---|
| Diffuse indirect | DDGI (Depth) or the analytic terms (Balanced) | the bake, where it covers |
| Sky-body fill | live, unshadowed | baked: shadowed, plus its bounce |
| DDGI probe update | on under Depth | off |
| SSGI | per preset | off (the bake already counts the bounce) |
| GTAO | per preset | full resolution drops to half (the bake holds long-range sky visibility) |
| Sun, shadows, paged cache, SSR, local lights | per preset | unchanged |

The bake lights every look except a chosen **Balanced**. That includes Depth, the preset default,
and the Low presets. Outside the volume, the frame keeps its analytic terms: more than one column
spacing past the outermost columns, or more than twice the top level above the ground.

## Host API

- `RuntimeRenderer::set_baked_gi(Some(Arc<BakedGiVolume>))` installs a volume (or
  `FrameRenderer::set_baked_gi`). `None` removes it. Install it once per map, like the GI field.
- `FrameRenderer::set_baked_gi_settings(BakedGiSettings)` sets the tunables: normal and view bias,
  intensity, `sky_scale` and `emissive_scale` (relative to the bake), `follow_sun`, `follow_fill`
  and `replace_live_fill`, the quality switches `deringing`, `leak_guard` and `sun_relight`
  ([Baked GI quality](baked-gi-quality.md)), and the leak control `visibility_filter` and
  `leak_reduction` ([Baked GI leak control in the lookup](baked-gi-visibility.md)).
- `FrameRenderer::set_baked_gi_enabled(Some(false))` keeps the volume installed but unused. Use it
  for comparisons against Depth.
- `FrameRenderer::baked_gi_status()` reports `installed`, `active`, probe count, GPU bytes, an
  error, the per-component light factors and the sun mismatch.
- `regia_render::sample_static_gi(volume, settings, scales, p, n, v)` is the same lookup on the
  CPU: a gameplay query, or a check against the GPU.
- `FrameRenderer::set_gi_occluders(&[GiCapsule])` (or `RuntimeRenderer::set_gi_occluders`) sets
  the capsule occluders of dynamic units. It returns how many it kept. See *Dynamic occluders*
  below.

Each component scales linearly with its light. The sun-bounce term follows the frame's sun, and the
fill term follows the frame's sky-body fill. Light-level changes after the bake still apply.
Changes to the sun's direction do not. A sun more than 3° from the baked one logs a warning once.

## Interiors

A volume in format v2 can flag a detail brick as `INTERIOR`: an enclosed room, a hangar, a tunnel.
The bake sets the flag on bricks whose centre lies inside an interior box. Inside an interior
brick:

- The terrain layer adds nothing, so outdoor sky and bounce light do not leak through the walls.
- The brick does not fade out over its last cell toward the outdoor probes. Its light ends at its
  box, so it does not leak out either.
- It wins over any other brick that holds the point, even a larger outdoor brick around it.
- The lookup tests the point pushed half a cell along its normal. An outer wall (normal pointing
  out of the box) reads the outdoor light. An inner wall reads the interior's.
- The test allows an eighth of a cell outside the probe box (ADR-1364). A floor pixel next to a
  wall, or a surface exactly on the box, can land a few ulps outside it; without this slack it
  read the outdoor light and drew a bright line along every room edge.
- Put the brick's probe lattice on the room's air box: the inner faces of the walls, floor and
  roof. `regia bake-gi --from-scene` does this with each `interiors.json` box. A lattice half a
  cell or more inside the air reads the outdoor light on the room's own surfaces. A box out to the
  wall centres puts probes inside the walls, and the walls come out blotchy.
- If every probe around the point failed the bake, the point reads black at full coverage instead
  of falling back to the outdoor terms.

An interior with an opening (a vehicle bay door) is flagged as a whole in v0.2.0. Light does not
enter through the opening yet. Version 1 files have no flags and light exactly as before.

## Emitters the bake holds

The bake's emissive component holds the light of every emissive surface it traced, seen directly
and bounced. Desktop High and Ultra also light the scene with area lights fitted to emissive
meshes (`emissive_lights`). Without care the same emitter would light a room twice: the Cornell
box in the v0.2.0 look review came out 13 to 57 % too bright on those tiers (ADR-1386).

- A volume in format v3 names the emitters it traced: per placed emissive mesh, its world bounds
  (`BakedGiVolume::emitters`, `BakedEmitter`). `regia bake-gi` writes the list.
- Each frame, the renderer matches the emissive meshes it draws against that list by their world
  bounds (2 cm plus 2 % of the emitter's size). The area lights of a matched emitter are tagged.
- The forward pass takes a tagged light's diffuse back out where the volume covers the pixel. It
  also takes out the light's specular as far as the probes stand in for the rough lobe. Outside the
  volume, and on surfaces the volume does not cover, the area light stays.
- An emitter the bake did not trace keeps its area light everywhere: one added or moved after the
  bake, a unit's lights, anything on the `structure` and `dynamic` layers.
- Over the area light budget, tagged lights go after the others.
- Desktop Low, Deck and Mobile draw no emissive area lights, so nothing changes there.
- `emissive_scale` 0 (the bake's emitter light off) or `intensity` 0 keeps every area light.
- A v1 or v2 volume does not name its emitters. It still loads, but every area light stays, so on
  High and Ultra its emitters are counted twice. The renderer logs a warning once. Rebake the map
  with this build: there is no other migration, because only the bake knows which emitters it
  traced.
- Change an emitter's material color or intensity after the bake and the tag still applies: the
  bake's emitter light follows `emissive_scale` only. Rebake after emitter changes, as after any
  scene change.

## Dynamic occluders

Units are not in the bake. To darken the baked light next to a vehicle or a character, the host
lists a few capsules per unit each frame:

```rust,wrap
# let renderer: &mut regia_render::FrameRenderer = todo!();
use regia_render::baked_gi::GiCapsule;
let hull = GiCapsule::new([10.0, 1.2, 4.0], [14.0, 1.2, 4.0], 1.1); // ends and radius, metres
renderer.set_gi_occluders(&[hull, GiCapsule::sphere([12.0, 2.6, 4.0], 0.8)]);
```

- Each capsule is the segment `a`–`b` swept by `radius`. `strength` (0 to 1, default 1) is how much
  of the blocked light it removes.
- At most `MAX_GI_OCCLUDERS` (64) are used per frame. The rest, and unusable ones (non-finite ends,
  radius or strength ≤ 0), are dropped and counted in `baked_gi_status().occluders_dropped`. There is
  no distance band here: with hundreds of units, list the 64 capsules nearest the eye, and give every
  unit's mesh proxy `OcclusionProxy::far_band` so the units past those still shade the ground under
  them (`render.mesh_occluders` `bands`, ADR-7970).
- The list stays until the next call. A host whose units move sets it every frame. An empty list
  clears it.
- They act only in the baked screen pass, so only where a volume lights the frame. Android has no
  volume, so they cost nothing there. With an empty list nothing is uploaded and the pass does no
  occluder work.
- Fit capsules inside the unit's hull. A capsule then darkens the ground and walls next to the unit
  but not the unit itself, because its surface faces away from the capsule.
- The occlusion is analytic: the light a sphere blocks from the hemisphere above a surface, with
  each capsule treated as its sphere nearest the surface. A long capsule is under-counted toward its
  ends; split a long body into two or three capsules.

## Agent methods

- `render.get_gi_mode` returns `uses_bake` (whether the current look would use an installed
  volume). It also returns `baked`, which is `null` without a runtime renderer, else `{ installed,
  active, probes, gpu_bytes, error, scales, sun_mismatch_deg, occluders, occluder_cap,
  occluders_dropped, interior_bricks, emitters }`. `scales` has one row per component, in the order
  sun bounce, sky, fill, emissive. Each row is per channel, and 1 is the baked level. `occluders` is
  the number of capsules in use, `occluder_cap` is 64, and `interior_bricks` counts the installed
  volume's interior bricks. `brick_grid` is `{ cells, entries }` of the brick lookup grid the screen
  pass walks instead of every brick (ADR-1835): `cells` 0 means no bricks, and `entries` over `cells`
  is how many bricks a texel's cell tests on average (keep it far below `interior_bricks` plus the
  plain brick count; a map where it approaches the brick count has bricks that span the whole level).
  `emitters` is the number of emitters the bake named, or `null` for a v1
  or v2 volume, which names none (its emitters are counted twice on High and Ultra: rebake).
- `render.emissive_lights` returns `baked`: how many of the uploaded area lights belong to emitters
  the volume holds. Those give way to the bake where it covers.
- `render.set_gi_occluders {"capsules": [{"a": [x, y, z], "b": [x, y, z], "radius": r,
  "strength": s}], "gizmos": true}` replaces the capsule occluders. `b` defaults to `a` (a sphere)
  and `strength` to 1. `"gizmos": true` also draws them as yellow wire capsules; a later call without
  it removes the drawing. `[]` clears the list. It returns `{ count, cap, dropped, gizmos,
  volume_installed, note }`. The host's own `set_gi_occluders` calls replace the agent's list, so
  use it on a paused game or one that sets none.
- Errors from `render.set_gi_occluders`: `F0002` (invalid params) for a missing `capsules` list, a
  capsule without `a` or `radius`, a point that is not three numbers, a radius ≤ 0, a strength
  outside (0, 1], or a `gizmos` value that is not a boolean. The message names the capsule index.
  `F0003` (not found) when the app has no runtime renderer.
- `render.set_gi_mode {"mode": "baked"}` forces the look and leaves the persisted
  `graphics.lighting` alone. `{"mode": "balanced"}` shows the analytic terms for a side-by-side
  comparison. `{"mode": "default"}` goes back to the default.

## Checking a bake

1. `render.get_gi_mode`: `baked.active` is `true`, `baked.error` is `null`, and `baked.scales` are
   near 1 at the look the map was baked with.
2. `seeing.screenshot` with `baked`, then with `balanced`. Shaded ground and walls should pick up
   the bounce of sunlit surfaces near them, and enclosed spaces should stay dark.
3. Compare against the path tracer's plates: shade within ±20 % is the target.
4. Emitters: on High or Ultra, `baked.emitters` is a number, not `null`, and
   `render.emissive_lights` reports `baked` > 0 in a room lit by a baked panel. Screenshot the room
   with `render.set_gi_mode {"mode": "baked"}`: it should look like Desktop Low, plus the panel's
   highlights on glossy surfaces.
5. Interiors: `baked.interior_bricks` matches the bake's interior count. Screenshot inside the room
   and just outside its walls. The room should not pick up outdoor sky, and the outer walls should
   not go dark.
6. Occluders: `play.pause`, then `render.set_gi_occluders` with a capsule over a lit patch of ground
   and `"gizmos": true`. Take a screenshot, clear with `[]`, and take another. The ground under the
   capsule should darken softly, darkest right under it.

Evidence in the repo: `crates/regia_pt/tests/baked_gi_runtime.rs` checks the following.

- The GPU lookup matches its CPU mirror.
- Per term, the bake is within the band of the path tracer's own probe at floor receivers.
- DDGI and SSGI stay off on a baked frame.
- A sunlit trench wall lights the shadowed floor.
- A sealed room stays dark (≤ 1 % of the outside ground).
- The `baked_gi_trench` golden.

`crates/regia_render/src/baked_gi.rs` and `baked_gi/occluders.rs` test the near field on the CPU
mirror:

- A v1 file looks up bit for bit as before.
- An interior brick keeps the terrain out and does not blend outward.
- An interior brick wins over an enclosing brick.
- A capsule darkens by its analytic factor, and one behind the surface does nothing.
- An empty occluder list uploads nothing.

`crates/regia_pt/tests/baked_emitters.rs` checks the emitters the bake holds (ADR-1386) on any
GPU, lavapipe included, without a golden:

- In a closed room lit by a ceiling panel, Desktop High and Ultra with a v3 volume light the room
  like the same frame without area lights, plus at most the area lights' specular. The room stays
  within the indirect light a closed room can hold over the direct light alone. The same volume
  read as v2 (no emitter list) is a full direct light brighter: the double count.
- An emissive strip added after the bake adds the same light over the bake as with no volume.
- On an open floor that a brick covers only in part, a baked lamp lights the uncovered floor
  exactly as with no volume, and the covered floor with the bake's light.
- Desktop Low, Deck and Mobile render the same bytes with and without the emitter list.

`emissive_lights.rs` tests the tagging on the CPU: only matched emitters are tagged, not one added
or moved after the bake or a unit's light; a v2 volume tags nothing; tagged lights go last over the
budget. `regia_asset` tests that a v2 file loads with no emitter list and writes back as v3.

## Screen-space AO under the bake

Where the volume lights a pixel, the SSAO counts only the occlusion its probes do not already hold,
and capsule occluders leave the SSAO's share of theirs to it. `render.set_baked_gi_ao` switches this
off for a side-by-side. See [Baked GI and screen-space AO](baked-gi-ao.md).

Maps whose layout needs more than one portable 128 MiB binding bake as split volumes (up to four part files,
`render.get_gi_mode` `baked.volumes`): see [Split baked GI volumes](baked-gi-split.md).
