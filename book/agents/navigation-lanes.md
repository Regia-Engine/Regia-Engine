# Navigation: area costs and crowd lanes

Two ways to make a world feel inhabited without writing pathfinding: areas tell paths what the ground
is like, and lanes move big ambient crowds along set routes. Design: ADR-1934.

## Areas

Every navmesh polygon has an area id from 0 to 63, and 0 is plain ground. An area has a cost per metre,
and a path is the route with the lowest total. A cost above 1 makes ground less attractive; it does not
slow anyone down.

`nav.add_area_volume` paints an area over a shape on the `regia_nav::NavTileCache`, and polygons split
along the edge. The shapes are those of `nav.tile_cache_add_obstacle`; where volumes overlap the later one
wins, and the agent radius does not inflate them. `nav.set_area_cost` sets the cost for everyone, on the
tile cache or the `NavMesh`, and crowd agents replan:

```json
{ "shape": "box", "min": [-6, -1, -8], "max": [6, 1, 8], "area": 3 }
→ { "id": 0, "rebuilt": [[1, 1]], "pending": [], "revision": 4 }

{ "area": 3, "cost": 8.0 }
→ { "costs": [{ "area": 3, "cost": 8.0 }] }
```

`nav.area_status` shows the costs that differ from 1, polygons per area and the volumes.
`nav.remove_area_volume` takes one away by `id`. In a game, `regia_nav::NavMeshBuilder::build_with_areas`
paints at build time, `NavTileCache::add_area_volume` at runtime, and a `regia_framework::nav_world::NavAreaZone`
component keeps a box around an entity painted (with `NavWorldPlugin`; a zone that moves paints last).

### The level's area table (nav bake settings)

