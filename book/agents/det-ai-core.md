# Deterministic AI core

A lockstep match runs the AI on every peer, so every peer must commit the same decision for the
same unit on the same tick. `regia_detsim::ai` (re-exported as `regia_ai::det`) is the AI core for
that code: a seeded random stream per unit, a time-sliced schedule, and a utility scorer whose
curves use the lockstep math (ADR-8152). It depends on no ECS, GPU or platform crate, and
`cargo xtask det-scan` checks it with the rest of `regia_detsim` (see
[Lockstep math and the determinism scan](lockstep-math.md)).

The scorer, the generator and the schedule have the same bits as Direct Order 27's sim (copied
under ADR-8012), so a game moving its AI onto Regia keeps its checksums.

## The pieces

| Item | What it does |
|---|---|
| `AiRng` | xorshift64. `AiRng::unit_stream(world_seed, entity_id)` and `AiRng::tick_stream(world_seed, tick, entity_id)` seed a stream from sim state only; `next_u64`, `next_f32` (0..1), `range(min, max)` (inclusive). |
| `AiSchedule` | The default cadences as constants: a decision every `DECISION_TICKS` (10) ticks, a sighting refresh every `SIGHT_TICKS` (6). `due_to_decide(tick, id)` is true when `tick % 10 == id % 10`. |
| `AiCadence` | The same schedule as a setting: `AiCadence::new(decision_ticks, sight_ticks)`; `Default` is `AiSchedule`'s. A zero cadence counts as one. |
| `ResponseCurve`, `Consideration`, `AiOption`, `Inputs`, `score` | The Infinite-Axis Utility scorer. Each consideration maps one normalized input (0..1) through a curve: linear, quadratic, inverse quadratic, logistic, inverse logistic, plateau or band. Additive considerations average by weight; a **multiplier** consideration multiplies in, so a zero (no ammo, almost dead) vetoes the option. |
| `score_options` | Scores a catalogue for one unit: a filter (a repertoire mask), a bias per option (a difficulty), clamped to 0..1, in catalogue order. |
| `AiCore` | The decision step. Each tick it visits the units whose phase is due, in ascending entity id, calls your scorer with the unit's tick stream, and commits through a `DecisionPolicy`. |
| `ArgMax` | The built-in policy: the best score wins (the last of equal best scores), and no scores means `Hold`. A policy that holds a decision for a while plugs in through the same trait. |

`OptionId` is a closed set of twelve options (push, hold, strafe, dogfight, kite, retreat, regroup,
defend, collect, escort, flank, focus fire). Its order is the repertoire bit index, so reordering it
changes the sim.

## Running it each tick

Implement `AiAgent` for your unit type (anything with a stable entity id; `u32` already is one),
build the core once, and call `decide` once per sim tick:

<!-- ignore-reason: a sketch of a host's sim tick; `Unit`, `nearest_enemy_frac` and `CATALOGUE` come from the game -->
```rust,ignore
use regia_detsim::ai::{AiAgent, AiCadence, AiCore, ArgMax, Inputs, score_options};

impl AiAgent for Unit {
    fn entity_id(&self) -> u32 {
        self.id
    }
}

let mut ai = AiCore::new(world_seed, AiCadence::default(), ArgMax);

// In the sim step, before movement:
let report = ai.decide(tick, &units, |unit, rng, scores| {
    let inputs = Inputs {
        hull_pct: unit.health_fraction(),
        distance_to_threat_frac: nearest_enemy_frac(unit),
        // any randomness comes from `rng` only
        commander_priority: 0.4 + 0.2 * rng.next_f32(),
        ..Inputs::default()
    };
    score_options(&CATALOGUE, &inputs, |_| true, |_| 1.0, scores);
});
for unit in &units {
    if let Some(option) = ai.decision_for(unit.id) {
        // steer, aim and fire for `option`
    }
}
```

Rules that keep it deterministic:

- Pass sim state only into the scorer. Draw randomness from the `rng` argument, never from another
  generator, the clock or a thread.
- The order of `units` does not matter: the core visits due units in entity-id order. Duplicate ids
  are visited in slice order and the last one's scores win.
- Use `regia_detsim::math` for any transcendental in the scorer (the curves already do).
- A unit that leaves keeps its decision until you call `forget(id)` or `retain(..)`.
- An order that must show at once (a squad command) writes `*ai.decision_entry(id) = ...` on any
  tick; the unit's next due tick re-scores it as usual.
