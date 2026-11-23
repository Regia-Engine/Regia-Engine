# 78 — World streaming runtime

Status: proposed (ADR-1804; ADR-1888 records how the build refined §§ 8.1, 9.1, 11.1, 13.1 and 16; ADR-2022 records the second pass: flat-color proxies, the opt-in cross-fade, the rebase reach of § 12.2 with terrain and baked GI in their own space, rebase blockers, the `nav` gate hook and the net wire frame of § 14). Plan items: `22-tier2.md` § T2.11 (world streaming), `master-plan.md` § 3.4 (b) ("world
streaming start": grid cells with streaming sources, async load/unload with per-frame budgets, HLOD proxies, large-world
coordinates, the teleport hitch bench), § 5.1a (VRAM, streaming and CPU budgets per tier), § 5.3 item 10 (teleport bench);
gap audit 10-30 rows "Streaming runtime" and E1. Rungs: v0.3.x (b) start (sources, cooked cells, budgeted load/unload,
HLOD, floating origin, the 8 km² acceptance), v0.4.0 finish (every subsystem hook in place, the teleport bench green on
the measured budgets, `docs/versions.md` "world streaming finished"). Crates: `regia_world` (new, the runtime),
`regia_scene` (authoring types, cooked cell data, the existing data model), `regia_asset_pipeline` (partition and cook),
`regia_physics`, `regia_render`, `regia_terrain`, `regia_nav`, `regia_audio`, `regia_vfx`, `regia_agent`, `regia_cli`.

