# Navigation & AI

<!-- index: Navmesh -->
<!-- index: Navigation agent -->
<!-- index: Crowd -->
<!-- index: Off-mesh link -->
<!-- index: Behavior tree -->
<!-- index: Perception -->
<!-- index: Formation -->
<!-- index: Flow field | see: Navigation agent -->

`regia_nav` builds a navmesh from level geometry and moves agents over it with pathfinding and
local avoidance. `regia_ai` adds behavior trees and a blackboard for decision-making. The tier-2
scope is [spec 22](../../docs/specs/22-tier2.md). Off-mesh links are
[spec 92](../../docs/specs/92-navmesh-offmesh.md). State trees are
[spec 38](../../docs/specs/38-state-trees.md). Which stack to call from a game is
[Choosing a navigation stack](choosing-navigation.md). Goal-oriented planning is
[GOAP](goap.md).

## Navmesh generation

```rust
use regia_core::math::Vec3;
use regia_nav::{NavMesh, NavMeshBuilder};

fn build_a_navmesh() -> Result<NavMesh, regia_nav::NavBuildError> {
    // A triangle soup (e.g. the level's static colliders), then build.
    let triangles = [
        [Vec3::new(-4.0, 0.0, -4.0), Vec3::new(4.0, 0.0, -4.0), Vec3::new(4.0, 0.0, 4.0)],
        [Vec3::new(-4.0, 0.0, -4.0), Vec3::new(4.0, 0.0, 4.0), Vec3::new(-4.0, 0.0, 4.0)],
    ];
    NavMeshBuilder::default().build(&triangles)
}
```

The pipeline is Recast-style (voxelization → walkable marking → regions → polygons). Polygons are a greedy rectangle merge of the
walkable grid; contours are traced separately for display and do not feed them.
`NavAgentParams` (radius, height, max_climb, max_slope) describe the creature the mesh is for; the
radius erodes the walkable surface away from walls. `NavMeshBuildParams` set the horizontal voxel
size (`cell_size`), the span-merge gap (`cell_height`: stacked spans in one column merge when the
gap is at most half of it; surface heights stay on the source triangles) and the border.

## Moving agents

An agent is an entity with a `NavAgent` (movement parameters) and a `NavAgentState` (velocity,
waypoints). `NavPlugin` steers them in `FixedUpdate` with ORCA local avoidance, integrating by
the fixed step (`FixedTime::step`), so speed does not depend on the frame rate.

```rust
use regia::prelude::*;
use regia_nav::{NavAgent, NavAgentState, NavPlugin};

fn add_nav_agents(app: &mut App, agent: Entity) {
    app.add_plugins(NavPlugin);
    app.world_mut().entity_mut(agent).insert((
        NavAgent { radius: 0.35, max_speed: 3.5, ..Default::default() },
        NavAgentState::default(),
    ));
}
```

Plan a path with `NavQuery` and install it:

```rust
use regia::prelude::*;
use regia_core::math::Vec3;
use regia_nav::{NavAgentState, NavMesh, NavObstacles, NavQuery};

fn plan(mesh: &NavMesh, obstacles: &NavObstacles, state: &mut NavAgentState, from: Vec3, to: Vec3) {
    let query = NavQuery { mesh, obstacles: Some(obstacles) };
    if let Some(path) = query.find_path(from, to, Vec3::splat(2.0)) {
        state.set_path(path);
    }
}
```

`find_path` runs A* over the polygon graph, then string-pulling (the funnel algorithm) to a smooth
waypoint list.

The agent step runs in the `NavStep` set and advances by the fixed step (`FixedTime::step()`), so
agents move at the same speed at any frame rate. Order a system that plans paths `.before(NavStep)`,
so agents follow the new plan on the same step. Order a system that reads the moved agents
`.after(NavStep)`. Left unordered, the executor may run them either way, and replays then depend on
the thread count (see [Replay soak](../agents/replay-soak.md)).

## Dynamic obstacles

