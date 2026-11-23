# 92 — Navmesh off-mesh links and dynamic obstacles

Status: proposed (ADR-1821). Plan items: `master-plan.md` § 3 rung v0.3.x (b) "navmesh off-mesh links and dynamic
obstacles", table rows I4 (runtime carving, exists) and K3 (multi-level navmesh with off-mesh links at portals), and
`22-tier2.md` § T2.2 ("off-mesh links (jumps, ladders); area costs", "runtime tile rebuilds for dynamic obstacles").
Crate: `regia_nav` (plus its agent surface in `regia_agent::nav`).

No reviewed public-spec set covers this item; the Regia docs and the existing code are the source. Public sources:
the Recast & Detour documentation and code (zlib, already ported in part by ADR-1484) for the off-mesh connection and
query-filter ideas. The link generator, the link table and the crowd traversal are original code.

## 1. Built baseline (main 28fb09309)

- `regia_nav::OffMeshLink { from, to, bidirectional, area }` exists. `NavQuery::find_path` resolves every link on
  every call (a nearest-polygon search per endpoint with a fixed 2 m box), and A* scans the whole link list at every
  expanded polygon. `NavCrowd` calls the same resolution once per agent per step in its corridor check.
- Crowd agents cross a link instantly (a teleport) once within `2.25 * radius` of the take-off point.
- Links have no identity, no enable switch, no per-agent filter, no cost scale and no file format. There is no way to
  produce links except by hand.
- Dynamic obstacles: `NavObstacles` (boxes carved per query) and `NavTileCache` (cylinders and boxes, per-tile
  rebuilds, ADR-1484). An obstacle cannot move without changing id, there is no rotated box, and an obstacle standing
  on a link's landing does not disable the link (the 2 m snap finds a polygon next to it).

## 2. Purpose

Agents use jumps, drops, ladders and teleports the way the level designer or the generator placed them, only when
they are allowed to, over a visible traversal time that gameplay can animate, and stop using them when a door or a
crate blocks one end. Obstacles can move and rotate without losing their identity.

## 3. API (Rust, `regia_nav`)

### 3.1 Links (`mesh.rs`, `links.rs`)

```rust
pub struct OffMeshLink {
    pub from: Vec3, pub to: Vec3, pub bidirectional: bool, pub area: u8,   // existing
    pub user_id: u64,        // game id; 0 = unnamed (ids need not be unique, but enable/disable acts on every match)
    pub kind: NavLinkKind,   // Jump (default) | Drop | Climb | Teleport | Custom(u16)
    pub radius: f32,         // endpoint snap radius, metres (default 2.0 = the old fixed box)
    pub cost_scale: f32,     // stored edge = 3D length * cost_scale (default 1.0, > 0);
                             // A* then multiplies by the mesh area cost and the filter scale
    pub flags: u16,          // filter bits (default NavLinkFlags::WALK = 1)
    pub enabled: bool,       // runtime switch (default true)
    pub duration: f32,       // traversal seconds; 0 = 3D length / agent max_speed
}
impl OffMeshLink { pub fn new(from, to) -> Self; fn one_way(self); fn with_kind(self, k); fn with_user_id(self, id);
                   fn with_flags(self, f); fn with_radius(self, r); pub fn validate(&self) -> Result<(), NavLinkError> }

pub struct NavQueryFilter { pub include_flags: u16, pub exclude_flags: u16,   // default: all, none
                           pub area_costs: NavAreaCosts, pub excluded_areas: u64 }   // ADR-1934
impl NavQueryFilter { pub fn passes(&self, link: &OffMeshLink) -> bool }      // (flags & include) != 0 && (flags & exclude) == 0
                                                                               // && the link's area is not excluded

impl NavMesh {
    pub fn set_off_mesh_links(&mut self, links: Vec<OffMeshLink>);
    pub fn set_link_enabled(&mut self, user_id: u64, enabled: bool) -> usize; // links changed
    pub fn link_statuses(&self) -> Vec<NavLinkStatus>;
}
pub struct NavLinkStatus { pub index: usize, pub user_id: u64, pub state: NavLinkState }
pub enum NavLinkState { Active { from_poly: u32, to_poly: u32 }, Disabled, Blocked, Unresolved { from: bool, to: bool } }

impl NavQuery<'_> { pub fn find_path_filtered(&self, start, end, extents, filter: &NavQueryFilter) -> Option<NavPath> }
```

