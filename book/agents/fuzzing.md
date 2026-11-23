# Fuzzing parsers

Every file a project holds, every datagram a peer sends and every request an agent writes is untrusted. The
engine keeps one registry of **fuzz targets**: each is a valid fixture plus the loader that parses it, uses it
(validation, compilation or instantiation where that runs on the CPU) and writes it back. The same registry
runs in every merge gate and inside a running dev build through two methods, so an agent can check that a
parser survives bad input, or that a project file it just wrote cannot crash the loader. Spec:
`docs/specs/97-fuzz-hardening.md`; decision ADR-1917.

## What a run checks

A run mutates the fixture `iterations` times with seed-stable mutators. Binary targets get truncation, bit flips,
spliced and zeroed windows, hostile length and version words. Text targets alternate two kinds: even iterations
corrupt the syntax (truncation, stray tokens, deep nesting, `NaN`/`inf`, unusual Unicode), odd iterations keep the
text well formed and corrupt the values (a number becomes an integer edge such as `4294967296`, an `f32` overflow
or a non-finite value; a string becomes empty, a path escape or a 4096-byte run; a boolean flips; a list is emptied
or its elements repeated), so the input reaches validation and the code that uses it. For each mutated input:

- the loader returns a value or an error with a message; a panic is a finding (`panic`), so is an error with an
  empty message (`empty_error`), and so is an engine bug the loader detects itself (`bug`: for example the
  material-graph target compiles each graph and has naga validate the generated WGSL; invalid WGSL is a compiler
  bug, not bad input);
- when the input loads, its re-encoding must load again and re-encode to the same text or bytes (`round_trip`).
  Text that differs only in the order of map entries passes when the parsed values are equal.

The same target, fixture, seed and iteration count always give the same counts on every platform: there is no
clock and no OS randomness in the mutators.

## `fuzz.targets`

No parameters. Answers `{ targets, count, available }`. Each target is
`{ name, format, format_version, input, round_trip, source }`: `format` is the frozen-format registry name it covers
(`docs/formats/format-hashes.ron`) or `null`; `input` is `text` or `bytes`; `round_trip` is `false` for a format
that has no writer; `source` is `builtin` for the engine's registry or `extension` for a target a crate added at app
build time (below). `available` is `false` in a build compiled with `panic = "abort"`.

```json
{ "jsonrpc": "2.0", "id": 1, "method": "fuzz.targets" }
→ { "result": { "count": 126, "available": true,
                "targets": [ { "name": "statetree", "format": "statetree", "format_version": 2,
                               "input": "text", "round_trip": true, "source": "builtin" }, ... ] } }
```

### Which targets exist

Besides one target per frozen format, the registry has:

