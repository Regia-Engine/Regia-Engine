# Navigation: navmesh queries and dynamic obstacles

Inspect the navmesh and query paths without driving gameplay. `agent.hello` does not list a
navigation capability — probe with `nav.get_mesh`.

`nav.get_mesh`, `nav.add_obstacle` and `nav.remove_obstacle` need a `regia_nav::NavMesh` resource
(built with `NavMeshBuilder`). Without one they return `NOT_FOUND` (`F0003`) whose hint names
`NavMeshBuilder` and does not name `NavPlugin`.

`nav.find_path` and `nav.find_nearest` use that `NavMesh` when one is present. When the app has no
`NavMesh` and does have a `NavTileCache`, those two queries use the cache. With neither resource
they return the same `NOT_FOUND`.

Tile-cache methods (`nav.tile_cache_*`) return `data.reason` `nav_no_tile_cache` when there is no
cache. Crowd methods return `nav_no_mesh` or `nav_no_crowd` (see [navigation crowds](navigation-crowds.md)).
Link methods prefer the cache when both resources exist (see [navigation links](navigation-links.md)).

## Summary

`nav.get_mesh` reports the shape of the base mesh (obstacles are not reflected; every polygon has
four vertices, so `vertices` is `4 * polys`):

```json
{}
→ { "polys": 126, "vertices": 504, "regions": 1, "layers": 1, "off_mesh_links": 0,
    "bounds": { "min": [-13, -1, -13], "max": [13, 16, 13] },
    "agent": { "radius": 0.35, "height": 1.6, "max_climb": 0.5, "max_slope": 0.785 },
    "cell_size": 0.25 }
```

## Stacked surfaces

A mesh built with `NavMeshBuilder::build_layered` (more than one layer) keeps the walkable surfaces under
the top one too (ADR-8076): a tunnel floor under the ground, the ground under a bridge and each floor of
a building get polygons. `NavMeshBuilder::build` keeps the top surface only. `layers` in `nav.get_mesh` is
how many walkable layers the mesh was built from: `1` for a top-surface mesh, more where surfaces stack.
A surface needs the agent's `height` of headroom to count, so the floor under a low table stays
unwalkable, and a vertical wall through that headroom closes it, so the inside of a closed box does not
join the floor around it. Where an agent can step between a lower surface and one beside
it (the ground under a bridge and the ground next to it, a tunnel floor and its ramp), the polygons are
linked, and `nav.find_path` routes through.

On a stacked mesh `regions` counts groups of linked polygons. `nav.find_nearest` keeps the polygon
nearest in XZ, and among equally near polygons the one whose height is nearest the query point, so give
a point at the level you mean (a point in a tunnel finds the tunnel floor). A `NavTileCache` mesh has
one layer for now.

To check a level built with layers: call `nav.get_mesh` and expect `layers` above 1 when the level has
tunnels or overpasses, then `nav.find_nearest` with a point on the lower floor and a small `extents` y (say 0.5)
and expect a polygon at that floor's height.

## Layers and off-mesh links

Each walkable layer of a stacked floor is its own set of regions, contours and polygons. Bake with
`NavMeshBuilder::build_with_nav_level_cap(triangles, areas, volumes, max_nav_levels)`. A column with
more walkable levels than the setting fails the build (`invalid navmesh build input`, reason
`max_nav_levels exceeded: a column has more walkable levels than the setting`). `build_layered` still
drops surfaces past its count. Five storeys need the setting at 5 or above.

`NavAgentParams::pilot()` is the on-foot agent: radius 0.35 m, height 1.8 m, step 0.55 m.
`NavAgentParams::hover(radius)` is a hull: height is the radius plus 2 m, step 4 m. A hover bake
erodes one extra metre on each side, so a passage narrower than `2 * radius + 2` m is not on that
hull's mesh. `NavAgentParams::for_radius(radius)` keeps the pilot height and step at 1.5 m or below,
and uses the hover height with the pilot step above that.

`NavAreaCosts::passages()` sets area 60 (tunnel) to 0.75 and area 61 (flank) to 0.20. Assign it with
`NavMesh::set_area_costs`. A path of the same length prefers the tunnel. Both costs are below 1, so
the straight-line heuristic stays admissible.

