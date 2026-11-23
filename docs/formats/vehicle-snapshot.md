# .vehicle-snapshot.ron: a saved vehicle, v2

Library: `regia_physics::vehicle_snapshot` (`VehicleSnapshot::{to_ron, from_ron, validate, digest}`,
`Vehicle::{snapshot, restore}`). Guide page: `book/agents/net-vehicles.md`. Decision: ADR-3080.

One vehicle's state at full precision. A vehicle restored from it and stepped gives the same bits as one that never
saved. The one exception: a chassis body touching something when it was saved (`chassis_in_contact`) loses rapier's
contact cache, so the steps differ in the last bits until the contact ends. `VehicleSimWorld::checkpoint` covers that
case for a car in a private world.

```ron
(
    format_version: 2,
    translation: (0.0, 0.82, -5.0),
    rotation: (0.0, 0.0, 0.0, 1.0),
    linear_velocity: (0.0, -0.06, -7.0),
    angular_velocity: (0.0, 0.0, 0.0),
    wheels: [ /* rapier's whole Wheel record, one per wheel */ ],
    vehicle_speed: 7.0,
    drivetrain: None, // Some(..) with a geared car: the whole DrivetrainState
    tires: [], // one per wheel when any wheel has a slip tire
    input: (throttle: 1.0, brake: 0.0, steer: 0.0),
    steps: 100,
    contact_shift: (0.0, 0.0, 0.0),
    chassis_in_contact: false,
)
```

| Field | What |
|---|---|
| `format_version` | Writes 2; v1 reads with zero tire transient state. Other versions are refused: `vehicle_snapshot_unsupported_version`. |
| `translation`, `rotation` | The chassis body's origin and rotation, world space. Finite; a rotation of length under 1e-6 is refused. |
| `linear_velocity`, `angular_velocity` | The chassis at its center of mass, m/s and rad/s. |
| `wheels` | rapier 0.36's `Wheel`, serialized whole: its fixed data must match the vehicle, its spin, suspension and contact record are the state. It includes the private rotation delta a wheel in the air coasts on. |
| `vehicle_speed` | rapier's own speed record. |
| `drivetrain` | The geared drivetrain's `DrivetrainState`; `None` with the `Simple` drivetrain. The model must match the vehicle's. |
| `tires` | One entry per wheel when any wheel has a `Slip` tire, empty otherwise; another count is refused: `vehicle_snapshot_config_mismatch`. Each is `anchor` (where the contact stuck, `None` while not stuck), `anchor_body` (rapier's index and generation of the ground body whose frame `anchor` is in, `None` for world space) and the last step's readouts `slip`, `lateral_force`, `longitudinal_force`, `load`, `slip_angle`, `stuck`. Version 2 adds `lag`, `lagged_slip_angle`, `aligning_torque`, `camber`, and `camber_thrust`, each defaulting to zero when reading v1. A value that is not finite is refused: `vehicle_snapshot_not_finite`. |
| `input` | The last input applied. |
| `steps` | Updates since spawn, attach or reset. |
| `contact_shift` | The contact record's pending floating-origin shift. |
| `chassis_in_contact` | Written by the save; a reader takes it as information. |

The `wheels` field carries a rapier type, so a rapier upgrade that changes it needs a version bump here. A snapshot is
a save's working state. `migrate_vehicle_snapshot` and `regia upgrade-project --write` upgrade v1
to v2 with zero transient history and preserve existing saved values (ADR-4590). A current file
is returned unchanged, including comments. Incompatible future wheel changes need a separate migrator.

Reading: at most 1 MiB (`vehicle_snapshot_too_large`). A value that is not finite is refused
(`vehicle_snapshot_not_finite`), including in the wheel fields the public API cannot see. Text that does not parse is
`vehicle_snapshot_parse`. `Vehicle::restore` also refuses a snapshot that does not fit: `vehicle_snapshot_wheel_count`,
`vehicle_snapshot_config_mismatch`. A refused restore changes nothing.

Fuzz target: `vehicle-snapshot`.
