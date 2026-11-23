# Spec 63: Procedural node library

Status: proposed (ADR-1596).
Reviewed: integ/specs (ADR-1585), 2026-10-28. Built baseline: none of these nodes exists; the registry, data model and evaluator are spec 62. Part of the procedural content graph (spec 62, read it first). Behavior written from the
public-sources spec set, area `runtime-procedural` (ADR-1526, behavior only); node ids, pin names, settings, defaults and
algorithms are Regia's own.

## 1. Purpose and conventions

This spec defines every built-in node type of `regia_procgen::nodes`: its id, pins, settings with defaults, exact behavior
and edge cases, so a builder can implement and test each one without other sources. Spline, polygon, module and road
nodes are in spec 64; biome nodes in spec 65; tier nodes in spec 66; plant nodes (plant domain, pipeline only) in spec 68.
**Baseline.** Reused as built: `regia_core::math::Rng`, `regia_asset::data_table` (`DataTable<R>`, `from_ron_str`), `regia_render::quality::QualityTier`, `regia_physics` ray and overlap queries, `regia_terrain::TerrainMap`. **Formats and agent surface:** this spec adds no file format except the saved component `InstancedMeshSet` (§ 13.3, v1); node ids, pins, settings and ranges reach agents through `procgen.node_types` (spec 62 § 9, page `book/agents/procgen.md`, which carries the per-family catalog generated from the registry) and every node error code in this spec arrives as `data.reason` inside `data.errors`.

Every node type in this spec supports the `World` domain; the generic families (§§ 4 `gen.*`, 5, 6, 8, 9, 10, 11, 12)
also support `Plant` (spec 68 § 2).

Conventions used by every node unless its row says otherwise:
- **Per item.** A node processes each item of its main input (`in`) independently and emits one result item per input
  item, in input order, keeping the item's tags and item scope.
- **Pins.** `in` main input, `out` main output. Filters have `inside` and `outside` outputs. A pin written `in*` accepts
  several edges (spec 62 § 4.2). Pins that take regions accept any spatial kind (`region` pins, § 2).
- **Selectors** follow spec 62 § 3.5. `target` settings name the attribute a node writes; empty means "the same as the
  source selector" for in-place nodes, or the node's documented default name.
- **Randomness.** A node's draw for element `i` uses `Rng::with_stream(((seed_i as u64) << 32) | node_seed as u64, slot)`
  (spec 62 § 6.2); the slot numbers below are fixed per node so adding a setting never shifts other draws. Composite
  nodes that run several stages (for example `biome.layer`, spec 65) give each stage its own salt
  (`mix(node_seed, stage_number)`) so stages never share a stream.
- **Angles** in settings are degrees; rotations are applied yaw (Y), then pitch (X), then roll (Z), all through
  `detmath::sin_cos`.
- **Settings types:** `f` float, `i` int, `b` bool, `v3` vec3, `n` name, `a` asset, `sel` selector, `curve`, `enum`.
- Every node writes a `procgen.node` tracing span; nodes that loop over elements check the cancel token every chunk.
- **Settings versions** start at 1 for every node type in this spec.

## 2. Region values

Many nodes test points against regions. For any spatial item `R` and world point `p`, `region_value(R, p) ∈ [0, 1]`:

| Kind | Value |
|---|---|
| `Points` | max over its points of `density_j · edge(j, p)`, where `edge` is 1 inside the point's oriented box shrunk by `hardness`, falling linearly to 0 at the box surface (`hardness` 1 = hard box); a uniform grid over the points' world AABBs accelerates the query |
| `Shape` | 1 inside the shape; 0 outside; with the shape's `edge_m > 0` (spec 64 § 2.4), linear falloff over `edge_m` outside |
| `Spline` | closed: polygon test in the spline's plane (spec 64 § 3); open: 1 within half the interpolated `width` attribute of the curve (default 1 m), falloff over `edge_m` |
| `Polygon` | 1 inside (even-odd per ring, holes subtract), falloff over `edge_m` |
| `Ground` | 1 when `p.xz` is within its rectangle |
| `Mask` | bilinear sample of the mask channel at `p.xz`, 0 outside its rectangle |

When a region pin receives several items they combine by the node's `combine` setting: `max` (default), `add` (sum
clamped to 1) or `any` (1 when any value > 0).

## 3. Inputs: reading the world (`in.*`)

| Id | Pins | Settings (default) | Behavior |
|---|---|---|---|
| `in.self` | out | — | One item describing the generator: a `Shape` (its `ProcBounds` box) and, when the entity has one, its `Shape` (spec 64 § 2.4) and its `Spline` (spec 64 § 2; a closed spline also as a `Polygon` item via `polygon.from` semantics) as further items tagged `self`, and its `PaintedPoints` (spec 67 § 5) as a point item tagged `painted`. Item scope: `guid`, `name` |
| `in.ground` | out | `rect: self` (`self` or explicit `[min_x, min_z, max_x, max_z]`), `layers: []` (names of terrain layers to capture) | One `Ground` item over the rect clipped to the terrain. Captures heights, normals and the named layer weights. No terrain → empty bundle and warning `no_terrain` |
| `in.entities` | out | `tags: []` (all required), `any_tags: []`, `within: self\|world`, `as: points\|splines\|shapes\|all`, `children: false` | Finds level entities by tag and position. `points`: one point per entity (transform, bounds from its mesh or 1 m box), attribute `entity` (Name = guid). `splines`/`shapes`: one item per entity carrying a `Spline`/`Shape` component, item scope `entity`, plus `road_profile` (Asset) when the entity has `RoadOf` (spec 64 § 7.1) and `exclude_width_m` when it is tagged `procgen_exclude`. Order: by `EntityGuid`. The generator's own outputs are never found |
| `in.table` | out | `asset: a` (a data table, `docs/specs/content-data.md` § 3), `row: n` (optional) | One `Table` item with the table's rows. The data table file is read **untyped** (its RON rows parsed as generic values, not through the game's typed `DataTable<T>` struct): each row's named fields become columns, types inferred per column (numbers → Float, or Int when every value is integral; strings → Name, or Asset for values the asset registry knows; tuples of 2-4 numbers → vectors); a column with mixed types fails with `table_column_types`. With `row`, only that row (error `table_row_missing` if absent) |
| `in.mask` | out | `asset: a` (texture), `rect: [f;4]`, `channel: r\|g\|b\|a\|luma`, `filter: bilinear\|nearest` | One `Mask` item. The texture is read from its CPU-side source pixels (the cook keeps a CPU copy for masks used by `OnLoad`/`Anchored` graphs) |
| `in.mesh` | out | `asset: a` | One `Mesh` item (reference plus local bounds); one point at the origin with `bounds_*` = mesh bounds and attribute `mesh` = asset |
| `in.generator_output` | out | `generator: n` (guid or entity name), `table: n` | The named exported table or points of another generator (spec 62 § 7.2 ordering) |
| `in.rays` | in (region), out | `spacing_m: 2`, `from_height_m: 500`, `max_dist_m: 1000`, `layers: all` (physics layer mask), `skip_generated: true` | A grid of downward rays over the region's XZ bounds (`regia_physics` ray casts, captured in the snapshot). One point per hit: position, rotation aligned to the hit normal, attributes `normal` (Vec3), `entity` (Name), `phys_material` (Name). Misses produce nothing |
| `in.overlap` | in (points), inside, outside | `layers: all`, `skip_generated: true` | Each point's world box is overlap-tested against physics colliders; points touching any collider go to `inside` with attribute `entity`, others to `outside` |

