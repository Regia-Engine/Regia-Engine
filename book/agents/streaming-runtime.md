# World streaming runtime

The runtime behind `docs/specs/78-world-streaming.md`. A cooked streamed level loads its cells around
streaming sources, within a time budget per frame and a hard memory budget, without blocking the frame
on IO. The stateless data-model methods (`scene.stream_plan`, `scene.hlod_select`,
`scene.large_position`) are on the [World streaming and HLOD](world-streaming.md) page and do not touch
this runtime.

Topic: `streaming-runtime` (`docs.guide`). Crate: `regia_world`. Subsystems never depend on it: they
talk through `regia_scene::streaming`.

## Streaming a level

Add a `LevelStreaming` resource to the level (`regia_scene::streaming::LevelStreaming` in the
`resources` map of `.level.ron`, or `level.ron` in a `.level.d`):

```ron
"regia_scene::streaming::LevelStreaming": (
    enabled: true,
    cell_size_m: 128.0,
    load_radius_m: 512.0,
    unload_radius_m: 640.0,
)
```

`regia cook` writes one blob per non-empty cell, the always-loaded set and the world index (format page:
`docs/formats/world-stream.md`). `regia assets stream-report <level>` shows the partition without
writing anything. Open the level with `stream.open` or `regia_world::open_streamed_level(world, level,
mode)`. A level opened from its source file spawns whole, as it always did.

With `wait_on_open` (the default) the open counts in `StreamStats.pending_waits` until the cells around the
start sources are `sim_ready`: show a loading screen until then, while the guard holds bodies still.
The guard sees dynamic bodies and characters through the `StreamGuarded` marker, which physics adds only while a
streamed level is open (`StreamingActive`), so a game that never streams runs exactly as without the runtime.

Entities go to the cell containing the center of their bounds; a hierarchy moves with its root.
`StreamPlacement::AlwaysLoaded`, `Persistent` entities, entities with no transform and entities wider
than `max_entity_extent_cells` stay with the level. `StreamPlacement::Cell { x, z }` forces a cell.
`StreamPriorityVolume` raises the load priority of the cells it overlaps.

## Cell states

| State | Meaning |
|---|---|
| `unloaded` | Nothing in memory. |
| `queued` | Wanted, waiting for an IO slot. |
| `loading` | Read and decode in flight. |
| `applying` | Decoded; entities spawning over one or more frames. |
| `spawned` | All entities exist; a simulation gate is not ready. |
| `sim_ready` | Every simulation gate is ready. |
| `visible` | Every render gate is ready too. |
| `unloading` | Despawn in progress. `CellUnloading` went out one apply run before it. |
| `failed` | The load failed; `reason` says why. |

A gate is a named check: `physics` (colliders exist), `terrain_collision` (the terrain collider exists; only
levels with one) and `render` (every `MeshSource` and `Primitive` member has its mesh). A cell whose state
stays `spawned` shows the missing gate's name in `missing_gates`, which is how you find a
stuck cell. A cell in the world's bounds with no record has nothing to load and counts as ready, and
so does everything outside the bounds.

A dynamic body or character near a cell that is not `sim_ready` is frozen (`StreamFrozen`): it keeps
its velocity and wakes when the ground is there. `frozen_bodies` and `source_frozen_steps` count them;
a frozen source is the visible symptom of a budget that is too small.

## Sources and pins

An entity with a `StreamingSource` is a source: `kind` (`Camera`, `Player`, `Scripted`), `radius_scale`,
`priority`, `view_bias` and `predict_s`. With no enabled source, the active render camera is one.
`view_bias` changes the load order only, never which cells are wanted, so turning the camera unloads
nothing. A pin is a source with no entity. `radius_scale` on a pin is `0.1..=8`, the same range as a
`StreamingSource`; outside that range `stream.pin` returns `stream_settings_invalid` and stores nothing.
At most 64 sources and pins, counting pins plus enabled entity sources. A pin that would pass 64 returns
`stream_source_limit` and does not drop an entity source. Entity sources beyond the room the pins leave
are the ones `StreamStats.sources_dropped` counts.

