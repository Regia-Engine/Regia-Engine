# Integer local avoidance

`regia_detsim::avoid` chooses an XZ velocity from integer disc snapshots. Positions
and radii are millimeters; velocities are millimeters per second. The Stage A
kernel and seven source tests come from the owner's `ir-sim/src/pathing/avoid.rs`,
commit `d6ed37677` (`regia-absorb-1`, ADR-8012 and ADR-8150).

## Host integration

Call `orca_velocity` for the compatibility defaults, or
`orca_velocity_with_settings` with an `OrcaSettings` shared by every peer.
The constants remain available for callers migrating their imports. Settings are:

| Field | Default | Meaning |
|---|---|---|
| `max_neighbors` | 8 | Closest candidates retained after sorting |
| `horizon_ms` | 2000 | Latest predicted closest approach that creates a constraint |
| `keep_right_mm_s` | 600 | Lateral head-on preference bias |
| `min_preferred_mm_s` | 500 | Preferred speed threshold for the bias only |
| `head_on_range_mm` | 25000 | Maximum distance for the head-on bias |
| `look_m` | 20.0 | Caller broadphase distance in meters; the solver does not read it |

The lateral term uses `[preferred_z, -preferred_x]`, preserving the source
simulation's keep-right convention. A host with another coordinate handedness
converts positions and velocities consistently at its boundary. Do not reverse
this term inside the compatibility kernel; doing so changes the simulation bits.

Build every disc snapshot from the same tick before solving any unit. Supply
unique stable IDs; entries with the moving unit's ID are ignored. Neighbors sort
by `(distance_squared, id)` before truncation. Use the same candidate set on every
peer, independent of its incoming order. The caller must bound candidate counts;
collecting and sorting all candidates costs O(n log n) and O(n) scratch space.
Only the projection pass is limited by `max_neighbors`. Profile a whole crowd
step in the host's tracing span.

`m_to_mm` rounds f32 meters half away from zero; feed finite, bounded values.
`mm_to_m` and `mm_s_to_m_s` are presentation helpers.

One ordered projection pass is best effort; a later plane can invalidate an
earlier one. Contact matches the neighbor's normal velocity, rather than always
forcing separation. The speed clamp uses the preference **after** the head-on
bias, with floor square roots and integer truncation; it is not a strict cap on
the original preferred speed. A preference below the threshold still projects
contact planes. These are preserved source behaviors, not new guarantees.

## Input domain

Every intermediate product fits its integer type inside this domain (ADR-8377
has the proof for each arithmetic site):

| Quantity | Domain | Constant |
|---|---|---|
| Position, each axis, every body | `-2^31 ..= 2^31` mm (about ±2,147 km) | `ORCA_MAX_POSITION_MM` |
| Velocity component of every body, and the preference | `-2^20 ..= 2^20` mm/s (about ±1,048 m/s) | `ORCA_MAX_SPEED_MM_S` |
| Radius | `0 ..= 2^16` mm (about 65.5 m) | `ORCA_MAX_RADIUS_MM` |
| `horizon_ms` | `0 ..= 2^20` ms | `ORCA_MAX_HORIZON_MS` |
| `keep_right_mm_s`, `min_preferred_mm_s` | `0 ..= 2^20` mm/s | `ORCA_MAX_SPEED_MM_S` |
| `head_on_range_mm`, indexed `look_mm` | `0 ..= 2^33` mm | `ORCA_MAX_RANGE_MM` |
| Retained neighbors, `min(max_neighbors, candidates)` | at most 256 | `ORCA_MAX_NEIGHBOR_PLANES` |

Entries with the mover's own ID are ignored and never checked. Inside the domain
the output is the pinned source's, bit for bit. The check runs before any
arithmetic, so outside the domain every build profile returns the same value:

- `try_orca_velocity`, `try_orca_velocity_into` and `try_orca_velocity_indexed`
  return `Err(OrcaDomainError)`.
- `orca_velocity`, `orca_velocity_with_settings`, `orca_velocity_into` and
  `orca_velocity_indexed` return `orca_out_of_domain_velocity(preferred)`: the
  preferred velocity clamped per axis to `±ORCA_MAX_SPEED_MM_S`.

`OrcaInput { me, preferred, others }.validate(&settings)`, `OrcaSettings::validate`
and `OrcaBody::validate` run the check alone. The first failure is reported in
this order: settings, mover, preference, neighbors in slice order, neighbor cap.