Snapshot rule: `in.*` nodes never touch the world during evaluation; the host captures what they declare (spec 62 § 5.3).

## 4. Samplers and generators (`sample.*`, `gen.*`)

| Id | Pins | Settings (default) | Behavior |
|---|---|---|---|
| `sample.ground` | region (Ground or any spatial; when not Ground, the run's Ground over the region's XZ bounds), out | `spacing_m: 4`, `jitter: 0.8`, `bounds_m: v3(1,1,1)`, `keep_zero: false`, `align: up\|normal (up)` | Lays a square grid with cell size `spacing_m` aligned to world multiples of `spacing_m` (so neighboring generators and cells line up). One candidate per cell at the cell center offset by `(u − 0.5) · jitter · spacing_m` per axis (draw slots 0, 1). Y = ground height (bilinear); rotation = identity or aligned to the ground normal. Density = region value at the candidate (0 dropped unless `keep_zero`). `bounds_*` = ±`bounds_m`/2. Order: row-major by (z cell, x cell) ascending |
| `sample.region` | region, out | `spacing_m: 2`, `jitter: 0`, `height: region\|value`, `y: 0` | As `sample.ground` but without terrain: Y is the region's own height (spline/polygon plane, shape center) or `y` |
| `sample.volume` | region, out | `cell_m: v3(2,2,2)`, `jitter: 0` | 3D grid over the region's AABB; keeps cells whose center has region value > 0 (density = value). Order: (y, z, x) ascending. Refuses more than the point cap before allocating (`limit_exceeded`) |
| `sample.poisson` | region, out | `min_dist_m: 2` (≥ 0.05), `attempts: 4` (1-32), `max_points: 100000` | World-anchored 2D blue noise: the world XZ plane is covered by a fixed grid of cell `min_dist_m/√2` aligned to the origin. Candidate `k` of cell `(cell_x, cell_z)` draws `u`, `v` and its priority as the first three `f64` values of `regia_core::math::Rng::with_stream(key as u64, k as u64)`, where `key = mix(mix(cell_x as u32, cell_z as u32), node_seed)`; its XZ is `((cell_x as f64) + u) * cell`, `((cell_z as f64) + v) * cell`. Over the region's XZ bounds plus a halo of `2 · min_dist_m`, candidates are accepted in descending priority (ties by cell, then candidate index) when no accepted point lies within `min_dist_m` (background-grid lookup). Points outside the region bounds or with region value 0 are then dropped. Because candidates depend only on world cells, the same place gives the same points in every run, generator and tile (up to the halo rule of spec 66 § 2). Y from the run's Ground if present, else 0 |
| `sample.mesh_surface` | in (Mesh or points with `mesh`), out | `per_m2: 1`, `max_points: 100000` | Area-weighted random points on the mesh's triangles (triangle by cumulative area, then uniform barycentric by the square-root method), transformed by each input point's transform; normal-aligned rotation; attribute `normal`. Needs the mesh's CPU geometry (pipeline-side for `Baked`; cooked CPU copy flagged by the cook for runtime modes) |
| `gen.grid` | out | `count: [i;3] (4,1,4)`, `step_m: v3(1,1,1)`, `centered: true` | A local grid of points around the origin (meant for `points.stamp`). Order: (y, z, x) |
| `gen.points` | out | `points: [ (position, rotation_deg?, scale?) ]` | Literal points |
| `gen.one` | out | `at: origin\|self` | One point at the origin or the generator's position |

## 5. Density (`density.*`)

| Id | Settings (default) | Behavior |
|---|---|---|
| `density.remap` | `from: [0, 1]`, `to: [0, 1]`, `outside: remap\|keep (remap)` | With `from = [a, b]`, `to = [c, e]`: `d' = c + (d − a) / (b − a) · (e − c)`. When `a == b`, every value maps to `(c + e)/2`. `outside: keep` leaves values outside `[a, b]` unchanged. The result is clamped to [0, 1] always |
| `density.curve` | `curve: [(0,0),(1,1)]` | `d' = curve(d)` (piecewise linear; constant beyond the end keys) |
| `density.noise` | `scale_m: 32, octaves: 3, gain: 0.5, lacunarity: 2, mode: mul\|set\|add (mul), min: 0, max: 1` | Value-noise fBm of `position.xz / scale_m` (lattice hashed from the node seed, smoothstep interpolation), normalized to [0, 1], remapped to `[min, max]` and combined with the density by `mode` |
| `density.facing` | `dir: v3(0,1,0)`, `from: rotation_up\|attr`, `attr: sel (normal)`, `lo_deg: 0`, `hi_deg: 45`, `mode: mul` | Angle `θ` between the point's up (or the attribute) and `dir`: factor 1 below `lo_deg`, 0 above `hi_deg`, linear between; combined by `mode`. Slope thinning |
| `density.realize` | `mode: random\|threshold (random)`, `threshold: 0.5` | Turns density into existence: `random` keeps a point when `u < d` (slot 0), `threshold` when `d ≥ threshold`; kept points get density 1 |

