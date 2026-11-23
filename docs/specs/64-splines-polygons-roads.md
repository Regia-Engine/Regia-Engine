# Spec 64: Splines, polygons, modules along paths and roads

Status: proposed (ADR-1596).
Reviewed: integ/specs (ADR-1585), 2026-10-28. Built baseline: `regia_terrain::road` (straight decal and ribbon), `regia_framework::shots::spline` (Catmull-Rom), `regia_terrain::edit::apply_stroke`; no general spline, `Shape`, polygon or road-profile code exists. Part of the procedural content graph (spec 62; node conventions in spec 63 § 1). Behavior
written from the public-sources spec set, area `runtime-procedural` (ADR-1526, behavior only); the spline model, the module
pattern language, the road design and all names are Regia's own.

## 1. Purpose

Paths drive a large share of level content: roads, fences, walls, rivers, power lines, field borders, city blocks. This
spec adds:
1. a general **spline** component that any system can read (`regia_scene::spline`), with exact evaluation, arc length,
   frames and per-control-point attributes;
2. 2D **polygons** with robust boolean operations and offsetting;
3. procedural nodes that sample, build, split, intersect, offset and clip splines and polygons;
4. **modules along a path**: a small pattern language that fits posts, panels, gates and ends onto a spline;
5. **spline-deformed meshes** (rails, pipes, walls, road pieces);
6. **roads**: a profile asset and a starter graph that conform a road ribbon to the terrain, cut junctions and optionally
   flatten the terrain under the road at bake time.

Baseline in the repo:
- `regia_terrain::road`: `RoadDecal` (straight segments drawn in the terrain shader) and `RoadRibbon` (a mesh whose
  vertices sit on `TerrainMap::sample_bilinear` plus a small lift). Kept; roads here generalize them to curved splines.
- `regia_framework::shots::spline`: centripetal Catmull-Rom used by camera paths (`PathInterp::CatmullRom`). The general
  spline reuses that math for its automatic tangents: move the segment evaluation to `regia_scene::spline` and re-export it
  from `shots` (no behavior change for camera paths; their tests must stay green unchanged). The moved code takes the knot
  step as a parameter: `shots` keeps its current `powf(0.5)` step, the general spline uses `sqrt` (α = 0.5 is a square root;
  `sqrt` is IEEE-exact, `powf` is libm, § 8). (ADR-4764 (6) later moved `shots` to `sqrt` as well, for the same
  determinism reason; the step stays a parameter.)
- `regia_terrain::edit`: dirty-rect height edits with undo (`apply_stroke`, `TerrainEdits`), used by road flattening.
- `regia_render::gizmos::curves`: curve drawing for the editor and debug overlay.

## 2. Spline (`regia_scene::spline`, layer 1)

### 2.1 Model

```rust
/// A piecewise cubic curve in the entity's local space.
/// `Default` is hand-written (empty, open, version 1); `SplinePoint`'s default has `scale` (1, 1) and mode `Auto`.
#[derive(Component, Reflect, Clone)]
#[reflect(Component, Default)]
pub struct Spline {
    pub points: Vec<SplinePoint>,
    pub closed: bool,
    pub up: SplineUp,                     // RotationMinimizing (default) | WorldY
    pub attrs: Vec<SplineAttrDecl>,       // per-point custom attributes: (name, ty: F32 | Vec3 | Name)
    pub format_version: u32,              // 1
}

#[derive(Reflect, Clone, Default)]
pub struct SplinePoint {
    pub position: Vec3,
    pub tangent_in: Vec3,                 // used by Free / Aligned
    pub tangent_out: Vec3,
    pub mode: TangentMode,                // Auto (default) | Linear | Free | Aligned | Step
    pub roll_deg: f32,
    pub scale: Vec2,                      // (width, height) factors, default (1, 1)
    pub values: Vec<SplineValue>,         // one per `attrs` entry, same order
}
```

- Segment `i` runs from point `i` to point `i + 1` (and from the last to the first when `closed`). A spline with fewer than
  2 points has length 0 and evaluates to its single point (or the origin); nodes treat it as empty with warning
  `spline_degenerate`.
