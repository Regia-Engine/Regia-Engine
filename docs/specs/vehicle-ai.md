# Vehicle AI: drivers for physical vehicles on lane graphs

Source of truth: ADR-3117 (`docs/decisions.md`). Agent page: `book/agents/vehicle-ai.md`. Rung: v0.6.0
(generic vehicle physics, `docs/versions.md`).

## 1. Scope

In scope: one AI driver per vehicle that follows a `NavLaneGraph` route on real vehicle physics; curve speeds;
car following; stop lines at the route end and at disabled lanes; recovery from being stuck; an agent surface.

Out of scope (later rungs or game code): lane changes and overtaking, junction priority and traffic lights,
racing lines, rear sensing while reversing, perception of non-vehicle obstacles (a static obstacle is handled
only by the stuck recovery), network replication of drivers (the server runs them; clients see the vehicle).

## 2. API (`regia_framework::vehicle_ai`)

- `VehicleDriver` (component): `new(params)`, `to(target)`, `drive_to(target)`, `stop()`, `set_params(params)`,
  `plan(graph, position, speed)` (plan now and see the error), readouts `state()`, `status()`, `route()`,
  `path()`, `planned_speeds()`, `curve_speeds()`, `path_length()`.
- `DriveTarget`: `Lane { lane, distance }`, `Point { point, reach }`, `Loop { lane }`.
- `VehicleDriverParams`: the tuning, `validate()` names the first bad field.
- `VehicleDriverStatus`: state, station, remaining, speed, target and curve speed, cross-track error, gap and
  leader, the written input, recoveries, failed recoveries, laps, steps, last error.
- `VehicleDriverState`: `idle`, `driving`, `following`, `blocked`, `arrived`, `recovering`, `stuck`.
- `VehicleAiError` with stable `code()`s `vehicle_ai_*`.
- `drive_vehicle_ai` (system) and `VehicleAiPlugin`: `FixedUpdate`, in `PhysicsStep`, after
  `sync_velocity_and_forces`, before `drive_vehicles`. The system writes only `VehicleInput`.
- Pure helpers for tests and tools: `menger_curvature`, `plan_speeds`, `pure_pursuit`, `idm_accel`,
  `idm_interaction`.

## 3. Algorithms

See ADR-3117 for the full statement: a uniform 1 m path from `route_points` with the lateral offset; curve speed
`min(limit, max_speed, sqrt(a_lat / κ))` with κ from the circle through points 3 m apart; backward and forward
passes; pure pursuit from the rear axle with a speed-scaled look-ahead; a PI pedal controller with feedforward and
conditional-integration anti-windup; the IDM interaction term `−a_max (s*/s)²` against the nearest vehicle ahead on
the path, found through a 32 m uniform grid rebuilt each step; a counter-based stuck detector with bounded
reverse-and-counter-steer recoveries.

## 4. Budgets

- Planning: O(route length) once per target (and on `set_params`); allocates the path arrays.
- Per step and driver: O(look-ahead window) for the cursor, O(cells in reach + neighbours × follow range /
  spacing) for the leader search, O(route lanes within 200 m) for closures. No allocation after warm-up
  (tested with 64 drivers and a counting allocator).
- Path: at most 20,000 points (`MAX_PATH_POINTS`).

## 5. Acceptance (`crates/regia_framework/tests/vehicle_ai.rs`, `vehicle_ai_alloc.rs`)

Fixed 60 Hz steps, deterministic inputs, no wall clock:

- A default car laps a closed oval (two 80 m straights, 25 m radius ends) with cross-track RMS ≤ 0.5 m and
  max ≤ 1.5 m, never more than 5 % over the planned curve speed, and stops within 1 m of the route end.
- A two-car platoon with the leader braking at its limit: the follower's gap stays above the IDM minimum gap and
  the chassis never touch.
- A car started nose-first against a wall recovers and finishes its route; a car boxed in gives up as `stuck`.
- A disabled lane stops the car in front of it until it reopens.
- Two runs give identical trajectories; the trajectory hash is pinned in `vehicle_ai_trajectory_pin`, which moves
  whenever the vehicle physics changes.
- 64 drivers allocate nothing per step after warm-up.
