# Spec 121 — World geometry core (`regia_world_geom`)

Status: proposed (ADR-8019, lane D-03 world-geometry-design, 2026-11-16; amended by ADR-8072, DO27's sign-off
exceptions E1-E7, 2026-11-16; amended by ADR-8084, the absorb review rows W1-W5 and W7-W10, pending DO27's re-sign,
§ 16). Nothing below is built. Line: v0.6. The public API is semver-versioned and **DO27 signs
off on it and on every change** (ADR-8012 A-5, as for `regia_voxel_core`, spec 25 § 7.10). Sign-off state: § 16
(signed, with exceptions E1-E7, all carried here). The T3 reference (DO27's deterministic multi-layer query) is
described in DO27's sign-off answer; § 15 lists what it settles and what is still open, each item to be confirmed by
T3's handover.

Inputs, read as data: DO27's tunnel handover (§ 1-7, `int/v020` f7e8f4740), its Q14 answers, the tunnels research
report, ADR-7974 (spec 26 § 2a) and the absorb plan (§ 4). Citations of DO27 code are `crate/path:line` at f7e8f4740 unless they name T3 (`lane/tunnels_t3` @ 6fd2effb, re-cited
at the `regia-absorb-2` tag, § 4).
Citations of Regia code are `file:line` at `5c6639fb8`.

Related: spec 26 § 2a (terrain mesh layers: the presentation half, rewritten by ADR-8019 to sit on this crate),
spec 25 § 7 (DO27 interface contracts), ADR-8012 (copy exception A-1, determinism policy A-4, new crates A-5).

## 1. Need and scope

DO27's owner directive (2026-11-16) puts tunnels, overhangs and multi-level maps in the engine, from the description
to the frame **and to the lockstep query**. ADR-7974 kept sim collision out of scope. ADR-8019 reopens it: the engine
owns one GPU-free, ECS-free crate, `regia_world_geom`, that DO27's lockstep sim links and calls every tick.

The crate answers, from one set of triangles and one traversal order:

| Need (handover) | Call (§ 5) | Who calls it |
|---|---|---|
| T3 floor under, ceiling over | `WorldGeom::support`, `surface_at`, `floors_at` | hover hulls, pilots, projectiles, placement, AI |
| T3 wall push | `push_disc`, `push_capsule` | hulls, pilots |
| ceiling clamp | `clamp_under_ceiling` | hulls |
| rays: projectiles, LOS, rock between | `segment`, `rock_between`, `line_of_sight` | projectiles, hitscan, AI, radar |
| T7 satellite and overview occlusion | `covered_at` | sim rules, the overview bake |
| T7 collapse | `GeomState::set_blocker` | sim (health and damage rules stay in the sim) |
| D1 craters and berms | `GeomState::apply_ground_edit` | sim |
| T6 nav input | `sections`, `portals`, `walkable_spans` | `regia_nav` layered bake (W-18 to W-20) |
| T4, T5 presentation | `surface_at`, `segment`, `sections`, `zones` | camera, exposure, reverb, GI (spec 26 § 2a) |

In scope: the canonical geometry and its cooked format, the ground function, the query, the wall push, segments,
sections, portals, zones, blockers, D1 edits, the state hash, the bake checks that compare them, and the budgets.

Not in this crate: rendering, streaming, LOD and the seam draw (spec 26 § 2a, `regia_terrain`); navmesh building
(`regia_nav`); game rules (hover springs, the ledge and wall rules, damage, health, radar ranges); ECS components;
D2 volumes (the voxel core, spec 25 § 7.10, plugs into the same blocker and edit calls later; its proposed D2
amendment to this crate is spec 122 § 11, ADR-8116).

## 2. Principles

1. **One truth.** The canonical triangles are the only solid. Render, the mouth cut, nav, portals, zones and the query
   index are derived from them by one cook, and the cook fails when two derived outputs disagree (§ 10).
2. **Engine-general, not DO27-shaped.** DO27's caps (index cell, wall pad, same-height and lid gaps, step bound, hull fit, nav
   levels) are `GeomSettings` fields whose defaults are DO27's values. DO27 identifiers never enter the API.
3. **Determinism (ADR-8012 A-4).** Runtime queries are bit-identical on x86-64 and aarch64 (Linux, Windows, macOS,
   Android, the Deck): f32 positions in metres, IEEE `+ − × ÷` and `sqrt` in a fixed order, no `mul_add`, no SIMD
   vector types, no platform transcendental (none is needed: the query uses none), no hash-map iteration, no thread
   count in any result. Cooked data is integer and byte-stable. The crate is on the `det-scan` lockstep-closure list.
4. **Zero allocation per query.** Every per-tick call writes into caller-owned fixed-capacity buffers. The index is
   built once at load (and per dirty tile after an edit).
5. **Fully resident.** The sim's geometry never streams and never depends on render residency (research § 4).
6. **Two stages.** Stage A (§ 4) copies DO27's tunnel code with its arithmetic unchanged, so DO27 switches with no
   checksum change. Stage B (§ 5-9) is the native core; DO27 adopts it in one sim-version bump (§ 14, ADR-8019 A-10).

## 3. Units, coordinates and the ground convention (ADR-8019 A-7)

- **Axes.** x east, y up, z north, metres, the terrain centred on the origin (spec 25 § 3; DO27's sim uses the same
  axes, `tunnels-switchbacks.md:70`). Winding: a floor triangle is counter-clockwise seen from above.
- **Runtime numbers.** Positions and heights are `f32` metres, as DO27's mover stores them (`ir-sim/src/state.rs:1964`).
- **Cooked numbers.** Canonical triangle vertices are `i32` millimetres local to a 1024 m cooked tile (`tile_mm`
  = 1 024 000); a tile's origin is an `i32` millimetre multiple of `tile_mm`. Any local coordinate is below 2^24 mm in
  magnitude, so it converts to `f32` exactly.
- **The two pinned conversions (ADR-8084, review W9).**
  - `mm_to_m(origin_mm: i32, local_mm: i32) -> f32` is `((origin_mm as i64 + local_mm as i64) as f32) / 1000.0`: one
    exact integer-to-float conversion (the sum is below 2^24 in magnitude) and one IEEE division. Its result depends
    only on the `i64` sum, so a tile-local vertex and a lattice vertex with the same world millimetre get the same
    `f32` bits. It runs once per vertex at load (and once per vertex an edit changes), never per query.
  - `m_to_mm(m: f32) -> i32` is `((m as f64) * 1000.0).round_ties_even() as i32`: the `f64` product is exact (a
    24-bit significand times 1000 fits in 53 bits), `round_ties_even` is IEEE `roundToIntegralTiesToEven`, and the
    `as i32` cast saturates at `i32::MIN` and `i32::MAX` and maps NaN to 0 (Rust's defined cast). It is the only way
    a sim `f32` becomes cooked or edit millimetres (D1 centres, § 6; heights an edit starts from; the ring pins and
    hanging vertices below). No other rounding (truncation, `f32` multiply, `round`) is allowed on the lockstep path.
  - Pinned property: `m_to_mm(mm_to_m(0, v)) == v` for every `|v| < 2^23` mm (8.3 km). The relative error of the
    division is at most 2^-24, so the round trip is off by less than 0.5 mm there
    (`m_to_mm_inverts_mm_to_m_below_2_23`, W-06, exhaustive over the range, failing first on a truncating version).
  - A map whose world coordinates exceed 2^24 mm (16.7 km from the origin) is refused by the cook
    (`geom_extent_too_large`).
- **Ground lattice (ADR-8084, review W2 and W3).** The sim ground is the heightfield's sample lattice triangulated.
  Its constants are cooked into the canonical section (§ 9.2) and hashed, and are **independent of every render
  setting**:
  - `lattice_n` is the heightfield's sample count per side (`n`).
  - `width_mm` is the map width, `m_to_mm(world_width_m)`.
  - The pitch is `s = width_mm / (lattice_n − 1)`: Switchbacks is 1 280 000 / 4095 ≈ 312.576 mm.
  - `TerrainRelief::cliff_relief` (`crates/regia_terrain/src/gpu/mod.rs:515`) draws a doubled grid with faces
    displaced by up to 8 m near steep nodes. It is a presentation knob that can differ between peers, so the sim
    never reads it. Its doubled grid is nested in this lattice (spec 26 § 2a.4 item 1).
  - **Vertices sit on integer millimetres.** Vertex `(i, j)` is at world `(x_mm(i), z_mm(j))`, with
    `x_mm(i) = −width_mm.div_euclid(2) + (2·i·width_mm + (lattice_n − 1)).div_euclid(2·(lattice_n − 1))` in `i64`
    (that is `i · width_mm / (lattice_n − 1)` rounded half up), and the same for `z_mm(j)`. Its `f32` position is
    `mm_to_m(0, x_mm(i))`. The corners are exactly on the map edge (`x_mm(0) = −width_mm/2` and
    `x_mm(lattice_n − 1) = +width_mm/2` for an even width). The index is **`i / (n − 1)`** everywhere, as the
    terrain's own `world_to_grid` (`crates/regia_terrain/src/sampler.rs:270`). Each vertex is within 0.5 mm of the
    terrain's `f32` sample position `−W/2 + i · W / (n − 1)`.
  - One integer formula serves the ground, the mouth-cut bitmap, the ring vertices of every canonical band and
    collar, D1 edits, the nested levels (a level-`k` vertex is lattice vertex `(2^k·a, 2^k·b)`) and the renderer's
    seam lattice at the rings (spec 26 § 2a.4).
  - Triangles use the terrain's fixed diagonal from `(x0, z1)` to `(x1, z0)`: `[v00, v01, v10]` and
    `[v10, v01, v11]` (`holes.rs:1206`, `cdlod.rs:575-581`).
  - **Point location.** The lattice triangles are tested by § 5.3 rule 2's crossing test: the top-left ownership rule
    with each edge in its canonical direction (from the vertex with the smaller `(x_mm, z_mm)`), exactly like
    canonical triangles. The cell is found from an estimate `i = clamp(floor((x − x(0)) / s_m), 0, n − 2)` (`s_m`
    is the pitch in metres as `f32`). It is then corrected by comparing `x` with the `f32` vertex positions: step down
    while `x < x(i)` and up while `x >= x(i + 1)` (W-10 asserts that this never takes more than one step). The same
    is done for `z`. Both triangles of the cell are tested. When the point lies exactly on the found cell's west or
    south boundary, the triangles of the neighbors across it (three at a corner) are tested too, and exactly one
    triangle owns the point. This replaces the 5fa1f1d69 rule "a point on the diagonal belongs to the first triangle
    (`u + v <= 1`)", so a lattice edge shared with a canonical triangle (a ring edge) is owned once, never twice and
    never by neither.
- **Ground heights.** A lattice vertex stores the heightfield's own 16-bit count (the source quantum). Its height is
  `offset_m + (count as f32) * step_m`, with `offset_m` and `step_m` (`scale / 65535`) fixed `f32` values cooked
  into the canonical section: one multiply, one add, no fused multiply-add. A vertex **override** (a D1 edit, § 6; a
  ring pin, below; a hanging vertex, seam rule 4) replaces that with an absolute `i32` millimetre height, read through
  `mm_to_m`. An edit that starts from a vertex without an override starts from `m_to_mm` of its count height. The
  lattice is never resampled: the 5fa1f1d69 rule that wrote bilinear counts on a lattice finer than the samples
  (cliff relief) is gone (W3).
- **Mouth continuity: the ring pin (ADR-8084, review W2).** The ground and a layer meet at a mouth ring, the boundary
  of the lattice cut (spec 26 § 2a.4 item 2): closed loops of lattice edges, diagonals included. The cook makes the
  two surfaces share those edges bit for bit:
  1. Every ring vertex gets an override: `ring_mm = m_to_mm(count height)`, stored with the ring-vertex set in the
     canonical section (E5's set, now with heights). The ground there moves by at most 0.5 mm. A D1 edit never
     changes a ring pin (E5).
  2. Every canonical triangle vertex on the ring (the band's zipper strip, the collar) is written as world
     `(x_mm(i), ring_mm, z_mm(j))`, stored tile-local. Because `mm_to_m` depends only on the world sum, the layer
     vertex and the lattice vertex get equal `f32` bits in x, y and z.
  3. The band's ring boundary is split at every ring vertex, so each ring edge (axis edge or diagonal) is an edge of
     exactly one canonical triangle and of exactly one uncut lattice triangle. There is no T-junction and no sliver.
     `mouth_cut_mismatch` (§ 10) checks this exactly, on integers.
  4. **At the seam**, `support`, `surface_at`, `floors_at` and `covered_at` cross the shared edge by the ownership
     rule. Every point is in the ground or in the band, never in both and never in neither, so `support` cannot fall
     through to a lower floor at a mouth. `segment` and `line_of_sight` run E3's Möller–Trumbore test (T3's
     arithmetic, kept) on the lattice and canonical triangles over the same bit-equal edges, so the seam has no
     geometric gap. At equal `t` the ground wins over any `TriRef` (as at equal floor height, § 5.3 rule 4).
     Möller–Trumbore's inclusive bounds can still miss both triangles of an edge by rounding, as at every shared
     edge of the canonical mesh. § 13.3 `rays_through_mouth_seam_edges_hit` (W-11) measures this, and a fix is a
     § 14 bit change that needs DO27's yes (amendment 1, conflict C2).
- **Why this convention.** DO27's sim samples `i / n` on a 5 m bilinear grid (`ir-content/src/heightmap.rs:20`,
  `:219`) while its drawn grid samples `i / (n − 1)`. On a 4096² map over 1280 m the origins differ by half a texel,
  0.156 m, which leaves a lip or a bridge at every mouth (handover finding 9). Regia's terrain, holes and seam lattice
  already use `i / (n − 1)`; the sim adopts it, so the ring the renderer draws and the floor the sim stands on are the
  same lattice edges, with the same integer-millimetre vertices (the ring pin above).
- **Ground tiles (ADR-8084, review W4).** A *ground tile* is a square of `ground_tile_cells` lattice cells per side
  (default **64**, a power of two from 16 to 256, cooked and hashed). With `g = ground_tile_cells`, ground tile `(a, b)`
  covers cells `[g·a, min(g·a + g, n − 1))` in each axis, so the last tile of a side may be partial. The ground tile is the unit
  of the nested level, the mouth-cut bitmap, the override blocks of `GeomState` (§ 5.2), the height pyramid (§ 5.5)
  and the index rebuild after an edit. It is not the 1024 m *cooked tile* that stores canonical triangles. At
  Switchbacks' pitch a ground tile is 20 m; a 4096² lattice has 64 × 64 of them. Earlier text that said "fine tile"
  means a ground tile.
- **Memory and the ground mode (A-7; ADR-8084, review W4).** The full-pitch lattice costs `2 · n²` bytes
  (Switchbacks: 4096² → 33.5 MB). The sim-geometry budget of § 11 is 35 MB on maps up to 2.56 km and 64 MB beyond (34 MB until ADR-8396).
  - The **ground mode is authored**: `MeshLayersDocument::ground` (§ 9.1) is `GroundMode::Full` (the default) or
    `GroundMode::Nested { max_level }`. It is cooked into the canonical section and hashed (§ 9.2).
  - The cook **never switches modes by itself**. A `Full` map over the budget fails with `ground_over_budget` and
    names the mode that would fit (`Nested { max_level: k }`, the smallest `k` that fits). A `Nested` map over the
    budget fails the same way and lists the tiles (seam rule 2). A content change (about 300 KB of new layer
    triangles on Switchbacks, which clears the budget by about 0.3 MB) is therefore a cook error to act on, never a
    silent replacement of the ground everywhere.
  - In `Nested`, each ground tile takes its own nested CDLOD level `0..=max_level` (pitch `s · 2^level`; level 0 is
    full pitch) under the seam rule below. A tile whose side, in cells, is not a multiple of `2^level` cannot take
    that level. The difference to the LOD-0 surface is bounded by that rule and by § 10 `ground_vs_render`.
  - The level of every tile is recorded in the canonical section (§ 9.2) and reported by `terrain.bake_report`.
  - Washboard (6561²: 86 MB; no tunnels in round 1) and any 2.56 km map at 0.3125 m (8192²: 134 MB) must be authored
    `Nested`. A full-pitch 4096² map fits `Full` only while its triangles, pins, cut bitmaps and index stay under 1.4 MB (§ 11; Cinder Steps measured 0.56 MB, ADR-8396).
  - The review's alternative, one level `k` for the whole map ("the finest `k` that fits"), is superseded by E1's
    per-tile, error-driven levels. The authored `max_level` is the only cap (amendment 1, conflict C1).
- **Nested-pitch seam rule (DO27 sign-off E1, ADR-8072).** Nesting alone does not join two tiles: a fine tile next to
  a coarse tile has lattice vertices on the shared border that the coarse tile lacks, so the ground would step there by
  the full LOD error. The cook therefore applies, in this order:
  1. **Level per tile, chosen by error.** A tile takes the coarsest level whose ground stays within `pitch_error_m`
     (default **0.05 m**, DO27's hop gate and how far a pilot's feet may sink into drawn ground) of the full-pitch
     ground everywhere in the tile. Flat tiles go coarse, rough tiles stay fine.
  2. **Over budget is a cook error** (`ground_over_budget`), listing the tiles that would need a coarser level than
     rule 1 (or the authored `max_level`) allows. The cook never picks a silently coarser level and never changes the
     authored mode (ADR-8084).
  3. **Neighbors differ by at most one level** (CDLOD's own rule; the cook refines a tile when a neighbor is two
     levels finer).
  4. **Hanging vertices take the coarse edge (the stitch).** A border vertex of the finer tile that the coarser tile
     lacks gets the midpoint height of the coarser edge. It is computed once at cook, as `m_to_mm` of the coarse
     triangle's plane height (§ 5.3 rule 2's arithmetic) at the vertex, and stored as an absolute-mm vertex override
     (the D1 mechanism, through `mm_to_m`). In XZ there is no crack: a tile border is an axis line, so the coarse
     edge and the two fine edges lie on one line (equal `x_mm` or `z_mm`), and the ownership rule gives every point
     on it to exactly one side. In height, the step across a pitch border is at most **1 mm**: 0.5 mm from the mm
     rounding of the stored override, plus `f32` rounding of the plane heights. The 5fa1f1d69 revision and E1 said
     1e-4 m, which a millimetre override cannot reach (ADR-8084, amendment 1, conflict C3). By rule 1 this moves the
     finer tile by at most `pitch_error_m` over one cell, so the per-tick change a seam causes is inside the hop gate
     by construction.
  5. **No pitch border within `fine_band_m`** (default 16 m) of a mouth ring or a layer's XZ footprint: the cook keeps
     at full pitch every tile that touches the band, and every tile whose border with a coarser tile would lie inside
     it, until no such border is left. 16 m covers the largest hull (4.5 m radius), the tilt
     samples (3 m) and one tick of travel (under 1 m).
  6. **A D1 edit promotes its tiles to full pitch** (§ 6) and re-derives the hanging vertices of their neighbors, in a
     fixed order (ascending tile index, then ascending vertex index). `GroundEdit::Rect` always indexes the full-pitch
     lattice, whatever the tile's cooked level.
  7. **Checks, each failing first:** cook check `ground_pitch_seam` (§ 10: rules 1, 3 and 4) and the § 13.3 test
     `pitch_seam_hop_at_full_speed_le_5cm_per_tick` on a Washboard-sized fixture.

  DO27's note (no engine change): the sim ground goes from a smooth 5 m grid to 0.31 m detail on every map, so hover
  feel moves everywhere; DO27 re-checks handling parity at the Stage B bump.