Far-away work: `stream.teleport` (`WorldStreaming::prepare_teleport`) pins the target at priority +1000
and waits for it; pass `move_source` to move a source's entity once the target is ready. Without that,
wait for it yourself before moving the player. The move lives as long as its pin: `stream.unpin` on the
teleport's pin (or its `ttl_steps` running out) cancels the move, and it never fires later.

## Methods

| Method | Mode | Answer |
|---|---|---|
| `stream.open` | play | `{ level, always_loaded_entities }` |
| `stream.status` | read | `{ enabled, level, terrain_tiles, timing, suspended, origin_cell, cells, budgets, stats, vram_bound, render_residency, collision_tiles }` |
| `stream.cells` | read | `[{ cell, state, bytes_estimate, bytes_measured, entities, missing_gates, reason? }]` |
| `stream.sources` | read | `[{ id, kind, pos, radius_scale, priority, frozen }]` |
| `stream.pin` | play | `{ pin }` |
| `stream.unpin` | play | `{ removed }` |
| `stream.teleport` | play | `{ pin, ready }` |
| `stream.ready` | read | `{ ready, waiting: [cell] }` |
| `stream.budget_set` | play, dev builds | the active budgets |
| `stream.timing_set` | play, dev builds | `{ mode }` |
| `stream.retry` | play | `{ state }` |
| `stream.hlod` | read | `{ cut: [{ node, kind, cell?, fade, color? }], holes, leaving: [{ node, kind, cell?, share }] }` |
| `stream.origin` | read | `{ cell, cell_size_m, threshold_m, focus, blocked_by, holds, offset_m, last_rebase? }` |
| `stream.rebase` | play, dev builds | `{ cell_delta }` |
| `stream.trace` | read | `[{ step, cell, event, detail? }]`, oldest first |
| `stream.overlay` | play, dev builds | `{ on }` |
| `stream.bounds_set` | play | `{ vram_bound, render_residency: { on, waits, waiting_cells, timeouts } }` |
| `stream.collision_tiles` | read | `{ terrains, resident_tiles, pending_tiles, built, removed, held_cells }`, or `null` without physics |

Trace events: `queued`, `loading`, `applied`, `sim_ready`, `visible`, `unloading`, `unloaded`,
`cancelled`, `failed`, `rebase`.

Parameters (positions are world `[x, y, z]` in meters; a row cap `limit` is at most 10000, which is also
the default):

