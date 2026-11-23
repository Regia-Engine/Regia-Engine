# Aircraft: surfaces, thrust, gear, trim

Read, fly and trim fixed-wing aircraft in a running app (`regia_physics::aero`, ADR-3374). Each aircraft is
an entity with an `AircraftController` (one dynamic body, its surfaces, engines and gear) and an
`AircraftInput`. These calls need `regia_physics::PhysicsPlugin` and `regia_framework::aircraft::AircraftPlugin`
(added after it). Without a physics world, or for an entity that is not an aircraft, they return `NOT_FOUND`
(`F0003`) with a hint.

Axes follow the engine (`docs/02-architecture.md` § 3): forward is the body's local −Z, right +X, up +Y.
Distances are meters, speeds m/s, forces newtons, angles in degrees in the answers (radians in code).

- **Positive pitch** raises the nose (pull back).
- **Positive roll** lowers the right wing: a right roll, which is a rotation about the forward axis (−Z).
- **Positive yaw** turns the nose right, and turns a steerable nose wheel right.

## The model

Each surface (wing panel, tailplane, fin) is a flat panel at its quarter chord. Every fixed step, before the
world steps, the air-relative velocity at the panel is `v + ω × r − wind`. Spanwise flow is dropped, and the angle of attack is
measured from the chord. Lift, drag and pitching moment act at the panel:

| Range | Lift | Drag | Moment |
|---|---|---|---|
| \|α_eff\| ≤ stall angle | `cl0 + a·α_eff`, `a = 2π·AR/(AR + 2)` | `cd0 + CL²/(π e AR)` | `cm0 + cm_δ·δ` |
| past the stall | held at the stall value, blended over `stall_blend_deg` into a flat plate `CN = cn90·sin α_eff`, `CL = CN cos α_eff` | `cd0 + CN sin α_eff` | center of pressure moving aft to mid chord |

Control surfaces deflect the trailing edge by `δ` (each `ControlLink` maps an input channel to a signed
deflection). That shifts the effective angle `α_eff = α + τ·δ` by the thin-airfoil flap effectiveness
`τ = 1 − (θf − sin θf)/π`, with `θf = acos(2·cf/c − 1)`, and adds `cm_δ = −½ sin θf (1 − cos θf)` per radian.
Lift past the stall angle always drops, and the stall flag is set while any wing panel's |α_eff| exceeds its
stall angle and that panel's airspeed is above `STALL_MIN_SPEED` (5 m/s). The tail damps rotation by itself: its air velocity includes `ω × r`.

Air density follows the ISA troposphere to 11 km and the isothermal layer above it, up to 20 km. Altitude is
`y − sea_level_y` of the `AeroEnvironment` resource, which also holds the wind. Engines push along their axis
with `throttle × ρ/ρ0 ×` either a constant-power propeller `min(static, η·P/V)` or a jet
`static × (1 − lapse·V/a0)`. Gear legs are spring-damper rays down the body's −Y, with rolling resistance,
wheel brakes, side grip and nose-wheel steering.

Not modeled: downwash on the tail, ground effect, propeller wash and torque, compressibility, unsteady stall,
fuel burn, and rotorcraft.

## Read every aircraft

`physics.aircraft` lists the aircraft in entity order. `entity` limits it to one.

```json
{}
→ { "count": 1, "wind": [0, 0, 0], "sea_level_y": 0, "aircraft": [ {
      "entity": 4294967310, "position": [0, 152.3, -2210.4], "linear_velocity": [0, 0.2, -49.8],
      "body_rates": [0, 0, 0], "heading_deg": 0.0, "pitch_deg": 2.1, "bank_deg": 0.0,
      "airspeed": 49.8, "alpha_deg": 1.9, "sideslip_deg": 0.0, "altitude": 152.3, "density": 1.207,
      "dynamic_pressure": 1497.0, "cl": 0.39, "cd": 0.036, "stalled": false, "thrust": 1820.0,
      "engines": [1820.0], "grounded_gear": 0, "on_ground": false,
      "input": { "pitch": 0.1, "roll": 0, "yaw": 0, "throttle": 0.7, "flaps": 0, "brakes": 0 },
      "applied_input": { ... }, "steps": 3600, "last_error": null, "error": null,
      "surfaces": [ { "name": "left wing", "kind": "wing", "alpha_deg": 2.9, "deflection_deg": 0.0,
                      "cl": 0.5, "cd": 0.019, "cm": -0.05, "lift": 4150.0, "drag": 155.0, "stalled": false }, ... ],
      "gear": [ { "name": "nose", "grounded": false, "compression": 0, "normal_force": 0,
                  "friction_force": 0, "steer_deg": 0, "rolling_speed": 0 }, ... ] } ] }
```

- `alpha_deg` is the angle of attack at the center of mass: positive when the air comes from below the nose.
  `sideslip_deg` is positive when the air comes from the right.
- `cl` and `cd` are the whole aircraft's coefficients: aerodynamic force across and against the airflow over
  `q · S_ref`, where `S_ref` is the wing panels' area. They read 0 in still air.
- `body_rates` is the angular velocity in body axes: x pitch (+ nose up), y (+ nose left), z (+ left wing down).
- `input` is the entity's `AircraftInput` now; `applied_input` is what the last fixed step flew with, after
  clamping and including the assist's term.

## Fly

