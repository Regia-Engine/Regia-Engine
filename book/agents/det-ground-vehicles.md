# Ground vehicles (lockstep)

`regia_detsim::ground_vehicle` (ADR-8551) steps wheeled and tracked vehicles with lockstep
arithmetic: the same inputs give the same bits on every target (x86-64 and aarch64, Linux,
Windows, macOS, Android). Both models are **kinematic**: no tires, no drivetrain, no suspension.
Those stay on the rigid-body side (`regia_physics::vehicle*`, rapier), which is physical but not
lockstep code.

| Model | Module | What it is |
|---|---|---|
| `tracked` | `ground_vehicle::tracked` | Differential (skid-steer) drive: two track speeds, center speed their average, yaw rate their difference over the track separation |
| `wheeled` | `ground_vehicle::wheeled` | The kinematic single-track (bicycle) model: yaw rate `-v * tan(steer) / wheelbase`, capped by a lateral grip |

Each is a pure `step(params, world, floor, state, input, dt) -> GroundReport`. `world` is a
`WorldPhysics` (its gravity pulls a coasting vehicle down a slope), `floor` is any
`FloorSampler` (a `HeightGrid`, a `FloorFn`, `FlatFloor`; see
[Hover step (lockstep), Host kit](det-hover.md#host-kit)).

`regia_physics::vehicle_track` (the `physics.track_step` method, [Skid-steer tracks](vehicle-track.md))
now steps through `tracked` with its default parameters on a flat floor.

## Conventions

Y is up. Yaw is radians about +Y, wrapped to (-π, π] after every step. The nose at yaw 0 points along −Z: the nose is
`(-sin(yaw), -cos(yaw))`, and positive yaw turns the nose left (toward −X). This is the convention
of `vehicle_track`; the hover step's nose is `(sin(yaw), cos(yaw))` instead. A positive wheeled
`steer` input turns right (negative yaw), as a stick pushed right. A faster left track also turns
right.

## Parameters

`TrackedParams` (`TrackedParams::DEFAULT` is `vehicle_track`'s model):

| Field | Default | Valid | Meaning |
|---|---|---|---|
| `separation_m` | 2 | [0.01, 20] | Distance between the track centerlines |
| `max_track_speed_m_s` | 8 | (0, 100] | Track speed at throttle ±1 |
| `friction_accel_m_s2` | 4 | (0, 1000] | Most a track's speed changes per second |
| `coast_drag_m_s2` | 1 | [0, 1000] | Coast toward 0 at zero throttle (capped by the friction) |
| `pivot_turn` | true | | The tracks may counter-rotate; without it an opposing track is braked |
| `grade_limit_deg` | 35 | (0, 89] | Steepest climb |
| `grade_speed_loss` | 0.5 | [0, 1] | Fraction of drive speed lost toward the grade limit (linear) |
| `half_length_m` | 2.5 | [0.01, 20] | Half the footprint length the slope is sampled over (half width = separation / 2) |

`WheeledParams` (`WheeledParams::DEFAULT` is a generic road car):

| Field | Default | Valid | Meaning |
|---|---|---|---|
| `wheelbase_m` | 2.7 | [0.01, 20] | Distance between the axles |
| `max_steer_rad` | 0.6 | (0, 1.2] | Largest front steer angle |
| `steer_rate_rad_s` | 2.5 | (0, 100] | How fast the steer angle moves |
| `max_speed_m_s` | 40 | (0, 200] | Top forward speed |
| `reverse_speed_m_s` | 8 | (0, 100] | Top reverse speed |
| `accel_m_s2` | 6 | (0, 1000] | Acceleration toward a faster request |
| `brake_m_s2` | 12 | (0, 1000] | Brake, handbrake, direction change, grade refusal |
| `coast_m_s2` | 1.5 | [0, 1000] | No throttle, or easing off |
| `lateral_grip_m_s2` | 8 | (0, 1000] | The yaw rate is capped so that `|v * yaw_rate|` stays at or under it |
| `grade_limit_deg` | 30 | (0, 89] | Steepest climb |
| `track_width_m` | 1.6 | [0.01, 20] | Left-to-right wheel distance (the roll sample's width) |

`validate()` returns the first bad field (`field`, `kind`, `value`); `kind.code()` is `not_finite`
or `out_of_range`. Call it when parameters are loaded (a mod file, a lobby setting), never per tick.
Every peer of a match must step the same parameters.

Every length is at least 1 cm (`ground_vehicle::MIN_LENGTH_M`, ADR-8638). The yaw rate divides by the
separation or the wheelbase, and before v0.1.28's fix a length in the old open range `(0, 20]` as
small as `1e-40` overflowed it: the step published a NaN or infinite X, Z and yaw. With valid
parameters, `dt` at most 1 s and a start state whose words are within ±1e9
(`ground_vehicle::MAX_STATE_MAGNITUDE`), 10 000 steps stay finite and never take the recovery.

## The step

**Tracked.** A nonzero throttle `t` (clamped to −1..=1) asks for `t * max_track_speed_m_s` and the
track speed moves toward it by at most `friction_accel_m_s2 * dt`. A zero throttle coasts. The pose
moves with the average of the start and end speeds along the heading at mid-step, so from rest with
constant accelerations the yaw after `T` s is `(accel_right - accel_left) * T² / (2 * separation_m)`.

**Wheeled.** The steer angle moves toward `steer * max_steer_rad` at `steer_rate_rad_s`. The speed
follows, in order of precedence: handbrake (toward 0 at `brake_m_s2`; the grip cap doubles), brake
pedal (toward 0 at `brake * brake_m_s2`), throttle (toward `throttle * max_speed_m_s` forward or
`throttle * reverse_speed_m_s` backward, at `accel_m_s2` when speeding up, `brake_m_s2` when the
request is the other way, `coast_m_s2` when easing off), else coast. The yaw rate is the kinematic
bicycle rate at the mid-step speed; when `|v * yaw_rate|` would pass the grip it is scaled down and
`slip` is the fraction removed (0 with full grip). The turn radius at low speed is
`wheelbase_m / tan(steer)` (`wheeled::turn_radius`).

**Slope (both).** The ground's pitch and roll come from four floor samples on the footprint (the
hover host kit's `ground_attitude`) into the state's `pitch` (positive: ground rising ahead of the
nose) and `roll`. At or past `grade_limit_deg` uphill along the direction of travel a drive request
asks for speed 0, so the vehicle stops on a slope it cannot climb and the report says
`grade_limited`. With zero throttle the speed first takes `-gravity * sin(pitch) * dt`, then the
coast drag, so a vehicle rolls back down a slope steeper than its drag holds (a low-gravity world
holds steeper slopes). Horizontal travel is `speed * dt * cos(pitch)`; Y is the floor under the new
position.

**Why the step moves the pose.** The hover step leaves the position to its host. These steps move it
themselves: a kinematic vehicle's motion is its pose (the arc of the mid-step heading), the slope
terms need the floor at the new position, and a host that integrated the speed on its own would
re-derive the heading with its own trig.

## Bad input

| Input | What the step does |
|---|---|
| `dt` NaN, infinite, 0 or negative | Nothing: the state is unchanged, the report is all false |
| A NaN or infinite input axis | Read as 0 |
| A NaN or infinite floor sample | Y keeps its previous value; the attitude reads the center floor (or 0) |
| A NaN or infinite world gravity | Read as Earth's (9.81 m/s²) |
| A state with a NaN or infinite word | Reset to rest in place (finite X, Z and yaw kept; Y the floor), not stepped this tick; `recovered` |
| A step whose result would hold a NaN or infinite word (a host-written state near `f32::MAX`, or a length `validate` refuses) | Reset to rest at the pose before the step (Y the floor), on the same tick; `recovered` |

The steps never panic, never validate parameters and never publish a NaN or infinite word: x86-64
and aarch64 write different NaN bit patterns, so a published NaN would split peers on raw bits even
where a NaN-folding checksum stays equal (ADR-8638).

## `detsim.ground_vehicle_simulate`

Runs one vehicle for up to 10 000 steps and answers the final state. Stateless: every call carries
everything.

| Param | Type | Default | Meaning |
|---|---|---|---|
| `model` | string | required | `tracked` or `wheeled` |
| `params` | object | the model's `DEFAULT` | Fields of `TrackedParams` / `WheeledParams` written over the default, then validated |
| `state` | object | rest at the origin | Fields of `TrackedState` (`pos`, `yaw`, `left_speed`, `right_speed`, `pitch`, `roll`) or `WheeledState` (`pos`, `yaw`, `speed`, `steer`, `slip`, `pitch`, `roll`); every word finite and within ±1e9 |
| `inputs` | array | none | `TrackedInput` (`left_throttle`, `right_throttle`) or `WheeledInput` (`throttle`, `brake`, `steer`, `handbrake`) objects; step `i` reads `inputs[i]`, and past the end the last one is held (at most 10 000 items) |
| `steps` | integer | 1 | 1 to 10 000 |
| `dt` | number | 1/60 | Above 0, at most 1 |
| `floor` | object | flat at 0 | `{ height, rise_x, rise_z }`: `y = height + rise_x * x + rise_z * z` (rises within ±100) |
| `gravity_m_s2` | number | 9.81 | Above 0, at most 1000 |

```json
{ "model": "wheeled", "steps": 120, "floor": { "rise_z": -0.25 },
  "inputs": [ { "throttle": 1.0 }, { "throttle": 1.0, "steer": 0.3 } ] }
```

The answer: `{ model, steps, dt, params, state, digest, grade_limited_steps, recovered_steps }`,
plus `center_speed` and `yaw_rate` (tracked) or `max_slip` (wheeled). `params` is the effective set.
`digest` is FNV-1a 64 over the state words (NaN and −0 folded), as 16 hex digits: two calls with the
same parameters answer the same digest on every target. Every number in the answer is finite (never
`null`); a non-finite one would be an engine bug and is answered as an internal error (`F0001`) instead.

### Errors

Refusals are `F0002` (JSON-RPC `-32602`) with `data.reason`:

| `data.reason` | When |
|---|---|
| `bad_model` | `model` is missing or not `tracked` / `wheeled` (a `model` that is not a string is the generic `invalid_params` refusal with `data.param` and `data.expected`, ADR-2622) |
| `bad_params` | The parameters are not an object; a `params`, `state`, `inputs[]` or `floor` field is unknown or of the wrong type; a parameter fails `validate` (the message names the field and `not_finite` / `out_of_range`); a `state` word is not finite or is past ±1e9 (the message names the word); `steps` is 0 or not an integer; `dt`, `gravity_m_s2` or a floor rise is out of range |
| `steps_limit` | `steps` is past 10 000: call again from the answered state |

`errors.explain` with `{ "code": "bad_model" }` or `{ "code": "steps_limit" }` returns the long form.

## Golden

`ground_vehicle_mix` (engine scenario seven, [Determinism goldens](det-golden.md)): four worlds
(gravity 9.81, 1.62, 3.71 and 24.79 m/s²), each with 8 tracked vehicles (two parameter sets, one
without pivot turns) and 8 wheeled ones (two sets) on a procedural hill field (`GroundField`, slopes
to about 40°), seeded inputs every 30 ticks (pivot turns, handbrake pulls, releases), 1800 ticks at
60 Hz. It hashes every state word of every vehicle and the grade-limited and recovered counts.

## Check it

```
cargo test -p regia_detsim --lib ground_vehicle -- --nocapture
cargo test -p regia_physics --lib vehicle_track -- --nocapture
cargo test -p regia_agent --lib det_ground_vehicle
cargo test -p regia_detsim --test det_golden
```

`pivot_turn_spins_in_place`, `grade_limit_stops_the_climb`, `bicycle_turn_radius_is_wheelbase_over_tan_steer`,
`lateral_grip_caps_yaw_rate_at_speed`, `reverse_speed_cap`, `ground_vehicles_ignore_nan_input`,
`zero_dt_changes_nothing`, `ground_vehicle_mix_matches_its_committed_golden` (regia_detsim) and
`tracked_defaults_match_vehicle_track` (regia_physics: the delegate against the pre-ADR-8551 step
over 10 000 seeded steps).
