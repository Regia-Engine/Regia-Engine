# Navigation: tile cache and crowds

Drive runtime obstacles and crowd steering without touching gameplay code. The tile-cache methods
need a `regia_nav::NavTileCache` resource; the crowd methods walk on the tile cache's mesh when one
exists, otherwise on the `regia_nav::NavMesh` resource. For plain path queries see
[navigation](navigation.md).

The tile cache splits the walkable surface into square tiles. Adding or removing an obstacle
rebuilds only the tiles its footprint touches (the footprint is grown by the agent radius). Crowd
agents follow navmesh corridors, avoid each other and walls by sampling velocities, and move in
whole fixed steps, so two identical runs give identical results.

## Tile cache status

`nav.tile_cache_status` reports the tile grid and the rebuild counters:

```json
{}
→ { "tiles": [5, 5], "tile_count": 25, "tile_size": 8, "tile_world_size": 2.0, "polys": 31,
    "regions": 1, "obstacles": [ { "id": 0, "shape": "cylinder", "base": [2, 0, 1],
    "radius": 0.6, "height": 1.8 } ], "pending": [], "total_rebuilds": 1, "revision": 1 }
```

`total_rebuilds` counts tile rebuilds since the cache was built (the initial build is not counted).
`revision` goes up every time an update rebuilds at least one tile: polygon indices change then.

## Obstacles

`nav.tile_cache_add_obstacle` adds a cylinder (`base` is the centre of its bottom disc) or a box
(`min`/`max`, corners in any order). `touched` lists the tiles its footprint overlaps; with the
default `"update": true` they are rebuilt at once and listed in `rebuilt`:

```json
{ "shape": "cylinder", "base": [2, 0, 1], "radius": 0.6, "height": 1.8 }
→ { "id": 0, "touched": [[3, 2]], "rebuilt": [[3, 2]], "pending": [], "revision": 1 }

{ "shape": "box", "min": [-0.3, -1, -5], "max": [0.3, 2, 5] }
→ { "id": 1, "touched": [[2, 0], [2, 1], [2, 2], [2, 3], [2, 4]], "rebuilt": [...], ... }
```