- `AiCore` is serde: a snapshot (a save, a late join) resumes with the same decisions. The scratch
  buffers are not saved.

## Reading it from an agent

The core belongs to your sim, so the engine has no protocol method for it. `AiCore::rows(next_tick)`
returns one serde row per unit, in entity-id order, for your own agent method or debug overlay:

```json
[{ "entity": 4, "current": "Defend", "decision_phase": 4, "next_decision_tick": 14 }]
```

`decide` returns an `AiTickReport` (`tick`, `due`, `scored_options`): counters, not timings, so a
test can bound the work per tick. With the default cadence, 60 units re-score 6 per tick.

## Checks

- `cargo test -p regia_detsim --test ai_core`: the slice per tick (`ai_schedule_slices_by_entity_id`),
  determinism over 120 ticks, the ported Direct Order 27 tests, and the bit pins
  (`ai_bits_match_the_pinned_hashes_on_every_arch`), which must hold on x86-64 and aarch64 alike.
- `cargo xtask det-scan`: no std transcendentals, hash containers, rayon or SIMD types in the core.

Perception, the influence grid, tactics with hysteresis, aim, motor and squads build on these types
(later plan rows).

## Team rosters and the id index

An AI layer that asks "who is on my team" or "who are my enemies" by filtering the whole world
for every unit pays `units x rows` reads per tick: at 900 units that is 810,000 per query. Direct
Order 27's sim fixed this without changing a bit of its results (its SIM_TICK_PERF work, re-copied
here under ADR-8484): build each team's list once per tick in world order, and look entities up
by id through an index instead of a forward scan.

| Item | What it does |
|---|---|
| `TeamRosters` | `build(&rows, team_of)` reads each row once (`team_of(row) -> Option<u8>`; `None` keeps a row, say a dead unit, off every roster). `team(t)` is team `t`'s slots, ascending. `others_into(t, &mut out)` fills `out` with every listed slot not on `t` (the enemies in a two-sided match), ascending. `teams()`, `team_of_slot(slot)`, `team_count()`, `len()`. |
| `RosterSettings` | `max_teams` (default 256: every `u8`). A row on a team at or past it fails the build. |
| `RosterStats` | Counters for the tick profiler: `builds`, `rows_read`, and the gauges `members` and `teams`. |
| `SlotIndex` | Entity id -> first slot. `build(&rows, id_of)` once per tick; `push(id)` for a row added at the end, `remove(slot)` for a `Vec::remove`. `slot_of(id)` reads the index; `find(&rows, id, id_of)` also checks that the slot still holds the id and otherwise answers with the forward scan. |
| `SlotIndexStats` | `builds`, `hits`, `scan_fallbacks` (a change you did not report; zero when every push and remove is reported). |
| `team_group_mask(team)` | The `group_mask` bit for `regia_detsim::spatial::SpatialRow`: `1 << team` for teams 0-63, 0 past that. |

Both types index **slots** (positions in your row array), the same slots `DetSpatial` uses, and
both answer exactly what the scan they replace answers:

- `rosters.team(t)` equals `(0..rows.len()).filter(|&s| team_of(&rows[s]) == Some(t))`.
- `rosters.others_into(t, out)` equals the same filter for listed rows not on `t`.
- `index.slot_of(id)` equals `rows.iter().position(|r| id_of(r) == id)`, duplicates included
  (the first one wins), as long as every push and remove since the build was reported.

So switching a layer from the scan to these types keeps its results bit for bit: the scorer
visits the same rows in the same order. The `ai_core` pins do not move
(`roster_scoring_keeps_the_pinned_decisions`).

<!-- ignore-reason: a sketch of a host's sim tick; `Unit`, `units` and `aim_at` come from the game -->
```rust,ignore
use regia_detsim::ai::{RosterSettings, SlotIndex, TeamRosters};

let mut rosters = TeamRosters::new(RosterSettings::default());
let mut index = SlotIndex::default();
let mut enemies = Vec::new();

// Once per tick, before the AI layers:
rosters.build(&units, |u| u.alive().then_some(u.team))?;
index.build(&units, |u| u.id);

for unit in &units {
    rosters.others_into(unit.team, &mut enemies); // ascending slots, no allocation once warm
    for &slot in &enemies {
        let enemy = &units[slot as usize];
        // the same distance test, in the same order, as the old filter over `units`
    }
}
if let Some(slot) = index.find(&units, target_id, |u| u.id) {
    aim_at(&units[slot]);
}
```

