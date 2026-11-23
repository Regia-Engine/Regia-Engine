# Vehicle tuning: trailers, aircraft and proving-ground tracks (agents)

The editor's **Vehicle Tuning** panel edits a `.vehicle.ron` (see
[Vehicles: Vehicle Tuning panel](vehicles.md#vehicle-tuning-panel-editor) for the car's sections, Apply,
plots and runs). Beside the car it opens a `.trailer.ron` and a `.aircraft.ron` with the same forms, undo
and comment-keeping save, and its runs can drive on a generated track and tow the open trailer
(ADR-7745). It also opens a `.bike.ron` and a `.hoverhull.ron` the same way and rides them on the proving
ground (ADR-8630, [Bikes and hover hulls](#bikes-and-hover-hulls)). Every button below has a method here; the
panel and the methods share one state.

## Methods

| Method | What it does |
|---|---|
| `editor.vehicle_tuning_open { path, create? }` | The file's ending picks the document: `.vehicle.ron` (the car), `.trailer.ron` (the trailer section's **Open**) or `.aircraft.ron` (the aircraft section's **Open**). `create: true` starts the kind's default named after the file (**New**): the car trailer preset, or the sample trainer. Opening a trailer or an aircraft leaves the car as it is. Answers the state with `opened` set to `vehicle`, `trailer` or `aircraft`. |
| `editor.vehicle_tuning_set { field, value, asset? }` | Sets one field of the `asset` document (`vehicle` by default, `trailer`, `aircraft`): one undo step. A value the configuration refuses changes nothing and answers `data.reason` with its own code (`trailer_*`, `vehicle_*` for a trailer's chassis, `aircraft_*`). |
| `editor.vehicle_tuning_undo { asset? }`, `editor.vehicle_tuning_redo { asset? }` | **Undo** / **Redo** of that document. |
| `editor.vehicle_tuning_save { path?, asset? }` | **Save** of that document, atomically. A new file is the format's canonical writer's bytes (`TrailerAsset::to_ron`, `AircraftAsset::to_ron`); an opened file keeps its comments when the merge reproduces the asset. Answers `{ written, asset, comments_kept, notice }`. |
| `editor.vehicle_tuning_run { script, seconds, label?, track?, tow? }` | The car's run (**Run accel** / **Run skidpad**). `track` picks the proving ground (the **track** picker); `tow: true` hitches the open trailer behind the car (**tow the open trailer**). Both are kept for later runs. |
| `editor.vehicle_tuning_fly { speed?, altitude?, seconds?, label? }` | **Fly**: trims the open aircraft for level flight at `speed` m/s (default 50) and `altitude` m (default 1000), flies that trim for `seconds` (default 10, at most 120) of fixed 60 Hz steps and records the flight. |
| `editor.vehicle_tuning_state` | Adds `track`, `tow`, `trailer` and `aircraft` (each `{ path, open, name, dirty, config, validation, undo_depth, redo_depth, error, notice, comments_kept }`) and `flights`; then `bike` and `hoverhull` (the same shape) and `rides: { bike, hoverhull }` (each a ride, or `null` before the first). |
| `editor.vehicle_tuning_ride { asset, steps?, steer?, speed?, drive?, throttle?, strafe?, respawn? }` | **Ride 1 s** / **Respawn** of the bike or hover-hull section: rides the open `asset` (`bike` or `hoverhull`) on the proving ground for `steps` fixed steps (default 60, 1 to 7200) with the input held. Answers the ride: `{ asset, steps, spawned, total_steps, config, state, digest? , recoveries? }`. |

### Fields

Fields are dotted paths into the configuration's serde shape, as for the car:

- Trailer: `chassis.chassis_mass`, `chassis.wheels.0.radius`, `coupling.point.2`, `coupling.capture_radius`,
  `parking_brake`, `rear_hitch`, `stand.half_size`, `brakes.Follow.proportion` (an enum's variant is passed
  through: `brakes.proportion` reads inside `Follow`), `name`.
- Aircraft: `mass`, `center_of_mass.1`, `inertia.0`, `parasite_drag_area`, `surfaces.0.span`,
  `surfaces.2.controls.0.max_deflection_deg`, `engines.0.static_thrust`, `gear.1.stiffness`, `name`.
- Bike: `wheelbase`, `gravity`, `max_lean`, `max_lean_rate`, `upright_speed`, `upright_rate`,
  `lean_response`, `contact_reach`, `name` (`docs/formats/bike-ron.md`).
- Hover hull: any `HoverParams` field of the hull's `params`: `ride_height`, `thrust_accel`, `coast_drag`,
  `max_forward_speed`, `yaw_rate_at_rest`, `roll_bank`, ..., and `name` (`docs/formats/hoverhull.md`). The
  file's `limits`, `flight` and `notes` are kept as written.

## Proving-ground tracks

The car drives toward −Z from the origin. Each track is the 30 km plane plus fixed convex colliders in the
lane from 20 m ahead (`regia_physics::vehicle_proving::ProvingTrack`); nothing is loaded from disk.

| `track` | What is in the lane |
|---|---|
| `flat` | Nothing: the plane alone (the same world, and the same run digest, as before). |
| `ramp` | A wedge 1.5 m high and 6 m long, 8 m wide: the car jumps off its top edge. |
| `slope` | A 10 % grade rising 6 m over 60 m, then a 400 m plateau. |
| `bump_strip` | 150 ridges 12 cm high and 0.5 m long, one every 2 m, alternating between the lane's left and right halves: the wheels lose and regain the ground. |

A run's metrics add `track`, `min_contacts` / `max_contacts` (the fewest and most wheels on the ground on one
step of the script), `contact_changes` (steps on which that number changed), `max_rise_m` (how high the
chassis rose above its start) and, when it towed, `trailer: { coupled, max_articulation_deg,
max_coupling_gap_m, chassis_mass, config_digest }`. The trailer is spawned coupled on a tow ball 0.3 m
behind the chassis box and 0.25 m below its center line, and brakes with the car.

## Flights

A flight answers `{ label, speed, altitude, seconds, alpha_deg, pitch_deg, throttle, pitch_input, stalled,
max_altitude_change_m, max_speed_change_pct, max_bank_deg, digest, mass }`: the trim's angle of attack, pitch,
throttle and elevator, then how far the free flight drifted from the trim. A well-trimmed aircraft holds its
altitude within a meter or two; a large drift, or `stalled: true`, says the configuration cannot fly level at
that speed. Flights are fixed steps with no clock: the same configuration gives the same `digest`.

Neither a trailer nor an aircraft has a live **Apply** to a play session (`Trailer` and `Aircraft` have no
`set_config`); the proving ground and the flight read the edited values.

## Bikes and hover hulls

A `.bike.ron` (`docs/formats/bike-ron.md`) and a `.hoverhull.ron` (`docs/formats/hoverhull.md`) open beside
the car (`asset: "bike"`, `asset: "hoverhull"`) with the same form, undo and comment-keeping save as a
trailer. **New** starts `BikeConfig::default()` or `HoverParams::DEFAULT_HULL`, named after the file; a new
file saves as `BikeAsset::to_ron` / `HoverHullAsset::to_ron` writes it. A refused bike value answers
`bike_bad_input` with the field; a refused hull value answers the hull's field code (`not_finite`,
`negative`, `zero_rate`, `out_of_range`, `inverted_range`).

`editor.vehicle_tuning_ride` is the proving ground for both. The first ride (or one with `respawn: true`)
spawns the open asset at rest: the bike upright with both wheel rays at 0.3 m (in contact) on the flat
ground, the hull at its `ride_height` over a flat floor at height 0. Each later ride carries on from where the
last stopped and steps the document as it is at that step, so a `vehicle_tuning_set`, an undo or a redo
since the last ride reaches the riding bike or hover craft at its next step, without a respawn. Steps are
fixed (1/60 s) and there is no clock: the same configuration, inputs and step counts give the same state.

- Bike: `regia_physics::vehicle_bike::bike_step` at `FIXED_STEP_S` with `steer` (radians, default 0),
  `speed` (m/s, default 8) and `drive` (default 1) held. `state` is `{ lean, steer, speed, drive,
  front_contact, rear_contact, fallen }`; `digest` is `BikeState::digest` in hex. A step the model refuses
  (a steer past ±1.2 rad, a speed past ±150 m/s) answers `bike_bad_input`; the steps before it are kept,
  and a refused first ride spawns nothing.
- Hover hull: `regia_detsim::hover::step_with` at `tick_dt()` on Earth (`WorldPhysics::EARTH`) with the
  sanitized hull, the file's `limits` (or `HoverLimits::DEFAULT`) and its `flight` mode (or the default),
  and `[throttle, strafe, steer]` held (each -1 to 1, default 0). `state` is `{ velocity, yaw, yaw_rate,
  altitude, airborne, roll, pitch }`; `recoveries` counts ticks the step had to reset a poisoned state (0 for
  a valid hull).

## Check your work

```json
editor.vehicle_tuning_open { "path": "assets/rwd_car.vehicle.ron" }
editor.vehicle_tuning_open { "path": "assets/box.trailer.ron", "create": true }
editor.vehicle_tuning_set { "asset": "trailer", "field": "chassis.chassis_mass", "value": 1500.0 }
editor.vehicle_tuning_run { "script": "accel", "seconds": 5, "tow": true }
→ { "distance_m": …, "trailer": { "coupled": true, "chassis_mass": 1500.0, … } }
editor.vehicle_tuning_run { "script": "accel", "seconds": 10, "track": "bump_strip", "tow": false }
→ { "track": "bump_strip", "min_contacts": …, "max_contacts": 4, "contact_changes": …, … }
editor.vehicle_tuning_open { "path": "assets/trainer.aircraft.ron", "create": true }
editor.vehicle_tuning_set { "asset": "aircraft", "field": "mass", "value": 1200.0 }
editor.vehicle_tuning_fly { "speed": 50, "altitude": 1000, "seconds": 10 }
→ { "alpha_deg": …, "throttle": …, "max_altitude_change_m": …, "mass": 1200.0 }
editor.vehicle_tuning_save { "asset": "trailer" }
editor.vehicle_tuning_open { "path": "assets/city.bike.ron", "create": true }
editor.vehicle_tuning_ride { "asset": "bike", "steps": 60, "steer": 0.15, "speed": 9.0 }
→ { "spawned": true, "total_steps": 60, "state": { "lean": …, "fallen": false, … }, "digest": "…" }
editor.vehicle_tuning_set { "asset": "bike", "field": "lean_response", "value": 9.0 }
editor.vehicle_tuning_ride { "asset": "bike", "steps": 60, "steer": 0.15, "speed": 9.0 }
→ { "spawned": false, "total_steps": 120, "config": { "lean_response": 9.0, … }, … }
editor.vehicle_tuning_open { "path": "assets/scout.hoverhull.ron", "create": true }
editor.vehicle_tuning_ride { "asset": "hoverhull", "steps": 120, "throttle": 1.0 }
→ { "state": { "velocity": [0.0, …, …], "altitude": …, … }, "recoveries": 0, … }
```

## Error codes

These add to the [Vehicle Tuning panel's codes](vehicles.md#error-codes):

| Code | Meaning |
|---|---|
| `vehicle_tuning_bad_asset` | `asset` is not `vehicle`, `trailer`, `aircraft`, `bike` or `hoverhull`; or a ride names a kind that does not ride (only `bike` and `hoverhull` do). |
| `vehicle_tuning_bad_path` | Also: the path does not end in the document's ending (`.trailer.ron`, `.aircraft.ron`, `.bike.ron`, `.hoverhull.ron`), or an open names none of the five. |
| `vehicle_tuning_bad_steps` | A ride's `steps` is not 1 to 7200. |
| `vehicle_tuning_bad_value` | Also: a ride's `steer`, `speed`, `drive`, `throttle` or `strafe` is not finite. |
| `vehicle_tuning_bad_track` | `track` is not `flat`, `ramp`, `slope` or `bump_strip`. |
| `vehicle_tuning_no_trailer` | `tow` is on and no `.trailer.ron` is open. |
| `vehicle_tuning_bad_seconds` | Also: a flight's `seconds` is not above 0 and at most 120. |
| `vehicle_tuning_nothing_open` | Also: the `asset` document is not open (a flight with no aircraft open). |
| `trailer_*` | The trailer refuses the value or the file (`physics.trailer_config_check`'s codes). |
| `aircraft_*` | The aircraft refuses the value or the file, or cannot trim (`aircraft_trim_unreachable`, `aircraft_trim_failed`). |
| `bike_parse`, `bike_version`, `bike_bad_name`, `bike_bad_input` | The bike refuses the file or the value (`docs/formats/bike-ron.md`), or a ride's step (`bike_bad_input`). |
| `parse_error`, `bad_version`, `bad_name`, `bad_flight`, `not_finite`, `negative`, `zero_rate`, `out_of_range`, `inverted_range` | The hover hull refuses the file or the value (`docs/formats/hoverhull.md`). |

## Not covered

- The hover form edits `params` only; a hull's `limits` block and `flight` mode are kept and stepped as the
  file wrote them (edit them in the text).
- Neither a bike nor a hover hull has an **Apply** to a play session; the proving-ground ride reads the
  edited values. The proving ground's ground is flat: no bike or hover track picker.
