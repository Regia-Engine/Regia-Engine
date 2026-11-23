# Choosing a navigation stack and a utility AI

Regia has three ways to find paths and two utility reasoners. They solve different problems; this page says which one
to pick, and how one behavior-tree leaf, `MoveTo`, drives any of them (ADR-8293).

## The short answer

| Your game | Paths | Utility AI |
|---|---|---|
| An ECS game: one authority, a server and clients, or single player; replays on the same build | The **navmesh** (`regia_nav::NavMesh`, `NavQuery`, `NavCrowd`) | `.utility.ron` reasoners (`regia_ai::UtilityReasoner`) |
| A lockstep game on open, mostly 2D terrain (an RTS map, hover units, flow fields) | The **deterministic grid** (`regia_nav::det::grid` + `det::search`) | The lockstep utility scorer (`regia_detsim::ai`) |
| A lockstep game with interiors: rooms, doors, stairs, tunnels, several floors | The **deterministic navmesh** (`regia_nav::det::navmesh`), with the grid for hover hulls outside | The lockstep utility scorer (`regia_detsim::ai`) |

"Lockstep" means every peer runs the simulation itself from the same inputs and must compute the same bits, on every
operating system and CPU. The f32 navmesh and its crowd are not built for that: their f32 math (lengths, normalizing,
float functions) is not held to the lockstep rules and they use hash-map lookups, so two machines can drift. They are repeatable on one build of one binary,
which is what an authoritative server and a replay of a recorded session need.

## The three path stacks

**The navmesh** (`regia_nav`, f32). Baked from level geometry (voxelized, regions, contours, polygons), with area
costs, off-mesh links (ladders, jumps, doors) and link queues, dynamic obstacle carving (`NavObstacles`), tiles that
stream with the world (`NavTileCache`), and a crowd (`NavCrowd`) with local avoidance and link crossings. It is the
richest stack and the one the editor authors (bake settings, link tools, debug views). Pick it whenever you do not
need lockstep. See [Navigation & AI](navigation-ai.md) and [Navigation crowds](../agents/navigation-crowds.md).

**The deterministic grid** (`regia_nav::det::grid`, `regia_nav::det::search`). One cell per ground sample (5 m in the
default settings), a cost per movement class, obstacle discs stamped per size class, and a two-level A\* (regions per
cluster, then cells in the corridor) with a fixed expansion budget and `(f, h, index)` ties, so every peer pops the same
node. Optional tunnel layer (`det::layer`). It is the cheapest stack per request and the natural home of flow fields
and per-tick path budgets for hundreds of units. Its limits: one surface per cell (plus the tunnel layer), so it does
not describe stacked floors or rooms. See [Deterministic navigation grid](../agents/det-grid.md).

**The deterministic navmesh** (`regia_nav::det::navmesh`). Convex polygons baked from a ground raster plus authored
corridors, rooms, stairs and doors, with links that record their width, step and headroom, so one bake serves a 0.35 m
body and refuses a 2.5 m hull at a door. Same determinism rules as the grid, a byte-stable cooked form (`NVM2`), and
an expansion budget instead of a clock. Pick it for lockstep interiors and multi-level maps; keep the grid for hover
hulls on open terrain (`route_for` decides per unit). The layered multi-level runtime (plan rows W-19 and W-20) builds
on it. See [Deterministic navigation mesh](../agents/det-navmesh.md).

Do not mix stacks for one unit inside one tick: a unit's route comes from one provider. A lockstep game can still use
the f32 navmesh for presentation-only things (a camera drone, a menu scene) that never feed the simulation.

## The two utility AIs

**`.utility.ron` reasoners** (`regia_ai::utility`, f32, ECS). Data assets with response curves, compensation, inertia
and blocking, hot reload, a decision report for debugging, and `UtilityReasoner` on entities. They read the
blackboard. Use them in ECS games, next to behavior trees and state trees. See [AI: utility
reasoners](../agents/utility-ai.md).