Ladders and deck stairs are climb links. `pilot_layer_links` turns a `NavLayerLink` list into
bidirectional climb links for a pilot and returns nothing for any other agent, so a hull mesh does
not contain them. Each link's flags are `NavLinkFlags::PILOT` (bit 8) only, the snap radius is 0.5 m,
and the `user_id` is `0x4E414C00` plus the list index for a ladder or `0x4E415300` plus the index for
deck stairs. `NavQueryFilter::pilot()` allows the climb. `NavQueryFilter::hull()` does not. The
default filter allows every flag, so a climb that is on the mesh is usable until the filter says
otherwise. `NavQuery::locate_body` retries `find_nearest` at the body point, then 0.9 m below it,
then 1.8 m below it.

No new agent method. `nav.get_mesh` already reports `layers`. Hosts place ladders and stairs with
`nav.set_links` (kind `climb`, flag bit 8). `nav.links` and `nav.generate_links` are unchanged.

## Nearest point

`nav.find_nearest` snaps a point to the nearest walkable polygon:

```json
{ "point": [1.2, 1.0, -3.0], "extents": [2, 2, 2] }
→ { "poly": 42, "point": [1.2, 0.0, -3.0] }
```

`poly`/`point` are `null` when nothing is within `extents` (a half-extent box; default
`[2, 2, 2]`; each component must be in 0..=1000, so a negative component is refused with `F0002`). The vertical gate is strict: the same query with `y = 5.0` is 5 m above a floor at
`y = 0` and returns `{ "poly": null, "point": null }`.

## Paths

`nav.find_path` returns the smoothed waypoints (string-pulling), the polygon sequence and the
length:

```json
{ "from": [-5, 0, -5], "to": [5, 0, 5] }
→ { "path": { "waypoints": [[-5,0,-5], [0,0,0], [5,0,5]], "polys": [10, 12, 14], "length": 14.14 } }
```

`path` is `null` when either endpoint is off the mesh or no route exists (e.g. the corridor is
blocked).

## Dynamic obstacles

`nav.add_obstacle` records an axis-aligned box in the `NavObstacles` resource; `nav.remove_obstacle`
removes it (`{ "removed": false }` means a mesh is present but the id is unknown). Queries carve
the boxes on the fly, so blocked polygons are skipped by `nav.find_path` and `nav.find_nearest`,
but the stored mesh is unchanged: `nav.get_mesh` and the `nav_mesh` gizmo keep showing the base
polygons.

```json
{ "min": [-0.5, -1, -4], "max": [0.5, 2, 4] }   → { "id": 0, "obstacles": 1 }
{ "id": 0 }                                       → { "removed": true }
```

## Raycast

`nav.raycast` walks the mesh surface from `from` toward `to` (XZ; the height of `to` is ignored).
`from` snaps onto the nearest allowed polygon within `extents`; the walk then crosses polygons
through their shared edges and stops at the first edge with no neighbour (a wall) or whose
neighbour is in an area the `filter` leaves out (NM-1, ADR-8380):

```json
{ "from": [0, 0, 0], "to": [20, 0, 0] }
→ { "hit": true, "t": 0.4, "normal": [-1, 0, 0], "point": [8, 0, 0], "start": [0, 0, 0],
    "last_poly": 3, "path_polys": [0, 1, 3] }
```

`t` is the fraction of the segment from `start` (the snapped `from`) to `to` that was walked: the
wall's position when `hit`, else `1`. `normal` is the wall's unit normal in XZ, pointing back into
the walkable side (`[0, 0, 0]` when nothing was hit). `path_polys` lists every polygon crossed,
start polygon first; `last_poly` is the one the walk ended on. Off-mesh links are never taken: use
it for line-of-sight along the floor (can the agent walk straight there?), cover checks and
dash or charge abilities.

## Random points

`nav.random_point` draws seeded points. With no `center` every allowed polygon is picked in
proportion to its XZ area and the point is uniform inside it, so the points are uniform over the
walkable surface. With `center` and `radius` the points are uniform over the walkable surface
within `radius` (XZ) of `center`; `reachable_only: true` keeps only polygons connected to the
center's polygon, so islands (a rooftop, a closed room) never come up:

```json
{ "seed": 7, "count": 2, "center": [0, 0, 0], "radius": 5, "reachable_only": true }
→ { "seed": 7, "points": [{ "poly": 12, "point": [1.83, 0, -2.41] }, { "poly": 9, "point": [-3.02, 0, 0.77] }] }
```

