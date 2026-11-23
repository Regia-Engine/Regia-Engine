# Destruction over the network

`regia_framework::destruction_net` runs [destruction level 2](destruction.md) in a session: **the
server decides, clients replay.** Only the server sees hits. Each client applies the server's breaks
in order and reaches the same `structure_hash`, with the same debris, sounds and effects (ADR-1967).
It needs the `net` feature of `regia`.

| Method | Permission | What it does |
|---|---|---|
| `physics.destruction_net` | play | Report this process's stream; tune the pacing; make a client ask for a snapshot. |
| `physics.destruction_playback` | read | What this process plays, to compare with another process: broken bonds and chunk poses. |

## Setting it up

```rust,wrap
# use regia::prelude::*;
# use std::sync::Arc;
# use regia_net::{NetMode, NetPlugin};
# use regia_physics::{PhysicsPlugin, PhysicsWorld};
# use regia_physics::destruction::{
#     Authority, DestructibleAsset, DestructiblePose, Destruction, DestructionPlugin,
# };
# use regia_framework::destruction_net::{DestructionNet, DestructionNetPlugin};
# let app: &mut App = todo!();
# let (mode, authority): (NetMode, Authority) = todo!();
# let text: String = todo!();
# let destruction: &mut Destruction = todo!();
# let physics: &mut PhysicsWorld = todo!();
# let seed: u64 = 7;
app.add_plugins((
    NetPlugin { mode },
    PhysicsPlugin,
    DestructionPlugin { authority },
    DestructionNetPlugin,
));
// On every process, for every asset the server may place:
let asset = Arc::new(DestructibleAsset::from_ron(&text)?);
app.world_mut().resource_mut::<DestructionNet>().register_asset(asset.clone());
// On the server only:
let id = destruction.define(physics, asset, DestructiblePose::at([0.0, 0.0, -5.0]), seed)?;
```

Clients never place destructibles: `Define` ops arrive from the server, at the server's ids. The
plugin turns a process into a replica when its `NetMode` is `Client` and back when it is not, so a game
that joins from a menu needs no extra step: the first snapshot replaces whatever its world held.
While the mode is `Offline` the bridge does nothing; a session started later begins with a snapshot of
the world as it stands.

Damage is the server's: a client sends its input, the game's server code calls
`destruction.world.damage(..)`. On a replica `damage` answers `destruction_replica`.

The messages register with `install_destruction_net`, which the plugin calls: every process must do it
in the same order, like any net message, or the handshake refuses the mismatch.

## What goes over the wire

The server's stream is one reliable-ordered message type per peer:

| Op | When |
|---|---|
| Snapshot | A peer joined, or asked for one. Pose, seed and asset of each instance, its detached chunks, the broken bonds between attached chunks, and the event sequence and digest. |
| `Define`, `Remove` | An instance appeared or left. |
| `Compact` | A break the replica can rebuild from asset, pose and seed: a bond index, or a chunk and its velocity. |
| `Full` | A break of a structure that moved, or one the server could not rebuild bit for bit. |
| `Check` | The server's `structure_hash`, after a whole tick's ops and every `check_every_ticks`. |

Compact breaks cost about 8 bytes each on the sample scene against 34 in full. The server only uses
the short form after rebuilding the event itself and comparing, so the wire is lossless. Poses of moving
structures go separately, unreliable and newest-wins, repeated every `pose_refresh_ticks`.

A burst is paced: at most `max_ops_per_tick` ops per tick (default 256), the rest wait in order. A
replica behind the server is still correct, just later: it has passed through one of the server's
past states. `physics.destruction_net` shows `queued_ops` on the server.

## Late joiners and lost step

A joining client gets a snapshot, then the stream from that point. Chunks that fell before it joined
are simply gone: no collider, no debris body, no effect.

A snapshot is paced too: `snapshot_parts_per_tick` parts of up to `snapshot_part_bytes` per tick
(defaults 1 part and 48 KiB; at most 16 parts, 1 to 48 KiB each), so a big world cannot pass the 1 MiB
of reliable messages a tick may queue for a peer. Smaller parts send a snapshot more slowly, for a slow
link. An instance bigger than a part (a structure with hundreds of thousands of bonds broken) goes in
pieces that the client joins back, so no single message passes the transport's 64 KiB.

Breaks that happen during the send wait **on the server**, in that peer's place in the stream, and go
out when the last part has: the client holds nothing while it assembles. Then the peer catches up at
`CATCH_UP_FACTOR` (4) times `max_ops_per_tick` per tick until it is level, so it arrives even while the
stream runs full, however long the snapshot took. `stats.snapshots_in_progress` counts sends under way
on the server, `stats.catching_up` the peers still catching up, and `in_step` stays false on the client
until it has caught up. If a peer's backlog passes `MAX_PEER_BACKLOG` (262,144 ops, about 17 s of a full
stream at 60 Hz), the server drops it and sends that peer a fresh snapshot (`stats.backlog_resyncs`).
Those parts count against that client's replication budget, so with the relevance pipeline on, its
ordinary entity deltas wait until the send ends; spawns and records at their `max_interval_ticks` still
go. At the defaults a 1 MiB snapshot takes about 0.4 s.

