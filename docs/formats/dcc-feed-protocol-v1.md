# DCC scene feed wire protocol, version 1

Normative description of the protocol that `regia_agent::dcc_feed::wire` implements (spec 60 § 4).
The protocol is a development-time channel. It stores no project data, so `regia upgrade-project`
has nothing to migrate; only `.rfdrec` recordings carry a version (see `rfdrec.md`).
Within major version 1, changes are additive only: new message types, new mesh section ids, new
trailing payload fields. A breaking change bumps `PROTOCOL_VERSION` to 2.

## Conventions

- All integers little-endian. Floats are IEEE 754 binary32; every float must be finite.
- A string is a `u16` byte length followed by that many UTF-8 bytes (no NUL).
- A payload may carry trailing bytes after the fields a receiver knows; fields are only appended.
- Quaternions are `x, y, z, w`. One within 1e-3 of unit length is accepted; a longer deviation is
  normalized (the receiver counts a warning); zero length is `invalid_value`.

## Frame header (12 bytes)

| Offset | Type | Field |
|---|---|---|
| 0 | u32 | `len`: bytes after this field, `8 + payload length` |
| 4 | u16 | `msg_type` |
| 6 | u16 | `flags`: bit0 `FRAGMENT_MORE`, bit1 `OPTIONAL`, bit2 `COMPRESSED` (reserved, must be 0) |
| 8 | u32 | `seq`: per-direction counter starting at 1 |

A fragmented message is the concatenation of consecutive frames of one `msg_type` up to the first
frame without `FRAGMENT_MORE`. Only `ping`, `pong` and `ack` may interleave. The accumulated size is
checked against `max_message_bytes` while reading. An unknown `msg_type` without `OPTIONAL` is
`unsupported_message` (fatal).

## Messages

`FrameInfo` = `u64 frame_id`, `u64 source_time_ns`, `u8 has_timecode` (0 or 1), and when 1:
`u8 hh, mm, ss, ff`, `u16 rate_num`, `u16 rate_den`, `u8 drop_frame`. `frame_id` strictly increases
per item.

`TransformBlock` = `f32[3] translation`, `f32[4] rotation`, `f32[3] scale`, `u8 space_flags`
(bit0: world space instead of parent-local). 41 bytes.

