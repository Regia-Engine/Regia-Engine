# DO27 interface contracts (v0.2.0)

This page names the data and the code that Direct Order 27 builds against for the v0.2.0 tunnels
bundle: what DO27's bake writes, what Regia Engine loads and renders, and what each side may rely on.
The normative text is `docs/specs/25-sim-bridge-and-do27.md` § 7; this page adds the worked examples. For field-by-field host changes (struct literals, new fields) read the
[DO27 host checklist](do27-host-checklist.md).

DO27 owns the simulation, collision, navigation, its game messages and state on the wire (its `NetGame` types and
what it hands `SnapshotEncoder`), host migration and the bake that writes the files below. Regia Engine owns
loading, rendering, lighting, audio, picking and the network transport and session: DO27 builds its multiplayer on
`regia_net` (owner decision 2026-11-15, ADR-7906; see the [DO27 host checklist](do27-host-checklist.md#regia_net-multiplayer-for-do27)
and [Networking](networking.md)). The engine never writes back into the simulation.

## How to read the status

Every interface carries one of three labels. Names below were checked against the code at v0.5.0 (the release line
DO27's next alpha builds on, commit `59ff4b61d`) on 2026-11-15, name by name, not against the plan.

| Label | Meaning |
|---|---|
| **main** | In the engine code DO27 builds against (v0.5.x). |
| **lane** | Built and tested on a lane branch, not yet merged. None are left; the lane names below are history. |
| **proposed** | Design only. No code exists. Names and numbers may change until DO27 signs off. |

| # | Interface | Status | Lane branch |
|---|---|---|---|
| 1 | `<map>.holes.ron` (a serialized `HolesMask`) | **main**: the type, `set_holes`, `cut_heights`, the file (version 2, named holes), the loader and the hole-aware ray | `v020i-terrain-holes` (merged) |
| 2 | `MapLayer(u8)`, `world_pick` | **main** | `v020i-worldpick-layers` (merged) |
| 3 | `TerrainHeightEdits` (sample rectangles) | **as built, stages 1 and 2** (names proposed until DO27 agrees); probes still rebuild whole | `sprint-w9-do27-height-edits` (ADR-7585) |
| 4 | `ExposureZone` | **main**: the math and the frame hook (`ExposureZonesPlugin`, in `DefaultPlugins`, ADR-322). `PostProcessVolume` does not exist; `PostVolume` eye ranges join the same combine | `v020m-exposure-zones` (merged) |
| 5 | `AudioReverbZone` presets, `distance_scale`, occlusion | **main** | `v020i-audio-zones` (merged) |
| 6 | `VisCell` and portals | **lane** (stage 1, v0.6 line): `VisCell`, `<map>.viscells.ron` v1, the per-view portal walk, the colour-pass cull, the debug draw; stage 2: inherited cells, local lights and glass culled, `audio_zone_mouths`; shadow views not culled | `sprint-w9-do27-viscells`, `sprint-w10-do27-viscells-stage2` |
| 7 | `Steam::client()` | **main**; networking over it is `regia_net` with `regia_steam::SteamSocket` | `v020j-steam-core` (merged) |
| 8 | Object keys | **main** (a rule about an existing field) | none |
| 9 | `regia_voxel_core` | **proposed** (no crate; `.fvox` is now taken by the voxel far proxy) | none yet |

Where the plan and the code differ, the code wins and the difference is noted in the section.

## Rules for every file format

- A file carries `version: N` (an integer). A file without it is version 1.
- A file with a version **newer** than the engine's is refused with an error that says so. The
  engine never guesses at a newer body.
- Adding an optional field with a default does not change the version. Anything else does: renaming
  or removing a field, changing a unit or an axis, changing the meaning of a value, adding a variant
  that an older engine would misread.
- A version bump ships with a migrator that upgrades every older version (spec 12 § 4a). DO27 does
  not have to re-bake to take an engine update. `regia upgrade-project` lists the files that need
  migration.
- The format guard (`cargo xtask check-formats`) hashes each format's field names. A change to a
  format without the matching version bump fails the engine's gate.
- A format that an ADR designs before any crate defines it is listed in
  `docs/formats/proposed-formats.ron`. The same guard fails a listed format that has no row in spec 12
  § 4a, no spec page or no ADR, and one that is already built (ADR-8019). The world geometry core's
  `<map>.meshlayers.ron` and cooked `<map>.meshlayers` (spec 121 § 9) are listed there until lane
  W-06 builds them.
- A frozen format is never changed without a migrator, because DO27's assets are produced in
  parallel (AGENTS.md rule 9).

## 1. `<map>.holes.ron`: the terrain hole mask

**Status: main.** `HolesMask`, `HoleShape`, `HoleEllipse`, `HoleRaster`, `TerrainSharedDraw::set_holes`,
`HolesMask::cut_heights`, the file format (`HolesMask::from_holes_ron`, `to_holes_ron`, `load_holes_ron`,
`holes_ron_path`) and the hole-aware ray (`raycast_with_holes`, `pick_terrain`, agent
`terrain.raycast`) are all in `regia_terrain`. `HOLES_FORMAT_VERSION` is 2: version 2 adds named holes
(ADR-1595, `book/agents/terrain-runtime.md`); the version 1 example below still loads unchanged.

**Who owns what.** DO27's bake computes the footprints (the tunnel mouth and ramp trench outlines),
writes the file next to the map, and builds the tunnel lip meshes. Regia Engine cuts the terrain: no
triangles inside the footprint at any LOD, the same cut in color, depth, velocity and shadow passes,
irradiance probes and the path tracer that treat the cut as open space, rays that pass through, and
the physics heightfield cut (NaN samples).

**File name.** The map file name up to its first dot, plus `.holes.ron`:
`maps/switchbacks.level.ron` has `maps/switchbacks.holes.ron`
(`regia_terrain::holes_ron_path`). A map with no holes has no file; `load_holes_ron` returns
`Ok(None)`.

**Worked example** (this exact text is parsed by the engine test `do27_contract_examples`):

```ron,do27-holes
(
    version: 1,
    holes: [],
    shapes: [
        // A 30 m tunnel mouth: an even-odd polygon, world XZ metres.
        polygon(points: [(-15.0, -9.0), (14.0, -10.0), (16.0, 8.0), (0.0, 12.0), (-14.0, 9.0)]),
        // A ramp trench: a rectangle rotated about Y.
        rect(center: (40.0, 2.0), half_extents: (10.0, 12.5), rotation_radians: 0.7),
        // A collapse pit.
        ellipse((center: (-40.0, 0.0), radius: (6.0, 4.0), rotation_radians: 0.0)),
        // A coverage raster for an outline a polygon fits badly: 4 x 4 texels over a
        // 16 m square, row = local z, 255 = hole. Inside where the bilinear coverage is >= 50 %.
        raster((
            center: (-60.0, 30.0),
            half_extents: (8.0, 8.0),
            rotation_radians: 0.0,
            width: 4,
            height: 4,
            coverage: [
                0,   0,   0, 0,
                0, 255, 255, 0,
                0, 255, 255, 0,
                0,   0,   0, 0,
            ],
        )),
    ],
)
```

**Rules** (the loader refuses a file that breaks one):

- At most **64 shapes** in all, ellipses included. The GPU draws one 256 x 256 coverage window per
  shape.
- All coordinates are **world XZ in metres**. The terrain is centered on the world origin, so
  DO27's bake converts its map coordinates once, when it writes the file.
- A polygon has at least three finite points and is filled even-odd, so it may be concave or
  self-intersecting.
- Radii and half extents are finite and greater than zero. A raster's `coverage` holds exactly
  `width x height` bytes, at most 2048 texels a side.
- `holes` is the original ellipse list and `shapes` is everything else. Both count toward the 64.
  The bare `( ... )` form and the `HolesMask( ... )` form both parse.
- Prefer a polygon or rectangle over a raster where the outline allows it: the cut edge is then
  exact per pixel instead of limited by the raster. Baked lip meshes should extend past the cut
  edge so the seam is hidden.

**Loading in the shell.** The file is not applied by itself. After the map loads, the shell calls
`load_holes_ron(&holes_ron_path(map))`, then `TerrainSharedDraw::set_holes(mask)` for the drawn
terrain, the probes and the path tracer, and `HolesMask::cut_heights` on its own physics heights.
The agent call `terrain.load_holes` (`path` or `ron`) does the draw part for a check.

**Errors.** `load_holes_ron` and `HolesMask::from_holes_ron` return `HolesFileError`: `Io` (file
exists but cannot be read), `Parse`, `NewerVersion(n)` (the file's `version` is above
`HOLES_FORMAT_VERSION`; the version is read before the body, so a newer body is never guessed at),
`Invalid(HolesError)` and `RasterTooLarge`. The agent call `terrain.load_holes` reports any of them
as an invalid-params error; it also refuses a `path` outside the project and one with `..`.

**Versioning.** Version 1 files load as version 2 with no named holes (ADR-1595); `regia upgrade-project
--write` re-saves them at version 2. Changing a shape field is a version bump with a migrator. The format guard fingerprints the document, `HoleEllipse`, `HoleRaster` and the variant
names; a change to the `rect` or `polygon` fields is not caught by the fingerprint, so the version
is bumped by hand then.

**Acceptance tests DO27 can run** (all on lavapipe, no DO27 maps; they build synthetic scenes):

- `cargo test -p regia_terrain --test terrain_holes_flythrough`: a 30 m mouth flown from 9 km to a
  few metres at every LOD; no cracks, no floating skirt, no z-fighting.
- `cargo test -p regia_terrain --test terrain_holes_gi` and
  `cargo test -p regia_pt --test terrain_holes`: probes and the path tracer see the mouth as open.
- Agent: `terrain.load_holes`, `terrain.holes` (count and generation), `terrain.raycast` (a ray down
  a mouth hits nothing more of the terrain).
- DO27's own check: parse the baked file with `HolesMask::from_holes_ron` in its bake test and
  assert `mask.validate()` and the shape count.

## 2. `MapLayer(u8)` and `world_pick`

**Status: main** (lane `v020i-worldpick-layers`, merged). The plan put the picking code in `regia_editor`,
which runtime crates may not depend on, so the code moved into a new crate **`regia_pick`**.
`MapLayer` lives in `regia_scene` (`regia_scene::prelude::MapLayer`) and `regia_pick`
re-exports it.

**Who owns what.** DO27's bake assigns a layer to every entity that is not on the terrain surface.
Regia Engine reports which layer a ray hit.

<!-- ignore-reason: signature sketch of the host contract, items without bodies -->
```rust,ignore
// regia_scene
#[derive(Component, Reflect, Clone, Copy, Default, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MapLayer(pub u8);

// regia_pick
pub fn world_pick(world: &World, ray: Ray, filter: &PickFilter) -> Option<WorldPick>;
pub fn world_pick_layers(world: &World, ray: Ray, filter: &PickFilter) -> Vec<WorldPick>; // nearest hit per layer

pub struct PickFilter {
    pub max_distance: f32,      // default: unbounded
    pub layers: LayerSet,       // default: all 256; LayerSet::only(&[1, 2])
    pub cull: FaceCull,         // default: None (both faces)
    pub terrain: bool,          // default: true
    pub meshes: bool,           // default: true
    pub include_hidden: bool,   // default: false
    pub skip: Vec<Entity>,      // e.g. the shooter's own body
}
// PickFilter::on_layers(&[1, 2]) restricts a ray to those layers.

pub struct WorldPick {
    pub layer: MapLayer,
    pub entity: Entity,         // the mesh part or terrain entity, not the model's owner
    pub point: Vec3,
    pub normal: Vec3,
    pub distance: f32,
    pub surface: PickSurface,   // Terrain | Mesh
    pub triangle: Option<u32>,  // None for terrain
}
```

**Rules.**

- **Terrain is layer 0.** An entity without a `MapLayer` is layer 0 too.
- A `MapLayer` on a parent applies to every descendant that has no `MapLayer` of its own (the
  nearest ancestor wins).
- A layer is **a bake-assigned id per entity**, not a height band. Layers do not need to be
  contiguous. Regia Engine attaches no meaning to the number beyond identity; DO27 decides what 1, 2 and
  3 are (a tube, a deck).
- The terrain march passes through holes (section 1), so a ray at a tunnel mouth continues to the
  mesh floor below.
- Skinned meshes are picked where their joint palette puts them (CPU skinning). Rigid meshes of 64
  triangles or more use a cached BVH.
- Components serialize in a level through reflection:
  `"regia_scene::components::MapLayer": (2)`.
- `world.pick` in the agent protocol takes `origin`, `direction`, `max_distance`, `layers`,
  `all_layers`, `cull`, `terrain`, `meshes`, `include_hidden` and `skip`, and returns
  `{ hit: { entity, name, layer, surface, point, normal, distance, triangle } | null, count }`
  (or `hits` with `all_layers`).

**Not covered.** DO27's simulation keeps its own deterministic "what is under me" query. This is the
render-side query the camera and the order marker use.

**Acceptance tests.** `cargo test -p regia_pick --test world_pick` builds a stack of floors, a
terrain hole, inherited layers, skinned meshes and checks the BVH against brute force, with no GPU
and no maps. Agent: `world.pick` straight down through a mouth with `all_layers: true` returns one
hit per floor, nearest first.

## 3. `TerrainHeightEdits`: runtime terrain edits as sample rectangles

**Status: as built, stages 1 and 2 (ask 17a; ADR-1595, ADR-7585).** The names are still **proposed**
until DO27 agrees (spec 25 § 7.1), so they may change; `HeightPatch` (metres) stays as the general API.
Stage 1 is the runtime height-patch queue (`TerrainHeightPatches`): a handle that reaches the installed
draw, a per-frame sample budget, regional CPU refresh (the ECS map, its min/max pyramid, the draw's map,
GPU height texture and mips) and crack-free LOD seams. Stage 2 narrows what an applied band invalidates:
the scatter instances under it, the sun cascades it lands in, the grass selection when it is in range,
and the virtual-texture pages it meets. Still whole-map: the engine's probe volumes rebuild their
heightfield pyramid (planned in ADR-7585).

**Who owns what.** DO27's deterministic grid is the truth. DO27 computes the new heights and sends
Regia Engine a rectangle of them. Regia Engine uploads and re-lights; it does not compute craters for the
sim and never writes back. Physics stays DO27's (`TerrainHeightsChanged` carries the box if the shell
wants it).

**The message** (in-process Rust types; the shell sends them, they are not a file):

<!-- ignore-reason: signature summary of the host contract; the real items are in regia_terrain -->
```rust,ignore
/// A cloneable handle over the draw's patch queue. `Send + Sync`; also a `Resource`.
pub struct TerrainHeightEdits { /* the TerrainHeightPatches queue */ }

impl TerrainHeightEdits {
    pub fn new(patches: TerrainHeightPatches) -> Self; // draw.height_patches_handle()
    /// Queues one rectangle; returns its patch id (done once `patches().state().completed`
    /// reaches it). Nothing is queued on error.
    pub fn push(&self, edit: HeightEdit) -> Result<u64, HeightEditError>;
    /// Edits accepted since the last applied frame (at most `MAX_EDITS_PER_FRAME`).
    pub fn pushed_this_frame(&self) -> u32;
    /// Bands applied so far (for tests and telemetry).
    pub fn generation(&self) -> u64;
}

pub struct HeightEdit {
    /// Sample columns and rows of the render map, `[col, col + width) x [row, row + height)`.
    pub col: u32,
    pub row: u32,
    pub width: u32,  // at most MAX_EDIT_SIDE = 128
    pub height: u32, // at most MAX_EDIT_SIDE = 128
    /// `width * height` absolute heights in millimeters, row-major, row 0 = the -Z edge of the map
    /// (the order of `TerrainMap::heights`). Metres = `mm as f32 * 0.001`.
    pub heights_mm: Vec<i32>,
}

/// A message per applied band: `rect` (a `DirtyRect`) and the world box it touched
/// (`world_min`, `world_max`, over the old and the new heights).
pub struct TerrainHeightsChanged { /* rect, world_min, world_max */ }
```

**Worked example.** A 3 x 2 rectangle at sample column 300, row 412 (on a map whose height range holds
11 to 13 m). `crates/regia_terrain/tests/do27_contract_examples.rs` parses it, pushes it and checks the
samples:

```ron,do27-height-edit
HeightEdit(
    col: 300,
    row: 412,
    width: 3,
    height: 2,
    heights_mm: [12040, 11020, 12040, 12500, 11980, 12500],
)
```

**Rules as built.**

- **Absolute replace, not additive.** The rectangle overwrites the samples. A lost or repeated
  message cannot drift the surface, and DO27 derives any delta on its side.
- **Render-grid samples.** The rectangle is in the render map's sample grid
  (`TerrainMap::width()` x `height()`, `sample_spacing()`). If DO27's sim grid differs, DO27
  resamples on its side with a deterministic rule; Regia Engine does not resample sim data.
- **Integer millimeters → 16-bit samples.** Each value becomes the map's 16-bit sample
  `round((mm / 1000 - height_offset) * 65535 / height_scale)`, computed in `f64` from the integer, so
  it is the same on every machine; the drawn height is within one 16-bit step (`height_scale / 65535`)
  of the millimeters.
- **Order and coalescing.** Edits apply in push order; on overlap the later one wins. Every edit pushed
  before a frame's apply is taken in that frame, unless the per-frame sample budget
  (`TerrainHeightPatches::set_budget`, default 65 536 samples) spreads the rest over the next frames.
  No frame writes more than the budget (a single row wider than it goes alone).
- **Limits, refused with codes.** `edit_too_large` (a side over 128 samples), `edit_frame_full` (a 17th
  edit before the next applied frame), `edit_out_of_range` (outside the map's height range: refused,
  not clamped), and the `patch_*` codes for an empty or out-of-map rectangle, a wrong length, no target
  or a full queue (`book/agents/terrain-runtime.md`). A refused edit changes nothing.
- Normals come from the heights in the shader, so a rectangle needs no normal data and no apron.
- Holes (section 1) are independent of heights: an edit under a hole still writes its samples.
  Keeping craters off tunnel lips is DO27's rule.
- **What it costs.** Each applied band invalidates only: scatter tiles under it (the instances standing
  on changed ground move vertically onto it; every other instance keeps its matrix bit for bit), the sun
  cascades whose box it lands in (a one-sample edit far from the camera redraws no cascade), the grass
  patch selection when it is in grass range, and the virtual-texture pages its own rectangle meets.
  Counters: `TerrainDrawStats::{height_edit_boxes, cascade_edit_invalidations, grass_edit_reselects,
  virtual_texture_edits}`, `RuntimeScatter::last_resit`.
  The probe volumes' heightfield max pyramid (the terrain the GI rays march) re-reduces only the
  texels the band's changed samples reach, at every level (ADR-7659): one 13 × 13 crater on a
  513-sample map costs 278 of a full rebuild's 349 525 texels (0.08 %). A frame the probes skip, holes or a
  host's `update_heights` still rebuild the whole pyramid. Counter: `render.get_dynamic_gi`
  `renderer.pyramid` (`FrameRenderer::dynamic_gi_status().pyramid`). Measured GPU frame time
  (`probe_pyramid_edit_frame_gpu_cost`, 4097-sample map, 1280 × 720, RTX 5090): one 128 × 128 edit
  adds 0.085 ms to a 0.86 ms frame (0.28 ms before ADR-7659). Known issue: a scatter instance re-sat on a sloped crater keeps the tilt of the
  old ground.
- **Call `prepare_height_edits()` at load.** After inserting the `RuntimeScatter`, call
  `RuntimeScatter::field_mut().prepare_height_edits()` once during the load screen. It takes the
  scatter field's own copy of the heights there (one copy of the map's samples, plus one bit per sample
  recording which ground edits wrote), so the first edit in play copies nothing. Without it, the first
  edit copies the whole map in that frame (`ScatterResitStats::heights_copied_bytes`); later edits never do.