A replica that fails a `Check`, sees a gap, or gets an event that does not apply drops what it holds
and asks for a new snapshot. The server answers once per `resync_cooldown_ticks`; a request inside the
cooldown waits, it is not lost. The client asks again after `resync_retry_ticks` without a part of one. Debris
in flight on that client disappears when the snapshot lands.

If a client lacks an asset, its stream holds in order and `stats.missing_assets` lists the key
(`asset_key`, FNV-1a of the `.destructible.ron` text). Registering the asset lets it continue. Assets
are not sent: both sides load the same content.

## Relevance

By default every peer gets every structure. To stream a structure only to the peers that can see it,
give it an **anchor**: a replicated entity (`Replicated`, so it has a `NetId`) that the spec 72 relevance
pipeline decides for. Everything that pipeline does applies to the structure through its anchor: the
profile's spatial radius and hysteresis, `linger_ticks`, `NetVisibility` allow and deny lists, groups,
custom filters and temporary grants.

```rust,wrap
# use regia::prelude::*;
# use regia_core::math::{Vec3, Quat};
# use regia_scene::Transform;
# let mut commands: Commands = todo!();
# let world: &mut World = todo!();
# let app: &mut App = todo!();
# use regia_net::{NetProfileRef, Replicated};
# use regia_framework::destruction_net::DestructionNet;
# let id: regia_physics::destruction::DestructibleId = todo!();
# let building_center = Vec3::ZERO;
// On the server, with the relevance pipeline on (`install_net_interest`):
let anchor = world.spawn((Replicated::default(), Transform::from_translation(building_center),
                          NetProfileRef("building".into()))).id();
world.resource_mut::<DestructionNet>().set_relevance_anchor(id, anchor);
```

A peer that does not hold the anchor does not hold the structure: it is not in that client's world,
its breaks leave that peer's stream (one `Skip` op tells the client the sequence and digest after
them), and its `Check`s hash only what the client holds (`structure_hash_where` on the server), so a
filtered client still proves it is in step. When the anchor becomes relevant the client gets the
structure's state as the server has it then (an `Instance` op, in pieces if it is big; at most
`snapshot_part_bytes` times `snapshot_parts_per_tick` bytes of them per tick, continuing the same
pieces on later ticks). The server counts that instance as gained, and includes it in the peer's
held set, only after the last piece is queued. When it stops, the
structure leaves that client's world. Gains wait until the peer has the whole stream so far, so a
peer behind a full stream gets them late, never wrong. `clear_relevance_anchor` sends the structure to
everyone again; with no anchor left and every structure held, the peer is back to the whole world. An
anchor without a `NetId` yet holds the structure back; without a `NetInterest` resource anchors filter
nothing. Poses of moving structures follow the same rule: a peer only gets the poses of what it holds,
and a structure it gains later arrives in its current pose.