- Each segment is a cubic Hermite curve between the end positions with outgoing tangent `T_out(i)` and incoming tangent
  `T_in(i + 1)`:
  - `Auto`: tangent from centripetal Catmull-Rom over the neighbors (α = 0.5); end points of an open spline use the
    one-sided difference; both sides equal.
  - `Linear`: the tangents point at the neighbor (straight segment); a `Linear` point next to a curved one gets a corner.
  - `Free`: `tangent_in` and `tangent_out` as stored.
  - `Aligned`: `tangent_out` as stored, `tangent_in` = its direction scaled to the stored `tangent_in` length.
  - `Step`: the segment starting here is constant at this point's position (used for discrete attribute tracks only;
    position paths use it rarely).
- Values between points: `roll_deg`, `scale` and numeric custom values are interpolated linearly in the segment
  parameter; `Name` values hold the start point's value.

### 2.2 Arc length, frames and queries

- Arc length: each segment keeps a table of 32 uniform-parameter samples with cumulative arc length (each interval integrated by 4-point Gauss-Legendre on the speed, not chords); distance → parameter
  inverts the table with linear interpolation, then refines with two Newton steps on the true derivative. Error ≤ 1 mm on
  a 100 m segment with tangent lengths up to the segment length (test).
- Frames: `RotationMinimizing` propagates the up vector by the double-reflection method from the first point (initial up =
  world Y projected off the tangent, or world Z when the tangent is vertical); on closed splines the remaining twist at the
  end is distributed linearly over the length so the frame closes. `WorldY` uses world up with the tangent (sideways
  frame), falling back to rotation-minimizing where the tangent is within 1° of vertical. Roll is applied about the
  tangent after the frame.
- API (`impl Spline`, pure functions, `f32`, deterministic):
  `length()`, `segment_count()`, `segment_length(i)`, `at_distance(d) -> SplineSample`, `at_param(t)` (t ∈ [0, 1] over the
  whole spline by length), `at_key(k)` (k = segment index + local parameter), `closest(p) -> ClosestHit { key, distance_m,
  along_m, position }` (per-segment coarse search over the table, then Newton refinement, ties by lower key),
  `to_polyline(max_error_m) -> Vec<Vec3>` (adaptive subdivision), `bounds()`. `SplineSample` holds `position`, `tangent`
  (unit), `up`, `right`, `roll_deg`, `scale`, `curvature` (1/m), `along_m`, `key`, and custom values.
- World-space evaluation applies the entity's `GlobalTransform` (non-uniform scale is applied to positions and tangents;
  arc length is computed in world space for world queries).
- Format: the component is saved in scenes with `format_version` 1, pinned type path `regia_scene::Spline`, check-formats
  name `spline`. Migrator `Spline::migrate` (none yet).

### 2.3 Agent surface (`book/agents/splines.md`)

| Method | Class | Params | Result |
|---|---|---|---|
| `spline.get` | read | `{ entity, world?: bool }` | points, closed, attrs, length, segment lengths |
| `spline.set` | edit_scene | `{ entity, points?, closed?, attrs? }` | `{ length }` (validates: finite numbers, ≤ 4096 points) |
| `spline.edit` | edit_scene | `{ entity, ops: [insert {index, point} \| remove {index} \| move {index, position} \| set_mode {index, mode} \| set_value {index, attr, value}] }` | `{ length, points }` |
| `spline.sample` | read | `{ entity, at: [distance_m...] \| count: n, world?: bool }` | samples |
| `spline.closest` | read | `{ entity, point: [x,y,z] }` | the `ClosestHit` |

Errors (answered as `F0002`, or `F0003` for an unknown entity, with `data.reason` = the code; lowercase, listed on the page; a row in
`book/COVERAGE.md` covers every `spline.*` method): `not_a_spline`, `spline_index_out_of_range`, `spline_attr_unknown`, `spline_too_many_points`. Edits are undoable in
the editor (they go through the editor command stack when the editor is the host) and mark dependent generators stale
(spec 62 § 5.3).

### 2.4 Shape component (`regia_scene::shape`, layer 1)

A simple volume that generators, biome areas, exclusions and tools use as a region. There is no such component in the
repo yet; this spec adds it next to `Spline`.

```rust
/// A region volume in the entity's local space (not a collider).
#[derive(Component, Reflect, Clone)]
#[reflect(Component, Default)]
pub struct Shape {
    pub kind: ShapeKind,      // Box { half_extents: Vec3 } (default 1 m cube) | Sphere { radius } | Capsule { radius, half_height }
                              // | Hull { points: Vec<Vec3> } (convex hull of the points; ≤ 256 points)
    pub edge_m: f32,          // soft band outside the surface for region values (spec 63 § 2), default 0
    pub format_version: u32,  // 1
}
```

