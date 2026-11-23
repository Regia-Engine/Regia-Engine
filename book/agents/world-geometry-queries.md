# Native world geometry queries

`regia_world_geom` has an additive `stage_b` feature for queries over the cooked canonical triangles
and integer-millimeter ground lattice. T3 rows that disagree with spec 121 are classified, not copied
(ADR-8246 R1). Keep the compat calls until the integration lead clears W-29. Ring support and the
full-speed mouth hop wait on the W-14 zipper.

Load a `WorldGeom` from its cooked file and keep `new_state()` alongside it. This lane supplies the
initial immutable state; ground edits and blocker mutation belong to W-12 and W-13.

- `support(state, x, z, y)` returns the selected floor and its optional ceiling. A body caller adds
  its own reach to its origin before passing `y`; DO27 asks at its waist.
- `floors_at` writes an ascending column into a reusable `SpanBuf`. Its capacity is 16; `overflowed`
  reports excess floors or lids. The cook must reject content that exceeds the configured capacity.
- `surface_at` is the presentation query: the highest floor under the requested height and tolerance,
  and the lowest roof above that height. `ceiling_above` applies the cooked lid gap.
- `place_floor` (also `support_for_placement`) chooses the highest layer with sufficient clearance or
  open sky, falling back to ground. It is the placement call, not a body's movement rule.
- `clamp_under_ceiling` returns an origin height; `clamp_under_ceiling_contact` additionally identifies
  the lid only when the origin moved. A host can cancel upward speed then. There is no damping,
  bounce, horizontal speed change, or persistent contact state in the kernel.
- `covered_at` reports a roof above the selected interval. `push_disc` walks walls once inside a fixed
  world-space square (radius plus the cooked wall pad) around the input center, in canonical
  millimetre-key order. The physical index cell only accelerates that set. Its vertical band belongs
  to the caller.
- `segment` tests a finite line against `FaceMask` surfaces; `line_of_sight` is the all-solid clear-line
  check. `rock_between` selects a roof or a covered wall. The ground height pyramid only skips work;
  disabling it must preserve every answer bit.

The queries allocate nothing. Load-time index creation allocates. `stats` adds caller-owned
`QueryStats` and reference traversal without pyramid skipping, for diagnostics and W-28's agent
surface. No new agent dispatcher methods are installed by W-11.

Peers compare `(content_hash, QUERY_BITS_VERSION, regia_detsim::MATH_BITS_VERSION)`. `QUERY_BITS_VERSION`
is 8 (version 7 lets an active blocker plug answer, W-13; version 8 adds `push_capsule`, ADR-8531 numbered it; no earlier answer moved). Canonical plane heights use ordered scalar f32 arithmetic. IEEE square root normalizes normals
only; it never reaches a height. Shared edges are evaluated in a canonical direction (spec 121 §5.3
rule 2). That rule supersedes T3's inclusive barycentric test. Near-equal floors and equal-height lids
tie by the lexicographic key of the triangle's canonical integer-millimetre vertices, so a permutation
of the sources cannot move an answer (ADR-8246 R4). `TriRef`s follow that permutation. Wall queries use
the fixed world-space neighborhood above, so a 4 m cell and a 16 m cell return the same contacts, and
the height pyramid does not change a segment answer. Stage A arithmetic and hashes stay at the
version-5 bytes.

Run the native checks and benchmark:

```text
cargo test -p regia_world_geom --features stage_b,stats
cargo bench -p regia_world_geom --features stage_b --bench column
```

Pin the benchmark to one idle CPU using `taskset -c <cpu>`. It runs one call per T3 column and per
`at` row in source order, reporting median and p99 nanoseconds per individually timed call,
with native support, native columns, and compat support measured in the same process. Timer overhead
is retained equally for each kernel. Earlier batch measurements are labeled separately. Timing is
benchmark evidence, never a unit-test wall-clock assertion. The target is median support ≤150 ns
and native/compat ≤0.25 on each fixture. A pinned the build server bench after the millimetre-box reject
measured 1072–1923 ns and ratios 1.95–3.10. Ground-only medians were 241–431 ns. The bar is unmet.

The query table pins in `tests/native_table.rs` (x86-64) were made at `QUERY_BITS_VERSION` 6 and hold unchanged at 7 (W-13: blocker plugs move answers only while active) and 8 (the `push_capsule` rows are added). The full-pitch resident budget
holds after border deduplication and one triangle store (ADR-8246 R7). Ring support and mouth hops
stay ignored until W-14. The support speed bar is benchmark evidence, not a unit-test assert. See
ADR-8246 before a host swap.