`mode` values for all density nodes: `mul` (`d · f`), `set` (`f`), `add` (`d + f`), `min`, `max`; result clamped to [0, 1].

## 6. Point operations (`points.*`)

| Id | Settings (default) | Behavior |
|---|---|---|
| `points.jitter` | `offset: [v3, v3]` (lo, hi; default zero), `rotate_deg: [v3, v3]`, `scale: [v3, v3]` (default 1, 1), `scale_axes: one\|each (one)`, `rotate_mode: add\|replace (add)`, `scale_mode: mul\|replace (mul)`, `space: local\|world (local)` | Per point draws offset (slots 0-2), rotation (3-5) and scale (6-8, or slot 6 for all three axes when `scale_axes: one`, which reads the X range) uniformly between lo and hi. Offset is added in the point's local frame or in world space. Rotation multiplies the current rotation (`add`) or replaces it; scale multiplies or replaces |
| `points.align` | `to: up\|ground_normal\|attr\|toward_point (up)`, `attr: sel`, `target: v3`, `keep_yaw: true`, `amount: 1` | Rotates each point so its up axis matches the target direction by `amount` (slerp from current), keeping the yaw about the new up when `keep_yaw` |
| `points.project` | pins: `in`, `onto` (Ground, points with boxes, spline, polygon); `rotation: false, keep_outside: false, attributes: none\|all\|list (none), list: [], color: source\|target\|mul (source), tags: source\|target\|both (source)` | Moves points onto the target (the kind of the first `onto` item). There is no `position` or `scale` setting. A point on the target is moved: Ground sets Y to the height; a spline moves it to the nearest curve point; a polygon sets Y to the polygon origin's Y when the point's XZ is inside; points set Y to the top of the highest box under the point. `rotation` aligns up to the ground normal, the spline's up or the target point's up (a polygon does not change rotation). `attributes` and `color` apply to point targets (target wins, or colors multiply). `tags` keeps the item tags, replaces them with the `onto` items' tags, or adds those. Points outside the ground rectangle, the polygon or every box are dropped unless `keep_outside` |
| `points.stamp` | pins: `source`, `targets`; `transform: compose\|target\|source (compose)`, `color: multiply\|source\|target (multiply)`, `seed: mix\|source\|target (mix)`, `attributes: merge_target_wins\|merge_source_wins\|source\|target (merge_target_wins)`, `pairing: by_index\|all_pairs (by_index)` | For every target point, copies the whole source set placed by the target. `transform: compose` = target ∘ source (source positions, rotations and scales are carried into the target's frame); `target` puts every copy exactly at the target transform; `source` keeps source transforms offset by the target position only. `seed: mix` = `mix(source.seed, target.seed)`. `by_index` pairs the i-th source item with the i-th target item (counts must match or one side has 1 item, else `stamp_count_mismatch`); `all_pairs` stamps every source item on every target item. Output order: target-major, then source order |
| `points.repeat` | `count: 3, offset: v3(1,0,0), rot_deg: v3(0), scale: v3(1), chain: true` | Emits each point followed by `count` copies; with `chain`, copy k applies the offset, rotation and scale k times in the previous copy's frame (rows, spirals); without, k times in the original frame. There is no `step` setting |
| `points.bounds` | `mode: set\|scale (set)`, `min, max: v3`, `factor: v3(1)` | Sets the local box (`set`, from `min` and `max`) or scales it (`scale`, by `factor`) |
| `points.bake_scale` | — | `bounds *= scale` component-wise, `scale = sign(scale)` (keeps mirrors) |
| `points.merge` | pins: `in*` | Concatenates all items' points into one item in input order; attributes missing on some items are filled with the type's default (0, false, empty) and their union is the result's columns (a type conflict on one name is error `attr_type_conflict`). Tags: union; scope: first item's |
| `points.enclose` | `pivot: center\|min (center)` | One point whose box encloses each item's points (AABB in world space), one result per item |
| `points.split` | `axis: x\|y\|z`, `at: 0.5` (fraction of the box) | Cuts each point's box in two along the local axis; outputs `low` and `high` pins with boxes refit |
| `points.reseed` | `mode: position\|index\|mix (mix)` | Recomputes seeds: `position` = spec 62 § 6.2 hash; `index` = `mix(node_seed, i)`; `mix` = `mix(seed, node_seed)` |
| `points.keep_random` | `ratio: 0.5` | Keeps each point when `u < ratio` (slot 0). Count is random, not exact |
| `points.prune` | `order: large_first\|small_first\|random\|as_is (large_first)`, `size: box\|density\|attr (box)`, `attr: sel`, `equal_within: 0.0` (relative), `padding_m: 0` | Removes overlaps inside one set: visits points in `order` (size = box volume, density or attribute; ties broken by `seed` then index; `random` orders by a draw, slot 0) and keeps a point when its world AABB grown by `padding_m` overlaps no kept point. `equal_within` treats sizes within that relative fraction as equal (then ordered by draw) so equal sizes do not leave grid patterns. Uses a uniform grid over kept boxes |
| `points.dedupe` | `radius_m: 0.1`, `merge: first\|average (first)` | Points closer than `radius_m` collapse to one: the lowest index, or that point with the average position. Density, color and other attributes stay on the lowest-index point. There is no attribute average |
| `points.neighbors` | `radius_m: 5` | Per point, statistics over the other points within `radius_m` (grid accelerated). Always writes `nb_count`, `nb_density` and `nb_center` (the mean position). Does not write `nb_color` and has no `write` list |
| `points.nearest` | pins: `in`, `to`; `write: nearest_dist (n)`, `write_vec: n (optional)`, `max_m: 1000` | Distance (and vector) to the nearest point of `to` excluding identical index when `to` is the same item |
| `points.flat_only` | pins: `in`, `ground`; `probe_m: 1`, `max_dev_m: 0.15` | Samples ground height at the point and 4 points at `probe_m` around it; keeps the point when all 5 lie within `max_dev_m` of their best-fit plane (least squares) |

## 7. Filters and region operations (`filter.*`, `region.*`)

| Id | Pins | Settings (default) | Behavior |
|---|---|---|---|
| `filter.range` | in, inside, outside | `attr: sel`, `min: f`, `max: f`, `inclusive: [true, true]` | Elements whose value is in the range go to `inside`, others to `outside`. Works on points and table rows; keeps order |
| `filter.compare` | in, `rhs` (optional), inside, outside | `attr: sel`, `op: eq\|ne\|lt\|le\|gt\|ge`, `value` or `rhs_attr: sel`, `rhs_from: constant\|table\|region` | Compares against a constant, column 0 of a one-row table on `rhs`, or the region value of `rhs` at the point |
| `filter.region` | in, region, inside, outside | `min_value: 0.5`, `combine: max` | Points whose region value ≥ `min_value` go inside |
| `region.subtract` | in, minus* | `mode: scale\|cut (cut)`, `cut_at: 0.5`, `keep_zero: false` | `scale`: `density *= 1 − r(p)`; `cut`: drop when `r(p) ≥ cut_at`. Zero-density points are dropped unless `keep_zero` |
| `region.intersect` | in, with* | `mode: mul\|min (mul)` | `density = density · r` or `min(density, r)`; zero dropped |
| `region.union` | in* | `mode: max\|add\|any` | Merges point sets (as `points.merge`) and, where points of different items overlap, sets each point's density to the combined region value of the other items at its position by `mode` |

Where an element is a table row (no position), region filters are an error `not_spatial`.

## 8. Items and tags (`items.*`)

| Id | Settings (default) | Behavior |
|---|---|---|
| `items.tag` | `add: [n]`, `remove: [n]`, `keep_only: [n]`, `rename: [(from, to)]` | Edits tags in this order: rename, remove, keep_only (when non-empty), add |
| `items.by_tag` | `tags: [n]`, `all: false` | Items with any (or all) of the tags → `inside`, others → `outside` |
| `items.by_index` | `select: "0, 2, 4..6, -1"` | Rust-style index list: single indices, half-open `a..b`, closed `a..=b`, open `a..`; negative numbers count from the end (−1 = last). Duplicates and out-of-range indices are ignored with a warning. Selected → `inside` in original order |
| `items.by_kind` | `kinds: [Points, ...]` | Items of the listed kinds → `inside` |
| `items.by_attr` | `name: n`, `scope: element\|item` | Items that have the attribute → `inside` |
| `items.gather` | pins: `in*`, `after*` | Concatenates `in` bundles in edge order. `after` edges only order execution: this node runs after their producers; their data is not forwarded |
| `items.count` | pins: `in*` | One `Table` with one row per input pin: `items` (count of items) and `elements` (sum of lengths, empty items counted as 0) |
| `items.partition` | `by: [sel]` | Splits each item's elements into one item per distinct value tuple of the attributes; output items ordered by first occurrence; each gets item-scope values with the key |
| `items.bounds` | — | One `Table` row per item: `min`, `max` (world AABB) |
| `items.sort` | `by: [sel]`, `descending: false` | Sorts elements (not items) by the keys; stable; ties keep input order |

## 9. Attributes and tables (`attr.*`, `table.*`)

| Id | Settings (default) | Behavior |
|---|---|---|
| `attr.set` | `target: n`, `value` or `from: sel`, `scope: element\|item`, `type: AttrType?` | Creates or overwrites an attribute with a constant or a copy of another selector (converted to `type` when given) |
| `attr.math` | `op`, `a: sel`, `b: sel\|value`, `c: sel\|value`, `target: n` | Element-wise op in f64 (vectors component-wise, scalars broadcast). Ops: `add sub mul div mod pow min max abs neg sign floor ceil round trunc frac sqrt clamp(a,b=lo,c=hi) lerp(a,b,c=t) one_minus sin cos tan asin acos atan atan2 deg rad dot cross length normalize distance`. Division by zero yields 0 and a warning `div_by_zero` (once). `mod` uses floored modulo. `normalize` of a vector shorter than 1e-12 yields 0. `round` is half away from zero. Trig via `detmath` |
| `attr.compare` | `op: eq ne lt le gt ge`, `a`, `b`, `target` | Writes a `Bool` |
| `attr.logic` | `op: and or xor not nand nor`, `a`, `b`, `target` | Bool ops; `not` reads only `a`. Ints are treated bitwise for `and or xor not` |
| `attr.rotation` | `op: from_euler\|look_at\|compose\|invert`, `a`, `b` (compose), `target` | Quaternion math. `from_euler` reads degrees as yaw-pitch-roll. `look_at` aims −Z along a direction. `compose` and `invert` combine or invert quaternions |
| `attr.transform` | `op: compose\|invert\|lerp\|apply_point\|apply_dir`, `target` | Transform math; `apply_dir` ignores translation and scale length |
| `attr.pack` / `attr.unpack` | `parts: [sel]` / `prefix: n` | Builds a vector/transform from parts, or splits into `prefix_x, ...` (vectors) or `prefix_t, prefix_r, prefix_s` (transforms) |
| `attr.rename` | `from: n`, `to: n` | Renames; error when `to` exists (`attr_exists`) unless `overwrite: true` |
| `attr.remove` | `drop: [n]` or `keep: [n]` | Removes custom attributes (built-ins cannot be removed) |
| `attr.random` | `target: n`, `type: Float\|Int\|Vec3`, `min`, `max` | Uniform draw per element (slot 0..2) |
| `attr.reduce` | `attr: sel`, `op: sum\|mean\|min\|max\|count`, `to: item\|table` | Reduces over elements in index order (f64 left to right); writes an item-scope value or a one-row table |
| `attr.lookup` | pins: `in`, `table`; `keys: [(attr, column)]`, `columns: [n]`, `on_miss: keep\|drop\|default`, `multi: first\|weighted (first)`, `weight_column: n`, `nearest_within: f?` | For each element, finds table rows whose key columns equal the element's key values (exact for names, ints, bools; floats exact unless `nearest_within` allows the closest row within that distance; vector distance is Euclidean, rotation distance is angle in degrees). Copies the listed columns onto the element. Several matching rows: `first` = lowest row index; `weighted` = roulette by `weight_column` (slot 0). Key columns are not copied |
| `table.from_points` | — | One row per point with all custom attributes (built-ins omitted unless listed in `include: [n]`); an item with no custom attributes yields an empty table |
| `points.from_table` | `position: sel?`, `rotation: sel?`, `scale: sel?` | One default point per row; listed columns fill built-ins; other columns become attributes (a column with a reserved name is skipped with the warning `column_reserved`) |
| `table.make` | `columns: [(name, type, values)]` | A literal table (all value lists the same length, else `table_ragged`) |
| `attr.string` | `op: concat\|format`, `parts: [sel\|literal]`, `pattern: n`, `target` | Name operations. `concat` joins the parts. `format` replaces `{0}`, `{1}` … in `pattern` with the parts' text (floats with up to 6 decimals, trailing zeros trimmed). There is no `replace` op |

## 10. Control flow (`flow.*`)

| Id | Pins | Settings | Behavior |
|---|---|---|---|
| `flow.if` | in, then, else | `cond: sel` (item scope or a one-row table on pin `cond`), `value: b` | Routes the whole input to `then` when the condition is true, else to `else`. The untaken output is **inactive**: every node downstream of it that has no other active input is skipped (spec 62 § 5.1) |
| `flow.choose` | a, b, out | `cond` as above | Forwards `a` when true, `b` when false. Both inputs are computed (no inactivity) |
| `flow.match` | in, `case_0..case_{n-1}`, default | `key: sel\|value (Int or Name)`, `cases: [values]` | Routes to the first case equal to the key, else `default`; untaken outputs inactive |
| `flow.pick` | `case_0..`, default, out | `key`, `cases` | Forwards the matching input |
| `flow.quality` | `mobile_low, mobile_high, deck, low, medium, high, ultra`, out | — | Forwards the input matching the run's quality tier (`regia_render::quality::QualityTier`, seven variants; which tier a run uses is spec 62 § 7.4; editor preview uses the editor's chosen tier). Unconnected inputs fall back to the nearest lower tier in that list order, then the nearest higher. The run's cache key and bake include the tier |

