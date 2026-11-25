# Lockstep and rollback

<!-- index: Lockstep -->
<!-- index: Rollback -->
<!-- index: Prediction -->
<!-- index: Interpolation -->
<!-- index: Lag compensation -->
<!-- index: Desync -->
<!-- index: Host migration -->
<!-- index: Relevance -->
<!-- index: Snapshot delta -->
<!-- index: Replay -->
<!-- index: Desync bundle | see: Desync -->

Pick the netcode that matches who owns the world, then check it. Hosting,
join codes, and transports are in [Host a game](host-a-game.md). The lockstep
wire protocol is in [Networking](../../user-guide/networking.md#lockstep-matches).
Keeping the simulation deterministic is
[Deterministic simulation](../../user-guide/deterministic-simulation.md).

## Which path

| You are building | Use | The server holds |
|---|---|---|
| An action game. The server owns the world. Each player should feel their own character immediately. | Replication, prediction, interpolation, lag compensation. | The world. Clients receive the entities that are relevant to them. |
| A large simulation that is too big to replicate, and every machine can run it. | Strict lockstep. Send inputs. Step the same frames. | One frame of inputs per tick. Every seat steps its own copy. |
| A fighting or sports match, 2 to 8 players, that can replay a few ticks. | Rollback lockstep. | Confirmed inputs. Each seat predicts the rest and replays when they arrive. |
| A simulation step your game owns, outside the ECS predictor. | `RollbackLog`. | The snapshots you record. |
| Plain snapshot values, with no replicated components. | Snapshot delta. | Quantized world snapshots and the deltas a peer has not acknowledged. |
| A simulation that already has its own clock and netcode. | The sim bridge. Regia draws what the sim reports. | Whatever that sim decided. The bridge does not write it back. |

A listen host (`HostRole::Listen`) is also a player. A dedicated lockstep host
is an `Observer` (it steps and votes) or a `Relay` (it does not simulate).

## Authoritative server: predict, interpolate, compensate

Mark entities `Replicated`. The owning client marks the ones it predicts with
`Predicted` (the same component as `NetPredicted`). Register one input type and
the components you compare:

```text
app.add_net_input::<Pad>()
    .predict::<Transform>(Tolerance::position(0.01))
    .add_predicted_systems(step_hero);
```

`Pad` implements `NetInput`. `on_missing` repeats the last command and clears
one-shot fields such as a jump or a shot. The step runs on the server for
every entity and on the owning client for its predicted copies. It runs again
when the server disagrees, so play sounds and spawn effects on
`PredictionCx::first_time()`. The step reads the fixed tick, not wall-clock
time and not frame `dt`.

Other players' entities are proxies. Show them between two server states, about
one send interval behind. The template uses `LinkInterpolationPlugin`.

A hit is judged on the server against the targets as the shooter saw them, at
the view tick carried in the command. `LagCompPlugin` checks that tick against
the shooter's measured window. `lag_compensation.max_rewind_ms` defaults to
1000 and cannot be set higher. Lower it when you want targets to win on a very
long link.

Relevance decides which entities a connection receives. Distance tiers and
priority are in
[Networking](../../user-guide/networking.md#relevance-priority-and-interpolation-specs-72-and-74).
An entity the newest state tick did not carry is left out of that tick's
prediction comparison while its last record is still young, so a quiet send
does not count as a false correction (`PredictionStats::deferred`).

Read the counters:

| Method | Finished when |
|---|---|
| `net.prediction` | `resets` stays 0 on the links you support. A reset means a correction older than `prediction.history_ticks` (64 by default, 128 at most). |
| `net.interpolation` | Proxies are present and the buffer is not starving for the whole match. |
| `lagcomp.stats` | Shots are judged in the past, inside the rewind cap. `windows` counts view ticks that were clamped. |
| `net.input_buffer` | The server is consuming commands. A stuck lead means inputs are not arriving. |

Characters, moving bases, and montage root motion are `NetCharacterPlugin`
([Net characters](../../agents/net-character.md)). The field list is
[Net prediction (agents)](../../agents/net-prediction.md).

Offline, and on a listen-server host, the local command is applied directly.
There is no prediction stream and no rollback for that local player.

## Strict lockstep

Use this when every participant can run the whole simulation and you will send
inputs, not entity state.

1. Implement `NetGame` and `LockstepCarrier` so your message type can wrap a
   lockstep payload. `lockstep::net` then carries `LockstepHost` and
   `LockstepSeat` over `NetServer` / `NetClient`. The core itself is sans-IO.
2. Give every participant the same `LockstepConfig`. `LockstepConfig::at_rate`
   keeps the maximum input delay at 250 ms. Defaults: 60 Hz, delay 4 ticks,
   range 1 to 15, shared delay, interleaved redundancy, 1000-byte datagrams.
3. Each seat, each frame: `seat_receive`, step every `next_frame` from
   `frame.inputs`, `report_hash`, `submit_input`, `send_seat`. Each host tick:
   `host_receive`, `poll_frame`, step, `report_hash`, `send_host`. A listen
   host also calls `local_input`.
4. Call `report_hash` after every step, or `record_state` when you implement
   `DesyncHash` and want the probe to name a subsystem.
5. Build the simulation on `regia_detsim::math` and `regia_detsim::float`. Run
   `cargo xtask det-scan` over it. The scan and the math rules are on
   [Deterministic simulation](../../user-guide/deterministic-simulation.md).

A lost datagram does not stall the match. The next datagram repeats the frames
the seat has not acknowledged. A seat with no input is `Absent`, or the input
`missing_input` names (`Empty`, `RepeatAnalog`, or `Hold`). Set the same
`InputRules` on the host and on every seat.

Publish the host and the seats into `NetAgentState` and read `net.lockstep`.
Finished when, across two reads a few seconds apart:

- the host `tick` advanced,
- `desyncs` is 0 and `checkpoints_agreed` increased,
- each seat you expect has `member: true`, `catching_up: false`, and
  `interrupted: false`,
- `frame_lag_ticks` is about a round trip, not climbing without bound.

`late_stamp_suspects` flags a seat that samples later than its capture tick.
The engine does not remove that seat. Your operators decide.

For a two-player competitive match, run a dedicated `Observer` host so the
hash vote has a third participant. With two voters there is no majority. The
default `TiePolicy::HostGroup` adopts the host's group. `TiePolicy::EndMatch`
ends as `desync_unresolved`.

Late join and reconnect use `resume_token`, then `catch_up` with your
serialized state. A state older than the frame log is `lockstep_catchup_gap`.
Over `NetServer`, `resume_grace_ms` plus `SeatPeers` resumes the same `PeerId`
when the client presents its last accept ticket. The full field table is
[Net lockstep (agents)](../../agents/net-lockstep.md).

## Rollback lockstep

For 2 to 8 players, set `LockstepConfig::rollback(60)` on the host and on
every seat (or `LockstepMode::Rollback { max_rollback_ticks }` on your own
settings). Drive each seat with `RollbackRunner` instead of stepping
`next_frame` yourself. Implement `RollbackGame` (`step` and `hash`, the same
step a strict seat runs).

Each application frame, between receive and send:

```text
runner.advance(&mut seat, now_ms, sample_input, on_confirmed)
```

Draw `runner.state()`. Your own input appears after the configured delay (1 to
3 ticks). Other players are predicted as their last input with presses cleared,
then corrected when the real input arrives, at most `max_rollback_ticks` back
(8 by default). The state that is hashed and voted on is
`runner.confirmed_state()`. After a catch-up, `runner.reset(next_tick, state)`.

The host holds a frame for a live seat for up to twice the window. A seat
whose median input arrives more than the window late is not held: holding
would not bring most of its inputs in, and every other seat would wait at the
window's edge. That seat's inputs are merged late. Its host row shows
`hold_released: true`. Set `release_slow_seat_hold` to false when every seat
should wait for the slowest. With the default, the other seats keep stepping.

`net.lockstep` on a rollback seat reports `rollbacks`, `rollback_ticks_max`,
and `rollback_ticks_replayed`. Finished when `rollback_ticks_max` stays inside
the window you configured and `desyncs` stays 0.

When the game, not the ECS predictor, owns a step, keep a `RollbackLog`:
record the state and the inputs each tick, and `reconcile` when an older server
state arrives. A correction older than the ring returns `rollback_too_old` and
leaves the stored snapshots in place. `VisualOffset` spreads the visual jump
across a fixed number of ticks. Publish with `NetAgentState::publish_rollback`
and read `net.rollback` (`replays`, `depth`, `too_old`). See
[Predicting a step the game owns](../../user-guide/networking.md#predicting-a-step-the-game-owns-spec-120--95).

## Desync

At each checkpoint (every `hash_every` ticks, default 30) the host tallies
state hashes, its own simulation included. A strict majority stands. A seat in
the minority is told to take the majority state (`ResyncSeat`). The host in
the minority is told the same (`ResyncHost`). A seat is removed with reason
`desync` after `max_resyncs` (default 2). One forged hash is one vote.

Implement `DesyncHash` (`world_hash` plus up to 64 named subsystem hashes) and
call `record_state` after each step so the probe can name the subsystem.
`record_replayed` (or `report_replayed_hash`) records a replayed tick against
the history the simulation has now. After a verdict the host emits
`HostEvent::DesyncProbed` with the diverged subsystem and the last agreed
tick. `net.lockstep` shows the same under `last_desync`.

Write a bundle on that event, from the state at `last_agreed`:

```text
DesyncBundle::from_host(&host, snapshot)?.write_to_dir(&project.join("saved/desync"))
```

Replay it with the build that wrote it:

```text
regia net desync-replay <bundle>
```

A bundle from another game or another protocol version is refused, and the
message names both versions. In a development build, `net.desync_inject`
plants a divergence and `net.desync` reads the verdict, the rows, the bundle
path, and the replay. The file format is `docs/formats/desync-ron.md`.

## Host migration

Strict lockstep can continue after the host process stops, because every seat
holds the world.

1. `MigrationRoster::generate(match_id, port, host_seat)`.
2. Add a `RosterSeat` per seat: the Noise static public key it would host
   with, and its address.
3. `host.share_roster(roster)` once the seats exist. The roster travels on
   each seat's sealed link.
4. Give every seat a `HostMigration`.
5. Each frame, `migration.poll` and `migration.on_message`. Open the re-host
   listener, connect with the winner's key pinned, and send on that link's
   reliable stream when `poll_action` says so.

The winner receives `MigrationEvent::Promoted`, takes the host with
`take_host()`, and keeps its own seat on an in-process link. Nobody steps
backward. Feed round trips with `note_transport_rtt` so the seat with the
lowest round trips wins. Call `seat.note_host_heard` when the transport hears
the host, so delayed frames are not treated as a dead host. A player who
dropped, including the old host, returns with `resume` and `catch_up` on the
new host.

Replication games use a different hand-off (a warm successor for a listen
server). That path is
[Networking](../../user-guide/networking.md#warm-successors-for-a-listen-server-spec-120--105).

## Snapshot delta

`regia_net::snapshot_delta` sends a full quantized snapshot, then per-entity
deltas against the last snapshot the receiver acknowledged. It does not use
the ECS replication components. Both peers build the same `SnapshotSchema`.
Every packet carries a layout hash. A mismatch is
`SnapshotError::SchemaMismatch`, never a silent wrong world.

Values are quantized before they are compared, so a change smaller than one
step is not a change. Non-finite floats clamp into range. A zero or
non-finite quaternion travels as the identity. The type in code is spelled
`Vec3Quantised`. The method that reports the step error is spelled
`max_quantisation_error`.

This layer does not order packets, interpolate, or filter by relevance. Feed
it one packet per `decode` call on whatever channel you choose. An incomplete
packet is still a valid baseline: the next delta sends what is missing.
`SnapshotError::BudgetTooSmall` means the budget cannot hold an empty packet,
and no sequence number is consumed. The schema, the wire layout, and the caps
(`MAX_FIELDS` 128, `MAX_ENTITIES` 65536) are on
[Snapshot delta](../../systems/snapshot-delta.md).

## Replay

Two recordings, for two jobs:

| File | Job |
|---|---|
| `.lsreplay` | A lockstep match. `regia net replay-audit <file>` re-runs it from its full snapshots and compares every recorded hash. Set `hash_every_ticks: 1` to name the exact tick. Agents use `net.replays` and `net.replay_audit`. |
| `.do27replay` | Presentation playback through `ReplaySim`. Playback does not re-simulate. Between full snapshots the entity set stays the last full's, unless sub-tick blending is on. No Direct Order 27 data ships in this repository. |

`.lsreplay` recording is opt-in (`LsReplaySettings { record: true, .. }`).
The library keeps the newest 20 after `rotate`. A capture of one client's
packets (`.netcapture`) is a third tool: it replays what that client saw,
including prediction. Treat a capture as private. It holds decrypted traffic.
[Replay playback](../../user-guide/replay-sim.md) is the presentation file.
[Match replays](../../user-guide/networking.md#match-replays-lsreplay) is the
lockstep file.

## A simulation outside the engine

`SimBridgePlugin` reads `BridgeInput`, asks the sim for a snapshot, reconciles
entities, and drains events. Nothing downstream of the bridge writes
simulation state. A lockstep or rollback sim you already run can sit behind
the bridge. Regia's own client-server stack is the other path. A game can use
either, including both when the external sim is the authority and Regia is
the presentation. See [Networking and the sim bridge](../../systems/sim-bridge.md).

## Finished check

1. The path in the table matches the game: replicated world, strict lockstep,
   or rollback lockstep. A fighting game of 2 to 8 players uses rollback
   lockstep. A large deterministic sim uses strict lockstep.
2. `cargo xtask det-scan` is clean for every crate that steps a lockstep world.
3. `net.lockstep`: `desyncs` is 0, `checkpoints_agreed` increases, seats are
   members and not catching up. On rollback, `rollback_ticks_max` stays inside
   the configured window.
4. A development inject (`DesyncInject` or `net.desync_inject`) produces a
   verdict that names the subsystem, and `regia net desync-replay` on the
   bundle names the same tick.
5. On the replication path, `net.prediction`, `net.interpolation`, and
   `lagcomp.stats` match the table above for a minute under the `bad`
   conditioner ([Host a game](host-a-game.md)).
