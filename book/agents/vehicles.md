# Vehicles: wheel state, driver input, reset

Read, drive and reset the ray-cast vehicles in a running app (`regia_physics::vehicle`). Each
vehicle is an entity with a `VehicleController` (the chassis body and its wheels) and a
`VehicleInput`. These calls need `regia_physics::PhysicsPlugin` and
`regia_physics::vehicle::VehiclePlugin`. Without a physics world, or for an entity that is not a
vehicle, they return `NOT_FOUND` (`F0003`) with a hint.

Axes follow the engine: the chassis' forward is its local −Z, right is +X, up is +Y. Distances are
meters, angles radians, speeds m/s.

## Which vehicle?

These vehicles are physical: rapier chassis with tires, a drivetrain and suspension (and the hover craft
below). They give the same bits on the same build and CPU architecture, which suits servers, replays and
single-player, but not a lockstep simulation, which must use the kinematic models of
`regia_detsim::ground_vehicle` (tracked and wheeled, the same bits on every architecture;
[Ground vehicles (lockstep)](det-ground-vehicles.md), `detsim.ground_vehicle_simulate`) or the lockstep hover
([Hover step (lockstep)](det-hover.md)). A lockstep simulation never reads a rapier pose (ADR-8841). The table
per genre is in [Movement models](../user-guide/movement-models.md).

## Read every vehicle

`physics.vehicles` lists the vehicles in entity order. `entity` limits it to one.

```json
{}
→ { "count": 1, "vehicles": [ {
      "entity": 4294967310, "position": [0, 0.823, -14.72], "rotation": [0, 0, 0, 1],
      "forward": [0, 0, -1], "forward_speed": 9.7, "speed": 9.7, "rpm": 0.0, "gear": 0,
      "linear_velocity": [0, -0.06, -9.7],
      "angular_velocity": [0, 0, 0], "grounded_wheels": 4, "wheel_count": 4,
      "input":   { "throttle": 1.0, "brake": 0.0, "steer": 0.0 },
      "applied": { "throttle": 1.0, "brake": 0.0, "steer": 0.0 },
      "steps": 420, "last_error": null, "error": null,
      "wheels": [ { "index": 0, "grounded": true, "compression": 0.077, "suspension_length": 0.273,
                    "steer_angle": 0.0, "spin_angle": 41.2, "center": [-0.8, 0.35, -16.0],
                    "contact_point": [-0.8, 0.0, -16.0], "contact_normal": [0, 1, 0],
                    "ground_entity": 4294967296, "suspension_force": 2943.0,
                    "slip_ratio": 0.0, "slip_angle": 0.0 }, ... ] } ] }
```

- `forward_speed` is the chassis velocity along `forward`, negative when reversing. `speed` is the
  chassis speed in its own plane, never negative.
- `rpm` and `gear` are the geared drivetrain's (below); a `Simple` car reads `0` for both.
- `slip_ratio` is `(ω·r − v) / max(|v|, 1 m/s)`: positive driving, negative braking, `0` for a `Simple`
  car and in the air. `slip_angle` is `atan(v_side / |v_along|)` of the contact against the ground in
  radians, positive when it moves toward the wheel's right, `0` in the air or below 0.1 m/s.
- `input` is the entity's `VehicleInput` now; `applied` is what the last fixed step drove with
  (after clamping). They differ until the next fixed step runs.
- `compression` is `rest length − suspension_length`: positive when compressed, `0` in the air. A
  car with `n` equal wheels resting on flat ground compresses each by about `g / (n · stiffness)`.
- `grounded: false` means the wheel's ray found nothing within `rest length + radius`. Its
  contact fields are `null` and it pushes with no force.
- `steer_angle` is the wheel's rotation about the chassis' +Y: a right turn reads negative.
  `spin_angle` grows while the wheel rolls forward.
- `center` is the wheel center in world space, posed against the current chassis pose.
- `ground_entity` is `null` when the ground collider belongs to no entity.
- `linear_velocity` is rapier's end-of-step velocity. A car resting on its springs reads about
  −0.06 m/s vertically while its position holds still: the suspension impulse lands once per step
  and gravity acts across the solver's substeps. Check rest with positions over a few steps.