- Agent surface: `terrain.patch_heights { col, row, width, height, heights_mm }` and `terrain.get`'s
  `patches` (`completed`, `last_frame`, `edits_this_frame`).

**Versioning.** The Rust types follow the semver rule in the section below. If DO27 later wants a
replay file of edits, it is a new format with its own `version`.

**Acceptance.** `crates/regia_terrain/tests/terrain_height_edits.rs` (lavapipe, synthetic map): 20
crater edits over 200 fixed steps in millimeters, no step over the sample budget, no crack at node or
LOD edges (the vertex shader's seam rule on a mirror map), far one-sample edits redraw no cascade, one
virtual-texture rectangle per changed band, never merged. Not measured: GPU frame time on DO27-class
hardware (the plan's "20 explosions in 10 s, no frame over budget" is checked by the sample budget and
the counters, not a wall clock). Open question for DO27: the sim grid spacing against the render map.

## 4. `ExposureZone` and `PostProcessVolume`

**Status: main.** `ExposureZone` and the blend and adaptation math (lane `v020m-exposure-zones`, merged;
`regia_render::exposure_zones`) and the frame hook (ADR-322) ship. **The frame samples the zones:**
`regia_framework::exposure_zones::ExposureZonesPlugin`, part of `DefaultPlugins`, runs `sync_exposure_zones` each
frame in `PostUpdate`, weighs every zone at the active camera (the window camera with the lowest `order`) and
calls `RuntimeRenderer::set_exposure_target`. A host without `DefaultPlugins` adds the plugin, or calls
`set_exposure_target` and `set_exposure_dt` on its `FrameRenderer` before `render_into`. The host-side setup and
the tunnel example are in the [DO27 host checklist](do27-host-checklist.md#exposure-zones-drive-exposure-v0128-do27-ask-5).
`PostProcessVolume` does **not exist**; the plan's name was a draft. The volume that exists is `PostVolume`
(spec 30): one whose overrides carry an `eye` range joins the same combine as a zone (ADR-1464).
`ExposureZone` stays the interface for DO27's bake.

**Who owns what.** DO27's bake places the zones (a box or capsule per tunnel section and mouth) and
chooses the EV numbers. Regia Engine blends, adapts and logs.

<!-- ignore-reason: signature sketch of the host contract -->
```rust,ignore
// regia_render::exposure_zones, re-exported at the crate root
#[derive(Component, Clone, Copy, Debug, PartialEq)]
pub struct ExposureZone {
    pub shape: ExposureZoneShape,   // Box { half_extents: [f32; 3] } | Capsule { radius, half_height }
    pub ev_min: f32,                // the eye chases the midpoint of ev_min..ev_max
    pub ev_max: f32,
    pub blend_distance: f32,        // metres OUTSIDE the surface over which weight rises 0 -> 1
    pub adapt_up_speed: f32,        // EV per second, brightening (default 2.0)
    pub adapt_down_speed: f32,      // EV per second, darkening (default 1.5)
    pub priority: i32,              // higher wins; equal priorities blend
}
// Default: a 1 m box, EV 14.5, hard edge, speeds as above, priority 0.

pub fn zone_weight(signed_distance: f32, blend_distance: f32) -> f32;
pub fn combine_zones(zones: &[(f32, &ExposureZone)], default: EvRange) -> EvTarget;
pub fn adapt_ev(current: f32, target: f32, dt: f32, up_speed: f32, down_speed: f32) -> f32;
```

**Rules.**

- The zone sits on an entity with a `GlobalTransform`; the shape is local, before entity scale.
  A capsule runs along local Y; `half_height` is half the cylindrical part, so the total length is
  `2 * (half_height + radius)`.
- The blend band is **outside** the surface: weight 0 at `blend_distance` metres out, 1 on the
  surface and inside, Hermite smoothstep between. A narrow tube can therefore fade over a 25 m
  mouth. A distance of 0 is a hard edge.
- `adapt_ev` is `current + (target - current) * (1 - exp(-speed * dt))`, with `dt` capped at 0.1 s.
  It never overshoots the target and never steps past it.
- The component has **no serde and no reflection yet**, so a level cannot carry it. Until the
  renderer adds `Reflect`, the shell spawns zones from code (the bake can emit a list that the
  shell turns into entities). Adding `Reflect` is additive.
- No host struct gained a field: `ExtractedFrame` and `PostSettings` are unchanged.
- Manual exposure (auto exposure off, a camera `exposure_ev`) ignores zones; a map with no zones
  renders as before.

**Reference numbers** (from the acceptance drive): tube EV 8.5, sun EV 14.5, a box of half extents
8 x 4 x 200 m, blend 25 m, 2.0 EV/s up, 1.5 EV/s down.

**Acceptance.** `cargo test -p regia_render exposure_zones` includes
`scripted_tunnel_drive_has_no_flash_or_blackout`: 30 fps, 40 s at 20 m/s, the log stays inside
[8.5, 14.5], is monotone while approaching a target, and no frame moves more than 0.5 EV
(`MAX_TUNNEL_FRAME_DELTA_EV`). `cargo test -p regia_framework exposure_zones`
(`the_target_is_set_and_cleared_as_the_camera_crosses`) checks the frame hook. The in-engine version (the real
frame) is logged through the `render.exposure_log` agent call (`start`, `stop`, `get`) and read with
`render.get_exposure` (see [Exposure zones](../agents/exposure-zones.md)).

## 5. `AudioReverbZone`: presets, `distance_scale`, occlusion

**Status: main** (lane `v020i-audio-zones`, merged). The host-visible changes are in the host
checklist.

**Who owns what.** DO27's bake places the zones, picks the presets, and lists each tunnel's mouths.
DO27's shell installs the ground height for rock occlusion. Regia Engine blends the reverb, scales
distance and routes sound.

<!-- ignore-reason: signature sketch of the host contract -->
```rust,ignore
// regia_audio
pub struct AudioReverbZone {
    pub shape: ReverbZoneShape,   // Sphere { radius } | Box { half_extents } | Capsule { radius, half_length } | Global
    pub fade_distance: f32,       // blend distance outside the volume, metres (accessor blend_distance())
    pub send_db: f32,             // wet send (accessor wet_db())
    pub feedback: f32,            // decay 0..1 (accessor decay())
    pub damping: f32,
    pub stereo_width: f32,
    pub priority: i32,
    pub preset: ReverbPreset,     // Custom | Tube | MineGallery | Open
    pub distance_scale: f32,      // > 1 carries further; clamped to 0.1..10
    pub cavity: bool,             // the zone is air inside rock
}
// Constructors: AudioReverbZone::tube(shape), ::mine_gallery(shape), ::open(shape), ::with_preset(p, shape).

pub struct AudioZonePortals { pub portals: Vec<ZonePortal> }   // component, on a cavity zone's entity
pub struct ZonePortal { pub position: Vec3 /* zone-local */, pub radius: f32 }
pub trait RockField { fn surface_height(&self, x: f32, z: f32) -> Option<f32>; }
pub struct AudioRockField;     // resource: set(Arc<dyn RockField>) / clear()
```

The names the ask used map onto the existing fields, nothing is renamed: `wet` is `send_db`,
`decay` is `feedback`, `blend_distance` is `fade_distance`.

| Preset | `send_db` | `feedback` | `damping` | `stereo_width` | `fade_distance` | `distance_scale` | `cavity` |
|---|---|---|---|---|---|---|---|
| `Custom` | -6 | 0.85 | 0.4 | 1.0 | 2 | 1.0 | no |
| `Tube` | -3 | 0.94 | 0.2 | 0.6 | 6 | **1.5** | yes |
| `MineGallery` | -5 | 0.88 | 0.45 | 0.8 | 5 | **1.5** | yes |
| `Open` | -18 | 0.35 | 0.7 | 1.0 | 20 | 1.0 | no |

**Rules.**

- **Zones blend** instead of one winning: they are layered least specific first (priority, then
  larger volume), and each crossfades over the layers below by its own weight (1 inside, falling
  linearly to 0 over `fade_distance`). A nested room still takes over inside its volume.
- **Distance scale.** An emitter and a listener that both stand in a zone hear the emitter as if it
  were `distance_scale` times closer. Regia Engine stretches the emitter's `min_distance`,
  `max_distance` and rolloff points. A zone's share is the lower of the two memberships, so the
  scale fades out over the blend distance.
- **Rock occlusion** needs the shell. Install `AudioRockField` with the ground height at `(x, z)`,
  and `None` inside a terrain hole or off the map (`terrain.map.sample_bilinear(x, z)` and
  `HolesMask::contains`). Regia Engine marches the segment from listener to emitter and sums the length
  below the surface that is outside every `cavity` zone: 2.5 dB per metre (cap 60 dB) and a low-pass
  from 20 kHz down to 350 Hz over 6 m (`RockTuning`). Without a rock field no rock is ever
  reported.
- **Mouth routing.** `AudioZonePortals` on a cavity zone lists its mouths (zone-local). An emitter
  inside the cavity with a listener outside is heard from a virtual position along the
  listener-to-mouth direction at the length of the path through the nearest mouth. A zone without
  portals routes nothing, and thickness occlusion alone applies. The bake must emit the mouth points
  for an engine in a tunnel to be audible near the mouths and silent elsewhere on the surface.
- `AudioReverbZone`, `ReverbZoneShape`, `ReverbPreset` and `AudioZonePortals` are reflected, so a
  level carries them as components. `ZonePortal` positions are in the zone entity's local frame.

**Acceptance tests** (no devices, no DO27 maps): `cargo test -p regia_audio --test zones` has
`an_engine_in_a_tunnel_is_heard_from_the_surface_only_near_the_mouths` and
`underground_the_engine_carries_one_and_a_half_times_further` as pure math, and the same behaviors
through an `App` on the offline backend. Agent: `audio.probe_zones` with the listener above, at the
mouth and inside. **Not verified:** how the presets sound on a real output device.

## 6. `VisCell` and portals for indoor occlusion

**Status: lane, stage 1** (branch `sprint-w9-do27-viscells`, ADR-7586, the v0.6 line; ask 10), **stage 2**
(branch `sprint-w10-do27-viscells-stage2`, ADR-7673).
The names and the version 1 file below are built and tested; DO27 reviews them before the
v0.6.0 release freezes the format. Agent guide: [Indoor cells and portals](../agents/viscells.md).

**Who owns what.** DO27's bake knows the tunnel segments, floors and mouths, so it emits the cells
and the portals. Regia Engine traverses them per view and skips what no portal chain reaches. A bad
portal set pops geometry, so check it with the debug draw (`render.set_viscells { "debug_draw":
true }`) before trusting the cull.

<!-- ignore-reason: signatures only; the full types are in regia_scene and regia_render::viscells -->
```rust,ignore
// regia_scene::components
#[derive(Component, Reflect, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VisCell(pub u16);   // which cell the entity and its untagged descendants belong to; none up the chain = always drawn

// regia_render::viscells
pub const VISCELLS_FORMAT_VERSION: u32 = 1;
pub fn viscells_ron_path(map: &Path) -> PathBuf;                       // maps/x.level.ron -> maps/x.viscells.ron
pub fn load_viscells_ron(path: &Path) -> Result<Option<VisCells>, VisCellsError>; // no file = Ok(None)
impl VisCells { pub fn from_ron(text: &str) -> Result<Self, VisCellsError>; }   // insert as a resource
```

```ron,do27-viscells
// maps/cinder_steps.viscells.ron
(
    version: 1,
    cells: [
        // Cell 0 is outdoors: terrain, sky, and every view outside all boxes. Listing it is optional;
        // it never has bounds.
        (id: 0, name: "outdoor", outdoor: true, bounds: []),
        // An oriented box (center, half_extents, rotation_radians about +Y), or several of them.
        (id: 1, name: "tube_a", bounds: [(center: (120.0, -18.0, 40.0), half_extents: (8.0, 6.0, 90.0), rotation_radians: 0.4)]),
        (id: 2, name: "bunker", bounds: [
            (center: (150.0, -20.0, 140.0), half_extents: (12.0, 4.0, 10.0)),
            (center: (150.0, -20.0, 158.0), half_extents: (4.0, 3.0, 8.0)),
        ]),
    ],
    portals: [
        // A convex quad between two cells, four world points in order around it (either winding).
        // The tunnel mouth: outdoors to tube_a.
        (a: 0, b: 1, quad: [(110.0, -14.0, -48.0), (126.0, -14.0, -48.0), (126.0, -22.0, -48.0), (110.0, -22.0, -48.0)]),
        // The bunker door at the tube's far end.
        (a: 1, b: 2, quad: [(146.0, -16.0, 130.0), (154.0, -16.0, 130.0), (154.0, -24.0, 130.0), (146.0, -24.0, 130.0)]),
    ],
)
```

**Rules (as built).** Cell ids are `u16` and unique. A view is in the cell whose box holds its eye
(the smallest box wins on overlap). In no box but in a portal's doorway (over the quad, within the
wider of the two gaps the cells' boxes leave on each side of the portal, plus the portal tolerance,
from its plane), it is in both of that portal's cells (ADR-7639, ADR-7646), so boxes may stop at
the wall faces; in no box and no doorway, it is in the outdoor
cell. Keep that gap about the wall's thickness. Portals are two-way convex
quads: planar within 1 % of the longest diagonal (at least 1 cm), never a cell to itself, both cells
listed (0 always is). A file breaking a rule is refused whole with a code (`viscells_unknown_cell`,
`viscells_self_portal`, `viscells_portal_not_planar`, `viscells_portal_not_convex` and the rest are
listed on the agent page); a newer `version` is refused before the body is read. Put `VisCell` on
a building's root (or any ancestor): an entity without one takes its nearest tagged ancestor's
(stage 2, as `MapLayer` does), and a part tagged itself keeps its own. An entity with no `VisCell`
anywhere up its chain always draws.

**Stage 2 (ADR-7673).** Besides the colour draws, the point and spot lights of an unreached cell
leave the frame (no shading, no shadow-map update) when their range touches no reached cell, and
the glass panes of culled draws add no glass instance. The agent page lists what is not
cell-culled yet. Shadow views are still not culled: an unreached cell's casters draw into the sun's
cascades. The reached set is the union over the frame's views.

**Audio mouths from the same portals.** Instead of baking `AudioZonePortals` (section 5) twice,
derive them from the loaded cells:

<!-- ignore-reason: signatures only; the full types are in regia_render::viscells and regia_audio -->
```rust,ignore
// regia_render::viscells
pub struct AudioZonePose { pub centre: Vec3, pub rotation: Quat }      // the zone entity's world pose
pub struct AudioZoneMouth { pub zone: usize, pub position: Vec3, pub radius: f32, pub portal: usize }
pub fn audio_zone_mouths(cells: &VisCells, cell_zones: &[(u16, usize)], zones: &[AudioZonePose],
                         out: &mut Vec<AudioZoneMouth>);

// cell_zones: (cell id, index into `zones`); a cell not listed (usually 0, the outdoors) has no zone.
// Each portal that leaves a zone is one mouth of it: the portal centroid in the zone's frame and the
// farthest corner as the radius; a portal between two cells of one zone is no mouth. Then, per zone i:
//   AudioZonePortals { portals: mouths of zone i as ZonePortal { position, radius } }
```

**Acceptance (built).** `cargo test -p regia_render --test viscells` on a synthetic two-room scene:
facing away from the door, room B's draw is counted culled and the frame equals a control with B
hidden; looking through the door, B draws and the frame equals the control without cells; a portal
to cell 0 keeps the outside reachable; the walk stops at a visit cap; a steady frame allocates
nothing (`--test viscells_alloc`). Agent: `render.viscells` reports the per-view eye cell, the
reached ids and `frame.culled_draws`.

## 7. `Steam::client()`: one shared Steam client

**Status: main** (lane `v020j-steam-core`, merged).

**The rule: the shell must not initialize steamworks.** Steam allows one API instance per process.
`regia_steam` already owns it; a second `Client::init` double-initializes the SDK, and each side
pumps or shuts down the other's callbacks.

<!-- ignore-reason: needs the `steamworks` feature and signatures only -->
```rust,ignore
// regia_steam, behind feature `steamworks`, desktop only (not Android)
impl Steam {
    pub fn client(&self) -> Option<steamworks::Client>;  // a cheap clone; None when Steam is not running
    pub fn shutdown(&mut self);                           // idempotent; the plugin calls it on exit
}
// Resource: SteamStatus { running, offline, user_id, app_id, overlay_open }
```

What DO27's shell does:

1. Adds `SteamPlugin` (feature `steam`, plus `regia_steam/steamworks` for the real backend) and
   takes `res.client()` for its own Steam calls. Multiplayer over Steam does not need the client:
   `regia_net` runs over `regia_steam::SteamSocket` and Steam lobbies come from `SteamLobbyPlugin`, both on
   the engine's client (ADR-7906; [Networking](networking.md)).
2. Depends on the **same `steamworks` version** as `regia_steam`: 0.13.x (`docs/03` § 5). A second
   version means `Client` types that do not unify. Check: `cargo tree -i steamworks` shows one version.
3. Does **not** pump callbacks. The engine runs `run_callbacks` once per frame in `PreUpdate`;
   callbacks the shell registers on the shared client fire from that pump.
4. Drops any client clones it holds before the engine shuts down on exit.

The app id comes from project settings, `Regia.toml` `[steam]`, `STEAM_APP_ID` or
`steam_appid.txt`. Steam Input is bridged by the same layer (`SteamInputSettings`, agent `steam.input`; see
[Steam (agents)](../agents/steam.md)).

**Acceptance.** `cargo test -p regia_steam --test shared_client` runs on the mock backend with no
Steam running (`initializes_exactly_once_and_publishes_status`,
`shutdown_is_idempotent_and_runs_on_terminate`). DO27's side: `steam.status` over the agent
protocol shows `running`, and the shell's own `Cargo.toml` has no `Client::init` call. **Not
verified:** the real `Client` path, which compiles only where the Steamworks SDK is present.

## 8. Object keys

**Status: main.** This is a rule about `FrameMesh::mesh_key`, `material_key` and `object_key` that
the renderer already follows (`crates/regia_render/src/frame.rs`).

The renderer matches "the same object as last frame" by `object_key`: for the previous model matrix
(motion vectors, so TAA does not smear), the previous joint palette, per-object overlays and decals.
**Key 0 is ephemeral.** A mesh submitted with key 0 is given
`EPHEMERAL_KEY | its index in the frame's mesh list` (`EPHEMERAL_KEY = 0xFE00_0000_0000_0000`), so
"the same object" means "the same list position". Any spawn, despawn or reorder then pairs an
object with another object's previous matrix, and the shifted objects smear. Meshes with
no motion (scatter) may stay at 0.

**The rule for moving objects:**

```rust
/// `entity_id` is the sim entity's id: below 2^48, stable for the entity's lifetime, never reused
/// within a match. `part_index` numbers the entity's meshes (hull, turret, wheel): 0..=255.
fn object_key(entity_id: u64, part_index: u8) -> u64 {
    debug_assert!(entity_id < (1 << 48));
    (entity_id << 8) | u64::from(part_index)
}
```

- The key is below 2^56, so its top byte is `0x00`. **Never use a key whose top byte is `0xFE`
  (`EPHEMERAL_KEY`) or `0xFF` (`SHADOW_LOD_MESH_TAG`)**: the renderer reserves them.
- **Stable for the entity's lifetime:** the same entity has the same key every frame, whatever
  else spawns or despawns.
- **Never reused within a match:** a recycled key would pair a new entity with the old entity's
  previous matrix on its first frame.
- The same part always has the same `part_index`. Key 0 stays only for objects that do not move.
- `mesh_key` and `material_key` name cached GPU resources (the same key reuses the uploaded
  buffers; contents are re-checked), so those are shared by all objects that share a mesh or a
  material and are keyed by the asset, not the entity.

The engine test `do27_contract_examples` checks the formula against the reserved ranges.
**Built (v0.1.28):** a once-per-run `tracing` warning when temporal anti-aliasing is on and at least
`ZERO_KEY_WARN_MIN` (4) moving meshes are submitted with key 0, counted in
`DrawStats::zero_key_moving_meshes` and the telemetry `draws.zero_key_moving_meshes`; a test documents the
reorder mispairing as known behavior.

## 9. `regia_voxel_core`: the voxel boundary

**Status: proposed (ask 17, spike first).** No crate exists (checked at v0.5.0). This section records what DO27 and
Regia Engine agreed on 2026-10-17, so the crate is designed against it.

**Why a separate crate.** DO27's simulation needs the terrain field itself, bit-exact on every
machine: floor and ceiling sampling, hover physics, projectile hits and the navigation bake, under
lockstep and for replays. A float render mesh cannot be that. So the field and its edit log live in
a small pure-Rust crate with **no GPU dependency** that DO27's sim links directly. Regia Engine meshes the
same field for rendering. Collider meshes are acceptable only if derived deterministically (integer
vertices or a canonical extraction); otherwise DO27 queries the field.

**Boundary.**

- **Integer-only public API.** No `f32` or `f64` in anything the sim calls.
- **Edits are an op log** of fixed-point CSG operations (integer-only SDF update). Replaying the log
  produces a bit-identical field on any machine and thread count.
- **Storage (the spike's starting point):** sparse 32 x 32 x 32 bricks with a 1-voxel apron,
  `int16` distance clamped to +/- 4 voxels, `u8` material id, voxels of 0.25 to 0.5 m, three LODs.
  Only near-surface bricks are resident.
- **A frozen cooked format of its own** from a `.voxvol.ron` source, with a schema and a migrator
  test, per the rule at the top of this page. It cannot be `.fvox`: since ADR-2445 that extension is
  the cooked voxel far proxy (a rendering LOD, now version 4), which is not this boundary. The terrain format v3 is untouched.
- **Semver contract.** DO27 links the crate, so a public API change needs a version bump, and **DO27
  signs off on every API change** before it merges.
- **Memory gate: at most 1.5 GB of resident voxel memory per map on HIGH.** Lower figures for the
  Deck and LOW presets are still to be set with the owner. Sizing for the spike to check, not a
  result: a dense 256 m box at 0.5 m is about 134 million voxels, roughly 400 MB; a 32-cubed brick at
  3 bytes a voxel is about 98 KB, so 1.5 GB is about 15,000 resident bricks, about 1,000 per volume
  across 12 to 14 volumes. The spike measures real brick counts for a tube and a cave before any
  commitment. This is a go or no-go gate.
- Volume counts DO27 expects: The Cinder Steps about 12 to 14 volumes (128 to 256 m tubes, 64 to
  128 m decks, 64 m collapse sites); Shackleton 3 to 6 of 128 to 256 m; the Switchbacks retrofit 8 to
  10 of 256 m. Open-ground craters stay on section 3.

**Acceptance (planned, spike gates).** G1 a watertight mesh with matching seam vertices; G2 no rim
gap over 2 cm, 20 craters in 10 s under budget, equal field hashes across thread counts, and the
resident-memory measurement; G3 a lit cave plus an Android compile. DO27 can run the equal-hash
test on its own machines.

**Design for sign-off (2026-11-16):** spec 122 (`docs/specs/122-voxel-core.md`, ADR-8116). It keeps this
boundary and changes three starting points from measurements: 8 x 8 x 8 bricks without an apron, the file name
`<map>.voxfield` (`.fvox` is already the voxel far proxy), and a per-map memory cap that holds for every preset.
The sim reads the field through `regia_world_geom` (spec 121). The questions DO27 answers are in spec 122 § 18.

## 10. Stacked floors: probes, slabs and interior volumes (bake rules, no new format)

DO27 ask 11: light on one storey must not show on the storey under it, and probes must not interpolate across a floor.
No new file or type: these are rules for what DO27's bake already writes, checked by
`cargo test -p regia_pt --test stacked_floors` (ADR-7661; the numbers are in the ADR).

**What the engine does.** The baked probe volume tests every neighboring probe pair against the geometry at bake time and
weights each tap by its visibility, so one interior brick over a whole building keeps a storey's light on its storey
(measured leak 0.002 of the lit storey's light, bar 0.006 to 0.009). The dynamic probes (DDGI, Low and High tiers) leak
nothing measurable through a 0.3 m slab. Sun cascades and shadowed point and spot lights stop at the slab. An emissive
mesh drawn as an area light (Desktop High and Ultra) stops 0.25 m past the first surface its distance probe sees.

**What DO27's bake must do.**
- **Slabs at least 0.3 m thick**, closed (no gap between a slab and the outer wall; a slab may run through the wall).
  An emissive area light's shadow fades over at most 0.25 m, so a thinner slab passes some of a ceiling panel's light to
  the storey below.
- **Probe spacing.** One interior brick per building at the default 1 m interior spacing is enough; bricks per storey
  also work. Flag the brick as interior (`VolumeLayout::flag_interior_bricks`). Do not space probes wider than the
  clear height of a storey (2.7 m at a 3 m pitch): each storey must hold at least one probe layer.
- **Bake the emitters into the volume** (format v3 names them, ADR-1386). Inside the volume the forward pass then leaves
  a baked emitter's light to the volume, which keeps it on its storey. An emitter the bake did not see is drawn as an
  area light: a large ceiling panel (2 x 2 m) still puts about 0.5 % of its light on the top of the walls of the storey
  under it, where the slab meets the wall (the distance probe's 16 x 16 texels cannot resolve the slab there; a design
  question for the engine, not a bake rule).
- **One `InteriorVolume` per storey**: the box is the storey's air (floor top to slab underside, wall to wall), so the
  interior shade and the air under the roof follow the storey the camera stands in. Any number may be placed: the fog
  and the aerial perspective read four, the four nearest the camera (`regia_render::aerial_interior_slots`), so the
  camera's own box is always read.
- The lightmap path is not covered by the test yet.

## What changes here, and who is told

A change to a name, field, unit or file format on this page needs, in this order: an ADR in
`docs/decisions.md`; a version bump and a migrator for a file format; an entry in the host checklist
diff for the next release; and DO27's agreement before the release that carries it. Interfaces
labelled **proposed** can change freely until DO27 agrees to them; the ones labelled **main** or
**lane** change only through that path.