Rules:

- Build both at the top of the tick, after spawns and removals. A row that moves keeps its slot,
  so moving never needs a rebuild; a team change or a death does (or keep the dead on the roster
  and test liveness in the query).
- `others_into` copies the one other team's run in a two-sided match; with several other teams it
  makes one pass over the per-slot team table (no sort). It is the same for every unit of a team, so
  with many units keep one `others` buffer per team per tick and fill it once.
- Report removes with `index.remove(slot)` in the same order you remove rows. A swap-remove is a
  `remove` of the last slot plus a change of the swapped row's slot: rebuild instead.

### Error codes

| Code | Meaning |
|---|---|
| `roster_team_out_of_range` | `TeamRosters::build` met a row whose team is at or past `RosterSettings::max_teams` (`RosterError::TeamOutOfRange { slot, team, max_teams }`). The rosters are left empty. |

### Reading the counters

The rosters belong to your sim, so the engine has no protocol method for them. `rosters.stats()`
and `index.stats()` are plain counters: show them beside the tick profiler's per-system rows (one
`rows_read` per row per tick is the expected cost; a growing `scan_fallbacks` means a remove was not
reported). The builds run inside `tracing` trace spans `regia_detsim::ai::rosters_build` and
`regia_detsim::ai::slot_index_build`.

### Checks

`cargo test -p regia_detsim --test ai_core`: `rosters_match_a_linear_scan_every_tick` (900 seeded
bodies, 4 teams, moving, dying and replaced, every tick), `rosters_keep_the_tick_chain_bit_for_bit`
(a 300 x 240 and a 900 x 90 tick hash chain, scan and rosters equal and pinned),
`rosters_read_each_row_once_per_tick_instead_of_once_per_unit` (counters),
`roster_scoring_keeps_the_pinned_decisions`, `id_index_matches_a_scan_after_a_middle_remove`,
`id_index_keeps_the_first_of_duplicate_ids` and
`a_team_past_the_setting_is_refused_with_a_stable_code`.

## Target selection

A game that scores every known enemy for every unit every tick pays `units x enemies` score calls:
405,000 per tick at 900 units on two teams. That loop has no exact prune (an enemy out of range still
competes on value, and a tie goes to whoever came first in the list), so the cheaper rule is a
different rule. `regia_detsim::ai::targets` (audit gap AI-2) runs both behind a version number: the
game keeps its score closure, the engine owns candidates, cadence and ties.

| Item | What it does |
|---|---|
| `TargetRule::Exhaustive` | Rules version 1. Every unit scores every entry of its known-enemy list, in list order, every tick; `TargetHost::replaces` breaks ties (default: first of equal best, never NaN). Bit for bit the plain loop. |
| `TargetRule::Ringed { rings_m, max_candidates, repick_ticks }` | Rules version 2. Candidates: at most `max_candidates` eligible enemies of the innermost ring that holds any, lowest slots first, from `DetSpatial::within` plus an exact center-distance test (`dx * dx + dz * dz <= r * r`). A unit re-picks on its phase (`tick % repick_ticks == entity_id % repick_ticks`) and keeps its target in between, unless the target turns invalid (not eligible, off the enemy mask or floor, or beyond the outermost ring), which forces a re-pick on that tick. Ties: highest score, then lowest slot. `TargetRule::ringed_default()` is rings 80/120/250 m, 8 candidates, every 4 ticks. |
| `AiRules { version, target }` | What a match runs. `AiRules::default()` is version 1 `Exhaustive`; `AiRules::for_version(v)` the default rules of a version; `validate()`; `check_peer(peer_version)` for a handshake. |
| `TargetSelector` | `new(rules)` (validates), `pick(tick, unit, &mut current, known, &mut spatial, &mut host)` for one unit, `pick_tick(tick, &units, &mut targets, known_of, &mut spatial, &mut host)` for a tick (trace span `regia_detsim::ai::pick_targets`), `due(tick, id)`, `report()`, `take_report()`, `rules()`. No per-unit state: `current` is a slot the host keeps in its own sim state. Allocates nothing once warm. |
| `TargetHost` | The game's side: `score(unit, candidate) -> f32` (higher wins; NaN never wins), `eligible(unit, candidate) -> bool` (alive, known; default true), `replaces(challenger, best)` (`Exhaustive`'s reduction: its tie rule, and any minimum score the old loop required before it took a first target, as in `None => s > 0.0`). Keep them pure: `Ringed` calls them for fewer candidates. |
| `TargetUnit { slot, entity_id, enemy_mask }` | One unit's input; `enemy_mask` is the `team_group_mask` bits of the teams it may target. |
| `TargetPick { target, kind }`, `PickKind` | `Exhaustive`, `Scheduled`, `Forced`, `Kept` or `Idle` (no target, not due). |
| `best_target(pairs)` | `Ringed`'s reduction alone: highest score, then lowest slot, NaN skipped, any input order. |

