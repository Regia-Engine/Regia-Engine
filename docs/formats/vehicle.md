# .vehicle.ron: vehicle configuration v4

Library: `regia_physics::vehicle_asset` (`VehicleAsset::{new, from_ron, to_ron, validate, into_config}`,
`migrate_vehicle`).
Check without spawning: agent method `physics.vehicle_config_check`. Guide page: `book/agents/vehicles.md`.
Decisions: ADR-2301 (v1), ADR-2598 (v2), ADR-2719 (v3), ADR-3276 (v4).
Editor (as built, ADR-9760): the Vehicle Tuning panel (`Panel::VehicleTuning`, `editor.vehicle_tuning_*`) opens,
edits, plots and saves this format and applies it to a running car with `Vehicle::set_config`; a save writes v4
and keeps the file's comments when the merge reproduces the asset. The format itself is unchanged.

A `.vehicle.ron` holds the tuning of one ray-cast car: what `regia_physics::vehicle::VehicleConfig` takes,
under a format version and a display name. Every `config` field and every wheel field is optional and
defaults to `VehicleConfig::default()` / `WheelConfig::default()` (a 1,200 kg four-wheel car, front steer,
rear drive). A `wheels` list replaces the default wheels as a whole.

```ron
(
    format_version: 3,
    name: "rally car",
    config: (
        chassis_mass: 1100.0,
        max_steer_angle: 0.45,
        ground_layers: Some(3),
        query_layer: None,
        wheels: [
            (position: (-0.8, -0.2, -1.3), steer: true, brake: true,
             tire_model: Slip((cornering_stiffness: 48000.0, load_exponent: 0.15))),
            (position: (0.8, -0.2, -1.3), steer: true, brake: true,
             tire_model: Slip((cornering_stiffness: 48000.0, load_exponent: 0.15))),
            (position: (-0.8, -0.2, 1.3), brake: true,
             tire_model: Slip((curve: MagicFormula(shape: 1.3, curvature: -0.4), cornering_stiffness: 62000.0))),
            (position: (0.8, -0.2, 1.3), brake: true,
             tire_model: Slip((curve: MagicFormula(shape: 1.3, curvature: -0.4), cornering_stiffness: 62000.0))),
        ],
        drivetrain: Geared((
            engine: (torque_curve: [(1000.0, 180.0), (4500.0, 300.0), (7000.0, 220.0)], redline_rpm: 7000.0),
            gearbox: (forward_ratios: [3.4, 2.1, 1.45, 1.1], upshift_rpm: 6200.0, downshift_rpm: 2600.0),
            layout: Awd,
            front_torque_share: 0.4,
            rear_differential: LimitedSlip(torque_bias: 2.5),
        )),
        anti_roll: [(left: 0, right: 1, stiffness: 15000.0), (left: 2, right: 3, stiffness: 9000.0)],
        aero: Some((drag_coefficient: 0.32, frontal_area: 2.1, rear_downforce: 0.3)),
        aids: (abs: Some(()), traction_control: Some(())),
    ),
)
```

