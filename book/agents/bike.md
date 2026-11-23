# Two-wheel balance

A standalone lean model for one bike (`regia_physics::vehicle_bike`, ADR-4017). It does not drive
the car solver and it does not read a chassis body. The caller passes the steer, the speed, the
drive command and the two wheel-ray lengths on each step. The wheeled chassis in the same crate
keeps its own solver.

Axes match that chassis: forward is local −Z, right is +X, up is +Y, and the wheel axles lie on
+X. Positive steer and positive lean both point toward +X. Angles are radians, distances meters,
speeds m/s.

## Closed form

For a no-slip single-track turn the radius about the rear contact is `R = L / tan(δ)`, with `L`
the wheelbase (1.5 m by default) and `δ` the steer. The lateral acceleration is
`v² · tan(δ) / L`. For a small steer, `tan(δ) ≈ δ`, so that term is `v² · steer / wheelbase`.
The lean that balances it against gravity (`g` = 9.81 m/s²) is

```text
φ = atan(v² · tan(δ) / (L · g))
```

when `|v|` is at least the upright speed (2 m/s). Below that speed the target lean is 0.

## Lean rate and the spring

Above 2 m/s the lean rate tracks that angle: `clamp(4 · (φ − lean), ±2.5)` rad/s, integrated so a
held steer settles on `φ`. Below 2 m/s the `v²` term is off and a spring of the same 4/s pulls the
lean back to 0, with the same ±2.5 rad/s clamp.

A constant steer of 0 at 8 m/s holds the lean at 0. A constant non-zero steer at 8 m/s settles on
a lean of the same sign, within a tenth of the closed form, after a few seconds of 1/60 s steps.

## Fall

A lean past 1.05 rad sets `fallen` and stores drive `0`. Later steps keep that lean and that flag,
keep the drive at 0, and still take a new steer, speed and ray lengths.

## Wheel rays

`front_ray` and `rear_ray` are lengths in meters. A finite length from 0 through 0.7 is that wheel
in contact. A longer length is not. The two flags are reported for the caller; the lean equation
does not read them. `+∞` (a ray that missed) is not in contact either; JSON has no infinity, so
pass a length above 0.7.

## Calls

`physics.bike_step` advances the one stored bike. Leave a field out to keep its default:
`dt` 1/60 s, steer 0, speed 0, drive 0, both rays 0.3 m. `reset: true` restores the upright bike
and then applies the step.

```json
{ "dt": 0.016666668, "steer": 0.08, "speed": 8.0, "drive": 0.0, "front_ray": 0.3, "rear_ray": 0.3 }
```

The response reports `present`, `lean`, `steer`, `speed`, `drive`, `front_contact`,
`rear_contact`, `fallen`, and `digest`. The first step begins moving toward the steady lean;
repeat the step to converge. `digest` is 16 lowercase hex digits. The same inputs produce the same digest.

`physics.bike_state` reads the bike. Before the first accepted step, `present` is false and the
fields are the upright default. That read writes nothing.

This call needs the `play` permission. The read needs `read`.

### Error codes

| Code | Meaning |
|---|---|
| `bike_bad_input` | Steer, speed, drive, `dt`, a ray, or the configuration is not finite or is outside the range below. The stored bike does not change. |

Accepted ranges: `dt` in (0, 0.25] seconds, steer in [−1.2, 1.2] radians, speed in [−150, 150] m/s,
each ray length zero or more (a negative length is `bike_bad_input`). A value that does not fit in
a finite `f32` is `bike_bad_input`. A finite value such as `1e30` for steer also fails its range check.

The message starts with the code in brackets, for example
`[bike_bad_input] bike input or config is not finite or is outside the documented range`.

## Headless lab

`samples/bike_lab` runs the same stepper for 10 s at 1/60 s (600 steps) with no window. With no
arguments the steer is 0.08 rad and the speed is 8 m/s. It prints the final lean and the digest.

Run `cargo run -p bike_lab -- --help` for the argument list. `--steps N` or `--seconds S`
sets the run length (`--steps` takes precedence when both are present), and `--dt S` sets
the fixed step. The default is 600 steps, with a maximum of 60000. `--steer RAD`,
`--speed M/S`, `--drive N`, `--front-ray M`, and `--rear-ray M` set the inputs;
`--wheelbase M` and `--max-lean RAD` override those configuration fields. Invalid arguments
or model inputs exit with an error. No window or real-time wait is involved.

The host API also checks an initial lean before applying the spring. An initial state already
past the lean limit immediately latches `fallen` and stores drive 0; recovery cannot erase that fall.

## Host configuration ranges

`BikeConfig` fields must be finite and within these inclusive ranges. The agent methods use the
default configuration; hosts configure the Rust stepper directly. The sample exposes `wheelbase`
and `max_lean` through its arguments. Invalid configuration returns `bike_bad_input` before state changes.

| Field | Range | Default | Unit |
|---|---|---|---|
| `wheelbase` | 0.2–10 | 1.5 | m |
| `gravity` | 0.1–30 | 9.81 | m/s² |
| `max_lean` | 0.05–3.15 | 1.05 | rad |
| `max_lean_rate` | 0.1–20 | 2.5 | rad/s |
| `upright_speed` | 0–150 | 2 | m/s |
| `upright_rate` | 0.1–40 | 4 | 1/s |
| `lean_response` | 0.1–40 | 4 | 1/s |
| `contact_reach` | 0–5 | 0.7 | m |

The ranges are `BikeConfig::FIELD_RANGES`; `BikeConfig::first_bad_field` names the first field outside them.

## Bike files

A bike ships as a `.bike.ron` v1 (`regia_physics::vehicle_bike::asset::BikeAsset`, ADR-8630,
`docs/formats/bike-ron.md`): a name, these configuration fields (each optional, missing ones take the defaults
above) and an optional note. A bad value loads as `bike_bad_input` naming the field. The editor's Vehicle Tuning
panel opens, edits, saves and rides one (`asset: "bike"`, `editor.vehicle_tuning_ride`; see
[Vehicle tuning](vehicle-tuning.md#bikes-and-hover-hulls)).