**The lockstep utility scorer** (`regia_detsim::ai`). The deterministic scorer for lockstep sims: curves on the pinned
math, per-unit seeded random streams, and a time-sliced schedule that visits units in entity-id order whatever the
slice. Use it when every peer must take the same decision on the same tick.

Rule of thumb: if your paths come from `regia_nav::det`, your decisions come from `regia_detsim::ai`; otherwise use
`.utility.ron`.

## One move leaf for every stack: `MoveTo`

`regia_ai::MoveTo` is a behavior-tree leaf and a state-tree task (state trees tick the same task types). It plans
through a named **path provider**, follows the path, succeeds inside its acceptance radius (across the ground, and within the body height vertically), fails cleanly when there is
no path, and re-plans when its target moves.

<!-- ignore-reason: statement fragment over the host's own `app` and `grid`; the compiled use is crates/regia_ai/tests/move_to.rs -->
```rust,ignore
use regia_ai::{BtTaskRegistry, DetGridProvider, MoveTarget, MoveTo, PathProviders};

// AiPlugin installs a provider named "navmesh". A lockstep game adds its own.
app.world_mut()
    .resource_mut::<PathProviders>()
    .insert("det_grid", DetGridProvider::new(grid).with_default_tick_budget());

// A configured leaf: go to the blackboard's "enemy_base", stop within 4 m, plan on the grid.
MoveTo::new(MoveTarget::Key("enemy_base".into()), 4.0)
    .with_provider("det_grid")
    .with_speed(12.0)
    .register(&mut app.world_mut().resource_mut::<BtTaskRegistry>(), "game::MoveToBase");
```

Without code, the palette leaf `regia_ai::MoveTo` (registered by `AiPlugin`, listed by the behavior-tree and state-tree
editors) reads `move_target` (a vec3 or an entity), `move_acceptance_radius` (0.5 m without it) and `move_provider`
(`navmesh` without it) from the agent's blackboard.

| Provider | Built from | Follows how | Notes |
|---|---|---|---|
| `NavMeshProvider` (`navmesh`) | The `NavMesh` resource (and `NavObstacles`) | Hands the target to the `NavCrowd` when the entity has `CrowdSteered(id)`; otherwise the leaf follows the waypoints | Not lockstep |
| `DetGridProvider` (`det_grid`) | A `NavGrid` the provider owns | The leaf follows the waypoints | Optional per-tick work budget: requests past it queue as tickets, served in order each tick |
| `DetNavMeshProvider` (`det_navmesh`) | An `Arc` of a deterministic `NavMesh` | The leaf follows the waypoints | The agent spec becomes a `MeshAgent` (radius, height, steps) |

Positions cross the seam as world meters. The grid takes `(x, z)` and gives its waypoints back at its ground height;
the deterministic navmesh takes `[x, y, z]` as is. A game can implement `PathProvider` for its own stack (a flow field,
a road graph): `kind`, `plan`, and optionally `poll`, `cancel`, `is_reachable`, `begin_tick`, `steers_with_crowd`.

Progress goes to the blackboard under the leaf's progress key (`move_to` by default): `move_to_state` (`idle`,
`pending`, `following`, `crowd`, `arrived`, `failed`), `move_to_remaining` (meters), `move_to_replans` and, while
failed, `move_to_error`. The same numbers appear in `ai.bt_live`'s `move_to` list
([behavior tree editor](../agents/behavior-tree-editor.md)).

The failure codes: `no_target`, `no_position`, `unknown_provider`, the provider's `no_nav_data`, `start_off_nav`,
`goal_off_nav`, `unreachable`, `invalid_input`, `unknown_ticket`; `partial_path` (the plan stops short of the radius
and `allow_partial` is off), `path_ended` (the path ran out outside the radius), `crowd_refused`,
`crowd_agent_missing`.

**Determinism.** The deterministic providers give the same points for the same requests in the same order on every
target. The leaf's follow step uses only `+ - * /` and `sqrt` in a fixed order, so one binary moves an agent the same
way every run. The leaf runs inside the ECS tick (`AiPlugin`'s `AiStep`): a lockstep game drives that tick from its own
fixed step and orders its agents by spawn order, as `AiPlugin` does.
