# Network replay (agents)

Record what a server sends, to a file or to a memory ring, and play it back with no server (`regia_net::recorder`,
`regia_framework::net_replay`, spec 103, ADR-3021). Guide topic `net-replay`. Not the local replay system: [demo recording](demo.md) stores input
and simulates again; this stores the replicated stream and applies it.

## Recording

`net.replay_record_start` adds a virtual connection on the server. It is a non-owner, so `OwnerOnly` components and owner-addressed messages are
not recorded. It has no budget: every change goes in on every send tick. Real clients receive the same bytes with it and without it,
and `net.peers` does not list it.

```json
{ "method": "net.replay_record_start", "params": { "path": "match.netdemo", "keyframe_s": 5, "max_bytes": 1073741824 } }
{ "method": "net.replay_record_stop" }
```

- The file appears at `path` only when it closes. A size or time cap ends it cleanly inside the cap: `net.replay_state` shows `ended` and `net.replay_record_stop` collects it. A level change ends it too (one file holds one level).
- A size cap, a time cap or a slow disk closes the file on the writer thread, so the server's tick never waits for the disk; `net.replay_record_stop` waits for that close and reports how it went. A world dropped with a file still recording closes it the same way, so a clean exit keeps the recording. A crash leaves `<path>.<pid>.<n>.tmp`, with what reached the disk.
- A file that appeared at `path` during the recording is not replaced unless `overwrite` is on, and a rename that fails does not delete the recording: both errors name the tmp path where the whole file stays.
- `level` is a label for the header. What ends a recording is the session's level moving, whatever the label says.
- Paths follow the [agent path rules](safety.md): relative to the project, or under the project, the temp directory or an `output_roots` entry.
- `view`: `everything` (default, skips relevance) or `player:<peer>`: the set that peer holds at each tick, with its hysteresis and linger. If the peer leaves, the set freezes (`view_peer_gone`).

### Instant replays and kill-cams

A ring keeps the last seconds in a fixed block of memory, and an export writes them out while it keeps running:

```json
{ "method": "net.replay_record_start", "params": { "ring_seconds": 30, "ring_bytes": 16777216 } }
{ "method": "net.replay_ring_export", "params": { "path": "killcam.netdemo", "seconds": 10 } }
```

The export starts at the newest keyframe at or before that point, so it can hold up to `keyframe_s` more. A file and a ring can run together on one connection, with one view; the connection makes keyframes at the shorter interval, the ring keeps those at least its own `keyframe_s` apart, so a file's interval does not shorten the ring's history. A session that travels starts the ring over, so an export holds one level. `ring_bytes` is at most 4 GiB, allocated whole at the start. A segment larger than the ring empties it and counts `overflows`; the ring recovers at the next keyframe.

## Playback

`net.replay_open` plays a file in a world with no running session, with the same components and messages registered in the same order. No server runs. Frames go through the normal client path, so copies spawn from prefabs, proxies interpolate, despawns wait for their tick and message handlers run.

```json
{ "method": "net.replay_open", "params": { "path": "match.netdemo" } }
{ "method": "net.replay_seek", "params": { "fraction": 0.5 } }
{ "method": "net.replay_play", "params": { "speed": 2.0, "follow": 12 } }
{ "method": "net.replay_state" }
```

- **Seek** goes to the nearest keyframe and applies deltas forward. It is silent: state only, no messages. It lands on the next frame (a very long segment takes a few); wait for `seeking: false`. `generation` counts seeks.
- **Speed and pause** move the interpolation clock too: a pause freezes proxies without starving them. At the last tick the player pauses with `ended: true`; `playing: true` starts over from the first tick, with its messages. `net.replay_close` ends the replay: the copies go and the world's network mode comes back. A session started while a replay is open takes over: the player stops with `netdemo_session_active` and leaves the session's entities and traffic alone.
- **Camera:** `follow` is a `NetId`, and it survives seeks and the copy's despawn. `null` is a free camera. Both drive `SpectatorCamera`: add `SpectatorPlugin` and a `SpectatorView` camera. A game with its own camera reads `follow_entity`.
- **`resync_unanswered`** counts copies the client path asked the absent server to send again. A recording answers these in advance for references it can see (a target that spawned or despawned), so a small count is normal; `net.pending_refs` shows what is still waiting.

### What differs from linear play after a seek

A keyframe is what a client joining at that tick receives. A `SpawnOnly` or `SpawnThenOwner` component that changed after its spawn shows the new value after a seek and keeps the old one in linear play, and a component whose predicate audience stopped matching is absent after a seek. Likewise a level-bound copy that was out of scope seeks to its level state.

