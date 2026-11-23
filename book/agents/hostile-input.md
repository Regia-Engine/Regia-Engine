# Hostile input: what never crashes the game

An agent sends whatever JSON it likes, and a project holds whatever bytes its files hold. The
engine's promise for both: a bad request or a bad file ends in an error you can read, never in a
panic, a hang or an unbounded allocation. This page says what that means on the wire and how the
engine checks it.

## On the protocol

Every request gets exactly one answer: a `result`, or an `error` whose `data.code` is a stable
`F0xxx` code (`errors.explain` describes each one). Wrong types, missing fields, numbers past their
range and ids of entities that do not exist are all `F0002` (`INVALID_PARAMS`) or `F0003`
(`NOT_FOUND`), and they change nothing.

Some parameters have hard upper bounds, because a value past them would stall or exhaust the game
rather than fail:

| Method | Parameter | Accepted |
|---|---|---|
| `play.step` | `frames` | 1 to 10,000 per call |
| `play.set_time_scale` | `scale` | `0..=1000` |
| `play.inject_input` | `at_frame`, `duration_frames` | `at_frame` clamped to 10 000 000 frames ahead and to `u64::MAX / 4`; `duration_frames` clamped to 1 000 000; an omitted duration is 1 (`InputInjection::schedule` also stores at least 1) |
| `loop.set` | `max_fps` | `1..=10000`, or `null` (clears the agent cap; a profile cap applies again) |
| `loop.set` | `throttled_fps` | `1..=1000` |
| `loop.set` | `fixed_hz` | `1..=1000000000` |
| `loop.set` | `max_substeps` | `1..=64` |
| `anim.set_morph_weights` | `weights` | target indices below 4096, at most 4096 weights |
| `vfx.place_rut` | `points`, `spacing` | at most 4096 stamps; every coordinate finite in f32 |
| `players.get` | `index` | `0..=255` |

Vectors (`[x, y, z]`) for physics, navigation, picking and decals must be finite *after* conversion
to f32: `1e300` is a valid JSON number but an infinite f32, so it is `F0002`.

`scene.apply_edits` validates every edit before it applies any, and validation never changes the
world: a `dry_run` with a `despawn` edit leaves the entity in place.

```json
{ "method": "play.step", "params": { "frames": 18446744073709551615 } }
→ { "error": { "code": -32602, "message": "`frames` must be an integer in 1..=10000",
               "data": { "code": "F0002", "hint": "check the method's parameters in the agent guide" } } }
```

## On the network