## 11. Subgraphs and repetition (`graph.*`)

| Id | Settings | Behavior |
|---|---|---|
| `graph.call` | `graph: a` (`.pgraph.ron` or `.pgvar.ron`), `params: {name: value}`, `graph_from: sel?` | Runs another graph inline. Pins are the called graph's declared inputs and outputs. `params` override its parameters (with `Bind(...)` allowed, so a caller's parameter can drive them). Recursion (a graph calling itself directly or indirectly) is allowed: a call whose every input bundle is empty, or which is inactive, does not run and outputs empty bundles; call depth is limited (spec 62 § 5.2). With `graph_from`, the graph to call is read from an item-scope `Asset` value per item (dynamic call; each distinct graph is compiled once per run) |
| `graph.loop` | `graph: a`, `each: [pin]`, `carry: [pin]`, `params` | Runs the called graph once per item of the `each` pins (all `each` pins must have the same item count, else `loop_count_mismatch`); iteration k gets item k on each `each` pin, the full bundles on other pins, and on `carry` pins the caller's bundle for k = 0 and iteration k−1's output of the same-named output pin afterwards. Outputs: concatenation over iterations in order (carry outputs: the last iteration's) |
| `graph.loop_index` | — | Inside a loop body: one-row table `index` (Int) of the innermost loop; error `not_in_loop` outside |
| `graph.depth` | — | One-row table `depth`: current `graph.call` nesting depth (0 at the generator's graph) |
| `graph.input` / `graph.output` | `pin: n` | The called graph's declared input/output pins appear as these nodes |

## 12. Debug (`debug.*`)

| Id | Settings | Behavior |
|---|---|---|
| `debug.check` | `attr: sel`, `min`, `max`, `max_count: i?`, `min_count: i?` | Fails the run with `check_failed` (naming the node, the first offending element index and value) when any value is outside the range or the element count is outside the bounds. Passes data through otherwise. Runs in every build |
| `debug.log` | `message: n` (`{len}`, `{items}`, `{attr:<sel>}` placeholders), `level: info\|warn` | Logs once per run (target `regia_procgen`) and passes data through. In shipping builds it passes through without logging |
| `debug.show` | `color: v4`, `as: points\|boxes\|axes` | Passes data through; in the editor, records the bundle for the debug overlay (spec 67 § 3) as if the node were inspected. No effect at runtime |

## 13. Spawning and outputs (`spawn.*`, `out.*`)

### 13.1 `spawn.mesh`

Pins `in` (points), `out` (the same points with attribute `mesh` written and bounds updated). Settings:

```ron
(
    pick: Weighted,                       // Weighted | ByAttr(attr: "kind") | GroupThenWeighted(group_attr: "biome")
    entries: [
        (mesh: "meshes/rock_a.glb", weight: 3.0, group: "", key: "", material: None, cast_shadows: true,
         collision: None,                 // None | Simple (box) | Mesh (cooked collision mesh)
         draw_distance_m: 300.0, fade_m: 20.0, cull_below_px: 2.0, set_bounds: true,
         offset: (t: (0, 0, 0), r_deg: (0, 0, 0), s: (1, 1, 1)), enabled: true),
    ],
    write_attr: "mesh",
    color_from: "color",                  // per-instance tint selector (empty = white)
    custom: ["wetness"],                  // up to 4 float attributes passed as per-instance custom data
    target_layer: None,                   // layer name (spec 37 § 4.1): the node's instance sets join it (built, ADR-7214)
)
```

- `Weighted`: entry index from the point's draw (slot 0) against the cumulative normalized weights of enabled entries
  (`u · Σw`, first entry whose cumulative sum exceeds it). Entries with weight ≤ 0 or disabled never spawn. All weights
  zero → no spawn and warning `no_spawn_weight`.
- `ByAttr`: the entry whose `key` equals the point's attribute value (Name or Int as text); no match → no spawn for that
  point, counted in the report (`unmatched`).
- `GroupThenWeighted`: entries whose `group` equals the point's group attribute, then weighted among them.
- `offset` is applied in the instance's local frame before the point transform (pivot fixes for assets whose origin is
  not at the base). `set_bounds` replaces the point's box with the mesh's local bounds.
- Points with density 0 never spawn. Each spawned instance's transform is the point transform ∘ offset.
- The result goes to the run's output as instance sets (spec 62 § 7.3), grouped by (mesh, material, shadows, collision),
  instances ordered by point index within each set.
- `cull_below_px` is stored on the instance set, included in its hash and written to `.pgout`. The draw path culls by
  `draw_distance_m` and fades across `fade_m`. It does not read `cull_below_px`.
- **As built (ADR-7214):** `target_layer` (`None`, `"name"` or `Some("name")`; an invalid layer name fails with
  `setting_type`) sets `InstanceSet::layer` on every set the node fills; the layer is part of the grouping key, hashed only
  when set and written to `.pgout` v4. The set draws, and its collider is active, only while the layer is `Live` (a layer
  the level does not bind hides nothing); the generator's own presence applies on top (spec 62 § 7.3).

### 13.2 `spawn.prefab`

Pins `in`, `out`. Settings: `prefab: a` or `pick` + `entries` as for meshes (with `prefab` instead of `mesh`);
`overrides: [(attr: "color", path: "MeshRenderer.tint")]` mapping attributes to reflected component field paths (unknown
paths fail validation with `override_path_unknown` naming the prefab; type conversion by spec 62 § 3.5). A bake stores
the overrides in the `.pgout` override section (ADR-7750) and a loaded bake applies them as a live run does. `name: "rock_{index}"`
(entity name pattern); `tags: [n]` added to spawned entities. One prefab instance per point with density > 0, parented under
the generator's output root (spec 62 § 7.3), spawned in point order. Prefabs that contain their own `ProcGenerator` run it
after spawn as an ordinary generator; its output never flows back into this graph.

Limit: `spawn.prefab` creates entities, so its count limit is separate: 20 000 per generator desktop, 5 000 Android
(`limit_exceeded`, `prefabs_per_generator`).

### 13.3 Saved instance sets (detach target)

`InstancedMeshSet` (saved component, `regia_procgen`, `format_version` 1, check-formats name `procgen-instanced-set`):
mesh, material override, shadows, collision, draw distance, fade, and a list of `(transform, color, custom[4])`. Detached
output becomes these; the runtime draws them through the same instanced path.

### 13.4 Outputs

| Id | Settings | Behavior |
|---|---|---|
| `out.table` | `name: n` | Exports the input (tables, or points converted by `table.from_points`) as a named table of the generator's output |
| `out.mask` | `name: n` | Exports a `Mask` item as a named mask of the generator's output (`ProcOutput::mask`, spec 62 § 7.3) |
| `out.points` | `name: n` | Exports points under a name (readable by `in.generator_output`) |
| `out.graph` | `pin: n` | In a graph used by `graph.call`, the same as `graph.output` |

A name holds one value per run (v0.5.1, spec 62 § 7.3): a second `out.table`, `out.points` or `out.mask` write of the
same name replaces the first, in compiled order, and so does a write from a called graph (`graph.call`, each
`graph.loop` iteration, a biome child graph), which also warns `output_name_replaced` once on the calling node. A loop
body that should contribute every iteration's points returns them through an output pin (concatenated over
iterations, § 11) and the caller publishes them once.