`config_digest` is `VehicleConfig::digest` of the configuration the vehicle runs (FNV-1a of its compact
RON, 16 hex digits) and `config_revision` counts `Vehicle::set_config` calls since spawn: a retune from the
Vehicle Tuning panel (below) shows up as a new digest and a revision one higher.

`steps` counts fixed-step updates since spawn or the last reset. `last_error` is the code from the
last fixed-step update; `error` is the code from reading the vehicle for this report. Both are
`null` when all is well.

### Error codes in the report

| Code | Meaning |
|---|---|
| `vehicle_chassis_missing` | The chassis body is gone from the physics world (despawned or removed). |
| `vehicle_chassis_not_dynamic` | The chassis body was switched to fixed or kinematic; wheels cannot move it. |
| `vehicle_chassis_massless` | The chassis body has no mass (no collider with mass). |
| `vehicle_invalid_time_step` | The fixed step was zero, negative or not finite. |
| `vehicle_invalid_pose` | A reset pose was not finite, or its rotation was degenerate. |
| `vehicle_no_wheels` | The vehicle's configuration has no wheels. |
| `vehicle_too_many_wheels` | The configuration has more than 32 wheels. |
| `vehicle_invalid_wheel` | A wheel field is out of range or not finite (every wheel setting is at most `1e9`, `MAX_VEHICLE_SETTING`, ADR-7431). |
| `vehicle_invalid_config` | A vehicle-wide field is out of range or not finite (drivetrain, anti-roll and aero settings at most `1e9`). |
| `vehicle_no_physics_world` | The app has no `PhysicsWorld` resource. |
| `vehicle_no_gearbox` | `physics.vehicle_shift` on a car with the `Simple` drivetrain. |
| `vehicle_invalid_gear` | `physics.vehicle_shift` named a gear the gearbox does not have. |
| `vehicle_tire_damper` | `physics.vehicle_tire_curve` on a wheel with the `Damper` tire, which has no slip curve. |
| `vehicle_tire_no_wheel` | `physics.vehicle_tire_curve` named a wheel the vehicle does not have. |
| `vehicle_state_not_finite` | The car's own state (chassis velocity, wheel or drivetrain state) is not finite, so a client prediction world could not carry it into a rebuilt world; it keeps the old world until the next authoritative state (logged as `net.vehicle_statics_sync_failed`). |

The configuration codes come from creating a vehicle in code (`spawn_vehicle`). Over the wire you
see the chassis and time-step codes in `last_error` and `error`. A refused `physics.vehicle_reset`
names its code in brackets at the start of the message, for example
`[vehicle_chassis_missing] the vehicle's chassis body is not in the physics world` (`F0003`).

## Drive

`physics.vehicle_input` sets the driver input. Fields you leave out keep their value. The next
fixed steps drive with it until you change it.

```json
{ "entity": 4294967310, "throttle": 1.0, "steer": -0.5 }
→ { "entity": 4294967310,
    "before": { "throttle": 0.0, "brake": 0.0, "steer": 0.0 },
    "after":  { "throttle": 1.0, "brake": 0.0, "steer": -0.5 } }
```

- `throttle`: +1 full forward, −1 full reverse, 0 off.
- `brake`: 0 off to 1 full. On a wheel that can brake, braking cancels its engine force, so
  throttle and brake together hold the car.
- `steer`: +1 full right, −1 full left.

A value outside its range is refused with `F0002` before the call runs. This call needs the `play`
permission.

Drive, then check:

```json
{ "method": "physics.vehicle_input", "params": { "entity": 4294967310, "throttle": 1.0 } }
{ "method": "play.step", "params": { "frames": 120 } }
{ "method": "physics.vehicles", "params": { "entity": 4294967310 } }
```

## Reset

`physics.vehicle_reset` puts the vehicle back at rest: zero velocity, no accumulated forces, the
wheels cleared (spin, steering, suspension record) and the input released. With no pose it returns
to its spawn pose. `position` sets the chassis center; `yaw` sets the heading in radians about +Y.
`position` without `yaw` resets to no rotation.

```json
{ "entity": 4294967310 }
→ { "entity": 4294967310, "position": [0, 0.9, 0], "rotation": [0, 0, 0, 1] }

{ "entity": 4294967310, "position": [5, 1, 0], "yaw": 1.5708 }
→ { "entity": 4294967310, "position": [5, 1, 0], "rotation": [0, 0.7071, 0, 0.7071] }
```