| id | name | direction | payload |
|---|---|---|---|
| 0x0001 | `hello` | C to S | `"RFDP"`, `u32 min_version`, `u32 max_version`, str `client_name`, str `client_version`, str `dcc`, str `space` (`gltf` or `regia`), `f32 unit_scale` (> 0), `u32 caps`, str `token` |
| 0x0081 | `hello_ack` | S to C | `u32 version`, `u64 session_id`, `u8 accepted`, `u32 caps`, limits block, `u32 window_bytes`, str `server_name` |
| 0x0010 | `item_declare` | C to S | `u32 handle` (non-zero), str `key` (1 to `max_key_len` bytes), `u8 kind` (1 transform, 2 mesh, 3 camera, 4 light, 5 skeleton, 6 generic), `u32 parent_handle` (0 none), str `display_name`, `u8 variant`, and for kind 6: `u16 n` then n strings |
| 0x0011 | `item_delete` | C to S | `u32 handle` |
| 0x0020 | `transform` | C to S | `u32 handle`, FrameInfo, TransformBlock |
| 0x0021 | `camera` | C to S | `u32 handle`, FrameInfo, TransformBlock, `u8 mode` (0 perspective, 1 orthographic), 4 floats (`yfov_rad, aspect, near, far` or `xmag, ymag, near, far`), 5 floats physical block (`focal_mm, sensor_w_mm, sensor_h_mm, focus_m, f_stop`; all 0 = absent) |
| 0x0022 | `light` | C to S | `u32 handle`, FrameInfo, TransformBlock, `f32[3] color`, `f32 intensity`, `f32 range`, `f32 spot_inner_rad`, `f32 spot_outer_rad` |
| 0x0023 | `mesh` | C to S | `u32 handle`, FrameInfo, `u8 update_mode` (0 full, 1 vertex-only), `u16 section_count`, sections |
| 0x0024 | `skeleton_static` | C to S | `u32 handle`, `u16 joint_count`, per joint: str `name`, `i32 parent` (-1 root, otherwise an earlier joint), `f32[3] t`, `f32[4] r`, `f32[3] s` |
| 0x0025 | `pose` | C to S | `u32 handle`, FrameInfo, `u8 layout` (0 TRS, 1 scalar), `u16 count`, then `count * 10` floats (TRS per joint: t, r, s) or `count` floats |
| 0x0026 | `clip_push` | C to S | str `name`, `f32 fps`, `f32 duration_s`, `u8 loop`, `u32 bind_handle`, `u32 track_count`, per track: str `target`, `u8 property` (0 translation, 1 rotation, 2 scale), `u8 interp` (0 step, 1 linear), `u32 key_count`, `f32[key_count]` times, `f32[key_count * (4 for rotation, else 3)]` values |
| 0x0027 | `texture_push` | C to S | str `name`, `u8 colorspace` (0 srgb, 1 linear, 2 normal), `u8 source`: 0 then str path; 1 then `u8 container` (0 PNG, 1 JPEG, 2 EXR, 3 KTX2), `u32 byte_len`, bytes; 2 then `u8 pixel_format` (0 rgba8, 1 rgba16f, 2 rgba32f), `u32 w`, `u32 h`, `u32 byte_len` (must equal `w * h * bytes per pixel`), bytes |
| 0x0030 | `ping` | both | `u64 token`, `u64 sender_time_ns` |
| 0x0031 | `pong` | both | `u64 token`, `u64 sender_time_ns`, `u64 responder_time_ns` |
| 0x0082 | `ack` | S to C | `u32 seq_applied`, `u32 credit_bytes` |
| 0x0083 | `session_state` | S to C | `u8 state`, str `reason` |
| 0x0084 | `resync` | S to C | `u32 handle` (0 = every item) |
| 0x003F | `error` | both | `u32 ref_seq`, `u32 handle`, str `code`, str `message`, `u8 fatal` |
| 0x0032 | `goodbye` | both | str `reason` |

Limits block in `hello_ack` (in order): `u32 max_message_bytes`, `max_vertices`, `max_indices`,
`max_items`, `max_joints`, `max_clip_tracks`, `max_keys_per_track`, `max_texture_dim`,
`max_texture_bytes`, `u64 max_transient_texture_bytes`, `u32 max_key_len`.

Notes where the byte layout differs from, or refines, the prose of spec 60 (ADR-1669):

- `pose` carries `u8 layout` between FrameInfo and the count (the spec names the field without a
  position).
- `goodbye` carries its reason string.
- A mesh section `count` counts items (vertices, indices, ranges), not scalars; its `byte_len` must
  equal `count * bytes per item`.

## Mesh sections

Each section: `u8 section_id`, `u8 elem` (0 f32, 1 u8 normalized, 2 u16, 3 u32), `u16 index`,
`u32 count`, `u32 byte_len`, bytes. Unknown section ids are skipped by `byte_len`.

| id | content | per item | elem |
|---|---|---|---|
| 1 | positions | 3 x f32 (12 bytes) | f32 |
| 2 | normals | 3 x f32 | f32 |
| 3 | tangents xyz + sign | 4 x f32 | f32 |
| 4 | uv, `index` 0 to 3 | 2 x f32 | f32 |
| 5 | color, `index` 0 | 4 x f32 or 4 x u8 | f32 or u8 |
| 6 | indices (triangle list, multiple of 3, each below the vertex count) | 1 | u16 or u32 |
| 7 | submesh ranges | `u32 slot, first_index, index_count` | u32 |
| 8 | material slots | variable | any |
| 9 | joints | 4 x u16 | u16 |
| 10 | weights | 4 x f32 | f32 |

Per-vertex sections must have the position count. A full update (mode 0) needs positions and
indices. A vertex-only update (mode 1) must not carry indices.

## Error codes