With `"update": false` the tiles are only queued (`pending`); the next update (another call with
`update`, a crowd call, or `NavCrowdPlugin`'s fixed step) rebuilds them. An obstacle blocks the
surfaces from slightly below its bottom (the agent's step height) up to its top, so one floating
above the floor blocks nothing.

`nav.tile_cache_remove_obstacle` removes one by id and rebuilds the tiles it touched:

```json
{ "id": 0 }   → { "removed": true, "rebuilt": [[3, 2]], "pending": [], "revision": 2 }
```

Check the effect with `nav.find_path` when the app also publishes the cache's mesh, or from game
code with `NavTileCache::query`.

## Crowd agents

`nav.crowd_add_agent` snaps a start point onto the mesh and adds an agent (the `NavCrowd` resource
is created with default settings on first use). Optional `radius`, `height`, `max_speed` and
`max_acceleration` default to 0.35 m, 1.6 m, 3.5 m/s and 8 m/s². An optional `filter`
(`{ include_flags, exclude_flags, area_costs: [[area, scale]...], exclude_areas: [area...] }`, each optional,
ADR-1934) is the query filter the agent's paths use:

```json
{ "position": [-3, 0, 0], "radius": 0.35 }
→ { "id": 0, "agents": 1, "agent": { "id": 0, "state": "idle", "position": [-3, 0, 0],
    "velocity": [0, 0, 0], "target": null, "corridor_len": 1, "radius": 0.35 } }
```

`nav.crowd_set_target` plans the agent's corridor to a target on the mesh:

```json
{ "id": 0, "target": [3, 0, 0] }   → { "agent": { "id": 0, "state": "walking", ... } }
```

An unreachable target is not an error: the agent's state is `no_path` and it stands still.

## Stepping and determinism

`nav.crowd_step` runs whole fixed steps (default 1, at most 3600 per call). Queued tiles are rebuilt
first, and the crowd replans every corridor when the tile-cache revision changed:

```json
{ "steps": 600 }
→ { "steps": 600, "total_steps": 600, "arrived": 1, "agents": 1, "digest": "5f0c1d2e3a4b5c6d" }
```

`digest` hashes every agent's id, state, position and velocity bits. Two runs with the same mesh,
obstacles, agents, targets and step counts give the same digest: compare digests to check a
replay instead of comparing positions.

`nav.crowd_status` lists every agent plus counters (`steps`, `replans`, `path_failures`,
`velocity_samples`, `shortcuts`), `step_seconds` and the digest. Agent states are `idle` (no
target), `walking`, `arrived` (horizontal distance at most `max(radius / 4, 0.01)` metres from the
goal; the agent stays arrived until that distance exceeds one radius; height does not count),
`no_path`, `off_mesh_link` and
`dormant` (on a tile that left the world; see [area costs and crowd lanes](navigation-lanes.md)),
`awaiting_tile` (at the loaded border of a `streamed: true` target, waiting for its tile; same page, Streaming),
`link_queued` and `link_timeout` (waiting for, or gave up on, a reserved link; see
[navigation authoring](navigation-authoring.md)), and `stuck` (see Stuck agents below).

## Stuck agents

An agent pinned in a corner, wedged head-on in a narrow corridor or boxed in by others walks on
the spot for ever unless something notices. `nav.crowd_set_stuck` switches stuck detection on for
the whole crowd (off by default, so a crowd without it steps exactly as before; NM-2, ADR-8380):

```json
{ "window_steps": 60, "min_progress_m": 0.5 }
→ { "window_steps": 60, "min_progress_m": 0.5, "enabled": true }
```

A walking agent that has not come `min_progress_m` metres closer to its next corner for
`window_steps` fixed steps climbs one rung of a recovery ladder per such window:

1. `repath`: it plans a new corridor from where it stands.
2. `nudge`: it moves up to one radius toward the center of the polygon it stands on (it stays on
   that convex polygon) and plans again.
3. `reported`: it gives up as state `stuck`. It stands, keeps its target, and walks again on the
   next `nav.crowd_set_target` or replan (a mesh change). Its owner decides what to do.

Coming `min_progress_m` closer to the corner, or reaching it (within one radius plus
`min_progress_m`), counts as recovered and starts the ladder over. A new corner from the
ladder's own re-plan or nudge only restarts the distance measurement, so it never reads as a
recovery. `window_steps: 0` turns
detection off. `nav.crowd_step` lists each rung taken as `stuck_events`
(`{ agent, action, position, goal }`, `action` one of `repath`, `nudge`, `reported`) and both
`nav.crowd_step` and `nav.crowd_status` carry `stuck`: `{ window_steps, min_progress_m,
stuck_now, detections, repaths, nudges, reports, recoveries }`. Game code reads the same rungs
from `NavCrowd::stuck_events` or, with `NavCrowdPlugin`, as `NavStuckEvent` messages after
`NavCrowdStep`. `NavAgent` steering has the same ladder behind a `NavAgentStuckConfig` resource
(`NavAgentState::stuck`, `NavAgentStuck` messages).

## Errors

Errors carry `data.reason`:

| `data.reason` | Code | Meaning |
|---|---|---|
| `nav_no_tile_cache` | F0003 | The app has no `NavTileCache` resource (tile-cache methods). |
| `nav_no_mesh` | F0003 | Neither a `NavTileCache` nor a `NavMesh` for the crowd to walk on. |
| `nav_no_crowd` | F0003 | No `NavCrowd` yet (`nav.crowd_status`, `nav.crowd_set_target`, `nav.crowd_step`); add an agent first. |
| `nav_invalid_obstacle` | F0002 | A non-finite number, a zero or negative cylinder radius or height, a box with no width or depth, or an `oriented_box` half extent that is zero or negative. |
| `nav_unknown_obstacle` | F0003 | No obstacle has that id. |
| `nav_too_many_obstacles` | F0002 | The cache already holds 4096 obstacles. |
| `nav_invalid_agent_params` | F0002 | A non-finite agent parameter, a zero or negative radius or height, or a negative speed or acceleration. |
| `nav_invalid_input` | F0002 | Not returned for a protocol `position` or `target`. A value that is not three finite f32 numbers is F0002 with `data.code` and `data.hint` and no `data.reason`. Game code that calls `NavCrowd` directly still gets this reason for a non-finite position, target or elapsed time. |
| `nav_off_mesh` | F0002 | The position or target is outside the crowd's snap box (half-extents `[2, 2, 2]` by default), the same box `nav.find_nearest` uses. A point can be more than 2 m away in a straight line and still sit inside that box. |
| `nav_unknown_agent` | F0003 | No crowd agent has that id. |
| `nav_too_many_agents` | F0002 | The crowd is full (1024 agents by default). |
| `nav_invalid_crowd_config` | F0002 | A crowd inserted by game code has an invalid config (never from the default crowd), or `nav.crowd_set_stuck` got a `min_progress_m` that is not finite and positive. |
| `nav_invalid_tile_size`, `nav_no_geometry`, `nav_no_walkable_surface`, `nav_invalid_build_input` | F0002 | `NavTileCache::build` errors, reported by game code (the protocol never builds a cache). |

A bad parameter shape (a missing `position`, `steps` outside 1..=3600, a `shape` other than
`cylinder`, `box` or `oriented_box`) is the usual F0002 without a `reason`. So is a `position`
or `target` that is not three finite f32 numbers. `oriented_box` takes `center`, `half_extents`
and an optional `yaw` (default 0).
