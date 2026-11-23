# .aircraft.ron: fixed-wing aircraft v1

Library: `regia_physics::aero::asset` (`AircraftAsset::{from_ron, to_ron, validate, into_config}`,
`AIRCRAFT_FORMAT_VERSION`, `aircraft_schema_fingerprint`, `aircraft_migration_sources`). Check without spawning:
agent method `physics.aircraft_config_check`; trim one: `physics.aircraft_trim { ron, speed }`. Guide page:
`book/agents/aircraft.md`. Decision: ADR-3374.

A `.aircraft.ron` is a named `AircraftConfig`. Every field has a default. The top level's are the sample
trainer's (`regia_physics::aero::trainer()`, `samples/flight_lab/assets/trainer.aircraft.ron`), and list items
default per field (`AeroSurface::default()`: a 10 m × 1.5 m flat wing, 15° stall; `EngineConfig::default()`;
`GearConfig::default()`). A file lists only what differs. Axes: forward −Z, right +X, up +Y, meters, kilograms, newtons.

```ron
(
    format_version: 1,
    name: "glider",
    config: (
        mass: 350.0,
        inertia: (600.0, 900.0, 400.0),
        parasite_drag_area: 0.15,
        surfaces: [
            (name: "wing", position: (0.0, 0.5, 0.1), span: 15.0, chord: 1.0, cl0: 0.3,
             controls: [(channel: Flaps, max_deflection_deg: 12.0)]),
            (name: "tail", kind: Tailplane, position: (0.0, 0.5, 4.5), span: 2.5, chord: 0.6,
             controls: [(channel: Pitch, max_deflection_deg: -20.0)]),
            (name: "fin", kind: Fin, position: (0.0, 1.0, 4.6), normal: (1.0, 0.0, 0.0), span: 1.0, chord: 0.8,
             controls: [(channel: Yaw, max_deflection_deg: -25.0)]),
        ],
        engines: [],
        gear: [(name: "skid", position: (0.0, -0.4, 0.0), brakes: false)],
    ),
)
```

## Top level

| Field | Meaning |
|---|---|
| `format_version` | Must be 1. Another value fails with `aircraft_version`; no older files exist. |
| `name` | A display name; not empty (`aircraft_invalid_config`, field `name`). |
| `config.mass` | Kilograms, `> 0` and `≤ 1e7`. |
| `config.center_of_mass` | Body space, meters. The body origin is where the entity's `Transform` sits. |
| `config.inertia` | Principal moments about X (pitch), Y (yaw), Z (roll) through the center of mass, kg·m², each `> 0`. |
| `config.fuselage_half_extents` | The fuselage collider box, meters, each `> 0`. It is the only collider; the gear rays hold it off the ground. |
| `config.parasite_drag_area` | `CD·A` of everything that is not a surface, m², `0..=1000`, acting at the center of mass. |
| `config.surfaces` | 1 to 16 lifting surfaces (`aircraft_no_surfaces`, `aircraft_too_many_parts`). |
| `config.engines` | 0 to 8 engines. |
| `config.gear` | 0 to 8 landing-gear legs. |
| `config.ground_layers`, `config.query_layer` | The gear rays' layer filter, as `.vehicle.ron`'s: a `1 << index` mask and a query-matrix row (`< 32`). |

## Surfaces (`AeroSurface`)

| Field | Meaning (range) |
|---|---|
| `name` | For reports. |
| `kind` | `Wing` (default), `Tailplane` or `Fin`. Only wings set the stall flag and give the reference area. |
| `position` | The aerodynamic center (quarter chord), body space. |
| `normal` | Lift direction at zero angle of attack, body space; not along −Z. `(0, 1, 0)` for a flat wing, `(1, 0, 0)` for a fin; tilt inward for dihedral. |
| `incidence_deg` | Rotation about the span axis, + leading edge up (`−30..=30`). |
| `span`, `chord` | Meters (`0.01..=200`, `0.01..=50`); area is `span × chord`. |
| `aspect_ratio` | `Some(ar)` (`0.1..=50`) for the lift slope and induced drag, or `None` for `span / chord`. Give each panel of a split wing the whole wing's. |
| `cl0`, `cd0`, `cm0` | Lift at zero angle (`−1..=1`), zero-lift drag (`0..=0.5`), quarter-chord moment (`−0.5..=0.5`). |
| `oswald` | Span efficiency `e` of the induced drag (`0.3..=1`). |
| `stall_angle_deg`, `stall_blend_deg` | Where attached lift peaks (`2..=40`) and the blend width into the flat plate (`0.5..=30`). |
| `flat_plate_cn` | Normal-force coefficient at 90° (`0.5..=2.5`). |
| `control_chord_fraction` | Control-surface chord over chord (`0.05..=0.9`). |
| `controls` | Up to 4 `(channel: Pitch \| Roll \| Yaw \| Flaps, max_deflection_deg)`: the trailing-edge deflection at a channel value of +1 (`±45`), positive adds lift along `normal`. The sum is clamped to ±45°. |

## Engines (`EngineConfig`)

| Field | Meaning (range) |
|---|---|
| `name` | For reports. |
| `position`, `direction` | Where the thrust acts and along which body axis (default forward `(0, 0, −1)`). |
| `static_thrust` | Newtons at full throttle, zero airspeed and sea-level density (`> 0`, `≤ 1e7`). |
| `model` | `Propeller(power: W, efficiency: 0.1..=1)`: `T = min(static, efficiency·power/V)`; or `Jet(speed_lapse: 0..=1)`: `T = static·max(0, 1 − lapse·V/340.294)`. Both scale with throttle and `ρ/1.225`. |

## Gear (`GearConfig`)

| Field | Meaning (range) |
|---|---|
| `name` | For reports. |
| `position` | Strut mount, body space; the ray goes down the body's −Y. |
| `rest_length`, `wheel_radius` | Meters (`0.01..=20`, `0.01..=5`); the ray reaches their sum. |
| `stiffness`, `damping` | N/m (`1..=1e9`) and N·s/m (`0..=1e8`). |
| `steerable`, `max_steer_deg` | Turns with the yaw input, up to `0..=80`°. |
| `brakes` | Brakes with the brakes input. |
| `rolling_friction`, `brake_friction`, `lateral_friction` | `0..=1`, `0..=2`, `0..=3`. |

Every range check names the field, for example `surfaces[2].span` or `gear[0].stiffness`, with `aircraft_invalid_config`.

Migration: none at v1 (`aircraft_migration_sources()` is empty). A shape change needs a version bump, a migrator
and `regia upgrade-project` support (spec 12 § 4a). The format is registered in `cargo xtask check-formats` and
`regia upgrade-project` as `aircraft`, and fuzzed by the `aircraft` target of `regia_agent`'s fuzz registry.