| Targets | What they read |
|---|---|
| `net.connect_request` ... `net.deny`, `net.key_init`, `net.key_reply`, `net.sealed_header` | Each handshake and crypto datagram, decoded and encoded again. |
| `net.resume_init`, `net.resume_reply`, `net.resume_retry` | The one-round-trip resume datagrams of wire 6 (ADR-8858), decoded and encoded again. `net.resume_init` also runs what a server does with the fields before any state exists: the resumption ticket's open under fixed ticket keys and the first Noise message's open under the ticket's PSK. Both may refuse; neither may panic. |
| `net.session.<kind>` (`hello`, `welcome`, `refused`, `join_removed`, `join_records`, `in_game`, `travel_begin`, `goodbye`, `keepalive`, `player_removed`, `stream_begin`, `stream_chunk`, `admin_login`, `admin_command`, `admin_output`, `game`) and `net.session_msg` (`level_ready`) | Every session message kind, one fixture each, so mutations land in that kind's body. |
| `net.payload` | A connected packet body (acks, a fragmented reliable message, an unreliable one) through `Connection::receive`, twice. |
| `net.payload_sequence` | A list of `u16`-length-prefixed packet bodies (a 17-fragment reliable message in reverse order, so it waits ahead of a gap) into one connection, 300 ms apart; after every body the reassembly caps must hold and a failed link must hold nothing, else it is a `bug` (spec 120 § 5.2, ADR-7981). |
| `net.server_endpoint` | A real handshake, connected traffic, a disconnect and a key init replayed into a fresh server with a fixed secret: every datagram kind through the server's datagram handler. The input is a list of `u16`-length-prefixed datagrams. |
| `net.relay_server`, `net.relay.<request>` (`lookup`, `punch_intro`, `bind`, `publish`, `list_games`, `allocate`, `refresh`) | The relay's handler for each request kind, against a relay with a registered host; `refresh` goes to the allocation port's handler of a held allocation, from its client and from a stranger (relay protocol 2, ADR-7982). |
| `net.relay_reply.<kind>` (`join_code`, `register_challenge`, `host_info`, `punch_peer`, `directory`, `error`, `allocated`, `client_joined`, `challenge`, `full`, `version_too_old`, `version_too_new`) | The client's relay-reply decoder. |
| `net.server_announce`, `net.pcp_map_response`, `net.nat_pmp_address`, `net.nat_pmp_map` | LAN discovery and port mapping replies. |
| `net.frame_to_client`, `net.frame_to_server` | Replication and message frames (spawns, component state with numbers, strings and lists, a to-server message, acks, and since wire 6 a `RUMSG` frame of `ReliableUnordered` messages each way, ADR-10052) read by a fresh client or server through its whole receive, apply and tick. A frame it cannot read closes the link or kicks the peer: that is a rejection, not a finding. |
| `net.state_frame_apply` | A recorded `STATE` frame applied, after the frames before it, to a scratch client world with a small registry: the client's state-frame apply path (ADR-8761). A frame the client cannot apply is a rejection. |
| `net.destruction_message` | Destruction stream and snapshot messages (every op: define, short and full breaks, sequence skip, a relevance gain in two pieces, check, remove; a snapshot part), each decoded and encoded again. The input is a list of `u16`-length-prefixed messages (ADR-2305). |
| `net.anim_state` | A replicated `NetAnimState` (ADR-2551): decoded, its playback-state words restored into the demo net character's animator and its pose evaluated, then encoded again. A state that does not fit the graph is refused. |
| `extension.stamp`, `extension.objfile.elf`, `extension.objfile.pe`, `extension.objfile.macho` | A dynamic extension's stamp record, and synthetic ELF64, PE32+ and Mach-O 64 libraries exporting it: the bounds-checked object readers find the exported `REGIA_EXTENSION_STAMP` symbol and list the needed libraries. A symbol whose bytes lie outside the input is a `bug` (ADR-2844). |
| `agent.request`, `agent.response` | The JSON-RPC frame and the client's response parse. |
| `agent.params.<group>` | The 30 largest method groups (`render`, `ui`, `nav`, `anim`, `net`, ...: ranked by method count, ties by name). The fixture is one object mapping every method of the group to its worked example from `agent.schema`; value mutations keep that shape, so each call reaches the handler's own parameter parsing. Every call must answer with a result or an error that has a protocol code and a message; anything else is a `bug`. A handler runs only when it can change nothing outside a throwaway headless app (read queries, scene edits and play control without a path, directory, file or URL parameter); the others get the dispatcher's parameter checks only. |
| `ui-group` | The `.ui.ron` v7 fixture (`group_opacity`, `clip_shape`, `mask`), through the loader. |
| `ui-component` | The `.ui.ron` v8 component fixture (`component`, `use`, `params`, `bind_root`, `slots`, `slot`): the declaration checks, then an expansion in which every `use` loads the document itself (cycle, depth, parameter and slot errors). |
| `ui-picture` | The `.ui.ron` v9 fixture (`image` and `mask` named by `asset` or `icon`, `nine_slice`), through the loader and the picture-reference walk the cook and the designer read (ADR-3148). |
| `ui-parity` | The `.ui.ron` v11 containers, markup and focus-neighbors fixture (`split`, `foldable`, `tabs`, `markup`, `nav`; ADR-7827): the loader, the markup spans of every text, a split's runtime ratio (finite and inside its bounds), and a write that reads back. |
| `uisettings` | The project UI settings fixture (`.uisettings.ron`), through the loader. |
| `ui-repeat` | The `.ui.ron` v6 fixture (`repeat`, converters). Every converter runs on hostile values, every repeat's keys go through the keyed diff, and the tree with an edit merges back into the text (ADR-2489): a merge that returns a different tree is a `bug`; refusing to merge is not. |
| `ui-designer` | The `.ui.ron` v11 designer fixture (ADR-7751: `theme` and `animation` bindings, `repeat` `row_height: template`, image `fit: tile`), through the loader. Every binding builds its runtime form and samples its animation at hostile clocks (NaN, infinite, huge, negative), every image tiles a small and a huge rect: a non-finite sample, a non-positive template height or more than `MAX_IMAGE_TILES` copies is a `bug`. |
| `matgraph-wind` | A second `matgraph` target seeded with a v3 foliage graph (the `Wind` node, object position and time driving a vertex offset), compiled like the `matgraph` target, so mutations reach the v3 node and the vertex stage. |
| `matgraph-params` | A third `matgraph` target seeded with a v5 graph carrying parameter metadata (categories, tooltips, ranges with and without a step; ADR-8105 (b)), compiled like the `matgraph` target, so mutations reach the loader's range checks. |
| `vfx-bounds` | A second `vfx` target seeded with a v8 effect that has authored fixed bounds (`system.bounds`, P-225): the loader's box checks (finite, `min <= max`), then the culling volume and the measured bounds of the result; a non-finite box from an accepted definition is a `bug`. |
| `net.sealed_body` | Plaintext records (a kind byte and the body) sealed under fixed fixture keys, opened by the other side and parsed the way that endpoint parses them after the tag verified: a payload packet through `Connection::receive`, a join token, an accept, a deny, a goodbye's reason byte. A record the layer gives back changed is a `bug` (ADR-3283). |
| `net.client_datagram` | A fixed-secret server's challenge, accept, payload and goodbye, replayed into a `NetClient` with a fixed request nonce: every datagram kind through the client's handler. A client still connecting at the end is a rejection (ADR-3283). |
| `net.bridge_frame` | Frame parts (a channel byte and a `NetFrame`) through the bridges' own reassembly (`bridge::FrameAssembler`) with the client's and the server's limits; a reassembled frame that is not its parts is a `bug` (ADR-3283). Channel byte 2 is a part on the unordered RPC stream (wire 6, ADR-10052): it must come out whole as its one part, a split one is refused, and a stream-7 part that touches the ordered reassembly is a `bug`. |
| `net.lobby_query` | A lobby list query in its JSON form (`LobbyQuery`): validated, evaluated against fixed lobbies (each result must match, at most `max_results`), written again (ADR-3283). |
| `jewelry.part_kit`, `jewelry.material_library`, `jewelry.build`, `jewelry.product_package` | The product viewer contracts Regia owns, through the same check as `configurator.contract_check`. |
| `net.snapshot_delta` | Standalone snapshot delta packets (`SnapshotDecoder::decode`): a full packet and a delta against it, split by their `total_len` words and fed to one decoder, once as given and once with each packet's CRC-32 made right so mutations reach the body. |
| `net.ssdp_answer`, `net.upnp_description` | The UPnP IGD discovery parsers over what any LAN device can send: an SSDP `200` answer (`parse_ssdp_answer`, then the description URL), and a gateway's device description (`xml_text` over the document, then `find_wan_service` and its control URL). |
| `net.state_frame_apply` | A recorded `STATE` frame of the unreliable state channel (ADR-8761), mutated and applied after the recorded frames before it to a scratch client world with a small replication registry and `entity_cap` 4 (ADR-8850): the cap refusal path and the "maps <= cap" bound are reachable, and a panic or a map past the cap is a finding. |
| `animgraph-ik` | A second `animgraph` target seeded with the v4 IK example (`docs/formats/chain_ik.animgraph.ron`: a `ChainIK` node with a pole and per-joint cone and hinge limits under a `BodyAim` node with shares and a uniform limit; ADR-8475), loaded and re-saved like `animgraph`, so mutations reach the v4 node bodies and their validation. |
| `animgraph-warp` | A third `animgraph` target seeded with the v4 locomotion warp example (`docs/formats/locomotion_warp.animgraph.ron`: a `Pose` `StrideWarp` with full settings, legs and pelvis under an `OrientationWarp` with spine shares; ADR-8540), loaded and re-saved like `animgraph`, so mutations reach the warp node bodies and their validation. |
| `net.connection_sequence`, `net.connection_sequence.hostile` | An op script (`NSEQ`, then fragments, honest messages cut into fragments and put in any order, unreliable records, duplicates, keepalive runs) into one connection. After every packet a model of the reliable stream (spec 120 § 5.2) checks what it delivers, that an over-long fragment, an over-cap message and a gap held past `max(4 x RTO, 2 s)` of the peer's traffic fail the link, and that nothing fails it before the earliest legal deadline. The `.hostile` fixture is the audit's E6 bomb (ADR-8468). |
| `net.sealed_endpoint_sequence`, `net.sealed_endpoint_sequence.hostile` | The same op script through a real sealed session (`CryptoMode::Required`, a Noise handshake against a fixed-secret server), plus replays, NAT rebinds to new ports, acknowledgments of the server's path-validation probes from the probed port, garbage, forged records under the session's token and 64 KiB datagrams. The server must never hold two peers for the client, never move it to an address it did not use, must move it to a probed port once the client acknowledged the probe from there, and must have dropped it once its stream had to fail (ADR-8468). |
| `net.shared_port_sequence` | An op script (`NPRT`) of datagrams into a `SharedPort` and sends out of its halves (challenges, accepts, sealed records under tokens). Every datagram must reach exactly the halves the routing rules of the module docs name; one longer than an endpoint reads reaches neither (ADR-8468). |
| `net.voice_relay_route` | An op script (`NVOX`) against a `VoiceRelay`: peers join and leave channels and route packets, garbage and 64 KiB ones. A forwarded copy carries the sender's slot, never goes back to the sender and only goes to a peer the relay holds (ADR-8468). |
| `net.snapshot_sequence` | An op script (`NSNP`) of spawns, updates and despawns, encodes against the client's acknowledged baseline (or none, the oldest, or one it never saw) and deliveries that drop, reorder and duplicate packets, through `SnapshotEncoder` and `SnapshotDecoder`. Every snapshot the client decodes must equal the quantised world the server encoded at that sequence number (spec 120 § 8.1, ADR-8468). |
| `net.input_bundle_budget` | An input bundle of large legal samples (600 and 280 B), decoded whole and encoded again; it must fit one datagram (ADR-8468). |
| `net.link_pair_impaired` | A script (`NPAR`) of one impaired link (seeded delay, jitter, loss, duplicates, reordering, one two-way outage of up to 6 s, each read inside the envelope of the seeded link test) and up to 48 reliable messages of up to 20 KB sent by either end. Two real connections poll every 10 ms through a `NetConditioner` per direction, so the senders' own loss detection, probe timeouts, outage recovery and pacing do the resending. Any link failure (an honest sender stalled out by the reassembly deadline was the case it was written for), a message out of order or twice, or one still missing after 120 s of virtual time is a finding (ADR-8521). |
| `net.lobby_hub_msg` | The self-hosted lobby hub's requests and events (`lobby::hub::HubMessage`, one of every kind): decoded with the hub's bounded decoder (a count or length over its cap is refused before anything is read, trailing bytes are an error), re-encoded, and decoded again; a message that does not round-trip is a `bug` (ADR-8559). |