The resolved link table (each enabled link's endpoint polygons, its cost, and a per-polygon adjacency list) is built
once whenever the mesh or its links change, not per query. `off_mesh_links` stays a public field: the table keeps a
fingerprint of the link list, and a query on a mesh whose list was edited in place resolves a fresh table for that
query (correct, only slower) until `set_off_mesh_links` is called.

An endpoint resolves when a polygon lies within `radius` in X and Z and within `max(radius, agent.max_climb)` in Y. A
link is **blocked** when an active obstacle covers either endpoint (the tile cache's obstacles, inflated by the agent
radius like their carve, or a `NavObstacles` box). Blocked and disabled links are not graph edges.

### 3.2 Link generator (`link_gen.rs`)

```rust
pub struct NavLinkGenParams { pub max_drop: f32, pub max_jump_gap: f32, pub max_jump_up: f32, pub spacing: f32,
                              pub radius: f32, pub first_user_id: u64, pub flags: u16 }
pub struct NavLinkCandidate { pub from: Vec3, pub to: Vec3, pub kind: NavLinkKind, pub bidirectional: bool,
                              pub from_poly: u32, pub to_poly: u32 }
pub fn generate_links(mesh: &NavMesh, params: &NavLinkGenParams,
                      clearance: impl FnMut(&NavLinkCandidate) -> bool) -> Result<Vec<OffMeshLink>, NavLinkError>;
```

Every polygon edge part without a portal is a wall. Sample points are spread evenly along each wall part
(`max(1, floor(length / spacing))` of them, at most 4096 per wall part, centred). From each sample the generator steps
outward (perpendicular to the edge, away from the polygon centre) in `cell_size` increments up to
`2 * agent.radius + max_jump_gap` plus one cell (the walkable surface is eroded by the agent radius on both sides of a
gap), and
at each step looks at the polygons whose XZ footprint holds the probe:

- a polygon whose rise above the sample exceeds `max(max_jump_up, agent.max_climb)` stops the probe
  (a wall top; a rise the agent can still step is not a wall, because that landing is the bidirectional
  jump below);
- a polygon lower than `max_drop` is ignored (the floor of a pit too deep to drop into);
- otherwise the highest one is the landing: a bidirectional **Jump** when within `max_climb` of the sample height
  (only after at least one empty step: a level polygon right at the edge without a portal is not a gap), a one-way
  **Jump** when higher, and a one-way **Drop** when lower.

The landing point is pulled `agent.radius` inside the landing polygon, and the take-off point sits `agent.radius`
inside the source polygon, so an agent standing on either end is on the mesh. The `clearance` callback can reject a
candidate (a physics ray cast against the level, say); the generator itself does not see level geometry. A Jump that
would duplicate the reverse of an existing bidirectional Jump within `spacing` is skipped. Output order is the polygon
order, then the edge order, then the sample order: deterministic for a given mesh and parameters. User ids count up
from `first_user_id` (default `1 << 48`, out of the way of hand-placed ids).

### 3.3 Crowd traversal (`crowd.rs`)

- `CrowdAgentState::OffMeshLink` (protocol name `off_mesh_link`): the agent is on a link.
- `NavCrowdConfig::link_traversal: NavLinkTraversalMode` — `Timed` (default) or `Instant` (the old teleport).
- `CrowdAgentParams::link_filter: NavQueryFilter` — the links this agent may use (planning and traversal).
- `CrowdAgent::link_traversal() -> Option<&NavLinkTraversal>` with the fields `link`, `user_id`, `kind`, `origin`,
  `start`, `end`, `steps`, `total_steps`, `step_seconds` and the methods `elapsed()`, `duration()`, `progress()` and
  `position()`.
- `NavCrowd::link_events() -> &[NavLinkEvent]` — the `Started` / `Finished` events of the last `step` or `advance`
  (all the steps of one `advance`, in step order). Within a step the landings (and `Cancelled` events, § 3.6) of
  crossings already under way come first, then the take-offs, each in agent order. An agent removed during a
  crossing reports no `Finished`. With `NavCrowdPlugin`, read them in `FixedUpdate` after `NavCrowdStep`.
- `CrowdStats::link_traversals` counts finished traversals.

An agent starts a crossing within `2.25 * radius` of the take-off point, so it does not stand on it yet. Over
`duration` whole steps (rounded up, at least one) its position moves at one speed from where it stood (`origin`) to
the take-off point (`start`), then in a straight line to the landing (`end`, the authored landing clamped onto the
landing polygon); no step of the crossing jumps. `duration` 0 means the link's length over `max_speed` (one step for
a `Teleport`), and the short walk shares that time. The agent does not steer, is not pushed, and is not a neighbour
or a collider for other agents. `clear_target` and `set_target` during a traversal take effect on landing. Gameplay
plays its jump, climb or teleport animation from the events or the traversal's `progress()`; the crowd only moves
the point.

