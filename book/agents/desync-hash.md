# Lockstep desync hash

In a lockstep match every peer runs the same simulation and sends one checksum per tick. When the
checksums split, that one word says *that* the peers diverged, not *where*. `regia_detsim::hash`
keeps the "where": one hash per named system after every step, the last 240 ticks of them, and a
report that names each system that split and the first tick it did (spec 120 § 7.6, ADR-8047).

The module has no ECS, GPU or network dependency, so a game's own sim and a headless server use it
directly. The ECS adapter ([Lockstep desync hash for ECS games](desync-hash-ecs.md), with the agent method
`sim.desync_rows`) and the network probe and verdict build on it.

## Name your systems and fold them

A game lists its systems once, in a `const`, and folds each system's state into its own mixer. A bad
list (empty, more than 64 names, a name outside `a-z`, `0-9`, `_`, a duplicate) fails the build.

<!-- ignore-reason: a sketch of a game's sim; its state types come from the game -->
```rust,ignore
use regia_detsim::hash::{Mix, SystemFold, Systems};

impl SystemFold<3> for MySim {
    const SYSTEMS: Systems<3> = Systems::new(["units", "economy", "rng"]);

    fn fold_systems(&self, mix: &mut [Mix; 3]) {
        for unit in &self.units {          // a Vec in id order
            mix[0].u64(u64::from(unit.id));
            mix[0].fold(unit);             // any serde::Serialize value
        }
        mix[1].fold(&self.stockpiles);     // a BTreeMap: stable order
        mix[2].u64(self.rng_state);
    }

    // Optional: play tests and the desync gate nudge one system by index.
    fn inject(&mut self, system: usize) -> bool {
        match system {
            1 => self.stockpiles.insert(u32::MAX, 1).is_none(),
            _ => false,
        }
    }
}
```

`Mix::fold` walks the value through a hashing `serde::Serializer`: field values only, in declaration
order, with no field names, type names or field counts; no allocation, no text. A field added to a
folded struct is covered without touching the fold when it serializes a value. A unit or unit-struct
field, a `#[serde(skip)]` field and a `skip_serializing_if` field that is skipped write nothing, so
they are invisible, and because names are not mixed two structs whose written values line up fold
to the same word (two `skip_serializing_if` options, one `Some(5)` and the other skipped, collide with
the mirror image). This is Direct Order 27's encoding, kept as is (ADR-8621); the test
`the_mixer_does_not_see_unit_fields_or_skipped_option_slots` pins both collisions, so a change of the
encoding is a deliberate pin move. Fold a value, not a marker, for state that must be covered. Fold only
collections with a stable order (`Vec`, arrays, `BTreeMap`, `BTreeSet`), never a `HashMap`.

**Floats** fold by their canonical bits: `-0.0` folds as `+0.0`, and every NaN as one quiet NaN
(`canonical_f32_bits`, `canonical_f64_bits`). Two peers that agree on a value but not on the sign of
a zero or a NaN's payload do not report a split. The rule is Direct Order 27's own code, bit for bit (ADR-8078): a lockstep
game that used that rule keeps every word when it switches to this crate. Every other value, subnormals and both
infinities included, keeps its bits. The other side of that rule: a real zero-sign split (a `min`/`max` that returns
`-0.0` on one host and `+0.0` on another) is invisible to the per-system words, so the report names the first later
tick where the split moved a value, not the tick that caused it; `regia_detsim::float::det_*` keeps such splits from
happening.

## Keep the ring and find the split

<!-- ignore-reason: a sketch of a host's step loop; `sim`, `peer_rows` and `log` come from the host -->
```rust,ignore
use regia_detsim::hash::{self, SystemRing};

let mut ring = SystemRing::<3>::new();         // allocates once: 240 rows
// After every lockstep step:
ring.record(step_label, &sim);                 // no allocation
// The checksums split at `split`: send our rows, get the other peer's.
let ours = ring.window(split, 64);             // at most 96 rows
let report = hash::diverged_systems(&MySim::SYSTEMS, &ours, &peer_rows);
for line in hash::divergence_lines(split, "host", "seat2", &report) {
    log::warn!("{line}");
}
```

A row label at or below the newest one (a rewind) replaces the rows from there on. Rows only one side
has are skipped, not compared.