### The version bump

The rules version is the AI rules version, not the game's sim version; a game maps one onto the
other. Moving from `Exhaustive` to `Ringed` is one bump:

1. Adopt the module with `AiRules::default()` (`Exhaustive`). The picks, and every pinned checksum,
   stay the same; override `TargetHost::replaces` if your loop breaks ties differently from `>`.
2. In one change, bump the game's sim version, switch to `AiRules::for_version(2)` (or your own
   `Ringed` settings with `version: 2`), and re-pin the server-step chains once.

A peer whose build knows only older rules cannot run the match: `rules.check_peer(peer)` returns
`ai_rules_peer_too_old`, which the host maps to its own handshake refusal (this crate has no net code).
A rules file that names `Ringed` under version 1 fails `validate` with `ai_rules_version_too_old`.

### Error codes

| Code | Meaning |
|---|---|
| `ai_rules_unknown_version` | The version is 0 or newer than this build knows (`AiRulesError::UnknownVersion { version }`). |
| `ai_rules_version_too_old` | The rules name a rule their version may not run, such as `Ringed` under version 1 (`VersionTooOld { version, needs }`). |
| `ai_rules_peer_too_old` | A peer knows rules only up to an older version than the match runs (`PeerTooOld { peer, needs }`). |
| `ai_rules_bad_rings` | No rings, more than `MAX_TARGET_RINGS` (8), or a radius that is not finite, not positive or not above the previous one. |
| `ai_rules_bad_max_candidates` | `max_candidates` is 0. |
| `ai_rules_bad_repick_ticks` | `repick_ticks` is 0. |

### Reading the counters

`TargetPickReport` (serde) counts, per call or since the last `take_report`: `units`, `picks` (scored a
list), `scores_evaluated` (`TargetHost::score` calls), `candidates_seen` (known-list entries, or rows the
spatial query delivered before the exact tests), `forced_repicks`, `kept_targets`, and the last `tick`.
Under `Ringed`, `scores_evaluated <= (due units + forced_repicks) x max_candidates` every tick. The
selector belongs to your sim, so there is no protocol method yet: show the report beside the tick
profiler's rows. The planned `sim.ai_rows` agent rows (ai-nav-05) will carry it; until that lands it is
a host-side counter only.

### Checks

`cargo test -p regia_detsim --test ai_targets`: `exhaustive_rule_equals_a_brute_force_scan` (900 seeded
units, 2 and 4 teams, 600 ticks, moving, forgetting and dying; both tie rules),
`ringed_rule_scores_at_most_n_k_over_r_per_tick` (counters), `ringed_rule_prefers_an_in_range_target`
(a brute-force ring oracle), `an_invalid_target_is_repicked_on_the_same_tick` (kill, forget, leave the
outer ring), `ties_go_to_the_lowest_slot_whatever_the_list_order`, `older_version_refuses_ringed_rules`,
`rules_round_trip_serde`, `targets_bits_match_the_pinned_hash` (every pick of two seeded `Ringed` runs)
and `a_warm_selector_allocates_nothing`. Cost: `cargo run --release -p regia_detsim --example
ai_targets_cost`.

## Cadence channels, tiers and caps

