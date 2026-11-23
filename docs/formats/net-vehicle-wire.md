# Vehicle state and input on the wire

Library: `regia_framework::net_vehicle`. Guide page: `book/agents/net-vehicles.md`. Decision: ADR-3080. These travel in
`regia_net`'s replication and input frames; the codec's shape feeds the protocol hash, so a client and a server built
with different layouts do not join each other. Values are packed least-significant bit first.

## `VehicleNetInput`: 26 bits, delta 3 to 27

| Field | Bits | Meaning |
|---|---|---|
| `throttle` | 8, two's complement | value / 127. `-128` is refused: `vehicle_input_range`. |
| `brake` | 8, unsigned | value / 255. |
| `steer` | 8, two's complement | value / 127. `-128` is refused: `vehicle_input_range`. |
| `shift` | 2 | 0 none, 1 up, 2 down. 3 is refused: `vehicle_input_shift`. |

Delta against the next newer command: one change bit, the three axes when they changed, always the shift. A refused
command makes its whole frame malformed: `regia_net` disconnects the sender with `MalformedPacket`.

`DriverNetInput` is a `CharacterNetInput` followed by a `VehicleNetInput`, and its delta is theirs in that order.

## `NetVehicleState`: a replicated, predicted component

| Group | Field | Bits | Step | Range |
|---|---|---|---|---|
| velocities | `linear_velocity` | 16 × 3 | 1/128 m/s | ±256 m/s |
| velocities | `angular_velocity` | 15 × 3 | 1/512 rad/s | ±32 rad/s |
| looks | `steer` | 9 | 1/127 | ±2 |
| looks | wheel count | varint | | at most 32, else `vehicle_state_wheels` |
| looks | wheel `spin_angle` | 10 | 2π/1024 | wraps |
| looks | wheel `suspension_length` | 9 | 1/64 m | ±4 m |
| drivetrain | present | 1 | | |
| engine | `engine_speed` | 16 | 1/16 rad/s | ±2,048 rad/s |
| engine | `clutch_torque`, `torque_hints[3]` | 16 × 4 | 0.5 N·m | ±16 kN·m |
| engine | `tc_factor` | 10 | 1/255 | |
| gears | `gear`, `target_gear` | 6 × 2 | offset by 1 | -1 to 62 |
| gears | `shift_timer`, `dwell` | 12 × 2 | 1/256 s | ±8 s |
| gears | `automatic` | 1 | | |
| wheels | wheel count | varint | | must equal the looks' count, else `vehicle_state_wheels` |
| wheels | `angular_speed` | 16 | 1/32 rad/s | ±1,024 rad/s |
| wheels | `spin_angle` | 10 | 2π/1024 | wraps |
| wheels | `slip_ratio` | 12 | 1/64 | ±32 |
| wheels | `tire_force`, `load` | 15 × 2 | 4 N | ±65 kN |
| wheels | `abs_factor` | 10 | 1/255 | |

Past its range a value clamps and counts in the codec's `clamped` counter; a value that is not finite is written as
zero. The pose travels separately as the entity's `Transform` (`TransformReplication::Movement`: position on the
profile's 1 mm grid, 10-bit smallest-three rotation).

Delta: one change bit per group, then the groups that changed. The drivetrain's engine, gears and wheels each have
their own bit, because gears hardly ever change. A delta that names a drivetrain the base does not have is refused:
`vehicle_state_wheels`.

## `VehicleSeatRequest`: a reliable message, client to server

`vehicle` (optional `NetId`), `seat` (`u8`), `enter` (`bool`). The server refuses by code: see the guide page. A
refusal is counted; it does not disconnect anyone.

## Measured

At 30 Hz (`send_every_ticks: 2`), with every field changing at 20 m/s, counting every byte a peer receives, input
acknowledgments and frame headers included: see ADR-3080 for the numbers and
`bytes_per_second_per_vehicle_at_30_hz` in `crates/regia_framework/tests/net_vehicle.rs` for the budget that holds
them.

Fuzz targets: `net.vehicle_input`, `net.vehicle_driver_input`, `net.vehicle_state`, `net.vehicle_seat_request`.