## 14. Tests (per node; `regia_procgen/tests/nodes_*.rs`, no GPU)

Every node gets: a settings round-trip test, a validation test (one bad setting → its error code), and behavior tests on
small hand-built inputs. Required cases (each states what it proves):
1. `sample.ground` on a flat 64 m ground, spacing 4, jitter 0: exactly 256 points at cell centers, Y = height; with jitter
   0.8 every point stays inside its cell; two generators covering adjacent halves produce the union of one generator over
   the whole (world-aligned grid, position seeds).
2. `sample.poisson`: no pair closer than `min_dist_m` (brute-force check over 5 000 points); same seed → same points.
3. `density.remap`: the low end of `from` maps to the low end of `to`, the high end to the high end; a collapsed `from`
   maps to the middle of `to`; with `from: [0, 0.5]`, `to: [0, 0.2]`, `outside: keep` leaves 0.9 at 0.9 while `remap`
   gives 0.36.
4. `density.realize random` over 100 000 points of density 0.3 keeps between 29 000 and 31 000 (fixed seed, recorded count).
5. `points.prune large_first`: no two kept boxes overlap; the largest point is always kept; `random` with `equal_within`
   0.05 on equal boxes gives no regular row pattern (kept set differs from `as_is`).
6. `points.stamp by_index` with 2 source items and 3 target items fails with `stamp_count_mismatch`; 1 source item on
   3 targets gives 3 × source length points in target-major order.