A level-bound entity the recording removed cannot come back from a keyframe: host code that reloads the level passes `ReplayOptions::level_restore` to `regia_framework::net_replay::open_replay`. Without it such a copy spawns bare and logs `net.level_entity_missing`.

## Methods

| Method | Mode | Params | Returns |
|---|---|---|---|
| `net.replay_record_start` | play (server) | `{ path?, ring_seconds?, ring_bytes?, view?, keyframe_s?, max_bytes?, max_seconds?, level?, overwrite? }` | the recorder's state, as `net.replay_state`'s `recorder` |
| `net.replay_record_stop` | play (server) | `{ target? }`: `file`, `ring` or `both` | `{ file?: { path, bytes, ticks, keyframes, first_tick, last_tick, reason }, ring? }` |
| `net.replay_ring_export` | play (server) | `{ path, seconds?, overwrite? }` | `{ path, bytes, ticks, keyframes, first_tick, last_tick, reason }` |
| `net.replay_open` | play | `{ path, paused?, interpolate? }` | `{ header: { protocol_hash, level, engine_version, view, send_every_ticks, keyframe_every_ticks, components, messages }, player }` |
| `net.replay_seek` | play | exactly one of `{ tick }`, `{ seconds }`, `{ fraction }` | the player's state |
| `net.replay_play` | play | `{ playing?, speed?, follow? }` | the player's state |
| `net.replay_close` | play | none | `{ closed }` |
| `net.replay_state` | read | none | `{ recorder: { running, view, view_peer_gone, file?, ring?, keyframes_skipped, ticks_skipped, scratch_grows, ticks }, player: null \| { tick, first_tick, last_tick, tick_hz, seconds, total_seconds, playing, ended, seeking, speed, generation, camera, follow_entity, keyframes, resync_unanswered, error } }` |

`scratch_grows` counts how often a working buffer of the tick path grew: flat after warm-up. `keyframes_skipped` counts keyframes over the 64 MiB chunk limit; a seek falls back to the one before. `ticks_skipped` counts ticks over that limit: a keyframe follows at once and playback jumps to it. `file` holds `ended` once a cap, a slow disk or a level change ended it, and `error` once a write failed.

## Errors

`F0002` with `data.reason`.

| Code | When | Do |
|---|---|---|
| `net_not_running` | The app has no net runtime | Add `NetPlugin` |
| `net_not_server` | Recording on a client | Record on the server's world |
| `recorder_busy` | That kind already runs, or a recording that ended by itself waits to be collected | `net.replay_record_stop` |
| `recorder_idle` | No file recording runs | `net.replay_record_start` |
| `recorder_view_conflict` | The file and the ring share a connection and a view | Use the running view, or stop it |
| `recorder_view_invalid` | `view` is not `everything` or `player:<peer>` with a connected peer | `net.peers` lists them |
| `recorder_config_invalid` | A setting is out of range; the message names it | See Methods |
| `ring_idle` | No ring runs | `net.replay_record_start { ring_seconds }` |
| `ring_empty` | The ring holds no whole segment yet | Export after its first send tick |
| `netdemo_exists` | A file is at the path and `overwrite` is off. At a stop: it appeared during the recording, and the message names the tmp path that holds the recording | Another path, or `overwrite: true` |
| `netdemo_io` | A file operation failed; nothing appeared at the path. If only the rename failed, the message names the tmp path that holds the recording | Check the path and the disk |
| `path_outside_project` | `path` is absolute, has `..` or leaves the allowed roots (`data.path_reason`) | See [safety](safety.md) |
| `netdemo_not_a_demo` | The file is not a `.netdemo` | Open a recording |
| `netdemo_version` | Its `format_version` is not one this build reads | `docs/formats/netdemo.md` |
| `netdemo_protocol_mismatch` | It belongs to another protocol; the message names the first differing row | Play it with the build that recorded it |
| `netdemo_truncated` | The file ends early | It was cut after it closed: record again |
| `netdemo_corrupt` | A checksum, a length or a frame is wrong (at open, or as `error` in `net.replay_state` mid-play) | Record again |
| `netdemo_session_active` | This world hosts, joined or records a session, or one started while a replay was open | Leave it first, or open the replay in a client world |
| `replay_closed` | No replay is open | `net.replay_open` |
| `replay_seek_range` | The target is outside the recording | `net.replay_state` gives the range |
| `replay_already_open` | A `NetReplayPlayer` of this world is open: a second `NetReplayPlayer::open` is refused (ADR-8762). `net.replay_open` replaces the running replay instead | Close the open player first |

## See also

[Net replication (agents)](net-replication.md), [Net interpolation (agents)](net-interpolation.md), [Demo recording](demo.md), the format [`.netdemo`](../../docs/formats/netdemo.md).