A level carries its ground costs as the `regia_framework::nav_world::NavAreaCostTable` resource (stored in the
level's `resources` next to `LevelStreaming`), and its painted ground as `NavAreaZone` components (ADR-2133). The
streaming cook paints the zones into the cooked nav cells, and `NavWorldPlugin` applies the table to the tile cache
(and a `NavMesh`) when the table changes or the cache is replaced. Agents read and author the table:

```json
nav.set_area_table { "areas": [ { "area": 3, "cost": 4.0, "name": "mud" }, { "area": 5, "cost": 0.5, "name": "road" } ] }
→ { "areas": [ { "area": 3, "cost": 4.0, "name": "mud" }, { "area": 5, "cost": 0.5, "name": "road" } ],
    "costs": [ { "area": 3, "cost": 4.0 }, { "area": 5, "cost": 0.5 } ] }

nav.area_table {}
→ { "areas": [ { "area": 3, "cost": 4.0, "name": "mud" }, { "area": 5, "cost": 0.5, "name": "road" } ] }
```

`nav.set_area_table` replaces the table (the editor saves it with the level), applies it to the live mesh at once and
makes crowd agents replan. Areas not listed cost 1, and a later row for the same area wins. At most 256 rows; an
invalid row (`nav_invalid_area`, `nav_invalid_area_cost`) changes nothing. `nav.area_table` answers `areas: null`
when the level has no table. A cost set later with `nav.set_area_cost` holds until the table changes again.

A cell's volume needs to hold both its center and its surface height, so a volume over a bridge does not
paint the road under it. A cooked nav cell keeps the areas painted when it was cooked; a live volume paints
over them when the cell loads. An off-mesh link has an `area` too: the searched cost is the link's
3D length times `cost_scale`, times that area's mesh cost, times the query filter's scale for the area.

### Filters

`nav.find_path`, `nav.stream_find_path`, `nav.crowd_add_agent` and `nav.crowd_set_filter` take a `filter`:

```json
{ "from": [-12, 0, 0], "to": [12, 0, 0],
  "filter": { "area_costs": [[3, 0.125]], "exclude_areas": [5], "exclude_flags": 4 } }
```

| Field | Meaning |
|---|---|
| `area_costs` | `[area, scale]` pairs. A scale multiplies the mesh's cost for this query or agent only |
| `exclude_areas` | Areas left out: no path crosses them, and a path's end snaps to the nearest allowed polygon within the extents |
| `include_flags`, `exclude_flags` | Off-mesh link flags, as on the [links](navigation-links.md) page |

To a crowd agent a left-out area is a wall: avoidance sees its edge and nothing can push the agent in. A
visibility shortcut never crosses ground dearer than the corridor it replaces. A path inside one polygon
takes no search, so a cost acts only between polygons: paint the ground you want to matter.

## Crowd lanes

A lane is a one-way polyline with a width, a speed limit and filter bits, plus the lanes it leads to. A lane
agent is a lane, a distance along it, a sideways offset and a speed, so stepping one costs no pathfinding and
no navmesh query. Use lanes for pedestrians and traffic in the distance, and `regia_nav::NavCrowd` for the
agents the player deals with.

`nav.lanes_set` takes `lanes` or the text of a `.navlanes.ron` file (`docs/formats/navlanes-ron.md`). It
replaces the graph and drops the agents of the old one:

```json
{ "lanes": [
    { "points": [[0, 0, 0], [10, 0, 0]], "next": [1, 2], "user_id": 4 },
    { "points": [[10, 0, 0], [15, 0, 0]], "speed": 0.5 },
    { "points": [[10, 0, 0], [15, 0, 5]] } ] }
→ { "lanes": 3, "total_length": 22.07, "agents_dropped": 0 }
```

With `join_distance`, a lane also leads to every other lane starting that close to its end. Check the graph with
`nav.lanes_status`, `nav.lane_route` (the quickest route, by length over speed limit, with waypoints),
`nav.lane_nearest` and `nav.lane_check` (lane points with no navmesh under them). `nav.lane_set_enabled` closes
every lane with a `user_id`: nothing plans onto it, a wanderer turns away, and an agent routed over it stops until it
reopens. An agent on a closed lane walks off its end, and can still be given a destination.

### Agents

`nav.lane_add_agent` adds one (`lane`, `distance`, `offset`, `seed`, `radius`, `speed`, `side_speed`, `flags`,
`prefer_flags`) and makes the crowd on first use. `nav.lane_step` runs whole fixed steps:

```json
{ "steps": 600 }
→ { "steps": 600, "total_steps": 600, "agents": 1, "dormant": 0,
    "transitions": [{ "agent": 0, "from": 0, "to": 2 }], "transitions_total": 1,
    "digest": "60b1ef0e2a2b4c1d" }
```

The same inputs give the same `digest`. `transitions` lists at most 256; `transitions_total` counts them all.
`nav.lane_agents` pages through the agents (`offset`, `limit` up to 4096), and `nav.lane_remove_agent` removes one.

An agent walks at the lower of its speed and the lane's limit. It keeps its gap to the agent ahead in its band, and
sidesteps to whichever side clears it when the lane is wide enough; on a narrow lane a queue forms. The agent ahead can
be on the next lane, so a junction backs up on its own lane.

A wanderer takes the next lane by a hash of its `seed` and the lane, and stops at a dead end. With
`nav.lane_set_destination` it follows a route and stops at the end of the last lane; `lane: null` sets it wandering again.

| `state` | Meaning |
|---|---|
| `walking` | On its lane |
| `stopped` | At the end of its route, at a dead end, or its next lane closed |
| `dormant` | On a tile that left the world |

### Preference and opposing flow

`.navlanes.ron` v2 (ADR-2133) adds two optional lane fields, also taken by `nav.lanes_set`:

| Field | Meaning |
|---|---|
| `weight` | Preference, default 1. A wanderer at a junction picks among the open lanes in proportion to their weights; a route's cost through the lane is its time divided by the weight |
| `opposite` | The lane running the other way over the same strip. Naming one side pairs both. Walkers of the two lanes see each other as oncoming and keep right to pass |

A walker's `prefer_flags` (default 0) makes it take a lane sharing those bits whenever one is open at a junction. A
two-way promenade over one 3 m strip:

```json
nav.lanes_set { "lanes": [
    { "points": [[-10, 0, 0], [10, 0, 0]], "width": 3.0, "opposite": 1 },
    { "points": [[10, 0, 0], [-10, 0, 0]], "width": 3.0 } ] }
→ { "lanes": 2, "total_length": 40.0, "agents_dropped": 0 }

nav.lane_add_agent { "lane": 0 }
nav.lane_add_agent { "lane": 1 }
nav.lane_step { "steps": 300 }
nav.lanes_status {}
→ { "format_version": 2, "lanes": [ { "lane": 0, "opposite": 1, "weight": 1.0, ... },
                                    { "lane": 1, "opposite": 0, "weight": 1.0, ... } ],
    "crowd": { "oncoming_yields": 61, ... } }
```

The two walkers pass `band` apart (two radii and the crowd's `min_gap`), each on its own right. A strip too narrow for
that lets them pass closer: lanes never deadlock. A pair must run the other way along the same strip (each end within
half the wider width of the other lane's start, lengths within 10 %), and a lane naming itself, a lane outside the
list or a lane paired with two others is `nav_invalid_lane`. With every weight 1, no preference and no pairing, a crowd
steps exactly as with v1 data.

In a game, add `regia_nav::NavLanePlugin` with the `NavLaneGraph` and `NavLaneCrowd` resources, and read
positions with `NavLaneCrowd::position` after `NavLaneStep`. A `NavCrowd` agent can walk a lane route too:
`NavLaneGraph::route_points` gives its waypoints.

## Streaming

Crowd and lane agents standing on a tile that is not loaded go `dormant`: they keep their target, and nothing moves
or pushes them until the tile is back in the mesh with its edits replayed. A crowd agent then replans. `nav.crowd_step`,
`nav.lane_step` and both plugins do this on their own; a host stepping by hand calls `sync_streaming` before each
step. `nav.crowd_status` reports `dormant`. An agent on a loaded tile whose goal left is not dormant: it reports
`no_path` and goes on when the goal returns. Lane agents reach an unloaded tile and stop there, and those behind them
queue. A rebuild queued on a loaded tile puts nobody to sleep.

A crowd agent can also be sent toward ground that is not loaded: `nav.crowd_set_target` with `streamed: true`
(`NavCrowd::set_target_streamed`) plans over the loaded tiles. When the target is out of reach, the agent walks to the
loaded border closest to it and waits there in state `awaiting_tile`, with `awaiting_tile: [x, z]` naming the tile:

```json
nav.crowd_set_target { "id": 0, "target": [12, 0, 0], "streamed": true }
→ { "agent": { "id": 0, "state": "walking", "streamed": true, "awaiting_tile": [2, 1], ... },
    "outcome": "nav_tile_unloaded" }

nav.crowd_step { "steps": 900 }
nav.crowd_status {}
→ { "agents": [ { "id": 0, "state": "awaiting_tile", ... } ], "stats": { "tile_waits": 1, ... }, ... }
```

It replans whenever the mesh changes: when the tile it waits for is ready it goes on, and when a tile under its route
leaves it re-paths around it, or to the new border when there is no way round. `outcome` is `complete`,
`nav_tile_unloaded` or `no_path` (`null` while the agent is on a link or dormant: it plans later). A plain
`nav.crowd_set_target` or a clear ends the streamed mode. `streamed: true` needs a `NavTileCache`
(`nav_no_tile_cache`); a cache built whole plans exactly as a plain target.

## Errors

| `data.reason` | Meaning |
|---|---|
| `nav_invalid_area` | An area id above 63 |
| `nav_invalid_area_cost` | A cost that is not finite and positive |
| `nav_unknown_area_volume` | No volume has this id |
| `nav_too_many_area_volumes` | The cache holds 4096 volumes |
| `nav_no_lane_graph` | No `NavLaneGraph`: send one with `nav.lanes_set` |
| `nav_no_lane_crowd` | No `NavLaneCrowd`: `nav.lane_add_agent` makes one |
| `nav_invalid_lane` | A lane is invalid, or more than 65536 of them |
| `nav_lane_parse` | The text is not valid RON for the format |
| `nav_lane_version` | The file's `format_version` is newer than this build reads |
| `nav_unknown_lane` | No lane has this index |
| `nav_unknown_lane_agent` | No agent has this id |
| `nav_invalid_lane_agent` | Bad agent values, or the lane's flags refuse it |
| `nav_invalid_lane_config` | A crowd setting is invalid |
| `nav_too_many_lane_agents` | The crowd is full |
| `nav_no_lane_route` | `nav.lane_set_destination` found no route (`nav.lane_route` answers `route: null`) |

The tile cache and crowd reasons are on the [crowds](navigation-crowds.md) page.

Lanes generated from road splines (`nav.lanes_from_splines`), area ids painted per triangle at cook time
(`nav.cook_triangle_areas`) and `.navlanes.ron` files loaded at run time are on
[link queues, road lanes, triangle areas and link files](navigation-authoring.md) (ADR-2420).

<!-- depth-2:method-reference -->

## Call reference

Parameters, results and error codes for every method on this page. Types and defaults come from the schema and handlers; limits include the contract. Error codes are in `data.code`; names explicitly labeled `data.reason` are additional handler reasons. Every method can also return `F0001`, `F0002`, `F0004`, `F0007` and `F0012`. The server can return `F0005`, `F0006`, `request_too_large`, or `F0008` when the method is not registered; see [protocol](protocol.md).

Unless a lower or explicit method limit is listed, the dispatcher accepts at most 65,536 items in a top-level array parameter and 1,048,576 UTF-8 bytes in a string parameter. A limit violation returns `F0002`.

The request in each fence is the call that was sent with `regia agent call`.
The line under it is the host's answer.
A second fence tagged `json,ignore` is an error answer. Large fields are elided with `...`. A fence tagged `json,ignore` is a real call on a host or plugin method that the runtime schema does not list.


### `nav.add_area_volume`

Paints an area over a shape on the NavTileCache and rebuilds the tiles under it.

*Play, mutating, heavy.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `shape` | string | yes | cylinder, box or oriented_box, as for nav.tile_cache_add_obstacle. |
| `area` | integer | yes | The area id, 0 to 63. |
| `min` | array | no | Box: one corner. |
| `max` | array | no | Box: the opposite corner. |
| `base` | array | no | Cylinder: the center of its bottom. |
| `radius` | number | no | Cylinder: its radius. |
| `height` | number | no | Cylinder: its height. |
| `center` | array | no | Oriented box: its center. |
| `half_extents` | array | no | Oriented box: half its size. |
| `yaw` | number | no | Oriented box: its turn about +Y in radians; default 0. |
| `update` | boolean | no | Rebuild the tiles now (default true). |

`area` is from 0 to 63. Shape-dependent fields are required: box needs min/max; cylinder needs base/radius/height; oriented_box needs center/half_extents. Every vector has three finite f32-range components. Cylinder radius and height and all oriented-box half extents must be positive. Box corners are sorted componentwise, with positive width and depth; its vertical span may be zero.

Result: `{ id, rebuilt, pending, revision }`.

Errors: `F0002`, `F0003`. `data.reason`: `nav_no_tile_cache`, `nav_invalid_area`, `nav_invalid_obstacle`, `nav_too_many_area_volumes`.

```json
nav.add_area_volume {"shape": "box", "min": [-1, -1, -1], "max": [1, 1, 1], "area": 2}
```

```json,ignore
→ {
  "message": "no NavTileCache in this app: build one with regia_nav::NavTileCache::build and insert it as a resource",
  "code": -32001,
  "data": {
    "code": "F0003",
    "hint": "see book/agents/navigation-crowds.md",
    "reason": "nav_no_tile_cache"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:nav.add_area_volume -->

### `nav.area_status`

Polygon areas of the live mesh: the costs set, polygons per area and the tile cache's volumes.

*Read, does not mutate.*

No parameters.

Result: `{ costs: [{ area, cost }...], polys_by_area: [{ area, polys }...], volumes: [{ id, area, shape }...] }`.

Errors: `F0002`, `F0003`. `data.reason`: `nav_no_mesh`.

```json
nav.area_status {}
```

```json,ignore
→ {
  "message": "no NavTileCache or NavMesh in this app to carry off-mesh links",
  "code": -32001,
  "data": {
    "code": "F0003",
    "hint": "see book/agents/navigation-crowds.md",
    "reason": "nav_no_mesh"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:nav.area_status -->

### `nav.area_table`

The level's authored area costs (the NavAreaCostTable level resource, the nav bake settings for ground costs), or null when the level has none.

*Read, does not mutate.*

No parameters.

Result: `{ areas: [{ area, cost, name }...] or null }`.

No error codes beyond the universal set.

```json
nav.area_table {}
→ { "areas": null }
```

<!-- capture:nav.area_table -->

### `nav.crowd_set_filter`

Replaces a crowd agent's filter (link flags, area costs, left-out areas); it replans.

*Play, mutating, heavy.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `id` | integer | yes | The agent. |
| `filter` | object | yes | { include_flags, exclude_flags, area_costs: [[area, scale]...], exclude_areas: [area...] }, each optional. |

Result: `{ agent }`.

Errors: `F0002`, `F0003`. `data.reason`: `nav_no_mesh`, `nav_no_crowd`, `nav_unknown_agent`, `nav_invalid_agent_params`, `nav_invalid_area`, `nav_invalid_area_cost`.

```json
nav.crowd_set_filter {"id": 0, "filter": {"exclude_areas": [2]}}
```

```json,ignore
→ {
  "message": "no NavTileCache or NavMesh in this app for the crowd to walk on",
  "code": -32001,
  "data": {
    "code": "F0003",
    "hint": "see book/agents/navigation-crowds.md",
    "reason": "nav_no_mesh"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:nav.crowd_set_filter -->

### `nav.lane_add_agent`

Adds an agent to a lane, creating the lane crowd on first use.

*Play, mutating, heavy.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `lane` | integer | yes | The lane. |
| `distance` | number | no | Metres along the lane (default 0). |
| `offset` | number | no | Metres right of the center (default 0). |
| `seed` | integer | no | Picks its turns at junctions (default 0). |
| `radius` | number | no | Body radius (default 0.3). |
| `speed` | number | no | Own speed (default 1.4). |
| `side_speed` | number | no | Sideways speed while passing (default 0.6). |
| `flags` | integer | no | Filter bits (default 1). |
| `prefer_flags` | integer | no | Preferred lane bits at junctions (default 0, none). |

Result: `{ id, agent, agents }`.

Errors: `F0002`, `F0003`. `data.reason`: `nav_no_lane_graph`, `nav_unknown_lane`, `nav_invalid_lane_agent`, `nav_too_many_lane_agents`.

```json
nav.lane_add_agent {"lane": 0, "distance": 1}
```

```json,ignore
→ {
  "message": "no NavLaneGraph in this app: send one with nav.lanes_set or insert the resource",
  "code": -32001,
  "data": {
    "code": "F0003",
    "hint": "see book/agents/navigation-crowds.md",
    "reason": "nav_no_lane_graph"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:nav.lane_add_agent -->

### `nav.lane_agents`

One page of lane agents, in id order.

*Read, does not mutate.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `offset` | integer | no | Agents to skip (default 0). |
| `limit` | integer | no | Page size, 1 to 4096 (default 256). |

Result: `{ total, agents: [{ id, state, lane, distance, offset, speed, route, has_destination, position }...] }`.

Errors: `F0002`, `F0003`. `data.reason`: `nav_no_lane_crowd`.

```json
nav.lane_agents {}
```

```json,ignore
→ {
  "message": "no NavLaneCrowd in this app: nav.lane_add_agent makes one, or insert the resource",
  "code": -32001,
  "data": {
    "code": "F0003",
    "hint": "see book/agents/navigation-crowds.md",
    "reason": "nav_no_lane_crowd"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:nav.lane_agents -->

### `nav.lane_check`

Lists the lane points with no navmesh under them.

*Read, does not mutate.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `reach` | number | no | How far a polygon may be, in meters (default 1). |

Result: `{ on_mesh, gaps: [{ lane, point, position }...] }`.

Errors: `F0002`, `F0003`. `data.reason`: `nav_no_lane_graph`, `nav_no_mesh`.

```json
nav.lane_check {}
```

```json,ignore
→ {
  "message": "no NavLaneGraph in this app: send one with nav.lanes_set or insert the resource",
  "code": -32001,
  "data": {
    "code": "F0003",
    "hint": "see book/agents/navigation-crowds.md",
    "reason": "nav_no_lane_graph"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:nav.lane_check -->

### `nav.lane_nearest`

The lane nearest to a point.

*Read, does not mutate.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `point` | array | yes | The point [x, y, z]. |
| `reach` | number | no | Search radius in meters (default 2). |
| `flags` | integer | no | Filter bits (default every lane). |

Result: `{ lane, distance, away } or { lane: null }`.

Errors: `F0002`, `F0003`. `data.reason`: `nav_no_lane_graph`.

```json
nav.lane_nearest {"point": [1, 0, 0]}
```

```json,ignore
→ {
  "message": "no NavLaneGraph in this app: send one with nav.lanes_set or insert the resource",
  "code": -32001,
  "data": {
    "code": "F0003",
    "hint": "see book/agents/navigation-crowds.md",
    "reason": "nav_no_lane_graph"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:nav.lane_nearest -->

### `nav.lane_remove_agent`

Removes a lane agent.

*Play, mutating, heavy.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `id` | integer | yes | The agent. |

Result: `{ agents }`.

Errors: `F0002`, `F0003`. `data.reason`: `nav_no_lane_crowd`, `nav_unknown_lane_agent`.

```json
nav.lane_remove_agent {"id": 0}
```

```json,ignore
→ {
  "message": "no NavLaneCrowd in this app: nav.lane_add_agent makes one, or insert the resource",
  "code": -32001,
  "data": {
    "code": "F0003",
    "hint": "see book/agents/navigation-crowds.md",
    "reason": "nav_no_lane_crowd"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:nav.lane_remove_agent -->

### `nav.lane_route`

The quickest route between two lanes and its waypoints, or null.

*Read, does not mutate.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `from` | integer | yes | The start lane. |
| `to` | integer | yes | The goal lane. |
| `flags` | integer | no | Filter bits (default every lane). |
| `offset` | number | no | Waypoints this far right of the center line (default 0). |

Result: `{ route: { lanes, length, points } or null }`.

Errors: `F0002`, `F0003`. `data.reason`: `nav_no_lane_graph`, `nav_unknown_lane`.

```json
nav.lane_route {"from": 0, "to": 0}
```

```json,ignore
→ {
  "message": "no NavLaneGraph in this app: send one with nav.lanes_set or insert the resource",
  "code": -32001,
  "data": {
    "code": "F0003",
    "hint": "see book/agents/navigation-crowds.md",
    "reason": "nav_no_lane_graph"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:nav.lane_route -->

### `nav.lane_set_destination`

Sends a lane agent to a lane, where it stops at the end, or back to wandering with lane null.

*Play, mutating, heavy.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `id` | integer | yes | The agent. |
| `lane` | integer | yes | The goal lane, or null to wander. |

Result: `{ agent }`.

Errors: `F0002`, `F0003`. `data.reason`: `nav_no_lane_graph`, `nav_no_lane_crowd`, `nav_unknown_lane_agent`, `nav_unknown_lane`, `nav_no_lane_route`.

```json
nav.lane_set_destination {"id": 0, "lane": 0}
```

```json,ignore
→ {
  "message": "no NavLaneGraph in this app: send one with nav.lanes_set or insert the resource",
  "code": -32001,
  "data": {
    "code": "F0003",
    "hint": "see book/agents/navigation-crowds.md",
    "reason": "nav_no_lane_graph"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:nav.lane_set_destination -->

### `nav.lane_set_enabled`

Opens or closes every lane with a user id.

*Play, mutating, heavy.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `user_id` | integer | yes | The game's lane id. |
| `enabled` | boolean | yes | Open or closed. |

Result: `{ changed }`.

Errors: `F0002`, `F0003`. `data.reason`: `nav_no_lane_graph`.

```json
nav.lane_set_enabled {"user_id": 1, "enabled": false}
```

```json,ignore
→ {
  "message": "no NavLaneGraph in this app: send one with nav.lanes_set or insert the resource",
  "code": -32001,
  "data": {
    "code": "F0003",
    "hint": "see book/agents/navigation-crowds.md",
    "reason": "nav_no_lane_graph"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:nav.lane_set_enabled -->

### `nav.lane_step`

Advances the lane crowd by whole fixed steps; follows streaming when a NavTileCache exists.

*Play, mutating, heavy.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `steps` | integer | no | Fixed steps, 1 to 3600 (default 1). |

Result: `{ steps, total_steps, agents, dormant, transitions: [{ agent, from, to }...], transitions_total, digest }`.

Errors: `F0002`, `F0003`. `data.reason`: `nav_no_lane_graph`, `nav_no_lane_crowd`.

```json
nav.lane_step {"steps": 1}
```

```json,ignore
→ {
  "message": "no NavLaneGraph in this app: send one with nav.lanes_set or insert the resource",
  "code": -32001,
  "data": {
    "code": "F0003",
    "hint": "see book/agents/navigation-crowds.md",
    "reason": "nav_no_lane_graph"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:nav.lane_step -->

### `nav.lanes_set`

Replaces the lane graph, from lane objects or a .navlanes.ron text; lane agents are dropped.

*Play, mutating, heavy.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `lanes` | array | no | Lane objects: { points: [[x, y, z]...], width, speed, flags, user_id, next: [lane...], weight (preference, default 1), opposite (the lane running the other way over the same strip) }. |
| `join_distance` | number | no | With lanes: join lane ends this close (default 0, off). |
| `ron` | string | no | A .navlanes.ron text, instead of lanes. |

At most 65,536 lanes, each with 2..=4096 finite points; consecutive points must differ. Nested defaults: `width: 2`, `speed: 1.4`, `flags: 1`, `user_id: 0`, `next: []`, `weight: 1`, `opposite: null`. Width, speed and weight must be finite and positive; `flags` is a u16, `user_id` a u64, and every `next` or `opposite` index must exist (`opposite` cannot name itself). `join_distance` must be finite and nonnegative. Give exactly one of `lanes` or `ron`.

Result: `{ lanes, total_length, agents_dropped }`.

Errors: `F0002`. `data.reason`: `nav_invalid_lane`, `nav_lane_parse`, `nav_lane_version`.

```json
nav.lanes_set {"lanes": [{"points": [[0, 0, 0], [4, 0, 0]]}]}
→ { "agents_dropped": 0, "lanes": 1, "total_length": 4.0 }
```

<!-- capture:nav.lanes_set -->

### `nav.lanes_status`

The lane graph and, when there is one, the lane crowd's counters and digest.

*Read, does not mutate.*

No parameters.

Result: `{ format_version, total_length, lanes: [{ lane, user_id, length, width, speed, flags, next, enabled, weight, opposite }...], crowd: { agents, dormant, steps, transitions, slowed, oncoming_yields, stopped, dormant_entries, step_seconds, digest } or null }`.

Errors: `F0002`, `F0003`. `data.reason`: `nav_no_lane_graph`.

```json
nav.lanes_status {}
→ {
  "crowd": null,
  "format_version": 2,
  "lanes": [
    {
      "enabled": true,
      "flags": 1,
      "lane": 0,
      "length": 4.0,
      "next": [],
      "opposite": null,
      "speed": 1.399999976158142,
      "user_id": 0,
      "weight": 1.0,
      "width": 2.0
    }
  ],
  "total_length": 4.0
}
```

This answer follows `nav.lanes_set` in the same session, so the graph is not empty. `crowd` stays null until an agent is added.

<!-- capture:nav.lanes_status -->

### `nav.remove_area_volume`

Removes an area volume and rebuilds the tiles it painted.

*Play, mutating, heavy.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `id` | integer | yes | The volume id. |
| `update` | boolean | no | Rebuild the tiles now (default true). |

Result: `{ area, rebuilt, pending, revision }`.

Errors: `F0002`, `F0003`. `data.reason`: `nav_no_tile_cache`, `nav_unknown_area_volume`.

```json
nav.remove_area_volume {"id": 0}
```

```json,ignore
→ {
  "message": "no NavTileCache in this app: build one with regia_nav::NavTileCache::build and insert it as a resource",
  "code": -32001,
  "data": {
    "code": "F0003",
    "hint": "see book/agents/navigation-crowds.md",
    "reason": "nav_no_tile_cache"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:nav.remove_area_volume -->

### `nav.set_area_cost`

Sets what one area costs per meter on the live mesh, for every query; crowd agents replan.

*Play, mutating, heavy.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `area` | integer | yes | The area id, 0 to 63. |
| `cost` | number | yes | Cost per meter, finite and positive; 1 is plain ground. |

`area` is from 0 to 63.

Result: `{ costs: [{ area, cost }...] }`.

Errors: `F0002`, `F0003`. `data.reason`: `nav_no_mesh`, `nav_invalid_area`, `nav_invalid_area_cost`.

```json
nav.set_area_cost {"area": 2, "cost": 3.0}
```

```json,ignore
→ {
  "message": "no NavTileCache or NavMesh in this app to carry off-mesh links",
  "code": -32001,
  "data": {
    "code": "F0003",
    "hint": "see book/agents/navigation-crowds.md",
    "reason": "nav_no_mesh"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:nav.set_area_cost -->

### `nav.set_area_table`

Replaces the level's authored area costs (saved with the level) and applies them to the live mesh now; crowd agents replan.

*EditScene, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `areas` | array | yes | Rows { area: 0..=63, cost: finite > 0, name? }; at most 256; a later row for an area wins. |

`areas` has at most 256 items.

Result: `{ areas: [{ area, cost, name }...], costs: [{ area, cost }...] }`.

Errors: `F0002`. `data.reason`: `nav_invalid_area`, `nav_invalid_area_cost`.

```json
nav.set_area_table {"areas": [{"area": 3, "cost": 4.0, "name": "mud"}]}
→ { "areas": [ { "area": 3, "cost": 4.0, "name": "mud" } ], "costs": [ { "area": 3, "cost": 4.0 } ] }
```

<!-- capture:nav.set_area_table -->

## Workflow: send one agent along a lane

1. `nav.lanes_status`. If it returns `nav_no_lane_graph`, install connected lanes with `nav.lanes_set` (the earlier examples show points and next links). Read status again; crowd is null until an agent exists.
2. `nav.lane_nearest` at the agent’s point and keep a non-null lane. Optionally use `nav.lane_check` when this app also has a navmesh; a lane graph itself does not need one.
3. `nav.lane_add_agent` on that lane and keep the returned id. The first agent creates the lane crowd.
4. `nav.lane_route` from the current lane to the goal lane. A null route means the flags or the graph do not connect them.
5. `nav.lane_set_destination` with the agent id and goal lane, then `nav.lane_step` with steps 1 and `nav.lane_agents` to read the new position and state. Repeat fixed steps until the agent stops at its destination.
6. `nav.lane_set_enabled` closes a user id (a door, a blocked street) and `nav.lane_remove_agent` drops the agent when it arrives.

<!-- /depth-2:method-reference -->
