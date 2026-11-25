# Stream a level

<!-- index: World streaming -->
<!-- index: Streaming cell -->
<!-- index: HLOD -->
<!-- index: Floating origin -->

A streamed level is a grid of square cells. The cook writes one blob per cell. At run time the
engine loads the cells around the players and cameras, unloads the ones left behind, and can
draw a far cell as a merged proxy (HLOD). This is the path from an ordinary level to that grid.
The chapter is [World streaming](../../user-guide/world-streaming.md)
(`docs/specs/78-world-streaming.md`). The agent pages are
[World streaming runtime](../../agents/streaming-runtime.md) and
[World streaming and HLOD](../../agents/world-streaming.md).

This is not the terrain texture cache. Height and material tiles stream on their own. See
[Terrain data](../../user-guide/terrain-data.md). Procedural cells around an anchor are a
different grid again. See [Generate procedural content](procedural.md).

## 1. Turn streaming on

In `levels/world.level.ron` (or `level.ron` inside a `.level.d` directory), add this to the
`resources` map. `enabled` defaults to false. Without it, the level is not streamed.

```ron
"regia_scene::streaming::LevelStreaming": (
    enabled: true,
    cell_size_m: 128.0,
    load_radius_m: 512.0,
    unload_radius_m: 640.0,
),
```

`unload_radius_m` must be at least `load_radius_m`. The gap stops a cell on the edge from
loading and unloading as you walk. Pick a cell size so the load radius covers about three to
five cells. The other fields (HLOD, navigation tiles, floating origin, budgets) are in the
chapter.

Put a `StreamingSource` on each entity that should pull cells in (the player, a camera, a
scripted point). With no enabled source, the active render camera is the source. To jump across
the map, call `stream.teleport`. Moving the player by a large step does not pin the landing
site first.

An entity that must stay loaded (a sky, a manager) uses `StreamPlacement::AlwaysLoaded`. An
entity wider than `max_entity_extent_cells` stays with the level as well.

## 2. Read the partition before you cook

```sh
regia assets stream-report levels/world.level.ron
regia assets stream-report levels/world.level.ron --json
```

This writes nothing. It prints the cell count, the entities and bytes in each cell, the
always-loaded set, and every warning with its code. Fix these before you cook:

| Warning | What to do |
|---|---|
| `stream_settings_invalid` | A setting failed validation. The message names the field. |
| `stream_large_entity` | Split the entity, or raise `max_entity_extent_cells`. |
| `stream_cross_cell_ref` | A reference into another cell reads as absent while either cell is not spawned. |
| `stream_hlod_switch_beyond_load` | Raise `load_radius_m` or the HLOD screen-size threshold. |
| `stream_terrain_misaligned` | Make the terrain tile size divide the cell size, or the other way around. |
| `stream_nav_no_geometry` | Nav tiles were requested and the level has nothing to walk on. |
| `stream_relevance_radius` | Net relevance reaches past the load radius. |

## 3. Cook and run the cooked level

```sh
regia cook
regia run --level levels/world.level.ron
```

Opening the source level spawns it whole. `regia run` streams it when a cooked world index is
present. An index that does not read is an error. The level does not quietly open whole instead.

## 4. Watch the cells

In a development build:

```json
{"jsonrpc": "2.0", "id": 1, "method": "stream.overlay", "params": {"on": true}}
```

A shipping build refuses this with `stream_dev_only`. Green is visible, yellow is still loading,
red has failed, gray is unloaded. A cell an HLOD proxy covers gets a cross. A cell stuck waiting
names the missing gate in `stream.cells` (`physics`, `terrain_collision`, `render`, or `nav`).
The color table is in the chapter.

HLOD is on unless you set `hlod: None`. A proxy covers a distant cell exactly once: the cell,
its own proxy, or a resident ancestor. Switches are instant unless
`tuning.hlod_fade_frames` is set. Atlas baking is optional (`hlod_atlas_px`). Check a bake with
`world.hlod_proxy`: each node you expected should have an `atlas`, and `miss_ratio` should stay
near 0.

## 5. Keep gameplay across an unload

A cell that reloads comes back as cooked. An opened door or a moved crate is gone unless your
game stored it. Read the members on `CellUnloading`, store what you need by guid, and apply it
on `CellLoaded`. The example is on the [runtime page](../../agents/streaming-runtime.md).

Floating origin is on by default. Positions in agent replies stay world meters in `f64`. Code
that stores a local position follows `OriginShifted` in the same frame, or registers in
`RebaseBlockers`. A world within about 8 km of its origin can set `origin.enabled: false`.

## Finished check

All of these are true:

1. `regia assets stream-report` exits 0 and does not print `stream_settings_invalid`.
2. `regia cook` exits 0, and `regia run --level` on that level streams (the overlay shows a
   neighborhood of cells, not one solid load of the whole map).
3. After `stream.timing_set` with `"mode": "lockstep"`, a `stream.teleport` to the far end, and
   `play.wait_until` on `StreamStats.pending_waits == 0`, `play.expect` sees
   `StreamStats.holes == 0`. `holes` counts leaves that nothing covers.
4. A second lockstep run of the same teleport repeats `StreamStats.trace_hash`.

`cargo xtask stream-bench` times a 2 km teleport. It needs a GPU and a release build of the
`stream_world` sample. It is a comparison tool, not the check above. The lead runs it.
