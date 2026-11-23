# Net vehicles (agents)

Vehicles in multiplayer: `regia_framework::net_vehicle` and `regia_physics::vehicle_snapshot` (ADR-3080). Guide topic
`net-vehicles`. The module is behind the `net` feature of `regia_framework`, which the `net` feature of `regia` and the
agent server turn on.

The server simulates every vehicle in the shared physics world. The owning client predicts its own in a private world
holding the chassis and copies of the static colliders, which can go back to any tick. Other clients interpolate.

## Setting it up

<!-- ignore-reason: an outline across processes: `mode`, `config`, `peer` and `Axes` are the game's own -->
```rust,ignore
app.add_plugins((NetPlugin { mode }, PhysicsPlugin, VehiclePlugin, NetVehiclePlugin));

// Every process, before the first spawn: the tuning does not travel.
app.world_mut().resource_mut::<NetVehicleTunings>().insert("rally", config);

// The server spawns it; a client gets a copy.
let car = spawn_net_vehicle(world, "rally", (position, rotation), Some(peer))?;

// Each fixed tick on the client, before NetSet::Predict.
fn write_input(mut q: Query<&mut InputCommand<VehicleNetInput>>, axes: Res<Axes>) {
    for mut command in &mut q {
        command.value = VehicleNetInput {
            throttle: axes.throttle,
            brake: axes.brake,
            steer: axes.steer,
            shift: axes.shift,
        };
    }
}
```

- A vehicle with no owner belongs to the server and reads its own `VehicleInput` there (AI writes it).
- `VehicleNetInput` is quantized: 8 bits per axis (zero is exact), and the step runs on `quantized()` on both sides, so
  the client predicts with what the server receives. A missing command repeats the pedals and clears `shift`.
- A game where characters walk and drive has one input type: use `DriverNetInput` and `install_net_driver::<I>`, which
  installs characters first (their installer refuses a registered input), then vehicles. A game with richer input
  implements `VehicleCommand` on its own `NetInput` type and calls `install_net_vehicle::<I>` and
  `add_net_vehicle_systems::<I>` instead of adding the plugin.

## What is drawn

Draw `NetVehicleVisual::rendered(&transform)`, not the `Transform`. The simulated state jumps to a correction at once;
the drawn pose takes the difference as an offset and pays it back with `NetVehicleSmoothing`:

| Setting | Default | What |
|---|---|---|
| `half_life_ms` | 80 | The offset's half life. 0 shows corrections at once. |
| `max_step_m` | 0.09 | The most one step pays back, meters, so nothing visibly jumps. |
| `max_step_rad` | 0.05 | The same for rotation, radians. |
| `snap_m` | 2.0 | A correction past this snaps, and counts in `snaps`. |
| `snap_rad` | π/2 | The same for rotation. |

`NetVisualOffset` follows the same value, so a `NetVisualChild` shows the drawn pose too. A proxy has no offset: its
`Transform` is the interpolated pose.

## Seats

`VehicleSeats { seats: [VehicleSeat { offset, exit_offset, driver, occupant }] }` goes on the car. On the server,
`enter_seat(world, car, character, seat)` and `exit_seat(world, character)` change who sits where, and a client asks
with the `VehicleSeatRequest` message.

- Whoever sits in a `driver` seat owns the car: its `NetOwner` is the driver's. An empty driver's seat leaves the car to
  the server with its controls released. A seat change writes `NetOwner` in one place, so a car has at most one owner.