A link that is disabled, blocked or removed during a crossing does not stop it. When the mesh changes during a
crossing (`sync_mesh_revision` or `request_replan_all` asked for a replan), the agent finds the landing polygon again
when it lands: polygon numbers do not survive a tile rebuild.

**Jump arcs (ADR-2589).** `NavCrowdConfig::link_arcs: NavLinkArcConfig { enabled, gravity, max_jump_speed,
apex_clearance }` (off by default; `NavLinkArcConfig::on()` = 9.81 m/s², 6 m/s, 0.5 m; `NavCrowd::set_link_arcs`).
With it on, a **Jump** crossing follows a parabola from `start` to `end` under `gravity`, with constant horizontal
speed. Its apex rises to `max(start.y, end.y) + apex_clearance`, capped at `start.y + max_jump_speed² / (2 gravity)`.
When the landing is above the cap, the apex is the landing height and `NavLinkArc::speed_limited` is set. A **Drop**
crossing leaves level (apex = `start`) and falls for `sqrt(2 (start.y - end.y) / gravity)`. A Drop link that goes up
is treated as a jump. Climb, Teleport and Custom links stay straight, and so does `Instant` mode. The crossing takes
`walk_seconds` (the walk from `origin` to `start` at `max_speed`) plus the arc's `flight_seconds`, rounded up to whole
steps (at least one). An authored `duration` does not apply to an arc. Step `k` of `n` sits at time
`k / n × (walk_seconds + flight_seconds)`, so the last step lands exactly. `NavLinkTraversal` gains `arc:
Option<NavLinkArc>` (`start`, `end`, `apex`, `takeoff_velocity`, `gravity`, `flight_seconds`, `speed_limited`;
`point_at(t)`, `velocity_at(t)`) and `walk_seconds`, with `crossing_seconds()` and `flight_time()`. It is an ECS
`Message`: `NavCrowdPlugin` writes one at every take-off, and `NavCrowd::link_takeoffs()` lists the same. Arc
positions use only multiplications and a square root of the inputs, so two runs give the same digest.

### 3.4 Dynamic obstacles (`tile_cache.rs`)

- `NavObstacleShape::OrientedBox { center, half_extents, yaw }` (yaw in radians about +Y).
- `NavTileCache::move_obstacle(id, shape) -> Result<Vec<(u32, u32)>, NavTileCacheError>`: the id stays, the tiles
  of the old and the new footprint are queued (returned, sorted, deduplicated).
