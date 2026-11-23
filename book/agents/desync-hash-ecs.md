# Lockstep desync hash for ECS games

A lockstep game whose simulation lives in components and resources gets the per-system desync
diagnosis of [Lockstep desync hash](desync-hash.md) without writing a fold by hand.
`regia_framework::desync_hash` hashes, after every step, the `serde` components and resources each
named system owns, **in entity-id order**. The agent method `sim.desync_rows` reads the recorded
rows and, given another run's or peer's rows, names every system that split and the first step it
did (ADR-8080).

## Name the systems and what they own

Name your desync systems once, usually one per schedule set of your lockstep step. Then say which
components and resources each one owns. The record system runs after the sets you list.

<!-- ignore-reason: a sketch of a game's plugin; its components and sets come from the game -->
```rust,ignore
use regia_framework::desync_hash::{DesyncHashPlugin, Systems};

const SYSTEMS: Systems<3> = Systems::new(["movement", "combat", "economy"]);

app.add_plugins(
    DesyncHashPlugin::new(SYSTEMS)
        .component::<Position>("movement")   // any Component + serde::Serialize
        .component::<Velocity>("movement")
        .component::<Health>("combat")
        .resource::<Stockpiles>("economy")   // any Resource + serde::Serialize
        .after(StepSet::Movement)            // record after the game's step sets
        .after(StepSet::Combat)
        .after(StepSet::Economy),
);
```

- **Where it runs.** The record system runs in `FixedUpdate` by default, in `DesyncHashSet`. Use
  `.in_schedule(label)` for another schedule. A host that steps its lockstep schedule by hand can
  call `record_desync_hashes::<N>(world)` after each step instead.
- **Step labels.** By default a row's label is the count of recorded steps, starting at 1. Use
  `.tick_from(|world| ...)` to label rows with the game's own lockstep step number, which is the same
  on every peer. A label at or below the newest row is a rewind: it replaces the rows from there on.
- **Setup errors.** A name that is not in the list panics when the plugin is built, so the game finds
  it the first time it runs. A system with no component or resource logs a warning, because its word
  never changes.

## What a word covers

Each system's word folds its columns in the order you added them:

- **A component column:** every entity that holds the component, sorted by its order key. For each
  entity the column folds the key, then the component's value, through `regia_detsim::hash::Mix`.
  After the last entity it folds the row count.
- **A resource column:** whether the resource exists, and its value if it does.

Floats fold by the canonical float rule: `-0.0` folds as `+0.0`, and every NaN as one quiet NaN.

**Entity-id order, never archetype order.** Bevy queries visit entities table by table. Adding and
removing an unrelated marker moves an entity to the end of its table, and spawning into archetypes in
another order changes which table comes first. None of this moves a word. Only values, and which
entity holds them, move a word: swapping two entities' positions is a split.

**Order key.** The default key is the entity id, `(index << 32) | generation` (`entity_key`). That is
the same on two peers only if they spawn and despawn the same entities in the same order, and spawn
nothing else into that world. If your world also holds presentation entities (cameras, UI, effects)
that peers spawn differently, give each lockstep entity an id component and order by it:

<!-- ignore-reason: a sketch; `NetId` is the game's own id component -->
```rust,ignore
impl DesyncKey for NetId {
    fn desync_key(&self) -> u64 { u64::from(self.0) }
}

DesyncHashPlugin::new(SYSTEMS).component::<Position>("movement").order_by::<NetId>()
```

With `order_by`, entities without the id are left out, and the key folds instead of the entity id.

A game whose lockstep entities have no id component calls `order_by_content()` instead (ADR-10062):
each row is folded by its own value word and the column folds the sorted words, so entity ids do not
matter. A changed, added, removed or duplicated value still splits, and a failing `Serialize` still
marks the row partial. It cannot see two entities swapping their whole values; use `order_by` when
that matters. The default stays the entity id.

Fold only values with a stable order: `Vec`, arrays, `BTreeMap` and `BTreeSet`, never a `HashMap`.
After the scratch buffer has grown to the largest column, recording a row allocates nothing.

## `sim.desync_rows`

`sim.desync_rows { to?, count?, against?, split?, left?, right? }` is read-only.