`physics.aircraft_input { entity, pitch?, roll?, yaw?, throttle?, flaps?, brakes?, wind? }` changes the named
controls; the others keep their value. Values are clamped (pitch, roll and yaw −1..1; throttle, flaps and
brakes 0..1), and a non-finite one is refused with `F0002`. `wind: [x, y, z]` sets the world's wind, which
every aircraft shares. The answer is `{ entity, before, after, wind }`.

Gameplay flies a plane through input actions instead: an `AircraftBindings` component names one action per
channel (default `aircraft_pitch`, `aircraft_roll`, `aircraft_yaw`, `aircraft_throttle`, `aircraft_flaps`,
`aircraft_brakes`). An `AircraftAssist` component adds rate damping, `−gain × rate` on pitch, roll and yaw,
written fresh every fixed step so it never compounds.

## Trim

`physics.aircraft_trim { entity | ron, speed, altitude?, throttle?, flaps? }` solves steady straight flight
offline: wings level, no sideslip, no rotation, heading −Z.

- Without `throttle` it finds the pitch attitude, pitch input and throttle for **level flight**.
- With `throttle` it holds that throttle and finds the **flight-path angle**. At 0 that is a glide.

```json
{ "entity": 4294967310, "speed": 50, "altitude": 150 }
→ { "mode": "level", "speed": 50, "altitude": 150, "pitch_deg": 2.4, "alpha_deg": 2.4, "flight_path_deg": 0,
    "pitch_input": 0.12, "throttle": 0.68, "flaps": 0, "cl": 0.39, "cd": 0.037, "lift_to_drag": 10.5,
    "stalled": false, "iterations": 5, "residual": 3e-7, "gravity": 9.81 }
```

To level off, set the answer's `pitch_input` and `throttle` with `physics.aircraft_input`. A trimmed plane spawned
in this state holds altitude within 2 m and speed within 2 % for a minute with the stick fixed
(`regia_physics/tests/aircraft.rs`). Gravity is the world's.

## Check a configuration

`physics.aircraft_config_check { ron }` parses and validates a `.aircraft.ron` (`docs/formats/aircraft-ron.md`)
without spawning anything:

```json
→ { "ok": true, "name": "trainer", "format_version": 1, "mass": 1000, "surfaces": 4, "engines": 1, "gear": 3,
    "reference_area": 16.5, "static_thrust": 2600, "max_lift_to_drag": 11.6, "best_glide_alpha_deg": 5.6,
    "max_trimmed_lift_to_drag": 10.3, "trimmed_glide_alpha_deg": 6.9,
    "cl_max": 1.5, "stall_speed": 25.2, "warnings": [] }
```

`max_lift_to_drag` is the best `CL/CD` of the aircraft's own polar (controls neutral, no thrust);
`max_trimmed_lift_to_drag` is the best of its trimmed polar (the elevator set for zero pitching moment at each angle),
which is what a steady glide achieves. A large gap between them is trim drag: the tail is fighting a large static margin.
`stall_speed` is the sea-level speed at `cl_max`. Warnings: `no_gear`, `no_engines`, `no_wing`.

## Errors

| Code | When |
|---|---|
| `F0003` | No physics world, or the entity has no `AircraftController`. |
| `aircraft_parse` | The `.aircraft.ron` text does not parse. |
| `aircraft_version` | The file's `format_version` is not 1. |
| `aircraft_invalid_config` | A field is out of range or not finite; the message names it (`surfaces[2].span`). |
| `aircraft_no_surfaces` | No aerodynamic surfaces. |
| `aircraft_too_many_parts` | More than 16 surfaces, 8 engines or 8 gear legs. |
| `aircraft_trim_invalid_target` | Speed outside 1..1000 m/s, altitude outside −2000..20000 m, throttle or flaps outside 0..1. |
| `aircraft_trim_failed` | The Newton solve did not converge (too slow or too fast for steady flight). |
| `aircraft_trim_unreachable` | The trim needs a pitch input outside ±1 or a throttle outside 0..1. |

An aircraft's own update can also record `aircraft_invalid_time_step` or `aircraft_chassis_missing` in its
`last_error` (the body was removed).

## Sample

`samples/flight_lab`: a 3 km runway and the trainer (`assets/trainer.aircraft.ron`). Run `flight_lab` to watch
the built-in autopilot take off, climb and level off at 150 m. Run `flight_lab --no-script` to fly it yourself
through the calls above. `tests/agent_smoke.rs` does the same through the agent protocol: take-off, climb, then
level-off with `physics.aircraft_trim`.

## Determinism

The update runs in `FixedUpdate`, inside `PhysicsStep`, before the world steps, in spawn order. The aircraft keeps its own buffers;
its own paths allocate nothing (`regia_physics/tests/aircraft_alloc.rs::the_owned_paths_allocate_nothing`). One 32-byte
allocation per update remains inside rapier 0.36 itself: each `PhysicsPipeline::step` `mem::take`s the body's modified-objects
list and the first `RigidBodySet::get_mut` after the step (ours, adding the forces) regrows it. Every per-step rigid-body
mutation in the crate pays the same (hover, ground vehicles); no public rapier API avoids it (ADR-3374). With the same build,
platform and inputs, two take-off runs give bit-identical worlds.
`regia_physics/tests/aircraft.rs` pins one take-off's digest on Linux x86_64. Without `AircraftPlugin`, nothing
in the physics schedule changes.