`AiCadence` is a list of named channels plus importance tiers (audit gap AI-3, ADR-8560). `decide` and
`sight` are always channels 0 and 1 (`ChannelId::DECIDE`, `ChannelId::SIGHT`); a game adds its own, such
as `target`, `route` or `squad`, each with its own period, phase rule and cap. The default
(`AiCadence::default()`, or the legacy `AiCadence::new(decision_ticks, sight_ticks)`) is DO27's schedule
bit for bit: decide every 10 ticks, sight every 6, id-modulo phase, no caps, no tiers.

| Item | What it is |
|------|------------|
| `CadenceChannel { name, ticks, phase, max_per_tick }` | One channel. `CadenceChannel::new(name, ticks)`, `.with_phase(rule)`, `.with_max_per_tick(Some(n))`. `ticks` 0 and a cap of 0 are stored as 1. |
| `PhaseRule::IdModulo` | `base = id % ticks`, `sub = id / ticks` (the default; DO27's rule). Even for contiguous ids; ids that share a factor with the period land on one tick. |
| `PhaseRule::Hashed { salt }` | Fibonacci hashing: `w = id * 0x9E3779B97F4A7C15 + AiRng::mix(salt, 0)`, `base = (w * ticks) >> 64`. Spreads clustered ids (multiples of the period, other strides) close to evenly; give each channel its own salt. |
| `CadenceTier { multiplier }` | A tier multiplies every channel's period for the units in it. `AiCadence::with_tiers(vec![..])`. A tier index past the list uses the last tier; no tiers means 1x. |
| `AiAgent::cadence_tier(&self) -> u8` | The unit's tier this tick (default 0). Derive it from sim state every peer shares (distance to the nearest enemy, squad role), never from a camera. |
| `AiCadence::add_channel`, `with_channel`, `channel_id(name)`, `channel(id)`, `channels()`, `set_ticks`, `set_phase`, `set_max_per_tick`, `set_tiers`, `tiers()`, `multiplier(tier)` | Settings. |
| `AiCadence::due(ch, tick, id)`, `due_in_tier(ch, tier, tick, id)`, `next_tick(ch, tick, id)`, `next_tick_in_tier(..)`, `period(ch, tier)`, `phase(ch, tier, id)` | The schedule as pure functions. `due_to_decide`, `due_to_sight`, `next_decision_tick`, `decision_phase`, `sight_phase` stay, as wrappers over the built-in channels at tier 0. |
| `AiCore::due_units(ch, tick, &units, &mut out) -> Result<CadenceReport, CadenceError>` | The slice indices of the units that run on `ch` this tick, ascending entity id, with the cap applied. No allocation once warm. |
| `AiCore::decide` | Runs the `decide` channel's list (its phase rule, tiers and cap). |
| `AiCore::queued(ch)`, `AiCore::cadence_report(ch)` | The units waiting under a cap (`QueuedUnit { entity, since }`), and the last call's counters. |
| `AiCadence::max_queue_wait(ch, units)`, `max_gap(ch, units)` | The proven bounds (below). |

**Phase rule.** In a channel of `ticks = P` a unit of tier multiplier `m` is due when
`tick % (P * m) == base + P * (sub % m)`. Every think tick of a unit is on its base lattice
(`tick % P == base`) in every tier, so a tier change never moves its base phase; after a change at tick `t`
the unit next thinks at `next_tick_in_tier(ch, new_tier, t, id)`, at most `P * m - 1` ticks later. When one
multiplier divides another (1x, 2x, 4x), the slower tier's ticks are a subset of the faster tier's.

**Caps.** A channel with `max_per_tick: Some(c)` runs at most `c` units a tick. The others wait in the
channel's queue and go first on later ticks: longest wait first, then lowest entity id. A waiting unit that
falls due again keeps its older place (one run serves both). The queue is sim state and is kept in
`AiCore` snapshots. Pass every unit of the channel each tick: a unit missing from a call loses its place.
`AiCore::forget` and `retain` also clear queue places. Ask for each channel's list once per tick (`decide` already takes the `decide` channel's list, so
do not also call `due_units(ChannelId::DECIDE, ..)` on a tick you call `decide`). With `N` units taking part each tick:

- no unit waits in the queue longer than `ceil(N / c) - 1` ticks (`max_queue_wait`);
- no gap between two runs of a unit is longer than `P * lcm(tier multipliers) + ceil(N / c) - 1`
  (`max_gap`); for a unit that keeps one tier it is `P * m + ceil(N / c) - 1`.