7. `spawn.mesh Weighted` with weights 1, 1, 1, 1 over 40 000 points spawns each mesh 10 000 ± 300 times (recorded counts);
   weight 0 entry never spawns; `ByAttr` with an unknown key spawns nothing for that point and reports `unmatched`.
8. `flow.if`: the untaken branch's nodes appear as inactive in the report and produce no output; `flow.choose` computes
   both inputs.
9. `graph.loop` with 3 items and a carry pin: the body runs 3 times; carry starts with the caller's data and accumulates
   (a body that adds one point gives 3 extra points at the end).
10. Recursive `graph.call` that halves its input until empty terminates; depth 33 fails with `limit_exceeded`.
11. A failing `debug.check` fails the run (`Failed`, code `check_failed`, spec 62 § 7.2) and publishes nothing.
12. `attr.lookup` with weighted multi-match picks rows in proportion to weights over 10 000 elements (recorded counts);
    key columns absent from the output.
13. `items.by_index` "0, 2, 4..6, 7.., -1" on 10 items selects 0, 2, 4, 5, 7, 8, 9 (−1 = 9 already selected, no duplicate).
14. `region.subtract cut` with a box shape removes exactly the points inside the box; `scale` mode halves density at the
    midpoint of a 2 m `edge_m` band.
15. `points.flat_only` keeps points on a plane and on a uniform slope, drops points on a step.