`Default` is a hand-written impl (box with half extents 0.5, `edge_m` 0, version 1). Saved in scenes with a pinned type
path (`regia_scene::Shape`), check-formats name `shape`, migrator `Shape::migrate` (none yet). API: `contains(p)`,
`signed_distance(p)` (exact for box, sphere and capsule; for hulls the maximum plane distance, exact inside and a
lower bound outside (it can understate the true distance, e.g. 9 vs about 12.7 at (10, 10, 0) for a unit cube)), `aabb(transform)`. Region values use the signed distance with the entity's `GlobalTransform` (non-
uniform scale: distance computed in local space and scaled by the smallest axis scale, documented as approximate).
Agent: shapes are edited through the generic `scene.*` component methods; the editor draws them as wire volumes.

## 3. Polygons (`regia_procgen::polygon`)

- `PolygonData { transform: Transform, rings: Vec<Vec<[f64; 2]>> }`: rings in the local XZ plane; ring 0 and other
  counter-clockwise rings are outer, clockwise rings are holes (normalized on construction: orientation fixed by signed
  area, duplicate consecutive vertices removed, rings with fewer than 3 vertices or zero area dropped).
- **Robustness:** coordinates are snapped to a 1 mm integer grid (`i32`: ±2 147 km from the polygon's own origin; rings beyond that fail with `polygon_range`) for every boolean and offset operation, which makes
  results identical on all platforms. Self-intersecting input is resolved by the even-odd rule.
- Operations: `union`, `intersection`, `difference`, `xor`; `offset(distance_m, join: miter|round|square, miter_limit)`;
  `split_by_polyline` (cut a polygon with an open polyline into pieces, used for city blocks); `contains(p)`;
  `area()`; `triangulate()` (ear clipping with hole bridging; deterministic vertex order). As built (ADR-5645 (1)): the
  four booleans are one `boolean(other, BoolOp)` and the polyline split is `cut(polylines, gap_m)`.
- Implementation: a polygon clipping library behind the module boundary. Decision (ADR-1596, confirmed ADR-1585): use the `i_overlay` crate
  (pure Rust, so no NDK or system-library concern; integer-snapped boolean and offset operations; no cargo feature gate,
  because `Anchored` runtime generation uses it). The builder confirms its licence is on the `docs/03` § 4 allow list
  (MIT or Apache-2.0 expected) and that `cargo deny check` passes, then adds the `docs/03` row and the version; if either
  fails, implement the Martinez-Rueda sweep (2009) on the integer grid in `regia_procgen::polygon::clip`. Either way the module API above is
  the contract and the tests in § 9 apply.

## 4. Spline and polygon nodes (spec 63 conventions)

| Id | Pins | Settings (default) | Behavior |
|---|---|---|---|
| `sample.spline` | in (Spline), out | `by: step\|count\|per_segment (step)`, `step_m: 2`, `count: 10`, `per_segment: 4`, `fit_ends: true`, `start_m: 0`, `end_m: 0`, `place: curve\|band\|inside (curve)`, `band_m: 1`, `spacing_m: 2`, `jitter: 0`, `write: [along, distance_m, key, tangent, curvature, turn_deg]` | `curve`: points on the curve at the chosen spacing between `start_m` and `length − end_m`; `fit_ends` adjusts `step_m` so both ends get a point (step = L / round(L/step)). `band`: a grid of rows across the curve, `band_m` × the interpolated width (scale.x) wide, rows every `spacing_m`. `inside`: a closed spline's interior sampled like `sample.region` (error `spline_not_closed` on an open spline). Rotation = the spline frame. Each written attribute: `along` (0-1 by length), `distance_m`, `key`, `tangent` (Vec3), `curvature`, `turn_deg` (signed heading change since the previous sample). Custom spline values are interpolated onto the points. `jitter` offsets along the curve by up to `jitter × step/2` (slot 0) and never past the ends |
| `spline.from_points` | in (Points), out | `closed: false`, `mode: Auto\|Linear`, `tangent_attrs: none\|(in, out)`, `order: index\|nearest_chain` | One spline per input item through its points; `nearest_chain` orders points by greedy nearest neighbor from the first point (ties by index). Point attributes become spline values |
| `spline.simplify` | in, out | `merge_m: 0.05`, `tolerance_m: 0.1` | Drops control points closer than `merge_m` to the previous kept one, then Ramer-Douglas-Peucker on the remaining control positions with `tolerance_m` (endpoints and `Free`/`Aligned` points always kept) |
| `spline.split` | in, out | `at: distances\|every\|points\|attr (every)`, `distances_m: [f]`, `every_m: 50`, `attr: sel` (a control-point value, non-zero = cut; ADR-5645 (3)) | Cuts each spline into pieces; each piece keeps its frame continuity (tangents preserved at cuts); item scope gets `piece` index |
| `spline.intersect` | in*, out_splines, out_points | `plane: xz\|3d`, `tolerance_m: 0.05`, `insert: true` | Finds crossings between all splines of the inputs (and self-crossings), in XZ (default) or 3D within tolerance. `insert` adds control points (mode `Free`, tangents from the curve) at crossings; `out_points` gets one point per crossing with attributes `a`, `b` (item indices), `key_a`, `key_b`. Ordered by (a, b, key_a) |
| `spline.offset` | in, out | `distance_m: 2` (positive = right side), `vertical_m: 0` | Parallel curve: each control point moves along its right vector; tangents scaled by the local curvature ratio so the offset stays smooth; loops created by offsets larger than the radius are removed (the offset is rebuilt from `to_polyline` and simplified) |
| `spline.reverse` | in, out | — | Reverses point order and swaps tangents |
| `spline.clip` | in, polygons, inside, outside | — | Splits splines at polygon boundary crossings and routes pieces inside or outside |
| `polygon.from` | in (Points, closed Spline), out | `mode: hull\|ordered (hull)`, `max_error_m: 0.1` | `hull`: 2D convex hull of the points' XZ (Andrew's monotone chain); `ordered`: the points in order as a ring; closed splines are flattened with `to_polyline(max_error_m)` |
| `polygon.bool` | a*, b*, out | `op: union\|intersection\|difference\|xor` | Boolean on all rings of `a` against all of `b` (inputs of each pin unioned first) |
| `polygon.offset` | in, out | `distance_m`, `join: miter\|round\|square`, `miter_limit: 2` | Grow (positive) or shrink; collapsed parts disappear |
| `polygon.cut` | in, cutters (Spline), out | `gap_m: 0` | Splits polygons along the cutter polylines and removes a band `max(gap_m, 0.001)` meters wide (one grid step, so `gap_m` 0 still deletes 1 mm and the pieces do not share that edge). Output: one item per piece, ordered by piece centroid (x then z) |
| `polygon.to_spline` | in, out | — | Each ring becomes a closed `Linear` spline |

