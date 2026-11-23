# Navigation: region rebuilds and tile streaming

A navmesh is built once from level geometry, and the world then changes under it: walls fall, terrain
is edited, streaming cells come and go. These methods keep a `regia_nav::NavTileCache` matched to
that world. Everything here works on a tile cache; see
[Navigation: tile cache and crowds](navigation-crowds.md) for the cache itself.

## Region rebuilds

A region rebuild re-voxelizes one world box on the cache's own grid, then rebuilds only the tiles
whose cells changed. The result inside the box equals a full build from the same geometry. Agents
in a `NavCrowd` replan on their own, because `revision` moves.

Game code uses `NavTileCache::rebuild_region(min, max, triangles)`, or a `NavRegionQueue`.
`NavTileCache::region_window(min, max)` gives the area the triangles have to cover. It is larger
than the box, because agent-radius erosion looks past it. Any geometry missing inside the window
reads as a hole, and geometry past the grid's edge is not seen: the grid does not grow.

`nav.region_rebuild` with `triangles` does this at once:

```json
{ "min": [-0.5, 0, -8], "max": [0.5, 3, 8],
  "triangles": [ [[-8, 0, -8], [8, 0, -8], [8, 0, 8]], [[-8, 0, -8], [8, 0, 8], [-8, 0, 8]] ] }
→ { "queued": false, "tiles": [[1, 0], [1, 1]], "cells_changed": 48, "outside": false,
    "deferred": [], "rebuilt": [[1, 0], [1, 1]], "pending": [], "revision": 3 }
```

Geometry that changes nothing queues no tiles and leaves `revision` alone. A box wholly off the grid
returns `"outside": true`. Corners may come in any order. With `"update": false` the tiles are only
queued (`pending`).

Without `triangles` the box goes on the queue and the app's geometry source serves it:

```json
{ "min": [-1, 0, -1], "max": [1, 2, 1] }   → { "queued": true, "waiting": 1 }
```

## Destruction and terrain edits

`regia_framework::nav_world::NavWorldPlugin` connects the world to the queue. Add it next to your
`NavTileCache`:

- `NavmeshInvalidated` (a chunk left a destructible) is read in the fixed step that wrote it, so a
  replay rebuilds on the same tick. That orders `NavRegionStep` and `NavCrowdStep` after
  `PhysicsStep`: do not order a crowd before physics in an app with this plugin.
- `TerrainHeightsChanged` (a runtime height patch) is read in `PostUpdate`, once per frame.

Its geometry source, `level_triangles_in`, reads terrain surfaces with their holes cut, static box,
triangle-mesh and heightfield colliders, and the attached chunks of destructibles, each as its convex
hulls (the triangles of the chunk's collider as physics holds it, ADR-2305; a chunk collider that is
not a hull falls back to its bounding box). A collider counts when a static body sits on its own
entity. It is placed at `world_pose`: the current `Transform`s of the entity and its parents composed,
so a static wall under a moved group is where the group puts it on the step it moved, and where the
streaming cook put it in a cooked tile (the cook takes the same triangles, so a rebuild over a cooked
tile keeps what the tile held). A heightfield collider and a `Terrain` over the same ground both
count; spawn one or the other as nav geometry. A game with other walkable geometry sets its own:
`NavRegionQueue::set_source`. A source returns
`false` when its geometry is not available yet, and the box is tried again on the next step, ahead
of everything behind it. Write `NavRegionDirty` to queue a box from game code.

## The queue and its budget

`NavRegionPlugin` runs in `FixedUpdate`. A box is cut along tile borders, and each slice is one unit
of work, so a blast across the map spreads over steps and ends on the same mesh. Slices run in
arrival order, so identical runs rebuild the same tiles on the same steps. Boxes covered by a queued
box are dropped, but a replay (below) never merges. `NavCrowdPlugin` leaves queued tiles to this step
when the app has a queue, so `max_slices_per_step` and `max_tiles_per_step` both hold with a crowd.

One 6 m box, re-voxelized and polygonised, took about 2.2 ms on a shared x86 server with 128 threads
(`perf_micro_region_rebuild`). The default four slices a step can spend several times that, on a
60 Hz tick, while a blast drains: lower `max_slices_per_step` on slow devices.

`nav.region_status` shows the queue:

```json
{}
→ { "queued": [ { "min": [-1, 0, -1], "max": [1, 2, 1], "source": "destruction" } ],
    "has_source": true, "max_slices_per_step": 4, "max_tiles_per_step": 16, "max_queued": 1024,
    "received": 12, "covered": 5, "overflowed": 0, "rejected": 0, "slices": 20,
    "cells_changed": 310, "source_waits": 0, "failed": 0 }
```

| Field | Meaning |
|---|---|
| `covered` | boxes dropped because another queued box covers them |
| `overflowed` | boxes merged into the last one because the queue was full. The merged box is larger; no edit is lost |
| `rejected` | non-finite boxes, and boxes that miss the grid |
| `source_waits` | steps a box waited because the source had no geometry yet, or no source is set |
| `failed` | rebuilds refused because the source returned non-finite vertices |

`source` is `destruction`, `terrain`, `replay`, `agent` or `game`.

## Tile streaming

A streamed world keeps its navmesh in cooked cells. The world streaming runtime calls these on a
cell's load and unload (spec 78 § 13.2):

| Call | When |
|---|---|
| `NavTileCache::build`, `tiles_in`, `cook_cell`, `layout` | at cook time, once per streaming cell |
| `NavTileCache::streamed` | at startup: an empty cache of the cooked layout |
| `load_cell(cell, bytes)` | a cell's blob arrived |
| `cell_ready(cell)` | the `nav` gate: its tiles are in the mesh and its edits are back |
| `unload_cell(cell)` | a cell is leaving |
| `shift_origin(delta)` | a floating-origin rebase |
| `find_path_streamed` | a path that may run into an unloaded tile |

