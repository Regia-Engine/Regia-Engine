# Deterministic perception and influence

The [deterministic AI core](det-ai-core.md) decides; this page is what it knows. `regia_detsim::ai`
(re-exported as `regia_ai::det`) has a team-shared sighting board, a coarse threat and control grid,
and the line-of-sight seam both of them ask (ADR-8395). Everything is lockstep code: the same inputs
give the same bits on every peer, and `cargo xtask det-scan` checks it.

The board and the grid have the same bits as Direct Order 27's sim (copied under ADR-8012), so a
game moving its AI onto Regia keeps its checksums.

## Line of sight

Perception never tests geometry itself. It asks a `LineOfSight`:

```rust,wrap
use regia_detsim::ai::{LineOfSight, MouthWall, OpenField};

// A cliff face across z = 0 with a 20 m wide, 12 m high tunnel mouth in it.
let mouth = MouthWall::new(0.0, [-10.0, 10.0], [0.0, 12.0]);
let eye = [0.0, 2.0, -50.0];
assert!(mouth.clear(eye, [0.0, 2.0, 60.0])); // through the mouth
assert!(!mouth.clear(eye, [40.0, 2.0, 60.0])); // through the rock
assert!(!mouth.closed().clear(eye, [0.0, 2.0, 60.0])); // a collapsed mouth
assert!(OpenField.clear(eye, [40.0, 2.0, 60.0]));
assert!(None::<&MouthWall>.clear(eye, [40.0, 2.0, 60.0])); // no geometry is open field
```

| Implementation | What blocks |
|---|---|
| `OpenField` | Nothing. |
| `HeightfieldLos(height_at)` (a closure from `x, z` to ground height) | The ground, sampled every `LOS_STEP_M` (4 m) along the line, when it rises more than `LOS_CLEARANCE_M` (2 m) over the line. A line shorter than one step is always clear. |
| `regia_world_geom::compat::sampler::TerrainSampler` | The same heightfield rule over the sampler's ground. It cannot see into a tunnel: the ground over a covered gallery blocks. |
| `regia_world_geom::compat::mesh::TunnelCollisionMesh` | Rock: a wall or lid triangle between the two points. A line down a ramp, through an open mouth or along a tube is clear. |
| `LosFn(query)` (a closure from two points to `bool`) | Whatever your closure says, for your own query (for example the heightfield and the tunnel mesh together). |
| `MouthWall` | A test double: one cliff face with a rectangular mouth that can be closed. |

`&L` and `Option<L>` are lines of sight too (`None` is open field), so a sim that keeps its terrain
as `Option<&Terrain>` passes it unchanged. Your own implementation must be a pure function of sim
state and the two points, using `regia_detsim::math` and `regia_detsim::float` only, and should not
allocate: the board asks it once per observer and enemy pair it re-checks.

The native world query (plan row W-11: one ground with the mouths cut, lids and blockers, no
allocation) will implement the same trait; switching to it changes one argument.

## The sighting board

<!-- ignore-reason: a sketch of a host's sim tick; `observers`, `enemies_by_id` and `terrain` come from the game -->
```rust,ignore
use regia_detsim::ai::{PerceptionParams, SightingsBoard, RADAR_RANGE_M};

let mut board = SightingsBoard::new(team_count, RADAR_RANGE_M);
let params = vec![PerceptionParams::default(); team_count];

// Each sim tick, before the AI decides:
board.refresh(Some(&terrain), tick, &observers, &enemies_by_id, &params);
for known in board.for_team(my_team) {
    // known.entity, known.sighting.{last_pos, confidence, source}, known.threat.{dps, can_reach_me}
}
```

Per observing team and per enemy (enemies sorted by entity id):

- **Direct sight.** An ally whose re-check phase is due (`tick % n == id % n`, `n` =
  `los_recheck_interval_ticks`) sees the enemy when it is within the ally's `sight_range_m` (in plan)
  and the line is clear. The source is `SightSource::DirectLos`.
- **Radar.** Otherwise any ally within the board's `radar_range`, scaled by the ally's `radar_scale`
  (0.5 with damaged sensors), sees it; a lit enemy (`lit`: lamps, a flare, a ping) adds `lit_bonus_m`
  (150 m by default). Radar is shared with the whole team and needs no line of sight.