## 5. Modules along a path (`spline.modules`)

Places a sequence of modules (meshes or prefabs with a length along the path) to fill a spline: fences with posts, walls
with pillars and gates, guard rails, hedges.

Settings:

```ron
(
    modules: {
        "post":  (mesh: "fence/post.glb",  length_m: 0.2),
        "panel": (mesh: "fence/panel.glb", length_m: 2.0, stretch: (0.85, 1.15)),
        "gate":  (prefab: "fence/gate.prefab.ron", length_m: 3.0),
        "end":   (mesh: "fence/end_post.glb", length_m: 0.25),
    },
    pattern: "post repeat(panel post) end",
    fit: Stretch,          // Stretch | Scale | Gap
    align: Curve,          // Curve (module axis follows the tangent) | Chord (straight between its ends)
    forward_axis: X,       // the module's length axis in its mesh space
    follow_ground: true,   // drop each module's ends onto the run's Ground
)
```

Pattern language (Regia's own):
- A pattern is a sequence of terms separated by spaces. A term is a module name, or a function:
  - `repeat(seq)` — as many copies of `seq` as fit (at least 0);
  - `repeat(seq, n)` — exactly `n` copies;
  - `first(a, b, ...)` — the first alternative that fits in the remaining length (alternatives are sequences);
  - `pick(a: w, b: w, ...)` — weighted random choice among sequences (draw from the spline item's seed and the term's
    position in the expansion, so the same spline gets the same choices);
  - `gap(m)` — empty space of `m` meters.
- The pattern can also come from a per-item `Name` attribute (`pattern_from: sel`), so one graph serves different fences
  on different splines.
- Parse errors name the column: `pattern_syntax` (`data.column`). Unknown module names: `pattern_unknown_module`.
- Size (v0.5.1): a pattern whose counted repeats would expand past 400 000 terms (`MAX_EXPANDED_TERMS`, four times the
  100 000 modules one spline may get; wrappers and alternatives count: a `pick` or `first` of k alternatives counts
  1 + all k per copy, so `repeat(pick(a: 1, b: 1, c: 1, d: 1), 100000)` counts 500 000 and is refused although it would
  resolve to 100 000 modules) fails with `pattern_too_large`, counted before
  anything is built: at compile for a literal `pattern`, when the item is reached for `pattern_from`.

Fitting, deterministic, per spline (or per piece after `spline.split`):
1. Expand fixed terms and count the minimum length `L_min` (stretchable modules at their minimum stretch).
2. `repeat(seq)` expands to `k = floor((L − fixed) / len(seq))` copies, evaluated left to right; nested `repeat` is allowed
   one level deep (error `pattern_nesting` beyond that).
3. `first(...)` takes the first alternative whose length fits what remains after the rest of the pattern's fixed length.
4. Leftover `r = L − placed`: `Stretch` spreads `r` over stretchable modules in proportion to their length, within their
   stretch ranges (any rest goes to `Gap` behavior); `Scale` scales every module along its axis by `L / placed`; `Gap` adds
   equal gaps between modules.
5. If the pattern cannot fit even at minimum (`L < L_min`), the spline gets the pattern's first module only and warning
   `pattern_too_long`.

Output: one point per placed module at the module's start (rotation from the frame, `scale.x` = stretch), attributes
`module` (Name), `mesh` or `prefab` (Asset), `length_m`, `index`, `distance_m`; feed it to `spawn.mesh`/`spawn.prefab`, or
to `spawn.spline_mesh` (§ 6) to bend each module along the curve (ADR-5645 (2): there is no `deform` setting; the bend is
that node).

## 6. Spline-deformed meshes (`spawn.spline_mesh`)

Bends a mesh along a spline range: road pieces, pipes, rails, curbs, walls.
- Input: points from `spline.modules` or `sample.spline` carrying `mesh`, `distance_m` (start) and `length_m`, plus the
  spline item (pin `spline`). Settings: `forward_axis: X`, `segments_per_m: 1` (extra subdivision along the axis where the
  source mesh has long edges; 0 = keep the source vertices), `smooth_normals: true`, `uv_along: none|meters|stretch`,
  `material: a?`, `collision: none|mesh`.
- Each source vertex `v` with axis coordinate `s ∈ [s_min, s_max]` maps to the spline sample at
  `d = start + (s − s_min) / (s_max − s_min) · length`, then `position = sample.position + right·(v.side · scale.x) +
  up·(v.height · scale.y)`. Normals and tangents rotate with the frame; normals are re-normalized; with `smooth_normals`
  the seam vertices between consecutive pieces share normals so there is no shading crease.
- Output meshes are **generated meshes** of the run (spec 62 § 7.3): CPU vertex and index buffers in the cooked-mesh
  vertex layout, merged per (material, cell) up to 65 536 vertices per mesh, uploaded through the runtime mesh-creation
  path (`regia_asset` `Assets<T>::insert` plus GPU upload; if the plan item A1 "runtime asset creation" is not built yet,
  this spec's builder adds the minimal mesh builder it needs and records it in the ADR), stored in `.pgout` for baked
  generators (ADR-2003: f32 streams, not the quantized cooked layout, so a bake reloads bit for bit).
- Determinism: vertex order follows source vertex order per piece, pieces in point order.

## 7. Roads

### 7.1 Road profile asset (`<name>.road.ron`, format name `road`, `format_version: 1`)

```ron
RoadProfile(
    format_version: 1,
    width_m: 7.0,                              // carriageway
    shoulders_m: (0.5, 0.5),                   // left, right
    surface_lift_m: 0.02,                      // above the terrain (bounded as in regia_terrain::road)
    cross_slope_pct: 2.0,                      // crown fall from the center line
    material: "materials/asphalt.material.ron",
    shoulder_material: Some("materials/gravel.material.ron"),
    uv_m_per_tile: 8.0,                        // V coordinate in meters along the road / tile length
    samples_per_m: 0.5,                        // ribbon rows per meter (min 1 row per 4 m, plus rows at every control point)
    pieces: [ (mesh: "road/curb.glb", side: Left, offset_m: 3.5), (mesh: "road/curb.glb", side: Right, offset_m: 3.5) ],
    markings: [ (pattern: "dash(3, 9)", offset_m: 0.0, width_m: 0.15, color: (1, 1, 1)) ],
    terrain: (flatten: true, flatten_extra_m: 1.0, falloff_m: 6.0, carve_max_m: 3.0, fill_max_m: 3.0),
    junction: (margin_m: 1.0, material: None),
)
```

The road is a spline entity with a `RoadOf { profile: AssetRef<RoadProfile>, format_version: u32 }` component plus a generator (usually one generator per road
network, reading all road splines under it through `in.entities { tags: ["road"], as: splines }`). The starter graph
`starter/road_network.pgraph.ron` ships with the engine; the editor's road tool (spec 67 § 5) binds its own preset
`starter/tool_road.pgraph.ron`, built from the same road nodes (ADR-5645 (4)).

### 7.2 Road nodes

Every road node reads each spline item's profile from its item scope value `road_profile` (captured by `in.entities` from
the entity's `RoadOf`, spec 63 § 3), or from the node's `profile: a` setting when the item has none; an item with neither
fails with `road_profile_missing`. Settings shared by all road nodes: `profile: a?` (fallback profile). Node-specific
settings: `road.ribbon` `smooth_rows: 3` (1-15, odd); `road.markings` none beyond the profile; `road.junctions`
`snap_m: 0.5` (T-end detection slack beyond `width/2 + margin_m`), `min_arm_deg: 20`; `road.exclusion` `extra_m: 0` (added
to the profile's `flatten_extra_m`); `road.decals` `max_error_m: 0.25`.

| Id | Pins | Behavior |
|---|---|---|
| `road.ribbon` | splines, ground, out (generated meshes), out_edges (points) | Builds the carriageway and shoulders as one ribbon mesh per road: rows at `samples_per_m` plus every control point; each row spans the cross section with vertices at the center, carriageway edges and shoulder edges; heights follow the terrain under each vertex, then a smoothing pass along the road (moving average over 3 rows) keeps the surface from copying terrain noise, then the crown is applied, and finally every vertex is lifted to at least `ground + surface_lift_m` so the surface never dips under the terrain. UV: U across (0 to 1 over the carriageway), V = distance / `uv_m_per_tile`. `out_edges` gets points along both outer edges (for curbs, posts, scatter exclusion) |
| `road.markings` | splines, out | Builds thin marking strips (pattern `solid` or `dash(on_m, off_m)`) lifted 5 mm over the ribbon, as a generated mesh with the marking material (one per road) |
| `road.junctions` | splines, ground, out_meshes, out_splines | Finds crossings (`spline.intersect`, XZ) and T-ends (a spline end within `width/2 + margin_m` of another road): cuts the ribbons back from each crossing by half the other road's width plus `margin_m` (the cut splines go to `out_splines` for `road.ribbon`), then fills each junction with a patch polygon: the convex hull of the cut row ends, triangulated, heights from the cut rows (linear blend), material from the profile. Junctions of more than 4 arms are filled the same way; arms meeting at less than 20° get warning `junction_sharp` |
| `road.exclusion` | splines, out | A polygon per road covering carriageway plus shoulders plus `flatten_extra_m`, for `region.subtract` (keeps scatter off the road) |
| `road.decals` | splines, out | For hosts that draw roads in the terrain shader: converts each road into `RoadDecal` segments (polyline at 0.25 m error) |

### 7.3 Terrain flattening (bake only)

`terrain.flatten_road` (node; pins `splines`, `ground`, `out` (the splines, passed through); settings: none beyond the
profile) is allowed only when all of these hold, otherwise the run fails:
- the generator is `Baked` (else `flatten_runtime_forbidden`) and untiled or tiled without anchors (spec 66);
- the generator has `allow_terrain_edits: true` (spec 62 § 7.1) (else `terrain_edits_not_allowed`);
- the project allows it: `Regia.toml` `[procgen] terrain_edits = true` (default false; a project whose terrain heights are
  owned by a game simulation leaves it off, and then no graph can change the heightfield) (else
  `terrain_edits_not_allowed`).

It does not change the terrain while the graph runs (evaluation is pure, spec 62 § 5.1). It emits a **terrain edit**
output (spec 62 § 7.3): the road's target height into the terrain under the carriageway plus
`flatten_extra_m`, blending to the original over `falloff_m` with smoothstep, limited to `carve_max_m` down and `fill_max_m`
up (beyond that the road ribbon spans the gap and warning `flatten_limit` names the distance), as a rectangle of new
height samples. The apply step writes it through a new function `regia_terrain::edit::apply_height_patch(terrain,
rect, samples) -> HeightPatchUndo` (the existing `apply_stroke` takes brush strokes, not sample patches), which notes the
dirty rect for GPU re-upload and re-cook and returns the original samples. The edit is one undo entry in the editor and is
recorded in the `.pgout` (rect, original samples, new samples) so clearing the generator restores the terrain bit for bit.
Rules that keep it stable:
- **Order:** a run captures the terrain with the previous edit lifted out (original samples for the capture only, ADR-2003);
  the old edit's originals are written back when the new output publishes, so repeated bakes never accumulate and a failed
  run keeps the published ground.
- **Own edits are not dependencies:** terrain changes made by a generator's own edit never mark that generator stale
  (the dependency tracker ignores dirty rects tagged with the generator's guid); edits by other generators or by the
  author do.
- **Verify** restores the generator's own edit before re-running (spec 62 § 8).
- **Ownership (ADR-2003):** the rectangle is a bounding box, so a record owns only the samples it changed and restores one only
  while it still holds the record's value; an author's sculpt in the box stays, and marks the generator stale. Two generators
  may not own the same sample (`terrain_edit_overlap`).
- **Ribbon heights** are computed from the captured (unflattened) terrain plus the same target heights, so road and
  flattened ground agree.

### 7.4 Road agent notes

Roads use the generic `procgen.*` methods (spec 62 § 9) and `spline.*` above. `procgen.report` lists per road: length,
junction count, flatten rect and limit warnings.

## 8. Determinism

All spline, polygon and road math is `f32` (positions) with `f64` for arc-length accumulation and polygon snapping; no libm
transcendentals (frames use `sqrt` and `detmath`). Polygon booleans run on the 1 mm integer grid. Outputs are hashed in the
acceptance tests on Linux, Windows and Android aarch64.

## 9. Tests and acceptance

Unit tests (`regia_scene` spline, `regia_procgen` polygon and nodes; no GPU):
1. Spline evaluation: a straight two-point spline has length = distance and `at_param(0.5)` at the midpoint; a quarter
   circle of radius r built from two `Free` points with tangents of length 1.6569·r (the Hermite form of the 0.5523·r
   Bezier handle) has length within 0.1 % of π/2·r.
2. Arc length inversion error ≤ 1 mm over 1 000 fixed distances on a 100 m S-curve.
3. Frames: on a helix, the rotation-minimizing frame's twist per unit length matches the analytic value within 0.5°;
   a closed loop's frame at the end equals the start within 0.1°.
4. Camera paths: the existing `shots` tests pass unchanged after the Catmull-Rom code moves.
5. `closest`: for 1 000 seeded points near an S-curve, the hit distance is ≤ the distance to any of 10 000 dense samples
   plus 1 mm.
6. Polygons: union of two overlapping squares has the expected area exactly (integer grid); difference creates a hole;
   offset of a unit square by 1 m with round joins has area 1 + 4 + π within 0.5 %; a figure-eight input resolves by even-odd.
7. `polygon.cut` of a 100 m square by two crossing streets with `gap_m: 10` gives 4 pieces of 45 m × 45 m.
8. `spline.intersect` of an X of two straight splines gives one crossing at the center; `insert: true` adds one control
   point to each.
9. Module pattern: `post repeat(panel post) end` on a 21 m spline with 2 m panels (0.85-1.15 stretch), 0.2 m posts and a
   0.25 m end gives 9 panels and the panels stretched to fill exactly (sum of lengths = 21 m ± 1 mm); a 0.3 m spline gives
   only the first module and `pattern_too_long`; `first(gate, panel)` picks the gate when it fits; syntax errors report the
   column.
10. Spline mesh: a 1 m box bent along a 90° arc keeps its cross-section area within 1 % at every row; consecutive pieces
    share seam vertex positions exactly.
11. Road ribbon on a synthetic hill: every vertex ≥ ground + lift; the carriageway's cross slope is the profile's crown
    within 0.1 %; V coordinates increase monotonically along the road.
12. Junction: two crossing roads produce one patch whose boundary meets both cut ribbons within 1 mm; a T-end is detected.
13. Flatten: bake on a synthetic terrain changes heights only inside the rect; `clear` restores every sample bit for bit;
    baking twice gives the same terrain as baking once; the bake does not mark its own generator stale; `verify` passes
    after a bake; with `[procgen] terrain_edits` off (or `allow_terrain_edits` false) the run fails with
    `terrain_edits_not_allowed` and the terrain is untouched.
14. Shape: `signed_distance` of box, sphere and capsule matches analytic values at 1 000 seeded points within 1e-5 m.

Acceptance (sample level `samples/procgen_roads`): a road network of three roads with one crossing and one T, curbs from
`out_edges`, markings, a fence along a field polygon built by `polygon.cut`, rocks scattered with `road.exclusion` removing
them from the road. Through the agent: `procgen.run` → `ready`; `procgen.report` lists 2 junctions; `seeing.screenshot`
for review (no golden; lead TODO if one is wanted).

## 10. Platforms and performance

Pure CPU; all targets. Budgets (desktop reference, one core): sampling a 10 km spline at 1 m ≤ 5 ms; `closest` ≤ 20 µs per
query on a 200-point spline; polygon union of two 10 000-vertex polygons ≤ 30 ms; road ribbon for 10 km of road ≤ 60 ms;
`spawn.spline_mesh` 1 M output vertices ≤ 80 ms. Android ≤ 4×. Bench: `cargo bench -p regia_procgen --bench splines`.

As built (ADR-7238, lane post05-spline-biome-perf): `closest` skips segments by the box of their table samples and
returns the full scan's hit bit for bit; `spawn.spline_mesh` samples the curve once per distinct axis coordinate of
a piece and the run output hash is fed through a buffer. Measured speedups are in `docs/perf-log.md` (round
"post-v0.5 spline and biome perf").

## 11. Formats and migration

New: `Spline` component v1 (`spline`), `Shape` component v1 (`shape`), `RoadOf` component v1 (`road-of`), `road` asset v1,
the `[procgen] terrain_edits` key in `Regia.toml` (optional, default false; no version change needed for an optional key; the builder adds it to the project-manifest reader and to `regia new` templates as a commented line),
and `regia_terrain::edit::apply_height_patch` (additive API). `.pgout` (spec 62 § 8) carries
generated meshes and terrain edit records. No existing format changes; the camera path format (`shots`) is untouched. The
lead runs `cargo xtask check-formats --update` after registration.

## 12. Out of scope

Traffic lanes, lane graphs and traffic simulation (a later crowds/traffic item); bridges and tunnels generated from roads
(roads simply span gaps beyond the flatten limits); rivers with flow simulation (a river is a spline with a water material
ribbon today, using these nodes); spline physics (ropes, cables); 3D polygon booleans.

## 13. Guide pages

`book/agents/splines.md` (spline methods, sampling, edit ops, errors); a "Roads and fences" section in
`book/agents/procgen.md` (profile fields, starter graph, flatten rules, junction warnings).

## 14. Provenance

Behavior of spline and polygon operations and path-module placement in public user documentation of procedural
generation, via the public-sources spec set area `runtime-procedural` (set spec ids `runtime-procedural-node-reference`,
`-graph-authoring`, `-data-attributes`; their Sources pages; no node names, settings, grammar syntax or wording carried).
Algorithms: cubic Hermite and centripetal Catmull-Rom (Yuksel, Schaefer, Keyser, "Parameterization and applications of
Catmull-Rom curves", CAD 2011; existing camera-path code); rotation-minimizing frames by double reflection (Wang, Jüttler,
Zheng, Liu, "Computation of Rotation Minimizing Frames", ACM TOG 2008); Ramer (1972) and Douglas-Peucker (1973) polyline
simplification; Andrew's monotone chain convex hull (1979); Martinez, Rueda, Feito, "A new algorithm for computing Boolean
operations on polygons" (2009) or the `i_overlay` crate; ear clipping (Eberly, "Triangulation by Ear Clipping", Geometric
Tools 2002); smoothstep blending (textbook). The module pattern language, road profile and junction patch are Regia's
own design.

## Addendum (ADR-2731): `road.pieces`

The profile's `pieces` rows are placed by a separate node, so `road.ribbon`'s output does not move. `road.pieces` has pins `splines`,
`ground`, `out` (generated meshes) and settings `profile`, `smooth_rows`, `forward_axis`, `segments_per_m`, `material`, `collision`.
Each row repeats its mesh along the whole road, `round(length / mesh length)` copies and at least one, stretched to fit, bent as in § 6,
moved by `offset_m` to its `side` (Left is negative, as the ribbon's offsets) and laid on the surface `road.ribbon` builds at that offset.
A missing mesh warns `mesh_missing`. Pieces are not cut back at junctions.


### Public-page citations

Sources-section URLs recorded by the spec writer on 2026-11-08 (ADR-1526; ADR-1661).
These are documentation/reference citations, including public API-reference pages; only the
URLs were transcribed from the external set.

- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