Builders read this file, `14-scene-and-framework.md` (levels, A2 cooked scenes, A4 level lifecycle), `37-per-entity-levels-layers-instances.md`
(§ 6.3 streamed placed levels, § 7 the streaming contract), `12-assets.md` (§ 4 cooked header, § 4a freeze rules, § 5b
load sets and pak streaming), `40` (content chunks; not the same thing as streaming cells, § 2), `66-tiled-and-runtime-generation.md`
(§ 4 anchors, whose scheduler rules this spec shares), `23-agent-native.md` and `book/agents/world-streaming.md` (the data
model's agent page). The lane split is in `docs/notes/world-streaming-lanes.md`.

## 1. Purpose and scope

A level larger than what fits in memory, or larger than what can be drawn in full, is split into square **cells** on the
ground plane. The cook writes each cell's entities as a separate cooked blob plus one index for the world. At run time,
**streaming sources** (cameras, players, scripted points) pull nearby cells in and let far cells go, within a per-frame
time budget and a memory budget, without ever blocking the frame on IO. Distant cells are drawn as **HLOD proxies**.
Positions stay precise anywhere in the world through a **floating origin**: the engine keeps `f32` positions relative
to an origin cell and shifts everything by a whole number of cells when the focus moves far away.

This spec turns the ADR-1566 data model (`regia_scene::{stream_grid, stream_plan, hlod, floating_origin}`), which nothing
calls today, into a runtime. It fixes:

- the authoring settings and components (§ 5) and the cooked data with its versioning (§ 6);
- streaming sources (§ 7), scheduling, priorities and budgets (§ 8), async IO and cancellation (§ 9);
- spawning, cell membership, readiness gates and the physics guard (§ 10);
- the HLOD handoff (§ 11) and floating-origin integration with every subsystem that holds positions (§ 12);
- the terrain and nav tile hooks (§ 13), multiplayer (§ 14) and determinism (§ 15);
- the API (§ 16), the agent surface (§ 17), tests and acceptance (§ 18), platforms (§ 19) and budgets (§ 20).

A level without streaming settings loads exactly as today: every entity spawns on open (§ 6.5).

## 2. Terms

| Term | Meaning |
|---|---|
| **Streamed level** | A level whose `LevelStreaming` resource (§ 5.1) has `enabled: true`. |
| **Grid** | `regia_scene::stream_grid::StreamGrid`: square cells on X/Z, `cell = floor(p / cell_size)`, `CellCoord { x: i64, z: i64 }`, half-open `[i·s, (i+1)·s)`. Y is not part of addressing (2.5D). |
| **Cell blob** | The cooked entities of one cell (`.wcell`, § 6.3). One per non-empty cell. |
| **World index** | The cooked description of a streamed level (`.wstream`, § 6.2): grid, cells, costs, HLOD tree, the always-loaded set. |
| **Always-loaded set** | Entities of a streamed level that are not partitioned (§ 5.3); they spawn on level open like an ordinary level. |
| **Streaming source** | An entity or a scripted point that wants cells near it (§ 7). |
| **Source view** | One source's sample for a planning step: world position (`f64`), optional view direction, radius scale, priority. |
| **Residency state** | Where a cell is in its life (§ 8.1). |
| **Gate** | A named readiness check a subsystem registers; a cell is `SimReady` or `Visible` only when every gate of that scope says so (§ 10.3). |
| **HLOD node / proxy** | A node of `regia_scene::hlod::HlodTree`; its proxy is a cheap mesh standing in for all cells under it (§ 11). |
| **Origin cell** | The cell of `regia_scene::floating_origin::FloatingOrigin`; local space is `f32` relative to its center. |
| **Rebase** | A whole-cell shift of local space (`floating_origin::Rebase`), applied to everything that holds a local position (§ 12). |
| **Step** | One fixed simulation step (`regia_app` fixed time). Streaming decisions happen at step boundaries (§ 15). |

**Cells are not content chunks.** Spec 40 "chunks" are delivery units (paks a player downloads or installs). A cell blob
lives inside some chunk's pak like any cooked asset; the chunk rules (`regia_asset_pipeline::chunks`) assign it. A cell
whose chunk is not mounted fails with `cell_unavailable` (§ 8.6).

## 3. Built baseline

Built and used:
- `regia_scene::stream_grid` (`StreamGrid`, `CellCoord`, labels `"<x>_<z>"`, `distance_to_cell`, `cells_within`).
- `regia_scene::stream_plan` (`StreamPlanner`, `StreamConfig { load_radius, unload_radius, memory_budget_bytes,
  max_loads_per_step }`, `CellInfo { bytes, priority }`, `plan(viewers) -> StreamPlan { unloads, loads, deferred,
  resident_bytes_after }`, `apply`, `set_loaded`; hysteresis, priority-then-distance-then-coordinate order, hard memory
  bound with `MakeRoom` eviction, `TooLarge`, `OverBudget`, `RateLimited`; `tracing` span on `plan`).
- `regia_scene::hlod` (`HlodTree::build`, `select`, `switch_distance = r / (threshold · tan(fov/2))`, every leaf covered once).
- `regia_scene::floating_origin` (`LargeSpace`, `LargePosition { cell: [i64; 3], local: [f32; 3] }`, `FloatingOrigin::update`
  returning `Option<Rebase>`, whole-cell shifts exact for integer cell sizes up to 2^24 m).
- Agent methods `scene.stream_plan`, `scene.hlod_select`, `scene.large_position` (stateless, `read`), page `book/agents/world-streaming.md`.
- `regia_scene::level` (`LevelManager`, additive levels, `LevelMember`, `Persistent`, `LevelLoaded` messages),
  `regia_scene::cooked` (`CookedScene` v1, `cook_scene`), `regia_scene::{level_dir, layers, placed}` (spec 37: guid-addressable
  entity records, layer membership, streamed placed levels).
- `regia_asset::pak_stream::PakStreamer` (background per-asset reads from mapped paks, read-ahead, bounded output channel,
  failures marked in `LoadSets`; **no cancellation**), `regia_asset::load_set` (`LoadSets`, residency and progress),
  `regia_render::memory_budget` (tier budgets, admission), `regia_render::upload_queue` (per-frame upload budget),
  `regia_render::texture_stream` and `residency_feedback` (texture mip streaming, T2.12).
- `regia_terrain::stream` (`TileCache`: byte budget, distance-ordered requests, background loader; `CookedTiledHeightfield`).
- `regia_nav::tile_cache` (`NavTileCache`: tiles rebuilt around obstacles; built from one triangle soup, not streamed).
- `regia_core::tasks` (`ComputePool`, `AsyncComputePool`, `IoPool`, `spawn_io`, spans per task).
- `regia_asset::server::PayloadReader` (a cloneable reader of cooked payloads usable off the main thread) and
  `AssetServer::unavailable_chunk` (spec 40).
- Spec 66's anchor scheduler (`regia_procgen::tiles`) is **not** on this base: it is on an unmerged lane branch (b20).
  § 7.4 is a contract for when it merges.

Not built (this spec): the runtime crate, streaming sources, cooked cells, the partitioner, residency states, gates,
the physics guard, the HLOD draw handoff and proxies, the rebase system and every subsystem's rebase handling, the
terrain and nav hooks, the `stream.*` agent methods, the sample and the bench. Also missing from existing code and
needed here: a **batched, remapping spawn** of a cooked scene (`CookedScene::spawn` spawns everything in one call on
`&mut World` and decodes payloads during the spawn; § 10.1 needs a range spawn with a root-transform remap), a
`StreamPlanner` way to hold failed cells back (§ 8.6), and a `Resource` wrapper for `FloatingOrigin` (it is a plain
struct today).

## 4. Architecture

### 4.1 Crate placement

```
regia_scene    authoring types (§ 5), messages and gate types (§ 10.2, § 10.3), the existing grid/planner/HLOD/origin
               model; no asset types                              (depends on core, ecs, app: unchanged)
regia_world    NEW: the runtime (sources, scheduler, IO, apply, guard, HLOD cut, rebase driver, agent bridge data)
               and the cooked types `regia_world::format::{WorldIndex, CellBlob}` (§ 6), which use `AssetId`
               depends on regia_core, regia_ecs, regia_app, regia_asset, regia_scene
regia_physics, regia_render, regia_terrain, regia_nav, regia_audio, regia_vfx
               each registers its gates and handles CellLoaded / CellUnloading / OriginShifted (§ 10, § 12, § 13);
               they depend on regia_scene (types and messages), never on regia_world
regia_framework / regia   add `WorldStreamingPlugin` to the default plugin set (inert without a streamed level)
regia_asset_pipeline      partitioner and cook (§ 6.4), HLOD proxy build (§ 11.1); depends on regia_world for the
                          cooked types (the pipeline is desktop-only, so this edge is allowed)
regia_agent               `stream.*` methods (§ 17) through a bridge resource, like the other runtime surfaces
```

The subsystems talk to the runtime only through `regia_scene` messages and the `StreamGates` resource, so the dependency
graph stays acyclic and a headless server builds `regia_world` without `regia_render`. `regia_world` is a new workspace
member, not a third-party dependency; its row goes in `docs/03-workspace-and-dependencies.md` under the crate list.
New internal edges (none makes a cycle): `regia_terrain → regia_scene` (terrain does not depend on it today),
`regia_asset_pipeline → regia_world`, and `regia_world → regia_asset, regia_scene`.

### 4.2 Data flow

```
sources ─▶ source views ─▶ planner (StreamPlanner, per step) ─▶ scheduler queue ─▶ IO (CellSource on IoPool / PakStreamer)
                                                                                     │ cancel tokens, generations
                                                                                     ▼
            HLOD cut ◀── gates (render, physics, terrain, nav) ◀── apply (main thread, budgeted) ◀── decode (AsyncComputePool)
                                                                       │
                                                         CellLoaded / CellUnloading messages
```

### 4.3 Frame order

System sets in `regia_world::StreamSet`, in this order:

| Set | Schedule (async) | Schedule (lockstep) | Work |
|---|---|---|---|
| `Sources` | `FixedUpdate`, after gameplay movement and the physics step | same | sample every source into `StreamViews` (§ 7.3) |
| `Rebase` | `FixedUpdate`, right after `Sources` | same | `FloatingOrigin::update(focus)`; on a rebase, shift everything (§ 12) and write `OriginShifted` |
| `Plan` | `FixedUpdate`, after `Rebase` | same | run the planner when a trigger fired (§ 8.2); queue loads, cancel unwanted work (§ 9.3) |
| `Complete` | `PreUpdate` (once per frame) | `FixedUpdate`, after `Plan` | take finished decodes (§ 9.2); in lockstep, in issue order and waiting as § 15.2 says |
| `Apply` | `Update`, before user systems that read cell content | `FixedUpdate`, after `Complete` | spawn and despawn within the apply budget (§ 8.4); write `CellLoaded` / `CellUnloading` |
| `Guard` | `FixedUpdate`, before the physics step | same | freeze or wake bodies near non-ready cells (§ 10.4); reassign moved members (§ 10.5) |
| `Hlod` | `PostUpdate`, after transform propagation, before render extract | same | the HLOD cut and its repair (§ 11.2), cell render visibility |

`FixedUpdate` runs zero to N times per frame, so in async mode `Complete` and `Apply` run per frame with a time budget,
and in lockstep mode they move into the fixed step with an entity budget per step: there, every change to the
simulated world is tied to a step number. A rebase never happens inside a physics step. "The next run of `Apply`"
(used by § 10.6) is the next frame in async mode and the next step in lockstep mode.

## 5. Authoring

### 5.1 `LevelStreaming` (level resource)

```rust
/// Streaming settings of a level, stored in the `.level.ron` resources map
/// under `regia_scene::streaming::LevelStreaming`. Absent = not streamed.
#[derive(Resource, Reflect, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LevelStreaming {
    pub version: u32,               // 1
    pub enabled: bool,              // false
    pub cell_size_m: f64,           // 128.0; 8.0..=4096.0
    pub load_radius_m: f64,         // 512.0
    pub unload_radius_m: f64,       // 640.0 (>= load_radius_m)
    pub max_entity_extent_cells: f64, // 2.0: entities wider than this many cells go to the always-loaded set (§ 5.3)
    pub hlod: Option<HlodSettings>, // Some(default) when enabled; § 11
    pub origin: OriginSettings,     // § 12.1
    pub budgets: Option<StreamBudgets>, // None = the platform defaults of § 20 (StreamBudgets::for_platform())
    pub tuning: StreamTuning,
}

/// Behavior knobs that are not budgets.
#[derive(Reflect, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StreamTuning {
    pub plan_interval_steps: u32,     // 30 (§ 8.2)
    pub retry_after_steps: u32,       // 300 (§ 8.6)
    pub max_retries: u32,             // 3 (§ 8.6)
    pub guard_margin_m: f32,          // 4.0 (§ 10.4)
    pub wait_on_open: bool,           // true (§ 8.7)
    pub hlod_fade_frames: Option<u32>, // absent (`None`) is 0 on every platform; `Some(8)` opts into an 8-frame dither (§ 11.2)
    pub lockstep_latency_steps: u32,  // 2 (§ 15.2)
    pub lockstep_wait_cap_ms: u32,    // 10 000 (§ 15.2)
}
```

Spec 14 A4 lists "streaming settings (Tier 2)" as part of `LevelSettings`. They are a separate resource here instead,
so `LevelSettings` keeps its shape (a new field would break its struct literals) and an unstreamed level carries
nothing; ADR-1804 records the change.

Validation at cook and at load: `cell_size_m` in range, `unload_radius_m >= load_radius_m` (the planner's own rule,
`StreamConfig::validate`), `max_entity_extent_cells >= 1`, HLOD settings valid (`HlodConfig::validate`), and, when
the origin is enabled, `origin.cell_size_m` a whole number of meters from 1 through `1.0e7`
(`LargeSpace::new` / `MAX_ORIGIN_CELL_SIZE`; a larger cell is `origin.cell_size_m` and does not reach
partition). Failures are cook errors `stream_settings_invalid` naming the field. At load, a cooked index that fails
this check is refused before the level opens (nothing is spawned) and the agent reason is `stream_settings_invalid`
(ADR-2269; the cook refuses such a level, so only a damaged index reaches this).
A corrupt index that is not a settings failure is `stream_index_corrupt`. A payload that does not decode is
`stream_index_decode`. A missing asset server is `stream_no_assets`. An always-loaded set that does not spawn is
`stream_spawn_failed`. `stream_not_enabled` means no streamed level is open.

### 5.2 Components

```rust
/// Where an entity of a streamed level goes at cook time. Default: Auto.
#[derive(Component, Reflect, Clone, Copy, Default, Serialize, Deserialize)]
pub enum StreamPlacement { #[default] Auto, AlwaysLoaded, Cell { x: i64, z: i64 } }

/// Raises the load priority of the cells this volume overlaps (cook-time; CellInfo::priority = max over volumes).
/// The volume is the box `half_extents` about the entity's world pose (parent chain, rotation and scale), and it
/// overlaps the cells its world XZ bounds touch. A volume past the addressable cell range fails the cook.
#[derive(Component, Reflect, Clone, Serialize, Deserialize)]
pub struct StreamPriorityVolume { pub half_extents: Vec3, pub priority: i32 }

/// A streaming source (§ 7). On any entity with a GlobalTransform.
#[derive(Component, Reflect, Clone, Serialize, Deserialize)]
pub struct StreamingSource {
    pub kind: SourceKind,        // Camera | Player | Scripted
    pub enabled: bool,           // true
    pub radius_scale: f32,       // 1.0; 0.1..=8.0; divides distances (spec 66 reach_scale)
    pub priority: i32,           // 0; added to every cell priority this source wants
    pub view_bias: f32,          // 0.5; 0..=1; ordering only (§ 8.3)
    pub predict_s: f32,          // 0.0; 0..=4; adds a look-ahead point at pos + velocity · predict_s
    pub drives_origin: bool,     // false; at most one source has it (§ 12.1)
}
```

Runtime-only (not authored): `CellMember(CellCoord)` on every entity spawned from a cell blob; `StreamFrozen` on bodies
the guard froze (§ 10.4); `CellRenderHidden` on members hidden behind a proxy (§ 11.2); `StreamAutoMember` (authored on
a runtime-spawned entity that should belong to the cell it stands in, § 10.5).

### 5.3 Partition rules (cook)

1. Entities of the level are visited in guid order (the order `level_dir` reads them).
2. A hierarchy moves as one: the root decides, children follow it. A placed level instance (spec 37) is one entity.
3. `StreamPlacement::AlwaysLoaded` and `Persistent` entities, entities with no spatial extent (no transform, or resources-like
   entities: game rules, managers) and entities whose XZ bounds exceed `max_entity_extent_cells · cell_size_m` on either
   axis go to the always-loaded set. The last case is a cook warning `stream_large_entity` with the entity's name and size.
4. `StreamPlacement::Cell { x, z }` forces a cell.
5. Otherwise the entity goes to the cell containing the XZ center of its world-space bounds (the union of mesh, collider
   and light bounds of the hierarchy; an entity without bounds uses its position). The center rule gives every entity
   exactly one cell with no seam case, because cells are half-open.
6. A cell's recorded Y range is the union of its members' bounds (feeds `CellProxy::min_y` / `max_y`).
7. A reference from one cell's entity to another cell's entity is allowed but weak: it resolves while both are spawned
   and reads as absent otherwise. The cook lists each such pair as a warning `stream_cross_cell_ref`. References to
   always-loaded entities are strong.

## 6. Cooked data

### 6.1 Overview

A streamed level cooks to three kinds of assets, all with the standard cooked header (`12-assets.md` § 4,
`CookedHeader { magic: b"FCKD", format_version, type_tag, asset_id, .. }`):

| Asset | Type | Content |
|---|---|---|
| `<level>.wstream` | `regia_world::format::WorldIndex` | the world index (§ 6.2) |
| `<level>/cells/<x>_<z>.wcell` | `regia_world::format::CellBlob` | one cell's entities (§ 6.3) |
| `<level>/hlod/<node>.fmesh` | `regia_asset::CookedMesh` (v3, unchanged format) | HLOD proxies (§ 11.1) |

The always-loaded set is the level's ordinary cooked scene (`CookedScene` v1), so a level that turns streaming on keeps
one cooked scene, now smaller. Asset ids are sub-asset ids of the level: `AssetId::sub_asset(level_id, "cells/<x>_<z>")`
and `AssetId::sub_asset(level_id, "hlod/<node>")`, so a recook of the same level gives the same ids (the terrain tile
ids, `regia_terrain::stream::tile_asset_id`, are derived the same way).

### 6.2 World index (`.wstream`, `format_version` 2)

```rust
pub struct WorldIndex {
    pub format_version: u32,          // 2; version 1 still reads (ADR-2483 migrates it on load)
    pub level: Guid,
    pub content_hash: [u8; 32],       // blake3 over every cell blob hash in coordinate order
    pub settings: LevelStreaming,     // as cooked (validated)
    pub cells: Vec<CellRecord>,       // sorted by (x, z)
    pub bounds: [CellCoord; 2],       // min and max cell of the world (inclusive)
    pub hlod: Option<HlodIndex>,      // nodes in HlodTree order + proxy asset id per node
    pub replicated: Vec<(Guid, CellCoord)>, // entities with `Replicated`, sorted by guid (§ 14)
    pub nav: Option<StreamNavLayout>,       // the grid the cells' nav tiles were cooked on (§ 13.2)
}
pub struct CellRecord {
    pub coord: CellCoord,
    pub blob: AssetId,
    pub blob_hash: [u8; 32],
    pub entity_count: u32,
    pub own_bytes: u64,               // decoded cell blob size in RAM
    pub dep_bytes: u64,               // Σ over referenced assets of (asset bytes / number of cells referencing it)
    pub vram_bytes: u64,              // same amortization, GPU side (meshes + base texture mips)
    pub min_y: f64, pub max_y: f64,
    pub priority: i32,                // max over StreamPriorityVolumes, 0 otherwise
    pub deps: Vec<AssetId>,           // referenced assets (meshes, materials, textures, audio), sorted
    pub nav_tiles: u32, pub has_terrain_collision: bool, // hook hints, § 13
}
```

The planner's `CellInfo::bytes` is `own_bytes + dep_bytes`: an amortized estimate fixed at cook time, so planning in
lockstep never depends on what the asset server happens to have resident (§ 15). The runtime also measures real
residency (§ 8.5), which only feeds reports and the async-mode budget adjustment.

**Cells with no record.** A cell inside `bounds` that has no entities and no nav tiles gets no blob and no record.
Such a cell has nothing to load: the runtime treats it as `SimReady` and `Visible` for every gate except
`terrain_collision` (§ 13.1), which still applies where the terrain covers it. Outside `bounds`, cells are always ready.
Without this rule a body on a terrain-only cell would freeze forever and `ready_near` would never finish.

### 6.3 Cell blob (`.wcell`, `format_version` 1)

```rust
pub struct CellBlob {
    pub format_version: u32,          // 1
    pub coord: CellCoord,
    pub scene: CookedScene,           // the existing cooked scene form (postcard, hashed type paths), entities sorted by guid
    pub anchor: [i64; 3],             // the cell's LargePosition cell in the origin space (§ 12.1)
    pub nav: Option<Vec<u8>>,         // cooked nav tile data for this cell (§ 13.2), opaque to regia_world
}
```

Root transforms in `scene` are stored relative to the center of `anchor` in the origin space, so a cell 50 km out keeps
millimeter positions in its file; the apply step converts each root to local space with `FloatingOrigin::to_local`
of the `LargePosition { cell: anchor, local: stored }` (§ 12.2), through the remapping spawn of § 10.1.

### 6.4 Cook

`regia cook` (and `regia_asset_pipeline::cook`) gains a streamed-level step after the scene cook:
1. Read the level source (`read_level_source`, both forms). If `LevelStreaming` is absent or disabled, stop: unchanged output.
2. Partition (§ 5.3), cook each cell's entities with `cook_scene`, compute the record (§ 6.2), write blobs atomically
   (temp + rename, like every cook output). Unchanged cells (same input hash) are not rewritten: an edit near one
   corner of an 8 km² world recooks only the touched cells.
3. Build HLOD proxies if enabled (§ 11.1) and the `HlodTree`.
4. Write the world index last, so a cancelled cook never leaves an index pointing at missing blobs. As built
   (ADR-8812): the first file a cook changes first removes the level's old index, so a cook stopped between that file
   and the new index leaves the level not cooked (`stream_not_cooked`), never an old index naming new blob bytes. A
   recook that changes nothing writes and removes nothing. Removing a file counts as a change too (the stale HLOD
   voxel table, ADR-8860). In a packaged game the same code can mean the content chunk holding the index is not
   mounted: the error then names that chunk (`StreamOpenError::NotCooked { chunk }`, ADR-8862) and the fix is to
   request and mount it, not to cook again. The cook's level scan skips the staging directories an interrupted
   `regia upgrade-project --write` leaves (`.<name>.<pid>.stage`, ADR-8861).
5. Report: cell count, entities per cell (min/median/max), bytes per cell, the always-loaded size, warnings. Also as
   `regia assets stream-report <level>` (JSON with `--json`).

### 6.5 Versioning and migration note

- **No authoring format changes shape.** `LevelStreaming` and the components are new reflected types inside the
  existing open `resources` / `components` maps of `.level.ron`, `.level.d` and `.prefab.ron`; the scene and
  `level-dir` format versions stay where they are. An existing level has no `LevelStreaming` and loads as today. No
  migrator is needed, and `upgrade-project` has nothing to do for v1.
- `LevelStreaming` carries its own `version: u32` (1). A later shape change adds fields with serde defaults or bumps
  `version` with a migrator registered in `regia upgrade-project` (`12-assets.md` § 4a).
- `.wstream` and `.wcell` are cooked outputs. The world index `format_version` is 2 (`WORLD_INDEX_FORMAT_VERSION`);
  version 1 still reads (no node atlas, flat proxy colors) and `regia upgrade-project` rewrites it through
  `migrate_world_index_file`. `.wcell` stays at `format_version` 1. Both are registered with `cargo xtask check-formats`
  (lead runs `--update`) and described in `docs/formats/world-stream.md`. A later layout change bumps `format_version`;
  the loader keeps reading every older layout and `upgrade-project` rewrites cooked files in the dev cache, as for the
  cooked mesh (§ 4a).
- A world index whose `format_version` is newer than the runtime knows is refused (`stream_format_newer`); the level opens
  with its always-loaded set only, cell streaming does not start, and the runtime says so once. If the always-loaded
  cooked scene itself cannot be read, the level stays closed and the error is that read's reason.

## 7. Streaming sources

### 7.1 Kinds

| Kind | Typical owner | Notes |
|---|---|---|
| `Camera` | the active render camera | **Implicit:** when a streamed level is open and no enabled source exists, the primary view is a source with defaults. |
| `Player` | the player's pawn | The framework adds one per local player (and per connected player on a server, § 14) when `GameRules` spawns the pawn. |
| `Scripted` | any entity, or a point with no entity (§ 7.2) | cutscenes, teleport targets, loading screens, AI far from the player. |

### 7.2 Scripted points and pins

`WorldStreaming::pin(name, center: [f64; 3], radius_scale, priority, ttl_steps: Option<u32>) -> PinId` adds a source
with no entity; `unpin(PinId)` removes it; `ttl_steps` removes it automatically. `radius_scale` is `0.1..=8`, the same
range as a `StreamingSource` (a non-finite or non-positive scale, or one outside that range, is `stream_settings_invalid`
and the pin is not stored). `prepare_teleport(target) -> PinId` is
`pin` with priority `+1000` and the target's cells marked `wait`. `ready_near(center, radius_m, level: Readiness)` answers
whether every cell within `radius_m` of `center` has reached `SimReady` or `Visible`. The sequencer player (`.seq.ron`,
ADR-1507) and camera paths (`.shots.ron`) pull their camera `prefetch_s` ahead (default 3 s, `StreamPrefetch`) through a
look-ahead source: `StreamingSource::look_ahead` adds cells without taking the focus or the implicit camera source (ADR-2022).

Limits: at most 64 sources and pins in all (`MAX_STREAM_SOURCES`, the same 64 the stateless planner method allows).
The count is pins plus enabled entity sources. A pin that would pass 64 is `stream_source_limit` and is not stored;
it does not drop an entity source. When enabled entity sources alone exceed the room the pins leave, the sample drops
the extras (`StreamStats::sources_dropped`) and warns `stream_source_limit`.

### 7.3 Source views

Each step, `StreamSet::Sources` samples every enabled source into `StreamViews` (sorted by source id: entity guid for
entity sources, pin id for pins):

- position in world `f64` (`space.to_world(&origin.from_local(local)?)`, § 12.1) and optional view direction (cameras);
- if `predict_s > 0`, a second point at `pos + v · predict_s`, where `v` is the position change over the last step
  divided by the step length, clamped to 200 m/s; both points count as the same source;
- `radius_scale`, `priority`, `view_bias`.

`StreamViews` is a public resource: terrain tile requests (§ 13.1), procgen anchors (§ 7.4) and audio prefetch read it,
so every system streams around the same points.

### 7.4 Relation to procgen anchors (spec 66)

Spec 66 anchors and streaming sources describe the same thing. With a streamed level open, `gather_anchors` adds
`StreamViews` to the generator's own anchors (a source counts as a tagged anchor with its `radius_scale` as `reach_scale`;
a camera anchor keeps its frustum, ADR-2022); the scheduler keeps its own per-tier radii and budgets. Its private grid copy
is still to be replaced by `regia_scene::stream_grid` (mechanical, spec 66 § 1). Baked
procgen cells stay procgen's own cells; they are not world cells. Whichever of the two lands second does the wiring.

