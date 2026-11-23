# 97 — Fuzz and property-test hardening for untrusted parsers and the agent protocol

Status: proposed (ADR-1917, ADR-2723). Plan row: `master-plan.md` "Fuzzing, determinism, soak harnesses" (exists, ADR-537/538/867) and the
proposed security-review row (fuzz targets green, including the JSON-RPC frame parser). Source: the Regia docs; no reviewed
public spec set exists for this item. Crates: `regia_core` (mutator), `regia_test_utils` (time-boxed harness),
`regia_agent` (target registry and agent methods), and the format crates whose parsers the targets find faults in.

## 1. Purpose and scope

Every byte Regia reads from a project, a pak, a save slot, the network or an agent is untrusted. The promise (hostile-input page,
ADR-538): a bad input ends in an error a person can read, never in a panic, a hang or an unbounded allocation; and a value that
loads can be written back and loaded again unchanged.

**Baseline.** ADR-538 built the deterministic harness (`regia_test_utils::fuzz`: SplitMix64, byte and text mutators, a
time-boxed per-call runner) and per-crate fuzz tests for 18 frozen formats (then 45 in `docs/formats/format-hashes.ron`, now 100) plus
`.fgiv`, cooked headers, glTF, replays and agent dispatch. Nothing tied the list of formats to the list of fuzz targets, so
a new format shipped unfuzzed unless someone remembered: while this spec was being built the frozen list grew by 35 formats, none
with a target, and the `regia_net` datagrams and the agent request text had none either. The round-trip property is checked only on hand-written fixtures, never on the values the mutator finds.

This spec adds:

1. **One registry of fuzz targets** (`regia_agent::fuzz_targets`) that covers every frozen format, every other untrusted parser
   the agent crate can reach (`regia_net` datagrams and session messages, voice-chat packets, agent request text, the WebM demuxer,
   `.regiatrace`, ban lists, `server.toml`, input bindings, the extension manifest, PNG input, lightmaps, decals, data tables), and is
   checked by two tests: a key of `docs/formats/format-hashes.ron` without a target fails the gate, and so does a page of
   `docs/formats/` that names neither a target, a fuzz test, nor the reason nothing parses it.
2. **A round-trip property** on every mutated input that loads: `encode(load(x))` loads again and re-encodes to the same text or
   bytes (idempotence of load/encode). Text that differs only in map order is accepted when the parsed RON values are equal.
3. **The fixes** for every panic and property failure the targets find, in the owning crate.
4. **Agent methods** `fuzz.targets` and `fuzz.run` so an agent can fuzz a target (or a project file as the fixture) inside a
   running dev build.

Out of scope here: fuzzing GPU paths, the desktop-only
`regia_asset_pipeline` importers and files (their tests fuzz glTF, OBJ, fracture sets and `config/validation.ron`; the pipeline is
not a dependency of `regia_agent`), and the agent dispatch fuzz (exists, ADR-538).

## 2. API shape

### 2.1 Mutator (`regia_core::fuzz`)

The seed-stable PRNG and mutators move from `regia_test_utils::fuzz` to `regia_core::fuzz` unchanged (same constants, same
strategy order, so every existing seeded test sees the same inputs); `regia_test_utils::fuzz` re-exports them.

```rust
pub struct FuzzRng(u64);                       // SplitMix64
impl FuzzRng { pub const fn new(seed: u64) -> Self; pub fn next_u64(&mut self) -> u64; pub fn below(&mut self, n: usize) -> usize; /* range, chance, byte */ }
pub fn mutate_bytes(bytes: &[u8], rng: &mut FuzzRng) -> Vec<u8>;
pub fn mutate_text(text: &str, rng: &mut FuzzRng) -> String;
/// New: keeps RON/JSON well formed and corrupts values (integer edges, f32 overflow, NaN/inf, hostile strings,
/// flipped booleans, emptied or repeated lists, at most 64 KiB more), so input reaches validation and consumers.
pub fn mutate_text_values(text: &str, rng: &mut FuzzRng) -> String;
```

A run alternates `mutate_text` (even iterations) and `mutate_text_values` (odd) on text targets.

### 2.2 Target registry (`regia_agent::fuzz_targets`)