The same `seed` (and mesh, filter and parameters) always gives the same points on every platform:
replay a wander target by storing the seed. `count` is 1..=4096 (default 1). `points` is empty
when no allowed polygon touches the circle.

## Partial paths

`nav.find_path` with `"partial": true` still answers when the goal cannot be reached: the path
then runs to the reachable polygon nearest the goal (3D distance to its closest point, ties to
the lower polygon index) and ends at that polygon's point nearest the goal. `status` says which:

```json
{ "from": [-6, 0, 0], "to": [3, 0, 0], "partial": true }
→ { "status": "partial", "path": { "waypoints": [[-6, 0, 0], [0, 0, 0]], "polys": [0, 2], "length": 6 } }
```

`status` is `complete` (a reachable goal: the same path as without `partial`), `partial` (an
unreachable goal, or a goal off the mesh) or `no_start` (`path` is `null`: the start is off the
mesh). Without `partial` the answer is unchanged.

### Errors

The NM-1 queries use these `data.reason` values: `nav_no_mesh` (`F0003`: neither a `NavMesh` nor
a `NavTileCache`), `nav_bad_extents` (`F0002`: a negative `extents` component), `nav_bad_radius`
(`F0002`: a negative radius), `nav_no_start_poly` (`F0003`: `from`, or the `center` with
`reachable_only`, has no allowed polygon within `extents`).

## Working with the results

- Waypoints are world-space `[x, y, z]` on the walkable surface; entity ids from other methods
  (`scene.*`, `physics.*`) are unrelated to polygon indices.
- A wall carved as an obstacle makes `nav.find_path` return `null` across it — a good
  `play.expect`-free way to assert "no route exists".
- The stored mesh never changes when obstacles are added; rebuild it (or re-insert a new `NavMesh`) to reflect
  changed geometry.
- The builder rasterises level geometry conservatively (`cell_size`, default 0.25 m): a cell
  takes every triangle that overlaps its square, so a wall top or a plank thinner than one cell
  is in the mesh wherever it sits on the grid (since nav cell v3, ADR-8899; before it, such
  geometry could fall between cell centres, ADR-7558). A cell a triangle only partly covers
  counts as a boundary for an agent with a radius, so floor edges do not grow; a point-sized
  agent (radius 0) may stand on it. A purely vertical wall
  with no top face still records nothing in the top-surface build: give thin blockers a top, or
  carve them as obstacles.

<!-- depth-2:method-reference -->

## Call reference

Parameters, results and error codes for every method on this page. Types and defaults come from the schema and handlers; limits include the contract. Error codes are in `data.code`; names explicitly labeled `data.reason` are additional handler reasons. Every method can also return `F0001`, `F0002`, `F0004`, `F0007` and `F0012`. The server can return `F0005`, `F0006`, `request_too_large`, or `F0008` when the method is not registered; see [protocol](protocol.md).

Unless a lower or explicit method limit is listed, the dispatcher accepts at most 65,536 items in a top-level array parameter and 1,048,576 UTF-8 bytes in a string parameter. A limit violation returns `F0002`.

The request in each fence is the call that was sent with `regia agent call`.
The line under it is the host's answer.
A second fence tagged `json,ignore` is an error answer. Large fields are elided with `...`. A fence tagged `json,ignore` is a real call on a host or plugin method that the runtime schema does not list.


### `nav.add_obstacle`

Carves an axis-aligned box obstacle out of the navmesh; returns its id.

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `min` | array | yes | Box minimum corner [x, y, z]. |
| `max` | array | yes | Box maximum corner [x, y, z]. |

Result: `{ id, obstacles }`.

Errors besides the universal codes: `F0003`.

```json
nav.add_obstacle {"min": [-0.5, -1, -4], "max": [0.5, 2, 4]}
```

