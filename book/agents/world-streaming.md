# World streaming and HLOD

The data model for large worlds in `regia_scene` (ADR-1566): a grid of streaming cells, the
planner that decides which cells to load, the HLOD tree that stands in for distant cells, and
the coordinate type for worlds too big for `f32`. It is pure logic: the host performs the loads
and draws; these pieces decide. The three methods below run the same code on data you pass in.
They are stateless and read-only (`read` permission) and add no error codes: bad input answers
`F0002` with the reason in the message.

## The pieces

- **Grid** (`regia_scene::stream_grid`). Square cells on the X/Z plane, `cell = floor(p / size)`,
  addressed by `CellCoord { x, z }` (`i64`). The text id of a cell is `"<x>_<z>"`, such as `"-3_12"`.
- **Planner** (`stream_plan`). Register cells with a memory cost and a priority, then call
  `plan(viewers)` each streaming tick and `apply(plan)` once the host has done the work.
  - A cell loads when it is within `load_radius` of a viewer; a loaded cell unloads only beyond
    `unload_radius` (at least the load radius). In between it keeps its state, so walking along
    an edge does not make cells flicker.
  - Candidates load highest priority first, then nearest, then by cell coordinate: the same
    input always gives the same order.
  - The loaded bytes never exceed `memory_budget_bytes`. A cell that does not fit may evict
    loaded cells outside the load radius (farthest first, reason `MakeRoom`); otherwise it is
    deferred (`OverBudget`). A cell bigger than the whole budget is `TooLarge`. Lowering the
    budget sheds the farthest cells (`OverBudget`). `max_loads_per_step` rate-limits (`RateLimited`).
- **HLOD tree** (`hlod`). Each cell has a proxy. `merge_factor x merge_factor` neighbours merge
  into a parent, repeatedly, up to `max_levels` passes or one root. A node draws its proxy
  instead of its children once its bounding sphere covers at most `screen_size_threshold` of the
  viewport height, which is a distance of `radius / (threshold * tan(fov / 2))`. Leaves near the
  viewer draw `Full`. Every leaf is covered exactly once.
- **Large positions** (`floating_origin`). `LargePosition { cell: [i64; 3], local: [f32; 3] }`
  keeps `f32` precision at any distance (the offset is at most half a cell). `FloatingOrigin`
  chooses an origin cell; `update(focus)` returns a `Rebase` when the focus strays past the
  threshold, and every local-space position is shifted by it.

The runtime that drives these pieces in a running game (`regia_world`: streaming sources, cooked
cells, budgets, readiness gates, the HLOD handoff, the floating-origin rebase and the `stream.*`
methods) has its own page: [Streaming runtime](streaming-runtime.md) (spec 78).

## `scene.stream_plan`

```json
{ "method": "scene.stream_plan", "params": {
    "cell_size": 100, "load_radius": 450, "unload_radius": 500, "memory_budget_bytes": 35,
    "viewers": [[50, 50]],
    "cells": [{ "x": 0, "z": 0, "bytes": 10 }, { "x": 1, "z": 0, "bytes": 10, "priority": 2 },
              { "x": 2, "z": 0, "bytes": 10, "loaded": true }] } }
→ { "loads": [{ "cell": { "x": 1, "z": 0 }, "distance": 50.0, "bytes": 10, "priority": 2 },
              { "cell": { "x": 0, "z": 0 }, "distance": 0.0, "bytes": 10, "priority": 0 }],
    "unloads": [], "deferred": [], "resident_bytes_before": 10, "resident_bytes_after": 30,
    "memory_budget_bytes": 35 }
```

`loaded` says what is resident now. `unload_radius` defaults to 1.25 times `load_radius`, the
budget to unlimited. Unloads are listed first and should be done before the loads.

## `scene.hlod_select`

```json
{ "method": "scene.hlod_select", "params": {
    "cell_size": 100, "fov_deg": 60, "screen_size_threshold": 0.1,
    "cells": [{ "x": 0, "z": 0, "max_y": 40, "bytes": 1000 }, { "x": 1, "z": 0, "max_y": 40, "bytes": 1000 }],
    "viewers": [[5000, 0, 0]] } }
→ { "node_count": 3, "leaf_count": 2, "max_level": 1, "roots": [2],
    "draws": [{ "node": 2, "kind": "Proxy", "level": 1, "cell": null,
                "switch_distance": 1967.6, "proxy_bytes": 500 }], "proxy_bytes": 500 }
```

