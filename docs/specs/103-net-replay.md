# 103 — Network replay: recorder connection, `.netdemo`, player

Status: proposed (ADR-3021). Rung T2.3 (multiplayer beyond the template). Crates: `regia_net::recorder` (recording, ring, file
format, reader, playback primitives), `regia_framework::net_replay` (player, plugin, camera hook, feature `net`),
`regia_agent::net_replay` (agent surface). Builds on specs 70-74 and 77; answers the future item of spec 71 § 16.

Not the local replay system: `regia_framework::{demo, demo_world, demo_scrub}` records one process's input and state and replays by
simulating again. This records what a server sends and replays by applying it. The formats stay separate. The scrub maths
(`demo_scrub::{tick_at_fraction, fraction_of_tick, format_tick_time}`) and the spectator camera are reused.

Sources: original design on the public sources of § 13.

## 1. Purpose and scope

A server records what a client would have received, from a chosen point of view, to a file or to a memory ring. Any process with the
same registered protocol plays it back with no server: copies spawn from prefabs, proxies interpolate, despawns wait for their tick,
and messages reach their handlers, because every recorded frame goes through the client path of `regia_net::runtime`.

In scope: the recorder connection, keyframes, `.netdemo` v1, size and time caps, ring mode with export, the player (play, pause, seek,
speed, camera hook), the agent surface. Out of scope: § 11.

## 2. Terms

| Term | Meaning |
|---|---|
| Recorder connection | A virtual client connection on the server (`PeerId` in the reserved range, § 3.1). It has no socket, owns nothing and never answers. |
| View | Which entities the connection holds: `Everything`, or `Player(peer)`: the set that peer holds. |
| Tick chunk | The frames a client would have received for one send tick, verbatim. |
| Keyframe | One `TICK` frame holding the full state at a tick: what a client joining at that tick would receive. |
| Segment | A keyframe and the tick chunks up to the next keyframe. |
| Ring | The newest whole segments in a fixed block of memory. |
| Playhead | The player's position in fractional server ticks. |

## 3. Recorder connection

### 3.1 Identity

Peer ids from `FIRST_VIRTUAL_PEER = PeerId(0xFFFF_FF00)` up are reserved. Session ids stay below 65,536 (`u16` slots), so nothing
collides. `RECORDER_PEER` receives the stream; `KEYFRAME_PEER` exists for the gather of one keyframe tick.

`NetNode::peers()` (public: agent methods, `NetSender::peers`, relevance views) lists real clients only, so `net.peers`, player counts and
gameplay never see a recorder. Messages sent to `Recipient::All` do reach it, under the same `holders` filter as any non-owner.

### 3.2 What it receives

A recorder is a non-owner (spec 71 § 16). `OwnerOnly` components and owner-addressed messages do not travel to it, `Others` and
`ProxyCopies` do, `SpawnOnly` and `SpawnThenOwner` values go in its spawn records. The wire is always `ConnWire::Stream`: one `TICK`
frame per send tick, deltas against the previous recorded tick. Unreliable messages arrive as `UMSG` frames.

It is unbudgeted. Rate, spawn budget, priority and `min_interval_ticks` exist to protect a link it does not have, so it gets every
change every send tick: at each send tick it holds the newest state of everything in its view. A real client whose budget holds a
change back shows it later than the recording does.

`SharedGroup` components: `Everything` receives them all; `Player(peer)` receives what that peer's groups admit.

### 3.3 Views

- `Everything`: skips relevance entirely, including `NetRelevance::filter`.
- `Player(peer)`: with `NetInterest` installed, the held set of that peer at this tick (relevant and lingering), so hysteresis and linger
  match; without it, `NetRelevance::filter` evaluated for that peer. If the peer leaves, the last set stays frozen: entities held then
  keep updating until they despawn, nothing new enters (with or without `NetInterest`; no filter runs for the departed peer), and `net.replay_state` reports `view_peer_gone`.

Owner-exclusive entities of the chosen peer are in its set and so are recorded, with their `OwnerOnly` components still absent.

### 3.4 It must not change the session