```rust
pub enum FuzzInput {
    /// RON/JSON text; `load` parses, consumes and re-encodes to canonical text.
    Text { fixture: fn() -> String, load: fn(&str) -> Result<String, String> },
    /// Binary; `load` decodes, consumes and re-encodes to canonical bytes.
    Bytes { fixture: fn() -> Vec<u8>, load: fn(&[u8]) -> Result<Vec<u8>, String> },
}
pub struct FuzzTarget {
    pub name: &'static str,             // "statetree", "net.relay", "agent.request", ...
    pub format: Option<&'static str>,   // the frozen-format registry name it covers, if any
    pub format_version: u32,            // current version of what it parses (0: unversioned wire data)
    pub input: FuzzInput,
}
pub fn targets() -> &'static [FuzzTarget];
pub fn target(name: &str) -> Option<&'static FuzzTarget>;
pub const BUG_PREFIX: &str = "bug: ";       // a loader-detected engine bug, not a rejection
pub enum FailureKind { Panic, RoundTrip, EmptyError, Bug }
pub struct FuzzFailure { pub iteration: u32, pub kind: FailureKind, pub message: String, pub input_len: usize }
pub struct FuzzReport { pub accepted: u32, pub rejected: u32, pub panicked: u32, pub round_trip_failures: u32,
                        pub empty_errors: u32, pub bugs: u32, pub first_failure: Option<FuzzFailure> }
/// Err only when `fixture` is not UTF-8 for a text target.
pub fn run(target: &FuzzTarget, fixture: Option<&[u8]>, seed: u64, iterations: u32) -> Result<FuzzReport, String>;
pub fn check_round_trip_text(load: fn(&str) -> Result<String, String>, canonical: &str) -> Result<(), String>;
pub fn check_round_trip_bytes(load: fn(&[u8]) -> Result<Vec<u8>, String>, canonical: &[u8]) -> Result<(), String>;
```

Registry files: `formats.rs` (content formats, with the reflected-component helpers), `gameplay.rs` (abilities, conversations,
curves, choosers, functional tests, ragdolls, spatial queries), `world.rs` (procgen, biomes, roads, level directories, layers,
splines, shapes, content chunks, video sources), `files.rs` (unfrozen files and small text parsers), `net.rs` and `wire.rs`.

`load` goes past the parse where a CPU-only consumer exists (validation, compilation, instantiation); its `Ok` is the
re-encoded value. A target whose format has no writer returns an empty encoding and the property is skipped for it (the
registry says so).

No new file format: this spec reads every format it targets with that format's own loader and writes nothing to disk
(`format_version` of the registry is not a file; no migration note applies). The coverage test reads
`docs/formats/format-hashes.ron` as data.

A reflected component target decodes the way `spawn_scene` does and then calls the check `spawn_scene` calls:

```rust
// regia_scene::spawn
pub trait FormatVersion { const CURRENT: u32; }         // add `FormatVersion` to the type's #[reflect(...)] list
pub struct ReflectFormatVersion { /* type data */ }
pub fn check_format_version(registration: &TypeRegistration, value: &dyn PartialReflect) -> Result<(), String>;
```

A component at a newer `format_version` stays out of the world with the warning an unknown type gets; under
`UnknownTypePolicyOption::Preserve` its text is kept for the next save. `Spline`, `Shape`, `RoadOf`, `ProcGenAnchor`, `PaintedPoints`,
`BiomeArea`, `ProcGenerator`, `InstancedMeshSet` and `FunctionalTest` carry it (ADR-1917 finding 2). `Primitive` and `PostVolume`
keep their own `migrate` call in the systems that consume them.

Two small text entry points exist for the registry, because their loaders only took a path:
`ExpectedStore::from_ron_str` and `ServerSettings::from_toml` (`load` and `resolve_config` call the same code).

### 2.3 Gate tests (`regia_agent/tests/fuzz_targets.rs`)

- `every_frozen_format_has_a_fuzz_target`: each key of `format-hashes.ron` is some target's `format`, or is listed in
  `COVERED_ELSEWHERE` with the test file and function that fuzz it (that file must contain that `fn`).
- `every_format_doc_is_covered`: each page of `docs/formats/` has a row in `FORMAT_DOCS`: a registry target, a test file and
  function that exist, or the reason nothing reads the format from outside the build.
- `every_target_fixture_loads_and_round_trips`: the unmutated fixture loads and passes the round-trip check.
- `every_target_survives_mutation`: each target runs through the time-boxed `fuzz_loader`/`fuzz_text_loader` (seeded, fixed
  iteration count): no panic, no hang, canonical bad inputs rejected. Wire data without a magic word
  (`net.input_bundle`) uses `fuzz_headerless_loader`; files built to survive a cut (`media.webm`, `telemetry.regiatrace`) use
  `fuzz_tolerant_loader`, where a truncation is valid input and only the empty input must be rejected; the relay handler (it answers bad requests instead of failing) is
  checked by the registry run only.