After a reset no wheel is grounded until the next fixed step casts the wheel rays. This call needs
the `play` permission.

## Working with the results

- Entity ids work everywhere: `scene.get_entity` for the chassis `Transform`, `play.expect`
  conditions, `physics.raycast` from a wheel `center`.
- The chassis `Transform` is the stepped pose; the wheel poses are posed against it, so a host can
  parent wheel meshes under the chassis with each wheel's chassis-space pose (see
  `book/user-guide/vehicles.md`).
- Same build, same inputs, same fixed step: same result. `physics.stats` `digest` compares runs.

## Drivetrain telemetry and gears

A car with `drivetrain: Geared(..)` in its `.vehicle.ron` (`docs/formats/vehicle.md`) has an engine, a slip
clutch, a gearbox, differentials and a spin per wheel. `physics.vehicle_telemetry` reads them; `entity` limits it to
one vehicle.

```json
{ "entity": 4294967310 }
→ { "count": 1, "vehicles": [ {
      "entity": 4294967310, "steps": 420, "digest": "6f1c0b9d2a33e710", "drivetrain": "geared",
      "speed": 21.4, "forward_speed": 21.4, "rpm": 4310.0, "gear": 2, "target_gear": 2,
      "shifting": false, "automatic": true, "upshifts": 1, "downshifts": 0, "last_shift_rpm": 6204.0,
      "clutch_torque": 287.1, "clutch_slipping": false, "engine_torque": 287.1,
      "applied_throttle": 1.0, "traction_factor": 1.0, "error": null,
      "wheels": [ { "index": 2, "grounded": true, "slip_ratio": 0.031, "slip_angle": 0.0,
                    "angular_speed": 63.0, "tire_force": 2710.0, "load": 3120.0,
                    "drive_torque": 985.0, "abs_factor": 1.0 }, ... ] } ] }
```

- `gear`: −1 reverse, 0 neutral, 1 and up forward. A shift keeps the old gear for `shift_time` with the clutch open and
  the throttle shut; `target_gear` names the new one and `shifting` is `true` until it engages.
- `rpm` follows the driven wheels through the ratios while `clutch_slipping` is `false`. At a launch it runs ahead.
- `traction_factor` is traction control's cut on the throttle, `abs_factor` is ABS's on that wheel's brake: `1` is no
  cut. Both stay `1` with the aid off.
- `digest` covers the drivetrain's state and the step count. With `physics.stats` `digest`, which covers the chassis, it
  compares runs: same build, same inputs and fixed step, same digests.
- A `Simple` car reads `"drivetrain": "simple"`, `rpm` and `gear` `0`, `null` per-wheel spin and force, and has none
  of the gearbox fields.

`physics.vehicle_shift` changes gear. It takes the gears from the automatic until you hand them back. It needs the `play`
permission.

```json
{ "entity": 4294967310, "gear": 3 }       // -1 reverse, 0 neutral, 1 and up forward
{ "entity": 4294967310, "shift": "up" }   // or "down"
{ "entity": 4294967310, "automatic": true }
→ { "entity": 4294967310, "gear": 2, "target_gear": 3, "shifting": true, "automatic": false }
```

`gear` and `shift` together are refused, and so is a call with none of the three (`F0002`). A gear the gearbox does not
have is `F0002` with `[vehicle_invalid_gear]`, and a `Simple` car is `F0002` with `[vehicle_no_gearbox]`.

With a geared car, `throttle` follows the gear. A throttle against the direction of travel brakes; near a stop (under
1 m/s) the automatic takes reverse or first and drives. A manual gearbox keeps its gear and keeps braking.

Launch, then watch the shifts:

```json
{ "method": "physics.vehicle_input", "params": { "entity": 4294967310, "throttle": 1.0 } }
{ "method": "play.step", "params": { "frames": 600 } }
{ "method": "physics.vehicle_telemetry", "params": { "entity": 4294967310 } }
```

`samples/vehicle_lab` is a rear-wheel drive car and a hover craft on a flat track with a ramp, for this kind of run. Start it with
`--no-script`, which leaves them parked for you: without the flag a looping script writes their input every frame.