## 15. Platforms and performance

Pure CPU, no platform code. Per-node budgets (desktop reference, one core, 1 M input points): `points.jitter` ≤ 30 ms,
`filter.range` ≤ 10 ms, `density.noise` (3 octaves) ≤ 60 ms, `points.prune` ≤ 250 ms, `spawn.mesh` ≤ 40 ms, region tests
against 10 000 boxes ≤ 150 ms. Android aarch64: ≤ 4× desktop. Measured by `cargo bench -p regia_procgen --bench nodes`.

## 16. Out of scope

User-defined expression strings (an `attr.expr` node with a small expression language is a candidate for a later spec;
`attr.math` chains cover the need today); image-based skeleton or mesh extraction; external point-cloud file import;
saving data items to arbitrary files from a graph (the agent's `procgen.inspect` and the CLI cover inspection).

## 17. Provenance

Node families behave as described in public user documentation of graph-based procedural generation, via the
public-sources spec set area `runtime-procedural` (set spec id `runtime-procedural-node-reference`, plus `-overview` and
`-data-attributes`; their Sources pages). No node names, setting names or wording carried. Algorithms: dart-throwing blue noise with priorities on a world-anchored grid (Cook, "Stochastic Sampling in Computer Graphics", ACM TOG 1986; the background-grid neighbor lookup is the one in Bridson, "Fast
Poisson Disk Sampling in Arbitrary Dimensions", SIGGRAPH 2007 sketches); area-weighted triangle sampling with the
square-root barycentric method (Osada et al., "Shape Distributions", ACM TOG 2002); value-noise fBm (Perlin 2002 and
Musgrave's fBm in *Texturing and Modeling*, existing rows); roulette-wheel selection (textbook); least-squares plane fit
(textbook); Gram-Schmidt orthogonalization (textbook). Settings, defaults, the region-value rule and the index syntax are
Regia's own.

## Addendum (ADR-2724, 2026-11-04): spawn.prefab overrides, wfc.solve, budget bench

**`spawn.prefab` `overrides`** (§ 13.2 as built). Rows `(attr: sel, path: "Component.field.path")`; `Component` is a
full reflected type path (`my_game::fx::Glow`) or a short name (`Glow`). Compile checks: a row without `attr` or `path` is
`setting_type`; a path that is not `Component.field` is `override_path_unknown`. Whether the prefab carries that
component and field is known only once the instance exists, so that part of `override_path_unknown` is reported when
the overrides are applied (with the prefab's name). Each spawned `PrefabSpawn` carries the point's value per row
(`overrides`); the output hash includes them only when present. Application (`regia_procgen::prefab_overrides`): the
component on the instance root, else its first descendant that has it (depth first, child order); conversion to the
field's reflected type: a scalar into `f32`, `f64`, an integer (integral values only) or `bool`; a 2-4 component value
into a struct, tuple struct, tuple or array with that many numeric fields; a name or asset into `String`. Failures:
`override_path_unknown`, `override_type_mismatch`, `override_failed`; the other rows still apply.

**`wfc.solve`** (family `wfc`; optional inputs `bounds` (any spatial item: the grid origin and x/z cell counts cover the world box of every item; the layer count stays `size`) and `tileset` (a data table from `in.table` with columns `name, east, south, west, north, weight?, class?, up?, down?`, used instead of `tiles`); output `out` points). Settings: `mode: tiled|overlapping (tiled)`, `tiles: []`
(`(name, weight: 1.0, sockets: (east, south, west, north))`, sockets read clockwise; a side matches the reverse of the
facing side), `sample: []` (rows, one character per symbol), `n: 2` (1..8), `periodic_input: true`,
`symmetry: none|rotate|reflect|all (none)`, `size: (8, 8, 1)` (cells along x, rows along z, layers along y; whole
numbers), `cell_m: 1.0`, `origin: (0, 0, 0)` (the grid's min corner), `periodic: false`, `max_backtracks: 10000`,
`border: ""` (tiled 2D: a declared tile forced on every border cell, any of its variants). One point per cell at the
cell center (`origin + ((x + 0.5)·cell, z·cell, (y + 0.5)·cell)`), rotated by the variant's quarter turns (yaw −90°
each, clockwise seen from above), X scale −1 for mirrored variants, a cell-sized box; attributes `tile` (variant name:
`road`, `road@r1`, `corner@m`, `corner@mr2`; the symbol for overlapping), `tile_base`, `tile_id`, `tile_turns`,
`tile_mirrored`. Setting `seed: -1` (-1..9e15): a seed of 0 or more is the solver seed (the same seed given to `procgen.wfc_solve` gives the same grid); -1 (the default) seeds from the node seed (`(node_seed << 32) | tier_seed`), so the generator seed changes the grid and
the same inputs give the same grid everywhere. Errors: `wfc_invalid_model` (also no tiles or no sample at run time),
`wfc_contradiction`, `wfc_backtrack_limit`. Symmetry variants: the identity, then quarter turns, then the mirror and its
turns (`Symmetry::transforms`); variants with the same four sockets keep the first; overlapping blocks add every
transform as an occurrence. The search scans every cell per decision, so its time grows with the square of the cell
count; the node checks the cancel token before every decision and backtrack (`wfc::solve_cancellable`), so a cancelled
run stops during the search. Tests: `tests/nodes_wfc.rs`.

**Budgets bench.** `cargo bench -p regia_procgen --bench nodes` covers § 15 at 1 M points (node time from the report);
`tests/perf_nodes_budget.rs` (ignored, hand-run with `--release`) asserts the `points.jitter`, `filter.range` and
`density.noise` one-core budgets, and fails while they are missed. Measured 2026-11-05 on one idle desktop (i7-8700K,
6 cores, Windows, Rust 1.99, release, 1 M points, p50 of the node's own report time; `integ/v035` is the code before this
work, and it had no bench for the last three rows, so those start from the first bench run):

| Node | Before | Now, one core | Now, 8 workers | Budget | |
|---|---|---|---|---|---|
| `points.jitter` (offset, yaw, scale: 7 draws a point) | 145 ms | 92 ms | 16 ms | 30 ms | missed |
| `points.jitter` (offset only: 3 draws a point) | | 29-33 ms | | 30 ms | on the line |
| `filter.range` | 75 ms | 20 ms | 13 ms | 10 ms | missed |
| `density.noise` (3 octaves, points in spatial order) | 112 ms | 35 ms | 6 ms | 60 ms | met |
| `density.noise` (3 octaves, points in random order) | | 86 ms | | 60 ms | missed |
| `points.prune` | 543 ms | 410-435 ms | | 250 ms | missed |
| `spawn.mesh` (records the `mesh` attribute, the default) | 278 ms | 99 ms | | 40 ms | missed |
| `spawn.mesh` (`write_attr: ""`) | | 54 ms | | 40 ms | missed |
| `region.subtract` against 10 000 boxes | 71 ms | 73 ms | | 150 ms | met |

A second session the same day measured the unchanged nodes 6-11 % slower on the same CPU, so read differences of that
size as noise; the ranges above span both sessions. Old and new `points.prune` grids interleaved in one session: 515 and
435 ms. Most of what remains is cache misses from visiting boxes in size-then-seed order.

One deep copy of the 1 M-point set costs 14 ms on this CPU, and a `filter.range` split has to write all of it, so its
10 ms budget needs shared columns or selection vectors in `data.rs`. The table is this one CPU. A newer desktop differs: a
Ryzen 9 9950X3D, shared with other builds at load 20 of 32 (indicative only), one core: `points.jitter` 57 ms (offset only
17), `filter.range` 12-16, `density.noise` 20 (random order 65), `points.prune` 400, `spawn.mesh` 40 (`write_attr: ""` 32),
region tests 45. Numbers from the other shared lane host are not comparable. `density.noise` keeps the lattice hashes of each
octave's last cell, so its cost depends on point order; its output does not.

## Addendum (ADR-2731): node rows

| Id | Settings | Behavior |
|---|---|---|
| `in.mask` (now fed) | as § 3 | Pixels from the host's `ProcMaskLibrary`, else a project PNG, as stored numbers. Missing or broken: warning `mask_missing`, empty bundle |
| `in.ground` `layers` (now fed) | as § 3 | The item carries the named layers' weights (`GroundData::layers`); a name the terrain lacks warns `layer_missing` |
| `density.ground_layer` | pins `in`, `ground`; `layer: n`, `mode: mul\|set\|add\|min\|max (mul)` | Combines the layer's bilinear weight at each point's XZ into density. A layer the ground item does not carry fails with `layer_missing` |


## Addendum (ADR-3027, 2026-11-05): implementation status of the remaining node settings

- **Built:** WFC tile classes (`class: X|I|\|T|L|F`) and vertical sockets (`up`, `down`, `^` turn-sensitive) for
  `wfc.solve` and `procgen.wfc_solve`; `wfc.solve` `seed` (-1 = generator seed) and the optional `bounds` input;
  `spawn.prefab` override paths checked against the reflected types when a generator compiles
  (`override_path_unknown`, `override_type_mismatch`, with the node id and path); `graph.call graph_from` with a
  `graphs` candidate list (`call_pins_mismatch`); `attr.lookup nearest_within` (the scan checks the cancel token every
  4096 table rows, carried across elements, so a cancel lands within that many rows even when one element's bucket
  holds the whole table; ADR-7262, output unchanged); `points.align keep_yaw`;
  `points.project` onto splines, polygons and point boxes; `in.rays` `phys_material`; exact `in.overlap` (oriented
  point boxes against collider shapes; height fields and very large meshes by their bounds); `in.entities` mesh bounds;
  a tile-set asset for `wfc.solve` as a data table on its `tileset` input; `points.project` `attributes`, `list`,
  `color` and `tags` (attributes and color from point targets; tags from every target kind).
- **Not built:** none of the gap-audit D node settings remain.
- `graph_from` reads `sel` as an item-scope Asset or Name (`item.graph` or `graph`) on the items of the first input
  pin; the graphs it may name are listed in `graphs` so the host can capture them before the run.

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

## Addendum (ADR-5644, 2026-11-10): conformance pass as built

- **`filter.compare` (§ 7).** `rhs_attr: n` (default empty = off) is built: with `rhs_from: constant` the right-hand side
  is that scalar selector of the same element instead of `value`; it is ignored for `table` and `region`. The node now
  applies spec 62 § 3.5 to both sides: a vector read without a component suffix fails with `attr_type_mismatch` (it used
  to read component 0 silently). `rhs_attr` is a pure addition with a default, so the node stays at settings version 1.
- **`debug.log` (§ 12).** `{attr:<sel>}` is the selector's value at the first element of the first item (`item.<name>`
  reads item scope): numbers as `attr.string` writes them (up to 6 decimals, components joined by `,`), names and assets
  as text, `-` when there is no element, `?` when the selector cannot be read (the node never fails on a placeholder).
  A shipping build is a release build without the `pipeline` feature (`nodes::DEBUG_LOG_ENABLED`); there the node passes
  data through without logging.
- **§ 14 per-node validation.** `tests/spec63_conformance.rs` checks every registered node type: an unknown setting is
  `unknown_setting` and every typed setting given a value of the wrong shape is `setting_type`, both naming the node.
  It also adds behavior tests for node types that had none: `points.bake_scale`, `filter.compare`, `items.tag` order,
  `items.by_tag`, `items.by_kind`, `items.by_attr`, the `items.by_index` node, `items.bounds`, `items.sort`,
  `attr.rename`, `attr.remove`, `table.from_points`, `flow.pick`, `debug.log` and `debug.show`.
