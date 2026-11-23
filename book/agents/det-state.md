# Lockstep state snapshots

`regia_detsim::state` saves a lockstep simulation's whole state at a tick boundary and loads it back
bit-identical, so a run resumed from the snapshot steps to the same per-tick hashes as the run that
never stopped. It is what a minority seat resyncs from (the majority sends its state), what a late
joiner starts from, what a desync bundle and a lockstep replay carry, and what a rollback restores.
Read "Lockstep math and the determinism scan" first: the same rules apply.

There is no protocol method in this crate (it holds no runtime host state). The sim host's
`sim.snapshot` method (det-core-08) is built on it. The surface here is the error codes below and
the fuzz target `det_state_snapshot`.

## The codec: `DetWriter` and `DetReader`

| Item | Encoding |
|---|---|
| `u8`..`u128`, `i8`..`i128` | Fixed width, little-endian (two's complement for signed). |
| `bool` | One byte, `0` or `1`; any other byte is `bad_value`. |
| `f32`, `f64` | The **raw IEEE bits**, 4 or 8 bytes. `-0.0`, every NaN payload, subnormals and infinities come back exactly. This differs on purpose from the desync hash, which folds `-0.0` and NaN payloads (`hash::canonical_f32_bits`): a hash compares values, a restore must give back the bits a later tick reads. |
| `length(n)` | A `u32`. On read it must be at most `DetReadLimits::max_len`, else `length_limit`. |
| `bytes`, `str` | A length, then the bytes (`str` must be UTF-8). |
| `bits` | A length, then the bits packed eight to a byte, lowest bit first; padding bits must be zero. |
| `seq(items, each)` | A length, then each item. The reader preallocates at most 4 KiB (`PREALLOC_BYTES`) and never more items than bytes remain, so a forged length cannot allocate on its own. |
| `put(&value)` / `take()` | Any serde value in the same codec: fields in declaration order with no names, enum variants by index, `Option` as a `0`/`1` byte. The codec is not self-describing: `skip_serializing_if` and sequences of unknown length are refused (`bad_value`), and so is `deserialize_any`. A hash map or hash set is written in ascending order of each entry's encoded bytes (key, then value), so the same entries give the same bytes whatever the hasher's seed; every other sequence or map keeps its own order (see [Map order](#map-order)). |

### Map order

`put` writes a hash container (`std`'s `HashMap` and `HashSet`, hashbrown's, and wrappers named like
them: the outer type's name contains `Hash` and ends in `Map` or `Set`) sorted by each entry's encoded
bytes, key first, then value. Its iteration order follows the process's hasher seed, so writing it in
that order gave two peers with equal maps different snapshots. Byte order, not the key's `Ord`: the
serializer sees only `Serialize`, a hash map's key need not be `Ord`, and the codec's little-endian
bytes are the same on every host. A `BTreeMap`, a `BTreeSet`, an insertion-ordered `IndexMap` and a
`Vec` keep their own order, because it is part of their value. `take` reads entries in any order.

A custom `Serialize` that walks a hash container's iterator itself (`collect_seq(map.keys())`) is not
recognized; collect it into a sorted `Vec` first. Proof: `cargo test -p regia_detsim --test det_state`
(`same_hash_map_entries_put_to_the_same_bytes`, `same_hash_set_entries_put_to_the_same_bytes`,
`a_derived_state_with_nested_hash_containers_puts_the_same_bytes`: two hasher seeds, one byte string).

`DetReader::new(bytes, limits)` refuses input longer than `limits.max_bytes` before it reads a byte.
Every read is bounds-checked; no input makes the reader panic. `finish()` returns `trailing_bytes`
when bytes are left.

## `DetState` and framed snapshots

<!-- ignore-reason: `Ship` and its fields are the game's; the same calls run in regia_detsim's state module doctest -->
```rust,ignore
use regia_detsim::state::{self, DetReader, DetState, DetStateError, DetWriter};

impl DetState for Ship {
    const STATE_TAG: [u8; 8] = *b"ship\0\0\0\0";
    const STATE_VERSION: u32 = 1;
    fn save(&self, w: &mut DetWriter) {
        w.f32(self.pos[0]);
        w.f32(self.pos[1]);
        w.state(&self.ids); // a nested DetState, e.g. a DetIdAllocator
    }
    fn load(r: &mut DetReader<'_>) -> Result<Self, DetStateError> {
        Ok(Ship { pos: [r.f32()?, r.f32()?], ids: r.state()? })
    }
}

let bytes = state::snapshot(&ship, tick)?;          // never changes `ship`
let (tick, ship): (u64, Ship) = state::restore(&bytes)?;
```

A type that derives `Serialize` and `Deserialize` writes `w.put(self)` and `r.take()` instead.

The frame is a 40-byte header, then the body:

| Bytes | Field |
|---|---|
| 0..4 | Magic `RGDS` |
| 4..6 | Frame version (`FRAME_VERSION`, 1) |
| 6..8 | Kind: 0 a `DetState` body, 1 a `SimHarness` envelope |
| 8..16 | The type's `STATE_TAG` |
| 16..20 | The type's `STATE_VERSION` |
| 20..28 | Tick |
| 28..32 | Body length |
| 32..40 | Integrity hash: a `Mix` keyed by the tag over kind, version, tick and the body |

`read_header(bytes, limits)` checks the magic, the frame version, the kind, the body length and the
hash, so a relay can check a snapshot it does not decode. `frame_bytes` frames a body written
elsewhere. `to_bytes` / `from_bytes` give the body alone.

**Rules for `load`.** Give back a value that steps to the same bits: same orders, raw float bits
(vector capacities do not matter). Check what later code indexes by (an id below the table length,
two lengths that match) and return `DetStateError::bad_value(..)` instead of panicking later.
`save` takes `&self` and must not change the state.

**Version rule.** `STATE_VERSION` is the game's. Any change to what `save` writes is a sim-version
change: bump it. A snapshot of another version is refused with `bad_version`, which names both
versions. There is no migrator, because snapshots are not saved content: they live inside a match,
a desync bundle (`.desync.ron`) or a lockstep replay (`.lsreplay`) recorded by the same build, and
those formats own their own version rows.

## What the engine implements

| Type | Tag | What is saved |
|---|---|---|
| `ids::DetIdAllocator<u16>` / `<u32>` | `detids16` / `detids32` | Cap, fresh cursor, free list in FIFO order, live bits, generations. `load` refuses a broken invariant (`bad_value`). |
| `hash::SystemRing<N>` | `sysring` | `N`, whether the ring keeps raw rows, then the canonical rows oldest first; another `N`, more than 240 rows or ticks that do not increase are `bad_value`. Raw words and partial flags are diagnosis state and are not saved: a loaded row is whole with an empty raw slot (ADR-8621). |
| `tick::TickRate` | `tickrate` | Hz; 0 is `bad_value`. |
| `golden::scenarios::{SteerScenario, SlopeScenario, CollideScenario}`, `Vehicle`, `VehicleParams`, `Terrain`, `SplitMix64` | `steerslm`, `slopefol`, `disccoll`, ... | Every field a later tick reads. |
| `arena::ArenaScenario` | `aiarena` | Settings, units, the per-tick scratch, seed, tick, `dt`, attack count. |
| `broadphase::BroadBody`, `broadphase::BroadphaseConfig` | `brodbody`, `brodconf` | A body's position, radius, band and underground flag; the grid settings. A `DetBroadphase` is a cache: save the bodies and the config, and on load build `DetBroadphase::new(config)`; it visits the same contacts in the same order (only `stats()` restarts). |

`SimHarness::save()` frames the run (seed, tick budget, rate), the completed tick count and the
scenario's body; `SimHarness::resume(bytes)` (or `resume_with(bytes, limits)`) gives back a harness
at that tick, after restoring the default floating-point environment. It does not call `reset`.

## Checkpoints: `SnapshotRing`

`SnapshotRing::new(SnapshotRingSettings { capacity_ticks, every, max_bytes })` keeps a snapshot every
`every` ticks for `capacity_ticks` ticks, and drops the oldest to stay under `max_bytes`.
`record(tick, &state)` saves only on cadence; `push(tick, bytes)` keeps bytes you already have; a
tick at or below the newest one (a rewind) replaces the checkpoints from there on. `at(tick)` and
`latest_at_or_before(tick)` find the checkpoint a rollback or a resync starts from. A single
checkpoint larger than `max_bytes` is refused with `length_limit`.

## Error codes

| Code | Meaning |
|---|---|
| `truncated` | The input ended before a value (`offset`, `needed`). |
| `length_limit` | A length over `max_len`, an input over `max_bytes`, a written length over `u32::MAX`, or a ring checkpoint over the ring's budget. |
| `bad_version` | The frame or the state has another version than this build reads (`found`, `expected`). |
| `bad_tag` | Not a snapshot (wrong magic) or a snapshot of another type (wrong tag). |
| `trailing_bytes` | Bytes left after the frame's body or after `load`. |
| `bad_value` | A value out of its domain, a broken invariant, a frame of the wrong kind, or a serde error. |
| `bad_hash` | The integrity hash does not match: the snapshot was damaged. |

## Verify

- `cargo test -p regia_detsim --test det_state`: `resume_from_a_snapshot_equals_the_uninterrupted_run`
  (steering, slopes, disc collisions and the AI arena; snapshot at several ticks K, resume in a fresh
  instance, every per-tick hash to the end matches), `allocator_round_trip_keeps_fifo_and_generations`,
  `a_rebuilt_broadphase_resumes_the_same_contacts`.
- `cargo test -p regia_detsim --test det_state_reader`: `reader_refuses_bad_input` (each code, and
  20,000 seeded mutations that never panic and never allocate more than 32 bytes per input byte
  plus 64 KiB, under a counting allocator).
- `cargo test -p regia_detsim --lib state`: `raw_float_bits_survive`, `snapshot_ring_is_bounded_in_bytes`.
- The fuzz target `det_state_snapshot` (`cargo test -p regia_agent --test fuzz_targets`).

## Input logs and replay

`regia_detsim::replay` keeps the inputs of every tick so a run can be stepped again, headless, from a
snapshot (DC-3.7b). No protocol method here either: the sim host's `sim.replay_verify` (det-core-08) and
the desync tool's `regia net desync-replay` (N227) are built on it. The surface is the error codes below,
the fuzz target `det_input_log` and the example `cargo run -p regia_detsim --example replay -- <log>`.

| Item | What it is |
|---|---|
| `TickInputs<I>` | Every listed seat's input for one tick: `(seat: u16, Option<I>)` in strictly ascending seat order (`None`: no input by the deadline, or a gap tick). The shape of the net layer's lockstep frame, so a seat converts a received frame with `TickInputs::from_sorted(frame.tick, ...)`; out-of-order or repeated seats are `seat_order`. |
| `DetInputSim<N>` | A sim that is `DetState + SystemFold<N>` and steps from inputs: `type Input: Serialize + DeserializeOwned`, `step(tick, &TickInputs)`, and `hash()` / `system_hashes()` (defaults: the systems folded, `hash::system_hashes`). |
| `advance(sim, &inputs)` | **The** step: steps and hashes. The live loop, `replay`, `CatchUp` and the golden adapter all call it, so a replay runs the code the match ran; there is no replay-only branch. |
| `step_recorded(sim, inputs, &mut log, keep_hashes)` | `advance` plus appending the tick (and its world hash and system words) to an `InputLog`. |
| `InputLog<I>` | Ticks consecutive from `start_tick`, for one sim (its tag, version and system count), each with its inputs and optional recorded hashes; may carry the framed snapshot it starts from. `encode()`, `decode_with(bytes, limits)`, `suffix(tick)`, `entry_mut(tick)`, `truncate(tick)`. `read_input_log_header` reads the sim identity without the input type. |
| `replay(sim, &log, until)` | Steps the log on a sim at its start tick, compares every recorded hash, stops after the first that differs. `replay_from_snapshot` restores first; `replay_bytes` decodes first. |
| `ReplayReport` | `start_tick`, `ticks_run`, `end_tick`, `hashes_compared`, `first_diverging_tick`, `system` (named through `hash::diverged_systems`), `systems`, `last_agreed`, `final_world`; `verdict_line()`. |
| `CatchUp<I>` / `catch_up(sim, &mut frames, budget)` | Queued frames stepped at most `budget` ticks per call (spec 120 § 7.4). The budget is a tick count, not a time, so the result never depends on the machine; the host picks how many calls a frame makes. `push` refuses a gap (`tick_gap`). |
| `DesyncReplay`, `ReplayHook<S, N>`, `DesyncReplayRegistry` | The object-safe, bytes-in hook a desync tool registers per sim; the registry finds it by the tag in the log (sorted `Vec`, no hash map). |
| `golden::InputLogScenario<S, N>` | A recorded run as a golden scenario: `reset` restores the snapshot, `step(t)` replays log tick `start + t` through `advance`, `hash_state` feeds the world hash and each system's word under its name. The engine's `input_log_arena` (the AI arena from the committed `goldens/input_log_arena.detlog`) is one of `DetRegistry::with_engine_scenarios()`, so the det-golden runners check it on every leg. |

The log is a det-state frame of kind 2 (`InputLog`), tag `inputlog`, version `INPUT_LOG_VERSION` (1),
whose tick is the start tick. Body:

| Field | Encoding |
|---|---|
| Sim tag, sim version, system count | 8 bytes, `u32`, `u32` (at most 64 systems) |
| Start snapshot | Length-prefixed bytes, empty for none; if present it must be a `State` frame of the sim taken at the start tick |
| Entries | A count, then per entry: the seat count (at most `MAX_SEATS_PER_TICK`, 4096), per seat `u16` seat, a `0`/`1` presence byte and the input in the det-state codec; a flags byte (bit 0 world hash, bit 1 system words, other bits zero); the world hash `u64`; the system words |

Decoding is bounded like every det-state read: lengths are checked against `DetReadLimits` and the bytes
left before anything is sized by them, and nothing panics on input bytes.

**Seat order.** Steps walk `inputs.seats()`; the order is checked on every path in. Spec 120 § 7.6 notes
an offline replay that walked a hash map and disagreed with the match it replayed: never collect inputs
into one.

**Hashes.** A log may keep both hashes every tick, only every `k`-th tick, or only the world hash. A
replay compares what is there: the first diverging tick is the first recorded tick that differs, and the
system is named only where system words were recorded.

### Error codes (replay)

The det-state codes above pass through unchanged (`truncated`, `length_limit`, `bad_version`, `bad_tag`,
`trailing_bytes`, `bad_value`, `bad_hash`). `bad_value` also covers a presence byte other than 0 or 1,
unknown hash flag bits and more than 64 systems.

| Code | Meaning |
|---|---|
| `wrong_sim` | The log was recorded for another sim tag or version. |
| `system_count` | The log's system count (or an entry's system words) is not the sim's. |
| `tick_gap` | A tick is not the next one: a log entry, a catch-up frame, or a snapshot taken at another tick than the log starts. |
| `seat_order` | A seat listed twice or out of ascending order. |
| `no_snapshot` | Neither the log nor the caller gave a start snapshot. |
| `bad_until` | `until` (or a `suffix` tick) is outside the log's ticks. |
| `unknown_sim` | No hook is registered for the log's sim tag. |
| `duplicate_sim` | A hook for that sim tag is already registered. |

### Verify (replay)

- `cargo test -p regia_detsim --test replay`: `replay_of_a_recorded_run_matches_every_tick` (the AI arena
  and a 2,000-tick crowd soak with ids, the grid broadphase and seats joining and leaving; from the start
  and from a mid-run checkpoint), `a_flipped_input_is_named_at_its_tick_and_system`,
  `catch_up_reaches_the_live_tick_in_bounded_calls`, `input_log_scenario_is_a_golden`,
  `input_log_reader_refuses_bad_input` (every code, and 20,000 seeded mutations that never panic and stay
  under 64 bytes per input byte plus 64 KiB, under a counting allocator).
- `cargo test -p regia_detsim --test det_golden` checks `input_log_arena` with the other engine scenarios;
  `cargo xtask det-golden run --legs host,qemu` (and the device legs) compares it across targets.
- `REGIA_BLESS=1 cargo test -p regia_detsim --test replay input_log_scenario_is_a_golden` rewrites
  `goldens/input_log_arena.detlog` from its recipe (`golden::scenarios::record_input_log_arena`); run it a
  second time to re-bless `input_log_arena.detgold`. Only when the change in bits is intended.
- The fuzz target `det_input_log` (`cargo test -p regia_agent --test fuzz_targets`).
