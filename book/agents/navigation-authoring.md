# Navigation: link queues, road lanes, triangle areas and link files

These methods cover the nav-world authoring pieces of ADR-2420: off-mesh links that carry one agent at a time,
crowd lanes laid along road splines, area ids painted per triangle when the navmesh is cooked, links checked against
the level's colliders, and link and lane files loaded at run time. See
[off-mesh links](navigation-links.md), [area costs and crowd lanes](navigation-lanes.md) and
[region rebuilds and tile streaming](navigation-world.md) for the pieces underneath.

## Link reservation and queues

A ladder or a narrow drop should carry one agent at a time. `nav.crowd_set_link_queue` turns that on for every
off-mesh link of the crowd (`regia_nav::NavLinkQueueConfig` in Rust):

```json
{ "exclusive": true, "timeout_steps": 120 }
→ { "exclusive": true, "timeout_steps": 120, "holders": [], "queues": [], "waits": 0, "timeouts": 0 }
```

An agent that reaches a take-off point while another agent is on the link, or while others already wait for it,
stops there in state `link_queued`, with the link event `queued`. The queue is first come, first served; agents
arriving in the same fixed step queue in agent order, so a run replays exactly. Both directions of a bidirectional
link share one queue. When the link is free, the first agent in line takes off (event `started`). A waiting agent
stands still but still counts for avoidance, so others walk round it.

With `timeout_steps` above 0, an agent that has waited that many whole steps gives up on the next one: state
`link_timeout`, link event `timed_out`, reason `nav_link_queue_timeout`. It keeps its target and walks again on the
next `nav.crowd_set_target`, or when a mesh change replans the crowd. 0 waits for ever. Removing the agent on the link frees it. Turning `exclusive` off
releases every queue: the waiting agents walk on and cross as before.

`nav.crowd_link_queues` shows who crosses and who waits:

```json
{}
→ { "exclusive": true, "timeout_steps": 120,
    "holders": [ { "link": 0, "user_id": 9, "agent": 3 } ],
    "queues": [ { "link": 0, "user_id": 9, "agents": [5, 4] } ], "waits": 2, "timeouts": 0 }
```

`nav.crowd_status` agents gain `link_wait` (`null` or `{ user_id, ticket, steps }`), and its stats gain
`link_queue_waits` and `link_queue_timeouts`.

## Lanes from road splines

`nav.lanes_from_splines` reads the scene's road splines, never changing them, and lays crowd lanes along them
(`regia_nav::NavRoadNetwork::add_road` in Rust). By default it takes every entity with a `Spline` and a `RoadOf`
(`roads_only: false` takes every spline; `entities` picks some), in entity order, at the pose of its entity and
parents:

```json
{ "width": 7, "lanes_each_way": 1, "two_way": true, "join_distance": 1, "apply": true }
→ { "roads": [ { "entity": 4294967301, "lanes": [0, 2] } ], "lane_count": 2, "total_length": 84.6,
    "lanes": [ { "lane": 0, "user_id": 0, "length": 42.3, "width": 3.5, "next": [], "points": [[...]] } ],
    "applied": true }
```

The carriageway (`width`, a road profile's `width_m`) is split into equal lanes: `lanes_each_way` run with the
spline on its right-hand side and, when `two_way`, as many run against it on the left-hand side. Right is the
tangent crossed with +Y, level whatever the spline's roll. `center_offset` moves every lane sideways (a pavement
beside the road). Each lane lies within `max_error` (default 0.05 m) of its exact offset curve: the generator halves a
stretch until its quarter points are that close to the chord. A closed spline gives looping lanes. Roads join where
their lane ends meet: `join_distance` joins every lane end to the lane starts that close. On a bend tighter than a
lane's offset the offset curve folds, and so does the lane: keep road curves wider than half the road. With `apply`
the lanes become the `NavLaneGraph` and a lane crowd is dropped (lane indices do not carry over); without it they are
only listed (the first 256 point by point).

Each lane gets the speed limit `speed` (default 1.4 m/s), `weight` (default 1) and the filter bits `flags` (default
1). `first_user_id` (default 0: unnamed) is the user id of the first road's first lane; each further road adds
64.

Errors (`nav_invalid_lane_config`): a width, speed or weight that is not finite and positive, `lanes_each_way`
outside 1 to 8, `max_error` below 0.001, an invalid spline or one without length, or a lane needing more than 4096
points (raise `max_error`).

## Area ids per triangle

Area volumes paint the walkable cells at run time. A level whose surfaces already know their ground (a road mesh,
a mud decal) can give the cook an area id per triangle instead: `NavMeshBuilder::build_with_triangle_areas` and
`NavTileCache::build_with_triangle_areas` in Rust, and `regia_nav::paint_triangle_areas` turns area volumes into
triangle ids (a triangle takes the last volume holding its centroid). The walkable cells keep their triangle's area,
`cook_cell` writes it into the cooked tiles, so a streamed tile comes back painted with no volume registered, and a
region rebuild keeps it. A cook with no painted triangle is byte for byte the cook without triangle areas.