`pilot_only` affects navigation eligibility; it does not remove a deck from geometric support.
The reference fixtures can be located through `REGIA_WORLD_GEOM_FIXTURES` when running test binaries
on a device. That environment variable is test/benchmark infrastructure, not a runtime query setting.
The approved edge-row generator is `cargo run -p regia_world_geom --features stage_b --example
native_edge_vectors -- <output-directory>`; do not treat its output as permission to replace
non-edge or conditioning differences.

## Character mover

The `stage_b` module `regia_world_geom::mover` moves bodies over the native queries (lane W-30,
ADR-8323). The host's integrator owns speed, gravity and input; the mover turns one tick's horizontal
displacement into a new position. It keeps no history and allocates nothing.

- `move_disc(geom, state, &DiscParams, origin, delta_xz)` is the hull rule: `support` at the
  origin's reach, the band from that floor, one `push_disc`, and `support` again at the pushed centre.
- `move_capsule(geom, state, &CapsuleParams, feet, delta_xz, stick)` is the walker. The step is cut
  into substeps (at most `substep_m` each, at most 64). Walls under one step are stepped, so only walls
  in `[feet + step_up, feet + height]` push the disc. A floor standing in the body band, or a lid over
  the feet and under the head, at the axis or on the rim stops the move (`MoveStop::Ledge`,
  `MoveStop::Roof`). The feet take the floor `support` answers one step up, or stay at their height
  when the drop is over `step_down_m` (`airborne`: the host's fall). `stick: false` (a jump) keeps the
  height.
- `push_capsule` and `clamp_capsule` are the capsule's vertical rule on `WorldGeom`. The rounded feet
  rise onto floors met inside the radius, and the rounded head is held under lids met inside the
  radius. The column is read at the axis and eight half-radius points. A head pushed under the feet
  is set to the feet (`hi == lo`: no fit). Spec 121 § 5.4 has the formula.
- `body_probe(geom, state, &CapsuleParams, feet)` reads a standing body's support span, its band
  after the vertical rule, whether a lid covers it and whether it fits. This is the read the agent
  surface returns for a body.
- `dressing_grid(params, gltf, terrain, geom, state)` builds the on-foot grid of one set piece for
  loose dressing only. A drawn triangle within `SHELL_MATCH_M` (0.30 m) of a canonical floor, lid or
  wall is the tunnel shell and is never stamped.

`CapsuleParams` and `DiscParams` have no defaults: pass your game's sizes. Dressing that is a real
floor belongs in the canonical triangles. The cook turns every `*_skylight` terrain patch that
stands in a layer span into the prop layer `skylight_props`: up-facing, pilot-only floor triangles,
so a walker stands on the rubble through `support`. A map with no skylight patch cooks byte for byte
as before.

Each capsule move reads up to nine columns per substep for the body band and nine for the vertical
rule. Budget it for walkers, not for every projectile.

Agent surface: `geometry.body_probe` returns `body_probe` for one body over a cooked
`.meshlayers` file on the host. It is read-only and stateless: the file is loaded on every call,
so the cost class is heavy. The `terrain.*` methods over the running map's geometry (lane W-28,
`terrain.surface_at`) come later.

```text
geometry.body_probe { "cooked": "maps/demo.meshlayers", "feet": [0.0, 0.0, 0.0],
                      "radius": 0.35, "height": 1.85, "step_up": 0.55, "step_down": 0.55 }
-> { "support": { "floor": { "y", "kind", "layer", "triangle" }, "ceiling": { .. } | null },
     "band": { "lo", "hi" }, "covered", "fits", "query_bits_version" }
```

`kind` is `ground`, `floor`, `roof`, `wall` or `blocker`. `layer` and `triangle` are null for the
ground. `fits` is false when the vertical rule lowered the head or when a floor or lid stands in
the body band.

| Error | Meaning |
|---|---|
| `body_probe_bad_params` | `cooked` is missing, empty or over 4096 bytes, or a capsule size is not a finite number in range (radius and height above 0, steps at least 0). |
| `body_probe_bad_point` | `feet` is not three finite numbers. |
| `body_probe_unreadable` | The file cannot be read. |
| `body_probe_corrupt` | The file is not a cooked `.meshlayers` this engine loads. Re-cook it. |

The handler's end-to-end test cooks a flat map to a temp file and probes it. Run it with
`cargo test -p regia_agent --lib geometry_probe`. The mover's own tests:

```text
cargo test -p regia_world_geom --features stage_b,stats --test character_mover
```