## Hover craft

`regia_physics::hover` adds a second kind of vehicle: a chassis held above the ground by ray-cast
thrusters (spring and damper along the ground normal), with drive, brake, yaw and lateral-grip impulses
that act only while a thruster has ground. It reads the same `VehicleInput` as a car, so
`physics.vehicle_input` drives it (`F0003` only when the entity is neither a car nor a hover craft). The
rest of the axes match the car: forward is local −Z, steer +1 turns right.

`physics.hovercraft` lists them in entity order; `entity` limits it to one.

```json
{}
→ { "count": 1, "hovercraft": [ {
      "entity": 4294967311, "position": [0, 0.94, -8.1], "forward": [0, 0, -1],
      "forward_speed": 6.2, "lateral_speed": 0.01, "linear_velocity": [0, 0, -6.2],
      "grounded_thrusters": 4, "thruster_count": 4, "hover_height": 1.0,
      "input": { "throttle": 1.0, "brake": 0.0, "steer": 0.0 },
      "steps": 420, "last_error": null, "error": null,
      "thrusters": [ { "index": 0, "grounded": true, "distance": 0.94, "compression": 0.06,
                       "force": 1470.0, "contact_point": [-0.9, 0.0, -9.4] }, ... ] } ] }
```

- `distance` is the mount point's distance to the ground along its ray (the ray reaches twice
  `hover_height`); a craft with no ground in reach has `grounded: false` and applies no force.
- `compression` is `hover_height − distance`, `0` when not compressed.
- A craft settles at `hover_height − g / stiffness` (its total spring force is `mass · stiffness · compression`, shared by the thrusters).
- Error codes in `last_error` / `error`: `hover_no_thrusters`, `hover_too_many_thrusters`,
  `hover_invalid_config`, `hover_invalid_pose`, `hover_invalid_time_step`, `hover_chassis_missing`,
  `hover_no_physics_world`.

## Ray layers

A car's wheel rays and a craft's thruster rays hit every collision layer by default. `ground_layers` (a
bitmask of layer indices) and `query_layer` (a row of the `PhysicsLayers` query matrix) restrict them: a
ray hits only colliders on layers in `ground_layers & query_row`. Without a `PhysicsLayers` resource the
`query_layer` has nothing to read and only `ground_layers` applies. A wheel whose ray misses by layer
reads `grounded: false`, like one over a hole.

## Check a `.vehicle.ron`

`physics.vehicle_config_check` parses and validates the text of a `.vehicle.ron`
(`docs/formats/vehicle.md`) without spawning anything.

```json
{ "ron": "(format_version: 3, name: \"car\", config: ())" }
→ { "ok": true, "name": "car", "format_version": 3, "wheel_count": 4, "chassis_mass": 1200.0,
    "steered_wheels": 2, "driven_wheels": 2, "braked_wheels": 4, "ground_layers": null,
    "query_layer": null, "drivetrain": "simple", "gears": null, "anti_roll_bars": 0, "aero": false,
    "abs": false, "traction_control": false, "slip_tires": 0, "warnings": [] }
```

A v1 or v2 file answers with `format_version` 3: it migrated on the way in, with every tire `Damper`.

`warnings` lists tuning hints about the tires: the file is valid, but probably not what you meant. Each is
`{ code, wheel, message }`:

| Code | Meaning |
|---|---|
| `tire_mixed_axle` | Two wheels side by side have different tire models: the car pulls to one side in a turn. |
| `tire_roll_influence_low` | A slip tire's `roll_influence` is under 0.5: its side force acts near the center of mass's height, so load transfer is understated. |
| `tire_load_far_from_nominal` | The static load per wheel is under a third or over three times the tire's `nominal_load`: the load factor extrapolates. |
| `tire_slide_angle_large` | The curve peaks past about 20 degrees of slip at the static load: `cornering_stiffness` is low for `peak_friction`. |
| `tire_no_static_hold` | `stick_speed` is 0: a held brake creeps down a slope. |
| `tire_side_friction_ignored` | A slip tire has `side_friction_stiffness` other than 1, which only the damper tire reads. |

A bad file is an `F0002` error whose message starts with the stable code in brackets:
`vehicle_asset_parse`, `vehicle_asset_version`, `vehicle_asset_name`, or a `VehicleError` code such as
`vehicle_no_wheels` or `vehicle_invalid_config`.