- `every_text_target_survives_value_mutation`: the well-formed value mutations through the time-boxed
  `fuzz_text_values_loader`, so a huge count that makes a loader loop fails as a hang.
- `every_target_round_trips_what_it_accepts`: for every mutation that loads, the round-trip property holds.
- Outside the registry, because the crate is not a dependency of `regia_agent`: `regia_cli` `mutated_resource_uris_reach_real_methods_or_nothing`
  (the MCP adapter's resource URI, the one string it parses itself; rmcp parses the MCP frames and `agent.request` covers the
  JSON-RPC frame), `regia_asset_pipeline/tests/fuzz.rs` (fracture sets, `config/validation.ron`), and, under the `dcc-feed`
  feature, `regia_agent/tests/dcc_feed_hostile.rs` (DCC frames, `.rfdrec`).
- Agent method tests: `fuzz.targets` lists the registry; `fuzz.run` reports counts, rejects unknown targets (`F0003`), bad
  iteration counts (`F0002`) and paths outside the project (`F0002`).

## 3. Agent-protocol surface

| Method | Params | Result |
|---|---|---|
| `fuzz.targets` | none | `{ targets: [{ name, format, format_version, input: "text"\|"bytes", round_trip }], count, available }` |
| `fuzz.run` | `target` (string, required), `seed` (u64, default 1), `iterations` (1..=5000, default 200), `path` (project-relative fixture file, optional, at most 1 MiB) | `{ target, seed, iterations, accepted, rejected, panicked, round_trip_failures, empty_errors, bugs, clean, first_failure: { iteration, kind, message, input_len } \| null }`; `kind` is `panic`, `round_trip`, `empty_error` or `bug` (a loader-detected engine bug, error text prefixed `bug: `, such as generated WGSL that naga rejects) |

Errors: unknown target, or a `path` file that does not exist or cannot be read, `F0003`; out-of-range `iterations`, a non-u64 `seed`, a path that escapes the project or a file over 1 MiB
`F0002`; a build compiled with `panic = "abort"` (shipping profile) answers `fuzz.run` with `F0016` (unsupported), because a found
panic would end the process. `fuzz.run` is read-only (it writes nothing; `Permission::Read`). A panic it finds still runs the
crash hook, so the crash report appears in `diagnostics.crash_reports`: that is a real bug to file.

Guide page: `book/agents/fuzzing.md` (topic `fuzzing` in `agent.guide`), linked from `book/agents/hostile-input.md`.

## 4. Determinism

Every run is a pure function of `(target, fixture, seed, iterations)`: the mutator is SplitMix64 with no clock or OS entropy,
the loaders are deterministic, and the same seed on Linux, Windows and Android aarch64 produces the same inputs and counts.
Gate tests use fixed seeds and fixed iteration counts and no wall-clock asserts; the only timer is the harness's existing hang
guard (a call over 5 s fails, it never decides a pass).

## 5. Tests and acceptance

1. The coverage tests pass with every frozen format and every page of `docs/formats/` covered (100 formats when this was written, 183 at
   the 2026-11-10 audit; the test reads the registry, so the count is not part of the contract, ADR-5660): most in the registry;
   `bake` through `regia_pt`'s own test (an optional dependency of `regia_agent`), `luamod` and `luastate` through
   `regia_script_lua`'s own tests (that crate depends on `regia_agent`), `fracture` through `regia_asset_pipeline`'s own test
   (an editor-side crate the runtime registry must not depend on), and `save`, `pak`, `cooked-mesh`, `cooked-clip` through the per-crate tests that build
   their multi-part fixtures.
2. Every target survives its mutation run and holds the round-trip property; each fault found is fixed in its crate with a
   regression test there, and listed in ADR-1917.
3. `fuzz.targets` and `fuzz.run` are in the schema, dispatched, covered by `coverage_calls`, documented in the book page,
   `book/COVERAGE.md` and `agent.guide`.
4. The existing per-crate fuzz tests produce the same inputs after the mutator move (same seeds, same sequence);
   `regia_core/tests/fuzz_stability.rs` pins the published SplitMix64 stream and the seeded mutator output (ADR-5660).

## 6. Platform notes

Pure CPU, no `cfg(target_os)` code. Linux, Windows and Android aarch64 build the same registry; the agent server is a dev-build
feature on all three. `panic = "abort"` builds (Android release, shipping) answer `fuzz.run` with `F0016`.

## 7. Performance budget

