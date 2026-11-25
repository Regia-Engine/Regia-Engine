# Deterministic simulation

<!-- index: Lockstep -->
<!-- index: Desync -->
<!-- index: Deterministic simulation -->

A lockstep game runs the same simulation on every machine and sends only inputs. Every peer must compute the same
bits from the same inputs. Regia's lockstep pieces live in `regia_detsim` (math, fixed ticks, goldens) and
`regia_nav::det` (the navigation grid and its search). Choose strict lockstep or rollback lockstep, and check a
running match, in [Lockstep and rollback](../how-to/online/lockstep-and-rollback.md). The wire protocol is in
[Networking](networking.md#lockstep-matches). The reference pages are
[Lockstep math and the determinism scan](../agents/lockstep-math.md) and
[Deterministic navigation grid](../agents/det-grid.md). The hover step has its own page:
[Hover step (lockstep)](../agents/det-hover.md).

## Range and nearest queries without changing results

A simulation often scans the same entity array for auto-fire targets, nearest
enemies, radar contacts and acquisition lists. Build one `DetSpatial` each tick
and share it across those passes. Use array slots, not recycled entity ids.
The grid only narrows the candidates: retain the game's exact distance expression
and comparison, including its handling of ties and NaNs.

This auto-fire example chooses the highest score within 80 m. Equal scores keep
the first array slot, just as an ascending linear scan would. Radius is zero in
the index because this game tests center distance. The score remains game data.

<!-- ignore-reason: statement fragment; regia_detsim is not a book-snippet crate, and the caller's exact distance test must stay in the closure -->
```rust,ignore
use std::ops::ControlFlow;
use regia_detsim::broadphase::{BroadBody, BroadphaseConfig, FloorRule};
use regia_detsim::spatial::{DetSpatial, SpatialRow};

let positions = [[0.0, 0.0, 0.0], [25.0, 0.0, 0.0], [30.0, 0.0, 0.0]];
let teams = [1_u64, 2, 2];
let scores = [0.0_f32, 10.0, 10.0];
let rows: Vec<_> = positions.iter().enumerate().map(|(slot, &pos)| SpatialRow {
    slot,
    body: BroadBody::new(pos, 0.0, [-1.0, 2.0]),
    group_mask: teams[slot],
    flags: 0,
}).collect();
let mut index = DetSpatial::new(BroadphaseConfig {
    cell_size: 40.0,
    floor_rule: FloorRule::UndergroundOnly,
    ..Default::default()
});
index.build(&rows)?;
let center = rows[0].body;
let enemy_mask = !teams[0];
let mut best = None;
let mut best_score = f32::NEG_INFINITY;
let _ = index.within(center, 80.0, enemy_mask, |slot| {
    let dx = positions[slot][0] - center.pos[0];
    let dz = positions[slot][2] - center.pos[2];
    let distance_sq = dx * dx + dz * dz;
    // Keep this game's exact comparison and score reduction unchanged.
    if distance_sq <= 80.0 * 80.0 && scores[slot] > best_score {
        best = Some(slot);
        best_score = scores[slot];
    }
    ControlFlow::Continue(())
});
assert_eq!(best, Some(1));
```

For a first-hit scan, return `ControlFlow::Break(())` only when the exact predicate
accepts. For the first 24 enemies in entity order, use `first_k_within_by` with
that same predicate and a reusable output vector reserved for at least 24 slots.
`first_k_within` by itself returns candidates, which may be outside the radius.
For nearest center distance, `nearest` takes the game's pure squared XZ distance
callback and returns the lowest slot when distances tie; it accepts distance equal
to `max_r²` and ignores NaN distances. Use `within` for priority scores or a game
whose nearest comparison has a different NaN/boundary rule.

If an AI pass moves a unit while iterating, update its host position and call
`index.update(slot, [x, z])` immediately. Later queries then see the movement.
Use `within_mut` to do this inside a query callback. That callback may update any
row's XZ after an accepted visit, but must keep the center/radius fixed and cannot
nest queries or rebuild. Changes to radius, height, flags or groups need a build.

Build and query buffers grow to the largest row count seen. Cell heads are sized
to the configured cell cap, so refiling into an empty cell never allocates. A
far-away row can still coarsen the grid and increase candidate counts; it does
not change acceptance. Choose a reasonable cell cap and profile the real game
field density. `SpatialStats` exposes builds, queries, candidates, selected hits,
refiles and wild rows for the host profiler. See the
[agent guide](../agents/det-ids-broadphase.md#range-and-nearest-queries) for exact
counter definitions and the query/update contracts.

The collision pair pass retains its own movement-aware build and unchanged
results. The spatial index is shared across the range/nearest passes in a tick;
if collision resolution moves entities, rebuild or update the query index before
using it again. No frozen format or existing simulation hash changes here. DO27
adopts the API on its own swap branch and checks all five per-tick hash chains.

## Pathfinding for thousands of units

An RTS order can make a thousand units ask for routes on the same tick. Running a thousand searches in that tick
makes it slow, so use the request queue and let it spread the work across ticks the same way on every peer.

1. **Keep one queue in the sim.** `PathRequests::new(RequestSettings::for_grid(&grid))` sizes the per-tick budget
   from the map (`path_tick_budget_for`: 6000 work units up to 256 x 256, more on larger maps). The queue is sim
   state: clone it with the sim and include `state_hash()` in desync checks.
2. **Ask during the tick.** `queue.request(unit_id, from, to, profile)` returns a `Ticket`. The order units ask in
   does not matter: a tick's requests are served in unit id order, after every earlier tick's requests.
3. **Serve once per tick**, after the tick's obstacle update (`grid.sync_stamps(..)`): `queue.serve(&grid, &mut
   scratch)`. It spends the budget and leaves the rest for the next ticks. No request waits longer than
   `queue.wait_bound(work_ahead)` ticks.
4. **Collect results** with `queue.poll(ticket)` (or `drain_ready()`). A result keeps the plan's kind: `Full`,
   `Partial` (the goal is unreachable; the route ends at the nearest reachable point), `Coarse` (the cell search ran
   out; straight legs between regions), or no plan at all when no free cell is near the start or goal.
5. **Watch the counters.** `queue.counters()` shows requests, served, carried, shared routes (`cache_hits`) and
   expansions. Many carried requests mean the budget is small for the load. Raise `tick_budget`, or accept the wait.

Units that move together share work. Requests with the same start region, goal region and profile use one coarse
route while the obstacles are unchanged (even when the budget spreads them over several ticks), so a squad of 100 heading for the same area costs about one long-haul search plus 100 short cell
searches.

**Big maps.** At the default cluster size (8 cells) one coarse node covers 40 m. On a 4096 x 4096 map set
`GridSettings::cluster_cells` to 16 or 32. That gives fewer coarse nodes and a wider corridor, and the routes stay
within 5 % of the best path. Every peer must use the same settings. A different cluster size is a different
simulation, so change it only with a sim-version change.

## What det-scan catches and why

Every lockstep peer steps the same inputs and must produce the same state. Keep host services
outside the tick, use stable IDs and explicit seeded streams, and verify numerical pins across
supported platforms.

Run `cargo xtask det-scan --json` for the engine closure, or
`cargo xtask det-scan --external /path/to/game --crate game-sim --json` for a game.
The [generated rule catalog](../agents/lockstep-math.md#cargo-xtask-det-scan) lists every rule
and its reason. In addition to platform float math, hash iteration and parallel/SIMD operations:

| Rule | What to change |
|---|---|
| `wall_clock` | Replace clock-dependent step decisions with recorded tick counts. Profiling clocks must not feed simulation state. |
| `os_random` | Replace OS-seeded generators and `RandomState` with agreed seeds and deterministic streams. |
| `env_read` | Pass agreed settings into the step instead of reading host environment or arguments. `option_env!` and `env!` are allowed compile-time macros; peers still need compatible build settings. |
| `address_order` | Order by stable IDs, not pointer addresses. The scan flags visible pointer-to-`usize` casts and address extraction; explain non-ordering uses. |
| `thread_spawn` | Create workers outside the tick; consume results in a defined order, independent of completion order. |
| `arch_cfg` | Avoid architecture/feature-specific simulation behavior. An equivalence boundary, such as the floating-point environment guard, needs a reasoned waiver. |
| `float_sort` | Use `total_cmp` with an agreed NaN policy or an integer key. `partial_cmp` can return `None`; unwrapping it can panic on NaNs. |

Comments, literal contents and test-only items are skipped. New rules follow tokens across line
breaks. For a reviewed exception, put `// det-scan: allow(rule_name): concrete reason` on the
finding line or immediately above it. A reasonless waiver fails with `allow_without_reason` and
keeps the original finding. Files copied as-is use an exact-line `[[pin]]` row in the closure
policy; never change their bytes just to insert a comment.

Use the lockstep `clippy.toml` too: it resolves std methods and types through imports. Scanner
waivers and clippy expectations are separate reviews. Neither source scanning nor clippy proves
determinism: run the same recorded inputs and numerical pins on every supported platform.

## Hover hulls from user content

A hover hull (`regia_detsim::hover::HoverParams`) that comes from a mod, a map file or a settings
message is untrusted input. Check it when you load it and step the sanitized numbers:

<!-- ignore-reason: regia_detsim is not a dependency of the book snippet crate; validate_names_the_bad_field in regia_detsim runs these calls -->
```rust,ignore
use regia_detsim::hover::{self, HoverParams, HoverState};

let hull: HoverParams = load_hull()?;
if let Err(e) = hull.validate() {
    // `e.field` names the field, `e.kind.code()` is one of not_finite, negative, zero_rate,
    // out_of_range, inverted_range.
    report_to_modder(&e.to_string());
}
let hull = hull.sanitized(); // a valid hull comes back unchanged, bit for bit
```

The step itself never latches a NaN: a bad `dt` leaves the state as it was, a NaN input axis reads as
0, a NaN floor reads as the previous floor, and a state that already holds a NaN resets to rest at the
floor (`hover::try_step_tuned` reports it). The full rules and the error codes are in [Hover step
(lockstep), Bad input](../agents/det-hover.md#bad-input).

## Hover hulls and worlds

A hover game tunes three things besides the hull: the model's limits (`HoverLimits`: how high and how
often a hull hops, how well it steers in the air, when a slope counts as a cliff), the match's climb
rules (`ClimbTuning`) and the world (`WorldPhysics`: gravity, air density, vacuum, the low-g mode).
Start from the defaults and change what your game needs:

<!-- ignore-reason: regia_detsim is not a dependency of the book snippet crate; the doctest of regia_detsim::hover::step_with runs these calls -->
```rust,ignore
use regia_detsim::hover::{self, ClimbTuning, HoverLimits, WorldPhysics};

let limits = HoverLimits { hop_apex_m: 1.5, hop_cooldown_s: 0.3, ..HoverLimits::DEFAULT };
let world = WorldPhysics { gravity_m_s2: 3.7, ..WorldPhysics::EARTH };
limits.validate()?;
world.validate()?;
// each tick, for each hull in a fixed order:
hover::step_with(&hull, &limits, &ClimbTuning::AIR_TIME, &world, &mut state, &input, floor, dt);
```

These are match settings: every player must use the same ones. Send them in your lobby's settings
message and compare them at match start; for the world, compare `world.to_le_bytes()` (10 bytes) and
refuse a player whose bytes differ, or their simulation drifts apart from the first airborne tick.
The older calls (`hover::step`, `step_world`, `step_tuned`) keep working and step exactly as before.
Note that `hover::step` flies gravity 9.8, while `WorldPhysics::EARTH` is 9.81: both are fixed for
existing replays. Details: [Hover step (lockstep), Model limits and
worlds](../agents/det-hover.md#model-limits-and-worlds).

## Blessing determinism goldens safely

A determinism golden records a seeded scenario's hash after each tick. Every ordinary scenario
must produce the reference target's bits on every supported target. Run the checks with:

```sh
cargo test -p regia_detsim --test det_golden -- --nocapture
```

Read each `regia-det-golden` JSON line. `ok` means the target's committed hash matches and the
clean scenario agrees with the reference. `unrecorded_target` means the bits agree but the
fixture has no line for this target yet. `diverged` identifies changed bits. A previously blessed
target line that hides a cross-target disagreement fails as `disagrees_with_reference`.

When a scenario change is intentional, review its first differing tick, record the reference
on `x86_64-linux`, and explain the changed output in the commit message. Then run the same check
on the other targets and record their lines only after they agree. `REGIA_BLESS=1` enables
recording; `bless_refused_clean_differs` means a clean non-reference target disagreed and the
file was left untouched. Fix the difference before recording it. Missing or stale goldens need
review of their scenario name, seed, tick count and rate as well as their hashes.

Leave `DetScenario::planted()` at its default false for game scenarios. True is reserved for
negative controls deliberately designed to diverge across architectures. Both normal and
injected recordings carry the policy in `DetTrace::planted`; it never changes scenario bits or
the golden file format. Marking a desynchronized game scenario as planted defeats the check.

Golden hashing preserves the distinction between `-0.0` and `+0.0` to catch zero-sign drift.
The desync hasher's canonical rule folds both to positive zero; both hashers fold NaN payloads.
A matching desync hash therefore does not establish matching golden bits.

See [Determinism goldens](../agents/det-golden.md) for status fields and scenario registration,
and [golden runners](../agents/det-golden-runners.md) for collecting and verifying target legs.


## Roads, danger zones and buildings

The deterministic navigation grid (`regia_nav::det::grid`, see the agent page
[Deterministic navigation grid](../agents/det-grid.md)) prices every cell from its terrain. Three additions let a
lockstep game route the way its players expect, and none of them changes a grid that does not use them:

- **Roads and terrain types: areas.** List the area types in `GridSettings::areas`, for example a `"road"` at 50 % of
  the terrain cost, `"mud"` at 200 %, or `"deep_water"` that blocks walkers (`blocked_for`). Paint them onto the map
  once with `NavGrid::paint_area`, by cell rectangle or by convex polygon in centimeters. Movers then prefer the road
  when the detour is cheaper than the straight line. Every peer must use the same areas and paint the same cells: they
  are part of the match settings.
- **Danger zones and threat maps: the cost overlay.** Build a `CostOverlay` (one extra cost per cell for the profiles
  you choose) from your game's threat or congestion map, and hand it to `NavGrid::set_overlay` each time it changes
  (every tick, or every few ticks to save work). Routes bend around costly cells but can still cross them; to forbid a
  cell, use an area that blocks the profile or a stamp. The overlay is sim state: build it from sim data only, in a
  fixed order, so every peer gets the same numbers.
- **Buildings of any shape: stamp shapes.** Units and wrecks stay discs (`Stamp`). Give buildings, walls and parked
  convoys their real footprint with `StampShape::OrientedBox` (from a center, half sizes and a facing direction) or
  `StampShape::ConvexPolygon`, through `NavGrid::sync_shapes_from`. Each is grown by the mover's size class like a
  disc, so a large vehicle keeps its distance from a wall.

For a game with hundreds of moving units, call `NavGrid::sync_stamps_from` with one `Vec` you clear and refill every
tick: it gives the same result as `sync_stamps` and allocates nothing once the game has warmed up.

The neutral names `MoveClass::CLIMB_LOW`/`CLIMB_MID`/`CLIMB_HIGH`, the five `Profile` names such as
`Profile::MID_CLIMB_SMALL`, and the preset `GridSettings::three_class()` describe what each class can climb and how
big it is; the older names are kept as aliases.

## Presentation physics

Regia Engine's lockstep closure is the code that must agree across machines. Presentation
physics is not in that set.

`regia_framework::sim_bridge::presentation` (ADR-8356) places kinematic hulls, a drawn-ground
collider and debris beside an authoritative sim. Physics never writes a `Transform` of an entity
that carries a sim id: a dynamic body with a sim id is refused and counted. The module does not
feed bodies, contacts or debris back into the sim, and it is never on the lockstep closure.

Two clients may scatter debris differently. That is presentation, not a desync of the sim.

Agents read the counters with `sim_bridge.presentation_stats`. See
[Presentation physics](../systems/sim-bridge.md#presentation-physics) and
[Presentation physics (agents)](../agents/presentation-physics.md).

## Local avoidance at 900 agents

`regia_detsim::avoid` steers discs with integer arithmetic, so every peer gets
the same velocities. For a crowd step:

1. Build the tick's `DetSpatial` from your bodies (`slot = i`, XZ from
   `OrcaBody::position_m()`). The same index can serve your other range queries.
2. Keep one `OrcaScratch` per solving thread for the whole match.
3. For each mover, call `orca_velocity_indexed(&mut scratch, &mut index, &bodies,
   slot, preferred, &settings, &query)` with
   `OrcaNeighborQuery::from_settings(&settings, mask)`, against the same snapshot.
4. Apply all velocities after every mover is solved.

After the first ticks the step allocates nothing. Neighbors are the bodies whose
centers lie within the look distance (exactly, in millimeters) that match your
mask and floor rule; the nearest `max_neighbors` of them are kept. If you already
build your own neighbor lists, `orca_velocity_into` takes them with the same
scratch and gives the same bits.

Stay inside the input domain: positions within ±2,147 km, speeds within
±1,048 m/s per axis, radii up to 65.5 m, at most 256 retained neighbors (the
[agent guide](../agents/det-orca.md#input-domain) lists every bound). Outside
it, the solver returns the preferred velocity clamped to the speed bound in every
build, never a debug panic or a release wrap. `try_orca_velocity` and
`OrcaInput::validate` tell you which value was out of range, and
`OrcaScratch::stats()` counts calls, retained neighbors, planes, projections,
out-of-domain calls and index candidates for your profiler. Wrap the whole crowd
step in your tick's profiling span; the kernel has no tracing dependency.

## Measuring a lockstep tick

Name each system once. The desync hash and the tick profiler both take a
`regia_detsim::hash::Systems` list, so the row labeled `ai` in a profile is the same `ai` a
desync report names. Build the list in a `const`. A bad name or a duplicate fails the build.

```rust
const SYSTEMS: regia_detsim::hash::Systems<3> =
    regia_detsim::hash::Systems::new(["input", "move", "ai"]);
```

Keep the profiler beside the simulation, not inside it. Call `begin_tick` at the start of the
tick, `begin` / `end` (or `enter`) around each system, then `end_tick(tick)`. Do not call the
clock from the step function that the hash covers. The same scenario stepped with the profiler
and without it produces the same per-tick fingerprints.

The default budget is 4.0 ms for the whole tick and 4.0 ms for each system (4 000 000 ns).
Change either in `ProfileSettings`. A tick is over budget when its total, or any system, is
strictly greater than its budget. A value equal to the budget passes.

Read three numbers, in this order:

1. `tick.p99_ns` against the whole-tick budget. This is the plan's 4.0 ms p99 check for the tick.
2. Each system's `p99_ns` against that system's budget. The system over p99 is the one to open.
3. `over_budget`. Nearest rank can hide one slow tick behind the 99th of a long run. The list
   keeps every retained tick that missed, with that tick's system rows.

Quantiles use nearest rank: sort the samples the ring still holds and take
`ceil(percent * n / 100)`. The ring defaults to 3600 ticks and overwrites the oldest, so a miss
that scrolled off is gone from the report. `samples` is the retained count, not the lifetime count.

## Debugging a slow or diverging tick

When a lockstep tick is slow, start with the numbers, then look at the picture.

1. **Which system?** Open the Profiler's **Lockstep** tab (or call `sim.tick_profile`). Each system
   has a p99 bar against the tick budget line; a red row is over its budget. The over-budget list
   names the slowest system of each slow tick, so a single spike shows even when p99 looks fine.
2. **Why is the broadphase or a query slow?** Turn on the debug draw: Show ▸ Lockstep in the
   viewport, and the Grid, Occupancy, Candidates and Contacts toggles in the Lockstep tab (or
   `sim.det_debug_draw { "what": ["grid", "occupancy", "candidates"] }`). Many candidate lines and
   few contact lines mean the cells are too large for your bodies (lower `cell_size`); one huge
   count in a single cell means bodies are piled up; candidate lines reaching across the map mean a
   large body inflates every gather (the reach uses the largest radius). Queries shows each query's
   radius and what it returned.
3. **Diverging?** The debug draw never changes the simulation: capture reads state only, is not
   hashed, and a run with it on steps the same bits as a run with it off. Leave it on while you hunt
   a desync and read the per-system desync rows (`sim.desync_rows`) for the first system that
   differs. Contacts are drawn in visit order, so two peers' frames of the same tick can be
   compared line by line.

Capture is off by default and costs nothing while off. A game turns it on through
`DetDebugDraw::record` (an ECS game) or `FixedStepSim::det_debug` (a sim on the sim host); see
[Seeing the index](../agents/det-ids-broadphase.md#seeing-the-index).

Publish the report with `FixedStepSim::tick_profile` on a sim host, or insert a `DetTickProfile`
resource. `sim.tick_profile` returns it. See [Tick profile](../agents/det-tick-profiler.md) for
the method, the `count` cap and the error codes `no_tick_profile` and `bad_count`.

## Hover hulls on your terrain

The hover step moves a hull's speed, heading and height above the ground, but not its position, and it
tilts the hull with the ground only if your game tells it how the ground slopes. The host kit
(`regia_detsim::hover::host`) does both for you. Give it your floor as a `FloorSampler` (a
`HeightGrid` of your heightfield, or `FloorFn(|x, z| ...)` around your own floor function) and keep
your hulls in a `HoverFleet`:

<!-- ignore-reason: regia_detsim is not a dependency of the book snippet crate; the module doctest of regia_detsim::hover::host runs a fleet like it -->
```rust,ignore
use regia_detsim::hover::host::{FleetHull, FleetInput, HeightGrid, HoverBody, HoverFleet};
use regia_detsim::hover::{ClimbTuning, HoverLimits, HoverParams, WorldPhysics};

// Heights row by row: column along +X, row along +Z.
let floor = HeightGrid::new([-500.0, -500.0], 2.0, 501, 501, heights)?;
let hull = FleetHull::new(HoverParams::DEFAULT_HULL, 2.0, 1.0); // half length, half width
let mut fleet = HoverFleet::new(vec![hull]);
let player = fleet.push(HoverBody::at_rest([0.0, floor_height, 0.0], 0.0, &hull.params), 0);

// Every tick, with the inputs that arrived this tick, in any order:
let inputs = [FleetInput::new(player, [throttle, strafe, steer])];
fleet.step_all(&inputs, &floor, &WorldPhysics::EARTH, &HoverLimits::DEFAULT, &ClimbTuning::AIR_TIME, dt);
let pos = fleet.body(player).pos; // where to draw the hull
```

Every tick the fleet reads the ground's slope under each hull from four floor samples (nose, tail,
left, right), steps it, and moves it, always in slot order, so every peer gets the same bits whatever
order the inputs came in. A hull that gets no input this tick coasts. Your floor function must give
the same answer on every machine: use `regia_detsim::math` for any sine or cosine in it. Details and
the cost per hull: [Hover step (lockstep), Host kit](../agents/det-hover.md#host-kit).
## Projectiles

`regia_detsim::projectile` steps shells, grenades and bullets so that every player's simulation agrees:
a projectile kind is a `ProjectileParams` (gravity scale, drag, wind response, radius, lifetime, range,
fuse, bounces), a shot is a `ProjectileState`, and a `ProjectilePool` holds a fixed number of them. Each
tick, step the pool against your hit source:

<!-- ignore-reason: regia_detsim is not a dependency of the book snippet crate; the module doctest of regia_detsim::projectile runs these calls -->
```rust,ignore
use regia_detsim::projectile::{ProjectileParams, ProjectilePool, ProjectileState};

let shell = ProjectileParams { radius_m: 0.1, wind_response: 0.5, lifetime_s: 20.0, ..ProjectileParams::DEFAULT };
shell.validate()?; // once, when the kind is loaded
let mut pool = ProjectilePool::new(256);
let mut impacts = Vec::with_capacity(256);
pool.spawn(shell, ProjectileState::launch(muzzle, velocity, Some(shooter)));
// each tick:
impacts.clear();
pool.step_all(&world, wind, dt, &mut hits, &mut impacts);
for impact in &impacts { apply_damage(impact); } // ascending slot order on every peer
```

`hits` is anything that implements `SegmentHits`: the first contact of a sphere moving along a segment.
The whole segment is tested, so a fast bullet never passes through a thin wall between two ticks.
`LinearScanHits` (planes, boxes, body spheres) is the reference; put your level's collision and your
bodies' index behind the trait and check them against it. To aim, `aim::launch_angles` gives the
drag-free low and high pitch for a target, and `aim::solve_with_drag` the pitch that hits with drag;
both are pure, so an AI on the simulation tick and a trajectory arc in the UI get the same numbers. The
old `ballistic::mortar_*` functions still work (deprecated, same bits); new code uses
`ballistic::ArcLauncher`. Details: [Lockstep projectiles and aim](../agents/det-projectiles.md).

## AI queries without n² scans

An AI that filters the whole world once per unit ("my allies", "my enemies", "the unit with id
42") costs `units x rows` reads per tick. At a 900-unit fight that is most of a tick budget. Build
the answers once per tick instead, with `regia_detsim::ai::TeamRosters` (each team's rows, in row
order) and `regia_detsim::ai::SlotIndex` (entity id to row). Both give exactly the rows, in exactly
the order, the filter gave, so the AI's decisions and every lockstep hash stay the same.

1. At the top of the sim tick, after spawns and removals:
   `rosters.build(&units, |u| u.alive().then_some(u.team))?` and `index.build(&units, |u| u.id)`.
2. In the AI layers, read `rosters.team(my_team)` for allies and
   `rosters.others_into(my_team, &mut enemies)` for enemies, both ascending row order.
3. Look units up with `index.find(&units, id, |u| u.id)` instead of `units.iter().find(..)`.
   If you remove a row with `Vec::remove` during the tick, call `index.remove(slot)` too.
4. For range questions ("enemies within 30 m"), give each row's `SpatialRow::group_mask`
   `team_group_mask(team)` and query `DetSpatial` (above) with the enemy teams' bits.

`rosters.stats()` counts the work (one row read per row per tick). The agent guide has the full
API, the rules and the error code: [Deterministic AI core](../agents/det-ai-core.md#team-rosters-and-the-id-index).

## Choosing targets at scale

If every unit scores every enemy it knows every tick, the cost grows with units times enemies: a
900-unit fight on two teams scores 405,000 targets per tick. `regia_detsim::ai::TargetSelector` keeps
your scoring and decides which enemies get scored and when.

**Writing the score.** Implement `TargetHost` on a small struct that borrows your sim rows:
`score(unit, candidate)` gets two row slots and returns an `f32`, higher is better. Read numbers from
your rows (health, a kind index into a weight table, threat, distance); never look anything up by
name, format a string or build a `Vec` per call, because it runs thousands of times a tick. Use only
`+ - * /` and `regia_detsim::math`, so every peer gets the same bits. Put "is this a legal target at
all" (alive, seen recently) in `eligible`, not in the score.

**Starting point: `Exhaustive`.** `AiRules::default()` scores your known-enemy list exactly as your old
loop did, so moving your code onto the selector changes nothing a player can see, and no checksum
moves.

**Then `Ringed`.** `AiRules::for_version(2)` looks for targets in rings of 80, 120 and 250 m: it scores
at most 8 enemies from the nearest ring that has any, and each unit re-picks every 4 ticks (spread
over the ticks by entity id) unless its target dies, is forgotten or leaves the 250 m ring, in which
case it re-picks at once. At 900 units that is about 225 picks and at most 1,800 scores per tick plus
the forced re-picks.

Choosing rings: make the innermost ring your weapons' useful range, so units fight what they can hit;
the middle ring catches enemies about to arrive; the outermost ring is how far a unit will hold a target
at all. More rings cost nothing when the inner ring has candidates. `max_candidates` trades quality for
time: the candidates are the lowest row slots of the ring, not the best ones, so a larger value lets
your score see more of a crowded ring.

**What players notice when you switch.** Units prefer enemies in range over a higher-value enemy far
away; in a dense blob a unit chooses among a handful of enemies rather than all of them; a new enemy is
noticed within `repick_ticks` ticks (67 ms at 60 Hz with the default), not on the very next tick; and
equal scores now go to the lowest row slot rather than to list order. All of this is the same on every
peer. Because results change, the switch is a sim-version bump: bump your sim version together with
the rules version, re-record your lockstep pins once, and refuse peers on an older build
(`rules.check_peer(peer_version)`).

The agent guide has the full API, the counters and the error codes:
[Deterministic AI core](../agents/det-ai-core.md#target-selection).

## Perception without all-pairs scans

`SightingsBoard::refresh` asks the spatial index which allies are near each enemy instead of
testing every ally of every team against every enemy. The board it writes, and the line-of-sight
questions it asks (same pairs, same order), equal the all-pairs scan's bit for bit, so switching
changes no lockstep hash. Keep one `PerceptionIndex` per board and call `refresh_with` so the
buffers are reused:

<!-- ignore-reason: a sketch of a host's sim tick; `observers`, `enemies_by_id` and `terrain` come from the game -->
```rust,ignore
use regia_detsim::ai::{PerceptionIndex, PerceptionIndexSettings, SightingsBoard, RADAR_RANGE_M};

let mut board = SightingsBoard::new(team_count, RADAR_RANGE_M);
let mut perception = PerceptionIndex::new(PerceptionIndexSettings::default());
// Each tick:
board.refresh_with(&mut perception, Some(&terrain), tick, &observers, &enemies_by_id, &params);
let stats = perception.stats(); // queries, candidates, los_calls, scan_pairs
```

`refresh_linear` keeps the old scan (same arguments) for tests and comparisons. The default
384 m cell suits a 400 m radar; a far-away unit coarsens the grid and costs speed, never results.
The saving grows with the unit count: at a few hundred units in a tight field the two cost about
the same, and the line-of-sight checks (the same calls on both paths) are often most of the bill.

## AI world views

A unit's AI mostly reads team-level facts: its allies, its team's known enemies, their threat.
Copying those into every unit's view every tick costs a copy per unit per row. Build them once and
borrow them:

<!-- ignore-reason: a sketch of a host's AI stage; `units` and `decide_unit` come from the game -->
```rust,ignore
use regia_detsim::ai::{AiUnitFacts, AiTickWorld};

let world = AiTickWorld::new(tick, &rosters, &board);
for (slot, u) in units.iter().enumerate() {
    let view = world.view(AiUnitFacts { entity: u.id, slot: slot as u32, team: u.team, pos: u.pos });
    // view.allies(), view.known_enemies(), view.known_enemy(id), view.visible_threat(),
    // view.enemy_slots_into(&mut buf): all borrowed, nothing allocated.
    decide_unit(&view);
}
```

The stage order of one AI tick: rosters (`TeamRosters::build`), then the host's spatial index
build, then perception (`refresh_with`), then target choice, then decisions (`AiCore::decide`), then
the driver acts. Views borrow the rosters and the board, so those cannot change while views exist.

## Writing an AI driver without n² scans

A driver that, for each unit, looks for the nearest carrier, the enemies in acquire range and the
first 24 of them in id order is three scans of the whole world per unit. With one `DetSpatial`
built after the rosters (rows in id order, so ascending slots are ascending ids, and
`group_mask = team_group_mask(team)` with a separate bit for carriers):

<!-- ignore-reason: a sketch of a game's driver loop; `units`, `CARRIER_BIT` and `acquire_m` come from the game -->
```rust,ignore
use std::ops::ControlFlow;

let mut targets = Vec::with_capacity(24); // reused every unit, every tick
for slot in 0..units.len() {
    let me = index.row(slot).copied().ok_or(DriverError::MissingRow)?.body;
    let enemies = !team_group_mask(units[slot].team) & !CARRIER_BIT;
    // Nearest own carrier: the game's own squared distance (NaN rejects another team's carrier);
    // ties keep the lowest slot.
    let carrier = index.nearest(me, CARRIER_BIT, f32::INFINITY, |s| {
        if units[s].team == units[slot].team { dist_sq(&units[slot], &units[s]) } else { f32::NAN }
    });
    // The first 24 enemies in acquire range, in id order, with the game's exact test.
    index.first_k_within_by(me, acquire_m, enemies, 24, &mut targets, |s| {
        dist_sq(&units[slot], &units[s]) <= acquire_m * acquire_m
    });
    // Moving the unit inside the loop: update the host row, then the index, before the next unit.
    let pos = steer(&mut units[slot], carrier, &targets);
    index.update(slot, [pos[0], pos[2]])?;
}
```

Every query only prunes: keep the game's own distance expression and comparison in the callback,
and the driver picks the same units in the same order as the scan it replaces. Update the index
right after moving a unit (`update(slot, [x, z])`), or use `within_mut` to move units from inside
a query; later queries in the same pass then see the move, exactly as the linear scan would.

## Saving, resuming and sending a lockstep state

A lockstep match sometimes needs the whole simulation state as bytes: a seat that fell out of sync
takes the majority's state, a player who joins late starts from it, a desync report or a replay
carries it, and a rollback goes back to it. Implement `regia_detsim::state::DetState` for your sim
state: a tag, a version, `save` and `load`. Write every field a later tick reads, in a fixed order;
floats are kept as their exact bits, `-0.0` and NaN payloads included.

<!-- ignore-reason: `World`, `tick` and `send_to_seat` are the game's; regia_detsim's state module doctest runs the same calls -->
```rust,ignore
use regia_detsim::state::{self, DetReadLimits};

// On the majority: save at the tick boundary and send the bytes.
let bytes = state::snapshot(&world, tick)?;
send_to_seat(seat, &bytes);

// On the seat: check and load. Wrong game, wrong version or damaged bytes are refused, never a panic.
let (tick, world): (u64, World) = state::restore_with(&bytes, DetReadLimits::DEFAULT)?;
```

If your state derives `serde::Serialize` and `Deserialize`, `save` is `w.put(self)` and `load` is
`r.take()`. The engine's id allocator (`DetIdAllocator`), desync ring (`SystemRing`) and tick rate
already implement it, so a game state nests them with `w.state(&self.ids)` / `r.state()?`.
`SimHarness::save()` and `SimHarness::resume(bytes)` do the same for a whole seeded run.

For rollback, keep checkpoints in a `SnapshotRing`: it saves every `every` ticks, keeps
`capacity_ticks` of history and never holds more than `max_bytes`; `latest_at_or_before(tick)` is
where a rollback starts.

The version is yours. Change what `save` writes and you bump `STATE_VERSION`; old snapshots are
then refused with `bad_version`, which names both versions. Snapshots are not save files: they live
as long as a match, a report or a replay made by the same build, so there is no upgrade path for
them. The error codes and the frame layout are in [Lockstep state snapshots](../agents/det-state.md).

## Math, randomness and integers in a lockstep sim

Three modules cover what a sim tick computes.

**Math.** Call `regia_detsim::math` instead of the standard float methods. It routes every
transcendental through one pinned pure-Rust `libm`, so each host gets the same bits. It has `sin`,
`cos`, `tan`, `asin`, `acos`, `atan`, `atan2`, `sqrt`, `exp`, `exp2`, `ln`, `log2`, `log10`, `powf`,
`hypot`, `cbrt`, `sinh`, `cosh`, `tanh` and `sin_cos`, and `_f64` forms of them. The standard methods
(`x.sin()`, `f32::tan(x)`) are denied by the lockstep crates' clippy config and by
`cargo xtask det-scan`.

```rust,wrap
use regia_detsim::math;

let aim = math::atan2(1.0, 1.0);
let (s, c) = math::sin_cos(aim);
let range = math::hypot(30.0, 40.0);
assert_eq!(range, 50.0);
assert!(s > 0.0 && c > 0.0);
```

**Randomness.** Seed one `DetRng` per match (the same seed on every peer), and give each system its
own stream, so a new dice roll in the loot code never changes what the AI rolls:

```rust,wrap
use regia_detsim::rng::DetRng;

let match_rng = DetRng::seed_from(0x5eed);
let mut loot = match_rng.stream(1);
let mut ai = match_rng.stream(2);
let d20 = loot.range_u32(20) + 1; // exactly fair
let turn = ai.unit_f32(); // [0, 1)
assert!((1..=20).contains(&d20) && (0.0..1.0).contains(&turn));
```

Use `range_u32(n)` for "a number below n", never `next_u32() % n` (which favors small numbers). Save
the generator with the rest of your sim state (`w.state(&rng)` in a `DetState`, or serde), and include
`key()` and `state()` in your desync hash. Its streams come from its key, so save the whole generator,
not only its four state words.

**Integers.** Floats are the same on every host when you follow the rules above, but integers are
the same by construction. Use them where a value must never drift: positions on a grid, edits,
distances you compare. `regia_detsim::fixed::Mm` is an `i32` of millimeters, and `Mm2` an `i64` of
square millimeters. They have no `+`/`-` operators, because Rust's integer overflow behaves differently
in debug and release builds; you choose `checked_add` (gives `None`), `saturating_add` (clamps) or
`wrapping_add`.

```rust,wrap
use regia_detsim::fixed::{self, Mm};

let door = Mm::from_m_f32(0.9).unwrap_or(Mm::ZERO); // 900 mm
let wall = Mm(2_400);
let gap = wall.checked_sub(door).unwrap_or(Mm::ZERO);
assert_eq!(gap, Mm(1_500));
// Integer 2D math: exact.
assert_eq!(fixed::cross([1_000, 0], [0, 1_000]), 1_000_000);
assert_eq!(fixed::isqrt_u64(fixed::len2([300, 400])), 500);
```

`Mm::from_m_f32` rounds half away from zero using integer arithmetic only, and `to_m_f32` converts
back. The round trip is exact for anything within about 8.4 km of the origin.

The engine checks all of this on every target with a golden scenario, `math_rng_fixed_vectors`. If
your game adds its own lockstep code, register a scenario for it the same way (see
[Determinism goldens](../agents/det-golden.md)).

## Checking the hover step on every platform

The engine pins its hover step on every platform with two golden scenarios beside `hover_libm_1000`:
`hover_mixed` (twelve hulls on four worlds over ramps, crests, drops and cliffs, with every kind of
input, both flight modes, auto-level and modern yaw on and off, hops and jumps) and
`lowg_ballistic_mixed` (jumps, debris, mortars and hops on the same worlds in both low-g modes). Run
them on a machine with

```sh
cargo test -p regia_detsim --test det_golden -- --nocapture
```

and read the `regia-det-golden` lines: `ok` or `unrecorded_target` means this machine steps hovers
exactly as the reference build does, so players on it can share a lockstep match with everyone else.
`diverged` names the first tick that differs: report it, do not re-record it. Your own hull can run
the same kind of check in your game's tests ([Determinism goldens](../agents/det-golden.md), "Register
it from a game"). Details: [Hover step (lockstep), Mixed golden](../agents/det-hover.md#mixed-golden).
## Reproducing a desync offline

When two peers split, you want to step the match again on your own machine and see where it went wrong.
Record the inputs as you play, and keep a snapshot from where they start:

1. Implement `regia_detsim::replay::DetInputSim` for your sim (it is already `DetState`; name its systems
   with `hash::SystemFold`): your input type, and `step(tick, &inputs)`. Walk `inputs.seats()` in the
   order it gives.
2. In the match loop, step through `replay::step_recorded(&mut sim, inputs, &mut log, true)` (or
   `replay::advance` when you do not record). It is the same step the replay runs.
3. Keep checkpoints in a `SnapshotRing`. When a desync is reported, take the newest checkpoint at or
   before the split and `log.suffix(checkpoint_tick)`, and save both.

<!-- ignore-reason: `World`, `log`, `ring` and `split` are the game's; regia_detsim's replay module doctest runs the same calls -->
```rust,ignore
use regia_detsim::{replay, state::DetReadLimits};

let (at, checkpoint) = ring.latest_at_or_before(split).expect("a checkpoint");
let tail = log.suffix(at)?;
let report = replay::replay_from_snapshot::<World, 6>(Some(checkpoint), &tail, None, DetReadLimits::DEFAULT)?;
println!("{}", report.verdict_line());
// replay verdict diverged start 4200 end 4232 ticks 32 compared 32 first 4231 system units last_agreed 4230
```

The report names the first tick whose recomputed hash differs from the recorded one, the system that
moved first, and the last tick that still agreed. A clean report (`replay verdict ok`) on the same build
means the peers' inputs, not your step, differ: compare their logs.

To try it without a game, `cargo run -p regia_detsim --example replay` replays the engine's recorded
AI-arena log, and `--flip 120` changes one order at tick 120 so you can see a desync named. Pass your own
log as the first argument once your sim is registered in the example's `DesyncReplayRegistry`.

A recorded run is also a golden: `golden::InputLogScenario::new("my_match", None, log)` registered in a
`DetRegistry` checks every future build against the same bits. Late joiners use the same pieces:
restore the checkpoint, queue the frames in a `replay::CatchUp`, and call `catch_up(&mut sim, &mut queue,
budget)` once or a few times a frame until it reports `caught_up()`.

## Running a lockstep sim on its own thread

A lockstep game can run its sim on a thread of its own and still step exactly the frames every peer
agreed on. Implement `LockstepSim` (`step_inputs(tick, &inputs)`, one call per agreed tick) and
`DetState` for the sim, then start `regia_framework::sim_host::ThreadedSimHost::start_lockstep` with
`HostPacing::Lockstep`. It returns the host and a `LockstepFeed`; your netcode pushes each agreed
frame with `feed.push_frame(tick, frame)`.

- **Exact frames.** The host steps the frame for the next tick only, each once and in order. A gap
  waits for the missing frame; a duplicate or old frame is refused. Nothing is merged.
- **Catching up.** After a late join or a resync, `handle.catch_up(n)` lets the host step up to `n`
  queued frames per wake until it reaches the live tick, then it returns to one tick per wake.
- **Snapshots.** `handle.request_snapshot(tick)?.wait()?` returns the framed bytes when the sim
  reaches `tick`; `handle.restore(bytes)?.wait()?` loads them and republishes the frame at that tick.
  Both run on the sim's thread, between two steps.
- **Floating point.** The thread resets its floating-point environment before the first step and
  checks it again every `fp_check_every` steps, so a library that turned on flush-to-zero cannot
  change your sim's bits.
- **Suspend.** With `SimBridgePlugin`, an app the OS suspends holds the host until it resumes.

The details are in [Networking and the sim bridge: Lockstep pacing](../systems/sim-bridge.md#lockstep-pacing)
and the agent methods (`sim.host_snapshot`, `sim.host_restore`) in [the framework agent
guide](../agents/framework.md#snapshots-and-restore).

## Projectiles, bursts and cones

A shot that tests every entity each tick, a burst that tests every entity, and a
friendly-fire cone that scans every ally all have the same shape. They walk the
array in entity order, and the first exact hit wins. `DetSpatial` (ADR-8478)
keeps that order and narrows the walk. Build one index per tick and share it
with the range passes. Keep the game's own exact tests in the closures.

- **Projectile step.** `first_along_segment(from, to, r, enemy_mask, |slot| exact_hit(slot))`
  returns the lowest slot the game's capsule test accepts between the old and
  new position. That is the first entity in entity order, not the nearest along
  the ray. Write the test so a zero-length step selects an endpoint rather than
  producing NaN. Use `along_segment` with `ControlFlow` to collect every
  entity a piercing shot passes.
- **Burst.** `within(center, burst_radius, mask, ...)` with the game's distance
  test, as in the range section above.
- **Friendly-fire cone.** `within_sector(apex, aim, cos_half, range, ally_mask, |slot| ...)`.
  The closure keeps the game's cone and distance test and returns
  `ControlFlow::Break(())` at the first ally that blocks the shot.
- **Area box.** `within_box(probe, min, max, mask, ...)` for rectangular areas.

<!-- ignore-reason: statement fragment; regia_detsim is not a book-snippet crate, and the game's exact capsule test must stay in the closure -->
```rust,ignore
let from = rows[shooter].body;
let to = [from.pos[0] + vx * dt, from.pos[2] + vz * dt];
let hit = index.first_along_segment(from, to, bullet_radius, enemy_mask, |slot| {
    // This game's exact test, unchanged; clamp t so a zero-length step is a point.
    capsule_hits(from, to, bullet_radius, &rows[slot])
});
```

For lag compensation, keep the game's own history rows. To rewind a shot to
tick T, build a second `DetSpatial` from tick T's rows with `build_from(&history[t],
|i, h| SpatialRow { slot: i, .. })` and query that index. One build per distinct
rewind tick per tick is the whole cost. Cache it by tick in a small array, so
twenty shots that rewind to the same tick share one build. Regia stores no
history of its own.

Entities only: walls and terrain are not in this index. A line-of-sight check
combines the world-geometry query with the entity query. The shot hits the
first entity only when that entity is closer than the first wall.

## Is my tick inside budget?

A lockstep server has to finish every step before the next one is due. Measure
the step with your own systems: the tick profiler ([Measuring a lockstep
tick](#measuring-a-lockstep-tick)) gives one row per system and the whole-tick
p99. Compare that p99 against a pass line for the hardware it runs on.

Regia's AI-on soak gives the engine's share of a busy tick: 900 units in 4
teams with range queries, a scored target pick, nearest allies, keep-out
separation, shots swept along their paths, bursts, ID recycling and contacts.
It is a synthetic workload, not your game. Use it to see whether the engine
primitives leave room for your game's own work.

```sh
cargo run --release -p regia_detsim --example scale_soak -- --ai --verify
cargo run --release -p regia_detsim --example scale_soak -- --ai --pass-line-ns 8300000
```

Pass lines are settings, not engine limits. These are the defaults the
evidence uses:

| Where the step runs | Whole-tick p99 pass line | Flag |
|---|---|---|
| Desktop or server CPU | 4.0 ms | `--pass-line-ns 4000000` (the default) |
| Handheld PC or phone | 8.3 ms | `--pass-line-ns 8300000` |

The report's `status` is `inside` when the whole-tick p99 is at or below the
line, and `over` otherwise. A row in `systems` shows which system took the time.
Run release builds only, record the host load, and run `--curve 300,600,900,1500`
to see how the cost grows with the unit count. `--long --ticks 216000` (one hour
at 60 Hz) reports memory and allocation growth. See [Deterministic scale
soak](../agents/det-scale-soak.md#ai-on-soak) for every option and field.

## Bodies that push each other

Hulls, walkers, vehicles and crates that bump into each other need the same answer on every peer.
Give each body a `ContactBody` (its mass, bounciness, friction and a push cap) next to the
broadphase body, and run one contact pass per tick after you move the bodies:

<!-- ignore-reason: regia_detsim is not a dependency of the book snippet crate; the doctest of regia_detsim::contact runs these calls -->
```rust,ignore
use regia_detsim::broadphase::{BroadphaseConfig, DetBroadphase};
use regia_detsim::contact::{self, ContactBody, ContactConfig};

let mut grid = DetBroadphase::new(BroadphaseConfig::default());
let props = vec![ContactBody::with_mass(1200.0), ContactBody::STATIC /* a wall post */];
let cfg = ContactConfig { ram_threshold_m_s: 8.0, ..ContactConfig::default() };
let mut rams = Vec::new();
// each tick, after moving the bodies:
rams.clear();
let stats = contact::resolve_all(&mut grid, &mut bodies, &props, &mut velocities, &cfg, &mut rams)?;
for ram in &rams {
    apply_ram_damage(ram.a, ram.b, ram.closing_speed_m_s);
}
```

Heavy bodies push light ones aside; a body with mass 0 (`ContactBody::STATIC`) never moves, so use it
for posts and for bodies your game moves itself. Bodies on different floors (a deck over a road) do
not touch. A pair that closes faster than the ram threshold comes back as a `Ram`, in the same order
on every machine, for your damage rules. Restitution above 1 is refused, so contacts never add
energy. Details: [Contact response](../agents/det-ids-broadphase.md#contact-response).
## Fog of war: explored ground, ghosts, hills and stealth

`regia_detsim::fog` answers what each team sees, as a pure function of the sim state (the full guide
is the [Fog of war](../agents/fog-of-war.md) page). Four things a strategy game usually wants are
data or a small value away:

- **Explored and visible ground.** Keep one `FogGrid` per viewing team and call
  `grid.update(&vision, &los)` each frame: `state_at(x, z)` reads `Unexplored`, `Explored` or
  `Visible` for a minimap or a fog overlay.
- **Last-seen structures.** Mark a kind `KindSight::new("depot", 100.0).remembered()` and keep a
  `FogMemory`: a depot that leaves sight stays as a ghost at its last-seen spot until a watcher sees
  that spot empty. A ghost holds only what was seen.
- **Hills.** Plug `regia_nav::det::fog_los::GridLineOfSight::new(nav_static)` in as the line of
  sight: ground between two units hides them from each other, and a unit on the crest sees both
  valleys. Give watchtowers an eye height (`SightTable::building_eye_m`) to see over ridges, or turn
  on `FogRules::elevation` so low ground sees less of high ground.
- **Cloaked units.** `KindSight::new("cloak", 150.0).stealthed()` shows only inside a detector's
  radius (`KindSight::new("detector_van", 150.0).detector(80.0)`).

With the default rules (no kind traits, no elevation rule, eye heights 0, open ground) every answer
is the plain radius test, so a game that moved its fog onto Regia keeps its results. With hundreds of
watchers per team the fog files them in a spatial index; the answers stay the same and `fog.status`
reports the work (`queries`, `index_candidates`, `los_calls`).

Fog in a lockstep game is presentation only: every peer steps the full state, so a modified client
can read every unit. A client/server game that must not send hidden units to a client uses the fog
relevance filter on the server (`regia_framework::fog_relevance`; [Fog of
war](../agents/fog-of-war.md) § "Fog over the network").
### Showing the fog

Regia draws the fog for you once the view has a grid: `FogViewSim::with_grid(FogGrid::covering(..))`
plus `FogPresentationPlugin` keep a per-viewer fog texture (unexplored, explored, visible) that
uploads only the rows that changed and fades cells in and out at any frame rate. Register the world
overlay with `renderer.enable_fog_overlay(&handle)` and turn it on with the `FogLook` resource
(strength, explored tint, edge softness); put a `RadarFog` on a minimap to black out the same cells
there. In the editor, Show ▸ Fog of war draws the watchers' sight circles, shows the texture and
lets you switch the viewing team. Details: [Fog of war](../agents/fog-of-war.md#showing-the-fog).

## Hover hulls in an engine-native game

If your game runs on the engine's ECS rather than its own lockstep sim, you do not need a fleet of your own:
add `DetHoverPlugin` and spawn hulls as entities. The plugin steps every hull once per fixed tick, in the
order of the ids you give them, writes their `Transform`, and leans them with their speed and steering.

<!-- ignore-reason: the module doctest of regia_framework::det_hover spawns and steps a hull like this -->
```rust,ignore
use regia_framework::det_hover::*;

app.add_plugins(DetHoverPlugin);
// Fly over the level's terrain (client builds), or set your own floor: DetHoverFloor::new(your_sampler).
app.world_mut().resource_mut::<DetHoverConfig>().follow_terrain = true;

commands.spawn((
    DetHoverHull::DEFAULT,               // or DetHoverHull::new(params, limits, FlightMode::AirTime)
    DetHoverId(player_slot),             // the order hulls step in: give every hull its own
    DetHoverState::at_rest([x, ground, z], yaw, &DetHoverHull::DEFAULT.params),
    Transform::default(),
));

// In FixedUpdate, before DetHoverSet::Step: your controls into the hull's input.
fn drive(mut hulls: Query<&mut DetHoverInput, With<Player>>, controls: Res<Controls>) {
    for mut input in &mut hulls {
        *input = DetHoverInput { throttle: controls.forward, steer: controls.turn, hop: controls.hop, ..default() };
    }
}
```

The fixed time step is `DetHoverConfig::dt`, never the frame time, so the hulls fly the same at 30 and at
144 frames per second. Each tick is a pure function of the hull's state and input (`tick_hull`), so a client
can predict its own hull and replay it after a correction. For a lockstep match, step `regia_detsim`'s hover in
your sim instead: this plugin runs inside the ECS and is not checked by the determinism scan.

To tune a hull, ask the agent: `detsim.hover_probe { "script": "accel" }` answers its top speed and time to
90 % of it, and `brake`, `turn`, `hop` and `crest` answer the stop distance, turn radius, hop height and air
time over a crest. Turn on the `det_hover` gizmo group to see each hull's ride height, the ground samples and its
tilt. Details: [Hover step (lockstep), Hover hulls in an ECS app](../agents/det-hover.md#hover-hulls-in-an-ecs-app).
## Hover hull files

A hull an artist or a mod ships is a `.hoverhull.ron` (version 1). Load it on the host with
`regia_framework::det_hover::HoverHullAsset::from_ron`, which fills every missing number from
`HoverParams::DEFAULT_HULL` (and from `HoverLimits::DEFAULT` when the file has a limits block) and
rejects a bad field with the same codes as `HoverParams::validate`. The step still takes the
resolved `HoverParams`; it does not read the file.

The file text is not a match setting. After the host parse, compare the hull handshake,
`HoverHullParts::handshake_bytes(host_limits, host_flight)` (273 bytes: the `params` raw `f32` bits,
the flight-mode byte and the `limits` raw `f32` bits, `-0.0` kept), in the settings handshake.
Two peers that parsed one float token differently, or whose files differ in `limits` or `flight`,
then disagree on the bytes and must not start (`hull_handshake_mismatch` names the field). An agent
checks a file with `detsim.hover_hull_check` and reads `handshake_hex` for that comparison
(`bytes_hex` is the `params` part only). Details:
[Hover step (lockstep), Hull files](../agents/det-hover.md#hull-files).


## Thinking less: cadence and importance

Most of an AI's cost is how often each unit thinks. `regia_detsim::ai::AiCadence` lets you say that per
kind of thought and per unit, and keeps every peer of a lockstep match in agreement.

**Channels.** Every cadence has `decide` (10 ticks) and `sight` (6 ticks). Add a channel for each other
kind of thought with its own period: target picks every 4 ticks, route refreshes every 30, squad orders
every 60. Each stage of your tick asks the core for its slice:

<!-- ignore-reason: `units`, `world_seed` and `refresh_route` are the game's own -->
```rust,ignore
let mut cadence = AiCadence::default();
let route = cadence.add_channel(
    CadenceChannel::new("route", 30).with_phase(PhaseRule::Hashed { salt: 1 }),
)?;
let mut ai = AiCore::new(world_seed, cadence, ArgMax);
let mut due = Vec::new();
ai.due_units(route, tick, &units, &mut due)?; // indices into `units`, ascending entity id
for &i in &due {
    refresh_route(&units[i]);
}
```

**Spreading.** The default phase is `entity_id % period`. If your ids come in steps (one squad numbered
0, 10, 20, ...), they would all think on the same tick; `PhaseRule::Hashed { salt }` spreads them out. Use
a different salt per channel so a unit's route and target ticks do not line up.

**Importance tiers.** Give units that matter less a tier that multiplies their periods:
`cadence.with_tiers(vec![CadenceTier::new(1), CadenceTier::new(2), CadenceTier::new(4)])` and return the
tier from `AiAgent::cadence_tier`. Decide the tier from sim state, for example the distance to the nearest
enemy bucketed into close, mid and far, never from the camera or the player's screen, or peers will
disagree. Powers of two work best: a unit that changes tier keeps its place in the rhythm and never waits
longer than one slow period.

**Caps.** `CadenceChannel::with_max_per_tick(Some(n))` limits a channel to `n` units per tick. The rest
wait their turn, oldest first, so a burst (a reinforcement wave spawned on one tick) spreads over the next
few ticks instead of spiking one. No unit waits longer than `ceil(units / n) - 1` extra ticks.

**What players notice.** Nothing, with the defaults: your old results stay bit for bit. Adding a channel
for work you already do on a schedule, slowing far units, or capping a channel changes when units think,
so it changes results: treat each such change as a sim-version bump (re-record your lockstep pins once,
and refuse older peers).

The agent guide has the API, the bounds and the error codes:
[Deterministic AI core](../agents/det-ai-core.md#cadence-channels-tiers-and-caps).

## Crowds in corridors

The default avoidance kernel (Stage A) projects each neighbor's constraint once,
never runs faster than the preferred speed, and knows no walls, so bodies can
squeeze into corners in corridors and chokes. For those scenes switch the crowd
to the linear-program solver:

```rust
use regia_detsim::avoid::{OrcaSettings, OrcaSolver};

fn crowd_settings() -> OrcaSettings {
    OrcaSettings {
        solver: OrcaSolver::Lp,
        // Allow speeding up to 1.8 m/s to dodge; 0 keeps the preferred speed as the cap.
        max_speed_mm_s: 1_800,
        ..OrcaSettings::default()
    }
}
# fn main() { let _ = crowd_settings(); }
```

1. Give each body its nearby walls as `OrcaSegment { a_mm, b_mm }` (straight runs
   in millimeters). From the det grid, `regia_nav::det::walls::
   segments_from_grid_window(&grid, profile, center, radius_m, &mut segments)`
   builds them from the blocked-cell boundary around the body; reuse the vector.
2. Call `orca_velocity_obstacles_into(&mut scratch, &body, preferred, &neighbors,
   &segments, &settings)` for every body against the same snapshot, then apply
   the velocities.
3. Read `scratch.last_lp()` to see whether every constraint held (`feasible`) or
   the solver had to accept the least overlap (`worst_violation_mm_s`).

Walls are hard: in the corridor and choke tests (64 bodies, 600 ticks) no body
center ever crossed a wall. Bodies may still touch each other briefly in a
dense counterflow. The solver is integer-only, so every peer computes the same
velocities, but it costs several times Stage A per body: profile your crowd step.
Every peer must use the same settings; switching the solver changes the
simulation, so treat it like any other sim-version change.

## Finding the first tick of a desync

When two peers' checksums split, the per-system desync hash says which system split and on which
tick ([Lockstep desync hash](../agents/desync-hash.md)). Three things can make that tick later than
the real cause, or hide it; each has a switch.

1. **A zero sign or a NaN payload.** The wire rule treats `-0.0` and `+0.0` as equal, and every NaN
   as one value, so peers that agree on every value never report a false split. A sign that differs
   is then invisible until it changes a value (`1.0 / -0.0` is `-inf`), possibly many ticks later.
   Turn on the raw diagnose ring while you hunt the bug:
   - ECS game: `DesyncHashPlugin::new(SYSTEMS)...diagnose_raw()`.
   - Your own fold: `SystemRing::with_raw()` and `regia_detsim::hash::diagnose`.
   - External sim behind the sim host: `ThreadedSimHost::start_with_desync::<N>(sim, settings, true)`.

   The report keeps the normal answer and adds a `float_bits` entry (and a
   `lockstep desync raw <system> tick <T> ...` log line) for every system whose raw bits split first.
   It costs a second fold per tick and twice the ring memory, so leave it off in shipped matches. The
   words peers exchange do not change.
2. **A `Serialize` impl that fails.** The hash walks your state through `serde`; an impl that
   returns an error leaves a word that covers only part of the state. Debug builds assert on it in
   `system_hashes`; the ring flags the row instead (`partial`), logs one warning, and the report
   says `partial_from` with a `lockstep desync partial from tick <T>` line. A partial word can miss a
   real split, so the report never calls such a tick agreement: `report.same` is `false`,
   `report.verdict` is `partial` (unless a split is named), and `last_agreed` stays below the partial
   tick. Fix the impl.
3. **Rows that scrolled out.** The ring keeps 240 ticks (4 s at 60 Hz). Ask for the window around
   the split tick soon after it happens (`sim.desync_rows { to: <tick> }`).

With the agent protocol: step both runs to the same tick, call `sim.desync_rows {}` on one, and pass
its `rows` as `against` to the other. `report.same` is `true` only when every view agrees;
`report.verdict` (`canonical`, `float_bits`, `partial` or `no_overlap`) says which one did not.
`report.first_tick` is the canonical answer;
`report.float_bits[].raw_tick` is the earlier tick when the split began as a zero sign. A sim that is
not an ECS world answers the same method through its sim host (`source: "sim_host"`); see
[Lockstep desync hash for ECS games](../agents/desync-hash-ecs.md).

## Authoring a utility catalogue

The lockstep AI scores a list of options (push, hold, forage, whatever your game needs) from a list of
normalized inputs your sim computes each tick (health left, distance to the threat, hunger). You write that
list as a `.detutility.ron` file and load it; nothing about the engine changes when you add an option or an
input.

<!-- ignore-reason: a sketch of a host's AI stage; `text`, `units` and `inputs_for` come from the game -->
```rust,ignore
use regia_detsim::ai::{AiCadence, AiCore, ArgMaxIdx, UtilityCatalogue, score_options_idx};

let catalogue = UtilityCatalogue::from_ron_str(&text)?; // refused with a named error if it is wrong
let mut ai = AiCore::new(world_seed, AiCadence::default(), ArgMaxIdx);
// Each tick:
ai.decide(tick, &units, |unit, rng, out| {
    let values = inputs_for(unit, rng); // one f32 per input, in the file's input order
    score_options_idx(&catalogue, &values, |_| true, |_| 1.0, out);
});
let name = ai.decision_for(unit_id).and_then(|i| catalogue.option_name(i));
```

Each option lists its considerations: an input, a curve (`Linear`, `Quadratic`, `InverseQuadratic`,
`Logistic`, `InverseLogistic`, `Plateau`, `Band`), a weight, and whether it multiplies in. Additive
considerations average by weight; a multiplier at zero zeroes the option, so "never push with an empty
magazine" is one line. The full schema and the checks are in `docs/formats/ai-det-utility.md`.

**Order matters.** The position of an option in the file is its index, and when two options score the same
the later one wins. The position of an input is where your sim writes its value. Moving either changes what
units decide, so add new options and inputs at the end, and treat any edit to a shipped catalogue as a
sim-version change: every peer must load the same file.

**Limits.** 64 options (one 64-bit repertoire mask), 256 inputs, 32 considerations per option.

**Mistakes are caught at load and at cook.** An input name that is not listed, two options with one name, a
`NaN` in a curve, a misspelled field, or a file from a newer engine is refused with a named error
(`det_utility_unknown_input`, `det_utility_duplicate_name`, `det_utility_non_finite`, `det_utility_parse`,
`det_utility_newer_version`), and the asset cook fails on the same file.

**The built-in catalogue.** `UtilityCatalogue::do27_preset(&profile.thresholds)` is the built-in twelve-option
catalogue as data, scoring the same bits as before. `preset.to_ron()` writes it out as a starting point for
your own.

The agent guide has the API, the caps and the error codes:
[Deterministic AI core](../agents/det-ai-core.md#your-own-catalogue).

## Ground vehicles

For a lockstep game with tanks or cars, `regia_detsim::ground_vehicle` has two kinematic models:
`tracked` (skid-steer: two track throttles, spin in place with `pivot_turn`) and `wheeled` (a
bicycle model: throttle, brake, steer and handbrake, with a grip limit on how hard it can turn at
speed). Both read your floor, stop on slopes steeper than their grade limit, roll back down a slope
when coasting, and move the vehicle themselves.

<!-- ignore-reason: regia_detsim is not a dependency of the book snippet crate; the module doctest of regia_detsim::ground_vehicle runs a wheeled car like it -->
```rust,ignore
use regia_detsim::ground_vehicle::wheeled::{self, WheeledInput, WheeledParams, WheeledState};
use regia_detsim::ground_vehicle::WorldPhysics;

let params = WheeledParams { max_speed_m_s: 25.0, ..WheeledParams::DEFAULT };
params.validate()?; // when you load it, not every tick
let mut car = WheeledState::at_rest([0.0, floor_height, 0.0], 0.0);

// Every tick:
let input = WheeledInput { throttle, brake, steer, handbrake };
let report = wheeled::step(&params, &WorldPhysics::EARTH, &floor, &mut car, &input, dt);
let pos = car.pos; // where to draw the car; car.pitch and car.roll tilt it with the ground
```

The nose points along −Z at yaw 0 and a positive steer turns right. These are not physical
vehicles: no tires, gears or suspension. For those use the rigid-body vehicles (they are not
lockstep). The existing skid-steer agent method `physics.track_step` now runs on the tracked model.
Details, the parameter tables and the agent method `detsim.ground_vehicle_simulate`:
[Ground vehicles (lockstep)](../agents/det-ground-vehicles.md).

## Which movement model?

A lockstep simulation reads only the steps and queries of the lockstep closure (`regia_detsim`,
`regia_world_geom`, `regia_nav::det`, `regia_rules`, `regia_net::lockstep`), never a rapier pose,
contact or event (ADR-8841). Hover hulls, ground vehicles, projectiles, bodies that push each other
and the world's gravity have lockstep models; rigid bodies, ragdolls, cloth and destruction are
presentation-only in a lockstep game. The table per model and per genre, with each model's agent
methods and determinism guarantee, is in [Movement models](movement-models.md).

## Choosing cell size, world bounds and large radius

The lockstep broadphase (`DetBroadphase`) and the query index (`DetSpatial`) share one
`BroadphaseConfig`. None of its settings change which contacts or query results you get, only how
much work finding them takes:

- **`cell_size`**: about the diameter of a typical body (4 m by default). The grid widens its cells
  on its own when the bodies spread wider than `max_cells` allows.
- **`world_bounds`**: set it to your play area, `[min_x, min_z, max_x, max_z]` in metres. Bodies
  outside it (pooled entities parked far away, a unit flung off the map) are then handled one by one
  instead of stretching the grid until every live body shares a few cells. Leave it `None` if
  nothing ever leaves the map.
- **`large_radius`**: bodies bigger than this (twice `cell_size` by default) are tested one by one
  instead of widening every other body's search. Raise it if many of your bodies are that big; set
  `Some(f32::INFINITY)` to turn it off.

`DetBroadphase::stats()` reports `outliers` and `large` for each pass, beside `candidates` (the pairs
it tested): if `candidates` climbs while your bodies have not crowded together, look at those two.
Details: [Outliers and large bodies](../agents/det-ids-broadphase.md#outliers-and-large-bodies).