Every row at a tick takes part in the join (ADR-8724). A ring holds one row per tick, but rows read
from a log or a bundle may repeat a tick. Then each left row at that tick is compared with each right
row there, so a system splits at that tick unless all of its words there are one value, and the tick
is not agreement. Rows pair in input order; a row with no partner (its tick is missing on the other
side, or its side holds more rows at that tick) is listed in `report.unmatched`
(`UnmatchedRow { side, tick, index, tick_shared }`, by tick, then left before right, then position).
`hash::diagnosis_lines` adds a `lockstep desync rows ...` line when one side held more rows at a shared
tick. The result does not depend on the order of the rows within a slice.

## The log lines (the agent-readable surface)

An agent or tool reads these lines from the game log:

```text
lockstep desync systems tick 4240 host vs seat2: first economy at tick 4231 (economy@4231 production@4233)
lockstep desync verdict split 4240 left host right seat2 root economy first 4231 compared 64 last_agreed 4230 systems economy@4231,production@4233
lockstep systems row host 4231 1b1878bedf70f221:cfc052c02278c514:a1ba272ecd20c033
```

- The `verdict` line is `key value` pairs: `split`, `left`, `right`, `root` (the earliest system; ties
  go to the earlier name in the list), `first`, `compared` (shared ticks), `last_agreed` (`none` when
  the first shared tick already differs) and `systems` (`name@tick`, comma-joined).
- With no differing system, one line ends `no system differs in the <n> shared ticks`.
- `row_line` writes one ring row; `parse_row_line` reads it back (any prefix before
  `lockstep systems row` is ignored).

## Injection

`inject_divergence(&mut sim, "economy")` nudges one system through `SystemFold::inject`; it returns
`false` for an unknown name or a system the game cannot nudge. `parse_inject(&SYSTEMS, "economy@120#2")`
reads a plan (`<system>@<step>[#<seat>]`) from a host's test setting. A test asserts the injected system
is named at that tick:

<!-- ignore-reason: test sketch; `run_pair` steps two copies of the game and records a ring for each -->
```rust,ignore
let (a, b) = run_pair(seed, 18, Some(("economy", 12)));
let report = hash::diverged_systems(&MySim::SYSTEMS, &a.window(18, 64), &b.window(18, 64));
assert_eq!(report.root().map(|d| (d.system, d.first_tick)), Some(("economy", 12)));
assert_eq!(report.last_agreed, Some(11));
```

## Spec 120's `DesyncHash`

The network layer asks a game for `DesyncHash`: `world_hash(tick)` (the word sent every
`hash_every` ticks) and `subsystem_hashes(tick, out)`. A game with a `SystemFold` writes it in a few
lines; `SystemHashes::combined()` is a world hash for a game without a checksum of its own, and
`write_rows` fills `out` with no allocation once it has room for the list.

## Other pieces

| Item | What it is for |
|---|---|
| `should_sample(tick, every)` | A debug period: log a vector every `every` ticks (`None` or 0: never) |
| `SystemLogArm` | Shared by the net thread that sees a split and the sim thread that hashes: `arm(tick)` (the earliest wins, `SYSTEM_LOG_TICK_UNKNOWN` when the tick is not known), `take()` |
| `desync_report(&SYSTEMS, ...)`, `Systems::fields` | Multi-line and one-line dumps of two vectors |
| `SystemHashes<N>` | `Copy`; serializes as a struct with one `words` field (an `N`-tuple) |

## Checks

`cargo test -p regia_detsim`: the ported desync tests (`identical_runs_never_diverge`,
`every_system_injection_is_named_at_its_tick`, `isolated_injections_name_exactly_one_system`, ...),
`negative_zero_and_nan_hash_canonically`, `mix_words_are_pinned` (a change to the mix is a
sim-version change) and `tests/hash_alloc.rs` (0 allocations per fold, per full-ring record and per
`subsystem_hashes`). `cargo xtask det-scan` holds the module to the lockstep rules.

## Raw diagnose ring