## Slip tires

A wheel with `tire_model: Slip((..))` in a `.vehicle.ron` v3 (`docs/formats/vehicle.md`, ADR-2719) makes its side force
from the slip angle, on a brush or Magic Formula curve, instead of rapier's sideways velocity damper. Its grip falls
with load (`load_exponent`), the lengthwise force (the drivetrain's) takes its share first on a friction ellipse, the
side force acts at the contact's height (`roll_influence`), a held brake on a slope inside the grip does not creep, and
on a dynamic ground body the wheel pushes the ground back.

`physics.vehicle_tire_curve` reads a wheel's tire as force against slip angle without stepping anything. Give a spawned
vehicle (`entity`) or the text of a `.vehicle.ron` (`ron`), not both.

```json
{ "ron": "(format_version: 3, name: \"car\", config: (wheels: [(tire_model: Slip(()))]))", "wheel": 0, "points": 5,
  "max_slip_angle": 0.4 }
→ { "wheel": 0, "tire_model": "slip", "curve": { "kind": "brush" }, "load": 3000.0, "load_factor": 1.0,
    "friction": 1.0, "peak_force": 3000.0, "peak_slip_angle": 0.197, "cornering_stiffness": 45000.0,
    "roll_influence": 1.0, "stick_speed": 0.3,
    "points": [ { "slip_angle": 0.0, "lateral_force": 0.0 }, { "slip_angle": 0.1, "lateral_force": -2629.0 },
                { "slip_angle": 0.2, "lateral_force": -3000.0 }, ... ] }
```

- `load` (newtons) defaults to the tire's `nominal_load`; `max_slip_angle` (radians, up to π/2) to 0.5; `points` (2 to
  361) to 41, evenly from 0.
- `lateral_force` is along the wheel's right and opposes the slip: a positive slip angle gives a negative force.
- `friction` is `peak_friction · load_factor`, the grip per newton at that load; `peak_force` is `friction · load`.
- A wheel with the `Damper` tire is `F0002` with `[vehicle_tire_damper]`; a wheel the vehicle does not have is `F0002`
  with `[vehicle_tire_no_wheel]`; a bad file names its code as `physics.vehicle_config_check` does. An entity that is
  not a vehicle, or no physics world, is `F0003`.

`physics.vehicle_telemetry` reports every tire, slip or damper, in each wheel row:

- `tire_model`: `"slip"` or `"damper"`.
- `lateral_force`: the mean side force over the last step along the wheel's right, newtons (rapier's side impulse for a
  damper tire). Turning right, it is positive.
- `longitudinal_force`: the lengthwise force the tire used, newtons (the drivetrain's or rapier's, plus the hold).
- `tire_load`: the tire's load, newtons (suspension plus anti-roll; the suspension alone for a damper tire).
- `tire_slip_angle`: the slip angle the slip tire saw on that step (`0` for a damper tire).
- `stuck`: the slip tire held still this step (below `stick_speed`).

Check a corner, then the limit:

```json
{ "method": "physics.vehicle_input", "params": { "entity": 4294967310, "throttle": 0.4, "steer": 0.5 } }
{ "method": "play.step", "params": { "frames": 300 } }
{ "method": "physics.vehicle_telemetry", "params": { "entity": 4294967310 } }
{ "method": "physics.vehicle_tire_curve", "params": { "entity": 4294967310, "wheel": 0, "load": 3600 } }
```

A wheel whose `lateral_force` is near the curve's `peak_force` at its `tire_load` is at its limit.

For vehicles in multiplayer (replication, prediction, seats), see [net vehicles](net-vehicles.md).

## Tire transients and feedback

A `.vehicle.ron` v4 slip tire (`docs/formats/vehicle.md`, ADR-3276) can add, each off by default:

- `relaxation_length` (m): the side force builds over that much rolling, time constant `relaxation_length / speed`;
  it holds while the car stands still and does not ring at walking speed.
- `pneumatic_trail` (m): the self-aligning torque. It turns the wheel back toward its path and fades to 0 at the
  curve's peak slip angle (slightly past 0 beyond it for the Magic Formula), so the steering goes light at the limit.