`kind` is `Proxy` or `Full` (only leaves, which carry the cell id in `cell`). Defaults: heights
0 to 100, `merge_factor` 2, `max_levels` 8, `proxy_byte_ratio` 0.25 (a parent proxy costs that
fraction of its children's).

## `scene.large_position`

```json
{ "method": "scene.large_position", "params": {
    "cell_size": 1024, "position": [5000000001.002, 12.5, -3000], "origin": [5000000000, 0, -3000] } }
→ { "cell": [4882813, 0, -3], "local": [-510.998, 12.5, 72.0], "world": [5000000001.002, 12.5, -3000.0],
    "relative_to_origin": [1.002, 12.5, 0.0] }
```

`relative_to_origin` is computed from the cell difference and the offsets in `f64` and rounded
once, so it stays millimeter accurate where a plain `f32` world position could not tell 1 m apart.

## Checking your work

The flow that proves a layout: send `scene.stream_plan` with your cell sizes and costs and a
viewer path, apply the loads in your own data, repeat, and confirm `resident_bytes_after` stays
at or under the budget and `deferred` empties as the viewer settles.

## Limits

`cell_size` 0.001 to 10 000 000; up to 64 viewers and 100 000 cells per call.

## `world.hlod_proxy` (baked proxy atlases)

A streamed level can bake a texture atlas for every HLOD proxy at cook time (ADR-2483), so a distant
cell keeps its texture detail instead of one flat color. Turn it on in the level's
`LevelStreaming` resource:

```ron
"regia_scene::streaming::LevelStreaming": (enabled: true, hlod_atlas_px: 8, hlod_atlas_max_px: 1024)
```

- `hlod_atlas_px`: texels per meter of proxy surface at tree level 0; a level `L` parent bakes at this
  over `2^L`. `0` (the default) keeps flat-color proxies, and the cook writes exactly what it wrote
  before.
- `hlod_atlas_max_px`: the largest atlas side (16 to 4096, default 1024). A proxy that would need more
  is baked coarser and the cook warns `stream_hlod_atlas_capped`; a proxy that does not bake at all
  warns `stream_hlod_atlas_failed` and draws flat.

A baked proxy's textures count in the level's `hlod_budget_bytes`, so a budget set for flat proxies may
now hold fewer of them, and the cut coarsens instead of overflowing. Raise it with the atlas if the
distant cells should stay at leaf detail.

Each baked proxy gets an ordinary cooked standard material over three textures (base color, normal,
packed occlusion-roughness-metallic), compressed like every texture of the platform (BC7 on desktop,
ASTC on Android). The renderer draws the proxy with it; a material that does not load leaves the
proxy flat. `world.hlod_proxy` reads the cooked world index and reports what the bake did:

```json
{ "method": "world.hlod_proxy", "params": { "level": "8f1c…", "node": 0 } }
→ { "level": "8f1c…", "format_version": 2, "hlod_atlas_px": 8, "hlod_atlas_max_px": 1024, "atlases": 3,
    "nodes": [{ "node": 0, "level": 0, "cell": "0_0", "mesh": "…", "color": [0.6, 0.3, 0.2, 1.0],
                "atlas": { "material": "…", "textures": ["…", "…", "…"], "size": [128, 64],
                           "texels_per_m": 8.0, "asked_texels_per_m": 8.0, "capped": false,
                           "bake_error_m": 0.012, "miss_ratio": 0.0, "alpha_masked": false },
                "proxy_bytes": 104857 }] }
```

Params: `path` (the cooked world index `.fckd`, inside the output roots) or `level` (the level guid;
the index is read from this project's cooked cache for `platform`, a tag such as `linux` or `android`,
default this host's); `node`
(optional) narrows the reply to one tree node. Permission `assets`; read-only.

- `size`: atlas width and height in texels. `texels_per_m` is what the atlas reached; when `capped` is
  true it is below `asked_texels_per_m`.
- `bake_error_m`: root mean square distance, in meters, between the proxy surface and the source
  surface each texel sampled. It grows with simplification; a value near the proxy's size means the
  proxy no longer follows its cell.
- `miss_ratio`: share of texels whose short ray pair along the proxy normal found no source surface: none
  in reach, or only holes of an alpha-masked material with nothing behind them (they are filled from their
  neighbors). Above a few percent on opaque content, the proxy has surfaces the cell does not; an open canopy
  reports its holes here.
- `alpha_masked`: the proxy's material cuts holes (ADR-2713). A bake ray that lands in a hole of an
  alpha-masked source goes on through it, up to 8 times within its reach, so a card in front of a wall bakes
  the wall's color behind its holes; the texel's coverage (the atlas base color's alpha) is 0 where the
  nearest thing the rays found is a hole. Any such texel makes the material alpha-tested with cutoff 0.5.
  `null` when the cooked material is not beside the index.
- `proxy_bytes`: what the node's proxy weighs in `hlod_budget_bytes`. A baked leaf counts its mesh plus the
  three cooked texture files written for it, exactly (ADR-2713); a parent counts `proxy_byte_ratio` of its
  children's sum.
- `atlas: null`: the node draws with its flat `color` (atlas off, nothing to draw, or a version 1 index).

Error codes: `hlod_index_unreadable` (the file is missing or is not a cooked world index),
`hlod_no_proxies` (the level cooked without HLOD), `hlod_node_unknown` (`node` is past the tree).
`hlod_voxels_unreadable`: the level's HLOD voxel table (`.wvoxels`, terrain scatter proxies) is present but
does not decode ([HLOD scatter voxel proxies](hlod-scatter-voxels.md)).
Missing both `path` and `level`, or a malformed value, answers `F0002`.

A mesh drawn with a material graph (`.matgraph.ron`) or instance (`.matinst.ron`) bakes the graph folded to
constants: parameters at their defaults or the instance's values, constants, math nodes and static switches
evaluated. Texture samples read white, so the atlas holds the graph's tint and factors (and a constant
normal and occlusion), not its texture detail. `regia cook` reads the graph sources from the project.

Check a bake: cook the level, call `world.hlod_proxy`, and confirm every node with a `mesh` has an
`atlas`, nothing is `capped` that you did not expect, and `miss_ratio` stays near 0. Then look at the
level from far away (the cells draw as proxies past `load_radius_m`) and compare against the same view
with `hlod_atlas_px: 0`.

## Soak: long runs, budgets, leaks

`regia_world/tests/soak.rs` walks three streaming sources over a generated 24 by 24 cell world (6 km a side)
for more than 5,000 fixed steps, headless, in lockstep: a random walk, teleports, fast flyovers across the
floating origin's 5 km threshold in both directions, and a level switch (close the world, open an empty one,
come back). It uses counters and fixed steps only, never the wall clock.

After each phase the sources move to fresh ground and the run settles. Then:

- the spawned cell set equals what a fresh planner orders for those sources, every other cell is `Unloaded`;
- entities and `CellMember` counts equal the baseline plus the loaded cells' entities; none outlives its cell;
- when the sources leave, entities, the `stream_cells` load-set entries, `pending_bytes`, `inflight` and the
  resident byte counters return to zero;
- the same seed gives the same `trace_hash` and `world_hash` at every phase end on two runs.

Every step also checks its budgets: `last_apply_entities` against `apply_budget_entities`, `inflight` and reads
per step against `max_inflight`, `pending_bytes` against `max_pending_bytes`, plans per step (at most one) and the
cells a plan walks against the registered cells, and `apply_over_budget`, `failures`, `lockstep_stalls` and
`sources_dropped` staying zero.

The high-water marks live in `StreamStats` (`stream.stats`, `play.wait_until`, `play.expect`):
`peak_inflight`, `peak_pending_bytes`, `peak_apply_entities`, `peak_loads_per_step`, `peak_plan_cells` and the
running `plan_cells_evaluated`. Check a long run of your own game by playing it and then asserting each peak is
within the matching budget in `stream.budgets`.

Run the soak on its own:

```
cargo xtask stream-bench --soak [--seed N] [--json soak.json]
```

It needs no card. It builds `regia_world`'s soak test in release and prints one JSON line with the seed, the
step count and the final trace hash. A failing seed reproduces: pass the same `--seed`.

