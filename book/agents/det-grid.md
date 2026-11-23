# Deterministic navigation grid

`regia_nav::det::grid` is a navigation grid for lockstep simulations: every peer that builds it from the same ground,
settings and obstacles gets the same bits, on x86-64 and aarch64 alike (ADR-8012 A-4, ADR-8049). Costs, counts, labels
and obstacle discs are integers; the few floats come from `+ - * /` in a fixed order and `regia_detsim::math`. The
deterministic navigation is its own crate, `regia_nav_det`, with no ECS dependency (ADR-8877): a headless lockstep sim
depends on it directly, and `regia_nav` re-exports it as `regia_nav::det`, so both paths name the same types.
`cargo xtask det-scan` holds the crate to the lockstep rules (see [Lockstep math and the determinism
scan](lockstep-math.md)).

It is the base of the deterministic search, agents and navmesh that later land beside it in `regia_nav_det`.

## Pieces

| Item | What it is |
|---|---|
| `GridSettings` | `cell_m` and `canvas_cells` (the flat canvas when there is no ground), `move_classes`, `size_classes`, `profiles`. `GridSettings::hover_ground_jet()` (also `Default`): 5 m cells over 1280 m, three movement and three size classes, five profiles. `validate()` refuses bad settings. |
| `MoveClass { free, steep, cliff }` | Slope limits as rise over run. Cost is 10 (flat) up to `free`, climbs to 40 at `steep` and to 250 just under `cliff`; steeper is a cliff (cost 0, and a step up or down it is refused). Presets `HOVER`, `GROUND`, `JET`. |
| `SizeClass { inflate_m, max_radius_m }` | Obstacle inflation per size layer. `GridSettings::size_for_radius(r)` picks the first class whose bound holds `r`, else the last. Presets `SMALL`, `MEDIUM`, `LARGE`. |
| `Profile(u8)` | An index into `profiles` (a movement class plus a size class). Past the end means the last profile. Constants `HOVER`, `HOVER_LARGE`, `FOOT`, `WALKER`, `JET` name the preset's five. |
| `GridGround` | The ground a grid is built from (implement it for your heightfield). `HeightCells` is a plain implementation, with optional `GroundPatch`es that only lower the ground. |
| `NavStatic` | The terrain half, built once per map and shared by clones (`Arc`): per-class costs and 8-way step masks, slopes, heights, `lowered` cells, the `GroundKey` it was built from. |
| `NavGrid` | Terrain plus live state: per-size blocker counts, per-profile region labels, per-cluster `RegionSet`s (8 x 8 cell clusters), the host's closed cells and the exclusion discs. Cloned with the sim state. |
| `Stamp { x_cm, z_cm, r_cm }` | One obstacle disc in centimeters. `Stamp::from_world(x, z, r)`, `Stamp::covering_square(x, z, side)`. |

## Using it

```rust
use std::sync::Arc;
use regia_nav::det::grid::{ExclusionDisc, GridSettings, HeightCells, NavGrid, NavStatic, Profile, Stamp};

# fn main() -> Result<(), regia_nav::det::grid::GridError> {
let ground = HeightCells::centered(64, 64, 5.0, vec![0.0; 64 * 64]);
let settings = Arc::new(GridSettings::hover_ground_jet());
let stat = Arc::new(NavStatic::build(Some(&ground), settings)?);
let mut grid = NavGrid::new(stat)?;

// Every tick: the host turns whatever blocks (buildings, wrecks, parked vehicles, debris) into discs.
let sync = grid.sync_stamps(vec![
    (12, Stamp::from_world(20.0, -15.0, 6.0)),
    (40, Stamp::covering_square(-50.0, 30.0, 8.0)),
]);
// `dirty_clusters` lists the relabeled clusters: routes through them need a re-plan.
assert!(sync.changed && !sync.dirty_clusters.is_empty());

// A pit: ground below -25 m within 25 m of the center is closed to every profile.
grid.set_exclusions(&[ExclusionDisc { center_x: 0.0, center_z: 0.0, radius: 25.0, below_y: -25.0 }]);

let cell = grid.cell_at(20.0, -15.0);
assert!(!grid.passable(Profile::HOVER, cell));
let free = grid.nearest_passable(Profile::HOVER, cell, 8);
assert!(free.is_some());
# Ok(())
# }
```

- `sync_stamps` takes the whole obstacle set each time, ids unique, in any order. An unchanged set touches nothing
  (`StampSync::default()`); a changed one lifts gone or moved discs, stamps new ones on every size layer and relabels
  the touched clusters in ascending order.
- `NavGrid::with_closed_cells(stat, mask)` closes cells the host owns (for example the surface over a tunnel opening).
  The mask has one byte per cell, 1 = closed, or is empty.
- Queries: `passable`, `cost`, `terrain_blocked`, `can_step`, `label`, `cluster_of`, `cell_at`, `center`,
  `in_bounds_world`, `surface_y`, `nearest_passable`, `los` (supercover line of sight with a cost cap), and the free
  function `supercover`.
- Every peer of a match must use the same `GridSettings` and ground: they decide every cell.

## Areas and cost overlays

A cell's cost for a profile has three parts, all integers:

1. The terrain cost of the profile's movement class (10 flat, up to 250 just under a cliff, 0 for a cliff).
2. The cell's **area**. `GridSettings::areas` lists `AreaDef { name, cost_pct, blocked_for }`; `areas[k]` is area id
   `k + 1`, and id 0 is the implicit `"default"` area (100 %, blocks nobody). `NavGrid::paint_area(&region, id)` paints
   a `PaintRegion::CellRect { min, max }` (cell coordinates, inclusive, clamped) or a `PaintRegion::Polygon` (convex,
   vertices in cm, either winding; a cell is painted when its center is inside or on the boundary) and relabels the
   touched clusters. The scaled cost is `max(1, (terrain * cost_pct + 50) / 100)`: half rounds up, a passable cell never
   reaches 0, a cliff stays a cliff. A profile in `blocked_for` cannot stand in the area at all (`passable` is false and
   its label is `BLOCKED`).