- **As built (lane W-10, ADR-8214).** `regia_world_geom::ground` (the lattice, heights, point location, nested tiles),
  `cut` (mouths of a `Passages` layer as named holes from its open-cut segments, cut only for holes the layer `fills`;
  the rings; the ring pins; the collar: every cut lattice triangle on a ring as a canonical floor triangle) and
  `pitch` (levels, seam rules 1-5, the budget, `ground_pitch_seam`). Tie rules, the footprint, the collar and the
  budget formula are in ADR-8214.
- **As built (lane W-14c, ADR-8533).** A filled mouth whose skylight climb K8 opened past the mouth's cut gets the
  approach strip of ADR-8396 on the pins (flat), and the mesher's threshold sill at that run's start carries nothing.
  `mouth_holes_match_the_render_mask` checks item 3 of the ring pin above (every ring edge is an edge of exactly one
  canonical triangle) in place of W-10's every-ring-cell collar; a ring edge no band holds always keeps its collar.

## 4. Stage A: the compat module (lanes W-01 to W-05)

Stage A copies DO27's tunnel sim, layout, loaders and proofs into `regia_world_geom::compat`, under ADR-8012 A-1 and
the handover § 7 manifest marks. In Stage A even an *adapt* row is copied with **unchanged arithmetic**: the only edits
are the decoupling seams below, identifier and comment hygiene (DO27 names dropped, A-1), and imports. The pins of
§ 13.1 must pass unchanged. Every copied file gets its `docs/ported-sources.toml` `[[do27]]` row, its
`docs/provenance.md` `do27-owner-code` row and its header line (A-1).

**Source pin (DO27 sign-off E2, ADR-8072).** Stage A copies from two DO27 tags, never from a branch head:

| Modules | Copied from | Why |
|---|---|---|
| `compat::layout`, `compat::sense`, `compat::pack` | `regia-absorb-1` (d6ed37677; the cited files are unchanged from f7e8f4740) | TUNNELS_T3 does not touch them |
| `compat::mesh`, `compat::ground`, `compat::sampler`, `compat::state`, `compat::hover_ground`, `compat::onfoot` | **`regia-absorb-2`**, the tag DO27's orchestrator cuts on the T3 merge commit (DO27 protocol `ironreach/1.58`); never `regia-absorb-1` | T3 rewrites exactly these files (`git diff --stat f7e8f4740 6fd2effb`: `ir-content/src/tunnels.rs` 347 lines, `ir-sim/src/tunnels.rs` 427, `heightmap.rs` 63, `hover_ground.rs` 43) and adds `ir-content/src/surface.rs`. A compat module cut from f7e8f4740 would put back the 0.30-0.32 m mouth hops if DO27 swapped to it, and DO27's 1.58 pins would not match |