## 8. Scheduling

### 8.1 Residency states

```
Unloaded ─▶ Queued ─▶ Loading ─▶ Applying ─▶ Spawned ─▶ SimReady ─▶ Visible
   ▲           │         │           │          │           │          │
   └───────────┴─────────┴───────────┴──── Unloading ◀──────┴──────────┘
                         │
                         ▼
                      Failed ─(retry)─▶ Queued
```

| State | Meaning | Agent string |
|---|---|---|
| `Unloaded` | nothing in memory | `unloaded` |
| `Queued` | wanted, waiting for an IO slot | `queued` |
| `Loading` | read and decode in flight | `loading` |
| `Applying` | decoded; entities being spawned over one or more frames | `applying` |
| `Spawned` | all entities exist; some simulation gates not ready | `spawned` |
| `SimReady` | every simulation gate ready (colliders, terrain collision, nav) | `sim_ready` |
| `Visible` | every render gate ready too (members' meshes resolved; with `StreamRenderResidency` on, also meshes in the render cache with the writes submitted and materials bound) | `visible` |
| `Unloading` | despawn in progress (budgeted) | `unloading` |
| `Failed` | load failed (§ 8.6) | `failed` |

Backward moves besides unloading: a gate that clears a cell moves it down (`Visible → SimReady → Spawned`), and it
moves up again when the gate marks it; a `Failed` cell that is no longer wanted goes to `Unloaded`.

**Mapping to the planner.** `StreamPlanner` keeps only a loaded yes/no per cell. The runtime calls `apply(plan)` when it
queues the plan's loads, so the planner's loaded set is every state from `Queued` to `Visible` plus `Unloading`
(memory is reserved or still held). The planner's budget bound then covers queued and in-flight work, and a queued cell
is not offered again on the next plan. A cancelled or failed cell is `set_loaded(false)`. A failed cell is also **held**
until its retry step, or the next plan would want it again at once: the runtime lane adds
`StreamPlanner::set_held(cell, bool)` (held cells are never load candidates; a new method, not a breaking change). On a
dedicated server no render gates exist, so `SimReady` is final.

### 8.2 Planning triggers

The planner runs in `StreamSet::Plan` when any of these happened since the last run: a source moved more than
`cell_size / 8` (as spec 66 § 4.3), a source or pin was added or removed, a load finished, failed or was cancelled, an
unload finished (memory came free), a held cell reached its retry step, the budget changed, or `plan_interval_steps`
(default 30) passed. It is `StreamPlanner::plan_views` over the source views (§ 8.3).

### 8.3 Priorities

Want and release use the planner's distance rule with hysteresis, with each source's distance divided by its
`radius_scale` (so a source with scale 2 wants cells twice as far). Ordering of the loads in one plan, highest first:

1. `cell.priority + max over sources wanting it of source.priority`;
2. a cell that contains a source's own position, or is a neighbor of it (the 3 × 3 block), before others;
3. effective distance `d' = (d / radius_scale) · (1 − view_bias · max(0, cos θ))`, nearest first (θ between the view
   direction and the direction to the cell center; 0 without a direction), the same formula as spec 66 § 4.2;
4. cell coordinate `(x, z)` ascending.

View bias changes only the order, never which cells are wanted, so turning the camera cannot unload anything. This
needs one addition to the data model: `StreamPlanner::plan_views(&[SourceView]) -> Result<StreamPlan, StreamError>`
beside the existing `plan(&[[f64; 2]])`, which stays as it is (`plan` = `plan_views` with scale 1, priority 0, no bias).

### 8.4 Budgets per frame

| Budget | Field (`StreamBudgets`) | Meaning |
|---|---|---|
| Loads ordered per plan | `max_loads_per_step` | the planner's rate limit |
| IO in flight | `max_inflight` | cells in `Loading` at once |
| Decoded, not applied | `max_pending_bytes` | decoded blobs waiting for apply; IO pauses above it (back-pressure) |
| Apply time | `apply_budget_ms` | main-thread spawn and despawn per frame (async mode) |
| Apply count | `apply_budget_entities` | entities spawned or despawned per frame. Lockstep stops once another hierarchy would pass it. Async timing uses it as a floor: while apply time remains, a hierarchy that fits still starts even if the sum would pass it. A hierarchy larger than the budget starts only when the run has applied nothing yet, and that run then stops |
| Uploads | (existing) `UploadQueue::budget_us`, a per-frame **time** budget at a measured rate | GPU uploads of cell meshes and textures; presentation only (§ 15.1) |
| Memory | `memory_budget_bytes` | resident cell RAM (planner hard bound) |
| VRAM | `vram_budget_bytes` | cell VRAM estimate (`StreamStats::resident_vram_estimate`, `vram_over_budget`); one load set per level in `memory_budget` (`stream_cells:<level>`). Reported by default; with `WorldStreaming::set_vram_bound(true)` (agent `stream.bounds_set`) a second planner hard bound with the memory budget's rules (ADR-2317) |
| HLOD proxies | `hlod_budget_bytes` | proxy meshes resident (§ 11.3) |

Defaults per platform are in § 20, chosen by `StreamBudgets::for_platform()` (desktop, Deck, Android, dedicated
server). They are not keyed by `regia_render::memory_budget::BudgetTier`, which has no Android or server tier; the
VRAM row only adds a `stream_cells` category to that module. Apply works in batches of whole hierarchies; a hierarchy is never half-spawned at the
end of a frame. A hierarchy larger than `apply_budget_entities` spawns alone, only when the run has applied nothing yet, and is counted `apply_over_budget`. It does not join a run that already applied another hierarchy.

### 8.5 Memory accounting

The planner's bound uses the cook estimates (§ 6.2). The runtime also keeps measured numbers: decoded blob bytes per
cell, and the `memory_report` categories for meshes and textures loaded for cells. In async mode, if measured cell
memory exceeds the budget by more than 10 % for 60 steps, the runtime lowers the planner's effective budget by the
excess (never below 50 % of the configured one) and reports `stream_budget_adjusted`; the farthest cells shed. Lockstep
mode never adjusts (§ 15). `StreamStats` exposes both numbers.

### 8.6 Failure and retry

A cell fails when its blob is missing, its chunk is not mounted (`cell_unavailable`), the header or hash does not
match (`cell_corrupt`), its `format_version` is newer (`cell_format_newer`) or decoding fails (`cell_decode`). A failed
cell releases its memory, keeps its HLOD proxy drawn (§ 11.2), freezes bodies near it (§ 10.4), logs once and writes
`CellFailed { cell, reason }`. It is held (§ 8.1) and retries after `retry_after_steps` (default 300) up to
`max_retries` (3) times while still wanted, then stays held and `failed` until `stream.retry` or until it is unwanted
(it then goes `unloaded` and is released) and wanted again. A failed cell never stops other cells.
`cell_unavailable` is detected before any read: the runtime checks `AssetServer::unavailable_chunk(blob)` when it queues.

### 8.7 Teleport and waiting

When a source moves more than `2 · unload_radius` in one step, the scheduler treats it as a teleport: queued loads far
from every source are dropped, in-flight work for unwanted cells is cancelled (§ 9.3), and the plan runs at once. The
game pattern is `prepare_teleport(target)`, wait for `ready_near(target, r, SimReady)` (or show a loading screen), then
move the player. Level open of a streamed level waits for the cells around the start sources to reach `SimReady`
(`wait_on_open: true` by default), counted in the level load report, so the player never spawns over a hole.

As built (ADR-8813): a queued teleport move (`queue_move`, `stream.teleport` with `move_source`) belongs to the first open
level (by id) whose cooked cell bounds hold its target. Closing that level drops the move and its pin while other levels
stay open, so it never fires against another level's grid; a target outside every open level is dropped when the last
level closes.

Overlapping levels: when the cooked cell bounds of two open levels both hold the target, the move belongs to the
level with the lower id (the runtime's level map order, `WorldStreaming::queued_move_level` reports it). Closing that
level drops the move and its pin even though the other level still covers the target; closing the other level leaves
the move queued. A game that wants the move to survive re-queues it after the close.

## 9. Async IO and cancellation

### 9.1 Cell sources

```rust
pub trait CellSource: Send + Sync + 'static {
    /// Start reading one blob. Must not block the caller; completion arrives through the runtime's channel.
    fn read(&self, id: AssetId, ticket: Ticket, cancel: CancelToken);
}
```

Built-ins: `PakCellSource` (mounted paks, through `PakStreamer` or the same mapped-read path), `CacheCellSource` (the
dev cooked cache and paks through a cloned `regia_asset::server::PayloadReader` on `IoPool`; not
`AssetServer::read_cooked`, which takes `&mut self` and cannot leave the main thread), `MemoryCellSource` (tests;
optional injected delays per cell, counted in steps, for lockstep tests). `PayloadReader` does not check chunk
mounting, which is why the queue step checks `unavailable_chunk` first (§ 8.6). A source returns the blob's bytes or an error; decoding happens on
`AsyncComputePool` (`spawn`), never on the main thread.

### 9.2 Tickets and order

Every load gets a `Ticket { cell, generation: u64 }`; the generation increments per cell each time it is queued. A result
whose generation is not the cell's current one is stale: it is dropped on arrival, its memory freed, and counted
(`stale_discards`). `StreamSet::Complete` takes results from a bounded channel; in async mode in arrival order, in
lockstep mode in issue order (§ 15.2).

### 9.3 Cancellation

A cell stops being wanted (beyond `unload_radius` for every source, or dropped by a teleport):
- `Queued`: removed from the queue; nothing was read.
- `Loading`: its `CancelToken` (an `Arc<AtomicBool>`) is set. The source checks it before it starts a read, and the
  decoder checks it before decoding and before publishing; a cancelled result is never published. Reads already inside
  the OS finish and are dropped. The cell goes `Unloaded` at once; its memory counts as freed when the result is dropped.
- `Applying`: spawning stops at the next batch; what was spawned despawns (the cell goes `Unloading`).
- `Spawned` and later: an ordinary unload.

`PakStreamer` has no cancellation today. The runtime lane adds an optional cancel token to its request (a new field
on `StreamRequest` is a struct-literal break: listed under Upgrade in that lane's fragment) or wraps it with the
generation check alone; the lane's ADR records which. Either way, a cancelled cell never appears in the world.

### 9.4 Rules

- The main thread never waits on IO in async mode (counter `main_thread_io_waits` stays 0; a test asserts it).
- No `unwrap`/`expect` on any IO or decode path: every failure becomes a cell `Failed` reason.
- Shared dependencies (meshes, materials, textures) load through the asset server with ordinary handles; a cell holds
  its handles while spawned, so a dependency shared by two cells stays while either holds it. Texture resolution
  beyond the base mips is the texture streamer's job (T2.12) with its own budget.

## 10. Spawning, membership and readiness

### 10.1 Spawn

`StreamSet::Apply` spawns a decoded cell's entities through the cooked-scene spawn path, hierarchies in guid order,
root transforms converted to local space (§ 12.2). `CookedScene::spawn` does all of a scene in one call and decodes
component payloads while it spawns, so the runtime lane adds to `regia_scene::cooked`:
- `CookedScene::hierarchy_ranges()`: the entity index ranges of whole hierarchies, in guid order;
- `CookedScene::spawn_range(world, range, remap: &dyn Fn(Transform) -> Transform)`: spawns one batch of hierarchies,
  applying `remap` to root transforms, and links parents inside the batch;
- optionally a predecode step on `AsyncComputePool` that turns payloads into reflected values with a clone of the type
  registry, so the main thread only inserts. Whether predecode pays off is measured, and the lane's ADR records it.

The existing `spawn` stays as it is. Layer rules (spec 37 § 4.2) apply: a member whose effective layer
state is `Absent` or `Parked` arrives parked (`LayerParked`, hidden, not simulated, not picked), through
`regia_scene::layers::apply_member_presence`. A cell member has no record in the level's template, so an
`Absent` one parks rather than despawns: a later switch to `Live` must be able to bring it back without
reloading its cell (ADR-7411). Every spawned entity gets `CellMember(coord)` and the level's `LevelMember`. A placed
level instance in streamed mode (spec 37 § 6.3) loads its level when its instance entity spawns and unloads with it.

### 10.2 Messages

New messages in `regia_scene` (the existing `LevelLoaded` keeps its shape):
- `CellLoaded { level: LevelId, cell: CellCoord, entities: Vec<Entity> }`: written once when a cell reaches `Spawned`.
- `CellReady { level, cell, readiness: Readiness }`: on `SimReady` and on `Visible`.
- `CellUnloading { level, cell }`: written one `Apply` run before despawn starts, so game systems can save state (§ 10.6).
- `CellFailed { level, cell, reason: String }`.
- `OriginShifted { rebase: Rebase, origin_cell: [i64; 3] }` (§ 12).

### 10.3 Gates

```rust
#[derive(Resource, Default)]
pub struct StreamGates { /* gates by id; per gate a set of ready cells */ }
impl StreamGates {
    pub fn register(&mut self, name: &'static str, scope: Readiness) -> GateId;  // Readiness::{SimReady, Visible}
    pub fn set_ready(&mut self, gate: GateId, level: LevelId, cell: CellCoord);
    pub fn clear(&mut self, gate: GateId, level: LevelId, cell: CellCoord);
    pub fn is_ready(&self, level: LevelId, cell: CellCoord, scope: Readiness) -> bool;
}
```

A cell is `SimReady` when every `SimReady` gate marked it, and `Visible` when additionally every `Visible` gate did.
Built-in gates: `physics` (colliders of members created; `SimReady`), `terrain_collision` (§ 13.1; `SimReady`),
`nav` (§ 13.2; `SimReady`), `render` (members' meshes resolved; with `StreamRenderResidency` on, also drawn once by the
renderer: meshes in its cache with the writes submitted and materials bound, ADR-2317; `Visible`). A gate that
never answers is visible in `stream.cells` as the missing gate's name, which is how an agent finds a stuck cell.

In lockstep mode every `SimReady` gate must answer on a step that depends only on inputs. A gate whose work is
asynchronous (terrain collision tiles from the terrain tile cache, nav tiles) follows the same rule as cell IO
(§ 15.2): it is marked at the start of step N + latency after the cell spawned, waiting for its work if needed, under
the same cap. `Visible` gates are presentation only: their timing may vary (uploads run on a time budget), and
nothing in the simulation reads `Visible` (§ 15.1).

### 10.4 Physics guard

A dynamic body must not fall through a floor that has not loaded. Each step before the physics step, a dynamic rigid
body or character whose XZ position lies in a cell that is not `SimReady`, or within `guard_margin_m` (default 4 m) of
such a cell, gets `StreamFrozen`; `regia_physics` disables it (no integration, no contacts) and keeps its velocity for
when it wakes. It wakes the first step its cell and the margin are `SimReady`. A source's own body frozen this way is
counted (`source_frozen_steps`): it is the visible symptom of a budget that is too small. Kinematic bodies and
static colliders are never frozen.

### 10.5 Moving members

Each step, `StreamSet::Guard` recomputes the cell of every dynamic member (and every `StreamAutoMember`). If it changed
and the new cell is spawned, `CellMember` moves there: the entity now unloads with its new cell. If the new cell is not
spawned, the entity keeps its old membership and is frozen by the guard. A runtime-spawned entity without
`StreamAutoMember` is no cell's member and is never despawned by streaming.

### 10.6 Unload and persistence

Unloading a cell writes `CellUnloading`, then despawns its members from the next run of `Apply` on (§ 4.3: the next
frame in async mode, the next step in lockstep), within the apply budget, and drops their handles. Streaming does not save state: a cell that reloads comes back as cooked. A game that needs changes to
survive (a moved crate, an opened door) reads members on `CellUnloading`, stores what it needs by guid (the save
framework's job), and re-applies it on `CellLoaded`. The book page shows the pattern; a built-in "persist moved bodies"
helper is out of scope for v1 (§ 21).

## 11. HLOD handoff

### 11.1 Proxies (cook)

For each non-empty cell, the cook merges the cell's static, visible meshes into one proxy mesh and simplifies it with
the meshoptimizer simplifier (`meshopt` 0.6, already a pipeline dependency) to `HlodSettings::leaf_triangle_ratio`
(default 0.05) with a minimum of 64 triangles. Materials collapse to one proxy material with a small baked atlas; the
atlas method is the HLOD lane's ADR (ADR-2022 flat colors by default; ADR-2483 baked texture atlases when the
level sets `hlod_atlas_px`; ADR-2713 continues bake rays through cutout holes, writes coverage to the atlas alpha
with an alpha-masked proxy material, searches a parent's cells through a two-level index and folds material graphs).
Parents are built from their children's proxies, merged and simplified again by
`parent_triangle_ratio` (default 0.25 of the children's sum), following `HlodTree::build`'s grouping. Proxies are
cooked meshes (`CookedMesh` v3, unchanged). They have no collision and no nav.

### 11.2 Runtime cut and repair

Each frame in `StreamSet::Hlod`, the runtime calls `HlodTree::select` with the views' world positions, then repairs the
cut so that every leaf is covered exactly once by something that is actually ready:

1. A `Full` draw whose cell is `Visible` stays.
2. A `Full` draw whose cell is not `Visible` becomes a `Proxy` draw of the same leaf if the leaf's own proxy is
   resident (siblings are untouched). Otherwise it is replaced by its nearest ancestor whose proxy is resident; every
   other entry under that ancestor is removed from the cut, and those cells' members get `CellRenderHidden` (they keep
   simulating). If neither exists, the cell is a **hole** (counter `holes`, a test failure in the acceptance runs; § 18).
3. A `Proxy` draw whose proxy is not resident is replaced the same way by a resident ancestor, or by its children if
   all of them are `Visible`.
4. Members of a `Visible` cell covered by a proxy draw get `CellRenderHidden`; members of a `Full` cell lose it.

Because `HlodTree::build` carries a lone node up unchanged, a leaf can be a root. Root proxies are always resident
(§ 11.3), so every leaf has a resident proxy on itself or an ancestor, and with root proxies loaded `holes` stays 0.

Switching happens on the frame both sides are ready, so a cell never shows half its content. A per-draw `fade: f32`
is the dithered cross-fade over `StreamTuning::fade_frames()` (`hlod_fade_frames`; absent means 0 on every platform,
so switches are instant until an author opts in; ADR-2022). An author who wants an 8-frame dither sets `Some(8)`.
The renderer draws the fade when the length is above 0.

**Switch distance and the load radius.** A leaf is drawn `Full` while the viewer is closer than the leaf's own
switch distance, so that distance should sit just inside `load_radius_m`. With `HlodConfig`'s generic default
threshold (0.1 at a 60° field of view) a 128 m cell (bounding radius about 91 m) would switch at about 1.6 km, three
times the 512 m load radius, and the repair would draw proxies where full detail was wanted. So `HlodSettings` leaves
`screen_size_threshold` unset by default and the cook derives it:
`threshold = r_leaf / (0.9 · load_radius_m · tan(fov_y / 2))`, with `r_leaf` the median leaf bounding radius in
meters (about 91 m for a flat 128 m cell). For the defaults (128 m cell, 512 m load radius, 60° field of view) that
derived threshold is about 0.34. An explicit threshold is kept. Cook validation warns `stream_hlod_switch_beyond_load` when more
than 5 % of leaves (level 0) have a switch distance beyond `load_radius_m`.

### 11.3 Proxy residency

Root proxy entities are kept for the life of the HLOD step. Other proxies are wanted when they are in the cut, are the
parent of a node in the cut, or are a full leaf the selection asked to draw. The step spawns an `HlodProxyMesh` (or a
voxel proxy) entity for each one it keeps. The mesh loads through the ordinary asset server, as any other mesh; cell
IO (`pump_io`) reads cell blobs only and does not rank proxy reads above far cells. `hlod_budget_bytes` decides which
of those entities stay beyond the on-screen set (§ 11.4). When the budget is short, the coarsest proxies win.

### 11.4 What a zero or small proxy budget means (ADR-3253)

`hlod_budget_bytes` bounds the proxies held **beyond what is on screen**. The roots, the proxies the repaired cut draws,
the proxies still dissolving out of a cross-fade and the proxies the selection asks to draw are always held, whatever
the budget; the budget then admits the rest (the parents of the cut and the fallback proxies of full leaves), coarsest
first. So:

- **0, or a budget smaller than the next proxy**: no prefetch. What the selection draws loads on demand; while a newly
  wanted proxy loads (a few frames), the repair raises to its nearest resident ancestor exactly as in § 11.2. The cell
  the viewer stands in is never covered by a root proxy for want of budget.
- **A large budget**: as before, parents and fallbacks stay resident and a switch finds its proxy loaded.
- The budget is a target, not a hard cap: the on-screen set can exceed it, and the resident bytes are reported
  (`HlodProxyMesh::bytes`, `HlodProxyVoxels::bytes`). The Server column's 0 (§ 20) draws nothing anyway (a sim-only
  process runs no HLOD step).

Before ADR-3253, 0 evicted every non-root proxy the cut drew, the repair raised the cut to a root, and the root hid the
viewer's own visible cell. Test: `crates/regia_world/tests/hlod_voxels.rs`
`a_zero_or_tiny_proxy_budget_still_draws_the_viewers_cell_in_full`.

### 11.5 Leaves without a cell record (ADR-3253)

A leaf of the HLOD tree whose cell has no cell record streams nothing: its full detail is what draws without streaming
(the terrain's scatter, § 11.6). The repair treats such a cell as `Visible`, so near the viewer the leaf draws `Full`
(nothing from the HLOD side) and the scatter runtime's own instances show; far away its proxy draws as any other.
Every leaf the cook built before ADR-3253 has a record, so this changes nothing for those levels.

### 11.6 Scatter voxel proxies (ADR-3253)

Terrain scatter (`regia_terrain::scatter`) is not part of the level file, so the cook cannot see it in the scene. A host
hands it to the cook as it hands the terrain to the nav cook: `StreamAssets::scatter` returns `StreamScatter`s (the
cooked `.scatter.ron` set, the `TerrainMap` it stands on, holes, layers, density maps, the terrain's world origin and an
optional alpha test for leaf cards), and `WithScatter` wraps another `StreamAssets` with them. Then, for a level with
HLOD on:

1. The cook places every set with `regia_terrain::place_set` (the runtime's deterministic per-cell placement), poses each
   instance as the runtime field poses it (up-axis tilt, yaw, scale, the pivot sunk by `sink` of the height plus the
   downhill gap, capped at 0.6 of the height), takes the instanced mesh's cooked triangles (`StreamAssets::mesh`) and
   gathers them by the streaming cell of the instance's ground point. Colors are the mesh's vertex colors times the
   class tint times the instance's shade.
2. Every cell with scatter becomes a leaf of the HLOD tree, with or without a cell record (§ 11.5); its height range
   grows to its trees, and its proxy bytes include its voxel proxy.
3. Each node with scatter under it is voxelized with `voxel_far_cook::voxelize_with_coverage` (the alpha-aware coverage
   of ADR-3108 and ADR-3227 when the scatter names an alpha test, else opaque) at
   `StreamAssets::scatter_voxel_resolution` voxels (default 32) along the longest axis of the node's scatter: a leaf over
   its cell's instances, a parent over all its leaves' instances, so voxels grow with the node as mesh proxies coarsen.
   Each proxy is an ordinary cooked `VoxelFarProxy` (`voxel-far` format, unchanged) at `hlod/<node>/voxels`, anchored at
   its own origin cell.
4. The cook writes the **HLOD voxel table** (`.wvoxels`, `regia_world::hlod_voxels`, format v1) beside the world index,
   naming per node its proxy, anchor, instance and voxel counts, voxel size and bytes. The world index format is
   unchanged and the table is written only when a node has voxels, so a level without scatter cooks the same bytes as
   before.

At run time `open_streamed_level` reads the table (one that does not read leaves the level without voxel proxies), and
the HLOD step spawns one `HlodProxyVoxels` entity per kept voxel node beside its `HlodProxyMesh` entity. The renderer
draws the finest level as sub-voxel boxes (`VoxelShape::Boxes`, the smallest silhouette error) with the default
material and per-voxel colors. A node is resident when its mesh and its voxels both are (`HlodVoxelResidency`).

Bounds (tests in `stream_cook/scatter_tests.rs`, lavapipe readback from a fixed camera 450 m out): every pixel the
trees cover is covered by the proxy within one pixel, no proxy pixel lies more than a voxel diagonal from a tree pixel,
and the covered area lies between the trees' and the trees' grown by one voxel edge (measured: 1.72 times the trees' for
sparse crowns at 450 m, where the edge growth dominates). Cooking twice gives identical bytes.

Warnings: `stream_hlod_scatter_mesh` (a scatter mesh without cooked triangles), `stream_hlod_scatter_failed` (a set
that does not place), `stream_hlod_voxel_failed` (a node whose triangles do not voxelize).

Tuning: a class's `fade_end` should reach the leaf switch distance, or the trees between the two are drawn by neither.

### 11.7 Proxy shadows (ADR-3253)

HLOD proxies cast no shadow by default (`NotShadowCaster`). `regia_render::HlodProxyShadows { enabled, custom_cascades }`
turns them on per render tier: a proxy then casts with `ShadowCast::Local`, its own bounding radius as the reach of its
shadow, into at most the tier's cascades (Desktop High, Ultra and Cinematic 2; Desktop Low and the Deck 1; the mobile
tiers 0). A Local caster is drawn only into the cascades whose receivers its sphere can touch, so a far proxy reaches
only the far cascades (and their pages), never the near ones where full-detail cells cast their own shadows.
`MeshShadowCast` is the per-entity component behind it. Off by default because it changes the far field's look.

Bound (test `crates/regia_render/tests/hlod_proxy_shadow.rs`, lavapipe, measured 11-07): a 10-segment proxy cast
with `hlod_proxy_shadow_cast` shadows the floor where the 48-segment full-detail caster did: every shadow pixel
with a same-mask 8-neighbour lies within 4 screen pixels (Chebyshev) of the other mask, at most 2 isolated
single-pixel specks (the masks threshold a soft penumbra whose mean darkening is ~2 luma levels against the
8-level cutoff, so a lone crossing pixel is noise, not an edge) may lie beyond it, and the areas agree within 25 %.

## 12. Floating-origin integration

### 12.1 Settings and focus

```rust
pub struct OriginSettings {
    pub enabled: bool,          // true when the level is streamed
    pub cell_size_m: f64,       // 1024.0; a whole number of meters, at most 1.0e7 (MAX_ORIGIN_CELL_SIZE; whole-cell shifts are exact in f32, ADR-1566)
    pub threshold_m: f32,       // 5000.0 (T2.11: rebase when the focus is more than 5 km from the origin)
}
```

The focus is the source with `drives_origin: true`, else the first `Camera` source, else the first source.
`FloatingOrigin` is a plain struct today; `regia_world` wraps it in a resource `WorldOrigin(FloatingOrigin)`.
`GlobalTransform` and `Transform` stay `f32` **local** positions: no `f64` transform type is added. Conversions at the
edges (sources, cooked cells, saves, the agent, the net boundary) use the existing API: local → large with
`FloatingOrigin::from_local`, large → local with `FloatingOrigin::to_local`, and large ↔ world `f64` with
`LargeSpace::to_world` / `from_world` on `origin.space()`. `WorldOrigin::to_world(local)` and `from_world(p)` are
helpers that chain them.

### 12.2 The rebase step

In `StreamSet::Rebase`, `FloatingOrigin::update(focus)` may return a `Rebase { cell_delta, shift }`. Then, in the same
step and before anything else reads positions:

1. Every root `Transform` (entities without a parent) is re-expressed with `Rebase::apply`, which **subtracts**
   `shift` (the origin moved by `shift`, so local positions move by `-shift`), in parallel with `Query::par_iter_mut`.
   Children are relative to their parents and do not change. "Shift" below always means this same `Rebase::apply`.
2. `GlobalTransform` and `PreviousGlobalTransform` of every entity get the same shift, so motion vectors and TAA see no
   movement across the rebase frame.
3. `OriginShifted` is written. Every subsystem with positions outside the ECS transform handles it in the same frame:

| Subsystem | Handling |
|---|---|
| Physics (`regia_physics`) | translate every rigid body and collider position, character controller state and joint anchors in world frame by `shift`; shift `PhysicsPose` and `PreviousPhysicsPose` too, or `interpolate_transforms` blends across the jump for one frame (this is not a `Teleport`: the motion is continuous); velocities unchanged; the broad phase is updated as the physics lane's ADR finds necessary (full rebuild is acceptable at a rebase) |
| Physics: cloth (`regia_physics::ClothSet`) | world-space cloth particles (current and previous positions) shifted |
| Render (`regia_render`) | camera previous view and projection history shifted; light, probe volume and exposure-zone positions come from transforms; baked GI volumes keep their own space and the screen lookup adds `FrameRenderer::origin_offset`; the installed irradiance field and reflection probes move; other world-space GPU state that is not per-entity (cloud and atmosphere camera position, wind fields, shadow cascade snapping origins) is recomputed from local positions; one frame of cascade re-snap is allowed |
| Terrain (`regia_terrain`) | stays in its own world-anchored space (ADR-2022): `SharedFrameInfo::in_world_space` moves the camera and cascades into it at the one boundary, so selection, height and texture lookups, scatter, grass and water do not move; what it reads from the engine that is local (froxel lights, cloud map) goes through `EngineCsm::origin`; water bodies do the same through `WaterRenderer::set_origin_offset`; `WaterQuery` and picking take local coordinates |
| Audio (`regia_audio`) | listener and emitter positions follow transforms; the Doppler velocity skips the rebase frame (positions jump, velocity does not) |
| VFX (`regia_vfx`) | world-space particles (CPU and GPU buffers) shifted; the decal pool (`DecalPool`, which stores world positions, not transforms) shifted |
| Nav (`regia_nav`) | navmesh tiles and agent positions shifted; queries in flight complete in the old frame and are converted |
| AI (`regia_ai`) | spatial indexes over world positions (`SmartObjectIndex`) rebuilt or shifted; stored target and query positions shifted |
| Animation (`regia_animation`) | motion-warp targets (`WarpTarget`) and other stored world-space targets shifted |
| Net (`regia_net`) | positions cross the wire in the shared world frame (§ 14) and convert at the boundary; each peer has its own origin; buffered interpolation states (`NetSnapshotBuffer`) and their offsets are shifted with the local origin |
| Procgen anchors, sequencer paths, demo recordings | stored in world `f64` or `LargePosition`; converted on use |
| Render: material graphs (`regia_render::surface_graph`) | a graph reading `WorldPosition`, `Triplanar` or `ObjectPosition` sees world positions: an adapter spliced at composition adds the origin from the graph's origin slot (`graph_origin_slot`), written by the renderer (`SurfaceGraph::write_origin`); the generated code is unchanged (ADR-2435) |
| Terrain edits (`regia_terrain::edit`) | the brush stays in terrain space; local points go through `OriginOffset` (`apply_stroke_local`, `raycast_local`, `TerrainStrokeSession::push_local`); an open `TerrainStrokeSession` holds `terrain_brush_stroke` in `RebaseBlockers` (ADR-2435) |
| Vehicles (`regia_physics::vehicle`) | spawn pose and the wheels' last contact record shifted (`Vehicle::shift_origin`); `drive_vehicles` holds `vehicle_contact_solve` during its solve, which the step order keeps clear of the rebase (ADR-2435) |

The table is what a search of the crates found on 2026-10-30; it is not a closed list. Any component or resource that
stores a world position outside `Transform`/`GlobalTransform` (a cache, a spatial hash, a history buffer) must handle
`OriginShifted`. A subsystem that does not is a bug; the acceptance test (§ 18, test 9) walks 20 km and checks each
listed subsystem's positions against the world truth, and each lane that adds such a store adds it to that test.

### 12.3 Precision

Around the focus, the largest local coordinate of loaded content is about the threshold plus the unload radius
(5 km + 640 m with the defaults, rounded up by the origin cell's half-width), where the `f32` spacing is under 1 mm. The
rebase shift is a multiple of 1024 m, exactly representable, so a shifted position is the same `f32` it would have been
had the origin been there all along (ADR-1566's walk test covers 20 000 steps).

There is one origin per process. Content near other sources (a pin far away, another player on a server) has no such
bound: its precision is whatever `f32` gives at its distance from the focus (about 2 mm at 16 km, 8 mm at 65 km).
A server whose players spread further than that needs per-region origins, which is out of scope (§ 21).

## 13. Terrain and nav tile streaming hooks

### 13.1 Terrain

The terrain is not partitioned into cell blobs: its tiled heightfield and texture tiles already stream through
`regia_terrain::stream::TileCache` with their own byte budget. Streaming adds:
- **Request points:** the terrain tile cache takes its request positions from `StreamViews` (all sources, not only the
  camera), so a teleport target's ground tiles load with its cells.
- **Collision gate:** the `terrain_collision` gate marks a cell ready when the heightfield tiles covering its rectangle
  are resident and their colliders exist. Terrain tiles and world cells need not align: the hook maps a cell rectangle
  to the covering tiles and keeps a reference count per tile, so a tile shared by four cells stays while any of them
  wants it. Cook warns `stream_terrain_misaligned` when neither size divides the other (correct, but wasteful).
  Built (ADR-2317) as an opt-in per terrain: `TerrainTileCollision { tile_m, view_radius_m, max_tiles_per_step }` on the
  heightfield entity. Holders are the spawned cells (gate's awaiting list until the cell is forgotten) and the sources
  within `view_radius_m`, which also give cells without content their ground; tiles are built in tile order at most
  `max_tiles_per_step` per fixed step. The runtime publishes the open levels' grids as `StreamLevelLayouts`. Without the
  component the heightfield is one collider and the gate opens the whole level.
- A level without terrain registers no terrain gate.

### 13.2 Nav

Nav tiles are cooked per cell into `CellBlob::nav` (tile size = cell size, or cell size divided by an integer). On
`CellLoaded` the nav hook adds the tiles to the runtime navmesh and marks the `nav` gate; on `CellUnloading` it removes
them. Agents on a removed tile pause (a dormant status) until it returns; a path query across a missing tile returns
a partial path with `nav_tile_unloaded`. Obstacles keep working inside loaded tiles (`NavTileCache` rebuilds only the
touched tiles today). The per-tile cook and runtime add/remove are the nav lane's work (gap audit C7 `nav-world`);
this spec fixes the hook contract. The cook writes tiles when `LevelStreaming::nav` is set (`StreamNavCook`), from the
level's static colliders; `WorldIndex::nav` names the grid and `NavStreamPlugin` builds the streamed cache from it (ADR-2022).
The cook also takes the surfaces of the terrains its caller names (`StreamAssets::nav_terrains`), holes cut, as the
run-time `level_triangles_in` reads a `Terrain`; both place a nested static collider at its parents' composed pose
(ADR-2317).

## 14. Multiplayer and dedicated server

- **Server:** every connected player's pawn is a `Player` source, so the server holds the union of what the players
  need. A dedicated server registers no render gates and does not load HLOD proxies; `SimReady` is final there. Its
  `SimulationPlugins` add `NavStreamPlugin`, so streamed nav tiles load there and gate `sim_ready` (ADR-2317).
- **Client:** streams around its own sources. Level entities are matched by guid (spec 71 § 4.5). Spec 71 today
  spawns a bind for an unknown guid as a `Bare` copy and logs `net.level_entity_missing`, which is right for a content
  mismatch and wrong for a cell that is simply not spawned yet. Spec 71 § 4.5 (amended, ADR-2317): if the guid is in the
  world index's `replicated` table (§ 6.2), the client keeps the bind and asks for the latest state when the cell spawns;
  only a guid in no table is a mismatch.
- **Relevance:** the net profile's relevance radius should not exceed the client's `load_radius_m`. The cook warns
  `stream_relevance_radius` only when the caller supplies that reach (`WithNetRelevance`: the largest spatial
  relevance radius times `1 + hysteresis`). `regia cook` does not read `config/net.ron` or a level's net profiles, so
  a cook that does not wrap `WithNetRelevance` emits no such warning. Relevance by cell is a spec 72 follow-up.
- **Positions on the wire:** the shared world frame. Inside ±8 km the existing `f32` encoding of spec 71 keeps about
  1 mm; larger worlds need a cell-plus-offset encoding in the net profile, which is a spec 71/72 follow-up (§ 21).

## 15. Determinism

### 15.1 Guarantees

1. **Decisions:** the plan is a pure function of the registered cells, the loaded set, the source views and the
   settings (true of `StreamPlanner` today). Source views are sampled at step boundaries from simulation state.
2. **Spawn order:** within a cell by guid; across cells in plan order. Entity guids come from the cook and are stable
   across runs and machines; ECS `Entity` ids are not promised.
3. **Rebase:** decided from the focus after the step, applied at the step boundary, so the same inputs rebase on the
   same steps.
4. **Seeds:** per-cell randomness (procgen, scatter) derives from the level guid and the cell coordinate, never from
   load order or time.
5. **Cook:** the same sources give byte-identical `.wstream` and `.wcell` files (sorted inputs, no timestamps).
6. **Presentation is outside the guarantee:** `visible` events, the HLOD cut, `CellRenderHidden` and upload timing may
   differ between runs. Simulation never reads them (the guard reads `SimReady`, `ready_near(.., SimReady)` is what
   gameplay waits on), and the determinism hashes of test 10 exclude them.

### 15.2 Timing modes

`StreamTiming::Async` (default) keeps decisions deterministic but lets completion times vary with the disk. That is
right for players and wrong for tests, replays and lockstep games, so:

- `StreamTiming::Lockstep { latency_steps: u32 }` (default latency 2): a load issued at step N is applied in
  `FixedUpdate` of step N + latency (§ 4.3), in issue order. If its IO has not finished, the main thread waits for it, up to
  `lockstep_wait_cap_ms` (default 10 000); past the cap the run reports `stream_lockstep_stall` and the determinism of
  that run is void (the test fails, it does not hang). Apply uses `apply_budget_entities`, not time. Memory is the cook
  estimate only (§ 8.5). The world state after each step is then a pure function of the inputs.
- `StreamTiming::Replay(trace)`: an async run's `stream.trace` (§ 17) records the step each cell was applied; replay
  applies at the same steps, waiting as lockstep does. Demo recordings (ADR-1569) store the trace beside the inputs.

Tests, goldens, the agent's `play.wait_until` flows and lockstep games use `Lockstep`; the bench uses `Async`.

## 16. API shape

```rust
// regia_world
pub struct WorldStreamingPlugin;                       // adds StreamSet, resources, built-in gates' bookkeeping
#[derive(Resource)] pub struct WorldStreaming { /* per open streamed level: planner, states, queue, tickets */ }
impl WorldStreaming {
    pub fn state(&self, level: LevelId, cell: CellCoord) -> Option<CellState>;
    pub fn cells(&self, level: LevelId) -> impl Iterator<Item = (CellCoord, CellState)> + '_;
    pub fn pin(&mut self, name: &str, center: [f64; 3], radius_scale: f32, priority: i32, ttl_steps: Option<u32>)
        -> Result<PinId, StreamRuntimeError>;
    pub fn unpin(&mut self, pin: PinId) -> bool;
    pub fn prepare_teleport(&mut self, target: [f64; 3]) -> Result<PinId, StreamRuntimeError>;
    pub fn ready_near(&self, center: [f64; 3], radius_m: f64, readiness: Readiness) -> bool;
    pub fn set_budgets(&mut self, level: LevelId, budgets: StreamBudgets) -> Result<(), StreamRuntimeError>;
    pub fn set_timing(&mut self, timing: StreamTiming);
    pub fn retry(&mut self, level: LevelId, cell: CellCoord) -> Result<(), StreamRuntimeError>;
    pub fn stats(&self) -> &StreamStats;               // counters below
}
pub enum CellState { Unloaded, Queued, Loading, Applying, Spawned, SimReady, Visible, Unloading, Failed(String) }
pub enum Readiness { SimReady, Visible }
pub enum StreamTiming { Async, Lockstep { latency_steps: u32 }, Replay(StreamTrace) }
#[derive(Resource, Reflect, Clone, Default)]          // a resource so `play.wait_until` / `play.expect` can read it
pub struct StreamStats {
    pub pending_waits: u32,                            // pins with a wait (teleports, ready_near probes) not yet met, plus 1 per level in its open wait
    pub resident_cells: u32, pub resident_bytes_estimate: u64, pub resident_bytes_measured: u64,
    pub loads_issued: u64, pub loads_cancelled: u64, pub stale_discards: u64, pub failures: u64,
    pub holes: u64, pub source_frozen_steps: u64, pub main_thread_io_waits: u64, pub apply_over_budget: u64,
    pub rebases: u64, pub last_apply_ms: f32, pub last_plan_ms: f32,
}
#[derive(Resource)] pub struct StreamViews(pub Vec<SourceView>);
pub struct SourceView { pub source: SourceId, pub pos: [f64; 3], pub dir: Option<[f32; 3]>,
                        pub radius_scale: f32, pub priority: i32, pub view_bias: f32 }
```

`StreamStats` and `CellState` are `#[non_exhaustive]` so later counters and states are not breaking changes. The
runtime writes `StreamStats` into the world each frame (it is not only behind `WorldStreaming::stats`), which is what
makes the agent predicates of § 17 possible. Every
per-frame system has a `tracing` span: `stream_sources`, `stream_rebase`, `stream_plan` (exists), `stream_complete`,
`stream_apply`, `stream_guard`, `stream_hlod`.

## 17. Agent protocol surface

Guide page: `book/agents/streaming-runtime.md` (topic `streaming-runtime`), linked from `book/agents/world-streaming.md`
(the data-model page stays as is), with `book/COVERAGE.md` rows for every method. It explains the states and gates,
budgets and what the counters mean, pins and teleports, lockstep for tests, the persistence pattern (§ 10.6) and every
error code below. The stateless `scene.stream_plan`, `scene.hlod_select` and `scene.large_position` stay unchanged.

| Method | Mode | Params | Returns |
|---|---|---|---|
| `stream.status` | read | — | `{ enabled, level, timing, origin_cell, cells: { <state>: n }, budgets, stats }` |
| `stream.cells` | read | `{ state?, near?: [x,y,z], radius_m?, limit? (≤ 10 000) }` | `[{ cell: "x_z", state, bytes_estimate, bytes_measured, entities, missing_gates: [name], reason? }]` |
| `stream.sources` | read | — | `[{ id, kind, pos, radius_scale, priority, frozen }]` (pins included, `kind: "pin"`) |
| `stream.pin` | play | `{ name, center: [x,y,z], radius_scale?, priority?, ttl_steps?, wait?: { radius_m, readiness } }` | `{ pin }`; a wait counts in `StreamStats.pending_waits` until met |
| `stream.unpin` | play | `{ pin }` | `{ removed }` |
| `stream.teleport` | play | `{ target: [x,y,z], move_source?: id, wait?: "sim_ready"\|"visible" }` | `{ pin, ready }`; with `move_source`, moves that source's entity once ready |
| `stream.ready` | read | `{ center, radius_m, readiness }` | `{ ready, waiting: [cell] }`; the predicate for `wait_until` |
| `stream.budget_set` | play (dev builds) | `{ budgets }` (any subset of § 8.4 fields) | the active budgets |
| `stream.timing_set` | play (dev builds) | `{ mode: "async"\|"lockstep", latency_steps? }` | `{ mode }` |
| `stream.retry` | play | `{ cell }` | `{ state }` |
| `stream.hlod` | read | — | the repaired cut: `[{ node, kind, cell?, fade }]`, `holes` |
| `stream.origin` | read | — | `{ cell, cell_size_m, threshold_m, focus, last_rebase: { step, cell_delta }? }` |
| `stream.rebase` | play (dev builds) | `{ cell?: [x,y,z] }` (default: the focus's cell) | `{ cell_delta }` |
| `stream.trace` | read | `{ since_step?, limit? (≤ 10 000) }` | ring buffer (default 4096) of `{ step, cell, event, detail? }`; `event` ∈ queued, loading, applied, sim_ready, visible, unloading, unloaded, cancelled, failed, rebase |
| `stream.overlay` | play (dev builds) | `{ on }` | draws cell borders colored by state and the HLOD cut in the debug layer, so `seeing.screenshot` shows them |
| `stream.bounds_set` | play | `{ vram_bound?, render_residency? }` | `{ vram_bound, render_residency }` (ADR-2317) |
| `stream.collision_tiles` | read | — | `{ terrains, resident_tiles, pending_tiles, built, removed, held_cells }` (ADR-2317) |

Errors (`F0002` with `data.reason`, lowercase as spec 23 requires): `stream_not_enabled` (no streamed level open),
`stream_unknown_cell`, `stream_unknown_source`, `stream_unknown_pin`, `stream_source_limit`, `stream_bad_budget`,
`stream_dev_only` (a dev method in a shipping build), `stream_lockstep_stall`, `stream_format_newer` (the always-loaded
set is open; cell streaming did not start), `stream_settings_invalid` (cooked settings failed validation, or a pin
`radius_scale` outside `0.1..=8`; the level or the pin did not open), `stream_index_corrupt`, `stream_index_decode`,
`stream_spawn_failed`, `stream_no_assets`, `stream_rebase_blocked`, `stream_wait_area_refused` (a pin wait over an
area an open level's grid cannot list, past 2^22 cells; the pin is not stored, ADR-7612). Cell failure
reasons in `stream.cells` and `CellFailed`: `cell_unavailable`, `cell_corrupt`, `cell_format_newer`, `cell_decode`,
`cell_missing`. Cook warnings (in `stream-report` and the cook log): `stream_settings_invalid`, `stream_large_entity`,
`stream_cross_cell_ref`, `stream_hlod_switch_beyond_load`, `stream_terrain_misaligned`, `stream_relevance_radius`,
`stream_priority_volume_invalid`.

The agent's verification flow (rule 8): `stream.timing_set lockstep`; `stream.teleport` (or `stream.pin` with a wait)
for the next stop; `play.wait_until` with a `resource` predicate on `StreamStats.pending_waits == 0` (the predicate
language is `all|any|not|entity|resource|state|logs_contain|events_contain`; it cannot call a method, so the waits are
exposed as a resource field); `seeing.screenshot` with the overlay on; `play.expect` with a `resource` predicate on
`StreamStats.holes == 0` (cumulative hole-frames after the open wait) or read `stream.hlod`'s `holes` for the current
cut. `stream.ready` is the one-shot query for scripts that poll.

## 18. Tests and acceptance

Unit and integration tests run headless on lavapipe or with no GPU, in lockstep, with `MemoryCellSource` unless a test
says otherwise. No wall-clock assertions: counters and fixed steps only.

1. **Partition.** Center rule, hierarchy as one, forced cell, always-loaded cases, large-entity warning, Y ranges;
   results independent of entity file order; recook of an untouched level is byte-identical; one moved entity
   rewrites only its old and new cells.
2. **Formats.** `.wstream` / `.wcell` round-trip; a newer `format_version` is refused with its code; a truncated or
   hash-mismatched blob fails `cell_corrupt` without a panic (fuzz target beside the cooked-header fuzzers).
3. **States.** Every transition in § 8.1 is reached; every `CellState` is produced by some test.
4. **Budgets.** Along a 2000-step walk: planner resident bytes never above `memory_budget_bytes`; in-flight never above
   `max_inflight`; pending decoded bytes never above `max_pending_bytes`; entities applied per frame never above
   `apply_budget_entities` except whole-hierarchy overruns, which are counted.
5. **Cancellation.** A cell cancelled in each of `Queued`, `Loading` (delayed source), `Applying` never leaves an
   entity; stale results are counted and dropped; a teleport cancels far work at once.
6. **Failure.** Missing blob, unmounted chunk, corrupt blob, newer version: each sets its reason, keeps the proxy,
   freezes nearby bodies, retries three times, and never blocks other cells.
7. **Guard.** A box dropped at a cell edge whose neighbor is loading stays frozen and falls correctly once ready; a
   member moving into a spawned cell changes membership; into an unspawned cell, it freezes.
8. **HLOD repair.** For 200 random view paths, every leaf is covered exactly once every frame; `holes` stays 0 when
   root proxies are resident; a `Full` cell that is not `Visible` falls back to the nearest resident ancestor.
9. **Rebase.** A 20 km scripted walk with physics bodies, an audio emitter, a particle system, a nav agent and the
   camera: after every rebase each subsystem's position, converted to world `f64`, matches the truth within 1 mm;
   motion vectors on the rebase frame are zero for static content (lavapipe frame, read back).
10. **Determinism.** The same 1000-step scripted run twice in lockstep gives identical `stream.trace` hashes (over every
    event except `visible`, § 15.1 item 6) and identical `world_state_hash` at every 100th step (presentation
    components excluded); a replay of an async run's trace matches it.
11. **Async smoke.** The same path in async mode with a pak source: no `main_thread_io_waits`, budgets held, every
    wanted cell eventually `Visible`.
12. **Agent.** Every method round-trips; each error code is produced by a test; `cargo xtask agent-coverage` passes.
13. **Android lifecycle.** Suspend in the middle of loads (desktop simulation of the lifecycle events): no new reads
    while suspended, in-flight reads finish, resume continues; the GPU surface rule of `AGENTS.md` § 5 holds.
14. **Empty cells.** A body on a terrain-only cell with no record wakes once terrain collision is ready; `ready_near`
    over a region of empty cells answers at once.
15. **Planner mapping.** A queued cell is not offered again by the next plan; a failed cell is held until its retry
    step; an unload that frees memory triggers a plan that loads a deferred cell.

**Acceptance A, correctness on lavapipe (lane 5 builds it, the gate runs it):** `samples/stream_world`, a generated
world of at least 8 km² (2048 m × 4096 m, 128 m cells, 512 cells) from a fixed seed: terrain heightfield, about 200
static entities per cell from built-in meshes, dynamic bodies in every tenth cell, lights, HLOD on. A scripted flight
(ground walk, a 50 m/s vehicle leg, a 2 km teleport, a rebase crossing) in lockstep at 320 × 180: `holes == 0`,
`source_frozen_steps == 0` after the open wait, every budget held, every cell on the path `Visible` at least once, two
runs identical (test 10), no wall-clock assertion. Screenshots are written for review; **golden images are not part of
this spec's tests** (a lane cannot bless them); the lead may bless overlay shots later on the build server lavapipe
reference with the reason in the commit message.

**Acceptance B, GPU budget (the lead measures, `docs/perf-log.md`, exclusive card only):** the same world in async
mode at 1440p High on the reference cards and on the Deck and a Pixel 6 class device: the § 20 frame-side budgets, and
the teleport bench (`cargo xtask stream-bench`, master plan § 5.3 item 10): time from a 2 km teleport to every cell in
the view `Visible` and texture mips at full residency (target ≤ 2.0 s ★) and frames over 33 ms during the fill
(target 0 ★). Until measured these are targets, not pass lines (master plan § 5.1a).

## 19. Platform notes

- **All platforms:** the runtime is portable Rust with no `cfg(target_os)` code; platform IO stays in `regia_asset`
  (mapped paks, storage paths). It builds for Linux, Windows and Android aarch64 from its first commit (`cargo ndk -t
  arm64-v8a check -p regia`), and on the Mac where the workspace already builds (testing only).
- **Linux:** mapped pak reads with read-ahead (existing). The IO pool is sized by `regia_core::tasks`.
- **Windows:** the same mapped-read path. Antivirus scanning can add latency to first reads of a pak: async mode
  absorbs it; lockstep tests use the memory source. Cooked paths are forward-slash asset paths, never OS paths.
- **Android aarch64:** paks in the APK or install-time asset packs (spec 40) are read through the existing storage
  path. Lower defaults (§ 20). On `suspended`, streaming stops issuing reads, lets in-flight reads finish and keeps all
  cells (the device and GPU resources survive; only the surface is recreated). On a low-memory trim signal the memory
  budget drops to 50 % until resume, and the planner sheds the farthest cells. Thermal throttling is handled by the
  apply time budget, not by this spec.
- **Steam Deck:** desktop Linux path with the Deck tier budgets.
- **Dedicated server:** no render or HLOD; budgets from the server tier.

## 20. Performance budgets

On the reference classes of `06-testing-and-ci.md` § 3. ★ = a target until measured in `docs/perf-log.md`.

| Budget | Desktop | Deck | Android (Pixel 6 class) | Dedicated server | Span / counter |
|---|---|---|---|---|---|
| `memory_budget_bytes` (cell RAM) | 2 GiB ★ | 1 GiB ★ | 512 MiB ★ | 4 GiB ★ (the union of all players) | `resident_bytes_*` |
| `vram_budget_bytes` (cells, inside the § 5.1a process cap) | 2.5 GiB ★ | 1 GiB ★ | 384 MiB ★ | — | `memory_budget` category `stream_cells` |
| `hlod_budget_bytes` | 256 MiB ★ | 128 MiB ★ | 64 MiB ★ | — (no proxies) | proxy bytes |
| `max_pending_bytes` | 256 MiB | 128 MiB | 64 MiB | 256 MiB | pending decoded |
| `max_inflight` | 8 | 4 | 4 | 16 | in flight |
| `max_loads_per_step` | 8 | 4 | 2 | 16 | planner |
| `apply_budget_ms` | 2.0 ★ | 1.5 ★ | 1.0 ★ | 2.0 ★ (per fixed step) | `stream_apply` |
| `apply_budget_entities` | 400 | 300 | 150 | 400 | applied per frame |
| Plan, 5000 cells × 4 sources | ≤ 0.1 ms ★ | ≤ 0.2 ms ★ | ≤ 0.3 ms ★ | ≤ 0.5 ms ★ (64 sources) | `stream_plan` |
| HLOD cut + repair, 4096 leaves | ≤ 0.2 ms ★ | ≤ 0.3 ms ★ | ≤ 0.5 ms ★ | — | `stream_hlod` |
| Rebase frame, 50 000 roots | ≤ 1.0 ms ★ | ≤ 1.5 ms ★ | ≤ 3.0 ms ★ | ≤ 1.0 ms ★ | `stream_rebase` |
| Main thread IO waits (async) | 0 | 0 | 0 | 0 | `main_thread_io_waits` |
| Holes during the acceptance flight | 0 | 0 | 0 | — | `holes` |
| Teleport 2 km to full resolution | ≤ 2.0 s ★, 0 frames > 33 ms ★ | same | ≤ 4.0 s ★ | — | `stream-bench` |

The entity budget follows from the cooked-scene spawn budget of spec 14 (10 000 entities in under 50 ms): about 200
entities per millisecond on the desktop reference, so 400 per frame fits 2 ms.

## 21. Out of scope

- Vertical (3D) cell addressing; multiple grid sizes per level (a coarse grid for large entities); the always-loaded
  set covers large entities in v1.
- Editor streaming: the grid overlay in the editor viewport, "load only this region" editing, per-cell checkout. The
  editor opens the whole level as today (spec 37 § 7 (a) keeps partial loading possible later).
- Saving runtime changes of cells (the save framework's job; § 10.6 gives the hook) and a built-in "persist moved
  bodies" helper.
- Relevance by cell and a cell-plus-offset position encoding in netcode (spec 71/72 follow-ups).
- Server-side spatial sharding across processes.
- `f64` transforms in the ECS (rejected in ADR-1804).
- Streaming audio banks and music by cell (load sets cover menus and maps; spec 12 § 5b).
- Cooked-cell delta patches for live updates (spec 40 chunks deliver whole paks).
- Lightmap and baked GI data streaming by cell beyond what entity-owned assets already do.

## 22. Provenance

Public documents and papers only, re-derived in Regia terms; no engine source and no other engine's documentation.
Rows go in `docs/provenance.md` (ADR-1804) and are extended by each builder lane for its algorithm.

- Region-based prefetching around a moving viewer under a memory bound: T. A. Funkhouser, C. H. Séquin, S. J. Teller,
  "Management of Large Amounts of Data in Interactive Building Walkthroughs", ACM Symposium on Interactive 3D Graphics,
  1992.
- Budgeted selection of detail per frame: T. A. Funkhouser, C. H. Séquin, "Adaptive Display Algorithm for Interactive
  Frame Rates During Visualization of Complex Virtual Environments", SIGGRAPH 1993.
- Hierarchical proxies for groups of objects: C. Erikson, D. Manocha, W. V. Baxter III, "HLODs for Faster Display of
  Large Static and Dynamic Environments", ACM Symposium on Interactive 3D Graphics, 2001.
- Chunked, screen-error driven detail for large terrains: T. Ulrich, "Rendering Massive Terrains using Chunked Level of
  Detail Control", SIGGRAPH 2002 course notes.
- Proxy simplification: M. Garland, P. S. Heckbert, "Surface Simplification Using Quadric Error Metrics", SIGGRAPH 1997,
  through the meshoptimizer library (A. Kapoulkine, MIT licence, already a dependency).
- Floating origin: C. Thorne, "Using a Floating Origin to Improve Fidelity and Performance of Large, Distributed Virtual
  Worlds", International Conference on Cyberworlds, 2005.
- Origin-relative rendering and the precision of large coordinates: P. Cozzi, K. Ring, *3D Engine Design for Virtual
  Globes*, CRC Press, 2011, chapter 5 ("Vertex Transform Precision"); D. Goldberg, "What Every Computer Scientist Should
  Know About Floating-Point Arithmetic", ACM Computing Surveys 23(1), 1991.
- Regia design choices: the residency states and gates, the HLOD repair rule, the amortized cook-time cost, lockstep
  and replay timing, the physics guard, the rebase order and subsystem table, the budgets and their defaults.