`bad_magic`, `unsupported_version`, `auth_failed`, `not_accepted`, `frame_too_large`,
`limit_exceeded`, `malformed_frame`, `unsupported_message`, `unknown_item`, `item_conflict`,
`kind_mismatch`, `kind_not_supported`, `topology_mismatch`, `invalid_value`, `path_denied`,
`overloaded`, `timeout`, `internal`. Fatal (the session closes after the error is sent):
`bad_magic`, `unsupported_version`, `auth_failed`, `frame_too_large`, `malformed_frame`,
`unsupported_message`, `internal`.

## Worked example

A `transform` for handle 1, frame 9, no timecode, translation (1, 2, 3), identity rotation, unit
scale, world space, `seq` 1. The payload is 4 (handle) + 17 (FrameInfo without timecode) + 41
(TransformBlock) = 62 bytes, so `len` = 8 + 62 = 70 and the frame is 74 bytes:

```
46 00 00 00                                       len = 70
20 00                                             msg_type 0x0020
00 00                                             flags
01 00 00 00                                       seq 1
01 00 00 00                                       handle 1
09 00 00 00 00 00 00 00                           frame_id 9
e8 03 00 00 00 00 00 00                           source_time_ns 1000
00                                                has_timecode
00 00 80 3f  00 00 00 40  00 00 40 40             translation 1.0 2.0 3.0
00 00 00 00  00 00 00 00  00 00 00 00  00 00 80 3f   rotation 0 0 0 1
00 00 80 3f  00 00 80 3f  00 00 80 3f             scale 1 1 1
01                                                space_flags: world
```

The test `hand_laid_out_transform_decodes` builds exactly these bytes from the tables above and
checks the reference encoder reproduces them.

## Transports

| Transport | Address | Auth |
|---|---|---|
| Unix domain socket | `.regia/dcc_feed/feed.sock`, mode 0600 | none: OS permissions are the check |
| TCP | `127.0.0.1:<port>`, listed in `.regia/dcc_feed/endpoint.json` | `hello.token` |
| WebSocket | `ws://127.0.0.1:<port>/dcc_feed`, opt-in with `listen = ["ws:127.0.0.1:0"]` | `hello.token`; `Origin` absent or in `allowed_origins` |

`endpoint.json` is `{ "protocol": "regia-dcc-feed", "version": 1, "transports": [{ "transport", "address" }] }`. The token is in
`.regia/dcc_feed/token`, mode 0600. The Windows named pipe is a follow-up.

Subprotocol `regia-dcc-feed.v1` carries one wire message per binary WebSocket message: the frame without its `u32 len`, so
`u16 msg_type`, `u16 flags`, `u32 seq`, payload. Subprotocol `regia-dcc-feed-json.v1` carries one JSON object per text message.

## JSON variant

`t` selects the message. Mesh, texture, clip and pose are binary only (`kind_not_supported`). Every message passes through the
binary codec on the way in, so it takes the same checks.

| `t` | Fields |
|---|---|
| `hello` | `client_name`, `client_version`, `dcc`, `space`, `unit_scale`, `caps`, `token`, `min_version`, `max_version` |
| `item` | `handle`, `key`, `kind` (name), `parent`, `display_name`, `variant` |
| `delete` | `handle` |
| `transform` | `handle`, `frame_id`, `time_ns`, `t: [x, y, z]`, `quat: [x, y, z, w]`, `s: [x, y, z]`, `world` |
| `camera` | as `transform`, plus `yfov`, `aspect`, `near`, `far` |
| `light` | as `transform`, plus `color`, `intensity`, `range`, `inner`, `outer` |
| `ping` | `token`, `time_ns` |
| `pong`, `error`, `goodbye` | as the binary message |

Server to client: `hello_ack`, `ack`, `session_state`, `resync`, `pong`, `error`, `goodbye`.

## Session rules

A session's first message must be `hello`. An unknown client name gets `hello_ack { accepted: false }`; declarations are held and
data frames dropped until the accept. Either way a `resync` with handle 0 follows the accept: send the full state then.