With or without a recorder, every real client receives the same bytes (acceptance 2). So: it is excluded from the relevance
pipeline's connection list and from the `server_rate_bytes` sum; it does not hold entities awake and does not cause an asleep refresh;
it consumes its own message sequence; its message bytes are not counted in `MessageCounters`.

### 3.5 Keyframes

On a send tick `T` with `T - last_keyframe >= keyframe_every_ticks` (and at the first recorded tick), `KEYFRAME_PEER` joins the gather
as a new connection with the same view, and is removed before messages are routed. Its section (spawn records for everything, and
`level_removed`) becomes the keyframe, stored before the tick chunk of `T`. Keyframes are never fed to a linear play. A seek feeds the
keyframe and skips the tick chunk of the same tick. A keyframe over `MAX_CHUNK_BYTES` is skipped and counted (`keyframes_skipped`): seeks
fall back to the one before.

Default `keyframe_every_s` is 5 s. `keyframe_every_ticks` is clamped to `MAX_SEGMENT_TICKS` (2,048) so a segment always fits one apply
(`runtime::MAX_PENDING_FRAMES`).

**A keyframe is a join, not a history.** A `SpawnOnly` or `SpawnThenOwner` component that changed after its spawn, or a component
whose audience predicate stopped matching, has the value a client joining at that tick would see. Linear play keeps the older value, as
a client that was there all along does. Only these audience cases differ between a seek and linear play (acceptance 5 covers both sides).
Likewise a level-bound copy out of scope at the keyframe seeks to its level state instead of its last state.

### 3.6 Entity references

A client that decodes a reference to an entity it does not hold yet asks for the referrer's full state when the target spawns, and asks
again when a target despawns (spec 71 § 5). Nobody answers a recording, so the recorder answers in advance: after a frame that spawned or
despawned something, each copy with a reference-bearing component (`RepComponent::has_refs`, from the codec layout) whose value names a
target that just spawned, left this connection's view, or no longer maps goes out again in the next frame as a repeat spawn record. A copy naming only targets that
were already held costs one probe encode and no bytes.

Resync requests the playback client still makes (it asks whenever a reference resolves, even one the next frame answers) are counted (`resync_unanswered`) and dropped. `net.pending_refs` shows what still waits. A keyframe holds references too, so a seek and a playback's first keyframe (the connection may be older than the file, as in a ring's export, and then gains no copy that would heal them) feed a keyframe twice when the registry has a reference-bearing component: the first pass makes every copy, the second resolves references to copies later in the list. A keyframe large enough that both passes could pass the client's pending limit sends the second after the first has applied. A keyframe at tick 0 goes once: there is no earlier tick to stamp its first pass with, and the connection that started there heals on its next frame.

## 4. Recording API

```rust
pub struct RecorderConfig {
    pub view: RecorderView,        // Everything
    pub keyframe_every_s: f32,     // 5.0
    pub max_bytes: u64,            // 2 GiB; 0 = none
    pub max_seconds: f32,          // 4 h; 0 = none
    pub level: String,             // the header's label; empty = the session's level, if any
    pub overwrite: bool,           // false: refuse an existing file
}
pub struct RingConfig { pub view: RecorderView, pub seconds: f32, pub max_bytes: usize, pub keyframe_every_s: f32 }

pub fn start_recording(world, path, RecorderConfig) -> Result<(), NetDemoError>;
pub fn stop_recording(world) -> Result<RecordingSummary, NetDemoError>;
pub fn start_ring(world, RingConfig) -> Result<(), NetDemoError>;
pub fn stop_ring(world) -> Result<RingStatus, NetDemoError>;
pub fn export_ring(world, path, seconds: Option<f32>, overwrite: bool) -> Result<RecordingSummary, NetDemoError>;
pub fn recorder_status(world) -> RecorderStatus;
```

The file and the ring share one connection, so they share one view: asking for a second view is `recorder_view_conflict`. The shorter
interval sets when the connection makes keyframes, and a ring stores only those at least its own interval apart, so a file's shorter interval does not use up the ring's segments. The recorder runs on servers only (`net_not_server`).

## 5. `.netdemo` v1

