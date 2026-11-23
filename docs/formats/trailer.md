# .trailer.ron: trailer configuration v2

Library: `regia_physics::vehicle_trailer_asset` (`TrailerAsset::{new, from_ron, to_ron, validate, into_config}`,
`migrate_trailer`, `TRAILER_FORMAT_VERSION`, `trailer_schema_fingerprint`, `trailer_migration_sources`).
Check without spawning: agent method `physics.trailer_config_check`. Guide page: `book/agents/trailers.md`.
Decision: ADR-3281.

A `.trailer.ron` holds one trailer: what `regia_physics::vehicle_trailer::TrailerConfig` takes, under a format
version and a display name. Every `config` field is optional and defaults to `TrailerConfig::car_trailer()`
(a 750 kg single-axle box trailer on a tow ball).

```ron
(
    format_version: 1,
    name: "tandem box trailer",
    config: (
        chassis: (
            chassis_half_extents: (0.9, 0.35, 1.6),
            chassis_mass: 1300.0,
            center_of_mass: (0.0, -0.1, -0.15),
            max_brake_force: 3000.0,
            wheels: [
                (position: (-0.9, -0.15, 0.0), radius: 0.3, suspension_rest_length: 0.25, suspension_stiffness: 40.0),
                (position: (0.9, -0.15, 0.0), radius: 0.3, suspension_rest_length: 0.25, suspension_stiffness: 40.0),
                (position: (-0.9, -0.15, 0.8), radius: 0.3, suspension_rest_length: 0.25, suspension_stiffness: 40.0),
                (position: (0.9, -0.15, 0.8), radius: 0.3, suspension_rest_length: 0.25, suspension_stiffness: 40.0),
            ],
        ),
        coupling: (
            kind: TowBall(yaw_limit: 1.3, pitch_limit: 0.35, roll_limit: 0.3),
            point: (0.0, 0.0, -2.9),
            capture_radius: 0.3,
            jackknife_warning_angle: 0.95,
        ),
        brakes: Surge(threshold: 300.0, full_force: 4000.0, response: 0.15),
        parking_brake: 1.0,
        stand: Some((foot: (0.0, -0.55, -2.5), half_size: 0.1)),
        rear_hitch: Some((0.0, -0.1, 1.9)),
    ),
)
```

| Field | Type | Meaning |
|---|---|---|
| `format_version` | u32 | 2; v1 reads and migrates. |
| `name` | string | Display name, not empty. |
| `config.chassis` | `.vehicle.ron` v4 `config` | The chassis box, mass, center of mass, brakes and wheels (see `docs/formats/vehicle.md`). No wheel may have `drive: true` or `steer: true`; the drivetrain is `Simple`. `max_brake_force` is per braking wheel; `max_engine_force` and `max_steer_angle` are not used. A `chassis` section replaces the preset's chassis as a whole and its missing fields take `VehicleConfig::default()` (a car whose wheels drive and steer), so list the `wheels`. |
| `config.coupling.kind` | `TowBall(yaw_limit, pitch_limit, roll_limit)` or `FifthWheel(yaw_limit, pitch_limit)` | Radians, each the half of a symmetric range. `yaw_limit` is the jackknife stop, in (0, 1.745] (100°); pitch and roll in (0, 1]. |
| `config.coupling.point` | (x, y, z) | The drawbar's eye or the kingpin in chassis space (forward is −Z, up +Y), meters. |
| `config.coupling.capture_radius` | f32 | Coupling needs the hitch within this distance, meters, in (0, 5]. |
| `config.coupling.jackknife_warning_angle` | f32 | The readout's warning angle, radians, in [0, `yaw_limit`]. |
| `config.brakes` | `None`, `Follow(proportion, delay)`, `Surge(threshold, full_force, response)` | `proportion` in [0, 1], `delay` in [0, 2] s; `threshold` ≥ 0 N, `full_force` > `threshold`, `response` in (0, 5] s. |
| `config.parking_brake` | f32 | The brake while uncoupled, [0, 1]. |
| `config.stand` | `Some((foot, half_size))` or `None` | The stand's foot bottom in chassis space and half its box size (0, 1] m; down only while uncoupled. |
| `config.rear_hitch` | `Some((x, y, z))` or `None` | A hitch for the next trailer or a dolly. |

Errors (the `code()` of `TrailerAssetError`): `trailer_asset_parse`, `trailer_asset_version`, `trailer_asset_name`, or
a `TrailerError` code (`trailer_invalid_config` naming the field, `trailer_driven_wheel`, `trailer_steered_wheel`), or a
chassis `VehicleError` code such as `vehicle_no_wheels`.

## Versions and migration

v2 (v0.2.19): the chassis section is a `.vehicle.ron` v4 configuration, which adds the optional tire transient and
camber fields (ADR-3276). v1 files load unchanged (those fields default) and `migrate_trailer` rewrites them at v2
(`trailer_migration_sources()` is `[1]`); a current file comes back unchanged, comments and all. `regia upgrade-project`
migrates `.trailer.ron` files. The schema
fingerprint (`cargo xtask check-formats`) includes the `.vehicle.ron` fingerprint: a change to the vehicle
configuration's fields is a change to this format too and needs a version bump and a migrator here.