No per-frame cost: nothing runs unless `fuzz.run` is called; there is no system, no resource and no render work, so no frame
measurement applies. `fuzz.run` runs on the agent's request handler and blocks that frame for the call: at the default 200
iterations of a small fixture it is well under a frame on desktop; the 5000-iteration cap bounds the worst case. A `tracing`
span (`fuzz.run`) wraps the call. Gate cost: the registry tests run about 45 targets x (fixed iterations) small parses.

## 8. Out of scope (later)

GPU-side validation of decoded assets; running
the procgen evaluator from the registry (`regia_procgen/tests/fuzz.rs` runs it with its limits on mutated graphs); the non-Rust media
decoders of ADR-1593 (the demuxer is fuzzed here; a decoder library's fuzzing is part of adopting it).

## 9. Additions (ADR-2084)

- **Net message kinds**: one target per `SessionMsg` kind, the connected packet body, `KeyInit`, `KeyReply`, the sealed
  header, the server endpoint (a fixed-secret plain server replays a handshake and connected traffic), each relay request
  and reply, NAT-PMP, and the replication frames a client and a server read (`net_kinds.rs`).
- **Agent parameters**: `agent.params.<group>` for the 30 largest built-in method groups (`agent_params.rs`). The fixture
  is `{ method: contract example }`; each call goes through `dispatch::call_in_process` in a fresh headless app with
  `AgentPlugin` (built, not finished). `handler_runs` decides whether the handler runs or only the parameter gate.
- **Extensions**: `FuzzTargetExtensions` (a resource); `fuzz.targets` entries gain `source`. `regia_script_lua` adds
  `lua.luamod` / `lua.luastate`; the editor adds `import.gltf` / `import.obj`. The gate's `COVERED_ELSEWHERE` keeps
  naming the per-crate tests for `luamod` and `luastate`, because the gate test cannot see extension targets.
- **Soak**: `soak_every_target_over_many_seeds` (ignored, env-configured) and `import_soak_over_many_seeds`.
- **Snapshot delta and UPnP (w7-net-fuzz-seam-z)**: `net.snapshot_delta` (a full packet and a delta through one
  `SnapshotDecoder`, also with the CRCs resealed), `net.ssdp_answer` and `net.upnp_description` (`files.rs`).

## 10. Coverage-guided fuzzing (ADR-2723)

The nightly `cargo fuzz` wrappers and the corpus of past findings, once listed in § 8, are built:

- **`fuzz/`**: a cargo-fuzz crate outside the workspace. One libFuzzer target, `registry`, picks the target from
  `REGIA_FUZZ_TARGET`. `xtask/src/fuzz/one_input.rs` is the verdict (`check_one`: the classification of `run`, § 2.2, on one input;
  a text target reads its bytes lossily), shared by source with `xtask` and the gate test. `xtask/src/fuzz/extra_targets.rs` adds the
  `FuzzTargetExtensions` targets `xtask` can link (`lua.*`, `import.*`) and the loaders of § 5's per-crate tests
  (`pak.read`, `cooked.mesh`, `cooked.clip`, `save.payload`). The custom mutator takes one mutation in four from § 2.1.
- **`cargo xtask fuzz`**: `list`, `seed`, `build`, `run`, `run-all`, `replay` (`book/agents/fuzzing.md`).
- **`fuzz/findings/<target>/<hash>`**: minimized failing inputs, committed. `<hash>` is 128 bits of BLAKE3 of the file, in hex.
  `fuzz/findings/pending/<target>/<hash>` holds those whose fix is owned elsewhere. `fuzz/corpus/` and `fuzz/logs/` are
  gitignored working state.
- **Gate**: `regia_agent/tests/fuzz_findings.rs`: `committed_findings_stay_fixed`, `pending_findings_name_real_targets`,
  `the_check_catches_every_kind_of_finding`, `every_builtin_fixture_passes_the_check`, `committed_findings_are_named_by_their_hash`
  (ADR-5660).

- **`crates/regia_media/fuzz/findings/decode/`**: the same for the `decode` target, except that the files are copied in by hand
  and keep libFuzzer's artifact name (`oom-<sha1>`), so the BLAKE3 naming rule above covers `fuzz/findings/` only (ADR-5660);
  `regia_media/tests/fuzz.rs` `committed_decode_findings_stay_fixed` replays them, and `tests/vorbis_hostile_headers.rs`
  measures the allocation limits.

A finding is raw input bytes with no header and no version: nothing but the replay reads it, and it stays meaningful for as long
as its target exists. No frozen format is added. Determinism: the stable registry runs of § 4 are unchanged; a libFuzzer run
depends on the clock and is not a gate. Only its committed findings are, and they replay deterministically.
