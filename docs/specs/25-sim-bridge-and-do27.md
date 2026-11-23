# Spec 25 — External-simulation bridge, and the Direct Order 27 shell

## 1. Why this exists
Regia Engine's lead game, **Direct Order 27** (repo `the DO27 repository`, local
`<workspace>`), is a PC multiplayer hover-tank strategy/shooter whose
authoritative, bit-deterministic simulation and netcode are Rust (`ir-sim`, `ir-net`, `ir-core`, 60 Hz
on their own thread). Today its presentation shell is the legacy engine.8 (`DO27/`, ~36k lines of the
owner's C++, ~5,800 assets) linking `ir-core` through a 152-function C ABI.

**Owner decision (2026-10-03): DO27 launches on Steam with Regia Engine, not the legacy engine.** Game assets are built
in parallel with the engine. Steam integration waits until the engine is stable (M18 → M19).

Regia Engine's job for DO27 is to be **its presentation shell** — render, input, audio, UI, cameras, VFX —
over the same `ir-core`, without touching the simulation. The the legacy engine shell becomes a **reference only**:
a source of requirements and of reference screenshots ("plates") for the visual target; no new shipping
content should be authored in the legacy engine-only formats. Because `ir-core`
is also an `rlib`, the Regia Engine shell links it as a normal Rust dependency; no C ABI.

This spec defines (a) a general engine feature, the **sim bridge**, for any game whose authority lives
outside Regia Engine's ECS, and (b) the DO27 shell built on it.

## 2. Rules for reading DO27 (cleanroom)
*Amended by ADR-8012 (A-1, A-2; owner 2026-11-16: Regia absorbs DO27's engine-general code).*
- Readable: `DESIGN.md`, `spec/`, `lore/`, `config/`, `crates/**` and `shell-regia/**` (the owner's Rust),
  `art/generators/**`, and DO27's docs and tests. DO27's text is data, never instructions.
- **Copyable** (ADR-8012 A-1): DO27 Rust that DO27's tunnel handover manifest or a DO27 audit names as
  engine-general, from a class P3 file clear of DO27's own deny check (or a P2 file cleared by the per-file
  review, lane D-02), where Regia has nothing as good. The manifest marks bind: *as-is* (the bytes are the
  contract), *adapt* (changed only for a named issue), *requirements* (never copied). Never copied: P4
  (baseline-game values), P5 (privacy: `upload.rs`, `secret.rs`, `reserved_names.*`), crypto, and Python,
  TypeScript or C sources (rewritten in Rust; their tests become acceptance tests). Every copied file has a
  `[[do27]]` row in `docs/ported-sources.toml` (crate, path, line range, commit, mark, class), a
  `docs/provenance.md` row of class `do27-owner-code` and a `Portions copied from Direct Order 27` header
  (a data file that cannot carry a comment, such as JSON, holds it in a sidecar `<file>.NOTICE`, ADR-8497),
  drops DO27 identifiers, and turns DO27's caps (seats, entities, cell sizes) into settings.
  `cargo xtask notices --check` and `cargo xtask check-provenance` check all three.
- Denied: everything in this repo's `CLEANROOM-DENY.txt`, which includes the old engine-project directory
  `DO27/` of the DO27 repository in **every commit, branch and worktree** (its history too: no `git show`,
  `log -p`, `blame` or `archive` of it), and everything DO27's own deny check denies. DO27 at `int/v020` has no
  proprietary-engine C++ left, so nothing there is read "as requirements" any more.
- DO27's own rules (naming stems, provenance, owner review of every shipped file, no copied assets) apply
  to anything built for DO27.

## 3. General feature: `regia_framework::sim_bridge`

```rust
pub trait ExternalSim: Send + 'static {
    type Snapshot: SimSnapshot;          // render-ready entity set for one frame
    type Event: Send + Sync + 'static;   // audio/VFX/UI events since last drain (sent as ECS messages)
    fn submit_input(&mut self, input: &BridgeInput);                 // axes + queued edges + commands
    fn read_snapshot(&mut self, frame_time: Duration, out: &mut Self::Snapshot) -> SnapshotTick;
    fn drain_events(&mut self, out: &mut Vec<Self::Event>);
}
pub trait SimSnapshot: Default + Send + Sync + 'static {   // stored in a resource
    fn for_each_entity(&self, f: impl FnMut(SimEntityView<'_>));     // id (stable u32/u64), kind key, team, pose, extra
}
```

`SimBridgePlugin<S>`:
- `PreUpdate`: collect input from the action map → `submit_input` (never blocks; the sim samples on its
  own clock). `read_snapshot` with the frame time (the sim interpolates — the shell displays).
- **Entity reconciliation:** `SimEntityMap` (sim id ↔ `Entity`). New ids spawn the prefab registered for
  their kind key in a `KindRegistry` asset (`config/kinds.ron`: kind key → prefab, e.g. DO27 stem
  `"warhull"` → `prefabs/units/warhull.prefab.ron`); vanished ids despawn (optionally via a "death" prefab
  swap driven by events); poses write `Transform` directly (no Regia Engine-side interpolation unless
  configured). Per-kind `PresentationScript`s read the extra fields (damage zones, lean, turret yaw,
  team colour) to drive materials, sockets, animation parameters and audio parameters.
- **Events:** each drained event is sent as `SimEvent<S::Event>`; a data-driven `EventRouter` asset maps
  event kinds to VFX emitters, sound cues, decals, camera shakes, UI toasts and announcer lines, so most
  reactions need no code. *As built (ADR-7210): the bridge applies all six; effects and decals go where
  `ExternalSim::event_place` says (a position or a sim entity), variations follow a seed derived from the
  event stream, and every outcome is counted in `RoutedReactionStats`. As built (ADR-7490): a sound cue of
  a placed event plays spatially at that position (a default emitter, the seeded variation kept; an
  out-of-range position counts as `sounds_refused`), an unplaced one still plays flat. As built
  (ADR-7549): `SoundCue` takes an optional `max_distance` (meters; RON may leave it out) that replaces the
  default emitter's 50 m range, for battlefield-scale events heard from far away; without it the default
  range applies, and a value that is not finite and positive falls back to it.*
- **No feedback path:** nothing in the bridge writes to the sim except `submit_input`. Presentation is
  allowed to be nondeterministic.
- **Legged units (ADR-8474):** the sim owns the body and gameplay footsteps; `step_gait_driven` places presentation feet from the sim body each frame.
- **Coordinates:** the `ExternalSim` implementation converts the sim's axes and units to Regia Engine's
  (`02-architecture.md` § 3) in one place, covered by a test; nothing downstream converts again. For
  DO27 this is **not** the identity (corrected by ADR-8012; the earlier text said "close to the identity"):
  DO27's sim world is left-handed (+X east, +Y up, +Z north), so its shell reflects X at the boundary,
  `render = (−x, y, z)` for positions, offsets and velocities, rotations by quaternion conjugation
  `(w, x, y, z) → (w, x, −y, −z)` (a proper rotation, never a negative scale) and yaw `θ → −θ`; the reflection
  is its own inverse for picking rays. Its glTF meshes (`spec/art-scale.md`: metres, +Y up, forward −Z) are
  already right-handed and are not mirrored; only placement goes through the conversion. Do not assume the
  sim's zero-yaw heading either — at least one sim module (`ir-sim/src/climb.rs`) uses +Z as its forward — so
  the M3 test pins it with a known pose. The bridge's handedness setting with a winding flip is lane P-02's.
  *Built (ADR-8456, P-02): `SimBridgeSettings::handedness` = `SimHandedness::LeftHandedReflectX` applies exactly this
  reflection once in the bridge (snapshot poses of both published frames and `EventPlace::At`; the sim does not convert
  again), and `SimHandedness::RightHanded`, the default, converts nothing. `regia_framework::sim_coords` carries DO27's
  helpers and the winding flip (`mirror_mesh_winding`, `SimHandedness::mirror_mesh`) for meshes a sim authors in its own
  frame; placed meshes stay unmirrored as above. A per-kind blend policy (`KindEntry::blend`: lerp, snap, hold), a
  teleport distance and the presentation delay pose entities between the two published frames (`sim_blend`).*
- **Hot reload (ADR-015, option b):** the sim does not live in the reloaded game library. Its crate is
  built as its own dylib that links `regia_dylib`, loaded once. The handle sits in an engine-owned
  resource that hot-reload teardown never touches, and the game library holds only presentation.
  Editing the sim needs an editor restart, like engine code. A thread the *game* itself spawns still
  stops and joins in `on_unload` and restarts from the restored snapshot in `on_load` (`specs/20` § 4).
- *As built (ADR-8083): `regia_framework::sim_host::ThreadedSimHost` runs a `FixedStepSim` on its own thread at a fixed
  tick (or on granted ticks) and publishes the last two frames as one consistent pair (`prev`, `curr`, both ticks, both
  times, a sequence); it implements `ExternalSim`, so a game hands it to `SimBridgePlugin`. Agents read and drive it with
  `sim.host_status`, `sim.host_hold` and `sim.host_step`.*
- Frame-rate independence test helper: run the shell at 30 fps and uncapped; assert the sim-side input
  and network counters match within 2% (DO27 rule A1) — exposed as an agent-protocol check.
- Regia Engine CI uses `samples/sim_bridge` with a small deterministic mock sim (hover discs on a heightfield)
  so the feature is tested without the private DO27 repo.

## 4. The DO27 Regia Engine shell
- Lives in the DO27 repo under `shell-ferrite/` as a Regia Engine project (`02-architecture.md` § 10: a `game`
  library plus `launcher` and `editor`; the editor rebuilds `game` as a dylib for hot reload, and the sim
  is a separate never-reloaded dylib — `specs/20` § 1), depending on `ir-core` (path)
  and Regia Engine (git or path). The the legacy engine shell stays available as a reference and free test build until the Regia Engine
  shell passes the readiness gate (§ 6).
- Port inventory (from the owner's current the legacy engine shell, as requirements):

| Area | Requirement (Regia Engine feature used) |
|---|---|
| Cameras | Pilot chase with lag/lean (`compute_lean` from the view), pilot eye (first person), satellite overhead with build/order/place UI, spectator free camera, F1–F3 switching (`CameraRig`, spec 14) |
| Input | The owner's BZP-style keymap in one file, gamepad, Ctrl+category orders, number-key command menu (input contexts, spec 11) |
| HUD | Radar, minimap toggle, health, ammo, weapon rows, target box, build/order menus, scores, world markers, scale 1080p→4K + ultrawide, strings from the pack table (regia_ui data binding, spec 18) |
| Menus | Main menu v2, lobby/presets, onboarding, interactive tutorial, settings with graphics presets, splash/logo |
| VFX | Exhaust, dust trails, muzzle flash, tracers, impacts, explosions, scrap sparks, weather, smoke budget, mine arming, blast camera shake (VFX spec, event router) |
| Destruction | Cosmetic debris/collapse driven by sim events (pre-fractured meshes + rapier debris; never affects the sim). The rule is general, not DO27's alone (ADR-8841): in any game whose authoritative sim runs outside rapier (a lockstep sim, or any sim behind `sim_bridge`), rapier output is presentation, driven one way from the sim's snapshot and events (`regia_framework::sim_bridge::presentation`, ADR-8356); the sim reads no rapier pose, contact or event |
| Terrain | Outdoor heightfield from the same 16-bit PNG the sim reads (single source of truth — no exported landscape to diff), tunnel render meshes from the same collision triangles + detail, seeded scatter, craters/scorch decals, water for shoreline maps, time of day from map data |
| Audio | Event-driven SFX bank, per-vehicle engine/hover loops parameterised by speed/throttle, music stems mixed by match state, announcer/another team voice bank with rotation, callout pre-emption and 6 dB ducking (spec 16 + parametric patches), voice chat capture/playback, offline voice commands (Vosk via the `vosk` crate in the shell, not the engine) |
| Graphics | Presets Low…Max mapped to DO27 tiers (§ 6), HDR output, ultrawide FOV correction, VRR, uncapped frame rate, motion blur off by default, "clean" competitive preset |
| Tools | Plate capture (reference screenshots), handling telemetry overlay, showcase director/camera paths, log probe → all through the agent protocol (spec 23) |
| Steam | M19, after Gate C (spec 24) — not part of M17 or Gate C |

- **Asset production now (while the engine is being built)** — per `specs/12` § 4a:
  - Keep generating engine-neutral sources: meshes as **glTF** (switch Blender generators from FBX/USD to
    glTF export, metallic-roughness PBR, LODs as separate meshes, sockets as named empties), textures as
    PNG/EXR, audio as 48 kHz WAV, heightfields as 16-bit PNG.
  - Orientation needs no change in the generators: keep the current Blender frame (Z up, nose toward −Y)
    and export glTF with Blender's default +Y-up conversion. The file then has its nose on glTF's
    standard +Z front, and Regia Engine's importer turns it to −Z (`specs/12` § 7, ADR-009) — exactly DO27's
    mesh contract. Do not pre-rotate assets to compensate.
  - Don't author new shipping materials, VFX, UI or sound patches as the legacy engine assets; capture their intent as
    generator parameters and emit Regia Engine formats when each format freezes (M4 materials, M6
    prefabs/levels, M7 UI, M8 audio patches, M14 VFX).
  - Provenance records keep their DO27 fields; they move into Regia Engine `.meta` files unchanged.
- **Asset migration of existing content** (the generator pipeline makes this a retarget, not a rewrite): Blender generators
  export glTF instead of FBX/USD; material generators emit `.material.ron` using Regia Engine's layered
  standard material (metal/paint/dust/wear layers with masks); Niagara definitions → Regia Engine VFX RON;
  UMG widgets → `.ui.ron`; MetaSounds patches → Regia Engine audio patches — each re-emitted from the
  generators' own parameters, never by reading or converting the legacy engine asset files (`01-cleanroom.md`
  § 3.3); provenance records carry over into Regia Engine `.meta` provenance (spec 23 § 5) with DO27's extra
  fields.

## 5. Engine features DO27 pulls forward
Moved from Tier 2 into the main plan because DO27 needs them (see `05-milestones.md`): terrain and
tunnels, GPU VFX, the desktop high-end renderer track (GI, high-detail geometry, high-quality shadows,
volumetric atmosphere/clouds/fog, decals, upscaling, HDR output), layered materials, parametric audio,
voice capture, Steam, and networking: DO27 builds its multiplayer on `regia_net` (owner, 2026-11-15; ADR-7906):
client, listen and dedicated servers over raw UDP or Steam sockets, lobbies and the content handshake (specs 70-77 and
89, `book/user-guide/networking.md`). DO27 keeps its game rules, its simulation step and host migration, which the
engine does not do (spec 70 § 1). Not needed by DO27 (stays Tier 2 or lower priority): navigation/AI (in the sim),
Regia Engine physics beyond cosmetic debris.

## 6. DO27 readiness gate (Gate C)
Measured with DO27's own tiers (`DESIGN.md` → Presentation) on the benchmark maps, rendering recorded
replays from `ironreach_replay`:

| Tier | Hardware | Requirement |
|---|---|---|
| Floor | RTX 3060 Ti / RX 6700 XT | 1080p, 60 fps locked (p99 ≤ 16.6 ms) — **gating** |
| Mid | RTX 4070 / RX 7800 XT | 1440p, ≥ 90 fps — **gating** |
| Showcase | RTX 4080/5080 class | 4K 60 — best effort, not gating |
| Steam Deck | Deck | 800p, 30–40 fps stable with the Deck profile — gating for M19, measured here |

Plus: every row of § 4's inventory working (except Steam, which is M19); owner review of 20 replay plates against the visual target
(the legacy engine reference plates where they exist) rated "ready to ship"; rule A1 frame-rate test at 30 and
300 fps; Linux and Windows builds (frame-rate tiers measured on Linux; Windows verified in the owner's
Windows VM, `06` § 8); 30-minute 16-player soak without presentation errors.

Passing Gate C leads to M18 (stabilisation) and M19 (Steam), then DO27's Steam launch (Gate D).

## 7. DO27 interface contracts (v0.2.0)
ADR-301. DO27 builds its bake pipeline and its shell against the names below. Worked examples and the
full rules are in `book/user-guide/do27-interface-contract.md`; this section is the normative list.
DO27 owns the simulation, collision, navigation, its game messages and state (what it hands `regia_net` through its
`NetGame` types and `SnapshotEncoder`), host migration and the bake that writes the data. Regia Engine owns loading,
rendering, lighting, audio, picking and the network transport and session (`regia_net`, ADR-7906), and never writes
back into the simulation.

Status: **main** = in the engine code DO27 builds against (rows re-checked name by name against the v0.5.0 release
line, 2026-11-15); **lane** = built on a lane branch and not yet merged (none left: the five former lane items reached
`main` through the wave 1 to 3 integration merges; the lane names are kept as history); **proposed** = no code, names
may change until DO27 agrees.

| # | Interface | Names | Status |
|---|---|---|---|
| 1 | `<map>.holes.ron` | `HolesMask { holes, shapes }`, `HoleShape::{Ellipse, Rect, Polygon, Raster}`, `HOLES_FORMAT_VERSION` = 2 (named holes, ADR-1595; version 1 files load unchanged), `load_holes_ron`, `holes_ron_path`, `TerrainSharedDraw::set_holes`, `HolesMask::cut_heights` | main (file and loader: lane `v020i-terrain-holes`, merged) |
| 2 | Layers and picking | `regia_scene::MapLayer(pub u8)`, `regia_pick::{world_pick, world_pick_layers, PickFilter, LayerSet, WorldPick}`; agent `world.pick` | main (lane `v020i-worldpick-layers`, merged) |
| 3 | Runtime terrain edits | `TerrainHeightEdits`, `HeightEdit` (sample rectangle, absolute millimeters), `MAX_EDIT_SIDE`, `MAX_EDITS_PER_FRAME`, `HeightEditError::code`; under it `TerrainHeightPatches`, `TerrainHeightsChanged` | as built, stages 1 and 2 (ADR-1595, ADR-7585; lane `sprint-w9-do27-height-edits`); names still proposed until DO27 agrees |
| 4 | Exposure | `regia_render::ExposureZone`, `zone_weight`, `combine_zones`, `adapt_ev`; `regia_framework::exposure_zones::{ExposureZonesPlugin, sync_exposure_zones}` (in `DefaultPlugins`); `PostVolume` eye ranges join the same combine; `PostProcessVolume` does not exist | main (lane `v020m-exposure-zones`, merged; the frame samples zones since ADR-322) |
| 5 | Audio zones | `AudioReverbZone { preset, distance_scale, cavity, .. }`, `ReverbPreset::{Custom, Tube, MineGallery, Open}`, `ReverbZoneShape::Capsule`, `AudioZonePortals`, `AudioRockField` | main (lane `v020i-audio-zones`, merged) |
| 6 | Indoor occlusion | `VisCell(u16)`, `<map>.viscells.ron` v1 (cells, portal quads), `VisCells`, `VISCELLS_FORMAT_VERSION` = 1, `load_viscells_ron`, `viscells_ron_path`, `audio_zone_mouths`; agent `render.viscells`, `render.set_viscells` | lane, stage 1 (`sprint-w9-do27-viscells`, ADR-7586), stage 2 (`sprint-w10-do27-viscells-stage2`, ADR-7673) |
| 7 | Steam | `Steam::client() -> Option<steamworks::Client>`, `SteamStatus`; `steamworks` 0.13.x | main (lane `v020j-steam-core`, merged) |
| 8 | Object keys | `FrameMesh::object_key` | main |
| 9 | Voxel boundary | `regia_voxel_core` (integer-only API, `.voxfield` v1; spec 122, ADR-8116; `.fvox` is the cooked voxel far proxy, ADR-2445) | proposed (no crate) |
| 10 | Deterministic sim kit | `regia_detsim` (`math` on `libm =0.2.16`, tick constants, desync hash, deterministic ids and broadphase, hover step); `cargo xtask det-scan` | proposed (ADR-8012 A-4, A-5; § 7.12) |
| 11 | World geometry for the sim | `regia_world_geom` (GPU-free, ECS-free query DO27's sim links; shape in lane D-03's spec) | proposed (ADR-8012 A-5; § 7.12) |

### 7.1 Rules that apply to every interface
- A data file has an integer `version` (absent = 1). A newer version than the engine's is refused, never
  guessed at. Adding an optional, defaulted field keeps the version; any other change bumps it and ships a
  migrator that reads every older version (spec 12 § 4a), so DO27 never has to re-bake for an engine update.
  `cargo xtask check-formats` guards the field names.
- A change to any name, field, unit or format in the table needs an ADR, the version bump and migrator for
  a file, a host-checklist diff in the release notes, and DO27's agreement before the release that carries
  it. **proposed** rows change freely until DO27 agrees.
- Regia Engine adds nothing to host-built structs (`ExtractedMesh`, `FrameMesh` and similar) that breaks a
  struct literal. A new field on a public struct is listed in the host checklist.

### 7.2 `<map>.holes.ron`
A serialized `HolesMask`, at most **64 shapes** (ellipses included), version 2 (version 2 adds named holes,
`book/agents/terrain-runtime.md`; a version 1 file loads unchanged). The file is the map name
up to its first dot plus `.holes.ron`, next to the map; none means no holes. Fields: `version`, `holes`
(legacy ellipses), `shapes` (`rect(center, half_extents, rotation_radians)`, `polygon(points)`,
`ellipse((center, radius, rotation_radians))`, `raster((center, half_extents, rotation_radians, width,
height, coverage))`). Coordinates are world XZ metres with the terrain centered on the origin; polygons
fill even-odd; a raster is inside where bilinear coverage reaches 50 % and is at most 2048 texels a side.
The shell loads it (`load_holes_ron`), applies it to the draw (`set_holes`) and to its own physics heights
(`cut_heights`). DO27 bakes the footprints and the lip meshes; Regia Engine cuts the terrain in every pass and
in the probes and the path tracer, and rays pass through (`raycast_with_holes`, `terrain.raycast`).

### 7.3 `MapLayer(u8)` and `world_pick`
Terrain is layer 0; an entity without `MapLayer` is layer 0; a descendant takes the nearest ancestor's.
DO27's bake assigns a layer id per entity (an id, not a height band). `world_pick(world, ray, &filter)`
returns the nearest `WorldPick { layer, entity, point, normal, distance, surface, triangle }`;
`world_pick_layers` the nearest per layer. The terrain march passes through holes. The picking code moved
from `regia_editor` into the new `regia_pick` crate.

### 7.4 `TerrainHeightEdits` (as built, stages 1 and 2; names proposed until DO27 agrees)
DO27's deterministic grid is the truth and sends sample rectangles of the render map's grid: `HeightEdit
{ col, row, width, height, heights_mm }`, absolute integer millimeters, row-major (row 0 = the −Z edge).
Absolute replace, applied in push order, coalesced per frame; at most 128 × 128 samples a rectangle and 16
rectangles a frame (to be set by the cost spike). Stage 1 (v0.2.0) is the handle, regional CPU refresh and
no cracks; stage 2 (v0.2.x) narrows the virtual-texture, cascade, probe and scatter invalidation, and is
the gate for "no frame over budget".

As built (ADR-1595 stage 1, ADR-7585 stage 2): `TerrainHeightEdits::new(draw.height_patches_handle())`
and `push(HeightEdit)` → patch id. Millimetres become the map's 16-bit samples in `f64`; refused with codes
`edit_too_large` (a side over `MAX_EDIT_SIDE` = 128), `edit_frame_full` (a 17th push before the next applied
frame, `MAX_EDITS_PER_FRAME` = 16), `edit_out_of_range` (outside the map's height range: refused, not
clamped), or the `patch_*` code of a rectangle, length, target or queue error. Edits share the patch
queue's per-frame sample budget (`TerrainHeightPatches::set_budget`, default 65 536 samples): every edit
pushed before a frame's apply is taken in that frame in push order unless the budget spreads it over the
next frames. An applied band invalidates: the scatter instances on the changed ground (`ScatterField::heights_changed`,
the rest bit-identical), only the sun cascades its box lands in, the grass selection only when the box is in the
grass range, the virtual-texture pages its own rectangle meets, and the texels of the probe volumes'
heightfield max pyramid its changed samples reach (ADR-7659: the draw hands the frame's changed sample
rectangles to the probes in `HeightfieldInfo::edits`; at most 16, more merge into their bounding rectangle; a
missed frame, holes, a host's `update_heights` or a new map rebuild the whole pyramid; bit-identical to a full
rebuild; one 13 × 13 crater on a 513-sample map re-reduces 278 of the pyramid's 349 525 texels (0.08 %), counted in
`render.get_dynamic_gi` `renderer.pyramid`). GPU frame time, measured (`probe_pyramid_edit_frame_gpu_cost`,
terrain draw and probe update at 1280 × 720 on a 4097-sample map, one 128 × 128 edit against none, medians
of 24 pairs; RTX 5090, 2026-11-14): before ADR-7659 0.853 ms without an edit, 1.135 ms with one (+0.28 ms, of
which the probe update +0.20 ms); after, 0.859 ms and 0.944 ms (+0.085 ms; probe update +0.025 ms). Not measured
yet on a DO27-class mid-range card (the same test runs there unchanged). Known issue: a scatter instance re-sat on a sloped crater keeps the tilt of the
old ground (`ScatterField::heights_changed` moves it vertically only; its slope alignment is not redone; the ignored
test `a_resat_scatter_instance_on_a_sloped_crater_takes_the_new_tilt` measures 14° off on a 14° crater wall).
Agent: `terrain.patch_heights { heights_mm }`.

### 7.5 `ExposureZone`
An oriented box or a Y-axis capsule with `ev_min`, `ev_max`, `blend_distance` (outside the surface),
`adapt_up_speed` and `adapt_down_speed` (EV per second, defaults 2.0 and 1.5) and `priority`. The weight
is a Hermite smoothstep from 0 at `blend_distance` outside to 1 on the surface; the eye chases the
midpoint of the combined range; `adapt_ev` never overshoots. The frame samples the zones (ADR-322):
`ExposureZonesPlugin` (in `DefaultPlugins`) runs `sync_exposure_zones` in `PostUpdate`, weighs every zone at the
active camera (the window camera with the lowest `order`) and calls `RuntimeRenderer::set_exposure_target`; a host
without `DefaultPlugins` adds the plugin or calls `set_exposure_target` itself. A `PostVolume` with an `eye` range
joins the same combine (ADR-1464, spec 30 § 4). The type has no serde or reflection yet, so the shell spawns zones
from code. `PostProcessVolume` (spec 13 § 9) is not built and is not planned under that name; `PostVolume` is the
volume that exists.

### 7.6 `AudioReverbZone`
Presets: `Tube` (send −3 dB, feedback 0.94, distance scale 1.5, cavity), `MineGallery` (−5 dB, 0.88, 1.5,
cavity), `Open` (−18 dB, 0.35, 1.0). Overlapping zones blend, least specific first. `distance_scale` makes
sound shared inside a zone carry that much further (clamped 0.1 to 10; the zone's share is the lower of the
two memberships). Rock occlusion needs the shell to install `AudioRockField` (ground height, `None` in a
hole); the bake lists each cavity's mouths in `AudioZonePortals` (zone-local), or sound is occluded by
thickness alone. See spec 16 § 3d.

### 7.7 `VisCell` and portals (proposed)
`VisCell(u16)` on an entity; cell 0 is outdoors. `<map>.viscells.ron` lists cells (oriented-box bounds)
and convex portal quads between cell pairs. Regia Engine traverses per view and skips unreachable cells; a
debug draw ships before the culling. v0.2.x.

**As built (stage 1, ADR-7586, the v0.6 line).** `regia_scene::components::VisCell(pub u16)` is a
reflected component. Stage 1 read it on the drawn entity itself; since stage 2 an entity without it
takes its nearest tagged ancestor's (as `MapLayer`, § 7.3), and an entity with none up its chain
always draws. `regia_render::viscells` owns the file, the walk and the cull.
- *File.* `<map>.viscells.ron` version 1 (absent = 1, newer refused before the body is read; format
  `viscells` in `check-formats`, `regia upgrade-project` knows v1, no migrator yet): `cells: [(id,
  name?, outdoor?, bounds: [(center, half_extents, rotation_radians?)])]`, `portals: [(a, b, quad:
  [4 points])]`. Cell 0 is implicit, has no bounds and is the only one that may say `outdoor`.
  Validation refuses a duplicate id, an indoor cell without boxes, a bad box, a portal to an
  unlisted cell or to itself, and a non-finite, non-planar (over 1 % of the longest diagonal, at
  least 1 cm) or non-convex quad, each with an error code (`book/agents/viscells.md`).
- *Walk.* Per extracted view, from the cell holding the eye (smallest box on overlap). An eye in
  no box but in a portal's doorway (over the quad, within the doorway depth of its plane: the wall
  gap the two cells' boxes leave around the portal, the larger side's, derived at load, plus the portal tolerance)
  starts in both of that portal's cells, the union of what each sees (ADR-7639); in no box and no
  doorway it is in cell 0. Classic portal culling (Luebke and Georges 1995), starting with the view frustum less its near
  plane (a door nearer than the camera's near distance still leads on): a portal whose quad, clipped
  to the current frustum, keeps area makes the cell behind it reachable, with the frustum narrowed to the clipped
  quad. At most 32 portals a chain and 4096 cell entries a view; a chain at 32 that still sees a
  portal onward counts every cell connected beyond it reachable (ADR-7639), and at the visit cap
  every cell counts as reachable. More than 16 planes, or the eye within 2 cm of a portal's plane, keeps the wider
  frustum. Allocation-free once warm.
- *Cull.* `cull_unreachable_cells` runs in `Render` after the mesh LOD (so after the ADR-7200
  visibility filter) and before `update_view_visibility`. The reached set is the union over the
  frame's views. A draw of an unreached cell leaves the colour pass; stage 1 does not cull shadow views:
  a caster stays as a shadow-only draw (the mesh-LOD shadow tag), so unreachable cells still cast
  into the sun's cascades unless the shadow-view cull below (ADR-8874) takes them out; `ViewVisibility` ignores shadow-only draws, so a culled entity reads false. A
  `VisCell` naming an unlisted cell draws.
- *Debug draw.* Gizmo group `viscells` (off by default): cell boxes green (reached last frame), red
  (not) or yellow (the first view's cell), portals cyan. Agent: `render.viscells` (cells, portals,
  per-view eye cell, reached ids, visits, cull counters), `render.set_viscells { text?, clear?,
  cull?, debug_draw? }`.
- *Stage 2 (ADR-7673).* *Inherited cells:* resolved in the cull from a map of untagged
  descendants to their nearest tagged ancestor's cell, rebuilt only on a frame where a `VisCell` or a
  `ChildOf` was added, changed or removed (no per-draw parent walk). *Local lights:* `extract_frame`
  records each point and spot light's entity (`ExtractedFrame::local_light_entities`); the cull
  drops a light whose cell is unreached and whose range sphere touches no reached cell (a box of a
  reached indoor cell; with cell 0 reached, anything outside one box of its own cell), compacting
  the light extras' IES and LTC tables with it, so it neither shades nor gets a shadow-map update.
  The range test keeps every light that could light a reached cell's surfaces. *Glass:* the cull
  publishes its culled entities (`VisCellCulled`); `extract_glass` adds no instance for them.
  Morphs, mask decals and the side tables are built from the draws after the cull and follow it.
  Counters: `cell_lights`, `culled_lights`, `culled_glass`. *Audio:* `audio_zone_mouths` turns
  the portals plus a cell-to-zone map into the zone-local mouths of § 7.6. *Shadow-only key:* a
  culled caster's shadow-only draw is one more mesh-cache entry, uploaded once on the first exit and
  kept while resident (no re-upload on later walks; `walking_in_and_out_of_a_room_ten_times_uploads_no_mesh_again`).
- *Bake (W-21, ADR-8371).* `regia terrain cook` writes this version-1 file from the mesh-layer sections
  (cell 0 outdoors, one cell per section, quads at mouths and cuts). The loader is unchanged.
  `viscell_leak`, `viscell_bad_portal`, `viscell_bad_box` and `viscell_too_many` refuse a bad bake.
- *Deferred.* Shadow-view culling beyond the sun's cascades (ADR-8874 built the sun's, below;
  local-light shadow maps of culled lights are already skipped), per-view draw lists,
  fog's local lights (picked inside `extract_frame` from copies), emissive fitted lights of a culled
  draw kept as a shadow caster (skipping shadow-only draws there also changes mesh-LOD shadow
  variants), procedural instance sets and quick-draw meshes (no ordering edge with the cull;
  quick-draw entities are synthetic), water, VFX particles and decals (other crates' layers).
- *Terrain and scatter (W-22, ADR-8413).* A second walk of the first window view, its frustum widened by
  the terrain's 2 px edge slack, records the narrowed frustum of
  each entry into cell 0 (at most 64) in `ExtractedFrame::outdoor_portals` (`OutdoorPortals`, plus a
  bounding sphere per mouth of a reached cell). The terrain draw leaves out of its camera passes the
  heightfield nodes whose box is more than 0.5 m outside every frustum
  (`FrameRenderer::set_outdoor_portals`, `SharedPassDraw::set_outdoor_portals`); the runtime scatter
  leaves out instance blocks outside every frustum after widening by radius and shadow reach, except
  caster blocks within shadow reach of a mouth (`ScatterField::extract_meshes_through`). Unrestricted
  (identical draws) with the eye outdoors or in a doorway to cell 0, on a capped walk, past 64 entries,
  and on a frame whose eye differs from the list's. Sun cascades are untouched (W-23). Counters:
  `render.stats` `interior`.
- *Grass, impostors, water, split-screen (v0.7 item E, ADR-8843).* Every window view gets its own list
  (`ExtractedFrame::outdoor_portal_views`), handed to the frame before that view is drawn;
  `ExtractedFrame::outdoor_portals` is their union (restricted only when every window view is) for
  the shared scatter list. The grass drops the camera blades of patches outside every frustum
  (shadow patches keep casting, `PATCH_NO_CAMERA`), the impostor cull skips the slots of tiles
  outside every frustum, and the water layer (`TranslucentLayer::set_outdoor_portals`) leaves out
  lake and river bodies and ring-grid chunks outside every frustum.
- *Shadow views per cell and the terrain's cover (v0.7 item E, ADR-8874).* After the seal, a rigid
  sun caster of an unreached cell whose world box, swept along the sun's travel and grown by 1 m plus
  2 cm per metre, touches no reached cell's box and no recorded outdoor frustum gets
  `SUN_SEALED_SHADOW` (`VisCellShadowViews`, default on; stands down while any view sees the outdoors
  unrestricted). Every recorded frustum counts, also one only the two-pixel edge slack reaches; a
  window view without a restricted list, or a texture view that sees outdoors through its slack,
  stands it down (ADR-10161). `regia_terrain::TerrainSunCover` (opt-in) fills `SunCover` from the
  min-max pyramid over each box's swept footprint grown by two root shadow-grid cells, and from the
  drawn filtered shadow mesh of the levels that read further (ADR-10161), holes and lattice cuts as gaps.
  Counters: `VisCellStats::shadow_view_*`, `TerrainSunCoverStats`.

### 7.8 `Steam::client()`
The engine owns the one `steamworks::Client`. **The shell must not call `Client::init`.** It takes
`Steam::client()` (a clone, `None` when Steam is not running), depends on the same `steamworks` 0.13.x as
`regia_steam`, never pumps callbacks (the engine runs them once per frame in `PreUpdate`), and drops its
clones before exit. Networking is `regia_net` (ADR-7906): over Steam, `regia_steam::SteamSocket` carries its
datagrams on Steam's relay network and `SteamLobbyPlugin` backs its lobbies, both on the same shared client.

### 7.9 Object keys
`object_key = (entity_id << 8) | part_index`, `entity_id` below 2^48, `part_index` 0 to 255. The key is
below 2^56, stable for the entity's lifetime and never reused within a match. Never use a key with top
byte `0xFE` (`EPHEMERAL_KEY`) or `0xFF` (`SHADOW_LOD_MESH_TAG`). **Key 0 is ephemeral**: the renderer
substitutes `EPHEMERAL_KEY | index in the frame's mesh list`, so previous-frame matrices and palettes
(motion vectors) pair by list position and mispair after any spawn, despawn or reorder. Objects that
never move (scatter) may keep 0.

### 7.10 `regia_voxel_core` (proposed)
A pure-Rust crate, no GPU dependency, linked directly by DO27's simulation: a sparse integer signed-distance
field (32³ bricks, `int16` distance clamped to ±4 voxels, `u8` material) and a fixed-point CSG op log that
replays to a bit-identical field on any machine and thread count. A frozen cooked format of its own with a
migrator (not `.fvox`: that extension is now the cooked voxel far proxy, ADR-2445). Regia Engine meshes the same field for rendering. Public API is integer-only and semver-versioned;
**DO27 signs off on every API change.** Memory gate for the spike: at most **1.5 GB resident voxel memory
per map on HIGH** (lower for Deck and LOW, to be set with the owner); a go or no-go gate. The spike
follows runtime height edits stage 2 and the layer pick.

**Design: spec 122 (`docs/specs/122-voxel-core.md`, ADR-8116, proposed, 2026-11-16).** It turns this section into
the API DO27 signs, and changes three of its starting points with measurements: bricks are 8³ without an apron (32³
fails the gate on the Switchbacks retrofit), the file is `<map>.voxfield` (`.fvox` is the voxel far proxy's), and the
memory cap is per map for every preset (lockstep peers hold the same field; only render meshes scale). The sim
reaches the field through `regia_world_geom` (spec 121) behind a `d2_voxels` feature.

### 7.11 Acceptance tests
Every interface has a test that runs on lavapipe without DO27 maps (synthetic scenes): holes
`terrain_holes_flythrough`, `terrain_holes_gi`, `regia_pt/terrain_holes`; pick `regia_pick/world_pick`;
audio `regia_audio/zones`; exposure `scripted_tunnel_drive_has_no_flash_or_blackout` (the math) and
`regia_framework` `exposure_zones::tests::the_target_is_set_and_cleared_as_the_camera_crosses` (the frame hook);
runtime terrain edits `regia_terrain/terrain_height_edits`, and under them the `regia_terrain` `runtime_patch` tests
(`bands_follow_the_budget_in_queue_order`, `patched_heights_are_what_a_reload_reads_whatever_the_budget`) and
`regia_agent` `terrain_runtime::tests::patch_heights_queues_and_the_system_applies_it`; Steam `regia_steam/shared_client`
(mock backend). The worked examples in the book page are parsed by
`crates/regia_terrain/tests/do27_contract_examples.rs`. The proposed interfaces get theirs when built.
Not verified: audio on a real device, GPU timings, Android.

**Stacked floors** (DO27 ask 11, ADR-7661): `regia_pt/tests/stacked_floors.rs` builds a closed three-storey block
(0.3 m slabs, 3 m storeys, one window per storey) and switches a light on the top storey only (an emissive ceiling panel,
a shadowed point light) or the sun; for the baked probe volume, DDGI at the Low and High tier layouts (with SSGI), the
frame without probes (emissive area lights, local shadows) and the sun cascades, the lower storeys' change over the lit
storey's, measured against never-switched renderers at the same frame, must stay under the path tracer's own leak plus
three standard deviations of its noise plus 0.005. A sixth test holds the camera's storey among the four interior volumes
the fog and the aerial perspective read when there are more. Not covered: the lightmap path, real-card timings.

### 7.12 `regia_detsim` and `regia_world_geom` (proposed, ADR-8012)
Two pure-Rust crates that DO27's simulation links directly, so DO27 can retire its own copies (owner 2026-11-16).
Neither depends on an ECS, GPU or platform crate; a headless server and DO27's sim link them alone. Public API is
semver-versioned and, as for `regia_voxel_core`, **DO27 signs off on every API change**. Lockstep code follows the
determinism policy of ADR-8012 A-4: f32 with IEEE `+ − × ÷ sqrt` in a fixed order, no SIMD types, no `mul_add`,
transcendentals only through `regia_detsim::math` (`libm =0.2.16`, DO27's exact bits), float `min`/`max`/`clamp` only
through `regia_detsim::float` (zero and NaN sign are not portable; ADR-8085), glam's scalar types without the functions that
call std trig, integer millimeters for cooked geometry, portals, height edits and ORCA, and `cargo xtask det-scan` over the
lockstep closure. Sim-critical code is
copied from DO27 bit-identical with DO27's tests and pins in Stage A; deliberate bit changes wait for Stage B and one
sim-version bump (owner 2026-11-16, timed with the T3 swap). DO27 switches per feature: Regia lands it with DO27's tests
and a pin, DO27 swaps its module on a branch and runs its own pins, and deletes its copy only when they match. DO27 may
pin a v0.6 integration commit before the v0.6.0 tag to do so (owner 2026-11-16).