The streaming cook (`regia cook`, a level with nav settings) uses it (ADR-2788): put a `NavSurface { area }`
component (`regia_framework::nav_world`, reflected, `area` 0..=63) on an entity that has a static collider and
every triangle of that collider carries the id into the cooked tiles. A `NavAreaZone` still paints over it, terrain
is plain ground, and a level with no `NavSurface` cooks the same bytes as before. An id of 64 or more fails the cook
(`nav surface: area N is past the 63 ids a tile can hold`). The component is read by the cook only: a navmesh a game
builds at run time from colliders ignores it.

`nav.cook_triangle_areas` rebuilds the `NavTileCache` this way, keeping its settings, links and area costs (its
obstacles and area volumes are not carried over):

```json
{ "triangles": [ [[-8, 0, -8], [8, 0, -8], [8, 0, 8]], [[-8, 0, -8], [8, 0, 8], [-8, 0, 8]] ], "areas": [3, 0] }
→ { "triangles": 2, "painted": 1, "polys": 2, "poly_areas": [[0, 1], [3, 1]], "revision": 1,
    "dropped_obstacles": 0, "dropped_volumes": 0 }
```

`volumes` (shapes as for `nav.add_area_volume`, plus `area`) paint over `areas`. Errors: no tile cache
(`nav_no_tile_cache`), an `areas` list that is neither empty nor one id below 64 per triangle
(`nav_invalid_build_input`). Volume areas must be below 64 and their shapes must be valid.
An invalid volume returns an error before replacing the cache; its obstacles and revision stay unchanged.

## Links that clear the level's colliders

`nav.generate_links` with `"clearance": "level"` tests every candidate jump and drop against the level's static
colliders (`regia_framework::nav_world::generate_level_links`: the same geometry the navmesh is built from, chunk
hulls included), so a thin wall or a railing the navmesh cannot see stops a jump. The result gains `clearance` and
`rejected` (the candidates the colliders refused). The default `"none"` sees only the mesh, as before.

## Link and lane files at run time, and links as scene components

`regia_nav::NavAuthoredLinksPlugin` (opt-in) loads `.navlinks.ron` and `.navlanes.ron` files at run time
(`NavLinkSet::load`, `NavLaneSet::load`) and puts `NavLinkAnchor` components on the navmesh. An anchor's link takes
off at its entity and lands at `to` in the entity's own space; moving the entity moves the link. Each fixed step,
before the region and crowd steps, the files' links and then the anchors' (by user id, then entity) replace the
navmesh's links when they changed. Until the first file link or anchor appears the plugin changes nothing; from
then on it owns the list. The lane files, appended in order, become the lane graph.

`nav.set_link_files` names the files; they are read on the next frame:

```json
{ "links": ["levels/dock.navlinks.ron"], "lanes": ["levels/dock.navlanes.ron"] }
→ { "links": ["levels/dock.navlinks.ron"], "lanes": ["levels/dock.navlanes.ron"], "file_links": 0,
    "file_lanes": 0, "anchor_links": 0, "invalid_anchors": 0, "applied": 0, "errors": [] }
```

`nav.authored_links_status` returns the same object once they loaded. A file that fails is skipped and listed in
`errors` with its reason: `nav_link_io` / `nav_lane_io` (missing, unreadable, over 16 MiB, not UTF-8),
`nav_link_parse`, `nav_link_version`, `nav_invalid_link`, `nav_lane_parse`, `nav_lane_version`, `nav_invalid_lane`.
Without the plugin both methods answer `nav_no_authored_links`.

## Schedule order

With `NavWorldPlugin`, one fixed step runs physics, the destruction write-back, the nav-world forwarders, the region
rebuilds, the crowd and then the lane crowd, each ordered by an edge (`regia_framework::nav_world` module docs). A
crowd system ordered before `PhysicsStep` closes a cycle and the schedule refuses to build: feed crowd velocities to
physics on the next step instead.

## Reasons

| Reason | When |
|---|---|
| `nav_link_queue_timeout` | an agent gave up waiting for a reserved link (state `link_timeout`) |
| `nav_invalid_lane_config` | invalid road lane settings or road spline |
| `nav_invalid_build_input` | a bad triangle area list |
| `nav_no_tile_cache` | `nav.cook_triangle_areas` without a `NavTileCache` |
| `nav_no_mesh` | `nav.crowd_set_link_queue` without a navmesh |
| `nav_no_crowd` | `nav.crowd_link_queues` without a crowd |
| `nav_no_authored_links` | the file methods without `NavAuthoredLinksPlugin` |
| `nav_link_io`, `nav_lane_io` | a runtime link or lane file could not be read |
| `nav_invalid_area` | `nav.cook_triangle_areas` volume area in `64..=255` |
| `nav_invalid_obstacle` | `nav.cook_triangle_areas` volume with invalid dimensions or non-finite coordinates |