`capsule_vertical_rule_vectors` compares `push_capsule` against
`tests/fixtures/regia/capsule-vectors.tsv`. Set `REGIA_WRITE_CAPSULE_VECTORS=<path>` to write the
table your build computes. A difference on any arch is a desync, never a re-bless without a
`QUERY_BITS_VERSION` bump.


## Ground edits

D1 craters and berms are sim-owned edits of the ground lattice (spec 121 § 6, lane W-12, ADR-8313).
Every query takes the `GeomState` they live in, so `support`, `surface_at`, `floors_at` and `segment` move
together. A fresh state answers exactly as before (the version-6 pins do not move).

<!-- ignore-reason: a fragment over a loaded `geom` and sim positions `x`, `z`; the compiled example is the `regia_world_geom::edit` module doc test -->
```rust,ignore
use regia_world_geom::{GroundEdit, m_to_mm};
let mut state = geom.new_state();
let dirty = state.apply_ground_edit(
    &geom,
    &GroundEdit::Crater { x_mm: m_to_mm(x), z_mm: m_to_mm(z), radius_mm: 3000, depth_mm: 450 },
)?;
let checksum_term = state.state_hash(); // fold as one more little-endian u64
```

- **Integer arithmetic.** Centres and heights are `i32` millimetres. Convert a sim `f32` position with
  `m_to_mm` only. The profile is `depth * (r² - d²) div_euclid r²` over the lattice vertices strictly
  inside the radius, in ascending vertex index. A berm raises the same profile. `Rect` writes absolute
  heights of a lattice rectangle (row-major, x fastest). Heights clamp to ±1 000 000 mm.
- **Ceiling stop.** A crater never lowers a vertex below the highest layer roof under it plus
  `min_cover_mm` (default 2000), and never raises one to reach that cover. A layer floor never moves.
- **Mouth rings are fixed.** A ring vertex keeps its cooked pin, so the ground still meets the band's edge.
- **Nested maps.** An edit first promotes every ground tile it touches to full pitch. The new vertices
  take the old surface (at most 0.5 mm off), and a border next to a coarse tile takes that tile's edge.
- **Dirty rect.** The footprint's vertex box plus every promoted tile. Hand it, in this order, to the
  presentation (`GeomState::height_rects` cuts it into `TerrainHeightEdits` rectangles of absolute mm,
  at most 128 samples a side), to the nav tiles (`DirtyRect::ground_tiles`), and compare
  `GeomState::generation` (one per edit that changed the state; an edit that changes nothing keeps it).
- **State hash.** `state_hash()` is O(1): a running XOR of one term per override, promoted tile and
  flipped blocker, folded with the generation. A fresh state on any map hashes to
  `edit::FRESH_STATE_HASH` (`0x68752350ae1d483f`). The same overrides reached by other edits hash the same.
- **Snapshot.** `encode` appends the byte-stable little-endian snapshot (join in progress, host
  migration); `GeomState::decode(&geom, bytes)` restores an equal state with the same hash and answers.
- **Segments.** An edited state keeps its own copy of the height pyramid's ranges, widened by every
  height an edit writes (about 44 KB on a 4096-sample map, shared between clones until the next edit),
  so `segment` and `line_of_sight` skip ground tiles by height on an edited map as they do on a fresh
  one (ADR-8818). The ranges only grow, they are not part of the state's value (`==`, `state_hash` and
  the snapshot ignore them; `decode` rebuilds them), and the answers are the same with or without them.
- **Cost.** One edit walks its footprint once per ground tile it touches. A 6 m radius crater (about
  1150 vertices at a 0.3125 m pitch) takes about 0.06 ms on the Android test device and 0.08 ms on the
  Steam Deck (spec 121 § 11's bar is 0.2 ms); `height_rects` of its rectangle about 0.01 ms. On a
  `Nested` map the first edit in a coarse tile also promotes it: about 0.17 ms and 0.24 ms for one
  64-cell tile.

Error codes:

| Code | Meaning |
|---|---|
| `edit_out_of_map` | a disc wholly beyond the map edge, or a rectangle that is empty or past it |
| `edit_too_large` | the footprint, clipped to the map, is over 128 samples on a side |
| `edit_out_of_range` | a height outside ±1 000 000 mm, a radius outside 1..=1 000 000 mm, a depth or height outside 0..=2 000 000 mm, or a `Rect` whose height count is not `width * height` |
| `geom_snapshot_newer` | the snapshot was written by a newer engine |
| `geom_snapshot_other_map` | the snapshot's content hash is another map's |
| `geom_snapshot_corrupt` | truncated, trailing bytes, an index out of range, unsorted entries, or a hash that does not match |

Agent methods: `terrain.ground_edit` (spec 121 § 12) and a read-only `terrain.ground_edits`
(`{}` -> promoted tiles, overrides, `state_hash`, `generation`) come with lane W-28's `terrain.*`
dispatcher. Until then, verify edits with:

```text
cargo test -p regia_world_geom --features stage_b,stats --test ground_edits
```

## Blockers

A collapsible section's blocker plug is one bit of `GeomState` (spec 121 § 5.3 rule 6 and § 5.7, lane W-13). The cook
writes the plug as the section's cross-section over `plug_length_m` (two end faces and a top, `Blocker` triangles after
every passage triangle of the layer) and marks the section and its portals with the plug's id. The sim decides when
rubble falls and when it is cleared (health, explosive-only damage and "stays until cleared" are sim rules); the
geometry only holds the bit.