```json,ignore
→ { "message": "no NavMesh in this app", "code": -32001, "data": { "code": "F0003", "hint": "build a regia_nav::NavMesh (NavMeshBuilder) and insert it as a resource" } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:nav.add_obstacle -->

### `nav.find_nearest`

The polygon and closest walkable point on the navmesh to a query point.

*Read, does not mutate.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `point` | array | yes | Query point [x, y, z] (world space). |
| `extents` | array | no | Half-extents [x, y, z] of the search box (default [2, 2, 2]). |

`extents` is from 0 to 1000.

Result: `{ poly, point: [x, y, z] } or { poly: null, point: null }`.

Errors besides the universal codes: `F0003`.

```json
nav.find_nearest {"point": [1.2, 5.0, -3.0], "extents": [2, 2, 2]}
```

```json,ignore
→ { "message": "no NavMesh in this app", "code": -32001, "data": { "code": "F0003", "hint": "build a regia_nav::NavMesh (NavMeshBuilder) and insert it as a resource" } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:nav.find_nearest -->

### `nav.find_path`

A path between two points: smoothed waypoints, the polygon sequence and length.

*Read, does not mutate.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `from` | array | yes | Start point [x, y, z] (world space). |
| `to` | array | yes | End point [x, y, z] (world space). |
| `extents` | array | no | Half-extents [x, y, z] of the point-location box (default [2, 2, 2]). |
| `filter` | object | no | { include_flags, exclude_flags, area_costs: [[area, scale]...], exclude_areas: [area...] }, each optional (ADR-1934). |
| `partial` | boolean | no | true: an unreachable goal gives the path to the reachable point nearest it, with a status (complete, partial, no_start) (default false). |

Result: `{ path: { waypoints: [[x,y,z]...], polys: [...], length } | null }`; with `partial`:
`{ status, path }` (see [Partial paths](#partial-paths)).

Errors besides the universal codes: `F0003`. Filter refusals use `F0002` with `data.reason` `nav_invalid_area` or `nav_invalid_area_cost`. Omitted filter uses include_flags 65535, exclude_flags 0, no excluded areas and area scales 1; area ids are 0..=63 and area scales must be finite and positive.

```json
nav.find_path {"from": [-5, 0, -5], "to": [5, 0, 5]}
```

```json,ignore
→ { "message": "no NavMesh in this app", "code": -32001, "data": { "code": "F0003", "hint": "build a regia_nav::NavMesh (NavMeshBuilder) and insert it as a resource" } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:nav.find_path -->

### `nav.get_mesh`

A summary of the navmesh: polygon/vertex/region/layer counts, bounds and agent parameters.

*Read, does not mutate, heavy.*

No parameters.

Result: `{ polys, vertices, regions, layers, off_mesh_links, bounds, agent, cell_size }` (`layers`: see
[Stacked surfaces](#stacked-surfaces)).

Errors besides the universal codes: `F0003`.

```json
nav.get_mesh {}
```

```json,ignore
→ { "message": "no NavMesh in this app", "code": -32001, "data": { "code": "F0003", "hint": "build a regia_nav::NavMesh (NavMeshBuilder) and insert it as a resource" } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:nav.get_mesh -->

### `nav.random_point`

Seeded random points on the navmesh: area-weighted over the allowed polygons, or uniform within
radius of center (reachable_only skips islands). The same seed gives the same points.

*Read, does not mutate.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `seed` | integer | no | Random seed (default 0). |
| `count` | integer | no | How many points, 1..=4096 (default 1). |
| `center` | array | no | Circle center [x, y, z]; give radius too. |
| `radius` | number | no | Circle radius in metres (>= 0, else nav_bad_radius); give center too. |
| `reachable_only` | boolean | no | Only polygons connected to the center's (default false). |
| `extents` | array | no | Half-extents [x, y, z] for snapping the center (default [2, 2, 2]). |
| `filter` | object | no | Query filter as for nav.find_path. |

`count` is from 1 to 4096.

Result: `{ seed, points: [{ poly, point: [x, y, z] }...] }`.

Errors besides the universal codes: `F0003` (`data.reason` `nav_no_mesh`, `nav_no_start_poly`);
`F0002` with `data.reason` `nav_bad_extents` or `nav_bad_radius`.

```json
nav.random_point {"seed": 7, "count": 16, "center": [0, 0, 0], "radius": 5, "reachable_only": true}
```

<!-- capture:nav.random_point -->

### `nav.raycast`

Walks the navmesh from `from` toward `to` (XZ) through shared polygon edges and reports the first
wall or left-out area.

*Read, does not mutate.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `from` | array | yes | Start point [x, y, z]; snapped onto the mesh within extents. |
| `to` | array | yes | End point [x, y, z] (its height is ignored). |
| `extents` | array | no | Half-extents [x, y, z] for snapping the start (default [2, 2, 2]). |
| `filter` | object | no | Query filter as for nav.find_path; a left-out area stops the ray. |

Result: `{ hit, t, normal: [x, y, z], point: [x, y, z], start: [x, y, z], last_poly, path_polys: [...] }`.

Errors besides the universal codes: `F0003` (`data.reason` `nav_no_mesh`, `nav_no_start_poly`);
`F0002` with `data.reason` `nav_bad_extents`.

```json
nav.raycast {"from": [-5, 0, 0], "to": [5, 0, 0], "extents": [2, 2, 2]}
```

<!-- capture:nav.raycast -->

### `nav.remove_obstacle`

Removes a previously added obstacle by id, reopening the polygons it covered.

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `id` | number | yes | The obstacle id from nav.add_obstacle. |

Result: `{ removed }`.

Errors besides the universal codes: `F0003`.

```json
nav.remove_obstacle {"id": 0}
```

```json,ignore
→ { "message": "no NavMesh in this app", "code": -32001, "data": { "code": "F0003", "hint": "build a regia_nav::NavMesh (NavMeshBuilder) and insert it as a resource" } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:nav.remove_obstacle -->

## Workflow: path around a temporary obstacle

1. `nav.get_mesh`. `F0003` means this world has no nav mesh yet.
2. `nav.find_nearest` for the start and the goal. A point far from the mesh comes back with no polygon.
3. `nav.find_path` between those points and keep the corridor.
4. `nav.add_obstacle` on the box that should block the corridor. The id comes back in the answer.
5. `nav.find_path` again and compare the corridor.
6. `nav.remove_obstacle` with that id when the blocker is gone.

<!-- /depth-2:method-reference -->

## Deterministic layered plans

`nav.plan` plans on the deterministic layered bake of the app's `NavMesh` (`regia_nav::det::layers`,
ADR-8486). The bake is integer millimeters with a layer index per polygon (0 is the top surface of
its column), so the same request gives the same bits on every machine. Build the mesh with
`NavMeshBuilder::build_layered` to get more than one layer. It needs a `NavMesh` resource; without
one it returns `NOT_FOUND` (`F0003`).

```json
{ "from": [-3, 0, -3], "to": [3, 15, 3], "tunnel_pct": 75 }
→ { "plan": { "waypoints": [[-3, 0, -3], [0, 0, 0], [0, 15, 0], [3, 15, 3]],
              "waypoint_layers": [1, 1, 0, 0], "layer": 0, "polys": [0, 1],
              "cost_mm": 21485, "length": 23.48, "expansions": 2 },
    "bake_hash": "4f0c2b8e7d1a9c35", "bits_version": 1, "layers": 2 }
```

(The numbers above are an example; the reply carries the mesh's own.)

| Field | Meaning |
|---|---|
| `tunnel_pct`, `flank_pct` | What a meter in a tunnel (area 60) or flank (area 61) polygon costs, in percent of an open meter, 10 to 100. Defaults 75 and 20. |
| `passing_gap` | Meters a passage lane leaves free for a body coming the other way. Default: one body diameter of the mesh's agent; 0 turns lanes off. Inside a passage, a plan follows the right-hand wall (it moves onto it at most one gap from the shortest path and drifts outward slowly, so it does not turn into a side opening), so two bodies that meet head-on take opposite walls. |
| `layer` | The goal waypoint's layer; `waypoint_layers` has one entry per waypoint. |
| `bake_hash` | FNV-1a 64 of the bake's bytes, hex. Two peers with the same mesh report the same value. |

When there is no route the reply is `{ "plan": null, "reason": <code> }` beside `bake_hash`,
`bits_version` and `layers`:

| `reason` | When |
|---|---|
| `plan_start_off_mesh` | `from` is more than 3 m from every polygon. |
| `plan_goal_off_mesh` | `to` is more than 3 m from every polygon. |
| `plan_start_closed`, `plan_goal_closed` | The endpoint's polygon is closed by a blocker (a host's `LayerState`; `nav.plan` itself plans on an open state). |
| `plan_unreachable` | No open route joins them. |
| `plan_budget` | The search expanded more polygons than its budget. |

A lockstep host keeps a `LayerState` beside the bake. With the `stage_b` feature,
`regia_nav::layered::geom::cook_layered` bakes a world-geometry map (`regia_world_geom::WorldGeom`)
and `sync_layers` feeds each generation's closed sections (blockers) and ground-edit rectangles into
it, in that order. A dirty tile stays walkable until the host re-bakes it; a plan that crosses one
reports `crosses_dirty`.