A multiplayer server takes bytes from every connected peer. `regia_net`'s server refuses what it cannot decode and
tells the host (`ServerEvent::Rejected`, ADR-7892): an input bundle with a field past its cap (an over-long command
tail, more than 32 inputs) disconnects only its sender at once with `protocol_error`; other undecodable input and game
messages, including bytes left after a bundle, score malformed points until the sender is dropped with
`malformed_packet`. See [Net model (agents)](net-model.md#hostile-input-and-input-readiness-adr-7892).

Every decode path on an authenticated link charges its sender and names it; none drops bytes for free (spec 120 § 11.2,
ADR-7984):

| Path | What is refused | Kind | What happens |
|---|---|---|---|
| server, input lane | an input bundle with a field past its cap | `input_too_long` | sender disconnected with `protocol_error` |
| server, input lane | a truncated or invalid bundle, bytes after it | `input_malformed` | 16 malformed points |
| server, reliable input lane | an input over 16 KiB / one that does not decode | `input_too_long` / `input_malformed` | disconnect / 16 points |
| server, either input lane | any input from a spectator (refused undecoded) | `spectator_input` | one malformed point |
| server, game lane | a game message that does not decode | `message_malformed` | 16 malformed points |
| server, any | a lane the server does not know | `unknown_lane` | 16 malformed points |
| server, sealed link | a record that authenticated but whose kind byte names no kind, or a client's `Accept`/`Deny` record | (counted in `totals.malformed`) | 16 malformed points (ADR-8666) |
| server, sealed link | a record that fails its open (forged, replayed, damaged) | (auth totals) | nothing: not provably the peer's, and it never stops a later datagram from being opened (ADR-8606, ADR-8666) |
| server runtime | a server-only message type (`ToOwner`, `Broadcast`) from a client | `server_only_message` | sender disconnected with `protocol_error` |
| server runtime, voice relay | a voice packet that does not parse (after a token of the sender's voice bucket) | `voice_malformed` | 16 malformed points |
| server runtime, voice relay | a voice control message with an unknown kind, or a block account over 256 bytes | `voice_malformed` | 16 malformed points |
| client | a server message that does not decode on the reliable stream | (client) | link ends with `protocol_error`; the client says goodbye |
| client | a server message that does not decode on the unreliable channel | (client) | dropped, `totals.malformed` + 1 |

The endpoint's kinds arrive as `ServerEvent::Rejected`; the replication runtime's (`server_only_message`,
`voice_malformed`) as `NetNode::take_rejections`, and an agent reads them in `net.status` `rejections` (`total`,
`dropped`, the newest 16 with `peer`, `kind`, `detail`, `tick`). Only traffic that authenticated as the peer's (or, on a
plain link, came from its validated address) scores it (ADR-8606). Malformed points add up per peer at each kind's
`RejectKind::malformed_weight` (16; `spectator_input` 1) and leak 4 points a second, at the endpoint and in the runtime
alike (ADR-8666); no packet or pass earns credit. At 1024 the peer leaves with `malformed_packet`: a sustained malformed
stream goes in about a second, one refusal every 4 s never accumulates. Refusal log lines share the per-peer budget of 10
a second.
Check it with `cargo test -p regia_net --test swallowed_decode` and
`cargo test -p regia_net --features voice --test swallowed_decode_runtime` (real UDP loopback, encryption required).

## In content files

The loaders of scenes (`.level.ron`, `.prefab.ron`, postcard and cooked scenes), paks (`.fpak`),
baked GI volumes (`.fgiv`) and materials (`.material.ron`) return an error for anything they cannot
use. A file that parses but describes something no bake or editor writes is handled where it is used:

- A cooked scene whose parent links form a loop spawns with one link per loop dropped, and a
  warning in the spawn result naming the entity. Prefab added children are handled the same way.
- A prefab instance that would expand past 2^20 entities and edits (nested prefabs that fan out)
  fails to spawn with `SceneSpawnError::Invalid`.
- `added_children` nested more than 64 deep is a decode error.
- A `.fgiv` whose brick counts overflow or whose origins are not finite is a layout error. A
  terrain layer without columns, or a brick with a zero count, loads; the baked GI lookup treats it
  as empty.

## How this is checked

Deterministic fuzz tests run in every gate (no nightly toolchain, no random seeds; see ADR-538):

| Test | What it feeds | Past the parse |
|---|---|---|
| `regia_agent/tests/dispatch_fuzz.rs` | every method with hostile params of every JSON type, random combinations, mutated request text | the permission check, rate limit, handler and audit record, then two frames |
| `regia_scene/tests/fuzz.rs` | mutated `.level.ron`, `.prefab.ron`, postcard and cooked scenes | spawn with the prefab baked, transform propagation, snapshot |
| `regia_asset/tests/fuzz.rs` | mutated paks, `.fgiv` v1 and v2, the full material | every pak entry read, every probe queried, the material saved, reloaded and cooked |
| `regia_test_utils/tests/loader_fuzz.rs` | mutated `.fgiv` and `.material.ron` | the renderer's baked GI lookup and material conversion |
| `regia_net/tests/fuzz.rs` `over_cap_input_tails_drop_only_their_senders` | seeded input streams of four players: valid samples, junk (unknown tags, truncated tails, stray bytes) and one over-cap command tail each | the server's refusal events, the over-cap disconnect of exactly that sender, the clean player's every tick |

Run one crate's set with `cargo test -p regia_agent --test dispatch_fuzz` (or `-p regia_scene
--test fuzz`, and so on). A failure prints the method and parameters, or the loader, that panicked.

## Review for v0.5.0 (ADR-5869)

A review of the asset and agent-protocol input paths for v0.5.0 found and fixed these; each has a test that fails on
the code before the fix:

- A mod pak's one entry may decompress to at most `ModsLimits::max_file_bytes` (1 GiB, ADR-5993), checked from the table of
  contents; mod paks are read through the file, not the mapping, so a pak rewritten in place cannot fault a read.
- The glTF checked parse refuses a node graph that is not a forest (glTF 2.0 § 3.5.3: one parent, no cycles) and
  holds the zero-filled elements of accessors without a buffer view to 16 times the file's buffer bytes (at least
  64 MiB); image headers are read first and an edge over 32768 or more than 4 GiB of RGBA8 in all is refused before
  any decode. The plant envelope loader, the scene export's source read and an animated glTF's buffers (the BVH
  retarget target, which must now be a project file) go through the same checks.
- KTX2 bytes handed to `CompressedTexture::from_ktx2`, `texture_row` or the BCn-to-ASTC transcode get the DFD guard
  the cooked texture already had; a colour LUT PNG's shape is checked before its frame buffer is allocated; the pak
  summary saturates hostile declared sizes; a chunk mount looks ids up by index (it scanned the table per id).
- Agent methods: see `platform.power_state`, `demo.record_stop`, `content.check_host`, `configurator.open_product`
  (a network or device `root` is `configurator_bad_path`) and the DCC feed's linked-folder refusal on their pages;
  the terrain file writers are `mutating`.

Every frozen content format, the net datagrams and the agent request text also have a target in the fuzz-target
registry (spec 97, ADR-1917): `regia_agent/tests/fuzz_targets.rs` fails the gate when a format has no target, and a
running dev build fuzzes one target, or a project file as the fixture, through `fuzz.targets` and `fuzz.run`. See
[Fuzzing](fuzzing.md).
