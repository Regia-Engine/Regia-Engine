# Trailers: couple, uncouple, articulation, jackknife

`regia_physics::vehicle_trailer` (ADR-3281) adds articulated vehicles: a car with a trailer, a tractor
with a semi-trailer, and road trains in which a trailer's rear hitch tows the next trailer or a converter
dolly. A trailer is a chassis on unpowered ray-cast wheels (a `Vehicle` whose wheels neither drive nor
steer) and a coupling joint to a hitch point on the tow body:

| Coupling | Joint | Limits |
|---|---|---|
| `TowBall` | spherical | `yaw_limit` (the jackknife stop), `pitch_limit`, `roll_limit` |
| `FifthWheel` | revolute about the vertical, roll locked | `yaw_limit` (the jackknife stop), `pitch_limit` |

The app needs `PhysicsPlugin`, `VehiclePlugin` and `TrailerPlugin`. A tow entity carries a `TowHitch`
(a vehicle needs one added; a trailer with `rear_hitch` gets one from `spawn_trailer`). The plugin updates
trailers in the fixed step after the vehicles and before the physics step, in spawn order, and writes each
trailer's `TrailerState` component, which games and AI drivers read.

## Methods

| Method | Mutates | What it does |
|---|---|---|
| `physics.trailers { entity? }` | no | Lists trailers in entity order (one with `entity`). |
| `physics.trailer_couple { trailer, tow }` | yes (Play) | Couples a trailer to the tow entity's `TowHitch`. |
| `physics.trailer_uncouple { trailer }` | yes (Play) | Lets the trailer go: stand down, parking brake on. |
| `physics.trailer_config_check { ron }` | no | Parses and validates a `.trailer.ron` without spawning. |

`physics.trailers` rows: `entity`, `coupled`, `tow`, `front` (the train's front entity: the vehicle whose
brake the train follows, or an uncoupled trailer heading the rest), `train_position` (1 directly behind the
vehicle, 2 behind that trailer; 0 uncoupled), `coupling { kind, yaw_limit, pitch_limit, roll_limit,
jackknife_warning_angle, capture_radius }`, `brakes` (`none`, `follow`, `surge`), `has_stand`,
`rear_hitch`, `last_error`, and from the readout:

- `articulation_angle` (radians) and `articulation_angle_deg`: the trailer's yaw relative to the tow body
  about the tow body's up axis, positive when the trailer points to the tow's left. A steady right turn
  reads positive; a left turn negative.
- `articulation_rate` (rad/s), from the two bodies' angular velocities.
- `relative_pitch` (radians, positive nose up).
- `jackknife_ratio`: `|articulation_angle| / yaw_limit`, 1 at the stop; `jackknife_warning`: the angle has
  reached `jackknife_warning_angle`.
- `brake`: the brake applied at the last step (0 to 1); `coupling_gap` (meters, near 0 while coupled);
  `forward_speed`, `position`, `forward`, `grounded_wheels`, and `error` (a readout error code or null).

```json
{ "method": "physics.trailer_couple", "params": { "trailer": 4294967310, "tow": 4294967309 } }
{ "method": "play.step", "params": { "frames": 60 } }
{ "method": "physics.trailers", "params": { "entity": 4294967310 } }
```

Check the coupling by reading `coupled: true` and `coupling_gap` under 0.02 m after a second of steps.
To test steering, set the tow vehicle's input with `physics.vehicle_input` and watch
`articulation_angle`; in reverse, `jackknife_warning` turns on before the stop.

## Brakes

- `Follow { proportion, delay }`: the trailer brakes with the train's front vehicle's brake input times
  `proportion`, `delay` seconds later (whole fixed steps, at most 2 s).
- `Surge { threshold, full_force, response }`: an inertia brake. The drawbar's push is estimated from the
  trailer's deceleration (`mass × deceleration − its own brake and rolling force`); the brake rises from
  `threshold` to `full_force` newtons of push through a first-order lag of `response` seconds. Off below
  0.5 m/s and in reverse.
- `None`: no service brakes. An uncoupled trailer always holds with `parking_brake`.

## Error codes

Errors from `physics.trailer_couple`, `physics.trailer_uncouple` and `physics.trailer_config_check` are
F0002 (bad parameters) with the code in brackets at the start of the message, except where noted.

| Code | Meaning |
|---|---|
| `trailer_out_of_reach` | The hitch is farther from the coupling point than `capture_radius` (the message gives both). Move the trailer or the tow closer. |
| `trailer_already_coupled` | The trailer is on a hitch; uncouple it first. |
| `trailer_not_coupled` | Uncouple of a trailer that is not coupled. |
| `trailer_no_hitch` | The tow entity has no `TowHitch`. |
| `trailer_hitch_occupied` | Another trailer is on that hitch. |
| `trailer_self_coupling` | The trailer and the tow are the same entity or body. |
| `trailer_chain_cycle` | Coupling would make the train a loop. |
| `trailer_chain_too_long` | The train would be longer than 8 trailers (`MAX_TRAIN_LENGTH`). |
| `trailer_not_a_trailer` | F0003: the entity has no `TrailerController`. |
| `trailer_tow_missing` | F0003 from couple: the tow entity has no rigid body. As a `last_error`: the tow body went away while coupled; the trailer was uncoupled onto its stand. |
| `trailer_no_physics_world` | F0003: the app has no `PhysicsWorld`. |
| `trailer_invalid_config` | A trailer field is out of range; the message names it (`coupling.capture_radius`, `brakes.delay`, …). |
| `trailer_driven_wheel` | A chassis wheel has `drive: true`; trailer wheels are unpowered. |
| `trailer_steered_wheel` | A chassis wheel has `steer: true`. |
| `trailer_asset_parse` | The `.trailer.ron` text does not parse. |
| `trailer_asset_version` | The file's `format_version` is not one this build reads (1). |
| `trailer_asset_name` | The file's `name` is empty. |
| `vehicle_*` | The chassis failed a vehicle check (`vehicle_no_wheels`, `vehicle_invalid_wheel`, …; see [Vehicles](vehicles.md)). |

## In Rust

<!-- ignore-reason: statements from inside a setup function; they need a World with PhysicsPlugin and a spawned car -->
```rust,ignore
use regia_physics::vehicle_trailer::{TowHitch, TrailerConfig, car_tow_hitch, couple_trailer, spawn_trailer};

world.entity_mut(car).insert(TowHitch { point: car_tow_hitch() });
let config = TrailerConfig::car_trailer();
let at = car_position + car_tow_hitch() - config.coupling.point;
let trailer = spawn_trailer(world, config, at, Quat::IDENTITY)?;
couple_trailer(world, trailer, car)?;
```

Presets: `TrailerConfig::car_trailer()` (750 kg single-axle box trailer for the default car and
`car_tow_hitch()`), `TrailerConfig::semi_trailer()` (12 t tri-axle on a fifth wheel for `tractor_config()`
and `tractor_fifth_wheel()`), `TrailerConfig::dolly()` (a converter dolly with a fifth wheel on top). A
bare world uses `Trailer::spawn`, `Trailer::spawn_coupled`, `Trailer::couple`, `Trailer::update` (once
per fixed step, after the tow vehicle and before the world steps) and `Trailer::readout`.

Measured on the test machine (`crates/regia_physics/tests/vehicle_trailer.rs`): on a slow steady circle the
articulation angle is within 1 % of the kinematic off-tracking geometry; full braking from 20 m/s stops a
car and trailer in about 23 m with follow brakes against 32 m without; in reverse at full lock the tow
ball holds its 50° stop to within 0.01°.

The file format is in `docs/formats/trailer.md`.