- **Memory.** A seen enemy's row is refreshed at confidence 1. An unseen row decays by the team's
  `confidence_decay` only on ticks where the team actually re-checked (or when the enemy has left the
  world), and is forgotten below `forget_confidence`. `first_seen_tick` survives refreshes (a fire
  gate's reaction delay).
- `reveal_lit(team, tick, contacts)` writes contacts a light or sensor pass grants;
  `visible_threat(team)` sums the dps of the rows that can reach the team.

`PerceptionParams::default()` is a medium profile: trust 60 ticks, decay 0.96, a LOS re-check every
12 ticks, forget below 0.35. A sharper profile re-checks more often and forgets more slowly; it never
sees through a wall.

The board is serde and `PartialEq`, so a snapshot resumes it and a desync hash can walk it. The
`lit_bonus_m` setting is serialized only when it is not the default.

## The influence grid

`InfluenceGrid::new(min, max, INFLUENCE_CELL_M)` covers the map in 8 m cells with two channels,
threat and control. Each tick:

1. `set_stamps(&threat, &control)`: one threat stamp per known enemy, one control stamp per ally
   (`([x, z], strength)`).
2. `stamp_one()`: one pending stamp, round-robin, over a 5 x 5 cell neighbourhood with falloff
   `1 / (1 + d²)`, capped at 1000.
3. `decay_one()`: one cell (x0.9); a grid larger than `DECAY_CYCLE_CELLS` (160²) decays enough cells
   per call that a full cycle still takes 25 600 calls.

So no tick ever pays for the whole grid. Queries: `threat_at`, `control_at`, `local_force_ratio`
(`(control - threat) / (control + threat)`, -1..1), and for debug overlays `cell_m`, `origin`,
`threat_cells` and `control_cells`. `resize_covering` keeps the grid when the coverage did not change.

Two helpers sit with it:

- `flank_bearing(los, unit, threat, threat_yaw)`: of 12 bearings around the unit (30 m out), the one
  that is in cover from the threat (the line from the threat to the probe is blocked) and off the
  threat's facing.
- `line_of_fire(origin, target, friendlies)`: true when a friendly body is inside a 10° cone toward
  the target and nearer than it (a friendly-fire gate).

## Reading it from an agent

The board and the grid belong to your sim, so the engine has no protocol method for them.
`SightingsBoard::for_team(team)` returns serde `KnownEnemy` rows in first-sighting order, and the
grid's channel slices are row-major (`z * cols + x`): expose them through your own agent method or
debug overlay.

## Tests

`cargo test -p regia_detsim --test ai_perception` runs the row's acceptance test
`los_trait_sees_through_an_open_mouth`, Direct Order 27's board tests and
`influence_and_perception_bits_match_the_pinned_hashes` (the cross-arch pins);
`cargo test -p regia_detsim --lib ai::` runs the ported grid and board unit tests;
`cargo test -p regia_world_geom --test los_adapters` runs the same rules against the real terrain
sampler and a Stage A tunnel mouth.

## Perception on the spatial index

`SightingsBoard::refresh` runs the indexed refresh (gap AI-4, ADR-8518); `refresh_with(&mut
PerceptionIndex, ..)` is the same call with reused buffers, and `refresh_linear` keeps the all-pairs
scan with the same arguments as the oracle.

Per enemy (ascending id) and observing team (ascending), the indexed refresh:

1. **Line of sight.** Before the enemy loop, each observer whose re-check phase is due queries a
   `DetSpatial` of the offered enemies with its own sight range and keeps the (enemy, observer) pairs
   the scan's own test accepts (`dist_xz(ally, enemy) <= sight_range_m`, the enemy on another team).
   Sorted by enemy, then observer, these are each enemy's in-range due allies in the scan's order:
   `los.clear(ally, enemy)` is asked in that order and the first clear line wins.
2. **Radar.** When no line was clear, queries the team's `DetSpatial` of its observers (one index
   per team, slots in observer-list order): first the enemy's own cell (a radius-0 query), then, if no
   ally there passes, the team's largest radar reach for the enemy's lit state (`detection_range(lit) *
   radar_scale`, the maximum over the team). Each stops at the first ally the scan's test accepts; radar
   only asks whether one exists, so the order does not matter.
3. **Memory.** Upserts, decays and forgets exactly as the scan does.

The index only prunes allies the exact test would reject, so the board and the line-of-sight calls
(same pairs, same order) equal the scan's bit for bit; Direct Order 27's board tests and the S-10 pins
run through it unchanged. Boards with more than `PERCEPTION_INDEX_MAX_TEAMS` (256) teams run the scan.
A warm `PerceptionIndex` allocates nothing.

`PerceptionIndexSettings`: `cell_size_m` (default 384 m, about the radar range, since each team's
index holds only that team; any positive value gives the same boards) and `max_cells` per team index
(default 4096; every build resets that many cell heads, and the cells widen when a team spreads wider,
so one far-off unit costs speed, never results).

`PerceptionIndex::stats()` (`PerceptionIndexStats`, no wall-clock reads) is the profiler surface:

| Counter | Meaning |
|---|---|
| `refreshes` | refreshes through this index |
| `linear_fallbacks` | refreshes that ran the scan (more than 256 teams) |
| `queries` | spatial queries (one per due observer, and one or two radar queries per enemy and team no line saw) |
| `candidates` | allies handed to the exact distance test |
| `los_calls` | `LineOfSight::clear` calls |
| `scan_pairs` | allies the scan's sight loop would have read; `candidates / scan_pairs` is the saving |

There is no protocol method or error code: the board belongs to the host's sim. Show the counters
as tick-profiler rows beside the refresh's `det_ai.perception_refresh` span.

### AI world views

`AiTickWorld::new(tick, &rosters, &board)` holds the per-tick team facts; `world.view(AiUnitFacts {
entity, slot, team, pos })` returns an `AiWorldView` that borrows the team's roster slice
(`allies()`), the team's board row (`known_enemies()`, `known_enemy(id)`, `visible_threat()`) and the
rosters (`enemy_slots_into(&mut out)`), and keeps only the unit's fields by value. A view is a few
words, `Copy`, and never allocates. Stage order: rosters, index build, perception (sight channel),
targets, decide, act.

Tests: `cargo test -p regia_detsim --test ai_perception_index`
(`indexed_refresh_equals_the_linear_refresh_every_tick`, `plain_refresh_is_the_indexed_refresh`,
`los_is_asked_only_for_in_range_pairs`, `refresh_work_is_bounded_by_density`,
`views_borrow_and_allocate_nothing_after_warmup`).