| Method | Parameters |
|---|---|
| `stream.cells` | `state` (only cells in that state), `near` with `radius_m` (only cells within `radius_m` of `near`; `near` without `radius_m` is `invalid_params`), `limit`. |
| `stream.pin` | `center` (required), `name` (default `agent`), `radius_scale` (default 1, `0.1..=8`), `priority` (added to every cell it wants, default 0), `ttl_steps` (the pin lives that many streaming steps and is removed on the next one; unset keeps it until `stream.unpin`), `wait` `{ radius_m, readiness }` (`readiness` `sim_ready`, the default, or `visible`; an area the grid cannot list is `stream_wait_area_refused`). |
| `stream.ready` | `center`, `radius_m` (not negative) and `readiness` (`sim_ready` or `visible`), all required. |
| `stream.timing_set` | `mode` (`async` or `lockstep`, required); with `lockstep`, `latency_steps` (default: the level's `lockstep_latency_steps` tuning, else 2). |
| `stream.trace` | `since_step` (only events from that step on, default 0) and `limit`. |

## Budgets

| Budget | Meaning |
|---|---|
| `max_loads_per_step` | Loads one plan orders. |
| `max_inflight` | Cells in `loading` at once. |
| `max_pending_bytes` | Decoded bytes waiting for apply; IO pauses above this. |
| `apply_budget_ms` | Main-thread spawn and despawn per frame, async timing. |
| `apply_budget_entities` | Entities per run. Lockstep stops once another hierarchy would pass it. Async timing treats it as a floor: while apply time remains, a hierarchy that fits still starts. A hierarchy larger than the budget starts only when the run has applied nothing, then the run stops. |
| `memory_budget_bytes` | Resident cells, a hard bound on the cook's estimates. |
| `vram_budget_bytes`, `hlod_budget_bytes` | Cell VRAM estimate, resident proxies. |

A hierarchy is never half spawned. One larger than the budget waits until it is the only batch of a run,
then spawns alone and counts in `apply_over_budget`. It does not join a run that already applied another
hierarchy. Memory is released over several frames, so an unload does not make room until it
finished. Defaults per platform are `StreamBudgets::for_platform()`.

`StreamStats` is a resource, so `play.wait_until` and `play.expect` can read it. Besides the counters of
`stream.status` it carries `trace_hash`, which repeats exactly between two lockstep runs.

## Verifying a change

Run in lockstep, where a load issued at step N applies at step N + latency whatever the disk does:

1. `stream.timing_set { "mode": "lockstep" }`.
2. `stream.teleport` or `stream.pin` with a `wait` for the next stop.
3. `play.wait_until` with a `resource` predicate on `StreamStats.pending_waits == 0`. Predicates cannot
   call methods, which is why the waits are a resource field.
4. `stream.overlay { "on": true }` and `seeing.screenshot`.
5. `play.expect` with a `resource` predicate on `StreamStats.holes == 0`. That field is the cumulative
   hole-frames after the open wait. The current cut's count is `stream.hlod`'s `holes`.

`stream.ready` is the one-shot query for a script that polls. An area too large for a level's grid to
list (its bounding box passes 2^22 cells, for example a 10 km radius over 8 m cells) is never ready:
`stream.ready` answers `ready: false` (with no cell in `waiting`). A pin wait over such an area could
never be met, so `stream.pin` refuses it with `stream_wait_area_refused` and stores no pin (before,
the wait was accepted and `play.wait_until` on `pending_waits == 0` ran into its own timeout). Ask for
a smaller radius. A wait set through `WorldStreaming::set_pin_wait` before any level is open has no
grid to be checked against; once a level opens, it stays in `pending_waits`.

## HLOD

With `hlod` in the settings the cook merges each cell's static meshes into a proxy and simplifies it, and
parents merge their children's proxies again. Each frame the selection is repaired so every leaf is covered
exactly once by something ready: a cell that is not `visible` falls back to its own proxy, then to the
nearest resident ancestor; a proxy that is not resident rises the same way or opens into visible children.
Members a proxy covers get `CellRenderHidden` and keep simulating. `stream.hlod`'s `holes` is how many
leaves the current cut leaves uncovered. `StreamStats.holes` adds that count once per step after the open
wait, so it stays 0 when root proxies are resident and no frame has a hole.

A proxy draws with one flat color: the cook averages the `Primitive` colors it merges, weighted by surface
area, and parents average their children the same way. A proxy of `MeshSource` meshes only (the cook does not
read their materials) has no color and draws with the default material. `stream.hlod` names each proxy
entry's `color` (linear RGBA, or `null` for the default material), so a script can check that the horizon
takes the color of what it stands for.

Switches are instant unless the level sets `tuning.hlod_fade_frames` (opt-in until the look is signed off). With
it, a switch dissolves over that many frames (the switch frame is the first, so N frames show N - 1
dithered samples and 1 is instant): what entered the cut dithers in while what left it keeps drawing and
dithers out, with complementary screen-space dithers, so every pixel is drawn once. `stream.hlod` reports the
incoming share as each entry's `fade` (0 once settled) and lists what is dissolving out under `leaving`, with the
share it still covers; members and proxies in a fade carry `HlodFade`, which the
renderer draws as its LOD dither. Simulation never reads it.

## Floating origin

`Transform` and `GlobalTransform` stay `f32` around an origin cell. When the focus (the source with
`drives_origin`, else the first camera source, else the first source) strays past `origin.threshold_m`, a
step boundary moves the origin by whole cells: root, global and previous transforms shift together, and
everything else that stores a position follows through `OriginShifted` in the same fixed step: physics and
cloth, the renderer's camera and draw history, the installed irradiance field and reflection probes, the
cloud drift (fog and cloud histories start over on that frame), audio motion, warp targets, nav (meshes,
tile caches, obstacles, crowds, lanes, queued rebuilds, agent waypoints), and effects (instances and their
pending spawns, the GPU particle pool, decals, debris, fire lights, impact sites). A game's own `TrailRig`
has `shift_origin` for it to call.
Positions in agent replies are world positions, `f64` meters, and do not change at a rebase. Anything else
that stores a local position must handle `OriginShifted` in the same frame.

Replication keeps a separate origin per peer: root positions cross the wire in the shared world frame
(`OriginOffset` is the local origin's world position), so a client and a server with different origins agree on
where things are, and the wire bytes are unchanged without a streamed level. A local rebase moves the
interpolation buffers of root proxies, the predicted `Transform` history and the lag compensation history. The
wire keeps `f32`, about 1 mm inside 8 km of the world origin.

Terrain, water bodies and baked GI volumes stay in their own world-anchored space and the frame converts at the boundary,
so a level with any of them keeps its origin working. Nothing in them moves on a rebase: `terrain.*` and `water.*` agent
methods, runtime height patches, craters and holes, and bakes keep speaking world meters, while `WaterQuery` and
`regia_pick` take and answer local coordinates. A game calling a terrain edit API from a local position adds `OriginOffset`. DDGI re-tags its
probes and settles again after a rebase, and shadow cascades refit on their own. A world inside about 8 km of its
origin needs no rebase.

A level file is authored in world meters: one opened with the origin moved converts its roots as it spawns. When the last
streamed level closes, the origin goes back to cell zero through a normal rebase, so `OriginOffset` and every store return
to zero and a plain level sits at full precision.

A host store that cannot follow a rebase yet can register in `RebaseBlockers`: the runtime then takes no rebase,
`StreamStats.rebases_blocked` counts the ones it skipped (the first logs a `stream_rebase_blocked` warning),
`stream.origin` names it in `blocked_by` and `stream.rebase` is refused.

The engine holds two names through `RebaseHolds`, which counts owners so one name stays registered until the last
lets go (ADR-2435). `stream.origin` lists them in `holds` (`{ name: count }`) and `offset_m` is the origin's world
position (`OriginOffset`, meters):

- `terrain_brush_stroke`: an open `regia_terrain::TerrainStrokeSession` (an editor drag, a gameplay tool). Its points
  arrive over several frames; each is converted to terrain space when it arrives, and no rebase lands in the middle.
  `finish`, `cancel` or a failed finish lets go. A session that is dropped keeps its hold: release it with
  `release_rebase_hold(world, TERRAIN_STROKE_BLOCKER)`.
- `vehicle_contact_solve`: held by `drive_vehicles` while it casts the wheel rays and applies their impulses. The
  physics step runs before `StreamSet::Rebase`, so in the engine's order this never blocks a rebase.

Other stores follow a rebase instead of blocking it:

- Material graphs that read `WorldPosition`, `Triplanar` or `ObjectPosition` see world positions (the renderer adds
  the origin, `regia_render::surface_graph::graph_origin_slot`), so a world-space pattern stays on its world point.
- Terrain edits from local points (`apply_stroke_local`, `ramp_mode_local`, `raycast_local`, `brush_outline_local`)
  go through `OriginOffset`; the terrain itself stays in world meters. The brush preview gizmo draws through the
  origin.
- A vehicle's wheel contacts (`Vehicle::wheel_states`) move with the shift at once; the next update casts afresh.

## Opening a streamed level

`stream.open` opens a cooked level by guid. `regia run --level L` and a dedicated server's rotation do the same on their
own when `L` has a cooked world index (`StreamedLevelOpener`): the index and the always-loaded set are read, the cells
stream, and the source file is not parsed. A level with no cooked index opens the ordinary way, whole; an index that
does not read is an error, and the level does not open as a whole file instead. So run `regia cook` before flying a
streamed level through `regia run`.

## Terrain tiles

A game whose terrain tiles live in a `TileCache` inserts a `TerrainTileStream` resource over it. `TerrainPlugin` then asks
around every source in `StreamViews` once a frame and drains finished loads, so a teleport target's ground tiles load
with its cells. `stream.status` answers its counters as `terrain_tiles` (`resident`, `pending`, `failed`, `bytes`,
`budget`; `null` without a stream). Tiles are in the terrain's world-anchored space, so a rebase does not touch them.

### Terrain collision per tile

By default a terrain's heightfield is one collider over the whole map, and the `terrain_collision` gate opens a level's
cells all at once when it exists. Put `TerrainTileCollision { tile_m, view_radius_m, max_tiles_per_step }` on the
heightfield entity (it needs its own static `RigidBody`) and physics builds the heightfield as square tiles instead
(ADR-2317):

- a spawned cell holds the tiles under its rectangle until it unloads, and a source holds the tiles within
  `view_radius_m` of it, which is how cells with no content get ground; a tile stays while anything holds it, so a tile
  shared by four cells stays while any of them is loaded;
- the gate marks a spawned cell once all its tiles are colliders, and a cell with no content while the sources keep its
  tiles (the guard freezes bodies elsewhere);
- at most `max_tiles_per_step` tiles are built per fixed step, in tile order, so lockstep runs mark the same cells on the
  same steps; neighbors share their border samples, so there is no seam;
- tiles hang off the terrain's body, so a rebase moves them, and a hit on a tile reports the terrain entity.

`stream.collision_tiles` reads the counters (`stream.status` carries them as `collision_tiles`). Tiles and cells need not
line up; the cook warns `stream_terrain_misaligned` when neither size divides the other.

```ron
"regia_scene::streaming::TerrainTileCollision": (tile_m: 128.0, view_radius_m: 192.0, max_tiles_per_step: 16),
```

## Cutscenes

While a `SequencePlayer` with shots (or the shot player) plays in a streamed level, a scripted source named `sequence
prefetch` sits where its camera will be `StreamPrefetch::seconds` from now (3 s by default, `0` turns it off), so the next
stretch of the world is loaded when the camera gets there. It only adds cells: the origin's focus and the camera's implicit source stay
where they were (`StreamingSource::look_ahead`). It shows in `stream.sources` and goes when the sequence stops. A game
that does not stream never gets the entity.

Sequences and shot lists are authored in world meters. Across a rebase they stay put: the camera and root-entity tracks
convert with `OriginOffset`, and a held camera's saved gameplay pose moves with local space.

## Nav tiles

Set `nav: Some((cell_size: 1.0, tile_size: 16))` on `LevelStreaming` (`StreamNavCook`: the grid and the agent) and the
cook builds a nav grid over the level's static colliders (boxes, triangle meshes, heightfield colliders), and writes each
tile into the cell its center falls in. A cell with tiles and no entities joins the world for its tiles.
`stream_nav_no_geometry` warns when the level has nothing to walk on, and a grid past `MAX_STREAMED_GRID_CELLS` fails the
cook (`raise cell_size`). A collider counts when a static body sits on the same entity, the rule the run-time nav uses
when it rebuilds a region; a bodyless collider under a static body is left out of both. Both place a collider under a
parent at its parents' transforms composed (`regia_framework::nav_world::world_pose`), so a cooked tile and a run-time
rebuild of it hold the same triangles.

A `Terrain` is spawned by the game, not stored in the level file, so the cook's caller names it: wrap the cook's assets
in `regia_asset_pipeline::stream_cook::WithNavTerrain { assets, terrains: vec![NavTerrain::of(&terrain)] }` (or
implement `StreamAssets::nav_terrains`). Its surface, holes cut, goes into the grid with the colliders, exactly as the
run-time nav reads it from the `Terrain` component, and cells that only have terrain under them join the world for
their tiles:

```rust,wrap
# use regia_ecs::reflect::TypeRegistry;
# use regia_scene::{format::SceneFile, level::LevelId};
# let (level, scene, registry): (LevelId, SceneFile, TypeRegistry) = todo!();
# let cache_dir: std::path::PathBuf = todo!();
# let terrain: regia_terrain::Terrain = todo!();
use regia_asset_pipeline::stream_cook::{CacheAssets, NavTerrain, WithNavTerrain, cook_streamed_level};

let assets = CacheAssets::new(&cache_dir);
let with_terrain = WithNavTerrain { assets: &assets, terrains: vec![NavTerrain::of(&terrain)] };
cook_streamed_level(level, &scene, &registry, &with_terrain, Some(&cache_dir), "levels/world.level.ron")?;
```

A dedicated server (`regia::server::SimulationPlugins`) adds `NavStreamPlugin`, so its AI walks these tiles and a
cell's `sim_ready` (final on a server) waits for them; a client game still adds it itself.

A streamed level whose cells carry cooked nav tiles (`CellBlob::nav`) loads them when the game adds
`regia_nav::NavStreamPlugin`, which builds the streamed `NavTileCache` from the grid the world index names
(`StreamNavLayout`). The plugin registers the `nav` gate: a cell with
tiles is `sim_ready` once they are built (the region step builds them in the fixed step), a cell without tiles
passes at once, and a blob the cache refuses is counted in `NavStreamState::load_failures` and does not hold its
cell back. An unload takes the cell's tiles out. While a cell waits on its tiles, `stream.cells` lists `nav` in
its `missing_gates`.

## GPU memory

The cook's VRAM estimate of the spawned cells is `StreamStats.resident_vram_estimate` (`vram_over_budget` above
`vram_budget_bytes`), and each level is a load set named `stream_cells:<level guid>` in `render.memory_budget`, one item
per cell, resident while spawned. By default the planner's hard bound is RAM (`memory_budget_bytes`) and the VRAM
number is only reported.

`stream.bounds_set { "vram_bound": true }` (or `WorldStreaming::set_vram_bound(true)` in code) makes `vram_budget_bytes` a
second hard bound of the plan, with the memory budget's rules (ADR-2317): a cell loads only if the VRAM estimates of the
loaded and draining cells plus its own fit, a near cell may evict cells outside the load radius to fit, a cell whose own
estimate is over the budget is deferred as too large, and turning the bound on (or lowering `vram_budget_bytes`) sheds
the farthest cells. A budget of 0 (the dedicated server) means no bound. It applies to levels opened later too.

### When `visible` means on the GPU

By default the `render` gate marks a cell once its members' meshes resolved on the CPU, so `visible` can come a frame
before the renderer drew them. `stream.bounds_set { "render_residency": true }` (the `StreamRenderResidency` resource)
holds `visible` until the renderer drew every shown member once: its mesh is in the renderer's cache with the writes
submitted and its material is bound (which uploads its textures). Members an HLOD proxy hides are not waited for: the
proxy covers them, and they upload in the frame they show. Without a render device, or while no view draws, there is
nothing to wait for. A cell still not resident after 120 gate runs is marked anyway and counted in `timeouts`, which
then names a member the renderer never draws. `waits` counts gate runs that held a cell back.

```json
{"jsonrpc": "2.0", "id": 1, "method": "stream.bounds_set", "params": {"vram_bound": true, "render_residency": true}}
```

answers `{ "vram_bound": true, "render_residency": { "on": true, "waits": 0, "waiting_cells": 0, "timeouts": 0 } }`.
Then wait for the view as usual (`stream.ready` with `"readiness": "visible"`), and `stream.status` shows
`stats.vram_over_budget` staying `false`.

## Keeping changes across an unload

A cell that reloads comes back as cooked: streaming saves nothing. A game that needs a moved crate or an
opened door to survive reads the members on `CellUnloading`, stores what it needs by guid, and applies
it again on `CellLoaded`.

```rust
# use regia::prelude::*;
# use regia_scene::streaming::{CellMember, CellUnloading};
# use regia_scene::components::EntityGuid;
# #[derive(Component)] struct Door;
fn save_cell(mut unloading: MessageReader<CellUnloading>, members: Query<(&EntityGuid, &CellMember, &Door)>) {
    for message in unloading.read() {
        for (guid, member, door) in &members {
            if member.0 == message.cell {
                // Keep `door` under `guid` in the game's save data.
            }
        }
    }
}
```

Entity-owned lightmap data comes back with its cell: a member's `LightmapReceiver` entry, its `LightmapSurface` and its
`GiMobility` are part of the cooked cell, so a reloaded member finds the same lightmap entry by name
(`samples/stream_world/tests/live_world.rs`, `lightmap_receivers_come_back_with_their_cell`).

## Live acceptance

`samples/stream_world/tests/live_world.rs` is the release check for a large world (v0.5.0, spec 78 § 18). It cooks the
sample's 2048 m × 4096 m world (512 cells, about 100 000 entities) and checks what a game sees:

- Two teleports with no warning, one step apart: the first target's queued and in-flight loads are cancelled
  (`loads_cancelled` grows), nothing of them spawns later, no guid is in the world twice, and every spawned cell holds
  exactly its cooked `entity_count`.
- Through every rebase: static members stay within 1 mm of where the level put them, a dynamic body's `Transform`
  agrees with its physics body, and a nav mesh keeps its world position.
- The flight with the renderer: proxies become resident only through the renderer (never all of them), the cut has
  no hole after the teleport, the stream's VRAM estimate stays inside `vram_budget_bytes`, and the device's measured GPU
  memory (`render.memory_budget` `use.vram`) stays inside the declared tier (`STREAM_LIVE_TIER`, default `minimum`).
  Frame times are printed as one JSON line (`STREAM_LIVE_JSON`) and never asserted.

On a card, run the whole path at full size:

```sh
REGIA_GPU_UUID=<uuid> STREAM_LIVE_STRIDE=1 STREAM_LIVE_SIZE=2560x1440 STREAM_LIVE_JSON=live.json \
  cargo test --release -p stream_world --test live_world -- live_flight --nocapture
```

The same checks over the agent protocol, on a running game: `stream.timing_set { "mode": "lockstep" }`,
`stream.teleport`, then `play.wait_until` on `StreamStats.pending_waits == 0`, `play.expect` on `StreamStats.holes`
staying where it was and `StreamStats.vram_over_budget == false`, `render.memory_budget { "tier": "minimum" }` and
compare `use.vram` with `budget.vram`, and `seeing.screenshot`.

## Errors

`F0002` with `data.reason`:

| Reason | Meaning |
|---|---|
| `stream_not_enabled` | No streamed level is open. |
| `stream_unknown_cell` | The cell has no record in the world. |
| `stream_unknown_source` | No source has that id. |
| `stream_unknown_pin` | No pin has that id. |
| `stream_source_limit` | 64 sources and pins at most (pins plus enabled entity sources). The pin is not stored. |
| `stream_bad_budget` | A budget that cannot run, or an unknown budget name. |
| `stream_dev_only` | A development method in a shipping build. |
| `stream_lockstep_stall` | A lockstep wait hit `lockstep_wait_cap_ms`; the run's determinism is void. |
| `stream_format_newer` | The world index is newer than this build. The level is open with its always-loaded set; cell streaming did not start. |
| `stream_settings_invalid` | Cooked streaming settings failed validation (the level did not open), or a pin `radius_scale` is outside `0.1..=8`. |
| `stream_index_corrupt` | The world index is corrupt and the failure is not a settings check. The level did not open. |
| `stream_index_decode` | The world index payload did not decode. The level did not open. |
| `stream_spawn_failed` | The always-loaded set did not spawn. |
| `stream_no_assets` | The app has no asset server. |
| `stream_not_cooked` | The level has no cooked world index: it was never cooked, or its last cook stopped before it wrote the index (the cook removes the old index before its first changed blob, so a stopped cook never leaves an old index over new blobs, ADR-8812). Cook the level again. In a packaged game whose content chunk holding the index is not mounted, the message and hint name that chunk instead (`StreamOpenError::NotCooked { chunk }`, ADR-8862): `content.request` it, then `content.mount`. |
| `stream_rebase_blocked` | `stream.rebase` while something that cannot move with the origin is registered (`stream.origin` names it). |
| `stream_wait_area_refused` | A `stream.pin` `wait` over an area an open level's grid cannot list (its bounding box passes 2^22 cells, or it leaves the grid's range), or a `stream.teleport` target outside the grid's range: it could never be met. No pin is stored. |

Cell failure reasons in `stream.cells` and `CellFailed`: `cell_missing`, `cell_unavailable` (its chunk is
not mounted), `cell_corrupt` (header or hash), `cell_format_newer`, `cell_decode`. A failed cell retries
after `retry_after_steps`, up to `max_retries` times while wanted; a failed cell never stops the others.
`stream.retry` starts over.

Cook warnings, in `regia assets stream-report` and the cook log: `stream_settings_invalid` (an error),
`stream_large_entity`, `stream_cross_cell_ref`, `stream_hlod_switch_beyond_load`, `stream_nav_no_geometry`,
`stream_terrain_misaligned` (a tiled terrain collider whose tile size neither divides nor is divided by the cell size),
`stream_relevance_radius` (the net relevance reach the project hands the cook through `WithNetRelevance` is beyond
`load_radius_m`: clients then receive binds for cells they have not loaded, which they keep until the cell spawns),
`stream_priority_volume_invalid` (a `StreamPriorityVolume` whose half extents are not finite and strictly positive; the
volume is ignored).