- Every tile-cache update re-resolves the link table with the current obstacles, so links whose end is covered are
  `Blocked`; removing or moving the obstacle restores them.
- `NavTileCache::set_off_mesh_links` and a `set_link_enabled` that changes a link also increment `revision`: the
  routes changed, and a crowd following the revision replans (an agent in `no_path` finds a link that came back). On
  a plain `NavMesh`, which has no revision, call `NavCrowd::request_replan_all`. The agent methods do this themselves.

### 3.5 File format `.navlinks.ron` (v1)

```ron
(
    format_version: 1,
    links: [
        (from: (0.0, 0.0, 0.0), to: (0.0, -2.0, 3.0), bidirectional: false, kind: Drop, user_id: 7),
        (from: (4.0, 0.0, 0.0), to: (4.0, 3.0, 0.0), kind: Climb, flags: 2, duration: 1.5),
    ],
)
```

Every field but `from` and `to` is optional with the defaults of § 3.1. `regia_nav::NAV_LINKS_FORMAT_VERSION = 1`,
`NavLinkSet::from_ron_str`, `NavLinkSet::to_ron_string`, `nav_links_schema_fingerprint()` and
`nav_links_migration_sources()` (empty). Loading rejects a newer `format_version` and version 0 (`nav_link_version`), malformed RON
(`nav_link_parse`), more than `MAX_NAV_LINKS = 65_536` links and invalid values (`nav_invalid_link`). Registered with
`cargo xtask check-formats` and `regia upgrade-project` (unmigrated v1). Migration note: v1 is the first version;
links built in code before it need no migration (the new `OffMeshLink` fields have serde defaults, so serialized v0
link lists still read). Documented in `docs/formats/navlinks-ron.md`.

### 3.6 Links streamed per tile (ADR-2589, `tile_cache/stream.rs`)

A link belongs to the tile under its `from` point (`NavTileCache::link_tile`). A link that crosses a tile border
belongs to its start tile. `cook_cell` writes each tile's links into its cooked bytes (nav cell v2,
`docs/formats/nav-cell.md`): the cache's own links whose `from` lies on the tile, then the ones the tile streamed
in. A streamed cache keeps the links of each loaded tile (`tile_links(tx, tz)`). The mesh carries the cache's own
links, then each loaded tile's links in tile order, from the next `update`. `unload_cell` drops a tile's links. The
link resolution treats an endpoint on a tile that is not loaded as unresolved, even when the snap radius reaches a
loaded polygon. So a link into an unloaded tile is skipped by every path query (`find_path_streamed` included) until
that tile loads. `set_link_enabled` and `shift_origin` reach the streamed links. `link_tiles()` reports per tile:
loaded, links, streamed, active, and waiting (unusable while the landing tile is unloaded).

Mid-link rule: in `NavCrowd::sync_streaming`, an agent whose crossing lands on a tile that is no longer loaded is
called off. It goes back to the take-off point `start`, replans, and a `NavLinkPhase::Cancelled` event opens the next
step's events (`CrowdStats::link_cancellations`). The take-off tile is where the agent came from. If it unloaded too,
the agent then goes dormant there as usual. An agent whose take-off tile unloads mid-crossing finishes: it is leaving
that tile, and the landing is loaded.

### 3.7 Grid growth (ADR-2589, `tile_cache/grow.rs`)

`NavTileCache::grow_to(min, max) -> Result<NavGridGrowth, _>` grows a cache built whole by whole tiles so it covers
the XZ box. `rebuild_region_growing(min, max, triangles)` grows the grid, then runs `rebuild_region`. Existing tiles
keep their cells, polygons and rebuild counts. Toward +X/+Z their `(tile_x, tile_z)` stay the same. Toward -X/-Z the
origin moves back by whole tiles and every existing tile's coordinates move by `NavGridGrowth::tile_offset`. Only new
tiles and old partial edge tiles that the growth completes are re-polygonised. Obstacles, area volumes and queued
tiles follow the new numbering. New cells are not walkable until a region rebuild voxelises geometry there. A
streamed cache keeps its cooked layout, so growth is refused, as are a non-finite box, a grid past 64 Mi cells,
and a -X or -Z shift whose cell count is not exactly representable as f32 (ADR-5007)
(`NavTileCacheError::GridGrowth`, `nav_grid_growth`). Nothing changes on any of those errors.