- `camber_stiffness` (N/rad) and `static_camber` (rad, negative = top toward the center line): camber thrust, toward
  the side the wheel leans, from the static camber plus body roll and slope.
- `combined_slip`: `Ellipse` (default) or `SlipWeighted(shape, stiffness)`, which takes side grip away as the wheel
  spins or locks (its slip ratio) and keeps the resultant within `µ · load`.

`physics.vehicle_telemetry` adds to each wheel row:

- `lagged_slip_angle`: the slip angle the side force used (the lagged one with a relaxation length, else
  `tire_slip_angle`).
- `aligning_torque`: newton-meters about the contact normal, positive turning the wheel left (the sign of
  `steer_angle`). Turning right it is positive: the tires push the wheels back toward straight. 0 for a damper tire,
  with no trail, or while the tire is stuck.
- `camber`: the camber the tire saw, radians, positive leaning its top toward its right (0 without a
  `camber_stiffness`).
- `camber_thrust`: the part of `lateral_force` that is camber thrust, newtons along the wheel's right.

and to each vehicle row:

- `steering_torque`: the sum of the steered wheels' `aligning_torque` (newton-meters). This is the raw torque for a
  force-feedback wheel (before any steering ratio); Regia drives no wheel device.
- `steering_axles`: `[{ position_z, wheels, torque }]`, the same split per steered axle.
- `feedback`: `{ low, high, strength, device, requests }` when the entity has a
  `regia_framework::vehicle_feedback::VehicleFeedback`, else `null`.

`physics.vehicle_tire_curve` adds `pneumatic_trail` and `aligning_torque` to every point, and the tire's
`relaxation_length`, `pneumatic_trail`, `camber_stiffness` (N/rad at the asked load), `static_camber` and
`combined_slip` (`{ "kind": "ellipse" }` or `{ "kind": "slip_weighted", "shape", "stiffness" }`):

```json
{ "ron": "(format_version: 4, name: \"car\", config: (wheels: [(tire_model: Slip((pneumatic_trail: 0.04)))]))",
  "points": 3, "max_slip_angle": 0.2 }
→ { ..., "pneumatic_trail": 0.04, "relaxation_length": 0.0, "combined_slip": { "kind": "ellipse" },
    "points": [ { "slip_angle": 0.0, "lateral_force": 0.0, "pneumatic_trail": 0.04, "aligning_torque": 0.0 },
                { "slip_angle": 0.1, "lateral_force": -2629.0, "pneumatic_trail": 0.0085, "aligning_torque": -22.3 },
                { "slip_angle": 0.2, "lateral_force": -3000.0, "pneumatic_trail": 0.0, "aligning_torque": 0.0 } ] }
```

The aligning torque peaks well before the side force: a steering wheel that goes light warns of the limit.

### Rumble from the tires

Add `VehicleFeedbackPlugin` (after `VehiclePlugin`) and put a `VehicleFeedback` on the vehicle entity, with the
gamepad's device id to rumble it (`None` computes the channels only). After every fixed step:

| Channel | From | Map (defaults) |
|---|---|---|
| `low` | the steering torque | 0 below 5 N·m, 1 at 150 N·m, linear between |
| `high` | the largest change of a wheel's load in one step, over its static share of the weight (curbs, landings, impacts) | 0 below 0.25, 1 at 1.5, then fading by `e` every 0.12 s |

Both are in [0, 1] and both are 0 driving straight on flat ground. A `RumbleRequest` carries one strength, so it gets
`max(low, high) · strength_scale` for `request_duration` (0.1 s), sent every step the strength is above 0. Change the
map through `VehicleFeedback::map` (`VehicleFeedbackMap`). No new error codes: a vehicle the feedback cannot read is
skipped.

Check the feel of a corner:

```json
{ "method": "physics.vehicle_input", "params": { "entity": 4294967310, "throttle": 0.4, "steer": 0.6 } }
{ "method": "play.step", "params": { "frames": 120 } }
{ "method": "physics.vehicle_telemetry", "params": { "entity": 4294967310 } }
```

`steering_torque` should be positive in a right turn and grow, then fall back as the front tires pass their peak;
`feedback.low` follows it.

## Vehicle Tuning panel (editor)

