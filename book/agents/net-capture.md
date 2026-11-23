# Net client capture (agents)

Record what one client of a `NetLink` game received and did, in its own milliseconds, and replay it in a fresh world
with no server (`regia_net::capture`, `NetLink::set_capture`, `NetLink::replay`; spec 120 § 15.5, spec 103 § 14,
ADR-8221, ADR-8890). Guide topic `net`. Use it when one player lags, rubber-bands or sees a wrong state and nobody
else does: capture that player's client, save the file, and step through the exact corrections and proxy frames they
saw.

Not the server recording of [Network replay](net-replay.md): a `.netdemo` holds what a server sent to everyone; a
`.netcapture` holds what one client received after its network did its worst (loss, reorder, duplicates and
timing), and its replay runs that client's prediction and interpolation again.

## Methods

| Method | Params | Result |
|---|---|---|
| `net.capture_start` | `ring_bytes?` (4096 to 1073741824, default 16777216) | `{ role, replaced, capture: { ring_bytes, used_bytes, peak_used_bytes, records, written, evicted, oversized, sessions, head_cut } }` |
| `net.capture_save` | `path`, `label?`, `overwrite?` (default false), `stop?` (default false) | `{ path, bytes, records, sessions, stopped, capture }` |

```json
{ "method": "net.capture_start", "params": { "ring_bytes": 16777216 } }
{ "method": "net.capture_save", "params": { "path": "lag-report.netcapture", "label": "player 3 rubber-banding" } }
```

- `net.capture_start` attaches a new ring to the app's `NetLink`. On a connected client it starts a new session at
  once. On an idle link it waits for the client the link starts next (`role: "idle"`), so start it before
  the game connects to capture the whole connection. A running capture is replaced, its records dropped (`replaced: true`).
- The ring is allocated once and never grows. When it is full it evicts whole records, oldest first, and a save keeps
  whole sessions only. At the soak's rate a client writes about 96 KiB per second of play, so the 16 MiB default
  holds about three minutes.
- A reconnect or a resume (spec 120 § 10.3) writes its new connection into the same ring as a new session. The tap
  stays with the link after the client ends, so you can save it after a disconnect.
- `net.capture_save` writes the file atomically (a temporary file, synced, then renamed): a reader never sees half a
  capture. The capture keeps running unless `stop` is true.
- Paths follow the [agent path rules](safety.md): relative to the project, or under the project, the temp directory or
  an `output_roots` entry.
- A capture holds decrypted traffic: treat the file as private game data.

## What a capture holds

Per session: the game id and protocol, every receive and flush time of the client's link clock, the server's accept
(and whether the link was sealed) or deny, the handshake's shortest round trip in front of that accept (a replay
runs no handshake and anchors its clock with it), every connection packet after decryption and the token check, the
server's goodbye, the client's end, the inputs it stamped, and the game's frame markers. Format: `.netcapture` v1
(`docs/formats/netcapture.md`); see also [Networking: capturing a client](../user-guide/networking.md).

A frame marker is a digest the game computes once a frame of what it showed and predicted (its proxies'
interpolated state, its predicted state, its correction counters). Call `NetLink::capture_frame(digest)` after the
frame's update; chain it over the frames so one value at the end says the whole run matched. Without frame markers a
replay still compares the receive and flush times, inputs and the end.

## Replaying a capture

In code (a test or a tool), with the captured app's setup:

<!-- ignore-reason: `MyGame`, `world` and `captured_link_options` are the captured game's own type, app and link options -->
```rust,ignore
let file = regia_net::capture::CaptureFile::load(path)?;
let feed = regia_net::capture::ReplayFeed::new(file.sessions().remove(0))?;
let mut link = world.resource_mut::<regia_net::NetLink>();
link.replay::<MyGame>(feed, &captured_link_options)?;
link.set_capture(Some(regia_net::capture::CaptureTap::new(16 << 20)?)); // to compare
```

Then update the app once per captured receive, calling `capture_frame` with the same digest. The replay client reads
the captured packets at the captured times, runs the bridge, replication, prediction and interpolation on them, and
sends nothing. `NetLink::replay_progress()` counts receives at another time than the captured one (`time_mismatches`,
0 when the app updates as often as the captured one with a `LinkClock::Manual` clock) and receives past the capture's
end (`overruns`). `regia_net::capture::compare_sessions(captured, replayed)` names the first record that differs.
A capture made over a transport whose datagram size differs from the default (a relay, a vendor transport) replays
with `NetLink::replay_on` and a `ReplaySocket::with_max_datagram(bytes)` of that size, so the replay cuts its packets
as the capture did.

The time harness does all of this: `TimedHub::capture_client(i, ring_bytes, digest)` captures a client of a virtual-time
session, and `regia_net_soak::timed::replay_client::<G>(session, setup, configure, digest)` replays it and compares.
`cargo test -p regia_net_soak --test link_capture_replay -- --nocapture` runs a bad-wifi session through both.

## Errors

Refusals are `F0002` with `data.reason`:

| Code | Meaning |
|---|---|
| `capture_no_client` | The app has no `NetLink`, or its `NetLink` hosts a server (`net.capture_start`). |
| `capture_not_started` | No capture runs on this app's `NetLink` (`net.capture_save`). |
| `capture_config_invalid` | `ring_bytes` is outside 4096 to 1073741824 (`net.capture_start`). |
| `capture_exists` | A file exists at `path` and `overwrite` is off (`net.capture_save`). |
| `capture_io` | A file operation failed; nothing appeared at `path` (`net.capture_save`). |
| `capture_empty` | The ring holds no whole session: nothing connected yet, or the ring evicted the start of every session (`net.capture_save`). |
| `path_outside_project` | `path` leads outside the allowed roots (`net.capture_save`). |

`NetLink::replay` refuses a capture of another game or protocol with `capture_protocol_mismatch` and a link that
already runs with `link_running`; reading a damaged file gives `capture_not_a_capture`, `capture_version`,
`capture_truncated` or `capture_corrupt` (`docs/formats/netcapture.md`).

## Limits

- The replay is of one client: the server is not replayed, and what the client sent is counted, not compared byte
  for byte (the server's answers in the capture already reflect it).
- The app being replayed must register the same components, messages and systems in the same order as the captured
  one (the protocol check catches a different registration; a different game system shows as a frame digest that
  differs).
- A wall-clock `LinkClock::Wall` capture replays at the captured times only if the replaying app updates at the same
  times; use a manual clock for exact replays.
- No method replays a capture inside a running game: replay is a test and tool API (above).