| Reason code | Meaning |
|---|---|
| `orca_position` | a body's X or Z is outside `±ORCA_MAX_POSITION_MM` |
| `orca_velocity` | a body's velocity component is outside `±ORCA_MAX_SPEED_MM_S` |
| `orca_radius` | a radius is negative or above `ORCA_MAX_RADIUS_MM` |
| `orca_preferred` | a preference component is outside `±ORCA_MAX_SPEED_MM_S` |
| `orca_horizon` | `horizon_ms` is negative or above `ORCA_MAX_HORIZON_MS` |
| `orca_keep_right` | `keep_right_mm_s` is negative or above `ORCA_MAX_SPEED_MM_S` |
| `orca_min_preferred` | `min_preferred_mm_s` is negative or above `ORCA_MAX_SPEED_MM_S` |
| `orca_head_on_range` | `head_on_range_mm` is negative or above `ORCA_MAX_RANGE_MM` |
| `orca_neighbor_cap` | more than 256 neighbors would be retained |
| `orca_look` | the indexed look distance is negative or above `ORCA_MAX_RANGE_MM` |
| `orca_slot` | the mover's slot or a gathered index slot has no body |

An out-of-domain fallback is a host bug, not a game event: count it
(`OrcaStats::out_of_domain`) and fix the input.

## Allocation-free use

Keep one `OrcaScratch` per solving thread and pass it to `orca_velocity_into`
for every mover of every tick. It owns the ordering and plane buffers; once they
reach the largest candidate count and cap seen, a call allocates nothing. The
cap is chosen by partial selection, so a call costs O(n + k log k) for n
candidates and cap k. `OrcaScratch::with_capacity(candidates, planes)` pre-sizes
the buffers. The plain functions build a fresh scratch per call and return the
same bits.

`scratch.stats()` returns cumulative counters (`OrcaStats`); `fields()` names
them for agent rows: `orca_calls`, `orca_neighbors` (retained after the cap),
`orca_planes`, `orca_projections` (planes that moved the velocity),
`orca_out_of_domain`, `orca_index_candidates`. `reset_stats()` clears them.

## Neighbors from the spatial index

`orca_velocity_indexed(&mut scratch, &mut index, &bodies, me_slot, preferred,
&settings, &query)` takes neighbors from a `regia_detsim::spatial::DetSpatial`
built this tick instead of a host-built list. Build one row per body with
`slot = i`, XZ from `bodies[i].position_m()` and a nonnegative radius; the index
may be the one the crowd's other queries share. `OrcaNeighborQuery { look_mm,
mask }` (or `OrcaNeighborQuery::from_settings(&settings, mask)`, which rounds
`look_m`) is the neighbor rule:

- a neighbor's row shares a bit with `mask` and passes the index's floor rule
  against the mover's row, and
- its center lies within the look distance exactly: `dx² + dz² <= look_mm²` in
  integers. The f32 index only prunes; the query radius includes the rows'
  position tolerance, so no body inside the look distance is missed.

The result equals `orca_velocity_into` over that look set in slot order. When at
least `max_neighbors` bodies lie inside the look distance, it also equals the
full list of every body, because every body outside is farther than every body
inside. With fewer, bodies beyond the look distance are not considered: the look
distance is part of the rule, as `ORCA_LOOK_M` already was for hosts that
prefiltered by it. Only bodies inside the look distance are checked against the
domain. An index with a row for a missing body returns `orca_slot`.

<!-- ignore-reason: regia_detsim is not a book-snippet crate; the same example compiles as the orca_velocity_indexed doctest -->
```rust,ignore
use regia_detsim::avoid::{OrcaBody, OrcaNeighborQuery, OrcaScratch, OrcaSettings, orca_velocity_indexed};
use regia_detsim::broadphase::BroadBody;
use regia_detsim::spatial::{DetSpatial, SpatialRow};