Counters (server): `stats.relevance_peers`, `instances_gained`, `instances_lost`, `breaks_skipped`,
`poses_sent`, `poses_filtered`; on a client, `poses_received`.
`DestructionNet::held_by(peer)` lists what a filtered peer holds; `physics.destruction_net` reports
`relevance: { anchors: [{ id, anchor }], filtered_peers: [{ peer, held }] }` (`anchor` is the entity's bits).

## `physics.destruction_net`

```json
physics.destruction_net { }
→ { "changed": false, "mode": "Client", "authority": "replica", "phase": "live", "in_step": true,
    "structure_hash": "0x4c7decfde4c7d135", "event_count": 133,
    "config": { "max_ops_per_tick": 256, "check_every_ticks": 120, "pose_refresh_ticks": 30,
                "resync_cooldown_ticks": 60, "resync_retry_ticks": 120, "snapshot_parts_per_tick": 1,
                "snapshot_part_bytes": 49152 },
    "stats": { "breaks_applied": 133, "checks_passed": 9, "checks_failed": 0, "snapshots_applied": 1,
               "resyncs_requested": 0, "pending_items": 0, "missing_assets": [], "last_problem": null } }
```

Optional parameters: `max_ops_per_tick` (1 to 1023), `check_every_ticks` (0: only after breaks),
`snapshot_parts_per_tick` (1 to 16), `snapshot_part_bytes` (1024 to 49152), and `resync: true` on a client.
On a server, `stats` also carries `relevance_peers`, `instances_gained`, `instances_lost`,
`breaks_skipped`, `catching_up`, `backlog_resyncs` and `split_instances`.

A runnable check of a slow-link setting: make parts small and watch a joiner arrive anyway.

```json
physics.destruction_net { "snapshot_part_bytes": 2048, "snapshot_parts_per_tick": 1 }
→ { "changed": true, ..., "config": { ..., "snapshot_part_bytes": 2048 }, "stats": { ..., "catching_up": 0 } }
```

`phase` is `waiting` (no snapshot yet), `live`, or `resyncing` on a client,
`server` otherwise.

To check your own work: run the same hits on a server and a client and compare `structure_hash` from
both, wait until `stats.pending_items` and the server's `stats.queued_ops` and `stats.snapshots_in_progress` are 0, and expect
`checks_failed` 0.

`stats.last_problem` names the last reason a client lost step:

| Code | Meaning |
|---|---|
| `destruction_hash_mismatch` | A `Check` differed. |
| `destruction_stream_gap` | A break came out of sequence, or the held stream overflowed. |
| `destruction_bad_event` | A break named something this world does not have. |
| `destruction_bad_snapshot` | A snapshot or `Define` could not apply. |
| `destruction_resync_asked` | Someone called `resync`. |

Errors:

| Code or reason | When |
|---|---|
| `F0003` | No `DestructionPlugin`, or no `DestructionNetPlugin`. |
| `destruction_not_client` | `resync` on a server. |

## Determinism

Everything a replica computes uses the operations of the [break model](destruction.md), so the hash
is the same on every platform. Debris motion is rapier's on each process and is not synchronized: two
clients see the same pieces leave with the same velocities, and they land where their own physics
puts them. The hash does not include it.

Pinned in `crates/regia_framework/tests/destruction_net.rs`: the sample script ends at
`0x4c7decfde4c7d135` on the server and every unfiltered replica; with relevance (each of two peers holding
one structure), a client holding only the wall ends at `0xba2c34d5796c3547` and one holding only the bridge at
`0x806c6df2eefeb2ed` (Linux x86_64 and Android aarch64).

## `physics.destruction_playback`

```json
physics.destruction_playback { "list_bonds": false }
```

Answers `{ tick, instances: [{ id, asset, attached, detached, debris, broken_bonds }], broken_bond_count,
broken_bond_digest, debris_count, chunk_pose_digest, structure_hash }` for this process. The digests are
hex strings. `list_bonds: false` leaves the per-instance `broken_bonds` out; the digests stay.

- `broken_bond_digest` and `structure_hash` must match on the server and every unfiltered client once
  the stream has arrived, whatever the link does.
- `chunk_pose_digest` covers each instance's pose and chunk states, then the exact translation and
  rotation of each debris body. Debris is each process's own, so it matches between two processes
  only if they ran the same ticks from the same stream. Compare it at the same `tick`, in a lockstep
  test or the sample below. Real clients over a real link see the same chunks leave with the same
  velocities and may not match here.

`F0003` without a `DestructionPlugin`. `regia_framework::destruction_net::destruction_playback` is the
same report for a game or a test.

## A sample to copy

`samples/destruction_lab` runs all of this in one process: a dedicated server and two clients, a wall
and a tower cut by the fracture tool, `DestructionDrawPlugin`, `ExplosionPlugin`, and an agent test.
It also shows the one thing a game writes itself. The server's breaks travel by this module, but an
explosion is the game's event, so the sample sends `LabBlast`, its own net message, and each client
writes `ExplosionEvent` when it arrives. A client takes no damage from that; it pushes the debris it
is already simulating, plays effects and stirs cloth ([explosions](explosion.md)).

Client 0 is the app you run; each of its fixed ticks runs one frame of the server and of client 1, so
`play.pause` and `play.step` hold all three in lockstep. Two methods reach past client 0:

| Method | Permission | What it does |
|---|---|---|
| `lab.fire { center, preset?, radius?, impulse?, fx? }` | play | An explosion on the server on its next tick. A bad center answers `F0002` with `data.reason` `bad_center`; a bad preset or size answers the [explosion](explosion.md) reasons; a bad number answers `lab_bad_param`. |
| `lab.report` | read | `{ ticks, server, clients: [a, b], clients_agree }`, where each process is `{ playback, in_step, checks_failed, explosions, chunks_pushed, chunks_hit }`. |

## Limits

- Relevance works per structure, through its anchor: a structure is held whole or not at all.
- Drawing chunks: `DestructionDrawPlugin` draws the cooked chunk meshes once a structure breaks
  ([destruction drawing](destruction-draw.md)); without it the game draws them (`lods` names each
  chunk's).
- On a link slower than the snapshot rate, the server's backlog for that peer grows while the
  snapshot is sent; past `MAX_PEER_BACKLOG` the peer starts over with a new snapshot.
- Hit validation is the game's: the bridge only moves decisions from server to clients.