| Field | Meaning |
|---|---|
| `format_version` | 3, or 1 or 2 for a file that migrates on load. Another value fails with `vehicle_asset_version`. |
| `name` | Display name; not empty (`vehicle_asset_name`). |
| `config` | The `VehicleConfig` fields; see the rustdoc for units and ranges. A bad value fails with the `VehicleError` code (`vehicle_invalid_config`, `vehicle_invalid_wheel`, `vehicle_no_wheels`, `vehicle_too_many_wheels`). |
| `config.ground_layers` | Bitmask of the layers the wheel rays hit; `None` hits all. |
| `config.query_layer` | The layer index whose `PhysicsLayers` query row also limits the rays; `None` for none. |
| `config.drivetrain` | `Simple` (the default) or `Geared((..))`, below. A bad field fails with `vehicle_invalid_config` and names it, such as `drivetrain.gearbox.upshift_rpm`. |
| `config.anti_roll` | Bars, usually one per axle: `(left, right, stiffness)`, wheel indices and newtons per meter of compression difference. Default none. |
| `config.aero` | `None` (the default) or `Some((air_density, drag_coefficient, frontal_area, front_downforce, rear_downforce))`. Drag is `0.5 · density · Cd · A · v²` at the center of mass; a downforce is `0.5 · density · coefficient · forward speed²` at that axle's mean wheel position. |
| `config.aids` | `(abs, traction_control)`: each `None` (the default) or `Some((..))`. Either one needs `Geared` (`aids` otherwise), because only it has wheel spin. |
| `config.wheels[i].tire_model` | `Damper` (the default: rapier's sideways velocity damper, set by `friction_slip` and `side_friction_stiffness`) or `Slip((..))`, below. A bad field fails with `vehicle_invalid_wheel` and names it, such as `tire_model.load_exponent`. |

## Slip tires (v3)

A `Slip` tire makes its side force from the contact's slip angle `α = atan(v_side / |v_along|)` (ADR-2719). rapier's
side force is off for that wheel; `side_friction_stiffness` is not read. Every field is optional:

| Field | Default | Meaning |
|---|---|---|
| `curve` | `Brush` | `Brush` (the brush model: rises, rounds over, flat at the peak once the contact patch slides) or `MagicFormula(shape: C, curvature: E)` (peaks, then falls toward `sin(C·π/2)` of the peak; `0 < C < 2`, `E ≤ 1`; about `C = 1.3`). |
| `peak_friction` | 1.0 | Side friction coefficient at `nominal_load`, in (0, 10]: the curve peaks at `peak_friction · load`. |
| `cornering_stiffness` | 45000 | N/rad at `nominal_load`: the curve's slope at a slip angle of 0. It grows in proportion to the load. |
| `nominal_load` | 3000 | Newtons: the load the friction and the stiffness are quoted at. |
| `load_exponent` | 0 | Load sensitivity in [0, 1): grip and stiffness per newton scale by `(load / nominal_load)^(−load_exponent)`, so a heavily loaded tire is less efficient. Road tires are about 0.1 to 0.3. |
| `roll_influence` | 1.0 | Where the side force acts, in [0, 1]: 1 at the contact (load moves to the outer wheels as the geometry says), 0 at the center of mass's height (no roll moment). rapier's damper uses 0.1. |
| `stick_speed` | 0.3 | Below this contact speed (m/s, in [0, 10]) the tire sticks: sideways always, lengthwise while the wheel is held by its brake, as long as the hold stays inside the grip. 0 turns it off. |

The lengthwise force stays the drivetrain's (`Geared`: its tire curve; `Simple`: rapier's engine and brake impulses).
The side grip left is `sqrt(1 − (Fx / Fx_max)²)` of the curve's force (a friction ellipse), with `Fx_max` the
lengthwise peak (`drivetrain.tire.peak_friction`, or the slip tire's own `peak_friction` with `Simple`) times the
load factor times the load. On a dynamic ground body a slip wheel pushes the ground back with the opposite of every
impulse it gives the chassis. Wheels side by side should share a tire model; `physics.vehicle_config_check` warns
(`tire_mixed_axle`) when they do not.

## Tire transients (v4)

v4 adds five optional `Slip` fields (ADR-3276). Each defaults to off, so a tire without them drives bit for bit as in v3.

| Field | Default | Meaning |
|---|---|---|
| `relaxation_length` | 0 | Meters, in [0, 10]: the side force builds over this much rolling (time constant `relaxation_length / speed`), so turn-in lags and a flick needs distance. It holds while the car stands still and is damped at walking speed, so a stiff tire neither shimmies nor runs away. Road tires are about 0.3 to 1 m. 0 = no lag. |
| `pneumatic_trail` | 0 | Meters at a slip angle of 0, in [0, 1]: how far behind the contact the side force acts. It gives the self-aligning torque that turns the wheel back toward its path; the trail falls to 0 at the curve's peak (`Brush`) or slightly below it past the peak (`MagicFormula`), so the steering goes light near the limit. About 0.02 to 0.05 m for a car. 0 = no aligning torque. |
| `camber_stiffness` | 0 | N/rad at `nominal_load`, in [0, 1e9]: the side force of a cambered wheel with no slip, toward the side it leans; it grows with the load like the cornering stiffness. About a tenth of `cornering_stiffness`. 0 = no camber thrust. |
| `static_camber` | 0 | Radians, in [−0.5, 0.5]: negative leans the wheel's top toward the car's center line (a wheel at x = 0 counts as a right wheel). Body roll and the slope add to it. Only `camber_stiffness` reads it. |
| `combined_slip` | `Ellipse` | `Ellipse` (the friction ellipse above) or `SlipWeighted(shape: S, stiffness: B)`: the side force times `cos(S · atan(B · κ))` for the wheel's slip ratio κ (0 with `Simple`, whose wheels roll with the car), then the resultant with the lengthwise force is held within `µ · load` (a friction circle). `0 < S ≤ 1`, `0 < B ≤ 1000`; about `S = 1`, `B = 10`. |

```ron
tire_model: Slip((
    relaxation_length: 0.5, pneumatic_trail: 0.035,
    camber_stiffness: 4500.0, static_camber: -0.02,
    combined_slip: SlipWeighted(shape: 1.0, stiffness: 10.0),
)),
```

## The geared drivetrain

Every field is optional and defaults to a 300 N·m five-speed rear-wheel drive car on the default car's wheel order.
With `Geared`, `max_engine_force`, `rolling_resistance`, each wheel's `drive` flag and `friction_slip` are not used:
the layout picks the driven wheels and the `tire` section sets the grip.

| Field | Meaning |
|---|---|
| `engine.torque_curve` | `[(rpm, newton-meters), ..]`, rpm rising, at least two points, some torque above zero: linear between, flat beyond. |
| `engine.idle_rpm`, `redline_rpm` | The engine never runs below idle; the fuel is cut at the redline. |
| `engine.inertia` | kg·m². |
| `engine.engine_braking` | Closed-throttle braking torque at the redline, newton-meters; linear in rpm. |
| `gearbox.forward_ratios`, `reverse_ratio`, `final_drive` | Forward ratios from first (1 to 16 of them, each below the one before); reverse as a positive number; the final drive multiplies every gear. An upshift must land above `downshift_rpm` (`upshift_rpm · next / this · 0.98 > downshift_rpm`), or the automatic could never take the next gear. |
| `gearbox.efficiency` | The share of the torque that reaches the differential, in (0, 1]. |
| `gearbox.automatic` | Start in automatic. A manual shift turns it off until `physics.vehicle_shift { automatic: true }`. |
| `gearbox.upshift_rpm`, `downshift_rpm`, `shift_time` | The automatic's thresholds, and how long any shift holds the clutch open with the throttle shut (seconds). `upshift_rpm` must not be above the redline and `downshift_rpm` must be above idle, or the engine could never reach them. |
| `clutch.max_torque`, `clutch.engage_rpm` | The closed clutch's limit; it closes as the faster of its two sides goes from idle to `engage_rpm`, so a stopped car idles and a launch slips. |
| `tire.peak_friction`, `slide_friction`, `peak_slip` | Friction rises linearly to the peak at `peak_slip`, falls linearly to the slide value at a slip of 1, and stays there. The peak is also the wheel's sideways limit. |
| `tire.rolling_coefficient` | Rolling resistance as a share of the wheel's load. |
| `layout` | `Rwd`, `Fwd` or `Awd`. |
| `front_axle`, `rear_axle` | `(left, right)` wheel indices. Defaults `(0, 1)` and `(2, 3)`. |
| `front_differential`, `rear_differential`, `centre_differential` | `Open`, `Locked` or `LimitedSlip(torque_bias: n)`; `n` ≥ 1 is how many times the faster side's torque the slower side can carry. The centre one is AWD only. |
| `front_torque_share` | The front axle's share through the centre differential, 0 to 1 (AWD only). |
| `wheel_inertia` | One wheel with its brake and half shaft, kg·m². |

`aids.abs` is `(slip_threshold, release_rate, apply_rate, min_speed)` and `aids.traction_control` is
`(slip_threshold, cut_rate, restore_rate)`; `Some(())` takes the defaults.

## Migration

v3 to v4 (ADR-3276): a v3 slip tire has none of the five transient fields, so each takes its default (off) and the car
drives bit for bit as it did. A v1 to v3 file holding one fails with `vehicle_asset_parse`. `VehicleAsset::from_ron`
migrates v1 to v3 as they load; `regia upgrade-project --write` rewrites them as v4 and leaves a `.bak`.

v2 to v3 (ADR-2719): a v2 wheel has no `tire_model`, so every wheel gets `Damper`, rapier's model, and keeps every other
value; the car drives bit for bit as it did. A v1 or v2 file holding `tire_model` fails with `vehicle_asset_parse`.
`VehicleAsset::from_ron` migrates v1 and v2 as they load; `regia upgrade-project --write` rewrites them as v3 and leaves
a `.bak`.

v1 to v2 (ADR-2598): a v1 file has none of the four new sections, so it gets `drivetrain: Simple`, no anti-roll, no
aero and no aids, and keeps every other value. `Simple` is the v1 model, so a migrated car drives bit for bit as it
did (`crates/regia_physics/tests/vehicle_v1_pin.rs`). `VehicleAsset::from_ron` migrates a v1 file as it loads, so a
project may keep its v1 files; a v1 file holding a v2 section fails with `vehicle_asset_parse`.
`regia upgrade-project --write` rewrites them as v2 and leaves a `.bak`; comments do not survive the rewrite.

A shape change needs a version bump, a migrator and `regia upgrade-project` support (spec 12 § 4a). The format is
registered in `cargo xtask check-formats` and `regia upgrade-project` as `vehicle`.
