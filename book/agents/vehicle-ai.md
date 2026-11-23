# AI drivers on lane graphs

An AI driver drives a physics vehicle along a lane graph the way a player would: it writes the vehicle's
`VehicleInput` (throttle, brake, steer) every fixed step and nothing else, so the car skids, understeers and
collides under the same physics as the player's car, with the simple or the geared drivetrain. Design:
ADR-3117; spec: `docs/specs/vehicle-ai.md`.

The app needs `PhysicsPlugin`, `VehiclePlugin`, `regia_framework::vehicle_ai::VehicleAiPlugin` and a
`regia_nav::NavLaneGraph` resource (see [navigation lanes](navigation-lanes.md)). Vehicles come from
`regia_physics::vehicle::spawn_vehicle` (see [vehicles](vehicles.md)).

## What a driver does

- **Route.** `vehicle_ai.drive_to` takes one target: a `lane` and a `distance` along it, a world `point` snapped to
  the nearest open lane within `reach`, or a `loop` through a lane (the shortest cycle, lapped forever). The route
  is the lane graph's quickest one from the lane under the car. A target behind the car on its own lane goes once
  around through the lane's successors.
- **Steering.** Pure pursuit from the rear axle toward a point a look-ahead distance along the path; the look-ahead
  grows with speed. A goal behind the car gives full lock toward it.
- **Speed.** A plan made once per route: the curve speed `sqrt(max_lateral_accel / curvature)` capped by the
  lane's speed limit and `max_speed`, slowed ahead of curves and stops at `max_decel`, sped up at `max_accel`.
- **Following.** The nearest vehicle ahead on the driver's own path slows it with the intelligent driver model
  (time headway `time_headway`, minimum gap `min_gap`). The state reads `following`.
- **Stops.** The route end is a stop line (state `arrived`), and so is the start of a disabled lane on the route
  (`nav` lane closures, `NavLaneGraph::set_lane_enabled`): the car stops in front of it (state `blocked`) and
  drives on when it reopens.
- **Stuck recovery.** Counted in fixed steps: throttle asked for `stuck_steps` steps without progress starts a
  recovery, reversing with the opposite steer for `reverse_steps` steps (state `recovering`). A recovery fails
  when the car is stuck again before it advanced `recovery_progress` metres; after `max_recoveries` failures in a
  row the driver holds the brake in state `stuck` until it gets a new target. A geared car with a manual gearbox
  cannot reverse through its input, so it goes to `stuck` at once.

## Methods

`vehicle_ai.drive_to { entity, lane?, distance?, point?, reach?, loop? }` gives the vehicle a driver (if it has
none) and a target, and plans now. Exactly one of `lane`, `point` and `loop`:

```json
{ "entity": 4294967310, "lane": 2, "distance": 40.0 }
→ { "entity": 4294967310, "state": "driving", "target": { "lane": 2, "distance": 40.0 },
    "route": [0, 1, 2], "path_length": 165.0, "path_points": 166, "remaining": null, ... }
```

`vehicle_ai.status { entity? }` lists every driver (or one): `state` (`idle`, `driving`, `following`, `blocked`,
`arrived`, `recovering`, `stuck`), `target`, `route`, `path_length`, `station` (metres along the path),
`remaining` (to the stop line, null on a loop with nothing closed ahead), `speed`, `target_speed`,
`curve_speed`, `cross_track` (metres from the path, positive right of it), `gap` and `leader` (bumper to bumper,
the vehicle ahead), `input` (what it wrote), `recoveries`, `failed_recoveries`, `laps`, `steps` and `last_error`.
Verify a drive with `play.step` and then `vehicle_ai.status`: `arrived` with a small `remaining`.

`vehicle_ai.set_params { entity, ... }` changes the tuning: any of `flags`, `lateral_offset`, `snap_reach`,
`max_speed`, `max_lateral_accel`, `max_accel`, `max_decel`, `emergency_decel`, `look_ahead_min`,
`look_ahead_time`, `look_ahead_max`, `speed_preview`, `speed_gain`, `speed_integral`, `time_headway`, `min_gap`,
`idm_exponent`, `comfort_decel`, `follow_range`, `arrive_distance`, `stuck_steps`, `stuck_speed`,
`reverse_steps`, `reverse_throttle`, `max_recoveries`, `recovery_progress`. The whole set is validated; on an
error nothing changes. A driver with a target replans on the next step.

`vehicle_ai.stop { entity }` drops the target: the driver brakes to a stop and holds the brake (`idle`). To hand
the car back to `physics.vehicle_input`, remove the `VehicleDriver` component.

## Error codes

`data.code` names the reason; the message starts with it in brackets.

| Code | When |
|---|---|
| `vehicle_ai_not_a_vehicle` | `entity` has no `VehicleController`. |
| `vehicle_ai_no_driver` | `vehicle_ai.status` or `vehicle_ai.stop` on a vehicle with no driver. |
| `vehicle_ai_no_lane_graph` | There is no `NavLaneGraph` resource. |
| `vehicle_ai_unknown_lane` | `lane` or `loop` names a lane that does not exist. |
| `vehicle_ai_no_route` | No open lane leads from the car's lane to the target (or no cycle through the lane). |
| `vehicle_ai_off_lanes` | The car or the target point is farther than the snap reach from every open lane. |
| `vehicle_ai_route_too_long` | The path would hold more than 20,000 points (20 km, or a 10 km lap). |
| `vehicle_ai_bad_param` | A parameter is missing, of the wrong type, unknown or out of range. |
| `vehicle_ai_no_chassis` | The vehicle's chassis is not in the physics world. |

## In code

<!-- ignore-reason: a fragment of the game's setup: `app`, `lane_graph` and the spawn pose `at`, `rotation` (a `Quat`) are the game's own -->
```rust,ignore
use regia_framework::vehicle_ai::{DriveTarget, VehicleAiPlugin, VehicleDriver};

app.add_plugins((PhysicsPlugin, VehiclePlugin, VehicleAiPlugin));
app.insert_resource(lane_graph);
let car = spawn_vehicle(app.world_mut(), VehicleConfig::default(), at, rotation)?;
app.world_mut().entity_mut(car).insert(VehicleDriver::to(DriveTarget::Loop { lane: 0 }));
```

The driver system runs in `FixedUpdate` inside `PhysicsStep`, after the velocity sync and before
`drive_vehicles`, and visits vehicles in spawn order, so a run replays exactly.

## Limits

No lane changes or overtaking, and no junction priority: two routes that cross do not yield to each other. The
leader must be on the driver's own path; a car merging from the side counts once it is on it. A recovery reverses
without looking behind.