### Error codes

| Code | Meaning |
|------|---------|
| `ai_cadence_empty_name` | A channel name is empty. |
| `ai_cadence_duplicate_name` | Two channels share a name (`DuplicateName { name }`). |
| `ai_cadence_builtin_channels` | A serialized channel list does not start with `decide` and `sight`. |
| `ai_cadence_too_many_channels` | More than `MAX_CADENCE_CHANNELS` (64) channels. |
| `ai_cadence_too_many_tiers` | More than `MAX_CADENCE_TIERS` (16) tiers. |
| `ai_cadence_unknown_channel` | The `ChannelId` is not in this cadence (`UnknownChannel { index }`). |

Reading settings text runs the same checks; the error message starts with the code.

### Reading the counters

`CadenceReport` (serde) per channel call: `channel`, `tick`, `due` (units whose phase fell on the tick,
waiting ones included), `ran`, `carried` (left waiting), `max_wait` (the longest wait of a unit that ran).
`AiTickReport::due` from `decide` counts the units that ran. There is no protocol method yet: the planned
`sim.ai_rows` agent rows (ai-nav-05) will carry `CadenceReport` and `queued`; until then show them beside
the tick profiler's rows from your host.

Serialized settings keep the legacy fields: `{"decision_ticks":10,"sight_ticks":6}` still reads as the
default, and new text writes them next to `channels` and `tiers` (when `channels` is present it wins).

### Checks

`cargo test -p regia_detsim --test ai_cadence`: `hashed_phase_spreads_clustered_ids` (ids 0, 10, ..., 990:
one tick under id modulo, at most 12 per tick hashed, for 256 salts), `cap_carries_over_in_wait_then_id_order`,
`no_unit_waits_longer_than_the_bound` (900 seeded units, tiers 1x/2x/4x re-drawn every 150 ticks, 3,600
ticks, a cap that fits the load and one that does not, both phase rules), `tier_change_keeps_the_phase_rule`,
`channels_are_independent`, `cadence_settings_round_trip_and_validate`, `a_snapshot_keeps_the_queues`,
`cadence_bits_match_the_pinned_hash` (every due list of a seeded hashed, tiered and capped run);
`cargo test -p regia_detsim --test ai_cadence_alloc` (`warm_due_lists_allocate_nothing`); and
`cargo test -p regia_detsim --test ai_core default_cadence_is_bit_identical` (DO27's pins under the default
settings and an explicit 1x tier).

## Your own catalogue

The scorer's options and inputs are data (audit gap AI-7, ADR-8609). A `UtilityCatalogue` holds a game's
options (`OptionDef { name, considerations, min_score }`) and named inputs (`InputDef { name }`), addressed
by `OptionIdx(u16)` and `InputIdx(u16)`. A unit's inputs are a plain `&[f32]` (`InputValues`) in input
order. `score_idx` is `score` with the struct field replaced by a slice index: the same float operations
in the same order, so the same curves give the same bits. DO27's twelve options and fifteen inputs are
the preset `UtilityCatalogue::do27_preset(&thresholds)` (option index == `OptionId::bit`, input index ==
`ConsiderationId::index`), so `OptionId`, `ConsiderationId` and `Inputs` stay as typed names and nothing
that uses them changes.

```ron
(
    format_version: 1,
    name: "scavenger",
    inputs: [(name: "hunger"), (name: "threat"), (name: "food_near"), (name: "ammo")],
    options: [
        (name: "forage", considerations: [
            (input: "hunger", curve: (kind: Linear, m: 1.0)),
            (input: "food_near", curve: (kind: Plateau, k: 0.5), weight: 0.5),
            (input: "threat", curve: (kind: InverseQuadratic), multiplier: true),
        ]),
        (name: "flee", considerations: [(input: "threat", curve: (kind: Quadratic))]),
        (name: "fight", min_score: 0.1, considerations: [
            (input: "threat", curve: (kind: Band, b: 0.6, c: 0.2)),
            (input: "ammo", curve: (kind: Linear, m: 1.0), multiplier: true),
        ]),
    ],
)
```