fn crowd_step(bodies: &[OrcaBody], prefs: &[[i64; 2]], out: &mut [[i64; 2]],
              index: &mut DetSpatial, rows: &mut Vec<SpatialRow>, scratch: &mut OrcaScratch) {
    let settings = OrcaSettings::default();
    let query = OrcaNeighborQuery::from_settings(&settings, u64::MAX);
    rows.clear();
    rows.extend(bodies.iter().enumerate().map(|(slot, b)| {
        let [x, z] = b.position_m();
        SpatialRow { slot, body: BroadBody::new([x, 0.0, z], 0.0, [0.0, 2.0]), group_mask: 1, flags: 0 }
    }));
    if index.build(rows).is_err() {
        return;
    }
    for (slot, v) in out.iter_mut().enumerate() {
        *v = orca_velocity_indexed(scratch, index, bodies, slot, prefs[slot], &settings, &query);
    }
}
```

Solve every mover against the same snapshot, then apply the velocities.

## Agent protocol

This pure kernel has no live ECS state. Its protocol surface is the existing
`agent.run_tests` and `agent.job_status` pair (build permission required). Point the
host's project root at the Regia workspace and request the fixed acceptance:

```json
{"jsonrpc":"2.0","id":1,"method":"agent.run_tests","params":{"package":"regia_detsim","target":"orca_bits","filter":"orca_bits_match_on_aarch64","exact":true}}
```

The response contains a job ID. Poll that ID with `agent.job_status` and assert
`result.ok == true`, `result.failed == 0`, and `result.passed == 1`.
For all solver tests, omit `target`, `filter` and `exact` while keeping `package`.
The request uses the host architecture; it does not remotely select an aarch64
machine. F-03 runners must execute the same test on aarch64.

Domain checks: `--test orca_domain` (run it with and without `--release`; the
out-of-domain test asserts the documented fallback, so a panic or a wrap fails).

Common existing reason codes: `run_tests_bad_name` (invalid package, target or
filter), `run_tests_no_manifest` (project root lacks Cargo.toml),
`run_tests_busy` (another test job), and `run_tests_spawn_failed` (cargo unavailable).
The kernel adds no protocol methods. Its domain reason codes (table above) are
returned by `OrcaDomainError::code()` for host agent rows.

## Direct checks and migration

```text
cargo test -p regia_detsim
cargo test -p regia_detsim --test orca_bits -- --nocapture
cargo xtask det-scan --json
```

The lockstep closure has three exact-line pins for the copied helper's scalar
`v128` variable name, which the lexical scanner otherwise mistakes for a SIMD
type. The binding is explicitly `u128`; no SIMD is used (ADR-8150).

The acceptance test compares exact head-on velocities and a 2048-case corpus
hash computed from the unmodified source. A mismatch is a sim compatibility
failure. Do not update the expected hash to accommodate a numerical change.

DO27 replaces its avoidance module imports with `regia_detsim::avoid` on its own
branch, retaining its meter-to-millimeter conversion points and candidate query.
Use the defaults first. Before deleting its copy, DO27 runs the original seven
avoidance tests and its full simulation pins: the Switchbacks tunnel checksum
`0xbd96_cd18_09dc_958d`, prediction without a vertical jump, short determinism fuzz
fixtures, and the 3600-tick two-seat replay hash. Only DO27 can run those pins;
they are not implied by the isolated kernel test. Numeric repairs belong to the
Stage B re-pin process.

DO27 switch to the scratch form (ADR-8377): replace per-call
`orca_velocity`/`orca_velocity_with_settings` calls in its avoidance callers with
one `OrcaScratch` per crowd step and `orca_velocity_into`; optionally gather
neighbors with `orca_velocity_indexed` from the shared spatial index using the
same look distance. DO27's inputs are inside the domain (world extent well under
±2,147 km, hull speeds under 1,048 m/s, radii under 65 m, default settings, cap
8), so its pins must not move. Confirm with a debug soak asserting
`OrcaInput::validate` is `Ok` for every call (or `stats().out_of_domain == 0`),
then run the same pins listed above.

## The LP solver and walls

`OrcaSettings::solver` picks the kernel (ADR-8555). `OrcaSolver::StageA`, the
default, is everything above, bit for bit. `OrcaSolver::Lp` is an opt-in,
integer-only linear program after the public ORCA formulation (van den Berg,
Guy, Lin, Manocha, ISRR 2009):

- one half-plane per retained neighbor, from the nearest point of its truncated
  velocity obstacle (half responsibility; overlapping bodies use the contact
  rule over `contact_ms`), and one per static wall segment within reach
  (`OrcaSegment { a_mm, b_mm }`, two-sided, full responsibility, horizon
  `obstacle_horizon_ms`);
- walls first in slice order, then neighbors by `(distance_squared, id)`;
- the incremental 2D program picks the velocity nearest the (head-on biased)
  preference inside every half-plane and the speed circle of radius
  `max(max_speed_mm_s, |preferred|)`; when no velocity satisfies every plane,
  the fallback minimizes the largest penetration while keeping the walls.
  Where several velocities tie for that minimum (a body squeezed between two
  parallel walls), it takes the one nearest the preference, so the body keeps
  moving along the corridor instead of reversing at the speed cap. Walls that
  only conflict through the solver's 2 mm/s safety margin (a corridor barely
  wider than the body) stay hard without the margin (ADR-8703). A zero-length
  segment exactly on the body center gives no plane.

When to choose it: crowds in corridors and chokes, dense groups that must not
pass through each other, or bodies that should speed up to dodge (set
`max_speed_mm_s` above the preferred speed; the default 0 never runs faster than
preferred). Stage A stays the choice for hosts pinned to its bits.

Switching solvers changes results: every peer must use the same settings, and a
switch is a sim-version change for that host. Stage A ignores segments (counted
in `OrcaLpStats::stage_a_segments_ignored`).

```rust
use regia_detsim::avoid::{OrcaBody, OrcaScratch, OrcaSegment, OrcaSettings, OrcaSolver, orca_velocity_obstacles_into};