## 4. Agent protocol (`book/agents/navigation-links.md`)

| Method | Params | Result |
|---|---|---|
| `nav.links` | none | `{ links: [ { index, user_id, kind, from, to, bidirectional, enabled, flags, radius, cost_scale, duration, state, from_poly, to_poly } ], active, blocked, disabled, unresolved }` |
| `nav.set_links` | `links` (array of link objects) or `ron` (a `.navlinks.ron` text) | `{ count, active }` |
| `nav.set_link_enabled` | `user_id`, `enabled` | `{ changed }` |
| `nav.generate_links` | `max_drop`, `max_jump_gap`, `max_jump_up`, `spacing`, `radius`, `apply` (default false) | `{ links: [...], count, applied }` |
| `nav.tile_cache_move_obstacle` | `id`, shape as for `nav.tile_cache_add_obstacle` (`oriented_box`: `center`, `half_extents`, `yaw`), `update` | `{ id, touched, rebuilt, pending, revision }` |

The methods act on the `NavTileCache` resource when one exists (its links persist through tile rebuilds), else on the
`NavMesh` resource. Crowd agent objects gain `link` (`null` or `{ user_id, kind, progress, origin, start, end }`) and
`nav.crowd_step` returns `link_events`. New reasons: `nav_invalid_link`, `nav_link_parse`, `nav_link_version`,
`nav_unknown_link`, `nav_invalid_link_params`. `nav.generate_links` sees only the mesh (no clearance test); with
`apply` it replaces the links whose user ids lie in `[first_user_id, first_user_id + 2^32)` and keeps the rest.
Game code passes `TriangleClearance::for_mesh(&level_triangles, &mesh)` (segment against triangle at knee and waist
height along an L-shaped move: level then down for a drop, up then level for a jump up) or its own physics test.
A triangle whose XZ bounds cover more than 2^20 buckets of 2 m, or whose bucket count does not fit in `i64`, is not
indexed; every clearance segment is still tested against it (ADR-5007).

## 5. Determinism

Link resolution, generation, A* and the traversal use no hashing with random state, no wall clock and no threads.
Ties in A* break by polygon index; a link edge between the same polygon pair as another breaks by link index. The
traversal counts whole fixed steps, so two runs with the same inputs give the same `NavCrowd::digest`.

## 6. Tests and acceptance

1. Link fields: `new` defaults, `validate` rejects non-finite points, a radius or cost scale `<= 0`, a negative or
   non-finite duration.
2. The link table is built once: N queries on an unchanged mesh reuse the one resolved table (none resolves its
   own); editing `off_mesh_links` in place still gives correct paths.
3. Disabled links, filtered links and links blocked by a tile-cache obstacle or a `NavObstacles` box are not used;
   removing the obstacle restores the route; `link_statuses` reports each state.
4. Cost scale: of two links over the same gap the cheaper one wins; scaling it up flips the choice.
5. Generator: a ledge gets a one-way Drop, a 1.5 m gap between equal floors gets a bidirectional Jump, a gap wider
   than `max_jump_gap` and a drop deeper than `max_drop` get none, the clearance callback rejects, output is identical
   across runs, a path across the generated link exists.
6. Crowd: an agent crosses a timed link in `ceil(duration / step)` steps with `Started`/`Finished` events, is
   `off_mesh_link` in between, is not pushed by a neighbour standing on the landing; `Instant` mode keeps the old
   behaviour; an agent whose filter excludes the link reports `no_path`; two runs give identical digests; one
   `advance` of several steps keeps the events of all of them; no step of a walk and crossing moves the agent further
   than its speed allows; a tile rebuild during a crossing lands it at the link's end; a link switched back on
   replans an agent in `no_path`.
