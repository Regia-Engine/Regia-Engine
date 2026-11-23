# Navigation: off-mesh links and moving obstacles

Off-mesh links join two points of the navmesh with a move that is not walking: a jump across a
gap, a drop off a ledge, a ladder, a teleport. Each link has a `user_id` (the game's name for it),
a `kind`, a snap `radius`, a `cost_scale`, filter `flags`, an `enabled` switch and a traversal
`duration`. Links can be placed by hand, loaded from a `.navlinks.ron` file, or found on the mesh
by the generator. A link stops being used when it is switched off, when an obstacle covers
either end, or when an end has no polygon within its radius.

The link methods (`nav.links`, `nav.set_links`, `nav.set_link_enabled`, `nav.generate_links`)
work on the `regia_nav::NavTileCache` resource when there is one (its links survive tile
rebuilds), otherwise on the `regia_nav::NavMesh` resource. Crowd agents cross links over time
(see [navigation crowds](navigation-crowds.md) for the crowd itself). Design:
`docs/specs/92-navmesh-offmesh.md`, ADR-1821.

`nav.find_path` and `nav.find_nearest` do not follow that choice. They use the `NavMesh` resource
whenever one is present, and the tile cache only when there is no `NavMesh`
([navigation](navigation.md)). An `active` link stored on the cache is an edge of paths on that
cache. While a `NavMesh` resource is also present, `nav.find_path` searches the `NavMesh` and
does not walk the cache's links. `nav.get_mesh` describes the `NavMesh`, not the cache.

## Listing links

`nav.links` lists every link with its state:

```json
{}
→ { "links": [ { "index": 0, "user_id": 5, "kind": "jump", "from": [-1.5, 0, 0], "to": [1.5, 0, 0],
      "bidirectional": true, "enabled": true, "flags": 1, "area": 0, "radius": 0.5,
      "cost_scale": 1.0, "duration": 0.0, "state": "active", "from_poly": 3, "to_poly": 7 } ],
    "active": 1, "blocked": 0, "disabled": 0, "unresolved": 0 }
```

`state` is one of:

| State | Meaning |
|---|---|
| `active` | Both ends are on the mesh the link is stored on; the link is an edge of that mesh's path graph (`from_poly`, `to_poly`). It is not an edge of `nav.find_path` when that query is reading a different resource. |
| `blocked` | An obstacle on that mesh covers an end. On a tile cache that is a cache obstacle grown by the agent radius. On a `NavMesh` it is also a `nav.add_obstacle` box (the stored polygons stay as they are; the status uses the same boxes a path query carves). |
| `disabled` | The link's `enabled` switch is off. |
| `unresolved` | An end has no polygon within `radius` (in X and Z; vertically within the larger of `radius` and the agent's step height). |

`from_poly` and `to_poly` are `null` unless the link is active. Polygon indices change whenever
the tile cache rebuilds a tile.

## Setting links

`nav.set_links` replaces every link. Give either `links`, an array of link objects, or `ron`, the
text of a `.navlinks.ron` file:

```json
{ "links": [ { "from": [-1.5, 0, 0], "to": [1.5, 0, 0], "kind": "jump", "user_id": 5, "radius": 0.5 },
             { "from": [4, 0, 0], "to": [4, 3, 0], "kind": "climb", "bidirectional": false,
               "flags": 2, "duration": 1.5 } ] }
→ { "count": 2, "active": 2 }
```

Only `from` and `to` are required. Defaults: `bidirectional` true, `kind` `"jump"` (also `"drop"`,
`"climb"`, `"teleport"`, and `"custom"` with a `custom` number), `user_id` 0, `radius` 2 m,
`cost_scale` 1, `flags` 1, `enabled` true, `duration` 0 (automatic), `area` 0. The searched path
cost of a link is its 3D length times `cost_scale`, times the mesh cost of the link's `area`,
times that area's scale on the query filter. Area 0 and a default filter are both 1, so a default
link costs its length. Raising `cost_scale` or the area cost makes agents prefer walking.

The same links as a file:

```ron
(
    format_version: 1,
    links: [
        (from: (-1.5, 0.0, 0.0), to: (1.5, 0.0, 0.0), kind: Jump, user_id: 5, radius: 0.5),
        (from: (4.0, 0.0, 0.0), to: (4.0, 3.0, 0.0), bidirectional: false, kind: Climb, flags: 2, duration: 1.5),
    ],
)
```

The format is described in `docs/formats/navlinks-ron.md`.

## Switching links

`nav.set_link_enabled` switches every link with a `user_id` on or off (a drawbridge, a locked
door, a ladder that was kicked down):

```json
{ "user_id": 5, "enabled": false }   → { "changed": 1 }
```

`changed` is 0 when the links already had that state. A `user_id` no link has is the error
`nav_unknown_link`.

## Generating links

`nav.generate_links` looks along the mesh's wall edges for ledges and gaps. The height difference
is the landing polygon's height minus the sample height, and the gap is no wider than
`max_jump_gap` (default 2 m), measured between the walkable edges once the agent radius is added
back on both sides.

- At most the mesh agent's `max_climb` either way (default 0.5 m), and only after the probe has
  crossed an empty step: one bidirectional `jump`. A neighbour flush with the edge and no portal
  is not a gap.
- Higher than `max_climb`, and at most `max_jump_up` (default 1 m): a one-way `jump` up.
- Lower than `-max_climb`, and no deeper than `max_drop` (default 3 m): a one-way `drop`.

A landing whose rise exceeds `max(max_jump_up, max_climb)` stops that probe (a wall). A floor
deeper than `max_drop` is ignored. Samples are `spacing` apart along each wall (default 2 m).

```json
{ "max_drop": 3, "max_jump_gap": 2 }
→ { "links": [ { "index": 0, "user_id": 281474976710656, "kind": "jump", "from": [-1.35, 0, -2.6],
      "to": [1.35, 0, -2.6], "bidirectional": true, ... } ], "count": 6, "applied": false }
```

By default the links are only listed. With `"apply": true` they are stored: they replace any links
with user ids from `first_user_id` (default 2^48) up to 2^32 above it (an earlier generated set) and
keep every other link, so running it again does not double them. On a tile cache only the cache's
own links are replaced or kept: the links a streamed cache loaded with its tiles stay with their
tiles and are listed once.

Optional parameters besides the distances: `first_user_id` (an integer, default 2^48: where the
generated ids start), `flags` (the filter bits every generated link gets, default 1, `walk`) and
`clearance` (`"none"`, the default, or `"level"`: test each candidate against the level's colliders).
The result also carries `clearance` and `rejected` (how many candidates the clearance test refused).

The protocol generator sees only the mesh. It cannot tell a thin wall from a gap, or a railing
from an open edge, so check the result (with `nav.links` and the `nav_mesh` gizmo) before applying.
Game code can pass `regia_nav::TriangleClearance` (a test against the level triangles) to
`regia_nav::generate_links` instead.

## Crowd agents on links

A crowd agent that comes close to a link's take-off point enters the state `off_mesh_link`, moves
to the take-off point and then in a straight line to the landing, over whole fixed steps: the link's
`duration`, or its length over the agent's top speed when `duration` is 0 (a `teleport` with no
duration takes one step). Other agents neither avoid it nor push it meanwhile. The agent object in
`nav.crowd_status` shows the crossing (`origin` is where the agent stood when it started):

```json
"link": { "user_id": 5, "kind": "jump", "progress": 0.5, "origin": [-2.3, 0, 0], "start": [-1.5, 0, 0], "end": [1.5, 0, 0] }
```

`link` is `null` when the agent is not on a link. `nav.crowd_step` lists the take-offs and landings
of the steps it ran:

```json
{ "steps": 600 }
→ { ..., "link_events": [ { "agent": 0, "user_id": 5, "kind": "jump", "phase": "started", "position": [-2.3, 0, 0] },
                         { "agent": 0, "user_id": 5, "kind": "jump", "phase": "finished", "position": [1.5, 0, 0] } ] }
```

Game code reads the same events from `NavCrowd::link_events`: after `advance` they cover all its
steps, and with `NavCrowdPlugin` it reads them in `FixedUpdate` after `NavCrowdStep`. An agent removed
during a crossing reports no `finished`.

A crossing under way goes on when its link is disabled or blocked. `nav.set_links` and
`nav.set_link_enabled` make crowd agents plan again, so an agent in `no_path` starts walking when a
link comes back. Game code does the same with `NavCrowd::request_replan_all`; on a tile cache, a
crowd following `NavTileCache::revision` replans by itself.

`stats.link_traversals` in `nav.crowd_status` counts finished crossings. Game code can switch the
crossing to the old instant teleport with `NavCrowdConfig::link_traversal`, and limit which links an
agent may use with `CrowdAgentParams::link_filter` (a `NavQueryFilter`: include and exclude flag masks,
plus the agent's area costs and left-out areas; see [filters](navigation-lanes.md#filters)).

## Moving obstacles

`nav.tile_cache_add_obstacle` also takes an `oriented_box`: `center`, `half_extents` (each > 0)
and `yaw` (radians about +Y). `nav.tile_cache_move_obstacle` gives an existing obstacle a new
shape or place and keeps its id; it rebuilds the tiles of the old and the new footprint. It takes the shape
fields of `nav.tile_cache_add_obstacle`: a `cylinder` has `base` (the centre of its bottom disc), `radius` and
`height`, an axis-aligned `box` `min` and `max`, an `oriented_box` `center`, `half_extents` and `yaw`:

```json
{ "id": 0, "shape": "oriented_box", "center": [2, 0.5, 1], "half_extents": [0.5, 0.5, 0.3], "yaw": 0.6 }
→ { "id": 0, "touched": [[3, 2], [4, 2]], "rebuilt": [[3, 2], [4, 2]], "pending": [], "revision": 4 }
```

An obstacle that comes to rest on a link's end makes the link `blocked`; moving it away makes it
`active` again.

## Errors

Errors carry `data.reason`:

| `data.reason` | Code | Meaning |
|---|---|---|
| `nav_no_mesh` | F0003 | Neither a `NavTileCache` nor a `NavMesh` to carry links. |
| `nav_invalid_link` | F0002 | A link has a non-finite end, a radius or cost scale that is not positive, a negative or non-finite duration, or there are more than 65536 links. |
| `nav_link_parse` | F0002 | The `ron` text is not a valid `.navlinks.ron` file. |
| `nav_link_version` | F0002 | The file's `format_version` is 0 or newer than 1. |
| `nav_unknown_link` | F0003 | No link has that `user_id` (`nav.set_link_enabled`). |
| `nav_invalid_link_params` | F0002 | A generator setting is not finite, a distance is negative, or `spacing` or `radius` is not positive. |
| `nav_no_tile_cache` | F0003 | `nav.tile_cache_move_obstacle` without a `NavTileCache`. |
| `nav_unknown_obstacle` | F0003 | No obstacle has that id. |
| `nav_invalid_obstacle` | F0002 | A non-finite shape, or an oriented box with a half extent that is not positive. |

A bad parameter shape (neither `links` nor `ron`, an unknown `kind`, a `shape` other than
`cylinder`, `box` or `oriented_box`) is the usual F0002 without a `reason`.

Link reservation and queues (one agent per link at a time), links authored as scene components (`NavLinkAnchor`),
`.navlinks.ron` files loaded at run time and `nav.generate_links` with `clearance: "level"` are on
[link queues, road lanes, triangle areas and link files](navigation-authoring.md) (ADR-2420).

## Jump arcs, streamed links and grid growth (ADR-2589)

### Jump arcs

With `nav.crowd_set_link_arcs { "enabled": true }` (Rust: `NavCrowdConfig::link_arcs`,
`NavLinkArcConfig::on()`), crowd agents cross **Jump** links along a ballistic arc and **Drop** links
along a falling parabola instead of a straight segment. Climb, Teleport and Custom links stay straight.
Arcs are off by default, so a crowd that does not turn them on steps exactly as before.

- A jump's apex rises `apex_clearance` (default 0.5 m) above the higher end, but never more than
  `max_jump_speed² / (2 gravity)` above the take-off point (default 6 m/s, 9.81 m/s²: 1.83 m). When the
  landing is higher than that, the arc still lands there with its apex at the landing height and
  `speed_limited` set: play a climb.
- A drop steps off level (apex at the take-off point) and falls for `sqrt(2 h / g)`.
- The crossing takes the short walk to the take-off point (at the agent's `max_speed`) plus the
  arc's flight time, rounded up to whole fixed steps; an authored `duration` does not apply to an arc.
  Positions follow the arc at every step and two runs give the same `digest`.
- `nav.link_traversals` lists the agents on links now. Each has `progress`, `steps`, `total_steps`,
  `position`, and `arc` (`null` for a straight crossing) with `apex`, `takeoff_velocity`, `gravity`,
  `flight_seconds`, `flight_time` (seconds into the flight, 0 on the walk), `walk_seconds`, `velocity`
  and `speed_limited`. `takeoffs` lists the crossings that started in the last step.
  `link` is the link's index in the mesh's list. It follows the link when the list changes under the
  crossing. It is `null` once the link has left the mesh (its tile unloaded, or the list was replaced
  without it): the crossing still goes on to its landing. In Rust the same crossing reports
  `NavLinkTraversal::link == regia_nav::NAV_LINK_GONE` (`usize::MAX`).
- In Rust, `NavCrowdPlugin` writes a `NavLinkTraversal` message at each take-off (read it in
  `FixedUpdate` after `NavCrowdStep`); `NavCrowd::link_takeoffs` and `NavCrowd::link_traversals` give
  the same without messages.

```json
{ "enabled": true, "gravity": 9.81, "max_jump_speed": 6, "apex_clearance": 0.5 }
→ { "enabled": true, "gravity": 9.81, "max_jump_speed": 6.0, "apex_clearance": 0.5 }
```

### Links streamed per tile

In a streamed world, a link belongs to the tile under its `from` point (a link that crosses a tile border
belongs to its start tile). The cook writes it into that tile's cooked bytes (nav cell v2). When the tile
loads, the link joins the mesh on the next `update`, and it leaves when the tile unloads. A link whose
landing tile is not loaded is not used: its state is `unresolved` for that end, the path query skips it,
and it is used again once the tile loads. A nearby loaded polygon inside the snap radius does not count.

An agent on a link when its **landing** tile unloads goes back to its take-off point, which is still
loaded, and replans from there (link event `cancelled`, stat `cancelled`). An agent on a link when its
**take-off** tile unloads finishes the crossing, because it is leaving that tile. Either way, no agent ever
stands on an unloaded tile.

`nav.tile_links` lists, per tile that owns a link: `loaded`, `links`, `streamed` (brought in with the
cell), `active` (graph edges now), `waiting` (unusable until their landing tile loads) and `user_ids`.

### Grid growth

`nav.tile_cache_grow { "min", "max" }` (Rust: `NavTileCache::grow_to`, `rebuild_region_growing`) grows a
cache built whole by whole tiles so it covers the box. Then `nav.region_rebuild` or
`NavTileCache::rebuild_region` voxelises new geometry there. The existing tiles keep their polygons.
They also keep their `(tile_x, tile_z)` when the grid grows toward +X or +Z. Toward -X or -Z they move
by `tile_offset`: same tile, same place in the world. A streamed cache keeps its cooked layout and refuses
(`nav_grid_growth`). The same refusal covers a non-finite box, a grid past 64 Mi cells, and a -X or -Z shift
whose cell count is not exactly representable as f32 (ADR-5007). Nothing changes when growth is refused.

| `data.reason` | Code | Meaning |
|---|---|---|
| `nav_invalid_crowd_config` | F0002 | `nav.crowd_set_link_arcs` got a gravity or jump speed that is not positive and finite, or a negative `apex_clearance`. |
| `nav_grid_growth` | F0002 | `nav.tile_cache_grow` on a streamed cache, with a non-finite box, past 64 Mi cells, or a -X/-Z cell shift that is not exactly an f32 (ADR-5007). |
| `nav_no_crowd` | F0003 | `nav.link_traversals` without a `NavCrowd`. |
| `nav_no_tile_cache` | F0003 | `nav.tile_links` or `nav.tile_cache_grow` without a `NavTileCache`. |