Full layout in `docs/formats/netdemo.md`. All integers little-endian.

- **Header:** magic `RNETDEMO`, `format_version` u32, body length u32, CRC-32 of the body, body: `net_protocol`, `protocol_hash`, tick rate,
  send cadence, keyframe interval, first tick, view, level, engine version, component table (name, layout hash, audience), message table
  (name, direction, channel).
- **Chunks:** kind u8, tick u32, payload length u32, CRC-32 over those and the payload, payload. `1` keyframe (one frame), `2` tick (a
  frame list), `0xEE` end (counts, last tick, end reason).

Frames are the wire frames of `NET_PROTOCOL_VERSION`, stored verbatim, so a build whose protocol version or hash differs cannot read them
and refuses the file: `netdemo_protocol_mismatch`, with the first differing table row. There is no frame migration: a recording belongs to
its protocol, as a client does (spec 77 § 4). The container's `format_version` follows the frozen-format rules (`specs/12` § 4a, `check-formats`,
`regia upgrade-project` recognizes it and rewrites nothing while the version is current).

Reader limits: a chunk is at most `MAX_CHUNK_BYTES` (64 MiB, `runtime::MAX_PENDING_FRAME_BYTES`), checked against the bytes left before
any allocation; a header body at most 1 MiB; ticks strictly rise; the first chunk is a keyframe; the end chunk is last and its counts match.
`open` walks and checks every chunk, keeping the keyframe index only (16 bytes per keyframe), so a damaged file fails at open, not mid-play.

## 6. File writing

`start_recording` creates `<path>.<pid>.<n>.tmp`, a name no other writer holds. A writer thread owns the file; the tick path copies frames into pooled 256 KiB blocks and
passes them over under a mutex, so memory stays at 4 blocks plus the largest keyframe. A slow disk costs the tick a block handoff until every block is with the writer; it then waits at most `MAX_STALL_MS` (1 s), once, and the recording ends with reason `disk_slow` and
still closes valid. Closing writes the end chunk, syncs, and renames over `path`, all on the writer thread: a cap or a slow disk ending a recording costs the tick no disk wait. `stop_recording` waits for the close, so the file is in place when it returns. A write error removes the tmp file and reports
`netdemo_io` from `stop_recording`. Once the file is whole and synced it is not thrown away: a file that appeared at `path` meanwhile (with `overwrite` off) is `netdemo_exists`, a rename that fails is `netdemo_io`, and both leave the recording at the tmp path their message names. No file ever appears at `path` unfinished.

A world dropped with a file still recording closes it with reason `stopped`, waiting for the writer: every clean exit drops the world (ADR-188), so a clean exit keeps the recording. A process that dies leaves the tmp file.

Caps end the recording cleanly at a tick boundary (`size_cap`, `time_cap`): the file at `path` then holds at most `max_bytes` bytes. A
finished recording stays finished until `stop_recording` collects its summary; `recorder_status` shows `ended`, or `error` for one that failed. A level change (session travel) ends it with `level_changed`:
one file holds one level. `RecorderConfig::level` is only the header's label: the recorder follows the session's level apart from it. A session hosted after the recording began names its level for the first time, which is no change.

## 7. Ring

A fixed `Vec<u8>` allocated at `start_ring`, written circularly, holding tick chunks and keyframes in the file's chunk encoding, and a
fixed-capacity list of segment starts (at most `MAX_RING_SEGMENTS`, 65,536). `max_bytes` is at most `MAX_RING_BYTES` (4 GiB), and memory that is not there is `recorder_config_invalid`. Eviction removes whole oldest segments while the second-oldest starts at or before the window's start, so the window stays covered, and always
when bytes run out. The bytes used never pass `max_bytes`. A single segment larger than the ring empties it and counts `overflows`: the
ring waits for the next keyframe, and an export then has less history than asked. `export_ring` writes a `.netdemo` from the newest
segment start at or before `now - seconds`; the export is synchronous, atomic, does not stop the ring, and the bytes equal what a file recording
of those segments would hold, apart from header fields. A session that travels starts the ring over, so an export holds one level, named from the session.