`cell` is the caller's own `u64` key: it only has to name one cell for as long as that cell is loaded,
so a runtime with level and grid coordinates packs them into it.

A tile stays while any loaded cell holds it. Obstacles stay registered while their tile is away, and
apply again when it returns. A cooked cell is walkable cells, not polygons; the format is
[nav cell](../../docs/formats/nav-cell.md).

The same flow from the protocol, on a cache built whole:

```json
// nav.stream_cook_cell: the tiles under a box, in hex
{ "min": [-16, 0, -16], "max": [-2, 0, -2] }
→ { "tiles": [[0, 0]], "bytes": 16420, "blob": "524e5643..." }

// nav.stream_begin: swap in an empty streamed cache of the same layout.
// The whole cache is dropped: cook every cell you need first.
{}   → { "streamed": true, "tiles_total": 9 }

// nav.stream_load_cell
{ "cell": 7, "blob": "524e5643..." }
→ { "tiles": [[0, 0]], "added": [[0, 0]], "already_loaded": false, "ready": true, "revision": 1 }

// nav.stream_unload_cell
{ "cell": 7 }   → { "removed": [[0, 0]], "revision": 2 }
```

Loading a loaded cell changes nothing. With `"update": false` the tiles are only queued, and `ready`
stays false until an update rebuilds them. A `blob` that is not hex, or that decodes to more than 64 MiB,
is `F0002` with no `data.reason`.

`nav.stream_status` shows the state, with each cell's gate:

```json
{}
→ { "streamed": true, "tiles_total": 9, "tiles_loaded": 1, "cells_loaded": 1, "edited_tiles": 0,
    "replay_pending": 0, "cells": [ { "cell": 7, "ready": true } ], "origin": [-17, 0, -17],
    "width": 137, "height": 137, "format_version": 2 }
```

### Edits survive an unload

A tile that unloads and returns comes back as cooked, so an edit made to it would be lost. The cache
remembers the box of every edit per tile. When an edited tile loads, its cell is not ready until the
box is re-voxelized: `NavRegionQueue` does that on its next step, or call `take_replay_regions` and
feed `rebuild_region` yourself. Keep the geometry source serving the current world.

An edit over a tile that is not loaded skips its cells, reports the tile in `deferred`, and is
replayed the same way. Cells of an unloaded tile read as walkable during the rebuild, so a missing
neighbor does not erode the loaded surface beside it.

### Paths across missing tiles

`nav.stream_find_path` searches the loaded surface. If the end is out of reach and that surface
borders an unloaded tile, the path stops at the bordering point closest to the end. Optional `extents` is the
half-size of the box searched for the nearest polygon (default `[2, 4, 2]`) and `filter` the query filter
(`{ include_flags, exclude_flags, area_costs, exclude_areas }`, ADR-1934):

```json
{ "start": [-8, 0, -8], "end": [8, 0, -8] }
→ { "status": "nav_tile_unloaded", "waypoints": [[-8, 0, -8], [8, 0, -1]], "length": 17.5,
    "unloaded_tile": [1, 0] }
```

An end just off the tile grid counts as over the grid tile nearest to it: when that tile is not loaded, a path that
snapped onto the loaded surface is `nav_tile_unloaded` with that tile.

`status` is `complete`, `nav_tile_unloaded` or `no_path`. The search for the border crosses polygons and off-mesh
links, and an optional `filter` applies as on [`nav.find_path`](navigation-lanes.md#filters). The optional
`extents` (half-extents of the box that snaps `start` and `end` onto the mesh) defaults to `[2, 4, 2]`.

Crowd agents on a tile that left the world go `dormant` until it returns, and a crowd agent sent with
`nav.crowd_set_target { streamed: true }` walks to this border and waits there (`awaiting_tile`) until the tile
loads: see [area costs and crowd lanes](navigation-lanes.md#streaming). `NavCrowd::shift_origin` and
`NavLaneGraph::shift_origin` move agents and lanes with `NavTileCache::shift_origin` on a rebase, and
`NavRegionQueue::shift_origin` moves the boxes waiting. Every nav plugin adds `regia_nav::NavOriginPlugin`, which
applies each `OriginShifted` rebase to all of these (and to the `NavMesh`, `NavObstacles` and every
`NavAgentState`'s waypoints) once, in the rebase's fixed step; call the `shift_origin` methods yourself only in an
app without the nav plugins. A replay the cache refuses is given up and counts in `failed`:
the tile stays as cooked and the gate opens.

## Errors

Errors carry `data.reason`:

| Reason | Meaning |
|---|---|
| `nav_no_tile_cache` | no `NavTileCache` resource |
| `nav_no_region_queue` | no `NavRegionQueue`: add `NavRegionPlugin` or `NavWorldPlugin` |
| `nav_invalid_region` | a non-finite box or vertex; nothing changed |
| `nav_not_streamed` | a streaming call on a cache built whole |
| `nav_invalid_layout` | a layout with no cells, too many or a non-finite origin, or cooking a tile that is not loaded |
| `nav_tile_out_of_range` | a tile outside the grid |
| `nav_cell_corrupt` | a truncated or damaged blob; nothing changed |
| `nav_cell_version` | a blob from a newer format version |
| `nav_cell_layout_mismatch` | a blob cooked for another layout or other build parameters |
| `nav_unknown_cell` | that cell is not loaded |
