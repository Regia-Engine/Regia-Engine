# World streaming

A streamed level is cut into a grid of square cells. The cook writes each cell as its own blob, and
at run time the engine loads the cells around your players and cameras, unloads the ones left
behind, and draws far cells as cheap merged proxies (HLOD). Memory stays under a hard budget, and the
game never waits on disk inside a frame. This chapter takes you from an ordinary level to a streamed
one and shows how to check that it behaves. The agent-facing reference is
[World streaming runtime](../agents/streaming-runtime.md); the data-model methods are on
[World streaming and HLOD](../agents/world-streaming.md).

## 1. Turn a level into a grid level

Streaming is a resource on the level. In `levels/world.level.ron` (or `level.ron` inside a
`.level.d` directory) add it to the `resources` map:

```ron
"regia_scene::streaming::LevelStreaming": (
    enabled: true,
    cell_size_m: 128.0,
    load_radius_m: 512.0,
    unload_radius_m: 640.0,
)
```

`enabled` defaults to `false`: without it the level is not streamed at all. The fields you will
change most:

| Field | Default | Meaning |
|---|---|---|
| `cell_size_m` | 128 | Side of a cell on the X/Z plane, 8 to 4096 meters. |
| `load_radius_m` | 512 | A cell this close to a source loads. |
| `unload_radius_m` | 640 | A cell beyond this from every source unloads. It may not be below the load radius; the gap keeps cells from flickering as you walk along an edge. |
| `max_entity_extent_cells` | 2 | An entity wider than this many cells stays with the level instead of living in one cell. |
| `hlod` | on | Distant-cell proxies. `None` turns them off. |
| `nav` | none | Cook navigation tiles into the cells. |
| `origin` | on | Floating origin (section 6). |
| `budgets` | by platform | Section 4. |
| `tuning` | see below | Retry, guard and timing knobs. |

A bad value stops the cook, and the message names the field (for example
`unload_radius_m 400 is below load_radius_m 512`). Pick a cell size so that the load radius covers
about three to five cells: much smaller and the planner juggles thousands of cells; much larger and
each load is a hitch.

### Where entities go

At cook time each entity goes to the cell holding the center of its bounds, and a hierarchy moves
with its root. Four things override that:

- `StreamPlacement::AlwaysLoaded` keeps the entity with the level for good (a sky dome, a manager).
- `StreamPlacement::Cell(x: 7, z: -3)` forces a cell.
- `Persistent` entities, entities with no transform and entities wider than
  `max_entity_extent_cells` also stay with the level.
- A `StreamPriorityVolume` (half extents and a priority) raises the load priority of the cells it
  overlaps, so a boss arena loads before the meadow beside it.

```ron
"regia_scene::streaming::StreamPlacement": Cell(x: 7, z: -3),
```

Put that line in the `components` map of the entity, next to its `Transform`.

## 2. Cook and inspect the cells

```text
regia assets stream-report levels/world.level.ron
regia cook
```

`stream-report` partitions the level without writing anything: it prints the cell count, entities
and bytes per cell, the always-loaded set and every cook warning with its code. Add `--json` for
scripts, `--project DIR` and `--platform linux|windows|android` to choose the project and the
cache that sizes dependencies. `regia cook` then writes one blob per non-empty cell, the
always-loaded set and a world index. Read the warnings before you play:

| Warning | What to do |
|---|---|
| `stream_large_entity` | An entity spans more cells than allowed, so it stays always loaded. Split it, or raise `max_entity_extent_cells`. |
| `stream_cross_cell_ref` | An entity refers to one in another cell. The reference reads as absent while either cell is not spawned. |
| `stream_hlod_switch_beyond_load` | More than 5 percent of leaves would switch to full detail beyond `load_radius_m`, so proxies draw where full detail was wanted. Raise `load_radius_m` or the HLOD `screen_size_threshold`. |
| `stream_terrain_misaligned` | A tiled terrain collider whose tile size neither divides nor is divided by the cell size. |
| `stream_nav_no_geometry` | Nav tiles were asked for and the level has nothing to walk on. |
| `stream_relevance_radius` | The net relevance reach is beyond the load radius. |
| `stream_settings_invalid` | An error: the settings above did not validate. |

A level opened from its source file spawns whole. To fly the streamed one, cook first and then run
it: `regia run --level levels/world.level.ron`. When the level has a cooked world index, `regia run`
and a dedicated server's level rotation stream it. An index that does not read is an error; the
level does not quietly open whole instead.

## 3. Sources

Cells load around streaming sources. Put a `StreamingSource` on the entity that should pull the
world in:

