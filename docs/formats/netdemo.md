# `.netdemo` — network recording (binary)

Owner: `regia_net::recorder` (spec 103, ADR-3021). Current `format_version`: **1**.

What a server sent one virtual non-owner connection, in the wire frames of one network protocol, with keyframes for seeking. Written by
`regia_net::recorder::{stop_recording, export_ring}`; read by `regia_net::recorder::NetDemoReader` and played by
`regia_framework::net_replay::NetReplayPlayer`. Not the local replay system's [`.regiademo`](regiademo.md) or [`.frec`](frec.md), which hold input and
re-simulate.

## Layout

All integers little-endian.

| Offset | Size | Field |
|---|---|---|
| 0 | 8 | magic `b"RNETDEMO"` |
| 8 | 4 | `format_version` (u32) |
| 12 | 4 | header body length `n` (u32, at most 1 MiB) |
| 16 | 4 | CRC-32 (IEEE) of the body |
| 20 | n | header body |
| 20 + n | ... | chunks |

Header body, in order (`str` is a u16 byte length, at most 4,096, and UTF-8):

```text
net_protocol: u32            NET_PROTOCOL_VERSION of the recording build
protocol_hash: u64           runtime::protocol_hash of the recording world
tick_hz: f64                 server ticks per second, finite, in (0, 100000]
send_every_ticks: u32        at least 1
keyframe_every_ticks: u32    at least 1
first_tick: u32              the tick of the first keyframe
view_kind: u8                0 everything, 1 player
view_peer: u32               the player's peer id (0 for everything)
level: str                   the level; one file holds one level
engine_version: str
components: u32 count, then { name: str, layout: u64, audience: str }     the registered components, in order
messages:   u32 count, then { name: str, direction: str, channel: str }   the registered messages, in order
```

Chunk:

| Offset | Size | Field |
|---|---|---|
| 0 | 1 | kind |
| 1 | 4 | tick (u32) |
| 5 | 4 | payload length (u32, at most 64 MiB) |
| 9 | 4 | CRC-32 (IEEE) of bytes 0..9 and the payload |
| 13 | ... | payload |

| Kind | Payload |
|---|---|
| `1` keyframe | One `TICK` frame with the full state at its tick: what a client joining then receives, without messages. Stored before the tick chunk of the same tick. |
| `2` tick | The frames of one send tick, verbatim, each `{ channel: u8, length: u32, frame }`. Channels: 0 reliable ordered, 1 reliable unordered, 2 unreliable, 3 unreliable sequenced. |
| `0xEE` end | Last chunk: `tick_chunks: u64, keyframes: u64, last_tick: u32, reason: u8` (0 stopped, 1 size cap, 2 time cap, 3 level changed, 4 disk slow, 5 ticks exhausted). Its tick field is `last_tick`. |

Frames are private to `NET_PROTOCOL_VERSION` (spec 71 § 11). A frame's own tick equals its chunk's.

## What a reader checks

At open, all of it: magic (`netdemo_not_a_demo`); version (`netdemo_version`); lengths against the bytes left, before any allocation
(`netdemo_truncated`, or `netdemo_corrupt` over a limit); both CRCs (`netdemo_corrupt`); the first chunk is a keyframe at `first_tick`;
ticks strictly rise, and keyframes too; every frame header is whole and names its chunk's tick; the end chunk is last and its counts match.
A file with no end chunk is `netdemo_truncated`. A player then checks the protocol against its world
(`netdemo_protocol_mismatch`), and walks each frame as the client path does before feeding it (`netdemo_corrupt`).

## Versioning

`format_version` covers this container. There is no frame migration: a recording belongs to its protocol, as a client does, and a build
with another `NET_PROTOCOL_VERSION` or `protocol_hash` refuses it. `regia upgrade-project` lists `.netdemo` files and rewrites none.
`cargo xtask check-formats` holds the layout through `netdemo_schema_fingerprint`. A new chunk kind or field is a version bump with a reader for
version 1.

## Fixture

`regia_net::recorder::format::fixture_netdemo` builds a small valid file; the fuzz target `netdemo` starts from it.