| Item | What it is |
|------|------------|
| `UtilityCatalogue::from_ron_str(text)` | Parse and validate `.detutility.ron` (format `ai-det-utility` v1, `docs/formats/ai-det-utility.md`). No asset system, no IO: a headless sim calls it on text it read. The asset pipeline cooks the same file (importer `det_utility`) to its canonical text. |
| `to_ron()`, `to_file()`, `from_file(&file)` | Canonical text (floats round-trip bit for bit) and the serde file form. |
| `do27_preset(&t)`, `from_enum_options(name, &[AiOption])` | DO27's catalogue for a level's thresholds; any enum catalogue in slice order. |
| `options()`, `inputs()`, `option(idx)`, `option_name(idx)`, `input_name(idx)`, `option_by_name`, `input_by_name` | Lookup. Name lookup is a binary search of a table sorted at load (no hash map). |
| `score(idx, &values)`, `explain(idx, &values) -> OptionExplain` | One option's score; its considerations by input name with raw input, curve value, weight and kind. |
| `score_options_idx(&cat, &values, allowed, bias, &mut out)` | `score_options` by index: `(OptionIdx, f32)` in option order. |
| `ArgMaxIdx`, `AiCore<ArgMaxIdx, OptionIdx>` | `ArgMax` over catalogue options (last of equal best; no scores commit `None`). `DecisionPolicy<K>` and `AiCore<P, K>` take the option key, `OptionId` by default. |
| `OptionMask(u64)` | A repertoire over `OptionIdx`; `RepertoireMask` converts with the same bits (`DifficultyProfile::option_mask`). |
| `DifficultyProfile::score_options_idx`, `allows_idx` | A level's filter and bias over the preset by index. |

**The cap.** At most `MAX_CATALOGUE_OPTIONS` (64) options, so a mask is one `u64`; 256 inputs, 32
considerations per option, 64-byte names, 1 MiB of text.

**The bit-order rule.** An option's index is its position in the file and decides ties (`ArgMaxIdx` keeps
the last of equal best scores); an input's index is its position in `inputs` and in each unit's slice.
Reordering options or inputs changes the sim, even when no curve changes. Add new options and inputs at
the end, and treat any change to the file as a sim-version change (peers must load the same bytes).

`min_score` is carried as data, like `AiOption::min_score`: neither scorer applies it; a `DecisionPolicy`
may.

### Error codes

| Code | Meaning |
|------|---------|
| `det_utility_parse` | Not RON, a missing or unknown field, a wrong type. |
| `det_utility_newer_version` | `format_version` is newer than this build (`NewerVersion { found, supported }`). |
| `det_utility_unsupported_version` | `format_version` is 0 or missing. |
| `det_utility_unknown_input` | A consideration names an input the catalogue does not list (`UnknownInput { option, input }`). |
| `det_utility_duplicate_name` | Two options or two inputs share a name (`DuplicateName { kind, name }`). |
| `det_utility_bad_name` | A name is empty or over 64 bytes. |
| `det_utility_non_finite` | NaN or infinity in a floor, weight or curve parameter (`NonFinite { option, field }`). |
| `det_utility_over_cap` | Over a cap (`OverCap { what, count, max }`), or the text is over 1 MiB. |

`DetUtilityError::code()` returns the code; the cook's error message starts with it.

### Reading it from an agent

There is no new protocol method: the catalogue is host sim data. `UtilityCatalogue::explain` returns
serde rows with option and input names, the input that the planned `sim.ai_explain` (ai-nav-05) shows; a
host method can return them as they are.

### Checks

`cargo test -p regia_detsim --test ai_catalogue`: `preset_scores_equal_the_enum_scores_bit_for_bit` (10,000
seeded inputs x 12 options at each of the three levels, plus each level's filter and bias),
`the_index_core_commits_the_enum_cores_decisions`, `a_game_adds_an_option_and_an_input` (a 3-option,
4-input catalogue from RON against a hand-written reference), `bad_catalogues_are_refused_with_named_errors`,
`catalogue_ron_round_trips`, `newer_version_is_refused`; `cargo test -p regia_detsim --test ai_core`
(`pinned_decisions_hold_through_the_catalogue_index_path` and the unchanged pins); the `ai.det_utility`
fuzz target (`cargo test -p regia_agent --test fuzz_targets`); `cargo test -p regia_asset_pipeline --test
data_cook a_det_utility_catalogue_cooks_to_its_canonical_text`.
