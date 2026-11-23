# Spec 60: DCC scene feed

Status: proposed (ADR-1579). Target rung: v0.4.x (master plan "not yet placed" row for this feature).
Crates: `regia_agent` (protocol server, layer 3), `regia_animation` (pose target, layer 2), `regia_editor` (panel,
ownership rules, layer 6), `regia_asset_pipeline` (bake, layer 5), `regia_cli` (reference client, layer 6).
Reviewed: integ/specs (ADR-1585), 2026-10-28.
Built: the protocol layer (ADR-1669) and the rest of this spec (ADR-1891, which records every refinement); the Windows named pipe stays a follow-up. Reused pieces that exist: `regia_agent` WebSocket server on `tungstenite` (loopback, no TLS, ADR-014) and its `AgentExtension` method table (`regia_agent::extension`), `regia_core::Guid(u128)`, the glTF importer and `gltf` crate, `notify`, camera rig and physical-camera data (`specs/30`).
Naming (ADR-1585): the first draft used another engine's vocabulary for this feature (its feature name and its terms for items, kinds, senders and stalls); renamed to **DCC scene feed**, **item**, **kind**, **sender**, **stalled**, settings `playout`/`snap`/`delayed`. The file was renamed to `60-dcc-scene-feed.md` before any builder lane started (ADR-1661).

Source area: virtual-production/virtual-production-motion-link (generic streaming behavior only),
runtime-sequencer/runtime-sequencer-playback and runtime-sequencer/runtime-sequencer-cameras (the sections on recording
streamed camera and pose data only), runtime-characters/runtime-characters-digitalhumans (live-pose sections only).

## 1. Purpose and scope

An artist works in a content tool (Blender, Maya, Houdini, any DCC) and wants to see the change in a running Regia
editor or dev game without export, import and reload. The DCC streams scene data over a local socket; Regia shows
it as ordinary entities, marked feed-owned; when the artist is happy, one **bake** turns the live data into normal
assets through the same importer a `.glb` file would use.

In scope (Regia side only): the open wire protocol and its reference document (§ 4), the receiving server, the
mapping of streamed **items** onto entities, the ownership rules, the bake, a reference client
(`regia dcc-feed send`), the agent surface, and a record/replay format so tests need no DCC (§ 5, § 7, § 8).

Not in scope: the DCC-side add-ons (third parties write them from § 4; Regia ships none), sculpt-style sparse
deltas, morph targets, cloth or physics caches, USD scene streaming, bidirectional sync (Regia to DCC), motion capture
hardware sources and face capture, film-stage tracking protocols, multi-user editing, and any network-facing
service for untrusted peers (§ 9.1). The protocol is a development-time channel, not a game networking layer.

## 2. Regia terms and model

| Term | Meaning |
|---|---|
| **Sender** (client) | A process that connects and sends frames: a DCC add-on, the reference client, or a replay |
| **Session** | One authenticated connection from a sender; has an id, a client name, a state (below) and counters |
| **Item** | One named stream inside a session: a stable string `key` (<= 128 bytes UTF-8, chosen by the sender, e.g. the DCC object's persistent name or UUID), a **kind**, a parent item (optional) and static description |
| **Kind** | What an item's data drives. `transform`, `mesh` (a transform plus geometry), `camera` (a transform plus lens), `light` (a transform plus emitter), `skeleton` (static joint list plus per-frame poses), `generic` (named float channels). `mesh`, `camera` and `light` items carry a placement, so each also accepts plain transform frames |
| **Frame** | One timestamped sample for one item. Carries `frame_id` (u64, strictly increasing per item), `source_time_ns` (u64, sender clock) and an optional timecode |
| **Static data** | Data that does not change per frame: key, kind, parent, joint names and bind pose, lens type, light type, material slot names |
| **Feed-owned entity** | The entity the server spawns and keeps in step with an item. Carries `FeedItem` |
| **Bake** | Converting feed-owned entities and their data into normal assets and detaching them from the stream |
| **Recording** | A `.rfdrec` file: the exact frames a session delivered, with the simulation tick of each arrival |

### 2.1 Session state machine

| State | Entered when | Behavior | Leaves to |
|---|---|---|---|
| `negotiating` | transport accepted | only `hello` is allowed; 5 s timeout, counted on the feed clock (ADR-1891: tests drive it with numbers) | `pending`, `active`, `closed` |
| `pending` | `hello` valid and the client is not on the allow list | declarations are stored; data frames are discarded and counted (`dropped_not_accepted`); server sent `hello_ack{accepted:false}` | `active` (accept), `closed` (disconnect or timeout 120 s) |
| `active` | accepted (by allow list, by `dcc_feed.accept`, or by the editor prompt) | frames are applied; the server sends a `resync` request so the sender re-sends full state | `closing` |
| `closing` | `goodbye`, socket error, `dcc_feed.disconnect`, or fatal protocol error | pending inbox is discarded; items become `detached` | `closed` |
| `closed` | terminal | session record kept 60 s for `dcc_feed.status`, then dropped | none |

### 2.2 Item state

| State | Meaning | Transitions |
|---|---|---|
| `declared` | declaration received, no frame applied yet | `active` on first applied frame |
| `active` | frames arrive within `stall_after_ms` | `stalled` after `stall_after_ms` (default 500) with no frame; back to `active` on the next frame |
| `stalled` | connected, silent | `active`, `detached`, `removed` |
| `detached` | the session closed; entity remains, frozen, still feed-owned | `active` if a new session declares the same `key` (rebinding, § 2.4); `removed` by `dcc_feed.disconnect{despawn:true}` or editor close |
| `baked` | terminal; the entity became ordinary content | none |
| `removed` | `item_delete` or purge; entity despawned | none |

The editor marks `stalled` and `detached` items with distinct state icons and text, never color alone. The stalled timer runs on engine time
(on the fixed step during replay), so tests advance it with step counts.

### 2.3 Entities and components

- **`FeedItem`** (component, reflected, `#[reflect(ignore)]` on transient fields): `key: String`,
  `guid: Guid`, `kind: FeedKind`, `session: Option<SessionId>` (transient), `state: ItemState`,
  `owned: bool` (true until baked), `frames_applied: u64`, `last_frame_id: u64`, `last_source_time_ns: u64`.
  The server also inserts the ordinary components the kind needs: `Transform`, parent link, mesh and material
  components (mesh kind), the camera component (camera kind, with the physical-camera fields of spec 30 when the
  lens message carries them), a light component, or a skinned mesh binding plus `FeedPoseTarget` (§ 3.3).
- **Stable ids.** `guid` is a name-based 128-bit id from a fixed namespace and `key`: the RFC 9562 name-based
  layout with BLAKE3 in place of SHA-1 (version 8 bits, so no SHA-1 dependency; ADR-1669; `FeedItem.guid` is a `u128`
  because reflection has no `Guid`, ADR-1891), so the same key yields the same id in every session and on every machine. The server keeps a map
  `guid -> Entity` per world; a reconnect with the same key reuses the entity and keeps anything the user
  attached to it (§ 2.4). Transient assets use the id: `feed://meshes/<guid>`, `feed://textures/<name>`,
  `feed://clips/<name>`, `feed://materials/<guid>/<slot>`.
- **Handles.** On the wire an item is addressed by a `u32 handle` the sender assigns at declaration; handle 0 is
  reserved ("no item"). Handles are session-local; `key` is the identity.

### 2.4 Ownership rules

1. A feed-owned entity (and its transient assets) is read-only in the editor: the inspector shows a banner and
   disables fields, gizmos do not attach, `set_property`, `apply_edits`, `reparent` and `despawn` on it fail with
   `feed_owned`. Selecting, framing, screenshotting and adding **children** stay allowed.
2. Feed-owned entities are never written by "save level" or "save prefab". The serializer skips an entity whose
   `FeedItem.owned` is true.
3. A user entity parented under a feed-owned entity is saved with a `FeedAttach { key: String }` marker and
   its parent link omitted. On load, if an item with that `key` exists it is re-parented to it (local transform
   preserved); otherwise it loads under the scene root and logs a warning once per key.
4. Two sessions declaring the same `key` at once: the first declaration owns the item; the second receives
   error `item_conflict` for that handle and the rest of its session continues.
5. **Kind change** on an existing key is `kind_mismatch`; the sender deletes the item first.
6. `baked` entities are ordinary: editable, saved, and no longer addressed by the stream. A later declaration of
   the same key creates a fresh feed-owned entity next to them (the bake did not claim the key).

### 2.5 Coordinates and conventions

The hello message declares `space`: `gltf` (default) or `regia`. In `gltf` space data follows glTF 2.0: right
handed, +Y up, asset front facing +Z, meters, counter-clockwise front faces, texture origin at the top-left. The
server applies the same `forward` and `scale` import defaults as the project's glTF import (`specs/12` § 5:
default `forward: Convert`, a 180 degree turn about +Y), so a live mesh and its later bake land at the same world
pose. In `regia` space (+Y up, -Z forward) nothing is converted. A sender with another native convention (a Z-up
DCC) converts to one of these spaces itself; § 4.9 lists the standard Z-up conversion and the v flip. The hello
`unit_scale` (f32, default 1.0) multiplies positions, translations and lens distances.