- A driver whose peer leaves (the session's hand-off removes `NetOwner` after the reconnect grace) does not leave the car
  on its last pedals: on the next tick `net_vehicle_server_apply` releases its controls, frees the driver's seat of a pawn
  that was despawned or handed to the server (that pawn gets out at the seat's exit), and sets `owner` to null. A pawn
  handed to a live peer keeps driving for that peer. A car the server always drove keeps the input the server writes.
- Any seat whose occupant no longer exists (a passenger's pawn despawned when its peer left, for example) is freed on the
  next tick, so another character can take it; a freed driver's seat leaves the car to the server. A pawn that is still
  there keeps its seat, whoever owns it now. The server logs `net.vehicle_seat_freed` for each freed seat.
- The handoff loses no input: from the tick of the change the car reads the new driver's command for that tick. Their
  stream has been running all along, because input travels whether or not the player drives.
- A character moving between seats of one car needs no reach, so a swap at speed works. A character entering from
  outside must be within `VehicleSeatRules::reach_m` (default 6 m) of the seat.
- A seated character carries `NetSeated { vehicle, seat }`, rides the seat in the movement mode
  `Custom(SEATED_MODE_ID)` and compares on the car (`NetCharacterBase`), so a car its client sees late is no correction.
  Leaving stands it at `exit_offset` moving with the car, in the falling mode.

## `net.vehicles`

Reads every networked vehicle in this process. No params.

| Field | What |
|---|---|
| `entity`, `net_id`, `tuning` | Which one. |
| `owner` | The owning peer, on the server and on clients; null when the server drives it. |
| `mode` | `server`, `predicted` (this client's own) or `interpolated`. |
| `position`, `rendered_position` | Simulated and drawn. |
| `speed`, `gear`, `rpm` | The chassis and the drivetrain (0 with the `Simple` drivetrain). |
| `last_correction_m`, `corrections`, `snaps` | This client's corrections. |
| `bytes_per_second`, `bytes_source` | On the server, over all connections. `pipeline` is the relevance pipeline's own count; `codec` is the encoded size at the send rate, when that pipeline is off. Null on a client. |
| `seats` | The `net_id` in each seat. |
| `fault` | The last fault's code, or null. |

`seat_requests { accepted, refused, last_refusal }` sums the server's seat requests.

Error: `F0002` with `data.reason` `net_not_running`, when no vehicle support is installed.

## Codes

| Code | Where | What |
|---|---|---|
| `vehicle_input_range` | input frame | An axis the encoder cannot write. The sender is disconnected with `MalformedPacket`. |
| `vehicle_input_shift` | input frame | The unused shift value. The same. |
| `vehicle_state_wheels` | state frame | More than 32 wheels, two wheel lists that differ, or a drivetrain delta with no base. |
| `vehicle_tuning_unknown` | `spawn_net_vehicle`, a client's `fault` | `NetVehicleTunings` has no such name. |
| `vehicle_seat_no_vehicle` | seat request | The target is not a vehicle with seats. |
| `vehicle_seat_no_such_seat` | seat request | The vehicle has no such seat. |
| `vehicle_seat_taken` | seat request | Someone else sits there. |
| `vehicle_seat_no_character` | seat request | The sender has no networked character. |
| `vehicle_seat_too_far` | seat request | The character is too far from the seat. |
| `vehicle_seat_not_seated` | seat request | There is no seat to leave. |
| `vehicle_snapshot_*` | `VehicleSnapshot` | See `docs/formats/vehicle-snapshot.md`. |

## Saving a vehicle

`Vehicle::snapshot(&physics)` and `Vehicle::restore(&mut physics, &snapshot)` save everything the next step reads:
chassis, rapier's wheel records, the drivetrain. Restoring and stepping gives the bits a run that never saved gives,
unless the chassis body touched something at the save (`chassis_in_contact`), because rapier's contact cache has no
public write for one pair. `VehicleSimWorld::checkpoint` and `rewind` are exact in that case too, for a car in its own
world. `VehicleSnapshot::to_ron` and `from_ron` are the file form.

## Limits

- A client's private world holds the static colliders. A bump against a moving body, another car included, is the
  server's to report, as a correction.
- `NetVehicleState` looks (spin, suspension) on a proxy are the latest state, not interpolated.
- Both sides must hold the same tuning: it does not travel.
- Slip tires' state does not travel either. A correction lets go of their stick anchors and the next step sticks
  afresh, so a car held on a slope stays where the correction put it. A saved `VehicleSnapshot` keeps the anchors.
- A wheel ray that grazes the vertical face of a step gets a one-tick force spike along a horizontal normal. A few
  millimeters decide whether it happens, so the server and the owner can disagree: expect one correction of about
  0.2 m on stepped ground, drawn smoothly. Measured: 0.65 m/s lost, 0.231 m of error, at 100 ms each way.

Saved vehicles now write `.vehicle-snapshot.ron` v2 (ADR-4590), preserving tire relaxation and
camber state. V1 saves read with that state zeroed; `regia upgrade-project --write` upgrades
them and keeps the original backup. Unsupported versions retain `vehicle_snapshot_unsupported_version`.