The canonical float rule (the wire rule, DO27's NET_DESYNC2 bits) folds `-0.0` as `+0.0` and every
NaN as one pattern. That keeps two peers that agree on every value from reporting a false split, but
it also hides the tick where a zero's sign first differs: the canonical words split only later, when
the sign changes a value (`1.0 / -0.0` is `-inf`), so the report names a later tick than the root
cause, or never names it.

A ring built with `SystemRing::with_raw()` also keeps, per row, the vector folded under
`FloatRule::Raw` (the IEEE bits as they are). `diagnose` joins two peers' `window_rows` and reports
the canonical result plus every system whose raw words split first:

<!-- ignore-reason: a sketch of a host's diagnosis path; `sim` and `peer_rows` come from the host -->
```rust,ignore
use regia_detsim::hash::{self, SystemRing};

let mut ring = SystemRing::<3>::with_raw();    // diagnosis on: two vectors per row
ring.record(step_label, &sim);                 // folds the state twice
let ours = ring.window_rows(split, 64);        // RingRow { tick, hashes, raw, partial }
let d = hash::diagnose(&MySim::SYSTEMS, &ours, &peer_rows);
for line in hash::diagnosis_lines(split, "host", "seat2", &d) {
    log::warn!("{line}");
}
```

```text
lockstep desync systems tick 60 host vs seat2: first motion at tick 55 (motion@55)
lockstep desync verdict split 60 left host right seat2 root motion first 55 compared 60 last_agreed 54 systems motion@55
lockstep desync raw motion tick 40 host vs seat2: zero sign or NaN payload split (canonical at tick 55)
```

- The first lines are exactly `divergence_lines`' (tools that read the `verdict` line keep working).
- `Diagnosis::float_bits` lists `{ system, raw_tick, canonical_tick }` for every system whose raw
  words split before the canonical ones (`canonical_tick` is `None` when the canonical words never
  split in the compared rows: the split is a zero sign or a NaN payload only).
- `Diagnosis::raw` is the full report over the raw words (ticks where both rows carry them).
- Off by default. A raw ring holds twice the words and each `record` folds twice. The canonical rows
  (`window`, what peers exchange) are the same as a plain ring's: the wire is unchanged.
- `Mix::new_raw(tag)` and `Mix::with_rule(tag, FloatRule::Raw)` fold raw bits directly; a state with
  no `-0.0` and no NaN folds to the same words under both rules.

## Partial folds

`Mix::fold` walks a value through its `Serialize` impl. If that impl returns an error (a
`serde::ser::Error::custom` from user code), the walk stops part way and the word covers only part
of the state. `Mix::errors()` counts those failures. `system_hashes` debug-asserts that there were
none; `fold_system_hashes(state, rule)` returns `Folded { hashes, partial }` instead, and
`SystemRing::record` keeps the flag per row (`RingRow::partial`, `SystemRing::partial_rows()`) and logs
one warning on the first partial row. `diagnose` reports the earliest compared tick with a partial row
(`Diagnosis::partial_from`), and `diagnosis_lines` adds:

```text
lockstep desync partial from tick 5: a Serialize failed while folding; those words cover part of the state
```

## Mixer quality and cost

`mixer_has_no_collisions_over_seeded_single_bit_flips` checks the one-multiply mixer against the strict
avalanche criterion with fixed seeds (2 x 2^20 single-bit flips):

- No collision: two streams of the same length that differ in one word never fold to the same word
  (every step is a bijection of the state).
- A flip followed by at least one more word: every (input bit, output bit) pair changes with a
  probability within 6 standard deviations of one half.
- A flip in the last word before the finish: the top input byte reaches only part of the output bits
  (at most 128 of the 4096 pairs are outside the bound, all from input bits 56-63). Detection is not
  affected (still no collision); only the spread of the difference is narrower. The mix is DO27's
  and stays bit-identical until a Stage B ADR.

`cargo run --release -p regia_detsim --example hash_cost -- 900 2000 [raw]` times `SystemRing::record`
over 900 entity rows of a typical unit struct folded into four systems; the lane report holds the
measured numbers (they are evidence, not test assertions).

## External sims

A sim outside the ECS that implements `SystemFold` records into a `regia_framework::desync_hash::DesyncFeed`
(the fold runs outside its lock; the lock covers one row push). Behind the threaded sim host,
`ThreadedSimHost::start_with_desync::<N>(sim, settings, raw)` records the boot state and then a row
after every step, before the frame is published, so a reader that saw tick `T` published finds row
`T`. The host's `SimHostHandle::desync()` carries the reader (`SimHostDesync`), and
`sim.desync_rows` answers from it (`source: "sim_host"`) when the app has no `DesyncHashPlugin`. See
[Lockstep desync hash for ECS games](desync-hash-ecs.md) for the method.

Tests: `cargo test -p regia_detsim --lib hash` (`raw_ring_names_a_zero_sign_split_at_its_tick`,
`a_nan_payload_split_is_named_by_the_raw_ring_only`, `the_raw_ring_leaves_the_canonical_rows_alone`,
`a_failing_serialize_marks_the_hash_partial`, `mixer_has_no_collisions_over_seeded_single_bit_flips`);
`cargo test -p regia_framework --lib sim_host` (`a_folding_sim_records_a_row_per_step_before_the_publish`);
`cargo test -p regia_agent --lib desync_rows` (`sim_desync_rows_answers_for_an_external_sim`).