`NavObstacles` carves axis-aligned boxes out of the mesh; the carved surface is skipped by
pathfinding until the obstacle is removed.

```rust
use regia_core::math::Vec3;
use regia_nav::NavObstacles;

fn carve_then_reopen(obstacles: &mut NavObstacles) {
    let id = obstacles.add_box(Vec3::new(1.0, 0.0, 1.0), Vec3::new(2.0, 2.0, 2.0));
    // ... pathfinding routes around the box ...
    assert!(obstacles.remove(id));
}
```

## Tile cache and crowds

For obstacles that come and go often (doors, vehicles, dropped props), build a `NavTileCache`
instead of a single mesh. It keeps the voxelised walkable surface, cuts it into square tiles and
rebuilds only the tiles an obstacle touches:

```rust,wrap
# use regia_nav::*;
# use regia_core::math::Vec3;
# let level_triangles: Vec<[Vec3;3]> = todo!();
# let (min, max, from, to) : (Vec3, Vec3, Vec3, Vec3) = todo!();
let mut cache = NavTileCache::build(
    NavTileCacheParams { build: NavMeshBuildParams::default(), tile_size: 32 },
    &level_triangles,
)?;
let door = cache.add_obstacle(NavObstacleShape::Box { min, max })?;
cache.update(); // rebuilds the touched tiles only; `NavTileUpdate::rebuilt` lists them
let path = cache.query().find_path(from, to, Vec3::splat(2.0));
cache.remove_obstacle(door)?;
```

`NavObstacleShape::Cylinder { base, radius, height }` stands on `base`. Footprints are grown by the
agent radius. `update_limited(n)` spreads rebuilds over frames.

`NavCrowd` steers many agents along navmesh corridors with local avoidance (velocity sampling
against neighbours and walls), separation and collision resolution. It runs in whole fixed steps,
so two runs with the same inputs are bit-identical (`NavCrowd::digest`):

```rust,wrap
# use regia::prelude::*;
# use regia_core::math::Vec3;
# use regia_nav::*;
# let app: &mut App = todo!();
# let cache: NavTileCache = todo!();
# let (start, goal): (Vec3, Vec3) = todo!();
let mut crowd = NavCrowd::new(NavCrowdConfig::default())?;
let id = crowd.add_agent(cache.mesh(), start, CrowdAgentParams::new(0.35, 1.6, 3.5, 8.0))?;
crowd.set_target(cache.mesh(), id, goal)?;
app.insert_resource(cache).insert_resource(crowd).add_plugins(NavCrowdPlugin);
```

`NavCrowdPlugin` rebuilds queued tiles and steps the crowd once per `FixedUpdate` (set
`NavCrowdStep`), replanning corridors when tiles were rebuilt. Read agent positions with
`crowd.agent(id)` after `NavCrowdStep`. Agents report `Idle`, `Walking`, `Arrived` or `NoPath`, and
`OffMeshLink`, `Dormant`, `AwaitingTile`, `LinkQueued` or `LinkTimedOut` when off-mesh links or tile
streaming are in use.
The agent-protocol surface is on [navigation crowds](../agents/navigation-crowds.md).

## Raycasts, random points and partial paths

`NavQuery` also answers the questions AI code asks between paths (NM-1):

- `raycast(start, end, extents, filter)` walks the mesh surface along a straight line and stops at
  the first wall or left-out area. `hit`, `t` (how far along), the wall `normal` and the polygons
  crossed come back in a `NavRaycastHit`: can this agent walk straight there, and where does a
  charge stop?
- `random_point(filter, rng)` gives a point uniform over the walkable surface, and
  `random_point_in_radius(center, radius, extents, reachable_only, filter, rng)` one within a
  circle; `reachable_only` skips islands the agent could never reach. The `Rng` is yours: the same
  seed gives the same point on every platform, so wander and patrol picks replay.
- `find_path_partial(start, end, extents, filter)` returns `NavPathResult::Complete`, `Partial`
  (the goal is unreachable: a path to the reachable point nearest it) or `NoStart`. `find_path`
  still returns `None` for an unreachable goal.