3. The **cost overlay**: `CostOverlay { profiles: ProfileMask, cells: Vec<u16> }`, one `u16` per cell, added to the
   cost of the profiles in the mask. It never opens or closes a cell. `NavGrid::set_overlay(Some(Arc<CostOverlay>))`
   sets it (the same `Arc` again is a no-op; `None` clears it); a game rewrites it per tick or per cadence, for example
   a threat grid resampled onto the nav cells.

`NavGrid::cost(profile, cell)` returns the sum (0 when blocked); searches read every cell cost through it. Region
costs (the coarse search's) are the mean of the same cell costs, so a road or a threat moves the coarse corridor too;
`set_overlay` relabels every cluster for that reason (about the cost of `set_exclusions`). Painting and a new overlay
bump `NavGrid::generation()`, which route caches key on.

```rust
use std::sync::Arc;
use regia_nav::det::grid::{AreaDef, CostOverlay, GridSettings, NavGrid, NavStatic, PaintRegion, Profile, ProfileMask};

# fn main() -> Result<(), regia_nav::det::grid::GridError> {
let mut settings = GridSettings::three_class();
settings.areas.push(AreaDef { name: "road".into(), cost_pct: 50, blocked_for: ProfileMask::NONE });
settings.areas.push(AreaDef {
    name: "deep_water".into(),
    cost_pct: 100,
    blocked_for: ProfileMask::of(&[Profile::MID_CLIMB_SMALL]),
});
let stat = Arc::new(NavStatic::build(None, Arc::new(settings))?);
let mut grid = NavGrid::new(stat)?;
let road = grid.settings().area_id("road").unwrap_or(0);
grid.paint_area(&PaintRegion::CellRect { min: [0, 100], max: [255, 101] }, road)?;
assert_eq!(grid.cost(Profile::MID_CLIMB_SMALL, grid.index(10, 100)), 5);

// A danger zone for one profile, rewritten whenever the game's threat map changes.
let mut cells = vec![0u16; grid.width() * grid.height()];
cells[grid.index(20, 20) as usize] = 400;
let threat = CostOverlay { profiles: ProfileMask::of(&[Profile::MID_CLIMB_SMALL]), cells };
let before = grid.generation();
grid.set_overlay(Some(Arc::new(threat)))?;
assert!(grid.generation() > before);
assert_eq!(grid.cost(Profile::MID_CLIMB_SMALL, grid.index(20, 20)), 410);
# Ok(())
# }
```

With no areas, nothing painted (or everything painted back to 0) and no overlay, every cost, label, region and hash is
the terrain-only one bit for bit (`default_areas_and_no_overlay_keep_the_pins`). `det::search` scales its cell
heuristic by the cheapest area itself (KI-WD-031, ADR-10070): with `GridSettings::min_area_pct()` under 95 % the
heuristic is multiplied by the cheapest diagonal step over 14, so a road painted at 50 % is used when it is cheaper.
Grids without such an area plan bit for bit as before. A host that writes its own search over the grid still scales its
heuristic by `min_area_pct()`.

## Stamp shapes

Discs (`Stamp`) stay the per-tick obstacle of units and wrecks. Buildings of any shape go through
`NavGrid::sync_shapes_from(&mut [(u32, StampShape)])`, a second obstacle set with its own id space:

| `StampShape` | Shape |
|---|---|
| `Disc(Stamp)` | The disc, rasterized exactly like `sync_stamps` |
| `Square { x_cm, z_cm, half_cm }` | An axis-aligned square (the square itself, not `Stamp::covering_square`'s disc) |
| `OrientedBox { x_cm, z_cm, half_x_cm, half_z_cm, cos_sin_q }` | A turned box; `(cos, sin)` in units of `Q14_ONE` (16384). `StampShape::oriented_box(x, z, half_x, half_z, dir_x, dir_z)` builds one from meters and a direction |
| `ConvexPolygon(Vec<[i32; 2]>)` | Vertices in cm, either winding (`StampShape::polygon_from_world`) |

Every shape but the disc is tested with integers in centimeters: a cell is covered on a size layer when its center
(`NavGrid::cell_center_cm`: `round(origin * 100) + index * round(cell_m * 100)`) lies strictly inside the shape, or
strictly closer than the size class's inflation (rounded to cm) to its boundary. That is the disc rule (inside the
shape grown by the inflation). A box's corners are `center + round(R * (+-half_x, +-half_z) / 16384)`, half away from
zero; a polygon with fewer than 3 vertices or no area covers only the inflation band around its points. Coordinates
are clamped to `SHAPE_LIMIT_CM` (2^30 cm) so every product fits an `i128`.

## Per-tick sync without allocation

`NavGrid::sync_stamps_from(&mut [(u32, Stamp)]) -> bool` is `sync_stamps` without allocating once warm: it sorts the
caller's buffer in place (the buffer keeps the new set), copies it into the grid's own buffer, reuses its cluster
scratch and the regions' buffers, and leaves the relabeled clusters in `NavGrid::dirty_clusters()`. A host clears and
refills one `Vec` per tick. `sync_stamps(Vec)` stays as a wrapper with the same bits and the same `StampSync`.
`sync_shapes_from` reuses polygon buffers that keep their slot. `sync_from_allocates_nothing_after_warmup` drives 900
moving discs and 13 shapes through one warm 16-tick cycle under a counting allocator and counts 0 allocations.

## Names

The movement classes, profiles and preset have neutral names; the source game's names stay as aliases with the same
bits (searchable in the docs through `#[doc(alias)]`):

| Neutral | Alias |
|---|---|
| `MoveClass::CLIMB_LOW`, `CLIMB_MID`, `CLIMB_HIGH` | `MoveClass::HOVER`, `GROUND`, `JET` |
| `Profile::LOW_CLIMB_MEDIUM`, `LOW_CLIMB_LARGE`, `MID_CLIMB_SMALL`, `MID_CLIMB_MEDIUM`, `HIGH_CLIMB_SMALL` | `Profile::HOVER`, `HOVER_LARGE`, `FOOT`, `WALKER`, `JET` |
| `GridSettings::three_class()` (also `Default`) | `GridSettings::hover_ground_jet()` |

## Errors

`GridError`: `BadCellSize`, `CanvasTooSmall`, `EmptyList` (`move_classes`, `size_classes` or `profiles`),
`TooManyProfiles` (over 255), `BadMoveClass` (limits not `0 <= free < steep < cliff`), `BadSizeClass`,
`ProfileClassMissing`, `GroundTooSmall` (under 2 x 2 samples or a non-positive spacing), `TooManyCells` (over `u32`),
`ClosedMaskLength`, `BadAreaCost` (an area's `cost_pct` is 0), `DuplicateAreaName` (two areas share a name, or one is
named `"default"`), `TooManyAreas` (over 255), `AreaMissing` (`paint_area` with an id the settings do not define),
`OverlayLength` (an overlay whose length is not the cell count), `BadPolygon` (a paint polygon with under 3 vertices or
no area). A host that reports them to an agent uses the lowercase snake-case variant name as the error code
(`bad_area_cost`, `duplicate_area_name`, `too_many_areas`, `area_missing`, `overlay_length`, `bad_polygon`, and so on).

## Hashes and the agent surface

`NavStatic::bits_hash()` and `NavGrid::state_hash()` are FNV-1a 64 over the grid's bits (NaNs and signed zeros folded), for desync
checks. `NavGrid::summary()` returns a serializable `GridSummary`: size, cell size, clusters, stamp, shape, painted,
closed, excluded and lowered cell counts, whether an overlay is set, passable cells and regions per profile, and both
hashes as hex. Shapes, painted areas and an overlay enter `state_hash` only when present.

There is no protocol method: the grid lives in the host's sim, not in an engine resource. A host that wants an agent
to see it serializes `summary()` from its own method. Checks:

```text
cargo test -p regia_nav_det --lib
cargo test -p regia_nav --test det_grid_sync_alloc
cargo xtask det-scan --json        # "crates" lists crates/regia_nav_det
```

`grid_bits_match_the_pinned_hashes` pins both hashes over a fixed scenario. The same values must come out on every
architecture; a change there is a sim-version change.

## Coming from Direct Order 27's grid

The initial grid port retained Direct Order 27's computations (ADR-8012). This revision applies the canonical float
rules below, so hosts must account for the zero/NaN hash changes when switching. The map: the movement, size and profile enums became `GridSettings` (use `hover_ground_jet()`); `TerrainKey` is
`GroundKey`; the heightfield type implements `GridGround` (its patch hash through `patch_signature`); `funnel` is
`lowered`; the kill volume is an `ExclusionDisc` with `below_y = kill_y + margin`; the tunnel cut is the closed mask;
the obstacle sync is `sync_stamps`. The tunnel layer is `det::layer` (below); crawlways are not part of this module.
`hover_ground_jet()` and the `HOVER`/`GROUND`/`JET` and profile names stay (aliases of `three_class()` and the neutral names, same bits); a switching sim may keep them. A per-tick host moves to `sync_stamps_from` with one reused buffer (same bits, no allocation); areas, overlays and shapes are new and off by default.

## Planning routes: `det::search`

`regia_nav::det::search` is a two-level A* on the grid (ADR-8195). A coarse search over the regions of 8 x 8 cell
clusters finds the long haul; a cell search inside the corridor of clusters it crosses (dilated by one cluster, then
three if that fails) finds the cells; string pulling turns them into waypoints. Every search finishes inside the call
that starts it, and its work is capped: `fine_expansions_for(width, height)` cell expansions (12 000 up to a 256 x 256
grid, more on larger ones), then the plan falls back to straight legs between region centers. Ties break on
`(f, h, index)`, so every peer pops the same node. Costs are integers (tenths of a cell).

| Item | What it is |
|---|---|
| `plan(grid, scratch, profile, from, goal)` | A surface route between two world XZ points. `None` when no free cell lies within `ESCAPE_RINGS` of the start or `SNAP_RINGS` of the goal. |
| `plan_layered(grid, layer, scratch, profile, from, from_tunnel, goal, goal_tunnel, tunnel_pct)` | The same over the surface and a tunnel layer. `from_tunnel`/`goal_tunnel` name the node a body underground stands on (`TunnelLayer::locate`); `tunnel_pct` is a tunnel meter's share of an open one (`TUNNEL_COST_PCT` 75, `FLANK_COST_PCT` 20; clamped to 10-100). |
| `Plan` | `waypoints` (the first is `from`), `kind` (`Full`, `Partial` when the goal is unreachable and the plan ends at the nearest reachable point, `Coarse` when the cell search ran out), `snapped` (the goal was in a blocked cell), `layers` (per waypoint: underground), `cells` (the node path), and the work spent. |
| `Scratch` | Search buffers a host keeps per sim and reuses. Not sim state: a clone starts empty and plans the same routes. |
| `PATH_TICK_BUDGET`, `path_tick_budget_for` | The per-tick expansion budget a host's request queue spends (6000 up to 256 x 256). |

## The tunnel layer: `det::layer`

`TunnelLayer::build(&map_tunnels, &nav_static)` turns a map's tunnel description (`regia_world_geom::compat::state::
MapTunnels`: the layout and its sampled centerlines) into a graph: one node per centerline sample, passage ends joined
to the nearest node of another passage, and a portal at the surface cell just past each ramp top. It also returns a
closed-cell mask: the surface over each ramp's open cut and its shoulders, which the host hands to
`NavGrid::with_closed_cells` so surface routes keep out of the trench. Portal cells stay open.

```rust
use std::sync::Arc;
use regia_nav::det::grid::{GridSettings, NavGrid, NavStatic, Profile};
use regia_nav::det::layer::{layer_for, TunnelLayer, PRESET_TUNNEL_SIZE_LIMIT, TUNNEL_COST_PCT};
use regia_nav::det::search::{plan_layered, Scratch};
use regia_world_geom::compat::state::MapTunnels;

fn route(map: &MapTunnels, stat: Arc<NavStatic>) -> Option<bool> {
    let (tunnels, cut) = TunnelLayer::build(map, &stat);
    let grid = NavGrid::with_closed_cells(stat, cut).ok()?;
    let layer = layer_for(&grid, Some(&tunnels), Profile::HOVER, PRESET_TUNNEL_SIZE_LIMIT);
    let mut scratch = Scratch::default();
    let plan = plan_layered(&grid, layer, &mut scratch, Profile::HOVER, [-250.0, 0.0], None, [250.0, 0.0], None, TUNNEL_COST_PCT)?;
    // Collapse rubble: close every node of a passage; plans route round it until the list is cleared.
    tunnels.set_closed(vec![3, 4, 5]);
    Some(plan.layers.iter().any(|under| *under))
}
# fn main() {}
```

- `layer_for(grid, layer, profile, size_limit)` gives the layer to profiles whose size class is at most `size_limit`
  (`PRESET_TUNNEL_SIZE_LIMIT` = 1 in the preset: the large class goes round).
- `set_closed(ids)` replaces the closed node set (sorted, deduped); the search never enters, leaves or passes a closed
  node. It works through `&self` (a `Mutex` inside) so a layer shared by grid clones can be closed in place; every peer
  must apply the same list on the same tick.
- `locate(pos, surface_y)` finds the node a body underground (or on a ramp floor) stands on; `set_levels` tags nodes
  with mesh-layer levels for multi-level bakes.

The search and the layer are copied from Direct Order 27 with their arithmetic unchanged (ADR-8012, ADR-8195). Checks:

```text
cargo test -p regia_nav_det --lib search
REGIA_BLESS=1 cargo test -p regia_nav --lib search_plans_pinned_on_every_arch   # record this target's golden line
```

`search_plans_pinned_on_every_arch` runs 256 seeded plans (the grid fixtures, a tunnel world, and a maze that runs the cell search past its cap) and checks them against
`crates/regia_nav_det/goldens/search_plans.detgold` ([Determinism goldens](det-golden.md)); it prints one
`regia-det-golden {json}` line. A change there moves a lockstep sim's routes: a sim-version change.

## Agents: `det::agent`

`regia_nav::det::agent` keeps one agent per steered mover on top of the grid and the search (ADR-8405): it follows the
route with a look-ahead point that slows into corners, skips ahead when a later waypoint comes into sight, re-plans
when the goal moves, when a stamp changes a cluster on the route, when the mover leaves the route, or after a stuck
event, and recovers wedges. Requests wait in a FIFO queue and are solved under the per-tick expansion budget, oldest
first. Local avoidance is contact separation plus the integer velocity solver ([Integer local avoidance](det-orca.md)).

The host supplies its world through two traits, and moves its bodies through a third:

| Item | What it is |
|---|---|
| `DetAgentWorld` | Reads: `ids()` (live bodies, ascending), `body(id) -> Option<DetBody>` (position, radius, `profile` or `None` for a body that never moves, team, velocity, heading, cruise speed, `soft` for people on foot that vehicles neither avoid nor queue behind), `obstacle(id) -> Option<Stamp>` (the disc it stamps on the grid), `ground_y(x, z)` (optional, for a landed slide). |
| `DetAgentWorldMut` | The writes: `set_position`, `set_yaw`. |
| `DetMover` | `move_kinematic(id, goal, park) -> bool`: drive one mover one tick, true once parked. `KinematicMover::new(&mut agents, &mut world, tick)` is the default (a ramped cruise along the steer point, nudged by avoidance, backing off when stuck, never stepping deeper into a kill volume). A host with its own physics calls `DetAgents::steer` and drives there, then reports `note_moved(id, heading, speed, tick)`. |
| `DetAgents` | The layer: `service(tick, &world)` once per tick before the movers, `steer(id, goal, park, tick, &world) -> NavSteer`, `commit_escapes(tick, &mut world)` after the host's collisions (the bounded wedge slide, at most 3 m a tick), `prepare(&world)` to build the grid at map load, `route_reaches(profile, from, goal)`, `line_clear`, `ridge_hide`, `debug_paths`, `report`. `set_ground` takes the `GridGround`, `set_kill_volume` a `DetKillVolume`. |
| `DetAgentSettings` | The source's caps as settings with its values as defaults: `stuck_ticks` 150, `stuck_move_m` 2.5, `repath_min_ticks` 30, `agent_idle_ticks` 180, `give_up_stuck` 4, `escape_nudge_ticks` 90, `escape_still_ticks` 180, `escape_depart_ticks` 45, `direct_m` 60, `near_goal_m` 10, `off_path_m` 20, `pit_exit_m` 10, `separation_gap_m` 2, `tick_dt` 1/60, `tick_budget` (default: the grid's scaled budget). Every peer must use the same values. |
| `NavSteer` | The steer point, distance left, `arrived`, `speed_scale`, the avoidance push, `backoff` and its side, `routed`, `escape`, and the next corner's distance and turn. |
| `PathStats` | Counters only (requests, solved, stuck events, give-ups, slides, re-plans by cause, ORCA calls, the first 2000 stuck events). The sim never reads them; there is no wall-clock field. |

A tick, in order: `service`, every mover's `move_kinematic` (ascending id), the host's collisions, `commit_escapes`.
The layer moves a body only in `commit_escapes`; everything else goes through the host's mover.

```rust
use regia_nav::det::agent::{DetAgentWorldMut, DetAgents, DetMover, KinematicMover};

fn tick<W: DetAgentWorldMut>(agents: &mut DetAgents, world: &mut W, tick: u64, orders: &[(u32, [f32; 3])]) {
    agents.service(tick, &*world);
    {
        let mut mover = KinematicMover::new(agents, world, tick);
        for (id, goal) in orders {
            mover.move_kinematic(*id, *goal, 8.0);
        }
    }
    // ... the host's collisions ...
    agents.commit_escapes(tick, world);
}
```

### The agent surface: `nav.det_agents`

The agents are host sim state, so the host publishes a report once per tick into the
`regia_nav::det_view::DetAgentsView` resource (`view.publish(tick, &agents)`). `nav.det_agents` (no params, read
permission) returns the last one:

```json
{ "tick": 3, "agents": 2, "queue": 0, "disabled": false,
  "grid": { "width": 256, "height": 256, "cell_m": 5.0 }, "grid_error": null,
  "stats": { "requests": 2, "solved": 2, "stuck_events": 0, "gave_up": 0, "escape_attempts": 0, "stuck_log_len": 0, "...": 0 },
  "routes": [ { "id": 1, "waypoints": [[-150.0, 0.0], [-40.0, 45.0], [150.0, 0.0]], "next": 1 } ],
  "crowd": { "groups": 0, "chokes": 0, "lane_groups": 0, "flows": [] } }
```

`routes` lists every agent with a route that has not arrived, ascending id; `next` is the next waypoint's index. The
stuck log is reported by its length (`stuck_log_len`). Error: `nav_no_det_agents` (code `F0003`, not found) when the
app has no `DetAgentsView` or the host has not published yet.

Checks:

```text
cargo test -p regia_nav_det --lib agent
cargo test -p regia_agent --lib nav_det_agents
```

`agents_follow_through_a_custom_mover` drives a mover through a recording `DetMover`, and a mover that only steers
leaves every body in place until the wedge slide. The source's agent proofs (round a cliff and a building, the
choke, ten hulls through a gap, head-on avoidance, a structure on the route, two runs give the same hash, a cloned
state, the tick budget, turned-off pathing, large-hull clearance, group slots, the pit) run with their assertions
unchanged; `the_hash_run_is_pinned_on_every_arch` pins the hash run's position hash and counters on every target.

### Crowd lanes: `det::crowd`

`regia_nav::det::crowd` keeps one flow per order group on `DetAgents` (ADR-8448).
`DetAgents::crowd_refresh(group, members, anchor, tick)` takes living ids with world XZ and the order
point. Slots face from the group's centroid toward that point. Near a registered choke (within 60 m of
the leg) the slots narrow to three files, 8 m apart, trailing the anchor. `DetAgents::crowd_slot`
returns a member's world XZ slot, and `DetAgents::crowd_bent_slot` slides a slot back along a blocked
leg until the grid has line of travel. `DetAgents::register_choke` records a narrow; points within a
metre are the same choke. `DetAgents::service` drops a flow that nobody refreshed for 180 ticks. Caps
(`CrowdSettings`) default to 64 groups, 16 members, 3 lanes, an 11 m slot gap and those choke and idle
values. Every peer uses the same settings.

Tunnel mouths and junctions come from the map's tunnel layer: after `TunnelLayer::build`, call `DetAgents::register_tunnel_chokes(&layer)` (KI-WD-030, ADR-10070). It registers each portal's ramp top, then each junction node, and returns how many chokes it added (a second call adds none). `clear_chokes` forgets them on a map change. `nav.det_agents` counts them in `crowd.chokes`.

`nav.det_agents` includes `crowd`: `groups`, `chokes`, `lane_groups`, and `flows` (each `group`, `members`, `lane_mode`). The same error `nav_no_det_agents` covers a missing report. No new method.

```text
cargo test -p regia_nav_det --lib crowd
cargo test -p regia_agent --lib nav_det_agents
```

The five source proofs (slots face the march, lanes file a choke, idle flows evaporate, bent slots stay west of a cliff, a group reaches its point without wedging) run with their assertions unchanged. `crowd_hash_is_stable_over_two_runs` steps 16 members and one choke for 600 ticks twice and requires the same position hash.

### Coming from Direct Order 27's agents

Implement `DetAgentWorld`/`DetAgentWorldMut` on the game state (an entity's class and health decide `profile` and
liveness; `obstacle()` returns the building, wreck, parked-hull and scrap-mound stamps), keep `Pathing` as a
`DetAgents`, and call `service`, the movers and `commit_escapes` where `nav_service`, the orders and
`nav_commit_escapes` ran. `nav_move_kinematic` becomes `KinematicMover`; the hover drive keeps its own `DetMover`.
Not here: the tunnel and crawlway arms, the navmesh arm, hover drive requests and jet steering. The crowd layer is `det::crowd` above; tunnel mouths stay on `register_choke`.

## Large maps: cluster size

The coarse search plans over regions of square clusters. `GridSettings::cluster_cells` sets the cluster side in cells
(ADR-8295). The default is 8: 40 m clusters on 5 m cells, at most 64 regions per cluster, one-byte labels, and
DO27's bits. A 256 x 256 map keeps the default.

| `cluster_cells` | Labels | Regions per cluster | Use |
|---|---|---|---|
| 2, 4, 8 (default) | one byte (`LabelWidth::U8`, `NavGrid::labels`) | up to 64 | maps up to about 1,000 cells a side |
| 16, 32, 64, 128 | two bytes (`LabelWidth::U16`, `NavGrid::wide_labels`) | up to 16 384 | 4096 x 4096 maps and up |

- The value must be a power of two from 2 to 128 (`GridError::BadClusterSize`). The label width is picked when the
  grid is built: `GridSettings::label_width()` is one byte while `cluster_cells^2` fits below the two reserved
  values, two bytes above. Settings saved before the field existed load as 8.
- `NavGrid::cluster_cells()`, `max_regions()` (`cluster_cells^2`), `label_width()` and `region_label(profile, cell)`
  (`NO_REGION` when blocked) work at both widths. `label()` returns a byte; on a two-byte grid it saturates at 254, so
  read `region_label` there.
- Larger clusters mean fewer coarse nodes and a wider corridor for the cell search. On a 4096 x 4096 canvas with long
  walls and 4,000 discs, a full plan's cell path at 16 and 32 costs the flat A* optimum within 5 % (one in 20), and
  exactly the optimum on open ground. String pulling never makes a route longer than its cell path. A plan whose cell
  search runs out of expansions falls back to region centers (`PlanKind::Coarse`), as at the default size.
- `NavGrid::generation()` goes up on every relabel and stamp, and `cluster_generation(c)` records the last relabel of
  each cluster. Neither is part of `state_hash()`, so the pins do not move.

The coarse graph as a table: `regia_nav::det::region_graph::RegionGraph` holds, per profile and cluster, the regions
next to each region in the order the search reads them. A search that reads the table pops the same nodes as one
that scans the cluster borders.

| Item | What it does |
|---|---|
| `RegionGraph::build(&grid)` | The full table. |
| `repair(&grid, &sync.dirty_clusters)` | Recomputes the clusters `sync_stamps` relabeled and their four neighbors. Returns the count. |
| `sync(&grid)` | Finds the moved clusters from `cluster_generation`, whatever relabeled them (`sync_stamps`, `set_exclusions`, a host's `relabel_cluster`). A grid of another shape rebuilds the table. |
| `fits`, `cluster(profile, c)`, `edge_count`, `generation` | Shape check, one cluster's lists (`ClusterEdges { offsets, targets }`), counters. |

`dirty_cluster_repair_equals_a_full_rebuild` runs 600 ticks of seeded obstacle churn at 8, 16 and 32 and checks
both repairs against a fresh build every tick.

## Path requests and budgets

`regia_nav::det::requests::PathRequests` is a deterministic request queue (ADR-8295). Units ask; the queue serves in
`(submit tick, unit id)` order under a per-tick work budget and carries the rest to later ticks.

```rust
use std::sync::Arc;
use regia_nav::det::grid::{GridSettings, NavGrid, NavStatic, Profile};
use regia_nav::det::requests::{PathRequests, Poll, RequestSettings};
use regia_nav::det::search::Scratch;

fn tick(grid: &NavGrid, queue: &mut PathRequests, scratch: &mut Scratch) {
    // During the tick: units ask (any order; the queue sorts by unit id).
    let ticket = queue.request(42, [-100.0, 0.0], [300.0, 80.0], Profile::HOVER);
    // After the tick's `sync_stamps`: spend the budget.
    queue.serve(grid, scratch);
    if let Poll::Ready(result) = queue.poll(ticket) {
        let _route = result.plan; // `kind()`: Full, Partial, Coarse, or None without a plan
    }
}
# fn main() {}
```

| Item | What it does |
|---|---|
| `RequestSettings { tick_budget, share_routes }` | `tick_budget` is in `Plan::work` units (S-02's: cell expansions plus a share for the coarse search). `None` serves every request at once. `for_grid` uses `path_tick_budget_for(width, height)` (6000 up to 256 x 256). |
| `request(unit_id, from, to, profile) -> Ticket` | Queues a surface route at the queue's current `tick()`. |
| `serve(&grid, &mut scratch)` | Syncs the queue's `RegionGraph` to the grid, then serves while budget is left. The last request of a tick may overrun the budget; a positive budget always serves one. Then the tick advances. |
| `poll(ticket)` | `Pending { submit_tick }`, `Ready(PathResult)` (handed over once) or `Unknown`. `drain_ready()` takes every result in ticket order. |
| `cancel(ticket)` | Drops a queued request or an unread result. |
| `PathResult` | `plan` (`None` when no free cell is near the start or goal), `kind()`, `submit_tick`, `served_tick`, `waited()`, `seq` (serve order), `shared_route`, `work()` (`NO_PLAN_WORK` = 16 without a plan). |
| `wait_bound(work_ahead)` | The longest wait in ticks: `floor(work_ahead / budget)`. In every tick a request is not served, the requests ahead of it spend the whole budget. |
| `counters()`, `last_tick()` | `RequestCounters`: `requests`, `served`, `carried` (left queued after a serve, summed), `cancelled`, `cache_hits`, `expansions`, `coarse_expansions`, `work`, `graph_repairs`. Serializable. |
| `state_hash()` | FNV-1a 64 over the pending requests and unread results, for desync checks. |

**Shared routes.** Requests with the same start region, goal region, profile and grid generation use one coarse
route, in the same tick or a later one. The first request in serve order searches; the rest count as `cache_hits`
and spend no coarse work. A new grid generation (any relabel or stamp) empties the cache, and so does reaching
`MAX_CACHED_ROUTES` (4096). The cache is part of the queue's sim state (`state_hash` covers its keys). A served request that
does not share is bit for bit the direct `search::plan` call. A shared route can differ from what a lone request
would get only when two region routes tie in cost, or when the goal region cannot be reached and the route ends at
the region nearest the first requester's goal. Every peer serves in the same order, so every peer shares the same way.

**Surface only.** The queue plans on the surface grid. Tunnel plans call `plan_layered` directly.

**Agent surface.** There is no protocol method: the queue is host-owned sim state, like the grid. A host shows an
agent `counters()` (it serializes) from its own method. The det AI and nav agent surface (planned row ai-nav-05) is
where queue counters get a method; until then they are owed. Checks:

```text
cargo test -p regia_nav_det --lib requests region_graph
cargo test -p regia_nav --test det_requests                       # the 4096 x 4096 cost check
cargo test --release -p regia_nav --test det_requests -- --ignored --nocapture   # perf probe, prints numbers
```

`request_queue_pinned_on_every_arch` pins a seeded queue run (moving obstacles, a request stream, shared routes) by
hash. It must give the same value on x86-64 and aarch64.

## Canonical float adoption (review R1)

Grid hashes, ground probes and patch signatures encode every NaN as `0x7fc00000`
and either zero as `+0`. Patch minima and sampler clamps use
`regia_detsim::float::det_min` and `det_clamp`; their zero result is canonical.
Hosts adopting this revision rebuild derived grid caches and keep all peers on the
same sim revision. Existing finite nonzero encoder bits stay unchanged.

The existing `summary()` surface exposes the corrected static/state hashes; no new
protocol method is needed. Run `cargo test -p regia_nav_det --lib` to check patch
selection, cache keys, serialized summaries and the retained grid pins.

## Flow fields

`regia_nav::det::flow` builds one field toward a goal set that any number of movers read (ADR-8294). Use it when
many units go to the same place (a group move order, a rally point, a crowd leaving a stadium): one search replaces
one route per unit. A unit with its own goal still plans with `det::search`.

| Item | What it is |
|---|---|
| `FlowField::build(grid, profile, goals, settings)` | The integration field of `goals` (cell indices) over the whole grid: Dijkstra over the 8 neighbors, each move costing the cell search's step (10 straight, 14 diagonal, times the entered cell's cost multiplier over `COST_FLAT`), no corner cutting (`can_move`), ties on `(cost, cell index)`. Goals off the grid or in a blocked cell are dropped. |
| `FlowField::integration(cell)` | Integer cost to the nearest goal; `None` when no goal is reachable. |
| `FlowField::dir(cell)`, `next_cell(cell)` | The cell's move: `FlowDir::Step(k)` (an index into `NEIGHBOURS_8`), `Goal` or `Unreachable`. The move is the legal neighbor with the lowest `(value + step, index)`, so following it arrives with exactly the cell's integration value. |
| `FlowField::direction_at(world_xz)` | A steering vector: the nearest cell's unit move, `[0, 0]` on a goal, toward the goal itself in a line-of-sight cell, `None` off the grid, on a blocked cell or where nothing is reachable. |
| `FlowSettings` | `los_cells` (0 = off): cells within that many cells of a single goal with a clear grid line of sight (`NavGrid::los`, crossing no cell costlier than `los_max_cost`, default `COST_FLAT`) get `FLAG_LOS`; `blend`: bilinear sampling of the four cells around the point (off by default; only `+ - * /`, the result is not normalized). |
| `ClusterFlow::build(grid, profile, goals, capacity)` | For large maps. An abstract route over the grid's cluster regions (the same regions `det::search` uses: `cluster_cells` a side, 8 by default, either label width; ADR-8839), edges for every legal move between two regions, the coarse search's edge cost), then a local field per region built on first use: one cluster's region, seeded from the next region's field, so values chain to the goal. `integration`, `dir`, `next_cell`, `direction_at` as on a field (nearest cell only, no line-of-sight flag). |
| `ClusterFlow::on_sync(grid, &sync)` | After `NavGrid::sync_stamps`: rebuilds the abstract route, drops the local fields of each dirty cluster and its 8 neighbors, and checks every other field on its next use (same next region and seed values: kept, else rebuilt). |
| `FlowCache` | Full fields keyed by `FlowKey { goal_hash, profile, generation }` (the generation steps on each changed sync; full fields are then rebuilt whole) and clustered flows keyed by goal set and profile (kept across syncs, rebuilding only what the change touched). Least recently used out at `FlowCacheSettings { capacity, cluster_capacity, local_capacity }`. Entries sit in sorted vectors and a `BTreeMap`: no hash-container iteration. `stats()` / `cluster_stats()` count hits, misses, builds and evictions. |
| `FlowField::arrows(rect, cap)`, `FlowField::view(rect, cap)` | Debug arrows (`FlowArrow { from, to, los, goal }`) for a host's debug draw, and a capped raw window (`FlowView { rect, dirs, integration, truncated }`, serializable) for tools and agents. |

Memory: a field stores 5 bytes per cell of its window (`BYTES_PER_CELL`: a `u32` integration value, a `u8`
direction): 320 KiB for 256 x 256, 80 MiB for a full 4096 x 4096 field. A clustered flow stores 12 bytes per region
for its route and 5 bytes per cell of each cached local field (320 bytes at the default 8 x 8 cluster); `memory_bytes()`
reports both.

Direction byte: the low four bits are the move (0-7, `NEIGHBOURS_8` order), `DIR_GOAL` (8) or `DIR_NONE` (9);
`FLAG_LOS` (0x10) marks a line-of-sight cell. `UNREACHED` (`u32::MAX`) is the integration value of an unreachable
cell.

Clustered flow cost: a clustered flow only moves through the abstract route's chain of regions, so its arrival cost can
be above the full field's. The acceptance test holds every cell to `full + full / 8 + 2 * 14 * CLUSTER` (`CLUSTER` = 8, the default cluster size) on a
1024 x 1024 canvas with 1 860 obstacle discs; measured there, the mean excess over 200 sampled cells is 0.36 % and 92 of
them are exact. On open ground (a 200 x 136 canvas, three goals) more than half the cells are exact and none is more than
`2 * 14 * CLUSTER` above the full field (`cluster_flow_stays_close_on_open_ground`; measured: see the lane report).
Reachability is the same as the full field's. After a stamp in one corner of a 256 x 256 map, 16 of 1 024 local
fields were rebuilt and the other 1 008 kept (`dirty_clusters_rebuild_only_their_fields`).

Measured (release, the Windows min-spec laptop, `tests/det_flow_perf.rs`): a 256 x 256 full field builds in 17 ms; 900
movers sample it in 135 us per tick (172 us blended). On 4096 x 4096 the clustered flow's route builds in 1.7 s (a full field
takes 22 s and 80 MiB); a 900-unit group order builds 983 local fields in 25 ms on its first tick and samples in 313 us per tick
after that. A cached local field set must hold the regions units stand in plus their routes: 900 movers spread over the whole
4096 x 4096 map need about 77 000 (70 MiB), so size `local_capacity` for the spread you expect.

Choosing: below about 512 x 512 cells, or when the goal set changes rarely, a full field is simple and exact. On larger
maps, when goals change often, or when units fill only part of the map, the clustered flow builds only the clusters
units stand in (plus the chain to the goal) and survives obstacle changes far from the route.

**Cluster size (ADR-8839).** A clustered flow follows `GridSettings::cluster_cells`: its local fields are one cluster
of the grid's size and it reads `region_label`, so it works at every size from 2 to 128 and at both label widths.
`RegionKey` is `(cluster, u16 label)`. At the default 8 the bits are unchanged (`flow_bits_match_the_pinned_hash`).
`cluster_flow_follows_the_grid_cluster_size` checks sizes 2, 4, 8, 16 and 32 on a 256 x 256 canvas with 400 discs:
reachability equal to the full field's at every cell, every value within `full + full / 8 + 2 * 14 * cluster_cells`, and
walks from sampled cells that arrive with exactly the reported cost. Larger clusters mean fewer, larger local fields
(a 32-cell field is 5 KiB).

### Debug arrows

`FlowField::debug_emit(sink, rect, cap, y)` draws a field through the deterministic debug sink
(`regia_detsim::debug::DebugSink`, the one the indexes use) as `DebugKind::Flow` primitives: a circle of 0.35 cells on
a goal cell, and for every other reachable cell a shaft 0.8 cells long from the cell center toward `FlowArrow::to` (the
next cell, or the goal in a line-of-sight cell) plus a two-line head. It reads the field only and allocates nothing.
`FlowDebug { field, rect, cap, y }` (`FlowDebug::new(&field)`: the whole field, 4096 arrows, height 0) is a
`DebugSource` that draws when the switch holds `DebugWhat::FLOW`, so a game hands it to `DetDebugDraw::record` next to
its indexes, and a sim host calls it from `FixedStepSim::det_debug`. `ClusterFlow::debug_emit_cached` draws the local
fields a clustered flow has cached. The overlay colors flow arrows with `DetDebugColors::flow`, and
`sim.det_debug_draw { "what": ["flow"] }` switches them on and counts them under `kinds.flow`
([Deterministic IDs and broadphase](det-ids-broadphase.md)).

### The agent surface: `sim.grid_flow`

The grid is host sim state, so the host publishes it into the `regia_nav::det_view::DetFlowView` resource after each
tick's `sync_stamps`: `view.publish(tick, &grid)` copies the grid only when its generation or size moved (a host that
swaps in another grid of the same size and generation calls `publish_shared(tick, Arc<NavGrid>)` or `clear()`).
`view.settings` takes the host's `FlowSettings`. The method then builds the field an agent asks for: a full
`FlowField` up to `FLOW_FULL_MAX_CELLS` (1024 x 1024) grid cells, a `ClusterFlow` above (`view.full_max_cells` moves
the line). The last field is kept, so further windows on the same goal, profile, settings and grid generation run no
search (`cached: true`). The fields are the view's own; nothing feeds back into the sim.

| Method | Tool | Permission | Mutating | Params | Errors |
|---|---|---|---|---|---|
| `sim.grid_flow` | `sim_grid_flow` | read | no | `goal` (required), `profile`, `window`, `max_cells` | `no_det_flow`, `grid_flow_bad_goal`, `grid_flow_bad_profile`, `grid_flow_bad_window`, `grid_flow_goal_blocked` |

- `goal`: a world point `[x, z]` in meters, or a list of 1 to 64 points (each cell goes to its nearest). Goal cells
  blocked for the profile are dropped.
- `profile`: a profile index (`GridSettings::profiles`), default 0.
- `window`: `{ "x0", "z0", "x1", "z1" }` in cells, half open, cut to the grid. Default: a 32 x 32 square around the
  first goal.
- `max_cells`: most cells returned, 1 to 4096 (default 4096). A larger window is cut row by row, then column by column,
  and `truncated` says so.

```json
sim.grid_flow { "goal": [100.0, 50.0], "profile": 0, "window": { "x0": 40, "z0": 40, "x1": 42, "z1": 41 } }
→ { "tick": 5, "generation": 266, "kind": "full", "profile": 0, "goals": [[84, 74]],
    "grid": { "width": 128, "height": 128, "cell_m": 5.0 },
    "window": { "x0": 40, "z0": 40, "x1": 42, "z1": 41 }, "truncated": false,
    "dirs": [0, 0], "integration": [576, 566], "reached": 2, "dir_mask": 15,
    "cached": true, "memory_bytes": 82060,
    "stats": { "expansions": 16275, "reached": 16275, "los_cells": 0 }, "bits_hash": "8adaa1f0f81e115e",
    "cluster_stats": null }
```

`dirs` and `integration` are row-major over `window`. A direction byte's low four bits (`dir_mask`) are the move (0-7,
`NEIGHBOURS_8` order: +x, -x, +z, -z, then the diagonals +x+z, -x+z, +x-z, -x-z), 8 on a goal and 9 where nothing is
reached; bit 0x10 is the line-of-sight flag. `integration` is the integer cost to the nearest goal (10 per straight
move, 14 per diagonal, times the entered cell's cost over flat), `null` where no goal is reached. `goals` are the goal
cells `[cx, cz]` after dropping blocked ones. A full field also reports its build `stats` and `bits_hash` (the hex
`FlowField::bits_hash`, the value a desync check compares); a clustered flow reports `cluster_stats` instead and builds
only the local fields the window needs (on 4096 x 4096 its route takes about 1.7 s on the first call; see the measured
numbers above).

| Code | When |
|---|---|
| `no_det_flow` | The app has no `DetFlowView` resource, or the host has not published a grid yet (not found). |
| `grid_flow_bad_goal` | `goal` is missing, is not `[x, z]` or a list of 1 to 64 such points, or a point is off the grid. |
| `grid_flow_bad_profile` | `profile` is not an index of the grid's profiles. |
| `grid_flow_bad_window` | `window` is not `{ x0, z0, x1, z1 }` with cell indices, is empty, has another field, or does not overlap the grid. |
| `grid_flow_goal_blocked` | Every goal cell is blocked for the profile. |

A wrong JSON type or an unknown parameter is `F0002` (`data.param` names it). Checks:

```text
cargo test -p regia_nav_det --lib flow
cargo test -p regia_nav --lib det_view
cargo test -p regia_agent --lib sim_grid_flow
cargo test --release -p regia_nav --test det_flow_perf -- --ignored --nocapture   # perf numbers (printed, never asserted)
```

`flow_arrows_draw_through_the_debug_sink`, `flow_view_answers_windows_of_the_published_grid`,
`grid_flow_answers_a_window_of_the_published_grid`, `grid_flow_refusals_name_their_codes` and
`flow_arrows_reach_the_det_debug_draw_method` cover the arrows and the method.

`flow_bits_match_the_pinned_hash` runs 48 seeded ticks (full fields with random profiles, goals and settings, sampled
nearest and blended, plus clustered flows walked from random cells) against `crates/regia_nav_det/goldens/flow_fields.detgold`
([Determinism goldens](det-golden.md)). A change there moves a lockstep sim's units: a sim-version change.

## Driving the grid from behavior trees

`regia_ai::DetGridProvider` puts a `NavGrid` behind the `PathProvider` seam, so the `MoveTo` leaf and state-tree task
plan on it (`MoveTo::new(..).with_provider("det_grid")`). It converts at the boundary (a `Vec3` gives the grid its
`(x, z)`; waypoints come back at `surface_y`), maps `PathAgentSpec::profile` to the movement profile, returns
`Partial { reason: GoalUnreachable | GoalSnapped }` for partial and snapped plans, and with `with_tick_budget(work)`
(or `with_default_tick_budget()`, `path_tick_budget_for` of the grid) queues requests past the tick's work budget as
tickets served in ticket order at the start of each tick. The grid module itself still imports no ECS type. When to
pick the grid over the other stacks: [Choosing a navigation stack](../user-guide/choosing-navigation.md).