Crates above the agent add their own targets (`source: "extension"`): with the Lua host installed, `lua.luamod` (the
mod manifest) and `lua.luastate` (the save loader: the mod state a save holds is decoded, checked, restored into a
running mod and saved again); in the editor, `import.gltf` (a GLB with an indexed, textured triangle and a material,
through the whole glTF import), `import.obj` (OBJ to GLB, then the glTF import) `import.bvh` (BVH parse, convert, key reduction, retarget onto itself), `import.ply` and `import.stl` (convert to GLB, then the glTF import, ADR-3244), `import.psd` (decode the composite and a named layer), `import.font_subset` (subset a font, read the result back), `import.aseprite` (a two-frame tagged sprite: parse, composite, expand tags, pack the strips) `import.svg` (check, then rasterize an SVG document) `import.materialx` (MaterialX import and an unvalidated graph compile, ADR-3341), `import.ldtk` (a one-level project: parse into the merged map and the level maps, ADR-3358), `import.dds` (parse a DDS, cook it for desktop and Android, ADR-3266), `import.usd` and `import.usdz` (ADR-3235: the USD import of a text layer and of a package holding a binary crate layer). ADR-3262 adds the rest of the artist-file importers, each with
a fixture built in code and a loader that runs the whole import: `import.hdr` and `import.exr` (decode, validate, the 2D and
the six-face cube cook), `import.tiled_tmx`, `import.tiled_tmj`, `import.tiled_tsx` and `import.tiled_tsj` (the map import, the
`.tilemap.ron` round trip, the tilemap cook with its tileset image, and the Wang rules of the tileset; the `.tsx` and `.tsj`
targets mutate an external tileset that a fixed map names), `import.ies` (LM-63 parse, resample, cooked payload read back),
`import.audio_wav`, `import.audio_flac` and `import.audio_ogg` (the audio cook's decode and loudness measurement),
`import.webm` (container validation and the cooked payload), `import.heightmap_r16` (a raw 16-bit grid in both byte orders,
tiled into 4- and 256-sample tiles with min and max), `import.atlas_ron` (an `.atlas.ron` and its sprite images packed) and
`import.fbx` (ASCII FBX through the pre-scan, the `ufbx` scene copy, the GLB writer and the glTF import). The file-backed loaders
write the input into a private scratch directory (`regia-fuzz-*` under the temp directory, removed after each call). `ufbx` is
C: these runs fuzz Regia's wrapper, and an AddressSanitizer run (`cargo xtask fuzz run --target import.fbx`) covers the parser
itself. A host can add more through the
`regia_agent::fuzz_targets::FuzzTargetExtensions` resource.

A few frozen formats live in crates the agent crate cannot reach, so their own crates fuzz them with the same
mutators and the registry test names those tests: `config/validation.ron` (`regia_asset_pipeline`), the three XR
files (`regia_xr`, an optional dependency here), `.session.ron` and `regia-engine.release.ron` (`regia_cli`). The
formats frozen by ADR-2316 that the agent reaches have targets here: `net.lobby_keys` (the engine's lobby data as a
RON map, read the way a joining client reads it), `uianim`, `asset-registry`, `convc`, `bt` (compiled against an
empty task registry, so the graph checks run on every input) and `terrain.weights`; `fuzz.run { target: "bt",
seed: 3, iterations: 200 }` runs one of them. `cooked-video` is the cooked payload of a small WebM: its header and clip
report, then the media bytes through the same validation an import runs. `ecology` is a shipped `.ecology.ron` profile
(parsed, migrated, checked for plausibility, rewritten); `voxel-far` is a two-level cooked voxel far proxy blob read
the way the runtime loader reads it, levels, bricks and samples validated.

## `fuzz.run`

| Param | Type | Meaning |
|---|---|---|
| `target` | string, required | A name from `fuzz.targets`. |
| `seed` | integer | Mutation seed, any u64 (default 1). |
| `iterations` | integer | Mutations, `1..=5000` (default 200). The call blocks the frame it runs in. |
| `path` | string | A project-relative file to mutate instead of the built-in fixture, at most 1 MiB. |

Answers `{ target, seed, iterations, accepted, rejected, panicked, round_trip_failures, empty_errors, bugs, clean,
first_failure }`. `accepted + rejected + panicked + empty_errors + bugs = iterations`. `first_failure` is `null` or
`{ iteration, kind, message, input_len }` where `kind` is `panic`, `round_trip`, `empty_error` or `bug`;
`iteration` is 0-based, so the same seed reproduces it.

```json
{ "jsonrpc": "2.0", "id": 2, "method": "fuzz.run",
  "params": { "target": "statetree", "seed": 7, "iterations": 500, "path": "ai/guard.statetree.ron" } }
→ { "result": { "target": "statetree", "seed": 7, "iterations": 500, "accepted": 61, "rejected": 439,
                "panicked": 0, "round_trip_failures": 0, "empty_errors": 0, "bugs": 0, "clean": true,
                "first_failure": null } }
```

Errors:

| Code | When |
|---|---|
| `F0003` | No target has that name, or the `path` file does not exist or cannot be read. |
| `F0002` | `target` missing, `iterations` outside `1..=5000`, `seed` not a u64, `path` absolute or containing `..` (or leading outside the project through a link), a fixture over 1 MiB, or a non-UTF-8 fixture for a text target. |
| `F0016` | The build aborts on panic (shipping profile, Android release): a found panic would end the process, so the run is refused. |

A panic the run finds also goes through the crash hook, so a crash report shows up in
`diagnostics.crash_reports`. That report is a real engine bug: file it with the target, seed and iteration.
A stack overflow or an allocation failure aborts the process in any build; the loaders cap their nesting and
lengths so the mutators cannot reach either.

## In the gate

`regia_agent/tests/fuzz_targets.rs` runs every target with fixed seeds through the time-boxed harness of
`regia_test_utils::fuzz` (a call that does not return in 5 s fails as a hang), runs the well-formed value mutations
on every text target the same way, checks the round-trip property on
every accepted input, and fails when a frozen format in `docs/formats/format-hashes.ron` has no target, or a page of
`docs/formats/` names no target, test or reason. Adding a format therefore means adding its target to
`regia_agent::fuzz_targets` in the same change. The older
per-crate fuzz tests listed on [hostile input](hostile-input.md) keep running too.

## The soak

The gate runs a few hundred mutations per target. For a long run over many seeds, every finding listed with the seed
that reproduces it:

```text
REGIA_FUZZ_SOAK_SEEDS=3000 REGIA_FUZZ_SOAK_ITERATIONS=4 REGIA_FUZZ_SOAK_THREADS=4 \
  cargo test -p regia_agent --test fuzz_targets -- --ignored soak --nocapture
```

`REGIA_FUZZ_SOAK_PREFIX=net.` limits it to the targets whose name starts with that text, and
`REGIA_FUZZ_SOAK_FIRST_SEED=3001` starts a second pass where the first ended. `REGIA_FUZZ_SOAK_WALK=8` first walks each
seed's fixture eight mutations away, keeping only mutations that still load, so the run mutates a valid input unlike the
built-in fixture; a finding then also writes that walked fixture to the temp directory and prints its path (pass it as
`fuzz.run`'s `path` to reproduce). A finding prints as
`<target> seed <s> iterations <n>: <kind> at <i>: <message>`; `fuzz.run` with the same target, seed and iterations
reproduces it. `regia_asset_pipeline`'s `import_soak_over_many_seeds` soaks the glTF and OBJ entry points the same
way (same seed variable).

## Evaluating targets

Some formats have a second target that goes past the parse into what the engine does with the data.
`sequence.eval` (ADR-2135) loads a `.seq.ron`, puts it in a sub-sequence library whose other names resolve to a
built-in child that names the input again (so cycles are reachable), samples it on a grid and at every key,
section and marker time, resolves every curve slope and runs a fixed list of section edits. A non-finite sampled
value, a clip weight or sound fade outside `0..=1`, or a section edit that leaves an invalid sequence counts as
a `bug`:

```json
{ "jsonrpc": "2.0", "id": 3, "method": "fuzz.run", "params": { "target": "sequence.eval", "seed": 11, "iterations": 200 } }
```

## Coverage-guided fuzzing

The registry above runs seeded mutation on stable Rust, so a run is cheap and repeats exactly, but it cannot see which code
an input reached. `cargo xtask fuzz` runs the same targets under libFuzzer with AddressSanitizer: the fuzzer keeps the inputs
that reach new code, so hours of it get deeper than any fixed list of mutations (ADR-2723). It needs a nightly toolchain and
cargo-fuzz, runs on Linux or macOS on a build host, and has no agent method: a fuzzing process can end at any input, so it
never runs inside an agent's app. An agent drives the command and reads `--json` and the summary file.

```text
rustup toolchain install nightly --profile minimal -c rust-src -c llvm-tools-preview
cargo install cargo-fuzz --locked

cargo xtask fuzz list [--json]                       # every target: the registry, then the extra targets
cargo xtask fuzz seed [--target <name>]              # the fixtures into fuzz/corpus/<target>/ (gitignored)
cargo xtask fuzz build                               # the ASan binary; run and run-all do this unless --no-build
cargo xtask fuzz run --target <name> --seconds 300 [--workers N]
cargo xtask fuzz run-all --seconds-per-target 60 [--parallel N] [--prefix net.] [--resume] [--summary FILE]
cargo xtask fuzz replay [--pending] [--target <name>]  # stable: every committed finding through the registry
```

`run` and `run-all` exit with failure when they find anything. `run-all` rewrites `fuzz/logs/summary.json` after each target:
`{ format_version, seconds_per_target, targets, executions, findings, rows }`, with one row per target
(`name`, `seconds`, `executions`, `new_units`, `peak_rss_mb`, `corpus_files`, `findings`, `abnormal`); `--resume` skips the
targets it already holds. The other flags: `--timeout-seconds` (120: the first call of a material-graph target is a cold start of tens of seconds under ASan on a loaded host), `--rss-limit-mb` (4096), `--malloc-limit-mb` (2048: one allocation over it is ASan's `allocation-size-too-big`),
`--max-findings` per target (5), `--minimize-seconds` per input (180), `--toolchain`, `--target-dir` (or `REGIA_FUZZ_TARGET_DIR`).

**One binary.** `fuzz/fuzz_targets/registry.rs` is one libFuzzer target that picks the registry target from
`REGIA_FUZZ_TARGET`, so 200 targets cost one sanitized build. To run it by hand:
`REGIA_FUZZ_TARGET=statetree <target-dir>/x86_64-unknown-linux-gnu/release/registry fuzz/corpus/statetree -max_total_time=60`.

**What counts.** The check is `xtask/src/fuzz/one_input.rs`, the verdict `fuzz.run` gives: a panic that leaves the loader, an empty
error, a `bug: ` error and a broken round trip all end the process, so libFuzzer records the input. So do what only a
sanitizer or the fuzzer's limits can see: an out-of-bounds access, a call over the timeout, an allocation over the limit.
A text target reads any bytes, a broken sequence becoming U+FFFD, so every input reaches the parser. One mutation in four
comes from the registry's own mutators, so value mutations that keep RON and JSON well formed get coverage feedback too.

**Extra targets.** Beyond the registry: `lua.luamod`, `lua.luastate`, `import.gltf`, `import.obj`, and the loaders whose
fixtures the registry leaves to per-crate tests, `pak.read`, `cooked.mesh`, `cooked.clip`, `save.payload` (the input is the
save payload, framed with a valid CRC before the decoder sees it). `editor.prefs`, `editor.keymap` and `editor.layout` are not
included: `regia_editor` is not in `xtask`'s crate graph. `fuzz.run` and the editor's tests cover them.

**Findings.** `run` tries each input libFuzzer left again. One that fails again is minimized into
`fuzz/findings/<target>/<hash>`, the file you commit with the fix; its log stays in `fuzz/logs/<target>/<hash>.log`. One that
does not, usually a timeout on a loaded host, stays in `fuzz/logs/<target>/flaky-<hash>` and is reported as `flaky-<kind>`. Only `flaky-timeout` leaves the exit code alone: a crash, out-of-memory or leak that does not repeat depends on what ran before it, and still fails the command, as does a target that ran no input.
Findings are also a read-only corpus for later runs of that target.

`crates/regia_agent/tests/fuzz_findings.rs` replays the built-in targets' findings in the normal gate, so a fixed crash stays
fixed, and `xtask`'s `every_committed_finding_replays_clean` covers every target, extras too; `cargo xtask fuzz replay` does the
same by hand. A finding whose fix belongs to someone else goes under
`fuzz/findings/pending/<target>/<hash>` (`git mv` it): the gate only checks that the directory names a real target, and
`replay --pending` lists those that still fail. Move it out of `pending/` with the fix.

The first registry finding is `fuzz/findings/import.gltf/61f00769a64b311ce82f575db0028a57`: a 32-bit integer `TEXCOORD_0`
that reached an unreachable branch in the `gltf` crate's reader. The importer now refuses component types glTF does not
allow for texture coordinates, colors, joints, weights and indices (ADR-2723).

**What replay cannot see.** It runs without a sanitizer or the fuzzer's limits, so a finding that only ASan or
`-malloc_limit_mb` reports replays clean before its fix too. Those need the crate's own regression test beside the finding.

### Media decoding under ASan

The WebM demuxer and the decoders have their own cargo-fuzz crate, `crates/regia_media/fuzz` (ADR-1869). Copy the workspace
lock first, as `xtask fuzz build` does for `fuzz/`:

```text
cp Cargo.lock crates/regia_media/fuzz/Cargo.lock
cd crates/regia_media/fuzz && mkdir -p corpus/decode && cargo +nightly fuzz run decode corpus/decode ../tests/fixtures -- -max_total_time=1200
```

Its findings go in `crates/regia_media/fuzz/findings/decode/` by hand, and `tests/fuzz.rs` replays them
(`committed_decode_findings_stay_fixed`). The Vorbis comment count of ADR-2723 is the first; `tests/vorbis_hostile_headers.rs`
measures the allocation, since a replay without the fuzzer's limit cannot see it.

Leak checks stay on here. Over the registry they are off (ADR-2723): many loaders build and drop whole worlds, and the check
costs more than the input; growth still ends in the RSS limit. libFuzzer only enforces `-malloc_limit_mb` while its leak
checks are on, so over the registry the cap on one allocation is ASan's `max_allocation_size_mb`, which `xtask fuzz` sets to
the same value.
The `shot` target covers look-review `.shot.ron` lists through `ShotList::from_ron` and
`ShotList::to_ron`, including version checks, validation, mutation and canonical round trips.
Its seed matches `docs/formats/shot.example.ron`. Select it with `fuzz.run { "target": "shot" }`.

## The editor's own files

`editor.prefs`, `editor.keymap` and `editor.layout` are `fuzz.run` targets in a running editor, and since ADR-7902 so are
`editor.window_state`, `editor.inspector_pins`, `editor.profile`, `editor.view_state`, `editor.snap_steps`,
`editor.saved_searches` and `editor.theme` (the editor's other persisted files; `regia_editor::baseline_fuzz`'s test
runs all ten). Because `regia_editor` is not
in xtask's graph, the guided fuzzer does not reach them; `crates/regia_editor/tests/editor_files_fuzz.rs` (ADR-3290) covers
them and `Regia.toml` as Project Settings reads it: 12 seeds of 600 mutations per file with the engine's mutator, every
truncation of a rich fixture, hostile inputs (deep nesting, NaN split fractions, huge lines, invalid UTF-8), and the same bytes
as files on disk, where the editor must keep its defaults and record the error. Its round-trip check is stricter than
`fuzz.run`'s: write, read, write must give identical bytes, not just an equal value.

Three more of the editor's state files are `fuzz.run` targets in a running editor (`crates/regia_editor/src/state_fuzz.rs`),
each through the loader the editor reads the file with:

| Target | File | A malformed file |
|---|---|---|
| `editor.view_state` | `.regia/editor_view.ron` (cameras, bookmarks, locks per level); re-encoded | `view_state_parse: <why>` or `view_state_newer: <why>`; the editor keeps its default view |
| `editor.autosave_index` | `.regia/autosave/docs/index.ron` (the document autosave index); re-encoded | `not an autosave index: <why>` or `index version N is not 1`; nothing is offered for recovery. A row whose `id` or `file` names a path outside the docs folder is dropped |
| `editor.tool_manifest` | `<id>.tool.toml` (an editor tool's manifest); no writer, so no round trip | `TOML parse error: <why>`, `` `format = 1` is missing ``, `format N is newer than this editor reads (1)`, `invalid manifest: <why>`; the tool is listed with the error and never starts |

```json
{ "method": "fuzz.run", "params": { "target": "editor.autosave_index", "iterations": 200 } }
```

### Terrain authoring sources (CT-18, ADR-8362)

`terrain.source_json` and `terrain.source_png` cover schema-2 count JSON and
grayscale PNG imports. Each loader caps encoded input at 16 KiB and decoded data
at 64 samples. JSON additionally caps nesting at eight and ignored values/keys at
64, with assertions on charged/stored samples and sample-vector capacity. The
fixtures are synthetic and the loaders consume validated meter heights. These
external formats have no Regia source writer, so `round_trip` is false. For
request/project import ceilings and stable errors, see
[Terrain sources and shaded overviews](terrain-source.md).
### Bounded glTF content diagnostics (ADR-8226)

The editor and guided registry include `content.gltf_bounded`, registered through
`regia_asset_pipeline::fuzz_entry::more_entries`. It exercises the shared GLB chunk
scanner, JSON shape admission, URI validation, buffer decoding and normalization.
The harness has no external file IO. Its fixed ceilings are 64 KiB input/JSON,
128 KiB admitted reads, 1 MiB decoded reservations, 1,024 accessors and 65,536 work
units; counters are asserted on errors as well as successful inputs. The normal
pipeline test mutates every byte and truncation of a valid GLB and hostile URI
strings without depending on wall-clock assertions.

```sh
cargo test -p regia_asset_pipeline --test content_lint_budget bounded_fuzz_arbitrary_chunks_and_uri_strings
cargo xtask fuzz run --target content.gltf_bounded --seconds 300
```

The guided run is an integration/lead command. Limits and cancellation behavior
for authoring operations are documented in [Content lint](../user-guide/content-lint.md).

### Terrain authoring sources (CT-18, ADR-8362)

`terrain.source_json` and `terrain.source_png` cover schema-2 count JSON and
grayscale PNG imports. Each loader caps encoded input at 16 KiB and decoded data
at 64 samples. JSON additionally caps nesting at eight and ignored values/keys at
64, with assertions on charged/stored samples and sample-vector capacity. The
fixtures are synthetic and the loaders consume validated meter heights. These
external formats have no Regia source writer, so `round_trip` is false. For
request/project import ceilings and stable errors, see
[Terrain sources and shaded overviews](terrain-source.md).

`terrain.raster_descriptor` and `terrain.source_tiles` (ADR-8639) cover the two frozen
terrain source descriptors, `heightfield.raster.json` (`terrain-raster-descriptor`) and
`heightfield.tiles.json` (`terrain-source-tiles`). Both read the version key before the
typed parse and refuse unknown fields. The raster loader re-encodes through the serde
writer, so `round_trip` is true; the tile-set loader validates structure (paths, bounds,
overlap, coverage) without reading a tile and has no writer, so `round_trip` is false.
Their fixtures are the schema examples, also frozen as corpus v1.

```json
{ "method": "fuzz.run", "params": { "target": "terrain.source_tiles", "iterations": 200 } }
```