7. Obstacles: an oriented box blocks the cells it covers and not the corners its AABB would; `move_obstacle` keeps the
   id and rebuilds the old and new tiles only; a move restores a blocked link.
8. Format: round trip, defaults, a newer version, a parse error and an invalid value are typed errors.
9. Agent protocol: each method round-trips on a test app; errors carry the reasons above.
10. Arcs (ADR-2589, `tests/link_arcs.rs`): a gap jump's sampled apex is within 1 cm of the analytic apex, every flight
    step lies on the parabola, and the agent lands on the far polygon. A drop falls along `y = start - g t² / 2` and is
    one way. The crossing time comes from the arc. Two runs give the same digests and events. The plugin writes one
    `NavLinkTraversal` message per take-off.
11. Streamed links (`tests/link_stream.rs`): a link is cooked only into its start tile's cell and loads and unloads
    with it. A link into an unloaded tile is skipped by the path query and used after the load. v1 cells load with no
    links. Bad link records are `nav_cell_corrupt`. An agent mid-link goes back when its landing tile unloads (event
    `cancelled`) and lands when its take-off tile unloads. Runs are deterministic.
12. Growth (`tests/grid_growth.rs`): +X growth keeps every tile id, bounds, polygon count and rebuild count. -X/-Z
    growth moves ids by the reported offset and keeps the tiles in place. Obstacles follow the new numbering. New
    ground gets navmesh after a region rebuild. Streamed caches, bad boxes and huge grids are refused.

## 7. Platforms

Pure CPU Rust with no platform code: identical on Linux, Windows and Android aarch64 (the Android check builds the crate
through `regia`). macOS builds it too.

## 8. Performance budget

- Link table resolve: O(links), at most once per mesh/link change; 0 per query on an unchanged mesh. Each query and
  crowd step still hashes the link list (O(links)) to catch in-place edits of the public field.
- A*: link edges cost O(links at this polygon) per expansion instead of O(all links).
- Generator: O(wall samples × probe steps); a load-time or editor operation, budget 50 ms for a 64 × 64 m two-level
  test map at 0.25 m cells in release (measured by the ignored `perf_micro` test, never asserted).
- Crowd: a traversing agent costs less than a walking one (no avoidance). The crowd step stays within the ADR-1484
  budget; `perf_micro` prints the step with 100 agents and 64 links.

## 9. Scope after v1

Built since v1 and now in scope:

- link reservation and queuing (ADR-2420);
- per-triangle area paint at cook (ADR-2420);
- a generator that tests the level's colliders (`generate_clear_links`, `generate_level_links`, ADR-2420);
- links authored as scene components (`NavLinkAnchor`) and runtime `.navlinks.ron` loading (ADR-2420);
- jump arcs for Jump and Drop links (§ 3.3, ADR-2589);
- links streamed per tile (§ 3.6, ADR-2589);
- tile-grid growth (§ 3.7, ADR-2589).
- navmesh raycast along portal edges, seeded random points (whole mesh and within a radius, `reachable_only`),
  partial paths to an unreachable goal (`NavQuery::find_path_partial`), and stuck detection with a recovery ladder in
  `NavCrowd` and `NavAgent` (re-path, nudge, report; off by default) (NM-1/NM-2, ADR-8380; agent methods
  `nav.raycast`, `nav.random_point`, `nav.crowd_set_stuck`, `nav.find_path` `partial`).

Still out of scope: editor placement tools for links and anchors, per-polygon flags on the walkable surface, and the
streaming cook's per-triangle area paint (`regia_asset_pipeline` still paints `NavAreaZone`s per cell). The crowd does
not model air control or collisions during a flight: the arc is fixed at take-off.