The editor's **Vehicle Tuning** panel (`Panel::VehicleTuning`, ADR-9760) edits a `.vehicle.ron` in typed
sections (chassis, wheels with their tire model, engine and its torque curve, gearbox, clutch, differentials,
anti-roll, aero, aids), plots it, sends it to a running car without a respawn, and records scripted runs side
by side. Every button has a method here, and the panel and the methods share one state.
Trailers (`.trailer.ron`), aircraft (`.aircraft.ron`), the proving-ground tracks and towing are on
[Vehicle tuning: trailers, aircraft and proving-ground tracks](vehicle-tuning.md) (ADR-7745).

| Method | What it does |
|---|---|
| `editor.vehicle_tuning_open { path, create? }` | Opens a `.vehicle.ron` under the project (the panel's **Open**); `create: true` starts the default car named after the file when it does not exist (**New**), written at the first save. `create: true` on a path whose file already exists refuses with `vehicle_tuning_exists` — it never opens or overwrites that file. Opening drops the state bound to the previous file: a queued Apply, the explicit Apply target, the recorded runs and the live telemetry entity. |
| `editor.vehicle_tuning_state { plots?, wheel? }` | The edited `config`, `config_digest`, `dirty`, `validation`, tire `warnings`, `undo_depth`/`redo_depth`, the last `error`, `last_apply`, the recorded `runs` and the play car's last `telemetry` frame. `plots: true` adds the plot samples (below); `wheel` picks the wheel the tire and suspension plots show. |
| `editor.vehicle_tuning_set { field, value }` | Sets one field (one undo step). |
| `editor.vehicle_tuning_undo`, `editor.vehicle_tuning_redo` | Steps back or forward one edit (**Undo**, **Redo**); `changed` says whether there was one. |
| `editor.vehicle_tuning_apply { entity? }` | **Apply**: at the next editor frame, sends the edited configuration to a play vehicle through `Vehicle::set_config`. Read the answer in `state.last_apply`. |
| `editor.vehicle_tuning_save { path? }` | **Save**: writes `.vehicle.ron` v4 atomically (default: the open file). |
| `editor.vehicle_tuning_run { script, seconds, label? }` | **Run accel** / **Run skidpad**: drives the edited car on the proving ground and records the run. |

### Fields

`field` is a dotted path into the configuration's JSON shape: `chassis_mass`, `center_of_mass`,
`wheels.0.radius`, `wheels.0.suspension_stiffness`, `drivetrain.gearbox.final_drive`,
`drivetrain.gearbox.forward_ratios`, `drivetrain.engine.torque_curve` (a list of `[rpm, N·m]`),
`drivetrain.engine.torque_curve.2.1` (one torque), `drivetrain.rear_differential.torque_bias`,
`wheels.1.tire_model.cornering_stiffness`, `aero.drag_coefficient`, `aids.abs`, or `name`. An enum's
variant is passed through (`drivetrain.gearbox` is inside `Geared`, a slip field inside `Slip`), and a whole
section can be replaced: `{ "field": "drivetrain", "value": "Simple" }`, `{ "value": { "Geared": {} } }`
(missing fields take their defaults), `{ "field": "aero", "value": null }`.

```json
{ "field": "drivetrain.gearbox.final_drive", "value": 4.44 }
→ { "dirty": true, "undo_depth": 1, "config_digest": "…", "config": { … }, … }
```

A value the configuration refuses changes nothing. The error is `INVALID_PARAMS` with `data.reason` set to the
code `physics.vehicle_config_check` gives for the same file, and `data.field` set to the path:

```json
{ "field": "drivetrain.gearbox.final_drive", "value": -1.0 }
→ error { "data": { "reason": "vehicle_invalid_config", "field": "drivetrain.gearbox.final_drive" } }
```

In the panel a drag of a number or of a torque-curve point is one undo step however many frames it lasts.

### Apply in play

Apply needs a play session whose world has `PhysicsPlugin` and `VehiclePlugin`. The target is `entity` when
given (bits from `physics.vehicles` in the play world), else the vehicle running the opened file's
configuration, else the only vehicle. `Vehicle::set_config` keeps the chassis pose and velocity, the step
count and the input; a geared drivetrain keeps its rpm and gear (clamped to the new range and top gear) and
the wheels their spin; a wheel-count or drivetrain-kind change starts those fresh. The rules are listed on
`Vehicle::set_config`.

```json
// state.last_apply after the next frame
{ "ok": true, "answer": { "entity": 4294967310, "config_digest": "…", "config_revision": 1,
  "change": { "chassis_updated": false, "wheels_kept": true, "drivetrain_kept": true,
              "gear_clamped": false, "rpm_clamped": false, "tires_kept": 0 } } }
```

`physics.vehicles` then reports the same `config_digest` and `config_revision: 1` for that car. While play
runs the panel reads the car every frame into its telemetry strip (speed, rpm, gear, and per wheel the slip
ratio, slip angle and load); `state.telemetry` is the last frame.

### Runs

A run drives the edited car alone on a flat 30 km plane at 60 Hz (`regia_physics::vehicle_proving`), after one
second settling, with the same `VehicleInput` `physics.vehicle_input` sets: `accel` is full throttle straight
ahead; `skidpad` steers half lock right while the throttle holds a target speed that rises 1 m/s per second.
The answer, and each row of `state.runs`:

```json
{ "script": "accel", "seconds": 20.0 }
→ { "label": "run 1", "script": "accel", "seconds": 20.0, "zero_to_100_s": 6.69, "top_speed_kmh": 189.9,
    "max_lateral_g": 0.0, "distance_m": …, "upshifts": …, "digest": "…", "config_digest": "…" }
```

The vehicle lab's car (`samples/vehicle_lab/assets/rwd_car.vehicle.ron`) runs 0 to 100 km/h in 6.69 s and tops
out at 189.9 km/h in 20 s; with its final drive 20 % higher, 6.67 s and 189.2 km/h.
Its skid pad holds 1.09 g.

`zero_to_100_s` is `null` when the car never reached 100 km/h; `max_lateral_g` is the highest yaw rate × speed
held over half a second. The run reads no clock, so the same configuration gives the same `digest`. The panel
keeps the last 8 runs and shows them side by side with their speed traces.

### Plots

`state { plots: true }` returns the samples the panel draws, computed from the runtime's own math:
`torque_power` (`[{ rpm, torque_nm, power_kw }]` from idle to redline, `EngineConfig::torque_at`),
`wheel_force` (per forward gear `{ gear, total_ratio, points: [[km/h, N]] }`: torque × efficiency ×
`total_ratio` / driven wheel radius), `tire_side_force` (the selected wheel's slip tire at its nominal load,
the same numbers as `physics.vehicle_tire_curve`) and `suspension` (`{ rest_length, travel,
static_compression, points: [[m, N]] }`).

### Save

Save merges the edit into the file's text, so comments and layout stay; an older file's `format_version`
becomes 4. When the merged text would not load as exactly the edited asset, the panel writes the canonical
`VehicleAsset::to_ron` text instead and says the comments were lost (`comments_kept: false`).

### Error codes

| Code | Meaning |
|---|---|
| `vehicle_tuning_nothing_open` | No `.vehicle.ron` is open. |
| `vehicle_tuning_no_project` | No project is open. |
| `vehicle_tuning_bad_path` | The path climbs out of the project or does not end in `.vehicle.ron`. |
| `vehicle_tuning_not_found` | The file does not exist (pass `create`). |
| `vehicle_tuning_exists` | `create` was passed for a path whose file already exists; it refuses so **New** cannot silently overwrite a tuned file. |
| `vehicle_tuning_io` | Reading or writing the file failed. |
| `vehicle_tuning_no_path` | Save has no path (a created asset with none given). |
| `vehicle_tuning_bad_field` | The configuration has no such field (or a slip field on a damper tire). |
| `vehicle_tuning_bad_value` | The value has the wrong shape for the field. |
| `vehicle_tuning_bad_script` | `script` is not `accel` or `skidpad`. |
| `vehicle_tuning_bad_seconds` | `seconds` is not above 0 and at most 120. |
| `vehicle_tuning_not_playing` | Apply without a play session. |
| `vehicle_tuning_no_target` | No play vehicle matches (several and none runs the file: pass `entity`). |
| `vehicle_tuning_no_physics` | The play world has no `PhysicsWorld`. |
| `vehicle_*` | A refused value or retune: the `VehicleError` / `VehicleAssetError` code above. |