| Param | Meaning |
|---|---|
| `to` | Newest step label of the window. Default: the newest recorded step. |
| `count` | Rows in the window, 1 to 96. Default 96. |
| `against` | Rows to compare with, at most 96. Each row is `{ tick, words: [16-digit hex], raw?: [16-digit hex], partial?: bool }` (the shape this method answers), or a `lockstep systems row <peer> <tick> <w:w:...>` log line. |
| `split` | The step label the log lines name as the split. Default: `to`, else the newest step. |
| `left`, `right` | Labels of this run and of the `against` run in the log lines. Default `self` and `other`; 1-64 printable ASCII characters, no spaces. |

The answer:

```text
{ systems: ["movement", "economy"], newest: 6, recorded: 6, source: "ecs", raw: false, partial_rows: 0,
  rows: [{ tick: 1, words: ["1b1878bedf70f221", "cfc052c02278c514"] }, ...],
  report: { same: false, verdict: "canonical", root: "economy", first_tick: 4, compared: 6, last_agreed: 3,
            systems: [{ system: "economy", first_tick: 4 }], list: "economy@4",
            raw_list: null, float_bits: [], partial_from: null, unmatched: [] },
  lines: ["lockstep desync systems tick 6 b vs a: first economy at tick 4 (economy@4)",
          "lockstep desync verdict split 6 left b right a root economy first 4 compared 6 last_agreed 3 systems economy@4"] }
```

`report` and `lines` appear only with `against`. Rows that only one side has are skipped, not
compared, and listed in `report.unmatched` (below). `last_agreed` is the latest step before the earliest split on which every system agreed;
a partial step (below) never counts, so `last_agreed` stays below `report.partial_from`.

`report.same` is `true` only when the two sides agree in every view (ADR-8621): at least one step was
compared, no compared row was partial, and neither the canonical nor the raw words split.
`report.verdict` says which view answered:

| `verdict` | Meaning | First line of `lines` |
|---|---|---|
| `same` | Every compared step agrees. | `... no system differs in the <n> shared ticks` |
| `canonical` | The canonical words split (`report.root`, `report.first_tick`). | `... first <system> at tick <T> (...)` |
| `float_bits` | The canonical words agree, the raw words split (`report.float_bits`). | `... raw words split first <system> at tick <T> (...)` |
| `partial` | The words agree, but a compared row was partial (`report.partial_from`): the state after the failing `Serialize` was never hashed. | `... not agreed: partial rows from tick <T> (...)` |
| `no_overlap` | The two sides share no step (`compared: 0`), for example a window that scrolled out. | `... not agreed: no shared tick (0 compared)` |

The fields added by ADR-8547:

| Field | Meaning |
|---|---|
| `source` | `ecs` (the app's `DesyncHashPlugin`) or `sim_host` (an external sim behind the threaded sim host, used when the app has no ECS desync hash). |
| `raw` | The ring keeps raw-bits words (`DesyncHashPlugin::diagnose_raw`, or `ThreadedSimHost::start_with_desync(.., true)`). Each row then carries `raw: [hex]`. |
| `partial_rows` | Rows recorded with a failing `Serialize` since the ring was made. Such a row carries `partial: true`. |
| `rows[].raw`, `rows[].partial` | Present only when set. Pass the rows back as `against` unchanged. |
| `report.raw_list` | The split list over the raw words (`null` without raw words on both sides). |
| `report.float_bits` | `[{ system, raw_tick, canonical_tick }]`: systems whose raw words split before the canonical ones (a zero sign or NaN payload; `canonical_tick` is `null` when the canonical words never split). `lines` gets one `lockstep desync raw ...` line per entry. |
| `report.partial_from` | The earliest compared step with a partial row on either side, or `null`. `lines` then ends with a `lockstep desync partial from tick ...` line. |
| `report.unmatched` | `[{ side, tick, index, tick_shared }]` (ADR-8724): rows without a partner on the other side, by tick, then `left` before `right`, then position. `side` `left` is this run's `rows`, `right` the `against` rows; `index` is the row's position there. `tick_shared` is `false` when the other side has no row at that step (the row was not compared). `against` never repeats a step (`bad_against`), so here `tick_shared` is always `false`. |

### Raw diagnose ring

Add `.diagnose_raw()` to the plugin to keep the raw-bits vector beside the canonical one. Two runs
whose state differs only in the sign of a zero agree on `report.list` (the wire rule) and name the
tick the sign split in `report.float_bits`, before a later `1/x` turns it into a canonical split.
The canonical words, and so `rows[].words`, do not change.

### External sims

A sim that is not an ECS world (a ported game sim behind `ThreadedSimHost`) implements
`regia_detsim::hash::SystemFold` and starts with
`ThreadedSimHost::start_with_desync::<N>(sim, settings, raw)`. Insert the host's `handle()` as a
resource (the same handle `sim.host_status`, `sim.host_hold` and `sim.host_step` read). The host
thread records the boot state and a row after every step, before it publishes the frame, so after
`sim.host_step { ticks: 8 }` and the publish of tick 8, `sim.desync_rows {}` returns rows `0..=8` with
`source: "sim_host"`. An app with both an ECS desync hash and a recording host answers from the ECS
hash.

A host that restores also records rows: `ThreadedSimHost::start_with_snapshots_and_desync::<N>` or
`ThreadedSimHost::start_lockstep_with_desync::<N>` (same arguments). A restore to step `T` (`sim.host_restore`)
clears the ring and records the restored state under `T`, so `rows` start at `T` again and no row of
the timeline the sim left is compared (ADR-8621). `recorded` keeps counting.

## Find which system diverged

1. Run both runs (or both peers) to the same step with the same inputs.
2. In run A, call `sim.desync_rows {}` and keep its `rows`.
3. In run B, call `sim.desync_rows { against: <A's rows>, left: "b", right: "a" }`.
4. If `report.same` is true, the runs agree on every step both recorded, in every view. If not,
   read `report.verdict`: on `canonical`, `report.root` names the first system to split and
   `report.first_tick` the step; on `float_bits`, `report.float_bits[0]` names the system and step
   where a zero sign or NaN payload split; on `partial`, fix the failing `Serialize` (from
   `report.partial_from` on the words do not cover the whole state); on `no_overlap`, ask for a window
   both runs still hold (`to`). Inspect the named system's components at that step: `sim.state_diff`
   names the differing components when both runs reflect them.

From a log, pass the `lockstep systems row` lines of the other peer as `against` directly.

## Errors

| Code | When |
|---|---|
| `no_desync_hash` (`F0003`) | The app has no `DesyncHashPlugin` and no `SimHostHandle` of a host started with `start_with_desync`. |
| `bad_to` | `to` is not an integer >= 0. |
| `bad_count` | `count` is not an integer from 1 to 96. |
| `bad_split` | `split` is not an integer >= 0. |
| `bad_label` | `left` or `right` is empty, longer than 64 bytes, or holds a space or a non-ASCII character. |
| `bad_against` | `against` is not an array of at most 96 rows, a row is neither shape, or two rows have the same `tick` (ADR-8621). |
| `word_count` | An `against` row holds a different number of words (or `raw` words) than this app hashes systems. |

The codes appear in brackets in the error message, for example
`sim.desync_rows: ... [bad_count]`.

## Tests

- `cargo test -p regia_framework --lib desync_hash`:
  - `ecs_set_hash_ignores_archetype_order`: the same entities visited in another archetype or table
    order hash the same.
  - `a_change_moves_only_its_system`, `swapped_values_and_new_entities_are_seen`.
  - `order_by_key_ignores_presentation_entities`.
  - `content_order_ignores_entity_ids`, `content_order_keeps_the_partial_flag`.
  - `the_plugin_records_after_the_step_in_fixed_update`.
  - `two_runs_name_the_split_system_and_tick`.
  - `ecs_words_are_pinned`: integer-only state, so the float rule cannot move it.
- `cargo test -p regia_agent --lib desync_rows`: `desync_rows_dispatch_names_the_split_system`, an
  end-to-end test through the dispatcher, plus the error codes;
  `sim_desync_rows_answers_for_an_external_sim` (two manual sim hosts stepped with `sim.host_step`,
  the raw ring naming a zero-sign split at its tick) and
  `against_rows_carry_raw_words_and_partial_flags`; ADR-8621: `same_is_false_when_only_the_zero_sign_differs`,
  `same_is_false_when_no_tick_was_compared`, `same_is_false_and_last_agreed_stops_before_a_partial_row`.
- `cargo test -p regia_framework --lib sim_host`: `a_restore_restarts_the_desync_ring_at_the_restored_tick`,
  `a_lockstep_restore_restarts_the_desync_ring`.
- `cargo test -p regia_framework --lib desync_hash`: `the_ecs_diagnose_ring_names_a_zero_sign_split`,
  `a_failing_resource_serialize_marks_the_row_partial`,
  `a_desync_feed_reads_back_through_its_type_erased_reader`.