| Field | Default | Meaning |
|---|---|---|
| `kind` | `Camera` | `Camera`, `Player` or `Scripted` (a cutscene, a teleport target, an AI far away). |
| `enabled` | true | A disabled source counts for nothing. |
| `radius_scale` | 1 | 0.1 to 8. A source with scale 2 wants cells twice as far. |
| `priority` | 0 | Added to the priority of every cell it wants. |
| `view_bias` | 0.5 | 0 to 1. Pulls cells in the view direction forward in the load order. It changes the order only, never which cells are wanted, so turning the camera unloads nothing. |
| `predict_s` | 0 | 0 to 4 seconds. Adds a look-ahead point where the source will be. |
| `drives_origin` | false | This source is the floating-origin focus. At most one source has it. |

With no enabled source, the active render camera is the source. There can be up to 64 sources and
pins in all. A **pin** is a source with no entity (`stream.pin`), handy for keeping a distant
objective loaded.

To jump across the map, use `stream.teleport` rather than moving the player: it pins the target
with a priority boost and reports `ready` once the ground is there. Pass `move_source` and the
engine moves the source's entity for you at that moment. During a cutscene the sequencer adds its own
`sequence prefetch` source where the camera will be 3 seconds from now (see
[the sequencer chapter](sequencer.md)).

## 4. Budgets

Every budget has a platform default (`StreamBudgets::for_platform`). Override them in the
`budgets` field only when you have measured:

| Budget | Desktop | Deck | Android |
|---|---|---|---|
| `max_loads_per_step` | 8 | 4 | 2 |
| `max_inflight` (cells loading at once) | 8 | 4 | 4 |
| `max_pending_bytes` (decoded, waiting to apply) | 256 MiB | 128 MiB | 64 MiB |
| `apply_budget_ms` (main-thread spawn time per frame) | 2.0 | 1.5 | 1.0 |
| `apply_budget_entities` | 400 | 300 | 150 |
| `memory_budget_bytes` (resident cells, a hard bound) | 2 GiB | 1 GiB | 512 MiB |
| `vram_budget_bytes` | 2.5 GiB | 1 GiB | 384 MiB |
| `hlod_budget_bytes` (resident proxies) | 256 MiB | 128 MiB | 64 MiB |

`memory_budget_bytes` is the one the planner never exceeds, measured against the cook's size
estimates. The VRAM figure is only reported by default. Turn on
`stream.bounds_set { "vram_bound": true }` to make it a second hard bound. A dedicated server has
no VRAM or HLOD budget (0 means no bound).

A hierarchy is never half spawned: one larger than the entity budget spawns alone and counts in
`apply_over_budget`. Memory is released over several frames, so an unload does not make room until it
finishes. `max_inflight` must be at least 1 and `apply_budget_entities` at least 1.

The `tuning` block holds the other knobs: `plan_interval_steps` (30), `retry_after_steps` (300),
`max_retries` (3), `guard_margin_m` (4), `wait_on_open` (true), `hlod_fade_frames` (none, which
means instant switches) and the two lockstep values used by tests.

### Frozen bodies and loading screens

A dynamic body or character near a cell that is not yet ready is frozen: it keeps its velocity
and wakes when the ground exists, so nothing falls through a floor that has not loaded. If your
player freezes often, the budget is too small or the load radius too short. With `wait_on_open`
the open waits until the cells around the start sources are ready: show a loading screen until the
`StreamStats.pending_waits` counter is 0.

## 5. HLOD: distant proxies

With `hlod` set, the cook merges the static meshes of each cell into a proxy and simplifies it;
neighboring proxies merge into parents, up to `max_levels` passes. At run time each leaf cell is
covered exactly once by something that is ready: the cell itself, its own proxy, or the nearest
resident ancestor. A cell the proxy covers is hidden but keeps simulating.

`HlodSettings` fields: `screen_size_threshold` (when unset, the cook derives it so a leaf switches
just inside the load radius), `fov_y_rad`, `merge_factor`, `max_levels`, `proxy_byte_ratio`,
`leaf_triangle_ratio` (0.05), `parent_triangle_ratio` (0.25) and `min_triangles`.

By default a proxy draws in one flat color, the area-weighted average of the colors it merged. To
keep texture detail on the horizon, bake atlases at cook time:

```ron
"regia_scene::streaming::LevelStreaming": (
    enabled: true,
    cell_size_m: 128.0,
    load_radius_m: 512.0,
    unload_radius_m: 640.0,
    hlod_atlas_px: 8,
    hlod_atlas_max_px: 1024,
)
```