Until `regia-absorb-2` exists, the lanes that copy the second row (W-01's mesh and sampler half, W-02, W-03) wait;
W-01's layout and pack half continues from `regia-absorb-1`. The line ranges and item lists of § 4.1 for the second
row are those of f7e8f4740 and are **re-cited by the copying lane against `regia-absorb-2`**; the T3 changes already
known are listed under § 4.1. The T3 reference DO27 cited is `lane/tunnels_t3` @ 6fd2effb, not frozen until the tag.

### 4.1 Module map

| Module | Source (manifest mark) | Public items (names kept; they are the switch contract) |
|---|---|---|
| `compat::layout` | `ir-content/src/tunnel_layout.rs` 1-1423 (as-is; the stub clip `on_other_floor` 711-728 and `side_wall` 800 are mesh-pin bytes) | `TunnelLayout`, `Passage`, `PassageKind`, `Opening`, `Junction`, `CrawlwayVolume`, `Sample`, `SampledPassage`, `SampledLayout`, `MouthHole`; `TunnelLayout::sample` (436), `SampledLayout::mesh` (559, 688), `export_json` (652), `triangles` (941); constants `LAYOUT_VERSION`, `SAMPLE_STEP_M`, `ROCK_COVER_M`, `PORTAL_LENGTH_M`, `FLUSH_M`, `HEADER_EXTRA_M`, `WALL_CLIP_INSET_M`, `LIP_W_M`, `LIP_OVERLAP_M`, `LIP_PROUD_M`, `HOLE_END_M`, `MAX_HOLE_SHAPES`; the export tag constant `EXPORT_FORMAT` = `"do27-tunnels/1"` (a foreign format name, kept as data) |
| `compat::mesh` | `ir-content/src/tunnels.rs` 1-425 and 605-1323 at `regia-absorb-2` (as-is; W-02 re-synced W-01's regia-absorb-1 copy) | `TunnelTriangle`, `TunnelFaceKind`, `TunnelCollisionMesh` (`new`, `welded` (157), `with_layer` (166), `support_floors` (173), `is_empty`, `bounds`, `contains_hint`, `sample_floor_at`, `floors_at`, `floor_near`, `sample_floor_near`, `ceiling_y_at`, `is_portal_at`, `ceiling_above`, `ceiling_count_at`, `clearance_above`, `segment_blocker` (359)), `MeshLayers` (58), `clamp_altitude_under_ceiling` (571), `separate_disc_from_walls` (605), `separate_disc_from_walls_band` (617), `TunnelNavOverlay` (751: `bake`, `next_waypoint`, `next_waypoint_for`, `depth_at`), constant `MAX_NAV_LEVELS`; the synthetic meshes (`synthetic_gallery`, `synthetic_four_decks`, `synthetic_building_decks`) behind `#[cfg(any(test, feature = "fixtures"))]`. Gone at T3: `PORTAL_HYSTERESIS_M`, `DECK_SNAP_M` |
| `compat::ground` | `ir-content/src/tunnels.rs` 426-570 at `regia-absorb-2` (adapt; Stage A unchanged) | `SampleFloorTunnels` (432; no `previous`), `GroundSupport` (442), `HULL_FIT_M` (452), `STEP_BOUND_MM` (459, = 50), `step_ok` (463), `HoleDisc` (473), `HoleMask` (481), `query_ground` (505: `surface::column(..).span_at(y)`), `sample_floor_with_tunnels` (522; no `sticky`). Gone at T3: `SUPPORT_RISE_M` |
| `compat::sampler` | `ir-content/src/heightmap.rs` 1-478 at `regia-absorb-2` (the `TerrainSampler` the query calls, with `HeightGrid`; 268-329 the floor calls) and the `terrain_patch` module it imports (as-is, unchanged since regia-absorb-1) | `HeightGrid`, `TerrainSampler` (`shaft_at`, new at T3), `FloorSample`, `FloorSurface`, `HEIGHT_METERS_PER_COUNT`, `SIM_CELL_M` (the 5 m `i / n` grid: Stage A only), `sample_floor` (`sticky` no longer read). Gone at T3: `skylight_height_at`, `sample_floor_ext` |
| `compat::pack` | `ir-content/src/map_pack.rs` 548-617, 976-1147, 1150-1156 and `ir-core/src/core.rs` 2423-2431 (as-is) | `TunnelsFile` (`collision_mesh`), `parse_tunnels`, `load_tunnels_file`, `tunnel_mouth_issues`, `baked_holes`, `load_tunnels` (the baked file when it parses, else the spline description) |
| `compat::state` | `ir-sim/src/tunnels.rs` 24-424 at `regia-absorb-2` (as-is; `camera_floor` 291, `mesh_spans` 304, `camera_clip` 318: adapt, Stage A unchanged, W-26 moves them out; `bake_navmesh` 161-186 and `navmesh_ground` 426-463 are the `TunnelNavBake` seam, not copied); `ir-sim/src/state.rs` 10103-10113 (as-is) | `TunnelsState<N = ()>` (`disabled`, `with_mesh`, `for_layout` (126), `from_layout` (131), `from_baked` (149), the `*_with_nav` forms, `support` (195), `body_support` (201), `move_floor` (207), `place_floor` (213), `nearest_floor` (225), `confine` (242), `underground` (260), `below_ground_at` (275), `camera_floor`, `mesh_spans`, `camera_clip`, `rock_between` (335), `rock_hidden`, `rock_hidden_lit` (361), `separate_from_walls`, `clamp_altitude`, `next_waypoint`, `next_waypoint_for`, `depth_at`), `TunnelNavBake`, `NoNavBake`, `MapTunnels`, `tunnels_arm_active`, `skylight_prop_top` (71), `floor_with_skylight_prop` (80), `TUNNELS_DEFAULT`, `UNDERGROUND_M`, `CAMERA_FLOOR_REACH_M`, `ROCK_RADAR_M`, `UNIT_COLLISION_RADIUS_M`, `PILOT_STEP_M`, `BODY_REACH_M` (re-export), `sample_place_floor`, `nav_floor_y` |
| `compat::sense` | `ir-sim/src/tunnel_sense.rs` 1-106 at `regia-absorb-2` (as-is; unchanged since regia-absorb-1) | `hear_range_m`, `satellite_shows`, `can_salvage`, `InsideStructure`, `may_build_inside`, `UNDERGROUND_HEAR_MULT` |
| `compat::hover_ground` | `ir-core/src/hover_ground.rs` 1-214 at `regia-absorb-2` (adapt; Stage A unchanged), `ir-sim/src/attitude.rs` 108-111 (`ground_tilt`) and `ir-sim/src/ruins.rs` 79-100 (the fixture crater book) | `sample_move_floor`, `ground_hover_hull`, `ground_tilt_under`, `ground_tilt_at`, `ground_tilt`, `HoverGroundBody`, `GroundDelta`, `NoGroundDelta`, `TiltParams`, `LEDGE_M`, `HULL_WALL_M`; `CraterBook` (`crater`, `height_delta`) and `Crater` behind `#[cfg(any(test, feature = "fixtures"))]` |
| `compat::surface` (T3) | `ir-content/src/surface.rs` 1-571 at `regia-absorb-2` (as-is; kernel 1-368, nine tests 370-571) | `Span`, `Column` (`spans`, `span_at` (122), `highest_mesh_span` (160)), `column` (170), `surface_under` (273), `ceiling_over` (278), `MAX_SPANS` (12), `BODY_REACH_M` (0.9), `WELD_M` (0.15), `TRIM_M` (0.5), `TRIM_WIDTH_M` (2.0); crate-private `support_floors` (288), `weld_to_terrain`, `MAX_CEILINGS` (16), `EPS_M` (1e-3), `LID_GAP_M` (0.05) |
| `compat::onfoot` (W-03) | `ir-sim/src/pilot_collide.rs` 1-930 (tests 931-1347), `pilot_mesh.rs` 1-578 (tests 579-747), `state.rs` 14246-14305 (`pilot_floor_y`) at `regia-absorb-2` (adapt; Stage A unchanged; re-cited by W-03 from the planner's 1-1346 / 1-720 / 14262-14319) | `PilotCell` (`add_wall` returns the span-cap hit), `WallSpan`, `PilotGrid` (`stamp_floor`, `stamp_wall`, `stamp_floor_rect`, `stamp_wall_box`, `stamp_triangle` with the T3 0.30 m floor rule, `wall_hit_along`, `slide`, `clear_door_mouth`, `support`, and the new diagnostic `span_cap_hits`), `DoorCut`, `PilotSolids` (`world_floor_y`), `Blocker`, `accept_support`, `building_key` (opaque `variant`), `cell_index`, `world_to_local`, `local_to_world`, `door_local` (host socket or the rim notch), `push_out_of_wall_ring`, `slide_move`, `resolve_feet`, `pilot_floor_y`; seams `OnFootParams`, `OnFootSolids`; constants `PILOT_STEP_M` (re-export of `compat::state`'s), `PILOT_SKIN_M`, `PILOT_STEP_DOWN_M`, `BUILDING_WALL_M`, `CELL_M` (0.75), `FLOOR_MATCH_M` (0.30), `MAX_WALL_SPANS` (4), `SPAN_JOIN_M` (0.10), `LIP_THICK_M`, `LIP_REACH_M`. `compat::onfoot::mesh`: `GltfGridSource`, `GltfFile`, `GltfMemory`, `load`, `load_with`, `load_gltf_grid`, `load_gltf_grid_on`, `merge_grid`, `usable_gltf`, `gltfs_in`. Not copied (the host's): `scrap_blocker`, the building mesh table, the map directory walk, the rock props |

**Known changes at T3 (DO27 sign-off E2; re-cited at `regia-absorb-2`).** These rows of the table above change when
the copy moves to the T3 tag:

- `ir-content/src/surface.rs` (the column kernel, 570 lines at 6fd2effb) joins the copy, *as-is*, in `compat::mesh`
  (W-01); `compat::ground` gains its callers.
- `sample_move_floor(terrain, tunnels, delta, x, z, body_y)` has no `prev_floor` argument (§ 4.3).
- `TunnelsState::support`, `body_support`, `move_floor`, `place_floor`, `nearest_floor` and `mesh_spans` replace the
  history forms in `compat::state`.
- Gone at T3 (the table above still lists the f7e8f4740 items; the copy at `regia-absorb-2` drops these): `sample_floor_ext`, `skylight_height_at`,
  `SUPPORT_RISE_M`, `PORTAL_HYSTERESIS_M`, `DECK_SNAP_M`.
- `GroundDelta` is added only when the floor is the heightfield, never on a mesh floor (the copied arithmetic decides;
  the seam does not change).
- The `HoverGroundBody` field set is the same at T3 (§ 4.3).
- `ir_sim::attitude::ground_tilt` (`attitude.rs:108-111`, two `math::atan2` calls through the `regia_detsim::math`
  seam) is copied into `compat::hover_ground` with the file.

Not in this crate: the tunnel half of DO27's navmesh (`pathing/layer.rs`, the `navmesh.rs` tunnel lines and the
tunnel proofs of `pathing/tests.rs` 592-1238) goes to `regia_nav::det` with W-04, on S-06's navmesh. W-04 landed it as `regia_nav::det::tunnel_bake` (`TunnelNavMeshBake`, the `TunnelNavBake` for `NavMesh`; `NavTunnelsState` = `TunnelsState<NavMesh>`; ADR-8215), with the proofs in `crates/regia_nav/tests/det_tunnel_nav.rs`. The map layouts
`atacama_rail.rs` and `maps/cinder_steps.rs` become **test fixtures** (`tests/fixtures/`), not engine API:
`compat::layout::for_map` is replaced by `TunnelLayout` values the fixtures build. `ir-content/tests/map_load.rs` is
requirements only (its three bug tests become W-09 cook fixtures).

### 4.2 Seams (the only Stage A edits)

| DO27 dependency | Stage A seam | Why it is arithmetic-neutral |
|---|---|---|
| `ir_sim::math::sqrt` and the other `ir_sim::math` calls (`sin` and `cos` in `hover_ground.rs`, `atan2` in the copied `ground_tilt`) | `regia_detsim::math` (F-01: `libm = "=0.2.16"`, DO27's bits) | same functions, same pinned version |
| `ir_sim::handling::CUSHION_TILT_SPAN_M` (3.0), `CUSHION_TILT_MAX` (0.61), `CUSHION_TILT_LEDGE_M` (2.5) (`handling.rs:575-579`), read by the tilt pass (E2) | a `TiltParams` argument (§ 4.3); the values are DO27 handling tuning and never engine constants | the same three `f32` values, read in the same places |
| `ir_sim::attitude::ground_tilt` (`attitude.rs:108-111`) (E2) | copied into `compat::hover_ground` with the file (its two `atan2` calls go through `regia_detsim::math`) | the same function body |
| `ir_sim::state::RuntimeEntity` in `ground_hover_hull` and `ground_tilt_under` | `pub trait HoverGroundBody` (§ 4.3) | the function reads and writes the same fields in the same order through accessors |
| `ir_sim::ruins::RuinBook` in `sample_move_floor` | `pub trait GroundDelta { fn height_delta(&self, x: f32, z: f32) -> f32; }`; the crater book implements it | the same `y + delta` add (W-12 removes it in Stage B) |
| `crate::pathing::navmesh::NavMesh` and `BakeParts` in `TunnelsState::from_layout` / `from_baked` | `pub trait TunnelNavBake { type Mesh; fn bake(&self, sampled: &SampledLayout, terrain: Option<&TerrainSampler>) -> Option<Arc<Self::Mesh>>; }`; `TunnelsState<N = ()>` holds `navmesh: Option<Arc<N>>`; `regia_nav::det` implements it (W-04), and `navmesh_ground` (the ground raster that bake reads, `ir-sim/src/tunnels.rs` to 436) moves with it | the bake runs the same code in the nav crate; the collision mesh does not depend on it |
| `crate::tunnel_light::ROCK_RADAR_LIT_M` in `rock_hidden_lit` | a `rock_radar_lit_m: f32` argument | the same comparison with the caller's constant |
| `crate::state::UNIT_COLLISION_RADIUS` (2.5) in `confine` | `UNIT_COLLISION_RADIUS_M: f32 = 2.5` in `compat::state` | the same value |
| `ir_content::tunnel_layout::for_map(map_id)` in `TunnelsState::for_map` | `TunnelsState::for_layout(layout: TunnelLayout, terrain)` (the map registry is the host's) | the same `from_layout` call |
| `crate::human_scale::{DOOR_WIDTH_M, PILOT_CAPSULE_RADIUS_M, PILOT_HEIGHT_M}` and `crate::state::BUILDING_COLLISION_RADIUS` in `pilot_collide.rs` (W-03) | an `OnFootParams { capsule_radius_m, height_m, door_width_m, building_radius_m }` argument with no default (the test fixture `compat::fixtures::ON_FOOT_PARAMS` holds 0.35, 1.85, 1.10, 9.0) | the same `f32` values, read in the same places |
| `building_key(stem, area52)` and `door_local(stem, area52)` (W-03; the faction-keyed art stems and DO27's door socket table) | `building_key(stem, variant: bool)` (the second art set is keyed `stem@alt`); `door_local(socket: Option<[f32; 2]>, params)`: the host passes its socket, `None` gives the rim notch `[0, -building_radius_m]` | the key is a map key only (never arithmetic); the socket values and the notch are the same numbers |
| `scrap_blocker` and `crate::economy_e1::SIZE_*`, `crate::training_stage::solid_blockers`, `crate::pilot_rocks::PilotRocks` (the `PilotSolids::rocks` field) and the `runtime_entities` loops of `pilot_floor_y` (W-03) | `pub trait OnFootSolids { fn pilot_solids(&self) -> &PilotSolids; fn for_each_building(&self, f: &mut dyn FnMut(&str, [f32; 3], f32)); fn for_each_loose_blocker(&self, f: &mut dyn FnMut(Blocker)); fn for_each_fixed_blocker(&self, f: &mut dyn FnMut(Blocker)); fn rock_push_out(&self, x: f32, z: f32, feet_y: f32, radius: f32) -> [f32; 2]; }`: the host's live buildings (art key, centre, yaw; the source's live/health/faction tests stay the host's), its loose pieces (a `force_block` box is skipped, as the source's scrap loop does) and its fixed stage boxes (all accepted), in its fixed entity order; `PilotSolids` keeps `world` and `buildings` | the same `accept_support` calls on the same tops; the host iterates the same entities in the same order |
| `BUILDING_MESHES` (the art paths), `load(root, map_id, ..)` and its `assets_regia/maps/<map>/setpieces` walk (W-03) | `mesh::load(params, set_pieces, buildings, terrain, tunnels)`: the host lists its set-piece sources and its `(key, source)` building sources (`gltfs_in` and `usable_gltf` help with the walk) | the same per-file grids, merged in the same list order |
| `std::fs` reads in `pilot_mesh.rs` (`read_to_string`, `File::open` + seek + `read_exact`) (W-03) | `pub trait GltfGridSource: Sync { fn label(&self) -> String; fn gltf_text(&self) -> Result<String, String>; fn buffer_range(&self, uri: &str, start: u64, len: usize) -> Result<Vec<u8>, String>; }` with `GltfFile` (disk) and `GltfMemory` (in memory) | the same bytes into the same parse; load time only, no query path reads a file |
| `PilotCell::add_wall` drops a fifth span silently (W-03, plan acceptance 1) | `add_wall` returns `true` when the cap merge ran; `PilotGrid::span_cap_hits` counts it (also through `merge_grid`) and `mesh::load_with` logs the total | a counter beside the copied merge; no query reads it |
| `serde_json` parse of `do27-tunnels/1` (`f32` narrowed from the parser's `f64`) | kept, with a pinned precondition (ADR-8084, review W10). The two builds parse float text with **different algorithms**: Regia's workspace sets `serde_json`'s `float_roundtrip` feature (`Cargo.toml:122`), DO27 builds plain `serde_json` (DO27 `Cargo.toml:16` at f7e8f4740), and once DO27 links Regia crates its own build unifies to `float_roundtrip`. DO27's proof (0 of 1,844,875 float tokens differ after the cast) compared `f64` with `f32` parsing, not the two algorithms, so it is not the justification. The justification is two W-01 pins: (1) `do27_tunnels_tokens_parse_to_the_committed_f32_bits`, which parses every float token of every fixture description in Regia's build and compares the `f32` bits with a table DO27's build wrote at the source tag (committed with the fixtures; DO27 reruns the same table in its build at the swap); (2) `switchbacks_mesh_is_byte_identical_from_the_baked_description`, extended to **every** fixture map (Switchbacks, The Cinder Steps layout, the gallery). `compat::pack` is on the `det-scan` allow list with that reason. Stage B retires the path: the importer parses at cook time and the canonical section holds `i32` mm, so a parse difference between cook hosts changes `content_hash` and fails the handshake instead of desyncing | the same `f32` bits, proven per token and per mesh on the fixtures; load-time only, never per tick |

### 4.3 Seam signatures

```rust
/// What `ground_hover_hull` reads and writes on a body (Stage A seam for DO27's runtime entity).
/// Accessors only; the call order of the copied function is unchanged.
pub trait HoverGroundBody {
    /// True for a body on foot (no wall rule, no tilt).
    fn on_foot(&self) -> bool;
    /// World position, metres.
    fn position(&self) -> [f32; 3];
    /// Sets the world position.
    fn set_position(&mut self, p: [f32; 3]);
    /// Hover altitude over the floor, metres.
    fn altitude(&self) -> f32;
    /// Sets the altitude.
    fn set_altitude(&mut self, a: f32);
    /// Sets the horizontal velocity components (x and z) to 0 and leaves the vertical one; the copied function only
    /// ever writes 0 there and never reads velocity (DO27 sign-off E2 replaced the `velocity` / `set_velocity` pair).
    fn stop_horizontal(&mut self);
    /// Vertical velocity of the hover spring.
    fn vertical_vel(&self) -> f32;
    /// Sets the vertical velocity.
    fn set_vertical_vel(&mut self, v: f32);
    /// Sets the floor the spring measured last.
    fn set_prev_terrain(&mut self, y: Option<f32>);
    /// Heading, radians.
    fn yaw(&self) -> f32;
    /// Sets the ground pitch and roll under the hull, radians.
    fn set_ground_tilt(&mut self, pitch: f32, roll: f32);
}

/// A height added to the floor at (x, z) (Stage A: the crater book; removed in Stage B by W-12).
pub trait GroundDelta {
    /// The added height, metres.
    fn height_delta(&self, x: f32, z: f32) -> f32;
}

/// Bakes the tunnel navmesh beside the collision mesh (implemented by `regia_nav::det`, W-04).
pub trait TunnelNavBake {
    /// The navmesh type.
    type Mesh;
    /// Bakes it, or `None` without a terrain (no surface to link to).
    fn bake(&self, sampled: &SampledLayout, terrain: Option<&TerrainSampler>) -> Option<Arc<Self::Mesh>>;
}

/// The host's cushion-tilt tuning the tilt pass reads (DO27 sign-off E2: DO27 handling values, passed in, never
/// frozen in the engine). DO27 passes `CUSHION_TILT_SPAN_M` 3.0, `CUSHION_TILT_MAX` 0.61, `CUSHION_TILT_LEDGE_M` 2.5.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TiltParams {
    /// DO27 `CUSHION_TILT_SPAN_M`: the tilt sample span, metres (read where the copied pass reads the constant).
    pub span_m: f32,
    /// DO27 `CUSHION_TILT_MAX`: the tilt limit (read where the copied pass reads the constant).
    pub max: f32,
    /// DO27 `CUSHION_TILT_LEDGE_M`: the tilt pass's ledge distance, metres (read where the copied pass reads it).
    pub ledge_m: f32,
}

// At `regia-absorb-2` (T3): no `prev_floor` argument (E2).
pub fn sample_move_floor(terrain: Option<&TerrainSampler>, tunnels: &TunnelsState<impl Sized>,
    delta: &impl GroundDelta, x: f32, z: f32, body_y: f32) -> f32;
pub fn ground_hover_hull(body: &mut impl HoverGroundBody, last_pos: Option<[f32; 3]>,
    terrain: Option<&TerrainSampler>, tunnels: &TunnelsState<impl Sized>, delta: &impl GroundDelta,
    tilt: &TiltParams) -> f32;
// `ground_tilt_under` and `ground_tilt_at` take `tilt: &TiltParams` the same way, as their last argument.
```

The `HoverGroundBody` field set is signed off (§ 16, Q4): DO27 checked it against every field `hover_ground.rs`
touches at f7e8f4740 (unchanged through d6ed37677 and at T3), with no field missing:

| Body field touched | Access | Trait |
|---|---|---|
| `is_pilot`, `object_stem` | read | `on_foot` |
| `components.transform.position` | read, write | `position`, `set_position` |
| `hover_state.altitude` | read, write | `altitude`, `set_altitude` |
| `hover_state.velocity[0]`, `[2]` | write only (set to 0) | `stop_horizontal` |
| `hover_state.vertical_vel` | read, write | `vertical_vel`, `set_vertical_vel` |
| `hover_state.prev_terrain` | write only | `set_prev_terrain` |
| `hover_state.yaw` | read | `yaw` |
| `hover_state.ground_pitch`, `ground_roll` | write only | `set_ground_tilt` |

The file also reads three handling constants and calls `ground_tilt` and `sin`/`cos`; those are the § 4.2 seams
(`TiltParams`, the copied `ground_tilt`, `regia_detsim::math`), not body fields. The copying lane (W-02) re-checks the
table and the `TiltParams` call sites against `regia-absorb-2` and records any difference here before it copies.

Re-checked by W-02 at `regia-absorb-2` (ADR-8167): `hover_ground.rs` 1-162 touches exactly the fields of the table above, and the tilt pass reads `CUSHION_TILT_SPAN_M`, `CUSHION_TILT_MAX` and `CUSHION_TILT_LEDGE_M` only in `ground_tilt_at` (`handling.rs` 575-579, unchanged). No difference. One seam note: the source's `math::atan2` takes signed-zero inputs as `+0` and returns the canonical NaN, while `regia_detsim::math::atan2` is the bare libm `atan2f`; the copied `atan2` sites call `compat::source_math::atan2`, which adds those two steps.

### 4.4 What Stage A gives DO27

DO27 swaps `ir-content`'s tunnel modules, `ir-sim/src/tunnels.rs`, `tunnel_sense.rs` and `hover_ground.rs` for the
Regia crate on a branch, runs its DO27-only pins (§ 13.1) and deletes its copies when they match (handover § 6 item 12).
Stage A changes no checksum. It fixes none of the known issues; Stage B does (§ 13.3).

## 5. Stage B: the native core (lanes W-06 to W-13, W-20, W-29, W-30)

### 5.1 Types

```rust
/// The cooked, immutable world geometry of one map: the canonical triangles in world f32 metres (converted once
/// from i32 mm, § 3), the ground lattice, the per-tile query index, sections, portals, zones and blocker plugs.
/// Built by `from_cooked`; shared between threads (`Send + Sync`); never mutated after load.
pub struct WorldGeom { /* private */ }

/// The mutable sim state over a `WorldGeom`: blocker bits, the D1 vertex overrides (the result of the edits, never
/// their history; E4), the tiles a D1 edit promoted to full pitch (E1), the running state hash and the generation.
/// Bounded by the lattice size, cloneable, hashed into the sim checksum (`state_hash`, O(1)). One per sim (and one
/// per predicting client). The overrides live in one block per ground tile behind an `Arc`: a clone shares every
/// block, and the first write to a shared block copies that block only (copy-on-write), so a per-tick clone costs one
/// pointer per touched ground tile however many craters the match has (ADR-8084, review W8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeomState { /* private */ }

/// Settings with DO27's values as defaults (ADR-8012 A-1: caps become settings). Cooked into the canonical section
/// of the file (§ 9.2), so `content_hash` covers them: a peer with other settings has another content hash and fails
/// the match handshake (DO27 sign-off E6). The cook refuses a non-finite value and writes `-0.0` as `+0.0`, so equal
/// settings always have equal bytes (ADR-8084).
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GeomSettings {
    /// Query index cell, metres (DO27: 16.0). A performance setting, never a sim bit: results are independent of it
    /// (§ 5.3 rule 1; the query table is pinned at 4 m and at 16 m). 16 stands until W-31 measures.
    pub index_cell_m: f32,
    /// Wall triangles are filed this much wider so a disc of up to this radius finds every wall (DO27: 8.0). Also the
    /// largest `push_disc` radius (§ 5.4).
    pub wall_pad_m: f32,
    /// Floors closer than this are one floor: the first in candidate order stands, and at equal height the ground
    /// wins over a layer floor (T3; DO27: 1e-3).
    pub same_height_m: f32,
    /// A span's ceiling is the lowest lid more than this over its floor (T3; DO27: 0.05).
    pub lid_gap_m: f32,
    /// A layer span with less clearance than this (and a lid) carries no placed body (DO27 `HULL_FIT_M`: 2.5).
    pub hull_fit_m: f32,
    /// Largest per-tick floor step the query may report at full speed, millimetres (the Stage B acceptance bar,
    /// § 13.3; default 50, DO27 sign-off E3).
    pub step_bound_mm: i32,
    /// Floors kept per column in `floors_at` (DO27 `MAX_NAV_LEVELS`: 4; The Cinder Steps round 2 needs 5; default 8).
    pub max_floors: u8,
    /// Minimum rock kept over a layer roof by a D1 edit, millimetres (default 2000).
    pub min_cover_mm: i32,
    /// Nested-pitch fallback: the most a coarser tile's ground may differ from the full-pitch ground, metres
    /// (§ 3 seam rule 1; DO27's hop gate: 0.05).
    pub pitch_error_m: f32,
    /// Nested-pitch fallback: no pitch border within this distance of a mouth ring or a layer footprint, metres
    /// (§ 3 seam rule 5; default 16.0).
    pub fine_band_m: f32,
}

/// A triangle reference: the layer and the triangle's index in that layer's canonical (source) order. Total order
/// = candidate order. The cooked tile a triangle is stored in is not part of it, so no answer depends on the tiling
/// (DO27 sign-off E3, Q3 and Q5 row 6: tile size and `index_cell_m` are performance settings, not sim bits).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TriRef { pub layer: u16, pub triangle: u32 }

/// What a surface is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurfaceKind {
    /// The ground lattice (cut by open mouths).
    Ground,
    /// An up-facing layer triangle (a floor).
    Floor,
    /// A down-facing layer triangle (a roof).
    Roof,
    /// A near-vertical layer triangle (a wall).
    Wall,
    /// A blocker plug triangle (only while its blocker is active).
    Blocker,
}

/// One surface the vertical line through (x, z) crosses.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SurfaceHit {
    /// Height, metres.
    pub y: f32,
    /// Unit normal of the crossed triangle (normalized with IEEE `f32::sqrt`, flipped to face up; § 15 item 3).
    pub normal: [f32; 3],
    /// Ground, floor, roof or blocker.
    pub kind: SurfaceKind,
    /// The triangle (`None` for the ground).
    pub tri: Option<TriRef>,
    /// The section the triangle belongs to (`None` for the ground).
    pub section: Option<SectionId>,
    /// Footstep material slot of the source (0 for the ground; the terrain's layer weights are presentation).
    pub material: u16,
}

/// One free span of a column: a floor and the lid over it (T3's `Span`, with the hits). Also `support`'s answer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Span {
    /// The floor of the span (the ground or an up-facing layer or blocker triangle).
    pub floor: SurfaceHit,
    /// The lowest lid more than `lid_gap_m` over `floor` (`None`: open to the sky). Covered is `ceiling.is_some()`.
    pub ceiling: Option<SurfaceHit>,
}

/// `support`'s answer is the span the query point is in (§ 5.3 rule 4; DO27 sign-off E3 removed the separate
/// `covered` flag: it is `ceiling.is_some()`).
pub type Support = Span;

/// `surface_at`'s answer (spec 26 § 2a.7's shape): the raw presentation query, on the same column kernel as
/// `support`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SurfaceAt {
    /// Highest up-facing surface with y <= near_y + tolerance.
    pub under: Option<SurfaceHit>,
    /// Lowest down-facing surface with y > near_y.
    pub over: Option<SurfaceHit>,
}

/// Every span in one column, ascending by floor height (equal heights keep candidate order). Fixed capacity 16
/// (T3 keeps 12; The Cinder Steps needs 5), no allocation. Sets `overflowed` when the column has more spans than the
/// capacity instead of dropping them silently; the cook already fails a column over `max_floors` (§ 10).
#[derive(Clone, Debug, Default)]
pub struct SpanBuf { /* [Span; 16] + len + overflowed */ }

/// A vertical extent, metres (`lo <= hi`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Band { pub lo: f32, pub hi: f32 }

/// The result of a wall push.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Push {
    /// The pushed centre XZ.
    pub xz: [f32; 2],
    /// Number of walls that moved it (0: untouched, `xz` bit-equal to the input).
    pub contacts: u16,
    /// The last wall that moved it.
    pub last: Option<TriRef>,
}

/// The first solid a segment meets.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SegmentHit {
    /// Fraction along `from -> to`, in [0, 1].
    pub t: f32,
    /// The point, metres.
    pub point: [f32; 3],
    /// What was hit.
    pub kind: SurfaceKind,
    /// The triangle (`None` for the ground).
    pub tri: Option<TriRef>,
}

/// Which surfaces a segment tests (bit set).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FaceMask(pub u8);
impl FaceMask {
    pub const GROUND: FaceMask = FaceMask(1);
    pub const FLOOR: FaceMask = FaceMask(2);
    pub const ROOF: FaceMask = FaceMask(4);
    pub const WALL: FaceMask = FaceMask(8);
    pub const BLOCKER: FaceMask = FaceMask(16);
    /// Everything solid (projectiles, line of sight).
    pub const SOLID: FaceMask = FaceMask(31);
    /// Walls, roofs and blockers, no floors and no ground (DO27's `segment_blocker`; the camera boom).
    pub const NO_FLOORS: FaceMask = FaceMask(28);
}

/// Ids, dense and sorted by the cook.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SectionId(pub u16);
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PortalId(pub u16);
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockerId(pub u16);

/// Bumped by every state change that moves an answer (a blocker flip, a ground edit). Consumers (nav tiles, sound
/// occlusion, the presentation layers' `generation`) compare it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Generation(pub u64);
```

### 5.2 Load and hashes

```rust
impl WorldGeom {
    /// Decodes the canonical section of a cooked `<map>.meshlayers` file (§ 9.2) and builds the index.
    /// Allocates; load time only. Refuses a newer version (`GeomLoadError::NewerVersion`).
    pub fn from_cooked(bytes: &[u8]) -> Result<WorldGeom, GeomLoadError>;
    /// The BLAKE3 hash of the canonical section's hashed prefix exactly as stored (§ 9.2 item 2a: its version field
    /// first, through the head, which holds the hashes of the count body that the load checks; ADR-8816): what the match
    /// handshake compares (ADR-8012 A-4: cooked outputs need byte stability; peers compare this hash, not the file).
    /// The canonical section holds **every byte a query, an edit, a blocker flip or `new_state` reads** (§ 9.2): the
    /// section version, the settings, the lattice constants, the ground mode and the level of every ground tile, the
    /// hanging-vertex overrides, the ring pins, the triangles, the lattice counts, the cut bitmaps and the plugs. So
    /// two peers that differ in any of them never pass the handshake (E6; ADR-8084, review W1).
    pub fn content_hash(&self) -> [u8; 32];
    /// The settings cooked into the file.
    pub fn settings(&self) -> &GeomSettings;
    /// A fresh state: every blocker at its cooked initial value, no ground edit, generation 0.
    pub fn new_state(&self) -> GeomState;
    /// Resident bytes of the geometry, the index and the lattice (for the § 11 budget and `terrain.bake_report`).
    pub fn resident_bytes(&self) -> u64;
}

impl GeomState {
    /// The geometry's term in the sim checksum, O(1): returns the running hash kept up to date by every blocker
    /// flip and ground edit (E4). DO27 reads it every tick and folds it as one more little-endian u64 term of its
    /// FNV-1a-64 checksum at the Stage B bump.
    pub fn state_hash(&self) -> u64;
    /// The generation.
    pub fn generation(&self) -> Generation;
    /// Appends the byte-stable snapshot of this state to `out` (join in progress, host migration; E4). Allocates
    /// only by growing `out`.
    pub fn encode(&self, out: &mut Vec<u8>);
    /// Restores a state from `encode`'s bytes for this `geom`. The restored state equals the encoded one, answers
    /// every query identically and has the same `state_hash`. Refuses a snapshot of another map or a newer version.
    pub fn decode(geom: &WorldGeom, bytes: &[u8]) -> Result<GeomState, GeomSnapshotError>;
}

/// The version of the query code's bits (ADR-8084, review W5). Bumped by every change that moves any answer's bits
/// for the same cooked bytes: every § 14 rule 1 or rule 5 change, and every Stage A re-copy that moves a pin. The
/// lockstep handshake (N224) compares `(content_hash, QUERY_BITS_VERSION, regia_detsim::MATH_BITS_VERSION)`, so a
/// peer on an older Regia patch with the same map is refused instead of desyncing. Pinned by
/// `query_bits_version_matches_the_query_table`: the committed query table (§ 13.2) records the version it was made
/// with, and a table change without a bump fails.
pub const QUERY_BITS_VERSION: u32 = 1;

/// Why a `GeomState` snapshot did not decode (agent codes in parentheses).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GeomSnapshotError {
    /// (`geom_snapshot_newer`) written by a newer engine.
    NewerVersion { found: u32, supported: u32 },
    /// (`geom_snapshot_other_map`) its content hash is not this `WorldGeom`'s.
    OtherMap,
    /// (`geom_snapshot_corrupt`) truncated, trailing bytes, an index out of range, unsorted overrides, or a stored
    /// hash that does not match the recomputed one.
    Corrupt,
}

/// Why a cooked file did not load. Codes are part of the agent protocol (`terrain.bake_report`).
#[derive(Debug, thiserror::Error)]
pub enum GeomLoadError {
    /// `geom_newer_version`: written by a newer engine.
    NewerVersion { found: u32, supported: u32 },
    /// `geom_corrupt`: the bytes do not decode, or a count or index is out of range.
    Corrupt(String),
    /// `geom_extent_too_large`: a coordinate beyond 2^24 mm of the origin.
    ExtentTooLarge,
}
```

**The state hash (DO27 sign-off E4, ADR-8072).**

1. **State, not history.** The hash is a function of the current state only: the set of blockers whose bit differs
   from its cooked initial value, the set of lattice vertices whose height differs from the cooked one (with the
   override in mm), the set of promoted tiles, and the generation. Two states that reach the same overrides by
   different edits hash the same; the state never keeps an edit list, so it is bounded however long the match runs.
2. **O(1) per call, incremental per change.** The state keeps a running 64-bit accumulator `acc`: the XOR of one
   64-bit term per entry of the sets of rule 1 (Zobrist 1970). A term is `regia_detsim::hash` (F-04) of a tag byte
   (1 blocker, 2 vertex override, 3 promoted tile) and the entry's little-endian fields (blocker id u16; vertex index
   u32 and height i32 mm; tile index u32). A blocker flip XORs one term in or out; an edit XORs out the old term and
   in the new one for each vertex it changes (O(vertices touched), never a re-walk). `state_hash()` returns
   `regia_detsim::hash` of `acc` and the generation, both little-endian u64, in that order.
3. **The fresh state** has `acc = 0` and generation 0, so its hash is the same constant on every map. The building
   lane (W-12) writes the constant here and pins it (`fresh_state_hash_is_the_documented_constant`). As built
   (ADR-8313): `0x6875_2350_ae1d_483f`, pinned by `fresh_state_hash_is_the_documented_value`; the term is FNV-1a-64
   finished by SplitMix64 until `regia_detsim::hash` (F-04) exists.
4. **The fold is pinned.** The tags, the field order, the XOR and the final fold are part of the lockstep contract:
   changing any of them is a bit change under § 14 rule 5 (new issue, new pin, a DO27 bump).
5. **Snapshot.** `encode` writes, little-endian: `u32` snapshot version (1), the `WorldGeom`'s 32-byte content hash,
   the generation (u64), the blocker bit words (u64 each, ascending id), the promoted tiles (u32 count, then ascending
   u32 tile indices), the vertex overrides (u32 count, then ascending u32 vertex index with its i32 mm), and the
   state hash (u64) as a check. The bytes depend only on the state. It is a wire and save payload, versioned by its
   own leading `u32`, not a content file; a lane that stores it as a file registers it as a format first.
6. **Pins (W-12 with W-13, each failing first):** `state_hash_is_o1_after_10k_edits` (a counter of terms touched per
   `state_hash` call reads 0, never a timer), `same_overrides_by_different_edits_hash_the_same`,
   `snapshot_restore_keeps_state_hash_and_answers`, `snapshot_of_another_map_is_refused`.

### 5.3 Queries [semantics settled by T3 and signed off (E3, § 15); bits pinned by W-11's vectors]

```rust
impl WorldGeom {
    /// The span the query point `(x, y, z)` is in (T3's `Column::span_at`; rule 4). `y` is the query point, not the
    /// body origin: the caller adds its own reach (DO27: the waist, origin + 0.9 m, its `BODY_REACH_M`). A pure
    /// function of its arguments and `state`: no band, no gate, no history, no previous floor.
    pub fn support(&self, state: &GeomState, x: f32, z: f32, y: f32) -> Support;

    /// Floor under and ceiling over `(x, z)` near `near_y` (spec 26 § 2a.7): the raw presentation query, on the same
    /// column kernel as `support`. `tolerance_m` widens `under`. Pinned property: for a point inside a free span,
    /// `surface_at(.., y, 0.0)` gives `support(.., y)`'s floor as `under` and its ceiling as `over`. DO27's sim calls
    /// `support` only.
    pub fn surface_at(&self, state: &GeomState, x: f32, z: f32, near_y: f32, tolerance_m: f32) -> SurfaceAt;

    /// Every span in the column (each floor, the ground included, with its ceiling), ascending. Sets
    /// `out.overflowed` when the column has more than `out`'s capacity (the cook already fails a column over
    /// `max_floors`, § 10). Every DO27 consumer needs the pair (placement fit, underground, camera, nearest floor).
    pub fn floors_at(&self, state: &GeomState, x: f32, z: f32, out: &mut SpanBuf);

    /// The lowest roof more than `lid_gap_m` above `floor_y` at `(x, z)` (DO27's `ceiling_above`).
    pub fn ceiling_above(&self, state: &GeomState, x: f32, z: f32, floor_y: f32) -> Option<SurfaceHit>;

    /// T3's `confine`: when the body is at or under the lid of its own span (the span `support(.., y)` returns),
    /// returns `min(y, lid − half_h).max(floor_y)`; otherwise `y` unchanged. A body over the lid (falling in
    /// through a shaft) is not pulled under it; in the open nothing changes. The f7e8f4740 single-ceiling special case
    /// is gone (E3).
    pub fn clamp_under_ceiling(&self, state: &GeomState, x: f32, y: f32, z: f32, floor_y: f32, half_h: f32) -> f32;

    /// True when a roof covers `(x, z)` above `y` (T7: the overview hides covered ground and the units in it; mouths
    /// and open cuts are not covered).
    pub fn covered_at(&self, state: &GeomState, x: f32, z: f32, y: f32) -> bool;

    /// The section a point is in (`None` outdoors): the section of the floor under the point when a roof of the same
    /// section covers it.
    pub fn section_at(&self, state: &GeomState, p: [f32; 3]) -> Option<SectionId>;
}
```

**Query rules (T3's semantics, signed off with E3; W-11 ports them and fits them to the lattice ground):**

1. *Candidates.* The index is a uniform XZ grid (`index_cell_m`); each cell lists every triangle whose padded XZ
   extent touches it, whichever cooked tile stores the triangle. A point reads exactly one cell. Walls are filed
   `wall_pad_m` wider, floors and roofs 0.25 m wider (T3). Order inside a cell: ascending `TriRef`, that is (layer,
   source triangle index), whatever the tiling. **No result depends on `index_cell_m` or the tile size**: the query
   table of § 13.2 is run at 4 m and at 16 m and must hash the same (E3, Q5 row 6). Each cell keeps two lists, by
   the kind the cook gave the triangle: floors and roofs (padded 0.25 m), and walls (padded `wall_pad_m`), each in
   ascending `TriRef`. The column calls read only the first list, `push_disc` and `push_capsule` only the second, and
   `segment` both (its answer is the smallest `t`, ties by `TriRef`, so the visit order does not matter). This is a
   performance rule only: walls are never floors or roofs (rule 2), so no answer changes, and the query table pins it
   (ADR-8084, review W7).
2. *Crossing.* A triangle crosses the vertical line through `(x, z)` when the point is inside its XZ projection by
   three 2D edge functions with a **top-left ownership rule** (a point on an edge shared by two triangles belongs to
   exactly one; Woop, Benthin and Wald 2013, watertight test), so a seam is never counted twice or missed. **Each
   shared edge is evaluated in one canonical direction** (from its endpoint with the smaller cooked `(x_mm, z_mm)`,
   compared lexicographically), so both triangles that share it compute the same bits (E3, Q5 row 5; this rule
   supersedes T3's inclusive barycentric test, `≥ −1e-5`). The
   height is the plane's in T3's order (§ 15 item 3): `n = (b − a) × (c − a)` unnormalized (its components cached
   at load), `y = a.y − (n.x·(x − a.x) + n.z·(z − a.z)) / n.y`; no square root reaches `y`. Triangles with
   `|n.y| < 1e-6` or `|n| < 1e-8` are walls and never floors or roofs. The reported normal is normalized with IEEE
   `f32::sqrt` and flipped to face up.
3. *Ground.* The lattice triangle under `(x, z)` (§ 3, found by § 3's point location), skipped where an open mouth
   cuts it (the cut is the lattice triangle set of spec 26 § 2a.4 item 2, cooked as a bitmap per ground tile: the same
   cells the renderer discards), counted where a mouth is closed.
4. *The column and support selection (T3, E3).* The column under `(x, z)` is built in one fixed order (T3
   `surface.rs:169-269`): the lids (down-facing candidates) are read first, in candidate order; each up-facing
   candidate and the ground is a floor; floors closer than `same_height_m` are one floor (the first in candidate
   order stands, and at equal height the ground wins over a layer floor); each floor's ceiling is the lowest lid more
   than `lid_gap_m` over it; spans are insertion-sorted ascending and equal heights keep insertion order.
   `support(.., y)` then returns the span with the highest floor at or under `y`. If `y` is at or over that span's
   ceiling (a point in rock), the nearer of that ceiling and the next floor up decides, ties to the lower span. With no
   floor at or under `y`, the lowest floor over it. There is no support band, no covered-floor gate and no history:
   **DO27's 0.6 m `SUPPORT_RISE_M` band caused the mouth hops and is gone**, and so are `support_rise_m` and
   `support_tie_m`. **Removed in Stage B:** the rubble-cone floor read from the drawn skylight patch (`rubble_top`,
   598): dressing that is a floor is a canonical triangle (handover finding 5, W-30).
5. *Continuity at mouths.* The ground under an open mouth is cut on the same lattice edges the layer's ring uses.
   The ring's vertices are shared bit for bit by the lattice and the band (§ 3, the ring pin), and the band is
   canonical geometry. So every point near a ring is owned by the ground or by the band, never neither, and the floor
   `support` returns is continuous across every mouth (ADR-8084, review W2). W-11's acceptance: at most 0.05 m
   vertical step per tick at full speed on a scripted drive across every mouth, plus
   `support_never_falls_through_a_ring_edge`, which places points exactly on every ring edge and vertex of the
   fixtures and their `f32` neighbors one ulp either side; each must get the ground or the band as its floor.
6. *Blockers.* A blocker plug's triangles are candidates only while `state` has the blocker active (one bit test per
   candidate whose `TriRef` is in a plug range; plugs are sorted after their section's triangles).
7. *Bad input.* A non-finite coordinate returns the ground at the clamped map edge for `support` and `None` for the
   optional answers, never a panic. T3 agrees in kind (§ 15 item 8: DO27 relies on no particular answer for a
   non-finite point); an off-mesh point gets the ground alone, and an empty mesh is the ground bit for bit.

### 5.4 Wall push

```rust
impl WorldGeom {
    /// Pushes a disc of `radius` centred at `(x, z)` out of every wall whose extent at the contact overlaps `band`
    /// (T3's disc push, `tunnels.rs:616-667` at 6fd2effb). One pass, no iteration: the candidates are the walls of
    /// the one cell under the **input** point, visited once each in `TriRef` order, and each wall sees the position
    /// the previous one left. A non-finite input or `radius <= 0` returns the input (`contacts` 0); otherwise
    /// `radius` is clamped to [0.05, `wall_pad_m`] (over the pad, one cell no longer holds every wall; E3, Q5 row 9).
    pub fn push_disc(&self, state: &GeomState, x: f32, z: f32, radius: f32, band: Band) -> Push;

    /// A vertical capsule (`radius`, feet at `band.lo`, head at `band.hi`): the disc push of `push_disc` over the
    /// capsule's band, then a vertical clamp of the head under roofs and the feet over floors met inside the radius.
    /// Returns the pushed centre and the clamped band.
    pub fn push_capsule(&self, state: &GeomState, x: f32, z: f32, radius: f32, band: Band) -> (Push, Band);
}
```

The push is the single shared wall test for hulls (disc), pilots (capsule) and the interior camera's swept sphere
(spec 26, W-26 builds the sweep from `segment` plus `push_capsule`).

**The disc rule (T3, settled; § 15 item 6).** A wall is skipped when its triangle's extents miss the band, when its
vertical span at the contact (`wall_span_at`, evaluated at the current pushed position) misses the band, when the
centre is `radius` or more from the wall's line, or when the centre projects outside the wall's segment (no end-cap
push). Otherwise the centre moves to `radius` from the line, on the side it is already on. DO27's band for a hull is
`floor_y + 0.5` to `max(y, floor_y) + 2.0` (the caller's rule; the engine takes any `Band`). DO27 clamps its own
radii to [0.3, 6.0]. **The capsule's vertical rule is not a T3 result**: W-30 settles it with its vectors.

**The capsule rule (W-30, ADR-8323; `QUERY_BITS_VERSION` 7).** `push_capsule` is `push_disc` over the band, then
`clamp_capsule` at the pushed centre: the column is read at the axis and at eight points at half the radius (fixed
order); at distance `d` the rounded end rises `e = r − sqrt(r² − d²)` (0 at the axis, `(1 − sqrt(3)/2)·r` at `r/2`, an
f32 literal). A floor `f` with `lo + e < f ≤ min(lo + r, mid)` lifts the feet to `f − e`; a lid `c` with
`max(hi − r, mid) ≤ c < hi − e` lowers the head to `c + e`; the highest lift and the lowest head win, and a head under
the feet is set to the feet (no fit). A surface deeper in the band is an obstacle for the mover
(`regia_world_geom::mover`), not the capsule's end. Pinned by `tests/fixtures/regia/capsule-vectors.tsv` (the
four-deck stack and the gallery) and the query table.

### 5.5 Segments, rock and line of sight

```rust
impl WorldGeom {
    /// The first surface in `mask` that `from -> to` meets: two-sided Möller–Trumbore per triangle (Möller and
    /// Trumbore 1997), cells walked by a 2D DDA over the index (Amanatides and Woo 1987) in order of entry, the
    /// smallest `t` wins, ties by `TriRef` (the ground before any `TriRef`). The ground is tested per lattice cell the
    /// DDA visits, after the ground tile's height-pyramid test below.
    pub fn segment(&self, state: &GeomState, from: [f32; 3], to: [f32; 3], mask: FaceMask) -> Option<SegmentHit>;

    /// Rock between two points: the first roof, or wall or blocker with a roof over the hit (an open cut's walls do
    /// not count; DO27's `rock_between`, `ir-sim/src/tunnels.rs:284`).
    pub fn rock_between(&self, state: &GeomState, from: [f32; 3], to: [f32; 3]) -> Option<f32>;

    /// `segment(.., FaceMask::SOLID).is_none()`: the call for projectile and AI line of sight (S-10's `LineOfSight`
    /// trait is implemented on `WorldGeom`).
    pub fn line_of_sight(&self, state: &GeomState, from: [f32; 3], to: [f32; 3]) -> bool;
}
```

DO27's `segment_blocker` samples its bucket list every 6 m along the segment and allocates a candidate list
(`ir-content/src/tunnels.rs:337-348`). Stage B replaces both: the DDA visits every cell exactly once and keeps no
list. The DDA steps cell indices by ±1 and compares Amanatides and Woo's `t_max` values in f32 in a fixed order (an
exact tie steps x before z), so the visited cells are the same on every platform.

**Ground height pyramid (ADR-8084, review W7).** A 300 m line of sight crosses about 1000 lattice cells at 0.31 m.
To keep the ground test off most of them, each ground tile keeps its lowest and highest vertex height. The levels
above it keep the min and max of 2 × 2 tiles up to the whole map. Edits and promotions update the pyramid in the same
fixed order as the index rebuild. The segment skips the lattice cells of a pyramid node when its lowest `y` over the
node's XZ extent is more than `PYRAMID_MARGIN_M` (1e-3 m) above the node's highest vertex. 1e-3 m is far above the
`f32` rounding of a Möller–Trumbore hit at map scale, so a skipped cell could not have been hit. The skip is
bit-neutral: `query_table_is_the_same_with_and_without_the_pyramid` runs the § 13.2 table both ways and must hash the
same. W-31 measures the gain. DO27 agreed to the DDA (E3, Q5
row 10). It can report hits the 6 m sampler misses, which is a bit change: § 13.3 carries the row
(`segment_finds_the_wall_the_6m_sampler_misses`, § 14 rule 1).

### 5.6 Sections, portals, zones

```rust
/// A run of passage the cook cut (more than `section_chord_m` off the chord, a cover change, a junction, or a
/// blocker boundary). Sections are cells for visibility, zones and sound.
#[derive(Clone, Debug, PartialEq)]
pub struct Section {
    pub id: SectionId,
    /// The passage or layer it came from (source id from `<map>.meshlayers.ron`).
    pub source: String,
    /// Covered (a roof over every floor triangle) or an open cut.
    pub covered: bool,
    /// Axis-aligned bounds, metres.
    pub bounds: [[f32; 3]; 2],
    /// Its triangle ranges (sorted `TriRef` ranges).
    pub triangles: Vec<std::ops::Range<TriRef>>,
    /// Portals on its boundary, sorted.
    pub portals: Vec<PortalId>,
    /// The blocker plug that can close it, if it is collapsible.
    pub blocker: Option<BlockerId>,
}

/// A portal: a mouth ring (to the outdoor cell 0) or a junction or section cut (between two sections).
#[derive(Clone, Debug, PartialEq)]
pub struct Portal {
    pub id: PortalId,
    /// The two cells it joins (`None` = outdoors).
    pub sides: [Option<SectionId>; 2],
    /// The portal polygon in integer millimetres (ADR-8012 A-4: portals in i32 mm), and its f32 copy.
    pub polygon_mm: Vec<[i32; 3]>,
    /// The mouth hole it rings, for a mouth portal (`NamedHole` id).
    pub mouth: Option<String>,
    /// The blocker that closes it, if any.
    pub blocker: Option<BlockerId>,
}

/// Zones the cook emits per section (spec 26 § 2a, W-24): exposure, reverb and interior volume share one shape.
#[derive(Clone, Debug, PartialEq)]
pub struct SectionZone {
    pub section: SectionId,
    /// Oriented box or capsule around the section's free space.
    pub shape: ZoneShape,
    /// Covered exposure band, EV100 (default 8.5 to 10.5) and blend distance (default 25 m).
    pub exposure: [f32; 3],
    /// Reverb preset name (`ReverbPreset`) and its cavity size, metres.
    pub reverb: (String, f32),
}

impl WorldGeom {
    pub fn sections(&self) -> &[Section];
    pub fn portals(&self) -> &[Portal];
    pub fn zones(&self) -> &[SectionZone];
    /// Every walkable span over the lattice cell under `(x, z)` (floor, the roof over it), for the layered nav bake.
    pub fn walkable_spans(&self, state: &GeomState, x: f32, z: f32, out: &mut SpanBuf);
}
```

Sections, portals and zones are cook products, read-only at run time. A portal whose blocker is active is closed:
VisCell traversal (W-21), sound (W-25) and nav (W-20) read `GeomState::blocker_active`.

As built (W-08, ADR-8210): `regia_world_geom::sections` cuts at cover changes, at the member sample nearest each listed
junction, at both ends of each blocker run, and otherwise by the greedy 3D chord rule (`section_chord_m`); portals at
section cuts, open passage ends and open-cut sections (sky portals, `mouth` = the opening), each with a nav node
(`CookedPortal::nav_mm`); one oriented-box zone per section (exposure `[8.5, 10.5, 25]`, reverb `tube`/`gallery`/`open`);
plug triangles appended after every passage triangle of the layer, so passage `TriRef`s keep the mesher's indices. A
skylight opening's ramp climb is cooked as an open cut (K8: no lid in a shaft column). `WorldGeom::sections()`,
`portals()`, `zones()` and `blockers()` return the cooked types for now. Switchbacks on the canyon cooks into 37
sections. As built (W-24, ADR-8384): `regia_world_geom::zones::section_volumes` emits one exposure band, one
reverb preset and one interior volume per cooked section, in section order, with no cap of four. `ExposureEase`
advances by the frame clock's dt and holds when dt is 0. `EaseSettings::depth_blend` defaults to on.

As built (W-21, ADR-8371): the same cook emits `<map>.viscells.ron` version 1 (no new field). Cell 0 is outdoors.
Each section is cell `id + 1` with the section zone as its box. Mouth rings and junction cuts are the portal quads.
A wall gap of 0.5 m or more, or two sections that still meet with the portals closed, fails the cook as
`viscell_leak` and names the section. `CookReport.vis_cells` and `CookReport.vis_portals` are what
`terrain.bake_report` lists.

### 5.7 Blockers (T7 collapse; lane W-13)

```rust
impl GeomState {
    /// Activates or clears a blocker plug. Query, segment, nav polys and sound occlusion all read the same bit, so
    /// they change together; the generation is bumped once. Returns the new generation. An unknown id is an error
    /// (`blocker_unknown`), never a panic.
    pub fn set_blocker(&mut self, geom: &WorldGeom, id: BlockerId, active: bool) -> Result<Generation, GeomEditError>;
    /// Whether a blocker is active.
    pub fn blocker_active(&self, id: BlockerId) -> bool;
}

impl WorldGeom {
    /// The cooked blocker plugs: id, section, initial state, and the plug's triangle range.
    pub fn blockers(&self) -> &[BlockerPlug];
}
```

A plug is the section's cross-section over `plug_length_m` (default 6 m), cooked as canonical triangles after the
section's own. Health, explosive-only damage and "stays until cleared" are sim rules. In D2 the plug becomes a voxel
carve behind the same call.

As built (W-13, ADR-8346): `regia_world_geom::blockers`. A flip to the current state changes nothing (no generation);
`apply_blocker_flips` applies one tick's flips sorted by (section, id, active); an active plug's up-facing faces are
column floors, down-facing faces lids, every face a `segment`/`rock_between` solid and the near-vertical faces
`push_disc` walls; with no plug active the queries skip the test (`QUERY_BITS_VERSION` 7, the query table unchanged).
Nav and portal reads until W-20: `section_open`, `portal_open`, `closed_sections` (whole list, ascending) and
`cell_route` (Dijkstra over the portal nav nodes). `GeomState::blockers(&geom)` is `terrain.blockers`'s reply.

As built (W-20, ADR-8486): the nav consumer is `regia_nav::det::layers`. Each layered-bake polygon carries the section of
the floor under it; `layered::geom::sync_layers` reads `closed_sections` and the ground edits' `DirtyRect`s once per
generation (closed set first, then dirty tiles), so a blocker closes its polygons at the generation `set_blocker`
returns. `cell_route` stays as the section-graph check.

## 6. D1 ground edits (lane W-12)

```rust
/// A sim-owned height edit on the ground lattice, integer millimetres (ADR-8012 A-4), applied in push order.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum GroundEdit {
    /// A crater: lowers lattice vertices within `radius_mm` of the centre by up to `depth_mm` (the integer profile
    /// below), never below a layer roof plus `min_cover_mm`, never on a mouth ring (E5). The centre is world mm: the
    /// sim converts its `f32` position with `m_to_mm` (§ 3).
    Crater { x_mm: i32, z_mm: i32, radius_mm: i32, depth_mm: i32 },
    /// A berm: raises the same profile by up to `height_mm`.
    Berm { x_mm: i32, z_mm: i32, radius_mm: i32, height_mm: i32 },
    /// Absolute heights of a lattice rectangle (the `TerrainHeightEdits` shape, spec 25 § 7.4).
    Rect { col: u32, row: u32, width: u32, height: u32, heights_mm: Vec<i32> },
}

/// The lattice rectangle an edit changed (inclusive start, exclusive end), for `TerrainHeightEdits`, nav tiles
/// and the query index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirtyRect { pub col0: u32, pub row0: u32, pub col1: u32, pub row1: u32 }

impl GeomState {
    /// Applies one edit to the ground. Allocates only when an edit touches a ground tile for the first time (its copy-on-write block, § 5.1).
    pub fn apply_ground_edit(&mut self, geom: &WorldGeom, edit: &GroundEdit) -> Result<DirtyRect, GeomEditError>;
}

/// Edit and blocker errors (agent codes in parentheses).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GeomEditError {
    /// (`blocker_unknown`)
    UnknownBlocker(BlockerId),
    /// (`edit_out_of_map`) the edit's rectangle is outside the lattice.
    OutOfMap,
    /// (`edit_too_large`) a side over 128 lattice cells (spec 25 § 7.4 `MAX_EDIT_SIDE`).
    TooLarge,
    /// (`edit_out_of_range`) a height outside ±1 000 000 mm (DO27's clamp, `terrain_deform.rs:15`), a radius outside
    /// 1..=1 000 000 mm, or a depth or berm height outside 0..=2 000 000 mm (§ 6, the integer profile).
    OutOfRange,
}
```

**The integer profile (ADR-8084, review W2 and W9).** Every number is an integer until the final `mm_to_m`:

1. The vertices are the lattice vertices `(i, j)` of the full-pitch lattice (after any promotion, E1), at
   `(x_mm(i), z_mm(j))` (§ 3). Each is visited in ascending vertex index `j · lattice_n + i`.
2. `r2 = radius_mm²` and `d2 = (x_mm(i) − x_mm)² + (z_mm(j) − z_mm)²`, in `i64`. A vertex with `d2 >= r2` is not
   touched.
3. `delta_mm = (depth_mm · (r2 − d2)).div_euclid(r2)` in `i64` (floor). It cannot overflow: `apply_ground_edit` refuses
   `radius_mm` outside 1..=1 000 000 and `depth_mm` or `height_mm` outside 0..=2 000 000 (`edit_out_of_range`), so
   the product is at most 2 · 10^18 < 2^63.
4. `base_mm` is the vertex's current override, else `m_to_mm` of its count height (§ 3).
5. A crater writes `max(base_mm − delta_mm, stop_mm)`. `stop_mm` is `m_to_mm(roof_y) + min_cover_mm`, where
   `roof_y` is the highest layer roof under the vertex (the § 5.3 plane height at the vertex's `f32` position);
   without a roof there is no stop. A berm writes `base_mm + delta_mm`.
6. The result is clamped to ±1 000 000 mm. A ring vertex is skipped (E5). A vertex whose result equals its current
   height is not written and does not change the state hash.
7. The new height is read as `mm_to_m(0, new_mm)`.

So two peers that apply the same edits get the same overrides on every target, with no float step between the sim's
`m_to_mm` of the centre and the final `mm_to_m`. Pinned (W-12, failing first): `crater_profile_matches_the_integer_table`,
a committed table of `(vertex, new_mm)` for fixed craters and berms at the centre, edge and corner of a ground tile.

**The ceiling stop.** An edit applies to the ground lattice only. It never moves a layer floor, and it never lowers a
lattice vertex below the highest layer roof under it plus `min_cover_mm` (a crater over a covered tube stops at the
rock over the tube: the tube floor moves by zero, DO27's 0.45 m dip is gone, handover finding 2). Where a mouth cuts a
lattice triangle the edit still writes the vertex (the cut hides it; a closed mouth shows it).

**Mouth rings are fixed (DO27 sign-off E5, ADR-8072).** An edit never moves a lattice vertex on a mouth ring (the
boundary of the lattice cut of any filled hole, open or closed, spec 26 § 2a.4 item 2): the profile is clipped to zero
there, and the vertex keeps its cooked height. Otherwise a crater on a mouth pulls the ground off the band's fixed top
edge and puts back a hop. Vertices inside the cut and outside the ring still take the profile. The clip is part of
the edit's arithmetic (one membership test per touched vertex against the cooked ring set) and the dirty rect is
unchanged by it. The smooth integer profile replaces DO27's flat 0.45 m disc (`ruins.rs:79-100`), which is a step at
its rim; "ground only, never a layer floor" matches T3. Test, failing first: `crater_on_a_mouth_ring_keeps_the_step_le_5cm`
(§ 13.3).

**Nested-pitch tiles (E1).** On a map authored `Nested`, an edit first promotes every ground tile
it touches to full pitch. Each new vertex is filled from the tile's cooked level by the same plane interpolation the
query used and stored as an mm override (`m_to_mm` of that height), so promotion moves a height by at most 0.5 mm
(ADR-8084; the 5fa1f1d69 text said "moves no height", which an mm override cannot promise), then re-derives the hanging-vertex overrides of the neighbors (§ 3 seam rule 6), in
ascending tile index, then ascending vertex index. The dirty rect covers the re-derived neighbor borders too.

**Where the dirty rect goes.** The sim hands it to the presentation (`TerrainHeightEdits`, spec 25 § 7.4, as absolute
mm rectangles of the same lattice), to the nav tiles it overlaps (W-20 marks them dirty) and to the query (the index of
the touched ground tiles is rebuilt in the fixed order before the next query). DO27's `ruins.rs` XZ disc and the
`sample_move_floor` add are deleted in Stage B against a new pin (§ 14).

**As built (lane W-12, ADR-8313).** `regia_world_geom::edit`. Refinements: a crater writes
`max(base − delta, min(base, stop))` (never raised by the stop), and the "roof under the vertex" is the highest roof at
or below its current height; a `Rect` height under the base takes the same stop; `edit_too_large` counts footprint
samples clipped to the map; a disc on the map that holds no vertex succeeds with an empty rect; promotion is a state
change even with a zero profile; an override equal to a cooked hanging override is not stored; an edited state's
segments do not skip by the cooked height pyramid. The fail-first forms run each bar on the Stage A crater book too.

**As built (lane W-11 D1, ADR-8818).** The edit walks its footprint one owner tile at a time (row slices of the counts,
cursors over the cooked overrides and the tile's block, one merge per block; each roof's plane built once per edit for
the ceiling stop), with the same results as the per-vertex walk, which is kept as `apply_ground_edit_reference`.
Promotion reads each coarse cell's corners once and merges a tile's fills into its block once; `height_rects` reads
rows. The index rebuild of this section is the state's own copy of the height pyramid's ranges, widened by every
height an edit writes: an edited state's segments skip by those (this replaces the last refinement above). The ranges
only grow and are not part of the state's value.

**Review fixes (ADR-8884).** A node's height range is the cooked one where no edit touches the tile, else the state's
own widened range, else none. Every level-0 tile of the descent walks the cells the single walk over the whole lattice
visits inside it, from the cell that walk is in when it enters the tile (found by counting border crossings), so
`segment` equals `segment_without_pyramid` on fresh and edited ground, also along lattice lines and through vertices
and tile corners.

## 7. Settings, limits and errors in one place

| Limit | Default (DO27 value) | Where enforced |
|---|---|---|
| `index_cell_m` | 16.0 m (DO27 `INDEX_CELL_M`) | cook; a performance setting (results pinned equal at 4 m and 16 m); W-31 may change it |
| `wall_pad_m` | 8.0 m | cook; the largest `push_disc` radius |
| `max_floors` | 8 (DO27 `MAX_NAV_LEVELS` 4) | cook check `column_over_max_floors`; `SpanBuf` holds 16 |
| `same_height_m`, `lid_gap_m` | 1e-3 m, 0.05 m (T3) | query (§ 5.3 rule 4); DO27's `support_rise_m` and `support_tie_m` are gone (E3) |
| `hull_fit_m` | 2.5 m | `support_for_placement` (placement rule; T3 `place_floor`) |
| `step_bound_mm` | 50 (E3; also Stage A at the T3 handover SHA in § 15 item 11; previously 200) | acceptance only, never a query branch |
| `min_cover_mm` | 2000 | D1 edits |
| `pitch_error_m` | 0.05 m (DO27 hop gate) | cook (`ground_pitch_seam`, `ground_over_budget`) |
| `fine_band_m` | 16 m | cook (`ground_pitch_seam`) |
| ground mode | `Full` (authored; `Nested { max_level }` is the other value) | cook (`ground_over_budget` names the mode that fits; never switched by the cook; ADR-8084) |
| `ground_tile_cells` | 64 lattice cells (a power of two, 16 to 256) | cook; the unit of levels, cut bitmaps, override blocks and the pyramid (§ 3) |
| lattice | `lattice_n` = the heightfield's sample count, `width_mm`, vertices at `x_mm(i)` | cook; independent of every render setting, cliff relief included (§ 3, ADR-8084) |
| D1 edit radius, depth | 1..=1 000 000 mm, 0..=2 000 000 mm | `apply_ground_edit` (`edit_out_of_range`; keeps the `i64` profile exact, § 6) |
| plug length | 6 m | cook |
| tile | 1024 m, i32 mm local | cook (`geom_extent_too_large`); storage only, never a sim bit (E3: `TriRef` carries no tile) |

## 8. Placement

```rust
impl WorldGeom {
    /// The floor a placed or spawned body takes at `(x, z)` (T3's `place_floor`; E3, Q5 row 7): the highest layer
    /// span whose clearance (ceiling minus floor) is at least `hull_fit_m` **or that is open to the sky**, else the
    /// ground. An open cut or a bridge deck is a valid placement floor, and under an open cut there is no ground to
    /// fall back to.
    pub fn support_for_placement(&self, state: &GeomState, x: f32, z: f32) -> SurfaceHit;
}
```

## 9. Formats (ADR-8019; spec 12 § 4a rows; the register `docs/formats/proposed-formats.ron`)

*As built (ADR-8112, lane W-06):* both formats are defined in `regia_world_geom` (`meshlayers`, `passages`,
`cooked`, `cook`) and registered live (`format-hashes.ron` rows, the fuzz targets `terrain.mesh_layers`,
`terrain.passages` and `terrain.mesh_layers_cooked`, corpus fixtures, `docs/formats/terrain.md` §§ 12-13); the
proposed-formats register is empty and its non-empty-register test is gone. Both join the single v0.6 format bump
(plan § 7).

### 9.1 `<map>.meshlayers.ron` v1 (source; registry name `terrain-mesh-layers`)

Spec 26 § 2a.10's document, with the Stage B additions:

```rust
/// The source of a map's mesh layers and its world geometry. Next to the map, found like the holes file.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct MeshLayersDocument {
    /// 1. A newer file is refused (`NewerVersion`).
    pub version: u32,
    /// Render chunk side, metres (spec 26 § 2a.5; default 64).
    pub chunk_m: f32,
    /// World geometry settings (§ 5.1); omitted = the defaults.
    #[serde(default)]
    pub geom: GeomSettings,
    /// The sim ground mode (§ 3; ADR-8084): authored, cooked into the canonical section and hashed; never switched by
    /// the cook. Omitted = `Full`.
    #[serde(default)]
    pub ground: GroundMode,
    /// Lattice cells per ground tile side (§ 3): a power of two from 16 to 256. Omitted = 64.
    #[serde(default = "default_ground_tile_cells")]
    pub ground_tile_cells: u16,
    /// The section rule: a cut where a sample is more than `section_chord_m` (default 4) off the chord or the cover
    /// changes, plus every junction.
    #[serde(default)]
    pub sections: SectionRule,
    /// Layers in id order.
    pub layers: Vec<MeshLayerSource>,
}

/// The sim ground mode (§ 3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum GroundMode {
    /// Every ground tile at full lattice pitch. Over the § 11 budget it is a cook error that names the `Nested`
    /// mode that would fit.
    #[default]
    Full,
    /// Each ground tile at its own level `0..=max_level`, chosen by E1's seam rule (§ 3).
    Nested { max_level: u8 },
}

pub struct MeshLayerSource {
    pub id: String,
    pub source: LayerSource,
    /// Named holes it fills (spec 26 § 2a.3).
    pub fills: Vec<String>,
    /// Pick layer, shadows, the optional engine-physics collider (spec 26 § 2a.8), material and LOD settings.
    pub map_layer: u8, pub cast_shadows: bool, pub collider: bool, pub material: String, pub max_lods: u8,
    pub throat_m: f32, pub skirt_m: f32, pub max_stitch_gap_m: f32,
    /// Sections of this layer that can collapse (by passage id and arc range); each gets a blocker plug.
    #[serde(default)]
    pub blockers: Vec<BlockerSource>,
    /// Looks: the render material and detail (displacement is bounded by the § 10 render-vs-collision check).
    #[serde(default)]
    pub look: LayerLook,
}

pub enum LayerSource {
    /// A passage description (centrelines, widths, ceilings, openings, junctions): the Stage A layout meshed by its
    /// pinned mesher. The canonical triangles are that mesher's output, quantized to mm.
    Passages { path: String },
    /// A glTF render mesh with an optional collision node (canonical = the collision node, else LOD-0 render).
    Gltf { path: String, node: Option<String>, collision_node: Option<String> },
    /// Collision triangles plus render detail (`MeshTunnel::from_collision`).
    Collision { path: String, detail: TunnelDetail },
    /// A signed-distance volume, meshed at cook (optional lane; D2 later).
    Volume { sdf: String, iso: f32, cell_m: f32 },
}
```

The `Passages` file is the layout as data (`<map>.passages.ron`, a `TunnelLayout` with its version): `regia terrain
passages import <do27-tunnels.json>` converts DO27's `do27-tunnels/1` JSON into it and `export` writes it back
(W-06). `do27-tunnels/1` is a foreign host format: Regia imports it and never freezes it.
*As built (ADR-8112):* `<map>.passages.ron` is part of the `terrain-mesh-layers` family (root `version` 1, the
`TunnelLayout` inside keeps its own layout version). An imported file keeps DO27's baked `sampled`, `holes` and
`triangle_count`; the cook re-samples the layout and refuses a disagreeing stored block (`passages_drift`). The export
is byte-identical to `TunnelLayout::export_json`.

### 9.2 Cooked `<map>.meshlayers` v2 (cooked; registry name `terrain-mesh-layers-cooked`)

*Version 2 since v0.7 (ADR-8816, owner-approved format batch 2):* the canonical section stores its lattice counts at
a fixed width behind a hashed head (item 2a below). Version 1 (v0.6.0) stored the section as one
`postcard(WorldGeomCooked)` with every count a varint, and its `content_hash` was the BLAKE3 of all of it; decoding
16.7 M varints and hashing 33.5 MB were 61 ms of the 86 ms Switchbacks load on the Android test device (§ 11).

One postcard file (`.fckd` envelope), three parts in this order:

1. **Header**: magic, `version` (2), the input hash (BLAKE3 of the source document and every file it names), and
   copies of the settings and the ground mode for tools that read the header alone (`terrain.bake_report`,
   `regia terrain check-layers`). The header copies are informational: nothing at run time reads them, and the loader
   refuses a file whose header copies differ from the canonical section (`geom_corrupt`).
2. **Canonical section** (`WorldGeomCooked`, owned by `regia_world_geom`), in this order:
   - its own `canonical_version` (`u32`, 1);
   - the `GeomSettings`;
   - the lattice constants: `lattice_n` (`u32`), `width_mm` (`i32`), `ground_tile_cells` (`u16`), and `offset_m`
     and `step_m` as `f32` bits;
   - the authored **ground mode** (`GroundMode`), then the **level of every ground tile** (0 = full pitch; a
     `Nested` map records every tile's level, not one `k`);
   - the hanging-vertex overrides of § 3 seam rule 4 (ascending vertex index, `i32` mm);
   - the **ring pins**: the mouth-ring vertex set of E5 with each vertex's `i32` mm height (ascending vertex index;
     § 3);
   - the cooked tiles (origin mm), and per tile the sorted triangles as `[[i32; 3]; 3]` mm plus kind, flags (portal,
     pilot-only, no-support; ADR-8112, for DO27's re-sign), section, material and canonical (layer, source index);
   - the ground lattice (`u16` counts per ground tile at its level);
   - the mouth-cut bitmaps (one per ground tile that a cut touches, ascending tile index);
   - sections, portals (`i32` mm), zones, and blocker plugs with their initial states. (As built, W-08: portals also carry
     their nav node `nav_mm`; plug triangles are canonical triangles of their layer, after its passage triangles.)

   2a. **The section's bytes (version 2, ADR-8816).** The list above is the section's *value*. It is stored as:

   | Part | Bytes |
   |---|---|
   | `canonical_version` | postcard `u32` (2: one byte) |
   | `head_len` | `u32`, little-endian |
   | the head (`cooked::CanonicalHead`) | `head_len` bytes of postcard, in this order: the settings, the lattice constants, the ground mode, the tile levels, the hanging overrides, the ring pins, the cooked tiles, the cut bitmaps, sections, portals, zones, plugs, then `count_lens` (the number of counts each ground tile stores, `u32` each, ascending tile index) and `count_hashes` (one BLAKE3 per block of the count body) |
   | the count body | every ground tile's counts, ascending tile index, row major, each count a little-endian `u16`; no length prefix, no padding; `2 * sum(count_lens)` bytes |

   The count body is hashed in blocks of `COUNT_BLOCK_BYTES` (65,536) bytes, the last one shorter;
   `count_hashes` has `ceil(body_len / 65536)` entries. The loader refuses (`geom_corrupt`, naming the block) a body
   block whose BLAKE3 differs from its entry, a body whose length is not `2 * sum(count_lens)`, a wrong number of
   hashes, bytes after the head inside `head_len`, and every structural problem it refused in version 1. It checks a
   block just before it copies that block's counts, so the body is read once.

   **Rule (ADR-8084, review W1): every byte that `from_cooked`, a query, an edit, a blocker flip or `new_state`
   reads is in this section.** The run time builds `WorldGeom` from this section alone and reads the header only to
   check its copies. `content_hash` is the BLAKE3 of this section's **hashed prefix** exactly as stored: the bytes
   from `canonical_version` to the end of the head. The head holds the hash of every block of the count body and
   the loader checks each one, so `content_hash` covers every byte of the section, as in version 1, and it can be
   computed from the prefix alone (`cooked::content_hash_of_bytes`). So it covers the version, the settings, the
   lattice constants, the ground mode, the tile levels, every count and every
   other input of an answer (DO27 sign-off E6 covered the settings and the ground mode; ADR-8084 closes the rest).
   A field added later goes into this section, or the run time must not read it.

   **Version 1 files (v0.6.0) still load.** The loader reads the version first: a version 1 file holds a version 1
   section, which it decodes as before and treats as the same value at version 2 (`cooked::migrate_canonical`). Its
   `content_hash` is the hash of that value's version 2 bytes, so a peer that kept a v0.6.0 file and a peer that
   recooked agree. The version 1 path costs what v0.6.0's load cost plus one encode; `regia upgrade-project --write`
   rewrites the file (`cooked::migrate_file`: the header version and the canonical section move, the presentation
   and chunk sections are kept byte for byte), and the cook pipeline recooks a version 1 cache. A file whose header
   and section versions disagree is refused (`geom_corrupt`). Pins (ADR-8816):
   `canonical_v2_stores_fixed_width_counts_after_a_hashed_head`, `a_changed_count_byte_is_refused_at_load`,
   `a_v0_6_0_file_migrates_to_the_same_value_and_hash`.

   Pins (W-06, each failing first):
   - `settings_change_moves_content_hash`: each `GeomSettings` field changed by one ulp or one unit;
   - `every_canonical_field_moves_content_hash`: the version, each lattice constant, the ground mode, one tile level,
     one ring pin, one hanging override and one cut bit, each changed alone;
   - `runtime_reads_only_the_canonical_section`: a file with its header and presentation section zeroed, then the
     header copies refreshed, loads to a `WorldGeom` with the same query table.

   A DO27 headless server reads the header and this section only (postcard `take_from_bytes`) and never decodes the
   rest.
3. **Presentation section** (`TerrainMeshLayersCooked`, owned by `regia_terrain`): chunks and their `CookedMesh` v3
   LOD sub-assets, mouths (cut runs, rings, bands, collars) as in spec 26 § 2a.3.
   As built (W-07, ADR-8175): `PresentationCooked` (`regia_world_geom::render`, version first): per `Passages` layer
   the render mesh cooked from the conditioned canonical triangles, each face subdivided `look.subdivision` times
   (2..=16, default 4) in integer millimetres, displacement 0, each render triangle naming the canonical face it lies
   on (`RenderSource`); no canonical triangle is in the draw list. Plus the mouth cut quads. Chunks and LODs come with
   W-14/W-16.

It is a cache: a version mismatch recooks, and `regia upgrade-project` lists it as derived data. Peers compare
`content_hash`, so a render-only change does not split a match, and a settings or ground-mode change always does.
(This resolves the contradiction DO27 found in the 5fa1f1d69 revision, where § 5.1 said a peer with other settings
has another content hash while this section put the settings and the ground mode in the header, outside the hashed
bytes: two peers with different support settings or pitch levels would have passed the handshake and desynced.)

**Load-time conditioning becomes a cook step (E6, Q9 item 3).** T3 conditions DO27's tunnel mesh when it loads it.
In Stage B that is a cook step on the `Passages` import, listed in `terrain.bake_report` with counts per rule:
- **weld**: a floor vertex up to 0.15 m over the ground goes onto it;
- **trim**: a floor strip under 2 m wide lying within 0.5 m of another floor carries nothing (it is not a support
  floor).

The canonical triangles are the conditioned set: the cooked file holds them, and the run time never conditions.
`render_off_collision` (§ 10) then flags DO27's sills and proud lips against the conditioned triangles, which is
correct: DO27 fixes its mesher (its lane T1).

## 10. Bake consistency checks (lane W-09; the cook fails on any)

Each check is a cook error with a code, a location and the measured value. A missing input is an error, never an empty
pass (`input_missing`). `regia terrain check-layers <map>` runs them without writing; `terrain.bake_report` returns
them.

| Code | Rule | Source |
|---|---|---|
| `render_off_collision` | one-sided Hausdorff render → canonical and canonical → render ≤ 0.05 m (displacement bound) | DO27 rule 1 and 5, research (Metro, Cignoni et al. 1998) |
| `nav_off_floor` | every nav polygon sample within 0.5 m of a `support` floor; every walkable floor on a sample grid covered by nav or listed as excluded | DO27 rule 2, research |
| `mouth_cut_mismatch` | the mouth cut equals the canonical layer's opening on the lattice (same cells), and every ring edge is an edge of exactly one canonical triangle and of exactly one uncut lattice triangle with equal integer endpoints `(x_mm(i), ring_mm, z_mm(j))` (§ 3, the ring pin). An integer comparison, no tolerance | DO27 rule 3; ADR-8084 (W2) |
| `junction_open` | junction members share their cut boundary edges exactly (equal `i32` mm endpoints) | DO27 rule 4 |
| `lane_too_narrow` | an on-foot lane ≥ 2 m at every station | DO27 host gate |
| `hull_blocked` | a disc of each hull radius (2.5, 2.6, 3.6, 3.9, 4.1, 4.5 m) and the 0.35 m pilot capsule passes every passage: 0 blocked probes, worst push 0.000 m, lid ≥ 10 m over the floor (radii and bars are cook settings with these defaults) | DO27 sweep bar |
| `drawn_solid_off_wall` | drawn solid at body height within 0.5 m of the canonical wall | DO27 host gate (finding 5) |
| `hidden_face_in_wall` | no face under a floor fused into a wall | DO27 finding 1 |
| `not_watertight` | the canonical mesh plus the ground through the rings is closed; a flood fill from outside reaches no interior cell | research (one-source bake with a leak test) |
| `above_ground_outside_cut` | no canonical triangle above the LOD-0 ground outside a cut | ADR-7974 `seam_above_ground` |
| `ground_inside_span` | no ground lattice vertex between a span's floor and its lid (T3 lifted such ground onto the lid at run time; Stage B refuses it at cook) | DO27 sign-off § 15 item 7 |
| `seam_gap` | `seam_report` gap and lip ≤ 1e-4 m at every LOD pair | ADR-7974 |
| `portal_blocked` | a ray through each portal hits nothing; with portals closed, a flood fill separates the cells | research |
| `zone_mismatch` | samples inside a section lie in its zones; samples outside do not | research |
| `ground_vs_render` | compared per lattice vertex index `(i, j)` against the **undisplaced** LOD-0 render surface (`TerrainRelief` off, no detail displacement; cliff relief is presentation and never compared): height ≤ 0.5 mm in full-pitch tiles (the mm rounding of ring pins and overrides), ≤ `pitch_error_m` (0.05 m) in nested tiles; XZ ≤ 0.5 mm (`x_mm(i)` against the terrain's `f32` sample position) | A-7, E1; ADR-8084 (W3) |
| `ground_pitch_seam` | every tile's level is the coarsest within `pitch_error_m` of the full-pitch ground (§ 3 seam rule 1); neighbors differ by at most one level (rule 3); every hanging vertex carries the coarse edge's midpoint override, so the step across a pitch border is ≤ 1 mm (rule 4; E1's 1e-4 m is not reachable with a millimetre override, ADR-8084); no pitch border within `fine_band_m` of a mouth ring or a layer footprint (rule 5) | DO27 sign-off E1 |
| `ground_over_budget` | the authored ground mode cannot meet the § 11 memory budget: `Full` names the `Nested { max_level }` that would fit; `Nested` lists the tiles that would need a coarser level than `pitch_error_m` or `max_level` allows. Never a silently coarser level, never a mode switch | DO27 sign-off E1; ADR-8084 (W4) |
| `column_over_max_floors` | no column has more than `max_floors` floors | § 7 |
| `header_mismatch` | the header's settings and ground-mode copies equal the canonical section's (the loader also refuses a mismatch, `geom_corrupt`) | E6 |
| `lattice_mismatch` | the canonical lattice constants match the heightfield (`lattice_n` = its sample count, `width_mm` = `m_to_mm` of its width) and the terrain's level-0 node grid nests in the lattice (spec 26 § 2a.4 item 1) | ADR-8084 (W3) |
| `drift_triangles`, `drift_manifest`, `drift_mouth`, `drift_portal`, `spawn_not_flat` | triangle count vs the source's stated count; manifest rows vs files; mouths vs the layout; portal positions vs the cut; spawn ring rise ≤ the map's flatness limit | DO27 drift findings (3526 vs 3378; mouths [-48,-256,-78] vs [-40,-256,-90]; s 46.88 vs 57.7; 3.95 m on a 9 m ring) |
| `determinism_table` | the pinned query table (§ 13.2) hashes to the committed value | A-4 |

## 11. Budgets (lane W-31 measures them)

| Item | Budget | Basis |
|---|---|---|
| Sim queries per tick | 500 mixed calls (200 `support` + 40 `push_disc` and `clamp_under_ceiling` at 40 hulls, 64 `segment` + 64 `support` at the projectile cap, about 100 `line_of_sight` on Hard) ≤ **0.5 ms on the Deck**, ≤ 0.8 ms on the Android test device, ≤ 0.25 ms on the build server; 0 allocations | Q14 § 3-4; DO27's whole tick p99 at 40 seats is 2-3.3 ms |
| Index build | ≤ 50 ms per map at load (10k canonical triangles) | Cinder round 2 is about 5-8k triangles |
| Sim geometry RAM | ≤ 35 MB on maps ≤ 2.56 km (34 MB until ADR-8396), ≤ 64 MB beyond (lattice, triangles, index, sections, state); over it in the authored ground mode → `ground_over_budget`, which names the `Nested` mode that fits (§ 3; the cook never switches) | Q14 § 13 (32 MB ceiling estimate); Switchbacks lattice 33.5 MB, triangles about 0.2 MB |
| D1 edit | ≤ 0.2 ms for a 6 m crater including the index rebuild of its ground tiles | new |
| Blocker flip | O(1) plus one generation bump; no rebuild | new |
| Seams and steps | ≤ 1e-4 m render seam (`seam_gap`); ≤ 1 mm ground step across a pitch border (`ground_pitch_seam`, ADR-8084); ≤ 0.05 m mouth step per tick; ≤ 0.5 EV between exposure samples | acceptance.json |

`query_bench_1000_mixed_calls` (W-31) measures the mix; its rows go into `docs/perf/budgets.ron` (`cargo xtask
perf-budgets`, ADR-2704) with this section as their `spec`, and the allocation count joins
`docs/perf/budget-counts.json` (0 allocations per call).

Measured by W-31 (ADR-8662, `docs/evidence/tunnels-perf-low-tiers/`; bench `regia_world_geom/benches/mixed_calls.rs`,
the mix above as 508 calls on the cooked Switchbacks map; counters in `tests/perf_low_tiers.rs` and
`regia_asset_pipeline/tests/layer_memory_low_tiers.rs`):

| Item | Android test device (Pixel 9 Pro) | N150 (Windows) | the build server (shared host) | Deck |
|---|---|---|---|---|
| Mixed tick, p50 | 0.94 ms (bar 0.8; 28.7 before W-31) | 3.75 ms (95 before) | 1.65-2.13 ms at load 140-230 (bar 0.25 idle) | not measured (unreachable) |
| Allocations per tick | 0 | 0 | 0 | - |
| Index build at load (Switchbacks, 5301 triangles) | 86 ms (decode 42, hash 19) | 209 ms | 93-128 ms | - |
| Index build at load, format v2 (ADR-8816, 2026-11-19; `docs/evidence/w11-index-build/`) | 49.3-49.6 ms (84.6-90.0 before; hash 19.7, the device's cost for 4096 count blocks about 24; 29.7-30.5 with the body check deferred) | not measured | the test machine at load 20: 13.3 ms (38.9 before) | 28.0 ms (79.2 before; 9.2 deferred) |
| D1 crater, 6 m radius | 0.37 ms | 1.0 ms | 0.48-0.63 ms | - |
| Sim geometry RAM (counter, every host) | 34,038,156 B (35 MB bar; kernel 483,724 B, under T3's 4 MB) | | | |

Measured again by W-11 lane 001 (ADR-8815, `docs/evidence/w11-native-query-speed/`; the same bench; `support`
untouched, the other calls faster):

| Item | Android test device (Pixel 9 Pro) | Deck | the build server, an idle core pair | the test machine (load 20-25 of 32 threads) |
|---|---|---|---|---|
| Mixed tick, p50 | 0.239 ms, one run (bar 0.8; 0.93-0.96 before) | 0.428-0.431 ms, five runs (bar 0.5; 1.64 before) | 0.363-0.398 ms, four runs (bar 0.25: missed; 1.38-1.49 before) | 0.168 ms on an undisturbed core, 0.24-0.27 ms with a busy sibling thread (0.79-1.19 before) |
| The same tick after one D1 crater | 0.270 ms (3.57 before) | 0.483-0.485 ms (5.90 before) | 0.41-0.44 ms (5.0-5.5 before) | 0.18-0.33 ms (2.7-4.4 before) |
| Allocations per tick | 0 | 0 | 0 | 0 |
| Sim geometry RAM (counter, every host) | 34,176,684 B (35 MB bar; 0.88 % over the cook estimate, the test allows 1 %) | | | |

Of the build server's tick, `support` is 0.22-0.25 ms (264 calls at 0.85-0.93 us): the build-host bar waits for W-11 lane 003.

After W-11 D4 (ADR-8817, `docs/evidence/w11-native-support-column/`; the test machine stands in for the build host and
was not idle):

| Item | Android test device (Pixel 9 Pro) | Deck (idle core) | the test machine (load 25-29) |
|---|---|---|---|
| Mixed tick, p50 | 0.717 ms (bar 0.8: met) | 1.44 ms (bar 0.5; 1.64 before) | 1.01-1.03 ms (bar 0.25 idle; 1.14-1.21 before) |
| `support` in the tick, per call | 0.27 µs (0.59 before) | 0.50 µs (1.02) | 0.34-0.35 µs (0.63-0.68) |
| W-11 `support` median on the T3 fixtures (bar 150 ns) | 325-814 ns (692-1383) | 510-1430 ns (1100-2140) | 370-1012 ns (751-1373) |
| W-11 native/compat ratio (bar 0.25) | 1.13-2.22 (2.1-3.8) | 1.07-2.04 (1.90-3.12) | 1.28-2.46 (2.09-3.42) |
| Sim geometry RAM (counter, every host) | 34,171,270 B (35 MB bar; kernel 616,838 B) | | |

Measured by W-11 D1 (ADR-8818, `docs/evidence/w11-crater-edits/`; the same bench, p50):

| Item | Android test device (Pixel 9 Pro) | Deck | the test machine (9950X3D, loaded: not an idle core) |
|---|---|---|---|
| D1 crater, 6 m radius (1154 vertices; bar 0.2 ms) | 0.060-0.065 ms (0.359-0.381 before) | 0.083-0.084 ms (0.479-0.482 before) | 0.047-0.072 ms (0.21-0.34 before) |
| D1 crater, 3 m radius = 6 m diameter (288 vertices) | 0.019 ms (0.087-0.088 before) | 0.023 ms (0.123 before) | 0.012-0.018 ms (0.054-0.089 before) |
| D1 crater, 6 m radius, on a state with 30 craters | 0.056-0.061 ms (0.40-0.43 before) | 0.082-0.086 ms (0.57 before) | 0.041-0.066 ms |
| `height_rects` of that crater's rectangle (39 x 39) | 0.010 ms | 0.012 ms (0.074-0.075 before) | - |
| First 6 m crater in a coarse tile of a `Nested` map (one 64-cell tile promoted) | 0.168-0.171 ms | 0.241 ms (3.60-3.66 before) | - |
| Mixed tick on a state with 30 craters, p50 | 0.99-1.02 ms (3.93 before) | 1.77 ms (6.54 before) | 1.24-1.27 ms (4.67-4.80 before) |
| Mixed tick on a fresh state, p50 | 0.93-0.94 ms (0.93-0.96 before) | 1.65 ms (1.63-1.64 before) | - |

The fresh-state tick moved between 0.87 and 0.96 ms (Pixel) and 1.61 and 1.72 ms (Deck) across this lane's builds with
no source change in the calls that moved (ADR-8818): read differences of that size as code placement.

## 12. Agent surface (rule 8; lane W-28, book `book/agents/terrain-mesh-layers.md`)

| Method | Request → reply | Errors |
|---|---|---|
| `terrain.surface_at` | `{ points: [[x, z, near_y], ..] (≤ 64), tolerance_m? }` → `{ results: [{ at, under, over, support, section }], generation }` | `bad_point`, `too_many_points`, `layers_loading`, `F0003` |
| `terrain.mesh_layers` | `{}` → layers, chunks, mouths, sections, portals, blockers, `resident_bytes`, `content_hash`, `generation` | `F0003` |
| `terrain.bake_report` | `{ map? }` → the § 10 results of the last cook, the ground mode and the § 11 memory figures | `no_bake_report` |
| `terrain.blockers` | `{}` → `[{ id, section, active }]`; `{ set: id, active }` flips one (dev builds and the editor; the sim owns it in a match) | `blocker_unknown`, `blocker_sim_owned` |
| `terrain.segment` | `{ from, to, mask? }` → `{ hit: { t, point, kind, section }? }` | `bad_point` |
| `terrain.ground_edit` | `{ crater | berm | rect }` → `{ dirty, generation }` (dev builds and the editor) | `edit_out_of_map`, `edit_too_large`, `edit_out_of_range`, `blocker_sim_owned` |
| `geometry.body_probe` (W-30, ADR-8323; stateless, a cooked file path) | `{ cooked, feet: [x, y, z], radius, height, step_up, step_down }` → `{ support, band, covered, fits, query_bits_version }` | `body_probe_bad_params`, `body_probe_bad_point`, `body_probe_unreadable`, `body_probe_corrupt` |

## 13. Acceptance and pins

### 13.1 Stage A pins (copied with DO27's tests; they must pass unchanged)

| Pin | Runs in | Lane |
|---|---|---|
| `switchbacks_mesh_is_byte_identical_from_the_baked_description` (`tunnel_layout.rs:1276`) | Regia | W-01 |
| `the_runtime_loader_arms_every_map_byte_identically` (`tunnels_switchbacks.rs:82`) | Regia | W-01 |
| `gallery_floor.rs` floor-flag tests | Regia | W-01 |
| `move_floor_is_the_one_content_ground_query` (`ir-sim/src/tunnels.rs:571` at `regia-absorb-2`), `support_is_a_pure_function_of_the_point` (619; T3's replacement for `support_ignores_history`), `step_ok_matches_the_t3_acceptance` (`ir-content/src/tunnels.rs:1454`), the T3 vectors (`spec/maps/t3-vectors/`, `t3_vectors_match_the_compat_query`) and the Stage A bit table (`tests/mesh_bits.rs`, `stage_a_bit_table_is_pinned_on_every_arch`) | Regia | W-02 |
| `switchbacks_baked_file_builds_the_byte_identical_sim` (`tunnels_switchbacks.rs:49` at `regia-absorb-2`; the navmesh half with W-04) | Regia | W-02 with W-04 |
| `tunnel_runs_give_the_same_hash` (`pathing/tests.rs:768`) | Regia (`regia_nav::det`) | W-04 |
| the sweep (156/156), the walks (688/688 both ways, gallery 69/69), hull under and over | Regia | W-05 |
| `switchbacks_tunnel_fight_lockstep_checksum_is_pinned` (0xbd96_cd18_09dc_958d), `a_net_client_predicting_its_hull_through_the_tunnels_never_shows_a_vertical_jump`, `determinism_fuzz_short_fixtures_agree_every_tick`, the 3600-tick two-seat replay hash | **DO27 only** (its whole sim); the values are DO27's `ironreach/1.58` (T3) values, and the fight checksum is the 1.58 one if T3 moved it (E7) | DO27's swap branch after W-01 to W-05, F-06 and S-07 |

Pins of the modules copied from `regia-absorb-2` (§ 4, E2) are DO27's tests as they stand at that tag, with their T3
line numbers; T3 may rename or replace the W-02 rows above (its history forms are gone), and W-02 re-cites them before
it copies. The line numbers above are f7e8f4740's.

Stage A fail-first: the crate does not exist on the base. W-05 also adds `an_empty_solid_set_fails_the_sweep` and
`fused_spans_block_the_gallery_walk` (both fail first; the stale comment at `tunnels_switchbacks.rs:1048-1056` is
rewritten, the asserts are not).

### 13.2 Stage B pins (new)

- **The query table.** A committed table of `support`, `surface_at`, `push_disc`, `push_capsule`, `segment` and
  `clamp_under_ceiling` results (f32 bits) over a fixed point set on the Switchbacks and The Cinder Steps fixtures and
  a synthetic 4-deck stack, hashed; equal on x86-64 Linux and Windows and on aarch64 (qemu-user gate leg, the Mac, the
  Android test device, the Deck; F-03's runners). T3's reference vectors (§ 15 item 2) are added to the same table unchanged.
- **The sim pins**, re-recorded once at the Stage B bump (§ 14): the fight checksum, the predicted-hull drive, the
  determinism fuzz, `tunnel_runs_give_the_same_hash` (replaced by W-20's layered-nav pin).
- **Byte stability.** `cook_twice_gives_identical_bytes` for the cooked file on every runner.
- **Tiling independence (E3).** `query_table_is_the_same_at_4m_and_16m_cells`: the query table hashes the same with
  `index_cell_m` 4 and 16 (and with the tile size halved), so neither is a sim bit.
- **One kernel (E3).** `surface_at_inside_a_free_span_equals_support`: for points inside a free span,
  `surface_at(.., y, 0.0)` gives `support(.., y)`'s floor and ceiling.
- **Amendment 1 pins (ADR-8084; each fails first).**
  - `m_to_mm_inverts_mm_to_m_below_2_23` (§ 3; W9).
  - `settings_change_moves_content_hash`, `every_canonical_field_moves_content_hash` and
    `runtime_reads_only_the_canonical_section` (§ 9.2; W1).
  - `lattice_vertices_are_integer_mm_and_shared_with_the_rings`: every ring vertex of the fixtures has equal `f32`
    bits as a lattice vertex and as a canonical vertex (§ 3; W2).
  - `support_never_falls_through_a_ring_edge` (§ 5.3 rule 5; W2).
  - `crater_profile_matches_the_integer_table` (§ 6; W2, W9).
  - `lattice_pitch_ignores_cliff_relief`: the canonical section and the query table are identical when the same map
    is cooked with `TerrainRelief::cliff_relief` 0 and 1 (§ 3; W3).
  - `full_mode_over_budget_names_the_nested_mode`: a `Full` fixture over the budget fails with `ground_over_budget`
    naming `Nested { max_level }` and is not cooked (§ 3; W4). The refusal is the § 10 bake check's report, whose
    finding carries the mode (`fits`) or, for `Nested`, the tiles (ADR-8436).
  - `stage_a_bits_are_the_same_with_and_without_stage_b`: the Stage A bit table (W-05) built with and without the
    `stage_b` feature hashes the same (§ 14 rule 2; W5).
  - `query_bits_version_matches_the_query_table` (§ 5.2; W5).
  - `query_table_is_the_same_with_and_without_the_pyramid` (§ 5.5; W7).

### 13.3 Known issues that become Stage B acceptance (each fails first)

| Issue (handover § 7) | Test | Lane |
|---|---|---|
| Mouth hops 0.55-0.91 m | `mouth_hop_at_full_speed_le_5cm_per_tick` | W-11, W-29 |
| Crater dips a tube floor 0.45 m | `crater_over_a_covered_tube_moves_its_floor_by_zero` | W-12 |
| A crater on a mouth pulls the ground off the band (DO27 sign-off E5) | `crater_on_a_mouth_ring_keeps_the_step_le_5cm` | W-12 |
| XZ-only outdoor separation | `two_outdoor_bodies_on_stacked_floors_do_not_separate` | F-06 |
| Rubble pile is not a sim floor; 1.6 m drop | `rubble_pile_is_a_sim_floor`, `rubble_drop_le_step_bound` | W-30, W-29 |
| Art vs sim lane at the mid-east ramp | `drawn_solid_within_half_meter_of_wall` | W-09, W-30 |
| Four drifting files | `stale_manifest_row_fails_verify`, `mouth_drifts_from_layout_fails_cook`, `portal_report_drift_fails_cook`, `spawn_ring_rise_over_limit_fails` | W-09 |
| Half-texel grid shift | `visual_grid_shift_is_the_half_texel_at_the_origin` reads 0 | W-10 |
| Nested-pitch border step (DO27 sign-off E1) | `pitch_seam_hop_at_full_speed_le_5cm_per_tick` on a Washboard-sized fixture (fails first on a fallback cooked without the § 3 seam rule) | W-10, W-29 |
| Blocker generation | `a_blocker_closes_and_reopens_in_one_generation` | W-13 |
| The 6 m segment sampler misses walls (DO27 sign-off E3, Q5 row 10) | `segment_finds_the_wall_the_6m_sampler_misses` (the DDA reports a hit the Stage A `segment_blocker` does not) | W-11 |
| Sub-millimetre slivers at mouths: lattice vertices off the mm grid of the ring (absorb review W2) | `support_never_falls_through_a_ring_edge`, `lattice_vertices_are_integer_mm_and_shared_with_the_rings` (fail first on the `f32` lattice `−W/2 + i·W/(n − 1)`) | W-10, W-11 |
| A ray through a seam edge can miss both triangles (Möller–Trumbore is not watertight; ADR-8084, conflict C2) | `rays_through_mouth_seam_edges_hit`: rays aimed at every ring edge and vertex of the fixtures, from both sides, must hit. If it fails on E3's test, the fix is a § 14 bit change that needs DO27's yes | W-11 |

## 14. The Stage B re-pin protocol (ADR-8019 A-10)

1. A Stage B change to anything on the lockstep path names its issue (a row of § 13.3, or a new row added there by the
   same change) and lands with a **new pin** that fails on the base. A change with no named issue is refused at
   review.
2. Stage B lanes land on the v0.6 line behind the `stage_b` cargo feature of `regia_world_geom` (off by default), so
   Stage A's API and pins stay intact while Stage B grows. **The feature is additive only** (ADR-8084, review W5).
   Cargo unifies features per build, so a client that links `regia_terrain` (whose layers need Stage B) and a
   headless server that does not build the crate with different features. That is safe because:
   - `stage_b` only adds items;
   - no item outside the Stage B modules reads `cfg(feature = "stage_b")` (`compat` never does);
   - Stage A arithmetic is identical with and without it.

   `stage_a_bits_are_the_same_with_and_without_stage_b` (§ 13.2) pins this, and `det-scan` refuses a
   `cfg(feature = "stage_b")` in `compat`.

   **The query-bits version** (ADR-8084, review W5). The handshake compares cooked bytes through `content_hash`. A
   Regia patch that moves query bits (a rule 1 or rule 5 change) would meet an older build with an equal
   `content_hash` and desync. So:
   - every change that moves a bit for the same cooked bytes bumps `regia_world_geom::QUERY_BITS_VERSION` (§ 5.2),
     and every change to `regia_detsim::math` bits bumps `regia_detsim::MATH_BITS_VERSION`;
   - the lockstep handshake (N224) compares both with `content_hash`;
   - a host with its own sim version (DO27) sends them as well, and engine-native lockstep games get the check with
     no work of their own.

   The re-pin of rule 1 includes the bump, and review refuses a query-table change without one.
3. When W-29 is green, DO27 applies **all** of Stage B in one sim-version bump on a branch: it switches to the native
   API, records its new pins (the fight checksum and the others of § 13.1's DO27-only row) and bumps its sim version.
   Old replays play on old builds (owner, 2026-11-16). The bump lands when W-29 is green and outside an alpha week.

   **Two bumps on DO27's side, one from Regia (DO27 sign-off E7, ADR-8072).** The 5fa1f1d69 revision timed the
   Stage B bump "with the T3 swap"; that cannot hold, because T3 lands in DO27 now as `ironreach/1.58`, on DO27's own
   code, before Stage B exists. So DO27 bumps twice: **1.58** (T3, now: DO27's own pre-swap bump, not a Regia bump)
   and **one Stage B bump** when W-29 is green, outside an alpha week. Regia's Stage B is still **one** sim-version
   bump (owner decision, 2026-11-16): every Stage B change reaches DO27 in that single bump, never piecemeal. Stage A
   stays bump-free only because it copies the T3-rewritten modules from `regia-absorb-2` (§ 4, E2): DO27's
   "DO27-only" pin values (§ 13.1) are its **1.58** values, and the T3 lane confirms whether the fight checksum
   0xbd96_cd18_09dc_958d moved at 1.58 (if it did, § 13.1 records the 1.58 value).
4. After the bump the Stage A module is deprecated for one Regia minor release, then removed (an Upgrade line).
5. Any later bit change is a new numbered issue, a new pin and a new DO27 sim-version bump; never a silent restyle.
   The `GeomState::state_hash` fold (§ 5.2: tags, field order, XOR, final fold) and the snapshot layout are on the
   lockstep path: a change to either is such a bit change (E4). DO27 adds `state_hash` to its checksum as one more
   little-endian u64 term of its FNV-1a-64 fold (`ir-core/src/abi.rs:3456`) at the Stage B bump; T3 has no geometry
   term today (its geometry is immutable and craters are not a checksum term).

## 15. The T3 contract: what T3 settles

DO27's TUNNELS_T3 lane built the reference query (`lane/tunnels_t3` @ 6fd2effb: `crates/ir-content/src/surface.rs`,
the kernel, 570 lines; `crates/ir-content/src/tunnels.rs`, triangle tests, index, push, segment;
`crates/ir-sim/src/tunnels.rs`, the consumers; `spec/maps/tunnels-t3-surface-query.md`). DO27's sign-off answer
(§ 16) reports what it settles, and this section copies that report. T3's final test suites were still running at the
answer, so **every item is to be confirmed by T3's handover**: line numbers and values are 6fd2effb's and are
re-cited against the `regia-absorb-2` tag (§ 4, E2) once DO27 cuts it. Items marked **open** are not settled by T3 at
all; W-11 (and W-30 for the capsule) settles them. W-11 records each item's final state here.

1. **Query semantics. Settled** (to be confirmed by T3's handover). As § 5.3 (E3): T3's support rule, the span
   element, one kernel. The column build at `surface.rs:169-269`: lids read first in triangle order; each support
   floor's ceiling is the lowest lid more than 0.05 m (`lid_gap_m`) over it; floors within 1e-3 m (`same_height_m`)
   merge, the first stands; spans are insertion-sorted ascending and equal heights keep insertion order. `support`
   and `surface_at` are two calls on one kernel.
2. **Vectors. Open: only the reference vectors will settle it.** T3 has no vector file. It has 9 unit tests in
   `surface.rs`, the host-step gate `crates/ir-core/tests/tunnel_t3.rs`, `tunnel_sweep`, and two run hashes
   (bore_twelve `1bf9154f9164c5e0`, switchbacks `a73431db19d8cd80`), all on x86-64 Linux only. DO27 proposes this
   format, for the T3 lane to emit:
   - mesh-only rows (no terrain) on triangles first rounded to mm and read back as `(mm as f32) / 1000.0`, so
     Regia's cooked input is bit-equal: `x, z, y` in, then floor `y`, ceiling `y` and surface kind out, f32 as hex
     bits;
   - maps: Switchbacks, The Cinder Steps layout, `synthetic_four_decks`;
   - rows with terrain pin the span chosen (its index and kind), not height bits, because the ground changes from
     the 5 m grid to the lattice.

   W-11 adds the rows to the § 13.2 query table unchanged and runs them on every § 13.2 runner.
3. **Arithmetic. Settled for the floor, the push and the segment** (to be confirmed by T3's handover).
   - Plane height (`tunnels.rs:1008`): `n = (b − a) × (c − a)` unnormalized;
     `y = a.y − (n.x·(x − a.x) + n.z·(z − a.z)) / n.y`. No square root reaches `y`. The 5fa1f1d69 formula used the
     normalized cooked normal and gives other bits: **Regia takes T3's order** (§ 5.3 rule 2; the unnormalized
     components are cached at load).
   - Not a floor or roof: `|n.y| < 1e-6`, or `|n| < 1e-8`. The reported normal (`SurfaceHit::normal`) is normalized
     with IEEE `f32::sqrt` and flipped to face up.
   - Point in triangle (`tunnels.rs:1035`): barycentric, degenerate when `|d| < 1e-12`, inside when all three are
     ≥ −1e-5. **Superseded** by Regia's ownership rule (§ 5.3 rule 2, E3).
   - Epsilons: 1e-3 same height, 0.05 lid gap, 1e-9 segment determinant, 1e-8 squared wall length, 1e-4 and 1e-6 in
     the wall span.
   - Square root: IEEE `f32::sqrt` only (the reported normal, the wall's horizontal normal, the segment length). No
     `regia_detsim::math` call and no trigonometry on the query path. DO27's T3 sign-off addendum (2026-11-16)
     confirms this rule; DO27's own "no sqrt" text was wrong.
4. **Edge ownership and tie order. Settled** (§ 5.3 rules 1 and 2, E3). Vector rows that sit exactly on an edge may
   differ within 1e-3 m and are regenerated by W-11.
5. **Index. Settled, and it must not matter** (to be confirmed by T3's handover). One 16 m XZ grid over the whole
   triangle set, origin at the mesh minimum less 8 m; walls filed 8 m wider, floors and ceilings 0.25 m wider;
   ascending triangle index per bucket; a second index over the conditioned support floors. Layers append triangles
   to the one set. T3 did not try 4 m; § 13.2 pins the results equal at 4 m and 16 m.
6. **Wall push. Settled for the disc** (`tunnels.rs:616-667`; § 5.4). One pass, no iteration. Candidates are the
   cell under the input point, ascending. A wall is skipped when its triangle extents miss the band, when its span at
   the contact (`wall_span_at`, at the current pushed position) misses the band, when the centre is `radius` or more
   from its line, or when the centre projects outside the segment (no end-cap push). Otherwise the centre moves to
   `radius` on the side it is already on. Non-finite input or `radius ≤ 0` returns the input. DO27's band is
   `floor_y + 0.5` to `max(y, floor_y) + 2.0`. **Open: the capsule's vertical rule is not a T3 result**; W-30 and
   its vectors settle it.
7. **Ground. Open: T3 is on the 5 m `i / n` grid with baked patches; only W-11's fit and the vectors settle the
   lattice form.** T3's terrain rules map like this:

   | T3 rule (`surface.rs:196-267`) | Stage B form |
   |---|---|
   | an open mesh floor under the terrain cuts it | the cooked mouth-cut bitmap |
   | a skylight patch dipping into a lidded span leaves no ground in the column (a body falls in) | the same bitmap: a shaft is an open cut |
   | a baked patch dipping into a lidded span is dropped for the grid's ground | gone from the ground query: one lattice, no patches; the `*_skylight` patch stays as on-foot prop data until W-30 (the walker floor is raised to the patch top when it is at most one step, 0.55 m + 0.02 m, over the feet; no step-down limit) |
   | ground between a span's floor and its lid is lifted onto the lid | a cook error (`above_ground_outside_cut` and `ground_inside_span`, § 10), never a run-time rule |
   | weld and trim at load | cook steps (§ 9.2, E6) |
   | craters only on the heightfield floor | D1 edits on the ground only (§ 6) |
8. **Bad input. Settled in kind** (to be confirmed by T3's handover). A non-finite or off-mesh point reads no mesh
   candidate and gets the heightfield alone; an empty mesh is the heightfield bit for bit
   (`no_mesh_is_the_heightfield_and_nothing_else`). DO27 relies on no particular answer for a non-finite point, so
   § 5.3 rule 7 stands.
9. **Allocation. Settled.** No heap on the sim path: `[Span; 12]` and `[f32; 16]` on the stack (Regia: `SpanBuf`,
   16). `mesh_spans` (camera only), `segment_blocker` and the old `floors_at` allocate; none is on DO27's sim path
   after Stage B.
10. **Cost and mirror. Open.** T3 has not measured cost per call. A call reads one bucket of ceilings and one bucket
    of support floors. The Python mirror is `tools/ops/tunnel_surface_scan.py` (388 lines, an offline scan, not a
    test suite); W-11 rewrites its cases in Rust and W-31 measures the cost.
11. **Copy manifest. Superseded** (DO27's T3 sign-off addendum, 2026-11-16) by the copy manifest in DO27's
    reference-query handover (one SHA, line ranges); the list below is the earlier form. `ir-content/tests/t3_vectors.rs`
    is requirements only: W-11 mirrors it as its vector generator. `STEP_BOUND_MM` is 50 at that SHA (was 200). The
    vectors format `do27-t3-vectors/1` (four fixtures) and the per-`column` cost are in the same handover; the
    Switchbacks fight pin 0xbd96_cd18_09dc_958d and all twelve LOCKSTEP_050 300-tick checkpoints are unchanged by T3.
    The earlier list:
    - *as-is*: `ir-content/src/surface.rs`.
    - *adapt*: `ir-content/src/tunnels.rs` (the index, `support_floors`, `welded`, `query_ground`, the push, the
      segment), `ir-sim/src/tunnels.rs`, `ir-core/src/hover_ground.rs`.
    - *requirements only*: `ir-core/tests/tunnel_t3.rs` (becomes `mouth_hop_at_full_speed_le_5cm_per_tick`),
      `tunnel_sweep.rs`, `tools/ops/tunnel_surface_scan.py`.

Presentation `surface_at` reach (addendum, 2026-11-16): DO27's camera reads `surface_at` up to 12 m below the eye
(+0.5 m above it). In Regia that reach is the caller's range and tolerance argument, never a kernel constant.

## 16. DO27 API sign-off

**Sign-off record** (the build lanes W-01, W-06 and W-11 read this line before they start):

DO27 API sign-off: signed 2026-11-16 (20261116-TUNNELS_T3-world-geom-api-signoff.md), spec revision 5fa1f1d69; exceptions: E1 nested-pitch seam rule (Q2), E2 Stage A source pin moves to the T3 commit plus three missing tilt seams (Q4), E3 support rule and span buffer follow T3 (Q5), E4 state_hash O(1) plus snapshot (Q7), E5 D1 edits leave mouth-ring vertices alone (Q8), E6 content_hash covers settings and ground mode (Q9), E7 two bumps, not one (Q10)

Amendment 1, 2026-11-16 (ADR-8084; absorb review rows W1-W5 and W7-W10): changes the contract signed at 5fa1f1d69
with E1-E7. Each change is a row in the lead's forwarding file `reports/world-geometry-api-amendment-1.md`. **DO27
re-signed it on 2026-11-16 (dated line below).** Stage B lanes build to the amended text, and W-06, W-10, W-11 and W-12 do not merge before DO27
re-signs or names the rows it refuses. Three rows touch signed exceptions:
- C1: E1's per-tile levels stand. The review's single map-wide `k` is not adopted; only the authored mode and its
  `max_level` cap are new.
- C2: E3's Möller–Trumbore segment test stands. A watertight test at seam edges is offered for DO27's yes if
  `rays_through_mouth_seam_edges_hit` fails.
- C3: E1's 1e-4 m bound across a pitch border cannot be met with a millimetre override. The spec carries 1 mm, and
  DO27 confirms or picks the stitch-fan alternative of ADR-8084.

2026-11-16, amendment 1 re-signed (DO27 `20261116-TUNNELS_T3-world-geom-amendment-1-signoff.md`, as summarized in the
lead's v06n merge list; the lead adds the verbatim line from that file): no row refused; E1-E7 stand; C1
confirmed; C2 accepted with conditions; C3 option (a), 1 mm (E1 rule 4); DO27's conditions A1-A3 are in that file.
ADR-8084 is accepted with A1-A3. T3 sign-off addendum, same day (`20261116-TUNNELS_T3-signoff-addendum.md`): no
exception added or withdrawn; its text fixes are applied to § 15 items 3, 7 and 11 and the `surface_at` reach note.

No answer is "disagree"; every exception is "agree with change", and the sign-off holds once this spec carries the
change. The exceptions are written into this spec (table below) and recorded as ADR-8072, which amends ADR-8019. Every
later API change adds a dated line under the record. Stage A needs no sign-off to start (its API is DO27's own code);
Stage B's public API does.

| Exception | DO27's question | Where this spec carries it | Lanes that need it |
|---|---|---|---|
| E1 nested-pitch seam rule | Q2 | § 3 (*Nested-pitch seam rule*), § 10 `ground_pitch_seam`, § 13.3 | W-06, W-10 |
| E2 Stage A source pin and the tilt seams | Q4 | § 4 (*Source pin*), § 4.1, § 4.2, § 4.3 | W-01, W-02, W-03 |
| E3 support rule and span buffer follow T3 | Q5 | § 5.1, § 5.3, § 5.4, § 5.5, § 5.6, § 7, § 8 | W-11 |
| E4 `state_hash` O(1) plus snapshot and restore | Q7 | § 5.2, § 14 | W-12, W-13 |
| E5 D1 edits leave mouth-ring vertices alone | Q8 | § 6, § 13.3 | W-12 |
| E6 `content_hash` covers settings and ground mode | Q9 | § 5.1, § 5.2, § 9.2, § 10 | W-06, W-09 |
| E7 two bumps on DO27's side | Q10 | § 14 | W-29 |

The questions DO27 answered (the packet repeats them with context; answers in DO27's file, summarized above):

1. Crate and module split: `regia_world_geom::compat` (Stage A) and the crate root (Stage B); `regia_nav::det` for the
   tunnel nav half.
2. The ground convention (A-7): the lattice at the visual pitch (amendment 1: the heightfield's sample pitch on integer mm, ADR-8084), `i / (n − 1)`, the `(x0, z1)–(x1, z0)` diagonal, u16
   counts with a cooked `offset_m` and `step_m`, D1 edits as absolute mm; the nested-pitch fallback over the § 11 budget.
3. Integer cooked geometry (i32 mm, 1024 m tiles) with the one pinned conversion of § 3.
4. The Stage A seams of § 4.2 and the `HoverGroundBody` field set of § 4.3.
5. The Stage B types and calls of § 5, `FloorBuf`'s capacity (16; now `SpanBuf`, E3), `GeomSettings` and its DO27 defaults.
6. That hover rules (`LEDGE_M`, `HULL_WALL_M`, the tilt samples) stay outside the query (DO27's sim or
   `regia_detsim::hover`).
7. `GeomState::state_hash` as the geometry's term in DO27's checksum.
8. The blocker and D1 edit calls of § 5.7 and § 6, including `min_cover_mm` and the ±1000 m clamp.
9. The formats of § 9 and the `do27-tunnels/1` importer; DO27 stops shipping `tunnels.json`, the collision mesh, the
   mouth-patch file and the portal report once Stage B is in (handover § 6 item 1).
10. The re-pin protocol of § 14 and the timing of the one bump.

**ADR-7974's four open questions** (DO27 answered them in the same file; ADR-8072 records them):

1. *The Cinder Steps first:* the sim query and the mouth cut on the south-junction slice (about z −280 to +40,
   |x| ≤ 130: the Ledge Pit and its pit portal, the south junction at (0, −118) under the Ledge Bridge, the core's
   open run into the Great Pit, the nearest strand skylight shaft). It is W-10's and W-11's fixture before any
   presentation work; the ramp mouths, the on-foot pinch (W-30) and round 2's five levels come later.
2. *Mesh hand-off:* the `Passages` source through the `do27-tunnels/1` importer (§ 9.1).
3. *Lattice-snapped mouths:* accepted; the sim cuts on the same lattice (§ 3).
4. *Fixed filled-hole shapes:* accepted; a collapse or blast is a blocker flip on a pre-authored plug or mouth
   (§ 5.7, § 6).

## 17. Lanes

| Lane | Builds | Sections |
|---|---|---|
| W-01 | crate, `compat::{layout, pack}` from `regia-absorb-1`; `compat::{mesh, sampler}` (with T3's `surface.rs`) from `regia-absorb-2` once DO27 cuts it (E2) | § 4 |
| W-02 | `compat::sense` from `regia-absorb-1`; `compat::{ground, state, hover_ground}` from `regia-absorb-2` (waits for the tag; E2), with `TiltParams` and the copied `ground_tilt` | § 4 |
| W-03 | `compat::onfoot` from `regia-absorb-2` (waits for the tag; E2) | § 4 |
| W-04 | `regia_nav::det` tunnel nav (Stage A) | § 4.1 |
| W-05 | Stage A proofs | § 13.1 |
| W-06 | the two formats (settings and per-tile ground levels in the canonical section, E6; every byte the run time reads in that section, the lattice constants, the ring pins and the `GroundMode` field, ADR-8084), `mm_to_m` and `m_to_mm`, the importer, the weld and trim cook steps (E6) | § 3, § 9 |
| W-07, W-08 | the render cook and sections, portals, zones, plugs | § 5.6, § 9.2 |
| W-09 | the checks (with `ground_pitch_seam`, `ground_over_budget`, `ground_inside_span`, `header_mismatch`, `lattice_mismatch` and the exact `mouth_cut_mismatch`), `regia terrain check-layers` | § 10 |
| W-10 | the lattice ground (integer-mm vertices, cooked constants, the ring pin), the authored ground mode and ground tiles, the nested-pitch seam rule (E1) and the mouth cut (ADR-8084) | § 3 |
| W-11 | the T3 port: `support`, `surface_at`, `SpanBuf`, push, segment, placement (Fable; E3); lattice point location, the per-cell kind lists, the height pyramid and `QUERY_BITS_VERSION` (ADR-8084) | § 3, § 5.2-5.5, § 8, § 15 |
| W-12 | D1 edits (mouth-ring clip E5, tile promotion E1, the integer profile and `m_to_mm`, ADR-8084), the O(1) state hash and snapshot (E4), copy-on-write override blocks | § 5.2, § 6 |
| W-13 | blockers | § 5.7 |
| W-20, W-21, W-24 to W-27 | layered nav, VisCells, zones, portal sound, camera, overview | § 5.6 |
| W-28 | the agent surface and book page | § 12 |
| W-29, W-30, W-31 | the Stage B suite, the character mover, the budgets | § 11, § 13 |

### W-11 implementation checkpoint (2026-11-17; ADR-8246 proposed, not release acceptance)

The additive native kernel has ordered padded cell lists, a bounded dense lookup directory with a
sparse fallback, cached source-order planes and canonical directed edges, compact fixed crossing
buffers, cached integer-mm axis positions, and a conservative ground height pyramid. `QueryStats`
counts triangle crossing tests after bounds rejection (Switchbacks p99: 20 mesh-only / 18 canyon).
The C2-permitted sheared-edge ray test replaces native Möller–Trumbore; all exact mouth-edge ray
probes pass, while adjacent-ULP gaps still expose missing cooked joins. Compat remains unchanged.

The current experimental query version is 5 (base Stage A was already 2; provisional native 3,
raw-lid/C2 pins 4, pilot-only geometric floor correction 5). The native table matches on x86-64
Linux and arm64 macOS. This is determinism evidence, not T3-equivalence acceptance: the table's
vector adapter uses a provisional 65-sample flat lattice and does not yet include canyon cuts.
Full 4096-sample mouth fixtures are separately exercised by ring and hop tests, which fail.

The sole regenerated edge column is Cinder mesh column 602, in
`crates/regia_world_geom/tests/fixtures/regia/t3-edge-vectors/cinder_steps_mesh.json`, with `regia`
provenance. Its inputs, local conditioning and ground height are unchanged, and every changed
height is within 1 mm. The other T3 rows remain untouched. Non-edge ground lifting, W-06/T3
conditioning differences, input-cell wall invariance, and source-order overlap behavior need the
lead's reconciliation. The 150 ns / 0.25 ratio and 34 MB bars remain unchanged and unmet.