fn step(bodies: &[OrcaBody], prefs: &[[i64; 2]], walls: &[OrcaSegment], out: &mut [[i64; 2]], scratch: &mut OrcaScratch) {
    let settings = OrcaSettings { solver: OrcaSolver::Lp, max_speed_mm_s: 1_800, ..OrcaSettings::default() };
    for (k, me) in bodies.iter().enumerate() {
        out[k] = orca_velocity_obstacles_into(scratch, me, prefs[k], bodies, walls, &settings);
    }
}
# fn main() {}
```

`orca_velocity_into`, `orca_velocity_indexed` and the other entry points run the
LP too when `settings.solver` is `Lp` (without walls). For the det grid,
`regia_nav::det::walls::segments_from_grid_window(grid, profile, center, r_m,
&mut segments)` returns the blocked-cell boundary around a body as merged
segments in a fixed order (off-grid cells count as blocked). The window is
clamped to the grid, so a center or radius far off the map returns only the map
edge it reaches, or nothing (ADR-8703), on every legal grid up to a side of
`i32::MAX` cells (ADR-8804).

| Setting (LP only) | Default | Meaning |
|---|---|---|
| `solver` | `StageA` | the kernel |
| `max_speed_mm_s` | 0 | speed circle radius; never below the preferred speed |
| `obstacle_horizon_ms` | 1000 | how far ahead walls are avoided |
| `contact_ms` | 100 | time to leave an overlap with a neighbor or a wall |

LP reason codes (checked only when `solver` is `Lp`; `orca_horizon` also rejects
`horizon_ms = 0` there):

| Reason code | Meaning |
|---|---|
| `orca_max_speed` | `max_speed_mm_s` is negative or above `ORCA_MAX_SPEED_MM_S` |
| `orca_obstacle_horizon` | `obstacle_horizon_ms` is outside `1 ..= ORCA_MAX_HORIZON_MS` |
| `orca_contact` | `contact_ms` is outside `1 ..= ORCA_MAX_HORIZON_MS` |
| `orca_segment` | a segment endpoint is outside `±ORCA_MAX_POSITION_MM` |
| `orca_segment_cap` | more than `ORCA_MAX_SEGMENTS` (256) segments |

What the last call did: `OrcaScratch::last_lp()` returns `OrcaLpOutcome
{ feasible, worst_violation_mm_s, planes }`; `lp_planes()` lists the half-planes
as built (`point_q` in 1/256 mm/s, `direction` of length 2^24, `obstacle`), and
`OrcaHalfPlane::admits(v)` is the exact integer test. A feasible result is on the
free side of every plane exactly (the solver works 2 mm/s inside each plane; a
set with less slack is reported infeasible). `lp_stats().fields()` gives the
`orca_lp_*` counter rows (calls, feasible, fallbacks, agent and wall planes,
skipped far, Stage A segments ignored, worst violation); ai-nav-05's
`sim.avoid_rows` publishes them with `OrcaSolver::name()` (`stage_a`, `lp`).

Cost: the LP does more integer work per call than Stage A, and dense crowds
reach the fallback often. Measured per 900-body tick with 8 neighbors in a dense
stress scene (release, timing evidence only, never asserted; p50):

| Host | Stage A | LP | LP + up to 8 walls |
|---|---|---|---|
| x86-64 build host (heavily loaded) | 0.47 ms | 3.5 ms | 6.3 ms |
| Pixel 9 Pro (aarch64) | 0.43 ms | 1.4 ms | 2.6 ms |

About 30 % of the calls in that scene needed the fallback. Wrap the crowd step in
a profiling span.

Checks:

```text
cargo test -p regia_detsim --test orca_lp
cargo test --release -p regia_detsim --test orca_lp
cargo test -p regia_nav_det --lib walls
```

`lp_bits_match_the_pinned_hash` pins a 2048-scene corpus with walls; x86-64 and
aarch64 must agree. `stage_a_is_untouched` keeps the Stage A corpus
`0xf1a197b3f232f005` through every entry point.
