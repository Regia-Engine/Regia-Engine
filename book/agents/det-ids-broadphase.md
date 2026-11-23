# Deterministic ids and the lockstep broadphase

Two pieces of `regia_detsim` for a lockstep simulation (ADR-8048): entity ids that every peer
issues identically, and a broadphase for body-vs-body separation that gives exactly the same result
as the plain all-pairs loop, only faster. Read "Lockstep math and the determinism scan" first: the
same rules apply to code that uses them.

## Ids: `regia_detsim::ids::DetIdAllocator`

| Call | What it does |
|---|---|
| `DetIdAllocator::<u16>::new(cap)` / `::<u32>::new(cap)` | Ids `1..=cap` (id 0 is never issued). The cap is your setting, e.g. a lobby's entity budget. |
| `alloc()` / `alloc_handle()` | The oldest freed id, else the lowest never-issued id that is not reserved; `None` when all are live. `alloc_handle` also returns the id's generation. |
| `free(id)` / `free_handle(h)` | Put the id at the back of the free list and bump its generation. Call it after the entity is gone from the sim. `free_handle` frees only a current handle. |
| `reserve(id)` | Claim a fixed id (a seat's slot, a level-placed entity). Never moves the fresh cursor, so low ids stay available. |
| `is_live(id)`, `is_current(h)`, `generation(id)`, `handle(id)` | Queries. A `DetHandle` taken at spawn stops being current when the id is freed, and stays stale after the id is reused. |
| `live_count()`, `free_count()`, `live_ids()` | Counts, and the live ids in ascending order (a deterministic walk for a snapshot or a hash). |

The id an entity gets depends only on the order of earlier calls. Two peers that spawn and despawn
in the same tick order get the same ids. Memory grows with the highest id used, not with the cap.

## Broadphase: `regia_detsim::broadphase`

A lockstep sim usually starts with this loop, and its checksum depends on its order, because
resolving a pair moves the bodies the next pair sees:

<!-- ignore-reason: the reference loop in outline; `overlap` and `resolve` are the game's -->
```rust,ignore
for a in 0..n { for b in a + 1..n { if overlap(a, b) { resolve(a, b) } } }
```

`DetBroadphase::for_each_contact` visits the same contacts, in the same `(a, b)` order, with the same
bits, but only tests nearby pairs:

<!-- ignore-reason: a host's tick; `bodies` mirrors the game's entities and `push_apart` is the game's resolver -->
```rust,ignore
use regia_detsim::broadphase::{BroadphaseConfig, DetBroadphase, FloorRule};

let mut grid = DetBroadphase::new(BroadphaseConfig {
    floor_rule: FloorRule::Bands,
    ..BroadphaseConfig::default()
});
// Each tick:
grid.for_each_contact(&mut bodies, |bodies, c| {
    // c.a < c.b; c.dx, c.dz, c.dist_sq, c.min_dist are the reference loop's numbers.
    push_apart(bodies, c); // may change pos[0] and pos[2] of bodies c.a and c.b only
});
let stats = grid.stats(); // candidates, contacts, cell moves, re-gathers, non-finite, outliers, large
```

- **Bodies.** `BroadBody { pos, radius, band, underground }`: a disc on XZ of `radius`, filling the
  height `band = [lo, hi]` (`body_band(y, BandExtent { below, above })`).
- **The visitor's contract.** It may change x and z of the two bodies of the contact, and anything
  outside the slice. Changing other bodies, y, a radius, a band or `underground` during the pass
  breaks the equivalence with the reference loop.
- **Floors.** `FloorRule::Bands` (default): bodies whose bands do not overlap do not touch (a car on
  a bridge deck and one on the road under it). `FloorRule::UndergroundOnly`: only when one of them is
  underground; two outdoor bodies always touch on XZ. That is the source game's rule, kept for a host
  that must keep its checksums. `FloorRule::Never`: height-blind.
- **Settings.** `cell_size` (about a typical body's diameter; 4 m default), `margin` (how far a row's
  body may be pushed before its neighbor list is gathered again; 0.5 m), `max_cells` (memory bound;
  the cell grows to fit). `DetBroadphase::new` clamps `max_cells` to `1..=2^24` so cell indices
  remain representable. Any values give the same contacts; only the speed changes.
- **Odd bodies.** A body with a NaN or infinite x, z or radius is tested against every other body,
  as the reference loop does.
- **The reference.** `for_each_contact_n2(bodies, rule, visit)` is the all-pairs loop through the
  same `contact()` test: use it as an oracle in your own tests.

Cost per pass on a MacBook Air M5 (release, a crowd covering about half the ground, `cargo run
--release -p regia_detsim --example broadphase_cost -- <n> <passes> [cell_size] [margin]`): 210 bodies
0.007 ms (all-pairs 0.020 ms), 900 bodies 0.036 ms (0.262 ms), 4000 bodies 0.27 ms (4.8 ms). On a
heavily loaded x86-64 build host the same runs took about five times as long (900 bodies 0.17 to
0.24 ms against 0.9 to 1.45 ms). Wrap the call in your tick's `tracing` span; the crate itself has no
logging dependency.

## Outliers and large bodies

Two settings keep the grid fine when a few bodies do not fit it (ADR-8837). Both only prune: any
values give the same contacts, in the same order, with the same bits.

- **`world_bounds: Option<[f32; 4]>`** (`[min_x, min_z, max_x, max_z]`, default `None`). Without it
  the grid spans every finite body, so one body parked 1000 km away (a pooled or dead entity) makes
  the cell grow until the whole span fits `max_cells`, and every live body lands in a few cells. With
  bounds, a finite body outside them is an *outlier*: it stays off the grid, and each row tests it
  only when it lies inside that row's padded reach. The grid spans the bodies inside the bounds.
- **`large_radius: Option<f32>`** (default `None` = twice `cell_size`; `Some(f32::INFINITY)` turns it
  off). Without it every row gathers the cells within its own radius plus the *largest* radius of
  all bodies, so one 60 m body (a building, a hill) widens every gather. A body whose `|radius|` is
  above the threshold is *large*: it stays off the grid, the grid's reach uses the largest normal
  radius, and each row tests a large body only when it lies inside the row's reach plus that body's
  radius.

<!-- ignore-reason: a host's settings; `bodies` mirrors the game's entities -->
```rust,ignore
use regia_detsim::broadphase::{BroadphaseConfig, DetBroadphase};

let mut grid = DetBroadphase::new(BroadphaseConfig {
    cell_size: 4.0,
    // The map is 2 km across; pooled entities wait at (1e6, 1e6).
    world_bounds: Some([-1000.0, -1000.0, 1000.0, 1000.0]),
    // Bodies over 8 m (2 x cell_size, the default) are tested one by one.
    large_radius: None,
    ..BroadphaseConfig::default()
});
let stats = grid.stats(); // ... plus `outliers` and `large`: the bodies filed off the grid this pass
```

Outliers and large bodies cost one bounds check per row each (no contact test unless the row can
reach them), so keep both lists short: bounds that hold the play area and a threshold above most
bodies. `BroadphaseStats::{outliers, large}` count them at the start of each pass; a host shows them
in its own rows beside `candidates`. `DetBroadphase::new` ignores bounds with a NaN or `min > max`
and reads a NaN `large_radius` as infinity and a negative one as zero. `BroadphaseConfig` snapshots
(`brodconf`) are version 2: version 1 snapshots are refused with `bad_version`.

`DetSpatial` reads the same two settings. Rows outside `world_bounds` file into the grid's edge
cells (cell indices are clamped and monotone, so every query stays conservative) and no longer
stretch the grid; large rows sit in a list every query gathers, like rows with a non-finite
position. `SpatialStats::{outlier_rows, large_rows}` are gauges: rows outside the bounds at the last
build, and large rows now.

Checks: `cargo test -p regia_detsim --test det_crowd` (`one_parked_body_does_not_collapse_the_grid`:
900 bodies plus one at `(1e6, 0, 1e6)`, candidate tests within 1.1x of the run without it;
`one_large_body_does_not_widen_every_gather`: with one 60 m body the other rows' tests stay within
1.2x, and most of the large body's own tests are rows within its contact reach;
`the_oracle_scenarios_match_with_world_bounds_and_large_classes`;
`random_bounds_outliers_and_radius_mixes_match_the_n2_loop`: 200 seeded cases with parked, dragged and
NaN bodies, every contact's bits against the all-pairs loop) and `cargo test -p regia_detsim --test
spatial_oracle` (`a_parked_row_does_not_collapse_the_query_grid`, `a_large_row_does_not_widen_every_query`,
`random_bounds_outliers_and_large_rows_match_the_linear_scans`).

## For agents

The allocator and the broadphase belong to the game's sim, not to the engine. A game that runs them
exposes `DetBroadphase::stats()` and the allocator's counts in its own rows (an engine-native
lockstep game gets `sim.desync_rows`). To see the indexes, use `sim.det_debug_draw` (see
[Seeing the index](#seeing-the-index)). To check a change keeps the order, run
`cargo test -p regia_detsim --test det_crowd`: `broadphase_matches_the_n2_pair_order_bit_for_bit`
compares 900 bodies over 30 ticks against the reference loop and names the first contact that
differs.

Allocator memory is O(highest id touched), not O(live ids). Its live and generation
tables grow through the largest issued or reserved id and do not shrink on free.
A large cap alone is cheap; reserving a distant `u32` id can exhaust memory. Bound
imported ids to the host's memory budget before calling `reserve`.

## Range and nearest queries

`regia_detsim::spatial::DetSpatial` (ADR-8318) is a per-tick index owned by the
host simulation. Build it once from `SpatialRow { slot, body, group_mask, flags }`.
`slot` is the caller's array index, not an entity id. Rows may arrive in any order
but their slots must be a permutation of `0..rows.len()`. `body` is `BroadBody`,
including its radius, absolute band and underground flag. The opaque `u32` flags
word is preserved for host use; it is not a built-in filter.

| Call | Contract |
|---|---|
| `build(&rows)` | Replace all rows; retain buffers. Invalid slots leave the old index intact. |
| `update(slot, [x, z])` | Refile immediately, including outside the initial bounds or to/from NaN. Only XZ changes; rebuild for other row changes. |
| `within(center, r, mask, visitor)` | Conservative candidates, ascending slot order. The callback retains the exact distance test and returns `ControlFlow`; `Break` stops immediately. |
| `within_mut(center, r, mask, visitor)` | Also passes the index to the callback for updates. Re-gathers after a refile, visiting only later slots. No nested queries or builds. |
| `nearest(point, mask, max_r, dist_fn)` | Expanding cell rings, caller's pure squared XZ distance, inclusive maximum radius, lowest slot wins ties. NaN distances never win. Stops at the first ring whose rows all lie farther than the best so far (ADR-8875), so `dist_fn` is not called for them; the score must never be below the squared XZ distance to the row's disc. |
| `first_k_within(center, r, mask, k, out)` | First k conservative candidates in slot order, replacing `out`. |
| `first_k_within_by(center, r, mask, k, out, accept)` | First k exact hits under the caller's predicate. Both first-k calls read lists in order of their lowest slot and stop at the k-th hit (ADR-8875); same visit order as `within`. |
| `any_within(center, r, mask, accept)` | Read-only (`&self`): true when `accept` passes any candidate of the same envelope as `within`. Cell order, not slot order; stops at the first pass; moves no counter. For order-free questions only (fog of war's "does any watcher see this point", ADR-8473). |
| `row(slot)`, `config()`, `stats()`, `reset_stats()` | Inspect current data/settings/counters; reset cumulative counters while preserving the wild-row gauge. |

The query center is a `BroadBody`; its band and underground flag participate in
`FloorRule`, and its radius is ignored in favor of the explicit query radius.
A mask matches any shared bit; zero matches nothing. Non-finite rows always reach
the exact distance test when their mask/band matches. The padded envelope covers
f32 squared XZ disc distance against `(r + row.radius)^2`, including boundary
rounding. Negative range radii get an absolute conservative envelope; the caller
still decides acceptance. `nearest` rejects negative or NaN maximum radii.

`nearest` does not support arbitrary scoring metrics. For auto-fire priority,
keep the exact range predicate and best-score reduction in ordered `within`.
For mutation equivalence, attach movement only to accepted visits, keep the query
center/radius fixed during a call, and update both the host row and index before
continuing. Moving an already visited slot never revisits it.

The agent surface is `SpatialStats`: cumulative `builds`, `queries`, `candidates`,
`hits`, `refiles`, and current `wild_rows`. `hits` counts selected nearest results
and `first_k_within_by` acceptances; a `within` callback does not report exact hits.
`candidates` counts actual callback/metric invocations after mask and band filters,
including an invocation that breaks. `refiles` counts cell or wild-list changes,
not moves within a cell. `nearest_rings` counts the cell rings `nearest` walked and
`first_k_lists` the lists the first-k calls read (ADR-8875). Wrap the batch in the host's tracing tick span. The
planned `sim.tick_profile` integration consumes these counters; this lane adds no
protocol method or engine-owned simulation instance.

Errors: `SpatialError::code()` is `spatial_slot` for an absent update slot or an
invalid/duplicate build slot. There are no other protocol error codes.

Verification: `cargo test -p regia_detsim --test spatial_oracle --test spatial_alloc`
compares full accepted sequences, nearest choices and mutation results with a
linear scan over 900 rows, 40 groups and 600 ticks. A separate counting allocator
proves no allocations after warmup, including moves into empty cells and wild
lists. `cargo test -p regia_detsim --test det_golden spatial_queries -- --nocapture`
prints the sequence hash with the existing `regia-det-golden` report prefix.
Other targets must match the x86-64 reference; until recorded they correctly
report `unrecorded_target`. No game pin changes as a result of adding this API.

## Segment and shape queries

ADR-8478 adds swept and shaped entity queries to `DetSpatial`. Each one prunes
only. The caller's closure keeps its exact geometric test, and every call visits
candidates in ascending slot order, as `within` does. Filtering the visits by the
exact test gives the same sequence as a linear scan, bit for bit.

| Call | Contract |
|---|---|
| `along_segment(from, to, r, mask, visitor)` | Every slot whose disc may touch the capsule from `from` (its XZ) to `to` with radius `r`. Ascending slot order, not order along the segment. `from` also supplies the floor-rule band. |
| `first_along_segment(from, to, r, mask, accept)` | Lowest slot `accept` (the exact capsule test) accepts, or `None`. Stops at the first acceptance. This is "the first entity the shot passes in entity order". |
| `within_sector(apex, dir, cos_half, r, mask, visitor)` | Candidates in the sector's padded bounding box. The visitor applies the exact cone test, including the distance test. |
| `within_box(probe, min, max, mask, visitor)` | Candidates in the padded box. Corners are sorted per axis. `probe` supplies only the band. |
| `build_from(items, to_row)` | `build` from any slice through a projection `(index, &item) -> SpatialRow`, such as the game's history rows for a rewind tick. |

The segment walk visits the cells within `k` cells of an Amanatides-Woo walk
over the segment, clamped to the grid. `k` is the padded capsule radius in cells,
rounded down, plus one. The pad is `(reach + |dx| + |dz|) / 256 +
max|coordinate| / 262144 + 0.001`, where `reach` is `|r|` plus the largest row
radius. Long segments, and radii larger than a cell, therefore keep every
candidate. Rows that moved outside the build bounds sit in clamped edge cells,
and the walk clamps the segment the same way.

These inputs scan every cell, which matches the wild rule: a NaN or infinite
endpoint, a non-finite radius, an overflowed padded reach or its square, and an
endpoint more than 2^30 cells from the grid. Wild rows are always candidates.
The mask and the floor rule always apply.

The exact segment test must clamp a NaN parameter `t` to an endpoint. A NaN `t`
comes from a zero-length segment or an overflow. The envelope covers the f32
closest-point test against `(r + row.radius)^2`. A sector with a zero or
non-finite `dir`, or a NaN `cos_half`, prunes by its bounding circle. The sector
box pad covers angular rounding in the caller's cone test up to about 1/256 rad.

Lag compensation: Regia keeps no history. The game builds a second index with
`build_from` over its history rows for the rewind tick, then queries that index.
Each distinct rewind tick costs one build per tick, linear in rows. Cache the
index by tick in the caller (a small array, not a hash map), and reuse it for
every shot that rewinds to the same tick.

`SpatialStats` adds three fields:

- `segment_queries`: segment calls, including those made through `first_along_segment`.
- `shape_queries`: sector and box calls.
- `cells_walked`: grid cells those calls gathered.

None of these calls increments `queries`. All of them add to `candidates`, and a
found `first_along_segment` adds one to `hits`. World geometry and line of sight
are not part of this index. They belong to the native query core. The
`LineOfSight` seam combines its blocker distance with `first_along_segment`.

Errors: `build_from` returns `spatial_slot` when the projected slots are not a
permutation of `0..items.len()`. The previous index stays intact. These calls add
no other error code.

Verification: `cargo test -p regia_detsim --test spatial_oracle --test spatial_alloc`
covers four groups. 600 ticks of 200 shots each, 0.1 m to 400 m long, at radius
0 and 60 m (larger than the 40 m cell), match a linear scan. The segment edge
cases match it too: zero length, along a cell edge, an end exactly on a disc
boundary, endpoints at 1e6, and NaN or infinite endpoints. Sectors, boxes and
rewind indexes are compared against linear scans. No query allocates after
warmup. Evidence: `cargo run --release -p regia_detsim --example broadphase_cost
-- 900 600 --shots`.

## Contact response

`regia_detsim::contact` answers the broadphase's contacts (ADR-8563). Each body has a
`ContactBody` in a slice parallel to the broadphase bodies, and a velocity `[x, y, z]`:

| Field | Meaning | Valid |
|---|---|---|
| `inv_mass` | `1 / mass` (1/kg); `0` = static or kinematic, never moved by contacts | finite, `>= 0` |
| `restitution` | bounciness; a pair uses the larger value | `[0, 1]` (above 1 adds energy, refused) |
| `friction` | Coulomb coefficient; a pair uses `sqrt(fa * fb)` | finite, `>= 0` |
| `max_push_m` | most one contact may push the body in one pass (m) | `>= 0`, infinity for no cap |

`ContactConfig` holds `ram_threshold_m_s` (a pair closing strictly faster is a `Ram`; infinity
reports none) and `bounce_min_speed_m_s` (at or below it a pair does not bounce, so a resting pile
does not jitter). Defaults: 6 m/s and 0.5 m/s.

`contact::resolve(bodies, props, vel, contact, cfg)` handles one contact, in this order:

1. Normal `n = (dx, dz) / dist` from `a` to `b`. Coincident pairs (closer than
   `DEGENERATE_DIST_M`, 1 µm) and NaN distances use the fixed axis `+X`: the lower slot goes to
   `-X`, the higher to `+X`.
2. Push: the overlap is split by inverse mass (`a` moves `overlap * wa / (wa + wb)`), each share cut
   to the body's own `max_push_m`. Direct position projection, no velocity bias. A non-finite overlap
   (infinite or NaN radius) pushes nothing.
3. Normal impulse `j = (1 + e) * closing / (wa + wb)` for a closing pair.
4. Friction on the XZ tangent, at most `mu * j`. The solver never changes `vel[1]` or `pos[1]`.

A body with `inv_mass == 0` is never written, bit for bit. A NaN velocity skips the impulse. The
other body of a pair stays finite whenever its own inputs are.

`contact::resolve_all(&mut grid, bodies, props, vel, &cfg, &mut rams)` runs one pass through
`DetBroadphase` and `contact::resolve_all_n2(bodies, props, vel, &cfg, rule, &mut rams)` through
the all-pairs loop: the same contacts, the same order, the same bits. Rams are appended (not
cleared) in visit order, which is ascending `(a, b)`. Both check the settings first and change
nothing on an error. No allocation once the grid's buffers and `rams` have grown.

The pass returns `ContactStats { pairs, pushes, capped_pushes, rams }`. This is the
`sim.det_contact_stats` row: there is no protocol method yet (the lockstep sim host has not
landed), so a game exposes the struct in its own rows. Error codes (`ContactError::code()`):

| Code | Cause |
|---|---|
| `contact_bad_body` | a `ContactBody` field is out of range (the message names the slot and field) |
| `contact_bad_config` | a `ContactConfig` field is out of range |
| `contact_length_mismatch` | bodies, settings and velocities differ in length |

The golden `contact_pile` (`ContactPile`: 300 bodies, posts on a ring, a deck over the pit, 600
ticks) is one of the engine scenarios: `cargo test -p regia_detsim --test det_golden
engine_scenarios -- --nocapture`. Checks: `cargo test -p regia_detsim --test contact --test
contact_alloc`. `resolve_all_matches_the_n2_reference_with_the_same_resolver` runs 900 bodies for
600 ticks through both loops and names the first tick and body that differ. Cost on a host:
`cargo run --release -p regia_detsim --example contact_cost -- 900 600`.

Measured cost of one `contact_pile` tick with 900 bodies (pull, drag, integration and the contact
pass; about 606 contacts per tick; release builds, 60 warmup ticks, 600 samples, 2026-11-18):

| Host | Grid p50 | Grid p99 | All-pairs p50 | All-pairs p99 |
|---|---|---|---|---|
| Intel N150 laptop, Windows | 0.24 ms | 0.58 ms | 0.89 ms | 1.50 ms |
| Pixel 9 Pro (Tensor G4), Android | 0.30-0.32 ms | 0.44-0.45 ms | 0.59 ms | 1.03 ms |
| 128-thread build server, Linux, at load ~262 | 0.32-0.33 ms | 0.78-0.82 ms | 1.13 ms | 1.31 ms |

The server numbers were taken under heavy shared load and are an upper bound. These are evidence,
not limits: no test asserts a time.

## Seeing the index

Both indexes can draw what they did (ADR-8562). `regia_detsim` stays render-free: the indexes
describe themselves through `regia_detsim::debug::DebugSink` (lines, XZ circles, boxes and text at a
point), and the engine carries that to the gizmo overlay and the editor.

| Part (`DebugWhat`) | `DetBroadphase` | `DetSpatial` |
|---|---|---|
| `grid` | Grid bounds and the outline of each cell in use at the start of the last pass | Grid bounds and each cell in use now |
| `occupancy` | Each cell's body count, as text at its center | Each cell's row count |
| `candidates` | One line per candidate pair the last pass tested | (none) |
| `contacts` | One line per contact the last pass handed to its visitor, between the two centers as they were when the pair was tested, in visit order | (none) |
| `queries` | (none) | The last N queries, oldest first: a circle of the query radius, and a line from the center to each hit (the delivered candidates of `within`, the exact hits of `first_k_within_by`, the result of `nearest`; at most 16 per query) |

Capture is off by default, and a pass or query then does no debug work and allocates nothing
(`debug_emit` draws nothing). Turn it on per index with
`set_debug_capture(Some(DebugCapture::default()))`: the buffers are reserved at that call (4096
contacts, 4096 candidate pairs, 4096 cells, a 32-query ring by default), and a pass that produces
more keeps the first ones and counts the rest (`debug_capture_stats()`: `contacts_dropped`,
`candidates_dropped`, `cells_dropped`). Capture only reads: every contact, its order, every query
answer and every bit the sim computes are the same with it on or off, and nothing it holds is
hashed.

<!-- ignore-reason: statement fragment inside a game's tick; `draw`, `sim` and `tick` are the game's -->
```rust,ignore
use regia_framework::det_debug::{DebugWhat, DetDebugDraw};

// Once per tick, after the passes and queries to show:
draw.record(tick, &mut [&mut sim.grid, &mut sim.spatial]);
```

`DetDebugDraw::record` matches each index's capture to the switch (on while anything is drawn) and
records the frame; the `lockstep` gizmo group (added by `DebugGizmosPlugin`) replays it. Contacts and
queries appear from the tick after the switch turns on. A sim behind the sim host implements
`FixedStepSim::det_debug(&mut self, what, out) -> bool` the same way (return `true`, also for an
empty `what`); the host calls it after each step while drawing is on and the frame reaches the
overlay through `sync_det_debug_from_host`. In the editor, Show ▸ Lockstep draws play's frame over
the viewport, and the Profiler's Lockstep tab has a toggle per part.

### `sim.det_debug_draw`

| Method | Tool | Permission | Mutating | Params | Errors |
|---|---|---|---|---|---|
| `sim.det_debug_draw` | `sim_det_debug_draw` | play | yes | `what` array or string, optional | `no_det_debug`, `bad_what` |

```json
sim.det_debug_draw { "what": ["grid", "contacts"] }
→ { "source": "host", "what": ["grid", "contacts"], "tick": 2,
    "primitives": { "lines": 11, "circles": 0, "boxes": 4, "texts": 0 },
    "kinds": { "grid": 4, "occupancy": 0, "candidate": 0, "contact": 11, "query": 0, "query_hit": 0, "flow": 0 } }
```

`what` is a list of `grid`, `occupancy`, `candidates`, `contacts`, `queries`, `flow`, or the string `"all"`
or `"none"`. `flow` draws flow field arrows from a `regia_nav::det::flow::FlowDebug` source the game or sim records
(ADR-8839, [Deterministic navigation grid](det-grid.md)); the indexes ignore it. Omit it to read the current switch. A sim host whose sim draws wins over a
`DetDebugDraw` resource; setting `what` on a host also stores it in that resource (inserting it) so
the overlay follows.

`kinds.contact` equals the pass's `stats().contacts` unless the cap dropped some. `tick` is `null`
before the first drawn frame and after drawing is turned off.

| Code | When |
|---|---|
| `no_det_debug` | The app has neither a `DetDebugDraw` resource nor a sim host whose sim draws. |
| `bad_what` | `what` names an unknown part, or is not a string or a list of strings. |

Tests: `cargo test -p regia_detsim --test det_debug` (`debug_emit_lists_every_contact_of_the_last_pass`,
`capture_on_and_off_step_the_same_bits`), `cargo test -p regia_detsim --test debug_alloc`
(`debug_emit_is_off_by_default_and_allocates_nothing`), `cargo test -p regia_agent --lib
sim_det_debug_draw_agent_method_round_trip`.