`hlod_atlas_px` is texels per meter of proxy surface at the finest level (0 keeps flat colors);
`hlod_atlas_max_px` (16 to 4096) caps one atlas side. A proxy that would need more bakes coarser
and the cook warns `stream_hlod_atlas_capped`; one that cannot bake warns
`stream_hlod_atlas_failed` and draws flat. Baked textures count against `hlod_budget_bytes`, so
raise that budget along with the atlas. Check the bake with `world.hlod_proxy`: each node should
have an `atlas`, nothing should be `capped` that you did not expect, and `miss_ratio` should stay
near 0.

Switches between a cell and its proxy are instant. Set `tuning.hlod_fade_frames` to dissolve over
that many frames instead (it is opt-in until the look is signed off).

## 6. Floating origin

Single-precision positions lose detail far from zero. Streaming keeps `Transform` in `f32` around
an origin cell. When the focus strays more than `origin.threshold_m` (default 5000) from the
origin, the engine shifts the origin by whole cells (`origin.cell_size_m`, default 1024) at a fixed
step boundary, and everything that stores a position follows: physics, the camera, lights and
probes, audio, navigation, effects. Positions in agent replies are always world positions in
`f64` meters, so a rebase does not change what a script sees.

Rules for your code:

- Author levels, sequences and shot lists in world meters; they convert as they spawn.
- Anything of yours that stores a local position must handle the `OriginShifted` message in the same
  frame, or register in `RebaseBlockers` to hold rebases off until it can follow.
- Terrain edits from a local position go through `OriginOffset`.
- A world within about 8 km of its origin needs no rebase. Set `origin.enabled: false` to turn it
  off. The settings are checked: `cell_size_m` must be a whole number of meters and
  `threshold_m` at least half of it.

`stream.origin` shows the origin cell, the threshold, what is blocking a rebase (`blocked_by`, `holds`)
and the last rebase.

## 7. Watch it work

Turn on the stream overlay while you play:

```json
{"jsonrpc": "2.0", "id": 1, "method": "stream.overlay", "params": {"on": true}}
```

It is a development-build tool (a shipping build refuses it with `stream_dev_only`). Every cell within
twice the unload radius of the focus gets a border colored by state, and a cell that an HLOD proxy
covers gets a cross:

| Color | State |
|---|---|
| Green | `visible` |
| Cyan | `sim_ready` |
| Blue | `spawned` (waiting for a gate) |
| Yellow | `queued`, `loading` or `applying` |
| Orange | `unloading` |
| Red | `failed` |
| Gray | `unloaded` |

A cell stuck in blue is waiting for a gate (`physics`, `terrain_collision`, `render`, or `nav`);
`stream.cells` names the missing one in `missing_gates`. A red cell shows its reason there too:
`cell_missing`, `cell_unavailable`, `cell_corrupt`, `cell_format_newer` or `cell_decode`. A failed
cell retries on its own up to `max_retries` times while it is wanted, and never stops the others.
`stream.retry` starts over. `stream.trace` lists every state change with its step.

A repeatable check, in lockstep timing where a load issued at step N applies at step N plus a fixed
latency whatever the disk does:

1. `stream.timing_set { "mode": "lockstep" }`
2. `stream.teleport` to the far end of the map.
3. `play.wait_until` on the resource `StreamStats.pending_waits == 0`.
4. `seeing.screenshot`, with the overlay on.
5. `play.expect` on `StreamStats.holes == 0`.

`holes` counts leaves nothing covers; with root proxies resident it stays 0. Between two lockstep
runs `StreamStats.trace_hash` repeats exactly.

### stream-bench

In the engine repository, `cargo xtask stream-bench` runs the teleport benchmark: a 2 km teleport in
async timing, reporting the time to fill the view and the frames over 33 ms. Options:
`--size 2560x1440` (the default), `--json FILE` and `--bounds` (turns on the VRAM bound and the
render residency wait). It runs the ignored `teleport_bench` test of the `stream_world` sample in a
release build, one run at a time, so give it a card of its own. Use it to compare budget changes
before and after, on the same hardware.

## Keeping changes across an unload

A cell that reloads comes back as cooked: streaming saves nothing. A door that was opened or a crate
that was moved must be saved by your game. Read the members on the `CellUnloading` message, store
what you need by guid, and apply it again on `CellLoaded`. The example is on the
[runtime page](../agents/streaming-runtime.md).

## Next

- [The sequencer](sequencer.md) prefetches cells ahead of a cutscene camera.
- [Dedicated servers](dedicated-server.md) stream the same cooked levels with a server budget.
- For navigation across cells, add `regia_nav::NavStreamPlugin` and set `nav` on the level.