## 8. Player

`NetReplayPlayer` (a resource in `regia_framework::net_replay`, `NetReplayPlugin`):

```rust
NetReplayPlayer::open(world, path) -> Result<Self, NetDemoError>;  // checks the protocol against this world
play(), pause(), set_speed(f32 in 1/16..=16), seek(tick), seek_seconds(f32), set_camera(ReplayCamera)
advance(world, dt_seconds)   // the app system; tests call it directly
state() -> NetReplayState
close(world)
```

The world is a client world (`NetMode::Client`, `LocalPeer(RECORDER_PEER)`) with the same registrations. `open` refuses a world with a running
session, a server mode or a running recorder (`netdemo_session_active`: its reset would take that world's entities), and calls it a new connection (`client_session_started`). A session started while a replay is open owns the world from then on: the player stops with `netdemo_session_active`, stops clearing the outbox, and its `close` leaves the world's entities and mode alone.

**As built (ADR-8762):** a second `open` while another player of the same world is open is refused with `replay_already_open`, so the mode `close` restores is always the world's own; a player dropped without its close leaves a dead mark, and the next open restores to the mode that player saved. `open_replay` (and `net.replay_open`) replaces the running resource player: it closes it first.
**As built (ADR-8850):** the dead mark is reused only while the world never left the dropped replay (`LocalPeer == RECORDER_PEER`): a session that changed the mode since owns the world, and the next open saves — and its close restores — that session's mode and local peer, not the ones from before the drop. The open's reset (`forget_everything` and `client_session_started`) keeps the host-set `ClientStateChannel::entity_cap`.

- **Feeding.** `advance` moves the playhead by `dt × speed` and pushes the frames of every tick it passed into `NetNode::inbox`. Nothing else
  changes: `net_receive` and `net_apply` handle them in the normal sets, and the world's prefab spawner, interpolation, despawn timing and message
  handlers run as live. Before pushing, each frame is checked with the same walk `client_apply` makes, so a bad frame is `netdemo_corrupt`
  from the player rather than a closed connection.
- **The outbox.** Each advance clears what the client path wrote (acks, resync lists, input) and counts resync ids as `resync_unanswered`.
- **Clock.** The interpolation clock is pure (spec 74, `net_interpolate(world, now_ms)`). The player passes it playback time
  `playhead × ms_per_tick`, as arrival time and as frame time, so pause freezes proxies without starving them and speed scales them.
  `NetInterpolationPlugin`'s real-time systems stand down while the marker resource `NetReplayClock` exists.
- **Seek.** Silent: state only. If the target is ahead and no keyframe lies between it and the playhead, frames are fed forward without their messages.
  Otherwise the world resets (§ 8.1), the keyframe at or before the target is fed and then the tick chunks after it up to the target, without
  messages. Up to `SEEK_FRAMES_PER_APPLY` (2,048) frames or 32 MiB go per fixed tick, so `seeking` can last a few ticks. Unreliable messages are dropped too.
  `generation` counts seeks for code that holds per-copy state.
- **Reaching the end.** The player pauses at the last tick with `ended: true`. A seek continues from there, silently; `play` starts over as the open did, delivering the first tick's messages again.

### 8.1 Reset

Every replicated copy despawns (with its non-replicated children), except level-bound copies, which lose `NetId` and gain `NetUnbound` as on a fresh
join. `ClientReplication` and `NetEntities` are emptied, the node's tick returns to 0, `client_session_started` runs. A level-bound entity
the recording removed before the target cannot come back from the keyframe alone: with `level_restore` set (`fn(&mut World, &str)`, the host reloads
the level by name), it runs on every reset after the open (a seek back, a restart, the close; at the open the world still holds what the recording removed); without it such a copy spawns bare and logs `net.level_entity_missing`, as for a client missing that content.

### 8.2 Camera hook

`ReplayCamera::Free` leaves `SpectatorCamera` to free-fly. `ReplayCamera::Follow(NetId)` resolves the id through `NetEntities` every frame and sets
`SpectatorDrive::follow`, so a follow survives seeks and the copy's despawn. A followed id absent at the playhead keeps the last target until it returns. Add
`SpectatorPlugin` and a `SpectatorView` camera; a game with its own camera reads `state().follow_entity`.

## 9. Agent surface

Guide page `book/agents/net-replay.md` (topic `net-replay`). Errors are `F0002` with `data.reason`.

| Method | Mode | Params | Returns |
|---|---|---|---|
| `net.replay_record_start` | play (server) | `{ path?, ring_seconds?, ring_bytes?, view?, keyframe_s?, max_bytes?, max_seconds?, level?, overwrite? }` | recorder status |
| `net.replay_record_stop` | play (server) | `{ target?: "file"\|"ring"\|"both" }` | `{ file?, ring? }` |
| `net.replay_ring_export` | play (server) | `{ path, seconds?, overwrite? }` | the summary |
| `net.replay_open` | play (client world) | `{ path, paused?, interpolate? }` | header summary and player state |
| `net.replay_seek` | play | `{ tick? , seconds?, fraction? }` | player state |
| `net.replay_play` | play | `{ playing?, speed?, follow? }` | player state |
| `net.replay_close` | play | — | `{ closed }` |
| `net.replay_state` | read | — | `{ recorder, player }` |

Paths are confined as every agent path is (ADR-1816): `path_outside_project`. Each method reads all its parameters before it changes anything.

Codes: `net_not_server`, `net_not_running`, `recorder_busy`, `recorder_idle`, `recorder_view_conflict`, `recorder_view_invalid`, `recorder_config_invalid`, `ring_idle`, `ring_empty`,
`netdemo_exists`, `netdemo_io`, `netdemo_not_a_demo`, `netdemo_version`, `netdemo_protocol_mismatch`, `netdemo_truncated`, `netdemo_corrupt`, `netdemo_session_active`, `replay_closed`, `replay_seek_range`, `path_outside_project`.

## 10. Budgets

- Recorder path per send tick, on top of one extra connection's gather (span `net_recorder`): at most one block handoff and a copy of the frames; **no allocation after warm-up**. Measured by a counting allocator, not by time. The gather's cost for one more unbudgeted connection is spec 72's, and it does allocate per tick as it does for every real client (`whole_ticks_with_a_recorder_do_not_grow` prints the figure): what this budget covers is the recorder's own path.
- A keyframe tick (every `keyframe_every_s`, not every tick) costs one join's gather and one frame's buffer (span `net_recorder_keyframe`).
- File memory: 4 × 256 KiB plus the largest keyframe. Ring memory: exactly `max_bytes` plus the segment list.
- Player: holds one chunk at a time plus the keyframe index; a seek costs its segment's applies (span `net_replay_seek`).
- Android: same code; a listen host may record. Thread use is one writer per recording.

## 11. Out of scope (deferred)

Recording a client's own stream; compression of chunks; a demo spanning a level change; playing a recording of an older protocol; prediction or a local player in playback; a UI scrub bar widget (the `demo_scrub` maths apply, `state()` gives what a bar needs).

## 12. Acceptance

Loopback hub, fixed steps, no wall clock.

1. **Digest.** A server and 2 clients, 300 ticks with spawns, despawns, ownership changes and messages: playback's per-tick digest of replicated components equals the live non-owner client's digest at every tick, and the delivered messages match.
2. **Transparency.** Every real client's frames are byte-identical with a recorder and without one: both wires, with and without the relevance pipeline, a ring, a player's view, a start and a stop mid-session, and under the server-wide rate limit.
3. **Non-owner.** `OwnerOnly` values and owner-addressed messages are absent from a recording.
4. **Views.** `Everything` ignores relevance; `Player(peer)` matches that peer's held set per tick; a departed peer's view freezes, with and without the relevance pipeline.
5. **Seek.** Backward and forward seeks give the digest of linear play at every probed tick. A `SpawnOnly` value after its change shows the § 3.5 difference.
6. **Hostile files.** Every truncation length and single-bit flip of a real recording gives a typed error or a clean open, and never a panic; a lying length allocates nothing of its size (measured); a fuzz target runs it in the gate, each input also with its checksums made right, so mutations reach the header's tables and the frames.
7. **Protocol.** A world with another registration is refused with `netdemo_protocol_mismatch`, naming the row.
8. **Ring.** Bytes stay within the bound over a long session with varying load; the export plays and matches the live digest; an over-large segment counts an overflow and recovers; a file's shorter keyframe interval leaves the window whole; travel starts it over; a size or window asking for too much is refused or bounded, never an abort.
9. **Allocation.** The recorder path allocates nothing per tick after warm-up (counting allocator; file and ring).
10. **Caps and atomicity.** Size and time caps end with a valid file within the cap, and starting that close does not wait for a stuck disk. No file at `path` before close. A write error leaves none. A file that appeared at `path` is kept, and so is the recording. Dropping the world keeps the recording. A label ends nothing; travel ends the file.
11. **Interpolation, spawn and despawn.** With the replay clock, spawn reveal, despawn timing and poses match a live client with the same delay; pause freezes; speed scales.
12. **References.** A reference to a target spawned later resolves in playback, on a first keyframe too; one to a despawned target, or one that left a player's view, clears.
13. **Prefabs.** Prefab copies come from the world's spawner.
14. **Agent.** Every method round-trips; every code is produced by a test; a path outside the roots is refused; a refused call changes nothing; a failed close reaches `net.replay_record_stop` and frees the recorder; `book_cli_coverage`, `fuzz_targets` and `check-formats` pass.
15. **Takeover.** A server world or one that records is refused at open; a session started during a replay stops the player, keeps its traffic and keeps its entities at the close.

## 13. Provenance

Public sources: G. Fiedler, "Snapshot Compression" and "State Synchronization", Gaffer on Games (keyframe plus delta chains; apply order); Valve Developer Community, "Source Multiplayer Networking" (a recording as the server-to-client stream); Y. Bernier, "Latency Compensating Methods", GDC 2001 (interpolation delay in playback). Keyframe-plus-delta seeking is the group-of-pictures structure of video coding (ITU-T H.264, public). The virtual connection, the audience rules on it, transparency and reference healing are original Regia design. Nothing is read from another engine; no public-sources spec-set file was used.

## 14. Extension: client capture with timing (ADR-8221, spec 120 § 15.5)

The recorder above has no socket and no timing, so it cannot show why one client lagged, and § 11 left a client's own stream out.
A client capture is that half, in `regia_net::capture` (core, no ECS). A `NetClient` with a `CaptureTap` (`NetClient::set_capture`) logs, in
its own milliseconds: a session record per connection attempt (game id, protocol id), each `receive` and `flush` time, the server's accept
(with whether the link was sealed), refusal and goodbye, each connection packet after decryption and the token check, each `send_input`
stamp with the encoded input, each end code, and the game's frame markers (`CaptureTap::frame(at_ms, digest)`: its frame time and a digest
of its output). Records go into a ring of exactly `max_bytes` bytes (4 KiB to 1 GiB, default 16 MiB) allocated once and written circularly;
eviction removes the oldest records, and a save keeps whole sessions only. `ClientCapture::save` writes a `.netcapture` atomically (a
unique tmp file, synced, renamed; `docs/formats/netcapture.md`).

`NetClient::replay(socket, config, ReplayFeed)` reads one captured session instead of its socket: each `receive` takes the records of the
next captured receive, so the same packets reach the same connection at the same milliseconds; it sends nothing, runs no handshake (the
captured accept connects it, at the captured header size) and no connect timeout. A second tap on the replaying client and
`capture::compare_sessions` check it: every record but the fed ones (receive and flush times, input stamps and bytes, ends, frame digests)
must be equal, and the first difference is named. Codes: `capture_config_invalid`, `capture_io`, `capture_exists`, `capture_not_a_capture`,
`capture_version`, `capture_truncated`, `capture_corrupt`, `capture_empty`, `capture_protocol_mismatch`.

Out of scope here: an agent method to start a capture on a running client (a host attaches the tap in code), replaying an ECS world through
`NetLink`, and capturing the server side (the recorder above does that).