### 2.6 Playout and buffering

Per session setting `playout`: `snap` (default) applies the newest frame of each item at the next
`PreUpdate`; no interpolation. `delayed` keeps up to `delay_queue_len` (default 64, cap 1024) frames per
item, evaluates at `engine_now - playout_delay_ms` (default 50 ms) mapped through the clock offset (§ 4.6) and
interpolates: translation, scale, lens values and joint translations linearly; rotations with normalized
quaternion slerp (shortest path); `mesh` and `generic` items never interpolate (newest frame at or before the
playout time). Frames older than `max_frame_age_ms` (default 2000) are evicted; frames with a non-increasing
`frame_id` are dropped and counted (`dropped_out_of_order`). Timecode is carried and exposed in
`FeedItem` and `dcc_feed.items`; timecode-aligned playout is a follow-up (§ 11).

## 3. API shape in Rust

Layering: the codec, server, item registry and replay live in `regia_agent::dcc_feed` (layer 3, cargo feature
`dcc-feed` of `regia_agent`; it reuses the agent's loopback WebSocket stack and adds the socket listeners).
`regia_agent` already depends on layer 2 crates, so it may use `regia_animation` and `regia_render` mesh types.
`regia_agent` cannot depend on `regia_asset_pipeline` (layer 5) or `regia_editor` (layer 6), so the bake and the
panel are not in it: **Decision (ADR-1585)** `dcc_feed.bake` and the editor-only methods are registered through
`AgentExtension` by `regia_editor::dcc_feed`, exactly like the other editor methods; the runtime-safe methods
(`status`, `items`, `accept`, `disconnect`, `record`, `replay`) live in `regia_agent::dcc_feed`. The mesh, texture and clip
kinds are compiled only with feature `dcc-feed-assets` (on in the editor); the game runtime build can enable
`dcc-feed` alone (§ 9.2). Bake code is `regia_asset_pipeline::dcc_feed_bake`, the panel and read-only rules
`regia_editor::dcc_feed`, the CLI `regia_cli::dcc_feed` (command `regia dcc-feed`). Feature `dcc-feed` is added to
the `regia` umbrella's dev feature set only; it is never default and never in shipping profiles.

### 3.1 Protocol crate surface (`regia_agent::dcc_feed::wire`)

```rust
pub const PROTOCOL_VERSION: u32 = 1;
pub struct FrameHeader { pub len: u32, pub msg_type: u16, pub flags: u16, pub seq: u32 }
pub enum Message { Hello(Hello), HelloAck(HelloAck), ItemDeclare(ItemDeclare), ItemDelete(u32),
    Transform(TransformFrame), Camera(CameraFrame), Light(LightFrame), Mesh(MeshUpdate),
    SkeletonStatic(SkeletonStatic), Pose(PoseFrame), ClipPush(ClipPush), TexturePush(TexturePush),
    Ping(Ping), Pong(Ping), Ack(Ack), SessionState(SessionState), Resync(Resync), Error(WireError),
    Goodbye, Unknown { msg_type: u16, optional: bool } }
pub fn encode(msg: &Message, seq: u32, out: &mut Vec<u8>) -> Result<(), WireError>;
pub fn decode(header: FrameHeader, payload: &[u8], limits: &Limits) -> Result<Message, WireError>;
pub struct FrameReader<R: std::io::Read> { /* incremental, bounded */ }
impl<R: std::io::Read> FrameReader<R> { pub fn next(&mut self, limits: &Limits) -> Result<Option<(FrameHeader, Vec<u8>)>, WireError>; }
```

`decode` is pure and total: no panics on any byte string; every length is checked against `Limits` before any
allocation. `WireError` has a stable `code()` (§ 4.10).

### 3.2 Server and registry (`regia_agent::dcc_feed`)

- `DccFeedPlugin` (adds the resources below and the systems in § 3.4). `DccFeedSettings` (resource, reflected,
  loaded from `Regia.toml [dcc_feed]`): `enabled` (default false), `listen` (default `unix`/`pipe` plus
  `127.0.0.1:0`), `allow_remote` (default false), `max_sessions` (4), `playout`, `playout_delay_ms`,
  `delay_queue_len`, `max_frame_age_ms`, `stall_after_ms`, `auto_accept: Vec<String>` (client names),
  `limits: Limits`.
- `DccFeedServer` (resource, not reflected): listener threads (one acceptor, one reader thread per session, no
  async runtime in runtime crates), the bounded `FeedInbox`, counters. `sessions() -> Vec<SessionInfo>`,
  `accept(SessionId)`, `disconnect(SessionId)`, `start_recording(path)`, `stop_recording()`, `start_replay(path, opts)`.
- `DccFeedRegistry` (resource): `guid -> Entity`, item table, `items(filter) -> Vec<ItemInfo>`.
- Messages (ECS messages): `FeedSessionChanged { session, from, to }`, `FeedItemChanged { guid, entity,
  change: Added | Updated | Stalled | Detached | Baked | Removed }`, `DccFeedWarning { session, code, text }`.
- `DccFeedSet` system set in `PreUpdate`: `Ingest` (drain inbox, coalesce, decode already done on reader
  threads) then `Apply` (spawn/update entities, run delayed playout) then `MeshSwap` (finished builds); it runs
  before `Update` and before transform propagation.

### 3.3 Pose target (`regia_animation::live`)

`FeedPoseTarget { item: String, blend: f32, joint_map: Option<Vec<(String, String)>> }` (component, reflected)
on a skinned entity. After anim graph playout the component overrides the local joint transforms by joint
**name** (identical names match; `joint_map` renames source to target joints; unmatched source joints are ignored).
Each unmatched source name is recorded once on `FeedPoseBuffer` and published as a `DccFeedWarning` with code
`joint_unmatched`. `FeedItem` does not carry that list. The pose is blended with the graph pose by `blend` (0 = graph only, 1 =
stream only; default 1). `FeedPoseBuffer` (resource) is the only thing the animation crate sees; it knows nothing
about sockets. An anim-graph node that reads the same buffer is a follow-up (§ 11) because adding a node type
changes the `.animgraph.ron` format.

### 3.4 Pipeline

1. Reader thread: read frame, check size, reassemble fragments, `decode`, validate (§ 4.8), push to `FeedInbox`
   with the session id. All validation happens here, never on the main thread.
2. `Ingest` (main thread, cheap): move inbox entries to per-item pending slots applying § 4.7 coalescing; send
   `ack` credits.
3. `Apply`: for each item apply transform/camera/light/pose/generic immediately (O(1) per frame); for `mesh`
   enqueue a build.
4. Mesh build on the compute task pool (`bevy_tasks`): derive missing normals, tangents (MikkTSpace when uv0 and
   normals exist and a slot has a normal map), bounds, a triangle-count check, split by submesh. `MeshSwap`
   replaces the asset content at the same `feed://meshes/<guid>` id; at most `max_mesh_swaps_per_frame`
   (default 2, 8 MiB) per frame, the rest wait (newest build per item wins).

### 3.5 Bake (`regia_asset_pipeline::dcc_feed_bake`)

```rust
pub struct BakeRequest { pub items: Vec<Guid>, pub folder: String, pub import: ImportOverrides,
    pub replace_live: bool, pub dry_run: bool }
pub struct BakeReport { pub assets: Vec<AssetId>, pub glb_paths: Vec<String>, pub converted: Vec<Guid>, pub warnings: Vec<String> }
pub fn plan_bake(snapshot: &FeedSnapshot, req: &BakeRequest) -> Result<BakePlan, BakeError>;
pub fn write_glb(plan: &BakePlan, out: &mut dyn std::io::Write) -> Result<(), BakeError>;
```

`FeedSnapshot` is a plain-data copy of items taken on the main thread (so the bake runs as a job off-thread).
`BakeError` variants: `NothingToBake`, `NotBakeable(kind)`, `TargetExists`, `Import(String)`, `Io(String)`.

## 4. Files and formats

### 4.1 The reference document

The normative description of this section is published as `docs/formats/dcc-feed-protocol-v1.md` (the builder
copies § 4.2 to § 4.10 into it, expands every table into byte offsets, and adds worked hex examples) so third
parties write clients in any language. Changes within major version 1 are additive only: new message types, new
mesh section ids, new trailing payload fields. A change that breaks that bumps `PROTOCOL_VERSION` to 2; the
server then supports 1 and 2 at once for at least one minor release and `regia upgrade-project` has nothing to
migrate (the protocol stores no project data; only `.rfdrec`, § 4.11, carries a version).

### 4.2 Transports

| Transport | Address | Notes |
|---|---|---|
| Unix domain socket (Linux, macOS) | `.regia/dcc_feed/feed.sock`, mode 0600 | no token needed: OS permissions are the check |
| Windows named pipe (follow-up, § 9.2) | `\\.\pipe\regia-dcc-feed-<project-hash>` | current-user ACL only; no token needed; not in the first delivery |
| TCP | `127.0.0.1:<port>` (port in `.regia/dcc_feed/endpoint.json`) | token required |
| WebSocket (binary) | `ws://127.0.0.1:<port>/dcc_feed`, subprotocol `regia-dcc-feed.v1` | same payloads, one wire message per WebSocket binary message (no length prefix); `Origin` must be absent or in `allowed_origins` (default empty), RFC 6455 section 10.2 |
| WebSocket (text, JSON) | same URL, subprotocol `regia-dcc-feed-json.v1` | control and small updates only (§ 4.12) |

`.regia/dcc_feed/endpoint.json` lists the active transports; `.regia/dcc_feed/token` holds the session token (32
random bytes, base64url, mode 0600, regenerated by `dcc_feed.rotate_token` in the editor panel). The WebSocket
listener is separate from the agent protocol's: different token, different permission model. The WebSocket
binary variant exists for web-based tools and for clients in sandboxes without raw sockets; the text variant
exists so a human can type a transform into a browser console while debugging.

### 4.3 Framing

All integers little-endian; floats IEEE 754 binary32 unless stated; strings are `u16` byte length plus UTF-8 (no
NUL; invalid UTF-8 is `malformed_frame`); arrays of numbers are raw packed values.

```
FrameHeader (12 bytes): u32 len (bytes after this field: 8 + payload), u16 msg_type, u16 flags, u32 seq
flags: bit0 FRAGMENT_MORE (payload continues in the next frame with the same msg_type),
       bit1 OPTIONAL (receiver may ignore an unknown msg_type), bit2 COMPRESSED (reserved, must be 0 in v1)
seq:   per-direction counter starting at 1; used by ack, error and logging
```

A fragmented message is the concatenation of consecutive frames of the same `msg_type` up to the first frame
without `FRAGMENT_MORE`; nothing else may be interleaved except `ping`, `pong` and `ack`. The reassembled size is
checked against `max_message_bytes` while accumulating. Unknown `msg_type` without `OPTIONAL` is
`unsupported_message` (fatal). Payloads may carry trailing bytes beyond the fields a receiver knows (forward
compatibility); fields are only ever appended.

### 4.4 Message types

| id | name | direction | payload (in order) |
|---|---|---|---|
| 0x0001 | `hello` | C to S | `[u8;4] "RFDP"`, `u32 min_version`, `u32 max_version`, str `client_name`, str `client_version`, str `dcc` (free text, e.g. host app name and version), str `space` (`gltf`/`regia`), `f32 unit_scale`, `u32 caps` (bit0 mesh, bit1 texture, bit2 clip, bit3 pose, bit4 text-frames), str `token` (empty on Unix socket and pipe) |
| 0x0081 | `hello_ack` | S to C | `u32 version`, `u64 session_id`, `u8 accepted`, `u32 caps`, `Limits` block (§ 4.8), `u32 window_bytes`, str `server_name` |
| 0x0010 | `item_declare` | C to S | `u32 handle`, str `key`, `u8 kind` (1 transform, 2 mesh, 3 camera, 4 light, 5 skeleton, 6 generic), `u32 parent_handle` (0 none), str `display_name`, `u8 variant` (light: 0 directional 1 point 2 spot; camera: 0 perspective 1 orthographic; else 0), kind-specific tail (generic: `u16 n` then n channel names) |
| 0x0011 | `item_delete` | C to S | `u32 handle` |
| 0x0020 | `transform` | C to S | `u32 handle`, FrameInfo, `f32[3] translation`, `f32[4] rotation` (x,y,z,w, unit), `f32[3] scale`, `u8 space_flags` (bit0: world space instead of parent-local) |
| 0x0021 | `camera` | C to S | `u32 handle`, FrameInfo, transform block (as above), `u8 mode`, then perspective: `f32 yfov_rad`, `f32 aspect` (0 = viewport), `f32 near`, `f32 far` (0 = infinite); orthographic: `f32 xmag`, `f32 ymag`, `f32 near`, `f32 far`; then optional physical block `f32 focal_mm`, `f32 sensor_w_mm`, `f32 sensor_h_mm`, `f32 focus_m`, `f32 f_stop` (all 0 = absent) |
| 0x0022 | `light` | C to S | `u32 handle`, FrameInfo, transform block, `f32[3] color` (linear RGB), `f32 intensity` (glTF punctual-light units: lux for directional, candela for point and spot), `f32 range` (0 = none), `f32 spot_inner_rad`, `f32 spot_outer_rad` |
| 0x0023 | `mesh` | C to S | `u32 handle`, FrameInfo, `u8 update_mode` (0 full, 1 vertex-only), `u16 section_count`, sections (§ 4.5) |
| 0x0024 | `skeleton_static` | C to S | `u32 handle`, `u16 joint_count`, per joint: str `name`, `i32 parent` (-1 root), bind pose local `f32[3] t`, `f32[4] r`, `f32[3] s` |
| 0x0025 | `pose` | C to S | `u32 handle`, FrameInfo, `u16 joint_count` (must equal the static count), per joint `f32[3] t`, `f32[4] r`, `f32[3] s`; `generic` items use the same message with `f32` values and `joint_count` = channel count (TRS omitted: `u8 layout` 0 TRS, 1 scalar) |
| 0x0026 | `clip_push` | C to S | str `name`, `f32 fps`, `f32 duration_s`, `u8 loop`, `u32 bind_handle` (0 none), `u32 track_count`, per track: str `target` (node or joint name), `u8 property` (0 translation, 1 rotation, 2 scale), `u8 interp` (0 step, 1 linear), `u32 key_count`, `f32[key_count] times`, `f32[key_count * (3 or 4)] values` |
| 0x0027 | `texture_push` | C to S | str `name`, `u8 colorspace` (0 srgb, 1 linear, 2 normal map), `u8 source` (0 by path, 1 encoded bytes, 2 raw), source tail: path str; or `u8 container` (0 PNG, 1 JPEG, 2 EXR, 3 KTX2) plus `u32 byte_len` plus bytes; or `u8 pixel_format` (0 rgba8, 1 rgba16f, 2 rgba32f), `u32 w`, `u32 h`, bytes |
| 0x0030 / 0x0031 | `ping` / `pong` | both | `u64 token`, `u64 sender_time_ns`; a pong echoes `token` and adds `u64 responder_time_ns` |
| 0x0082 | `ack` | S to C | `u32 seq_applied`, `u32 credit_bytes` (window space returned) |
| 0x0083 | `session_state` | S to C | `u8 state` (§ 2.1 index), str `reason` |
| 0x0084 | `resync` | S to C | `u32 handle` (0 = every item): re-send static data and one full current frame |
| 0x003F | `error` | both | `u32 ref_seq`, `u32 handle`, str `code`, str `message`, `u8 fatal` |
| 0x0032 | `goodbye` | both | str `reason`; the sender then closes |

FrameInfo = `u64 frame_id`, `u64 source_time_ns`, `u8 has_timecode`, and when set `u8 hh, mm, ss, ff`, `u16
rate_num`, `u16 rate_den`, `u8 drop_frame`.

### 4.5 Mesh sections

`mesh` carries a table of sections; each is `u8 section_id`, `u8 elem` (0 f32, 1 u8 normalized, 2 u16, 3 u32), `u16
index` (uv set or color set number, else 0), `u32 count` (elements), `u32 byte_len`, bytes. A receiver skips a
section of unknown `section_id` by `byte_len`.

| section_id | content | elements per item | rules |
|---|---|---|---|
| 1 positions | vertex positions | 3 x f32 | required in a full update |
| 2 normals | unit normals | 3 x f32 | `count` equals vertex count; absent: server generates area-weighted smooth normals |
| 3 tangents | tangent xyz + sign | 4 x f32 | absent: generated when needed |
| 4 uv | texture coordinates, set `index` 0 to 3 | 2 x f32 | v origin top-left |
| 5 color | vertex color, set `index` 0 | 4 x f32 or 4 x u8 normalized | linear |
| 6 indices | triangle list | 1 x u16 or u32 | length multiple of 3, each < vertex count; required in a full update |
| 7 submeshes | draw ranges | per range: `u32 slot, first_index, index_count` | ranges lie inside the index list, in slot order, non-overlapping; absent: one range, slot 0 |
| 8 material slots | slot table | per slot: str `name`, `f32[4] base_color`, `f32 metallic`, `f32 roughness`, `f32[3] emissive`, then four str texture names (base color, normal, metal-rough, emissive; empty = none) | slot numbers are positions in this list |
| 9 joints | skin joint indices | 4 x u16 | needs a bound skeleton item (`bind_handle` in static form is the parent skeleton item) |
| 10 weights | skin weights | 4 x f32 | renormalized by the server |

`update_mode` 1 (vertex-only) replaces only the listed per-vertex sections (positions, normals, tangents, uv,
color, weights) and requires `count` to equal the current vertex count, otherwise `topology_mismatch`; indices,
submeshes and slots are kept, which lets the server keep its index buffer and bounds-tree and update only the
vertex data. A full update replaces everything.

### 4.6 Time

`source_time_ns` is the sender's monotonic clock; its zero is arbitrary. The server estimates a clock offset from
`ping`/`pong` exchanges (the client is expected to ping at least every 2 s): offset = `server_time - (client_send +
rtt/2)` of the lowest-RTT sample in the last 16. Before the first pong the first frame of an item anchors the
mapping. In `snap` playout the offset is unused. The server never sends wall-clock time to be trusted.

### 4.7 Back-pressure, partial updates and coalescing

- **Credits.** `hello_ack.window_bytes` (default 32 MiB) is the byte budget of frames the server has accepted but
  not yet applied. The client tracks unapplied bytes (sum of frame sizes minus `ack.credit_bytes`) and must not
  exceed the window; the server returns credit when a frame is applied or coalesced away, at most once per applied
  batch. TCP/pipe flow control is the second layer: a stalled server stops reading and the client blocks.
- **Overload.** If a client exceeds the window by more than one frame the server sends error `overloaded` (not
  fatal) and drops that frame; three in a row close the session.
- **Coalescing.** The inbox keeps one pending slot per `(item, message type)`:
  `transform`, `camera`, `light`, `pose`, vertex-only `mesh`: the newer frame replaces the older unapplied one
  (counted in `frames_coalesced`). A full `mesh` supersedes an older pending vertex-only `mesh`, and a
  vertex-only `mesh` arriving after a pending full `mesh` is merged into it (its sections replace the full
  update's matching sections). Never coalesced: `item_declare`, `item_delete`, `skeleton_static`,
  `clip_push`, `texture_push`, `goodbye`, and in `delayed` playout the interpolated message types (they are queued up
  to `delay_queue_len`, then the oldest is evicted). Order across message types for one item is preserved:
  declare before data, delete discards pending data of that handle.
- **Partial updates.** A `transform` never touches geometry; a vertex-only `mesh` never touches transforms or
  slots; the camera and light messages carry their transform, so a moving camera needs one message.

### 4.8 Validation and limits

`Limits` (all configurable, defaults below, sent in `hello_ack`): `max_message_bytes` 64 MiB, `max_vertices` 2,000,000,
`max_indices` 12,000,000, `max_items` 2048 per session, `max_joints` 1024, `max_clip_tracks` 4096,
`max_keys_per_track` 100,000, `max_texture_dim` 8192, `max_texture_bytes` 256 MiB per push,
`max_transient_texture_bytes` 512 MiB total (least recently used evicted), `max_key_len` 128. Checks: every float
finite (a frame with NaN or infinity is rejected whole: `invalid_value`); quaternions within 1e-3 of unit length
(else normalized, counted as a warning; zero length rejected); counts consistent; indices in range; strings valid
UTF-8; `by path` textures must resolve (after symlink resolution) inside the project directory or
`.regia/dcc_feed/inbox/`, otherwise `path_denied`; no texture or clip path is ever opened before the session is
`active`. A frame addressed to an undeclared handle is `unknown_item` (not fatal; one error per handle per
second).

### 4.9 Standard conversions for clients (informative)

Z-up right-handed DCC (Blender-style) to `gltf` space: `(x, y, z) -> (x, z, -y)` for positions and translations;
quaternion `(x, y, z, w) -> (x, z, -y, w)`; flip `v -> 1 - v`; triangulate with consistent counter-clockwise
winding after the axis change (the change is a proper rotation, so winding is preserved). Lengths in the DCC's
unit system multiply by `unit_scale` or are converted to meters by the client. What an add-on can read from the
host application (evaluated mesh data, object matrices, camera and light data, armature poses, change
notifications) is described in the host's public Python API reference
(https://docs.blender.org/api/current/ for Blender); the protocol assumes only that a client can obtain those
values, never any particular host code.

### 4.10 Error codes

Wire `error.code` and agent error codes share one lowercase ascii/digit/underscore vocabulary: `bad_magic`,
`unsupported_version`, `auth_failed`, `not_accepted`, `frame_too_large`, `limit_exceeded`, `malformed_frame`,
`unsupported_message`, `unknown_item`, `item_conflict`, `kind_mismatch`, `kind_not_supported`,
`topology_mismatch`, `invalid_value`, `path_denied`, `overloaded`, `timeout`, `internal`. `unsupported_version`
carries the server's supported range in `message`. Fatal codes close the session after the error is sent.

### 4.11 Recording file `.rfdrec`

Binary, little-endian: `"RFDR"`, `u32 format_version` (1), `u32 protocol_version`, str `client_name`, str `dcc`,
`u64 record_count`, then records `u64 sim_tick`, `u32 frame_len`, the complete wire frame (header included). The tap
sits after reassembly, so a fragmented message is stored whole as one record (ADR-1891). Recording taps frames after validation and before coalescing, so
a replay sees what the source sent. A v2 `.rfdrec` adds a reader that keeps accepting v1; no project migration.

### 4.12 Text variant (JSON)

One JSON object per WebSocket text message. Allowed `t` values: `hello`, `item`, `transform`, `camera`,
`light`, `delete`, `ping`, `pong`, `error`, `goodbye`; fields mirror § 4.4 with arrays for vectors and
`quat: [x, y, z, w]`. Mesh, texture, clip and pose are binary only (`kind_not_supported` in the text variant).
Floats must be finite JSON numbers. The OSC design (typed address plus arguments plus time tag) is the precedent for
keeping control traffic simple; no OSC listener is shipped (§ 11).

## 5. Agent-protocol surface

Methods use the dotted `dcc_feed.*` names. Error shape follows the neighboring built methods: `F0002` with `data.reason` set to a lowercase code from the § 4.10 vocabulary plus the ones in the table; `invalid_argument` becomes plain `F0002`; `permission_denied` and `unsupported` follow the protocol's existing codes. Feature off answers `F0013` (the table's `dcc_feed_disabled` is its `data.reason`). Guide page
`book/agents/dcc-feed.md`.

| Method | Access | Params | Result | Errors |
|---|---|---|---|---|
| `dcc_feed.status` | read | none | `{ enabled, listeners: [{transport, address}], settings, sessions: [{id, client, dcc, state, peer, protocol_version, items, bytes_in, frames_in, frames_applied, frames_coalesced, dropped, last_error}] }` | `dcc_feed_disabled` only for sub-queries; plain status always answers |
| `dcc_feed.items` | read | `{ session?, kind?, state?, key_glob?, limit? }` | `{ items: [{key, guid, handle, entity, kind, state, owned, frames_applied, last_frame_id, last_source_time_ns, detail}] }` (`detail`: vertex/triangle counts, joint count, texture size) | `session_not_found`, `invalid_argument` |
| `dcc_feed.accept` | edit_scene | `{ session, remember?: bool }` | `{ accepted: true, resync_sent: n }`; `remember` appends the client name to `auto_accept` in `.regia/dcc_feed.toml` | `session_not_found`, `already_accepted`, `session_closed` |
| `dcc_feed.bake` | edit_assets | `{ items: [key] or "all", session?, folder, import?: {scale?, lods?, collision?}, replace_live?: bool (default true), dry_run? }`. A present `forward` or `generate_tangents` is a warning and is not applied. | job id; the editor frame applies a finished worker and `dcc_feed.bake_status` reports it (`job_status` polls that read); `dry_run` returns the planned file list without writing | `item_not_found`, `nothing_to_bake`, `not_bakeable`, `target_exists`, `import_failed`, `path_denied` |
| `dcc_feed.disconnect` | edit_scene | `{ session, despawn?: bool (default false) }` | `{ closed: true, items_detached: n, despawned: n }` | `session_not_found` |
| `dcc_feed.record` | edit_scene | `{ action: "start" or "stop", path? }` | `{ recording, path, records }` | `record_active`, `path_denied`, `invalid_argument` |
| `dcc_feed.replay` | edit_scene | `{ path, ticks_per_record?: u32, loop?: bool, as_client?: string }` | `{ session, records_total }`; progress via `dcc_feed.status` (`replay: {done, total}`) | `replay_not_found`, `replay_corrupt`, `path_denied` |

`dcc_feed.bake` and `dcc_feed.replay` follow the long-operation rule of `specs/23` § 4 (job id, `job_status`).
`dcc_feed.accept`, `bake`, `disconnect` need the matching permission; a token holding only `read` gets
`permission_denied`. Methods work in a headless editor. In a game build without the feature they answer
`F0013` with reason `dcc_feed_disabled`. Edits against feed-owned entities through the generic scene methods answer `feed_owned`.

Verification recipe for agents (in the guide): start the reference client with `--record` or call
`dcc_feed.replay`; `wait_until` on `query_entities { component: "FeedItem" }` with predicate
`frames_applied >= N`; `screenshot` the viewport; `expect` on the entity's `Transform` and the mesh vertex count
reported by `dcc_feed.items`; then `dcc_feed.bake`, `save_level` and check the entity is no longer feed-owned.
Add a `COVERAGE.md` row per method (`dcc_feed.status | dcc_feed_status | [DCC feed](agents/dcc-feed.md)` and so
on), a crate-feature row (`regia_agent` `dcc-feed`), a CLI row (`regia dcc-feed`) and a format row (`.rfdrec`, the
wire protocol page).

### 5.1 Reference client: `regia dcc-feed`

- `regia dcc-feed send <file.gltf|file.glb> [--endpoint ADDR] [--token-file PATH] [--watch] [--animate CLIP]
  [--fps N] [--record OUT.rfdrec] [--client-name NAME]`. Connects (default: the project's `endpoint.json`), sends
  `hello`, declares one item per glTF node that owns a mesh, camera or punctual light, a `transform` item
  for plain nodes, a `skeleton` item per skin, then the initial frames, textures (by bytes) and clips. With
  `--watch` it re-reads the file when it changes and sends only what differs, found by hashing each item's
  sections (so moving one node sends one `transform`, editing one mesh sends a vertex-only or full `mesh`).
  `--animate` replays a glTF animation as `transform`/`pose` frames at `--fps` (default 30).
  A joint is also a plain item when it owns a mesh, camera or light, or when a non-joint node sits below it, so that
  node follows the joint. A joint's `skeleton_static` parent is its nearest joint ancestor, with the nodes in between
  (all ancestors, for a root joint) folded into its local transform (ADR-5291). Joints are sent parents first and a
  skinned mesh's `JOINTS` indices are renumbered per node to the sent order; a pushed clip carries the fold in the
  folded joints' keys, resampled at the union of the key times when a skipped node is animated (ADR-5618).
- `regia dcc-feed record OUT.rfdrec` and `regia dcc-feed replay IN.rfdrec` wrap `dcc_feed.record` and
  `dcc_feed.replay` for scripts. `regia dcc-feed dump IN.rfdrec` prints one line per record (tick, type, handle, size).
- The client uses `regia_agent::dcc_feed::wire` directly; a conformance test decodes its output with an independent
  decoder written from the published byte tables (§ 8, test 1).

## 6. Editor surface

A **DCC feed** panel (`regia_editor::dcc_feed`): listener toggle and the transports/endpoint shown for copying;
token reveal and rotate; a session list with accept/reject and a "remember this client" check; an item table
(key, kind, state icon and text, frames, last frame age in engine time); per-item "Bake...", "Bake all"
and a folder picker with the import settings used by glTF import; record/replay controls. Viewport and outliner mark
feed-owned entities with a "FEED" badge; the inspector shows the read-only banner. The panel is a thin client of the
same code the agent methods call. Human accept prompts appear as a toast when an unknown client connects while the
editor is focused.

## 7. Determinism notes

- The network is not deterministic; **replay is**. A replay injects each record into the inbox at its recorded
  `sim_tick`, uses the fixed step as the only clock (stalled timers, delayed playout and clock-offset
  estimation read it; `ping` records give offsets from recorded times), and applies the same coalescing rules.
  Same `.rfdrec` plus the same world gives the same entity state at every tick on one platform: transforms, pose
  arrays, vertex arrays and generated normals are bit-exact (mesh derivation uses fixed-order single-pass loops
  whose result does not depend on thread count).
- Across platforms (Linux/Windows/Android) buffered interpolation and normalized quaternions are tolerance-based
  (1e-6 relative); tests on those compare with that tolerance. Mesh-build results are compared by checksum on one
  platform and by tolerance across platforms.
- Entity creation order follows (tick, record order), and `Guid`s are derived from keys, so entity naming and
  id mapping do not vary between runs.
- Bake output is deterministic: sorted item order by `key`, the `.glb` writer emits fixed field order and no
  timestamps, so baking the same snapshot twice produces identical bytes.

## 8. Tests and acceptance

1. **Codec round trip and conformance** (no GPU): every message type encodes and decodes; a second decoder
   written only from the § 4 tables in the test crate agrees on the output of the reference client; byte vectors
   for each message are stored under `tests/data/dcc_feed/`.
2. **Fuzz/malformed input** (no GPU): truncated frames, lengths above limits, odd counts, NaN values, bad UTF-8,
   index out of range, bad fragment order, unknown mandatory message; each yields the documented error code, no
   panic, and allocation never exceeds `Limits` (a counting allocator in the test checks the peak).
3. **Version and auth**: `min_version > 1` gives `unsupported_version`; wrong or missing token on TCP gives
   `auth_failed` and closes; a Unix socket / pipe session needs none; a WebSocket upgrade with a foreign `Origin`
   is refused; a non-loopback bind without `allow_remote = true` refuses to start.
4. **Item mapping** (headless app): declare one of each kind and send frames; entities exist with the expected
   components, stable `guid` (twice from two runs equal; equals the vector in the test data), parents per
   `parent_handle`, and `frames_applied` counts.
5. **Ownership**: `set_property`, `reparent`, `despawn` on a live entity answer `feed_owned`; save level omits the
   entity; a child added by a user is saved with `FeedAttach` and re-parented after a second session declares
   the key; a conflicting declaration gets `item_conflict`.
6. **Coalescing and back-pressure**: queue 100 `transform` frames for one item before a tick, apply one tick:
   the applied frame is the 100th, `frames_coalesced` is 99; credits returned equal bytes accepted; a vertex-only
   `mesh` after a pending full `mesh` merges; delete discards pending data; exceeding the window yields
   `overloaded`.
7. **Mesh**: full update with positions, uv0 and indices (no normals) produces generated normals equal to a
   reference computation within 1e-6; vertex-only update with a wrong count gives `topology_mismatch`; submesh
   ranges map to slots; material slot names appear on the entity; 100k-vertex mesh builds within the counters of
   § 10 (checked by a bench, not a test).
8. **Pose**: a skeleton with 3 joints plus pose frames drives `FeedPoseTarget` joints by name; `blend = 0` leaves the
   graph pose bit-identical; unmatched joint names are reported once.
9. **Delayed playout**: with frames at ticks 0, 6, 12 and delay of 3 ticks, the evaluated translation at tick 6
   (tick 9 in the first draft; ADR-1891 corrected the arithmetic) is the exact linear midpoint of frames 0 and 6 within 1e-6; rotation is the slerp midpoint; eviction past
   `max_frame_age_ms` and the `delay_queue_len` cap are counted.
10. **Stalled and detach**: no frame for `stall_after_ms` of fixed-step time gives `stalled`; closing the session gives
    `detached`; a new session with the same key goes back to `active` on the same entity.
11. **Record and replay**: record the reference client sending a small glTF (checked-in fixture, a few KB) with
    `--animate`; replay twice into two fresh worlds and compare reflected state at ticks 1, 10 and 60: bit-equal. The
    shuffle check (`specs`, `04` § 12) runs the replay scene and reports no state difference across seeds.
12. **Bake round trip**: bake a replayed mesh item; the produced `.glb` loads through the normal importer; world
    positions of all vertices equal the live ones within 1e-5; material slot names equal; the entity is no longer
    `owned`, is saved by "save level" and reloads identically; the `.meta` provenance has `tool: "dcc_feed"`; the
    bake of the same snapshot twice is byte-identical; `dry_run` writes nothing.
13. **Agent**: every method in § 5 has a schema row, guide topic, `COVERAGE.md` row and a call in a test
    (`cargo xtask agent-coverage`); `accept` of a pending replay-as-client session works; error cases return the
    listed codes; layer check passes.
14. **Reference client `--watch`** (needs a file system, no GPU): editing a node transform in a glTF file produces
    exactly one `transform` frame; editing one mesh produces one `mesh` message and no other item traffic.
15. Needs a GPU or device: a screenshot via the agent protocol of a replayed scene matches a golden image (golden
    images are a TODO for the lead; the builder never creates them); an Android dev build receives a pose stream
    through `adb forward` and moves a skinned entity (manual device check recorded in `docs/progress.md`).

No test asserts wall-clock time; timing claims in § 10 are benchmarks that report numbers.

## 9. Platform notes and security

### 9.1 Security

- Local-only by default: `enabled = false` until the artist turns it on; when on, the listeners are the Unix
  socket / named pipe and `127.0.0.1`. A non-loopback address requires `allow_remote = true`, a token, and shows a
  persistent editor warning; the protocol has no TLS, so remote use means an SSH tunnel or a VPN (documented).
- Token: random 256 bit, file mode 0600 under `.regia/dcc_feed/` (git-ignored by the project template), compared in
  constant time, rotatable. A new, unknown client name lands in `pending` and needs `accept` unless allow-listed:
  authentication and approval are separate steps, so a local process that has read the token still cannot change the
  scene unseen.
- Parsers are bounded and total (§ 4.8). Texture paths cannot leave the project directory or the inbox. The server
  never executes data it receives and opens no URL. Compressed containers are not decoded beyond the engine's
  existing image decoders, whose output size is capped by `max_texture_dim`.
- Rate limits: `max_sessions` 4; one `hello` attempt per peer per second; 5 failed token checks lock that peer out
  for 60 s (counted on engine time).
- A live stream can make a lot of geometry: the 2 M vertex cap and 8 MiB per frame swap budget keep the editor
  responsive; `dcc_feed.disconnect` is the kill switch and always answers.

### 9.2 Platforms

| Platform | Editor receiver | Runtime receiver | Notes |
|---|---|---|---|
| Linux | yes | dev builds, kinds transform/camera/light/skeleton/generic | Unix socket + loopback TCP + WebSocket |
| Windows | yes | same | loopback TCP + WebSocket with the token. **Decision (ADR-1585):** no named pipe in the first delivery (no new dependency); a pipe via `interprocess` (MIT OR Apache-2.0, `cfg(windows)` only) is a follow-up |
| Android aarch64 | no (editor is desktop-only) | dev builds, pose and transform streams only, for debugging | TCP via `adb forward`; token in the app files directory; compiled out of shipping builds with the `agent` feature |
| macOS (testing only) | build and test | same as Linux | Unix socket |

Mesh, texture and clip kinds exist only with `dcc-feed-assets` (editor); a runtime build answers
`kind_not_supported`. Dependencies: none new. Existing workspace crates only: `tungstenite` (WebSocket), `gltf` (read; `.glb` is written by a small writer in `dcc_feed_bake`, the `gltf` crate has no writer), `notify` (`--watch`, already a desktop dependency), `serde_json` (text variant), `base64` only if already in the lockfile, else a local 20-line base64url. No async runtime in
runtime crates (ADR-014).

## 10. Performance budget (min-spec class: 4-core low-power x86, 8 GB)

| Item | Budget | Span |
|---|---|---|
| Ingest throughput, decode + validate on a reader thread | >= 150 MB/s per session on a dev machine; >= 40 MB/s min-spec | `dcc_feed_decode` |
| `Ingest` + `Apply` main-thread cost for 200 transform frames/tick | <= 0.2 ms | `dcc_feed_apply` |
| Pose frame, 100 joints, 120 Hz | <= 0.05 ms per frame applied | `dcc_feed_apply` |
| 100k-vertex mesh (positions, normals, uv0, 300k indices, about 4.4 MB): decode + validate | <= 4 ms | `dcc_feed_decode` |
| Same mesh: derived data (bounds, submesh split, tangents when needed) on the compute pool | <= 12 ms (dev), <= 40 ms (min-spec) | `dcc_feed_mesh_build` |
| Main-thread swap of a built mesh | <= 1 ms excluding GPU upload; upload follows the normal asset path | `dcc_feed_mesh_swap` |
| End to end, last byte of a 100k-vertex full update to visible | <= 3 frames at 60 Hz on dev hardware (benchmark) | all |
| Bake of a 100k-vertex mesh | <= 1.5 s including import (job, off the main thread) | `dcc_feed_bake` |
| Memory | window 32 MiB per session; 64 buffered frames per interpolated item; transient textures <= 512 MiB (LRU); staging buffers freed after swap | counters in `dcc_feed.status` |

Span names: `dcc_feed_ingest`, `dcc_feed_decode`, `dcc_feed_apply`, `dcc_feed_mesh_build`, `dcc_feed_mesh_swap`,
`dcc_feed_bake`, `dcc_feed_replay`. None runs per frame when idle (an idle server costs one atomic load).

## 11. Out of scope / follow-ups

- Timecode-aligned playout (needs an engine timecode provider); the fields are already carried.
- Anim-graph node reading `FeedPoseBuffer` (format change for `.animgraph.ron`; needs a migrator note).
- Sparse vertex deltas for sculpting, morph targets, curves/hair, volumes, instancing, USD streaming.
- Material graph sync (only simple PBR slot parameters and textures travel).
- OSC listener for control surfaces, bidirectional sync (selection or camera back to the DCC).
- A published client library crate for external Rust tools, and add-ons for specific DCCs (third-party).
- Remote-safe transport with TLS.

## 12. Provenance and open questions

| Area | Algorithm/behavior | Public source |
|---|---|---|
| Wire design | Length-prefixed typed messages, packed typed arguments, time tags carried with data | Open Sound Control 1.0 specification, https://opensoundcontrol.stanford.edu/spec-1_0.html |
| WebSocket carriage | Binary and text messages, subprotocol negotiation, `Origin` validation against cross-site connections | RFC 6455 (sections 1.3, 4, 10.2), https://www.rfc-editor.org/rfc/rfc6455 |
| Flow control | Byte-credit window returned as data is consumed | RFC 9113 section 5.2 (HTTP/2 flow control), https://www.rfc-editor.org/rfc/rfc9113 |
| Delayed playout | Render a fixed delay behind the newest sample and interpolate between the bracketing snapshots; drop out-of-order samples | Fiedler, "Snapshot Interpolation", https://gafferongames.com/post/snapshot_interpolation/ ; Bernier, "Latency Compensating Methods in Client/Server In-game Protocol Design and Optimization", GDC 2001 |
| Mesh, camera, light, skin data and conventions | Attribute layout, units, axes, winding, punctual-light units, joint/weight limits | glTF 2.0 specification, https://registry.khronos.org/glTF/specs/2.0/glTF-2.0.html ; KHR_lights_punctual; client-side data access per the Blender Python API reference, https://docs.blender.org/api/current/ (public docs only; the project is GPL and no code was consulted) |
| Stable item ids | Name-based UUID from a namespace and a key | RFC 9562 section 5.5 (version 5), https://www.rfc-editor.org/rfc/rfc9562 |

Decisions made in review (ADR-1585):

- Vocabulary renamed away from the set's (see the header); wire magic `RFDP`, recording magic `RFDR`, extension `.rfdrec`.
- Token on loopback TCP and WebSocket, none on the 0600 Unix socket (OS permission is the check); the Blender-side add-on reads `.regia/dcc_feed/token`. Kept.
- `pending` approval for unknown client names stays (authentication and approval are separate, § 9.1); `auto_accept` and `remember` remove the friction for the artist's own tools.
- Bake goes through a written `.glb` and the normal importer (one import path, byte-deterministic output); direct cooked-asset construction is a follow-up if the round trip proves slow (budget § 10).
- Order: `FeedPoseTarget` component first (no format change); the anim-graph node later with its migrator.
- Windows: loopback TCP in the first delivery, named pipe later (§ 9.2).
- New formats: `.rfdrec` v1 and the wire protocol v1 (`docs/formats/dcc-feed-protocol-v1.md`, `docs/formats/rfdrec.md`); the wire protocol is not a project format (no `regia upgrade-project` work). The lead runs `cargo xtask check-formats --update` and `cargo xtask manual`. `Regia.toml [dcc_feed]` is a new section (defaults when absent, no migration).
- Frame sizes in § 10 are targets to be measured, not requirements to fail on.

### Refinements recorded after review

The builder lanes recorded these as ADRs and left the prose above alone; this table (ADR-5643) lists them so a reader
of the spec finds them. The code and `docs/formats/dcc-feed-protocol-v1.md` follow the right-hand column.

| Section | Spec text | As built | ADR |
|---|---|---|---|
| § 2.3 | RFC 4122 version 5 id | BLAKE3 in the RFC 9562 name-based layout (version 8); `FeedItem.guid` is `u128` | ADR-1669, ADR-1891 (2) |
| § 2.4 rule 1 | guards on `set_property`, `apply_edits`, `reparent`, `despawn` | also `add_component`, `remove_component`, `duplicate`; `FeedOwned`/`FeedAttach` live in `regia_scene` | ADR-1891 (2) |
| § 2.1, § 7 | socket timeout; fixed step as the only clock | live: real time (a paused game still shows edits); replay or a host-set `DccFeedClock::fixed_step_s`: the fixed step | ADR-1891 (3) |
| § 3.2 | `DccFeedSettings` reflected | not reflected (from `Regia.toml`; `dcc_feed.status` reports it as JSON) | ADR-1891 (16) |
| § 3.5 | `write_glb(plan)` | `write_glb` takes one `PlannedGlb`; one `.glb` per mesh item, vertices in object space | ADR-1891 (6) |
| § 4.2 | WebSocket listener | opt-in (`listen = ["ws:127.0.0.1:0"]`), `allowed_origins` a setting; JSON passes through the binary codec | ADR-1891 (9) |
| § 4.4 | `error`, `goodbye`, `pose` payloads | `error` is `ErrorFrame` (ref_seq, handle, code, message, fatal); `goodbye` carries a reason; `pose` has `u8 layout` before the count; raw textures carry `u32 byte_len` | ADR-1669 |
| § 4.6 | offset from the lowest-RTT sample | smallest `server_now - client_send` of the last 16 client pings (the server sends no ping) | ADR-1891 (4) |
| § 4.11 | fragments stay separate records | a fragmented message is one record; a recording holds one session, starts at a synthesized `hello` and a `resync` | ADR-1891 (5), (18) |
| § 5 | seven methods | plus `dcc_feed.listen`, `dcc_feed.rotate_token` and the bake poll `dcc_feed.bake_status` (no shared `job_status` yet) | ADR-1891 (6), (8) |
| § 5.1 | `--watch` with `notify` | polls the modification time (dependency-free client); joints follow ADR-5291 | ADR-1891 (10), ADR-5291 |
| § 8 test 9 | midpoint at tick 9 | midpoint at tick 6 | ADR-1891 (11) |
| § 8 test 12 | `.meta` provenance `tool: "dcc_feed"` | `tool: generator`, `generator: "dcc_feed"` (no new frozen-format variant) | ADR-1891 (6) |
| § 9.1 | hello rate limit and token lockout | token transports only (TCP, WebSocket); the 0600 socket is not limited | ADR-1891 (14) |
| § 9.2 | Windows named pipe follow-up | still a follow-up | ADR-1891, ADR-2136 |

### Owner decisions

None.

### Public-page citations

Sources-section URLs recorded by the spec writer on 2026-11-08 (ADR-1526; ADR-1661).
These are documentation/reference citations, including public API-reference pages; only the
URLs were transcribed from the external set.

- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