<!-- ignore-reason: fragment of a sim tick: `geom`, `state` and `tick_flips` come from the game -->
```rust,ignore
use regia_world_geom::{BlockerId, SectionId};
let plug = geom.blockers()[0];                     // id, section, initially_active, triangle range
let g = state.set_blocker(&geom, BlockerId(plug.id), true)?; // Generation: one per flip that changes a bit
state.apply_blocker_flips(&geom, &mut tick_flips)?;  // one tick's flips, any order: applied in ascending section
assert!(!geom.section_open(&state, SectionId(plug.section)));
```

- **One generation.** A flip writes one blocker word, XORs one hash term and bumps the generation once. Every
  consumer reads that bit, so the query candidates, the nav cells and the sound occlusion all change at the
  generation `set_blocker` returns, and revert at the next one. Setting a plug to the state it is already in
  changes nothing and returns the current generation.
- **Queries.** While a plug is active its top is a floor of `support`, `floors_at` and `surface_at` (kind `Blocker`),
  every plug face is a solid of `segment`, `line_of_sight` and `rock_between` (`FaceMask::BLOCKER`), and the end faces
  are walls of `push_disc`. While it is clear it is skipped by one bit test; with no plug active at all the queries
  skip the test, so they answer bit for bit as a map without plugs. W-13 made `QUERY_BITS_VERSION` 7 (it is 8 since W-30's `push_capsule`).
- **Fixed order.** `apply_blocker_flips` sorts a tick's flips by the plug's section, then id, then `false` before
  `true` (a plug listed twice ends active), so two peers fed the same flips in their own orders reach the same
  `state_hash`. An unknown id anywhere in the list applies nothing.
- **Nav and portals.** `section_open`, `portal_open` (a portal is closed when its own plug or either side is closed;
  VisCell traversal, portal sound and the layered nav read it), `closed_sections` (the whole closed list, ascending;
  the shape of DO27's collapse closed set) and `cell_route(state, from, to, outdoor_cost_pct)`: a route over the
  section graph (Dijkstra over the portals' nav nodes, integer millimetres; outdoors costs `outdoor_cost_pct` per
  cent of the distance). W-20's layered nav builds its polys on these reads.
- **Agent view.** `GeomState::blockers(&geom)` lists `{ id, section, active }` ascending, the reply of the agent
  method `terrain.blockers` that lane W-28's `terrain.*` dispatcher serves (`blocker_unknown`, `blocker_sim_owned`).
- **Hash and snapshot.** The blocker term is `state_hash`'s tag 1 (see Ground edits); `encode`/`decode` carry the
  blocker words, so a restored state has the same bits, hash and answers.

| Code | Meaning |
|---|---|
| `blocker_unknown` | `set_blocker`, `set_blocker_with_stats` or `apply_blocker_flips` named an id the map has no plug for; nothing changed |

Verify with:

```text
cargo test -p regia_world_geom --features stage_b,fixtures --test blockers
```

The four tests: `a_blocker_closes_and_reopens_in_one_generation` (support, floors, segment, rock, sight, push and
nav at generations 0, 1 and 2), `two_peers_checksum_equal_across_a_close_and_reopen` (equal `state_hash` every tick
and equal query tables), `blocker_flip_is_o1_in_state_hash` (one word and one hash term per flip over 10 000 flips;
snapshot round trip) and `a_collapse_closes_its_passage_and_clearing_reopens_it_stage_b` (DO27's collapse proof on
the cooked Switchbacks fixture: through the passage, round on the surface, through again).