```rust
use regia_core::math::{Rng, Vec3};
use regia_nav::{NavPathResult, NavQuery, NavQueryFilter};

/// A reachable wander target within 8 m, replayable from `seed`.
fn wander_target(query: &NavQuery, here: Vec3, seed: u64) -> Option<Vec3> {
    let filter = NavQueryFilter::default();
    let mut rng = Rng::new(seed);
    let picked = query.random_point_in_radius(here, 8.0, Vec3::splat(2.0), true, &filter, &mut rng);
    picked.ok().flatten().map(|(_, point)| point)
}

/// Walk as close to `goal` as the mesh allows.
fn approach(query: &NavQuery, here: Vec3, goal: Vec3) -> Option<Vec<Vec3>> {
    match query.find_path_partial(here, goal, Vec3::splat(2.0), &NavQueryFilter::default()) {
        NavPathResult::Complete(path) | NavPathResult::Partial(path) => Some(path.waypoints),
        NavPathResult::NoStart => None,
    }
}
```

## Stuck agents

Agents get stuck: pinned in a corner by others, wedged head-on in a corridor too narrow to pass.
Set `NavCrowdConfig::stuck_window_steps` (or call `NavCrowd::set_stuck_detection`) and a walking
agent that has not come `stuck_min_progress_m` closer to its next corner for that many fixed steps
first re-plans, then is nudged toward the center of the polygon under it, then gives up as
`CrowdAgentState::Stuck`. Each rung is a `NavStuckEvent` (`NavCrowd::stuck_events`, and a message
with `NavCrowdPlugin`); `CrowdStats` counts them. A stuck agent walks again when you give it a
new target. `NavAgent` steering has the same ladder: insert a `NavAgentStuckConfig` resource and
read `NavAgentState::stuck.is_stuck()` or the `NavAgentStuck` messages. Both are off by default.

## Keeping the navmesh matched to the world

When the level itself changes, rebuild the navmesh under the change. Add `NavWorldPlugin` from
`regia_framework::nav_world` next to your `NavTileCache`, and chunks leaving a destructible and
runtime terrain edits rebuild the tiles under them, a few per fixed step:

```rust,wrap
# use regia::prelude::*;
# let app: &mut App = todo!();
app.add_plugins(regia_framework::nav_world::NavWorldPlugin);
```

The result equals a full build from the same geometry, and crowd agents replan on their own. For
geometry the plugin does not know, call `NavTileCache::rebuild_region` yourself, or write a
`NavRegionDirty` message and set your own source with `NavRegionQueue::set_source`.

A streamed world keeps its navmesh in cooked cells: see
[Navigation: region rebuilds and tile streaming](../agents/navigation-world.md), which also lists the
agent methods.

Ground can have a cost. Paint an area over the mud and price it, and paths go around it:

```rust,ignore
use regia_nav::{NavAreaCosts, NavAreaVolume, NavObstacleShape};

cache.add_area_volume(NavAreaVolume::new(
    NavObstacleShape::Box { min: Vec3::new(-6.0, -1.0, -8.0), max: Vec3::new(6.0, 1.0, 8.0) },
    MUD,
))?;
let mut costs = NavAreaCosts::default();
costs.set(MUD, 8.0)?;
cache.set_area_costs(costs);

// This unit does not mind mud, and never swims.
params.link_filter = params.link_filter.with_area_cost(MUD, 0.125)?.without_area(WATER)?;
```

In a scene, a `regia_framework::nav_world::NavAreaZone` component paints a box around its entity.

For crowds in the distance, lanes cost no pathfinding: a `NavLaneGraph` of one-way lanes, from code or a
`.navlanes.ron` file, and a `NavLaneCrowd` that follows, passes and takes junctions with a fixed step. See
[Navigation: area costs and crowd lanes](../agents/navigation-lanes.md).

## Behavior trees

A tree is authored as a flat node arena (`BehaviorTreeDef`, serializable to `.bt.ron`), compiled to
a runtime `BehaviorTree`, and ticked by `AiPlugin`. Leaf tasks are Rust types:

```rust
use regia_ai::{BtContext, BtStatus, BtTask};

#[derive(Default, regia_ai::BtTask)]
struct SeekTarget;

impl BtTask for SeekTarget {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        // ctx.world (exclusive), ctx.entity, ctx.dt, and the Blackboard via ctx.blackboard().
        let _ = ctx.dt;
        BtStatus::Running
    }
}
```

Composites (selector, sequence, parallel), decorators (condition, cooldown, repeat, timeout,
inverter) and **services** (a task run every tick for state updates, returning its child's status)
compose these leaves. Attach a `BehaviorTree` + `Blackboard` to the agent entity and register the
task in the `BtTaskRegistry`; `AiPlugin` ticks every tree each `FixedUpdate` and writes a reflected
`BehaviorTreeStatus` snapshot (root + per-node status) so the editor and agent protocol can inspect
the running tree.

The tick runs in the `AiStep` set, and `BtContext::dt` is the fixed step. The tick is an exclusive
system, so give it a place among your fixed-step systems. For example, the arena chains
`(PhysicsStep, AiStep, NavStep, ...)` so the trees plan paths that the nav step then follows on the
same step.

## Perception

A `Perception` component (sight radius and half-angle, hearing radius, team) gives an agent senses.
`line_of_sight` raycasts through the physics world, `in_sight_cone` tests the cone, `can_hear`
filters by distance, and `HearingEvent` is a message for team-filtered sound cues. Turn on
`AiPerceptionGizmos` to draw the senses ([Gizmos](gizmos.md)).

## Stealth-style perception

Leave the defaults and an agent still acquires a target on the first clear look and forgets it
after `max_age_ticks`. For a stealth, squad or indoor game, turn on the pieces you need:

- **Hiding.** Put `Visibility` on the target (`factor` from 0 to 1: light, stance, camouflage).
  Put `SightSamples` on the observer to cast several rays across the body's bounds. Set
  `PerceptionSettings.detection_threshold` above 0. The meter fills from the clear fraction, the
  visibility and how far the target sits from the center of the cone. A half-hidden or dark
  target takes more steps to notice. A target the agent already knows stays known while any
  sample is clear.
- **Memory that fades.** Set a sense's `*_confidence_half_life`. Confidence falls while the
  target is unseen, and the last known position keeps moving by the last known velocity. With
  the half-life left unset, nothing moves and confidence stays 1.
- **Touch and scent.** `touch_radius` feels contact with no line of sight. `PerceptionSenses`
  registers a named callback (a scent trail, a heat sense) that returns a strength or nothing.
- **Squads.** `share_knowledge` writes what this agent sensed into `TeamKnowledge` for its team
  and, when it has an `AiSquadMember`, for its squad. A mate reads that list and can react to a
  target it never saw.

`ai.perception_get` reports confidence, the last known velocity and the meter.
`ai.team_knowledge` reads one team's shared facts. `ai.senses` lists the registered callbacks.
The agent page is [AI perception](../agents/ai-perception.md).

See `samples/arena` for a flag-gated example (`ArenaConfig::use_navmesh`): bots navigate on the
navmesh and are driven by a behavior tree composed of a `SeekEnemy` and a `FireAtEnemy` leaf.

## Moving a thousand units to one place

On the deterministic grid (`regia_nav::det`, for lockstep games), a group move order should not plan one route per
unit. Build one flow field toward the destination and let every unit read its next step from it:

```rust
use std::sync::Arc;
use regia_nav::det::flow::{FlowCache, FlowCacheSettings, FlowSettings};
use regia_nav::det::grid::{GridSettings, NavGrid, NavStatic, Profile};

# fn main() -> Result<(), regia_nav::det::grid::GridError> {
let grid = NavGrid::new(Arc::new(NavStatic::build(None, Arc::new(GridSettings::hover_ground_jet()))?))?;
let mut flows = FlowCache::new(FlowCacheSettings::default());
let rally = grid.cell_at(200.0, 150.0);
let units: Vec<[f32; 2]> = (0..1000).map(|i| [-300.0 + (i % 40) as f32 * 4.0, -200.0 + (i / 40) as f32 * 4.0]).collect();
// Every unit with this order shares one field; the second lookup is a cache hit.
let field = flows.field(&grid, Profile::HOVER, &[rally], FlowSettings::default());
for pos in &units {
    if let Some(dir) = field.direction_at(*pos) {
        // Feed `dir` to the unit's steering (and the deterministic avoidance on top).
        let _ = dir;
    }
}
# Ok(())
# }
```

- Each tick, after `grid.sync_stamps(...)`, call `flows.on_sync(&grid, &sync)`. Full fields are rebuilt on next use;
  clustered flows rebuild only what the change touched.
- On a big map (past about 512 x 512 cells) use `flows.cluster_flow(&grid, profile, &[rally])` instead: it builds only
  the clusters units stand in and the chain to the goal. Its routes can cost up to about an eighth more than the full
  field's; reachability is the same.
- Turn on `FlowSettings { los_cells: 32, .. }` so units in plain sight of a single goal head straight for it instead of
  along 8-way steps, and `blend: true` for smoother turns between cells.
- A unit standing on a blocked cell gets `None`: move it to `grid.nearest_passable(...)` first.
- To see a field, hand `FlowDebug::new(&field)` to the deterministic debug draw (`DetDebugDraw::record`) and turn on
  its `flow` part; arrows point along each cell's move. To let an agent read fields, publish the grid into
  `DetFlowView` each tick (`view.publish(tick, &grid)`); `sim.grid_flow` then answers the directions and costs of any
  window for a goal it names.

Everything is integer cost and deterministic: two peers with the same grid and goals get the same field bit for bit.
Details and the costs: [Deterministic navigation grid](../agents/det-grid.md#flow-fields).

## Moving agents from a tree: `MoveTo`, and which stack to use

`regia_ai::MoveTo` is a ready-made move leaf (and state-tree task): it plans through a named path provider (this
navmesh, the deterministic grid or the deterministic navmesh), follows the path or hands it to the crowd, succeeds
inside its acceptance radius and re-plans when the target moves. `AiPlugin` registers it and a `navmesh` provider. The
page [Choosing a navigation stack and a utility AI](choosing-navigation.md) says when to use this navmesh, the lockstep
grid or the lockstep navmesh, and which utility reasoner goes with each.

## Group movement

A squad can move in a shape. Put an `AiFormation` on the squad, or load a `.formation.ron` file (named slots
around the leader, optional role tags, a spacing scale). The three built-in shapes are ring, line and wedge.

To have the navigation crowd keep that shape, mark each follower with a leader and a slot offset
(`NavCrowd::set_agent_group`). The leader walks its path. Followers steer to their slots and match the leader's
speed. In a doorway narrower than the shape they fall into a column behind the leader, then spread back into
the shape on the far side. When the leader stops, the group keeps the facing it had while moving. Turn on
the `ai_formation` gizmo group to see the slots and the column.

## Seeing AI in play

The behavior-tree debugger and the state-tree panel are editor panels. In a running game the same
information is an in-world overlay, off until you turn it on.

In the viewport, open **Show** and tick the row you want: Behavior tree, State tree, GOAP, Squad,
Formation, Move target, or Blackboard. Those rows stay as you set them when Game view is on. The
behavior-tree row writes the running leaf's name above the agent. The state-tree row writes the
active state. GOAP writes the current plan step and the goal. Squad draws a line from the leader
to each other member and a letter for the role. Formation marks each slot. Move target draws the
point the agent is walking to and the rest of its path. Blackboard writes the keys you asked to
watch.

The overlay can follow the selection, everyone within a radius, or every agent. It keeps at most
64 text labels a frame unless you raise that cap; the rest are counted and skipped. Shapes are
not capped. Turn the rows off and the overlay draws nothing.
