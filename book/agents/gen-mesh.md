# Procedural meshes (`gen.mesh`, `regia_gen`)

`regia_gen` builds meshes from code: extrude a 2D outline (with holes and a rounded bevel), lathe a
profile around the up axis, clip with a plane, intersect with a convex solid, and project UVs. Every
operation returns the engine's `regia_asset::CpuMesh`, so the result goes wherever a cooked mesh goes.
An agent reaches the same operations through one method, `gen.mesh`, which takes a JSON recipe and
reports what came out (ADR-7240, spec 23 § 6).

Every result is the same to the bit on every platform: the crate uses only exact IEEE operations
(`+ - * /`, `sqrt`) and the engine's platform-independent `sin_cos`. A test can pin a recipe by its
`hash`.

## The operations

| Operation | Rust | Recipe | Notes |
|---|---|---|---|
| Extrude | `extrude(&Profile, &ExtrudeOptions)` | `shape.extrude { profile, holes?, depth, bevel? }` | The profile lies in the XY plane and is pushed along +Z from `z = 0` to `z = depth`. Any winding. Hard edges: each side face owns its 4 vertices. |
| Bevel | `ExtrudeOptions::bevel = Some(Bevel { width, segments })` | `bevel: { width, segments }` | Rounds the edges of both caps with a quarter circle of `segments` facets (`1` is a 45° chamfer). `width ≤ depth / 2`. A width that turns an edge of the profile around is `gen_bevel_too_wide`. |
| Box | `rounded_rect` + `extrude` | `shape.box { size, bevel? }` | Centered on the origin. With a bevel, the vertical edges get a corner radius of twice the bevel width so every edge is round. |
| Lathe | `lathe(&[[r, y]], &LatheOptions { segments })` | `shape.lathe { profile, segments }` | Revolves `[r, y]` points (bottom to top, `r ≥ 0`) around +Y. Closed when the profile starts and ends at `r = 0`. |
| Clip | `clip(&mesh, Plane { normal, offset }, cap)` | `op.clip { plane: { normal, offset }, cap? }` | Keeps `normal · p ≤ offset`. With `cap` (default on) the cut is closed with flat triangles, holes included (cutting a tube gives a ring). |
| Intersect | `intersect_convex(&mesh, &[Plane])` | `op.intersect { planes }` | Keeps the inside of every plane: the intersection with a convex solid. |
| Merge | `merge(&[&a, &b])` | `op.merge { recipe }` | Concatenates solids that do not overlap. |
| Planar UVs | `uv_planar(&mesh, axis, scale, offset)` | `op.uv_planar { axis, scale?, offset? }` | X → (z, y), Y → (x, z), Z → (x, y), then `· scale + offset`. |
| Box UVs | `uv_box(&mesh, scale, offset)` | `op.uv_box { scale?, offset? }` | Each triangle uses the axis its normal is closest to; vertices on a seam are split. A cube of side `1 / scale` centered at `-offset / scale` fills `[0, 1]²` on every face. |
| Transform | `translate`, `scale` | `op.translate { by }`, `op.scale { by }` | A negative scale keeps triangles facing outward. |
| Stats | `mesh_stats(&mesh)`, `mesh_hash(&mesh)` | the result of `gen.mesh` | Counts, watertightness, bounds, UV range, volume, hash. |

"Watertight" means: after welding equal positions, every edge is used exactly once in each direction
and no triangle collapses. The volume is positive when the faces point outward.

Not built: a true boolean of two overlapping solids (union or difference). Convex intersection and
disjoint merge are the "lite" part. The crate's texture synthesis helpers (noise, masks, wear) and
heightfield tools are on [Procedural textures and heightfields](gen-textures.md) (ADR-7280).

## `gen.mesh`

```json
{ "jsonrpc": "2.0", "id": 1, "method": "gen.mesh", "params": {
  "recipe": {
    "shape": { "lathe": { "profile": [[0, 0], [0.6, 0], [0.8, 0.5], [0.4, 1.4], [0.5, 2], [0, 2]], "segments": 24 } },
    "ops": [
      { "clip": { "plane": { "normal": [1, 0, 1], "offset": 0.3 } } },
      { "uv_planar": { "axis": "y", "scale": 0.5 } }
    ]
  }
} }
```

The answer (the counts are illustrative):

```text
{ "vertices": 1440, "triangles": 600, "watertight": true, "boundary_edges": 0,
  "non_manifold_edges": 0, "degenerate_triangles": 0,
  "bounds": { "min": [...], "max": [...] }, "uv_bounds": { "min": [...], "max": [...] },
  "volume": 1.83, "hash": "…64 hex digits…" }
```

`mesh: true` adds `mesh: { positions, normals, uvs, indices }`
for a result of at most 16384 vertices. The method is read-only and needs no world, renderer or
project. It is rate-limited as a heavy call.

Limits (`RecipeLimits::default()`): 1024 points per ring and 4096 in all rings of one profile or
clip cap (`MAX_RING_POINTS`), 64 holes, 256 segments, 64 operations in
all (nested `merge` recipes included), merge nesting 4, 1 000 000 vertices.

## Errors

| `data.code` | When |
|---|---|
| `F0002` | `recipe` is missing or not an object, or `mesh` is not a boolean. |
| `gen_invalid_recipe` | The recipe does not parse: an unknown shape or operation, an unknown field, a wrong type. The message names the field. |
| `gen_invalid_input` | A value is out of range: a non-finite number, a ring with fewer than 3 points or zero area, a zero plane normal, a zero scale factor, a lathe radius below 0. |
| `gen_bevel_too_wide` | The bevel inset turns an edge of the profile around. Use a smaller width. |
| `gen_cap_not_closed` | A clip with `cap` cuts through an open mesh, so the cut edges do not close into loops. Clip a closed mesh or pass `cap: false`. |
| `gen_too_large` | A limit above was passed, or `mesh: true` on more than 16384 vertices. |

## Checking a recipe

1. Build it and check `watertight` is `true` and `volume` is positive before using it as a solid
   (collision, lightmap UVs, a cooked prop).
2. Pin `hash` in a test: the same recipe gives the same bits on Linux, Windows, Android and macOS.
3. Use `uv_bounds` to confirm a texture's repeat: a box UV with `scale = 1 / tile size` gives one
   repeat per tile.

The crate's acceptance tests (`crates/regia_gen/tests/ops.rs`) cover each operation's
watertightness, vertex and triangle counts, UV ranges and byte pins.

## Rounded primitives and sweeps (ADR-8044)

These sit on the same recipe as the table above. A new `shape` variant breaks an exhaustive
`match` on `regia_gen::Shape`. Existing recipes still parse.

| Operation | Rust | Recipe | Notes |
|---|---|---|---|
| Rounded box | `rounded_box(&Frame, half, radius, segments, tile)` | `shape.rounded_box { half, radius, segments, tile? }` | Centered on the frame origin. `half` is `[x, y, z]`. `radius` is in `[0, min(half)]`. Faces are a uniform grid (`segments` quads on an edge, `1` to 1024). The surface stays inside the sharp box. |
| Cylinder | `cylinder(&Frame, &CylinderOptions)` | `shape.cylinder { radius, length, sides, knurl?, chamfer?, cap?, bottom_cap?, tile? }` | Base on the origin, length along +Z in a recipe (`Frame::xyz`). `knurl` in `[0, 1)` insets odd facets. `chamfer` is a 45° ring at +Z, clamped to `min(length / 2, radius / 2)`. `cap` (default true) closes +Z. `bottom_cap` (default false) closes the base. |
| Annulus | `annulus(&Frame, &AnnulusOptions)` | `shape.annulus { inner, outer, sides, tile? }` | Flat ring in the XY plane, normal +Z. `inner: 0` is a disk. `outer > inner`. |
| Tube | `tube(&points, &TubeOptions)` | `shape.sweep { points, radius, sides?, steps?, tile? }` | Catmull-Rom spine, parallel-transport frame, open ends. Set `radius` and omit `half`. |
| Rounded-rectangle sweep | `sweep_rounded_rect(&points, &SweepRect)` | `shape.sweep { points, half, corner?, shift?, out?, sides?, steps?, tile? }` | Straight spans. `half` is `[across, out]`. `out` defaults to `[0, 1, 0]`. Set `half` and omit `radius`. A path parallel to `out` is `gen_invalid_input`. |
| Section sweep | `sweep_section(&rail, &profile, arris_band, wear)` | (Rust only) | Closed profile on explicit `RailFrame` stations. `arris_band` of `0.0015` is the machined corner width; pass it when you want it. `wear` paints vertex color `(1 - weight)`. |

`Frame::identity` is the engine frame (+X right, +Y up, −Z forward). Recipe cylinders, boxes and
annuli use `Frame::xyz` so local +Z matches an extrude's depth. Positions are authored in that
frame.

## `gen.contour`

```json
{ "jsonrpc": "2.0", "id": 1, "method": "gen.contour", "params": {
  "grid": { "origin": [-6.0, -6.0], "cell": 0.125, "width": 96, "height": 96, "samples": [] },
  "levels": [3.25, 4.25, 5.25],
  "smooth": 0,
  "min_length": 0.0
} }
```

`grid` lies on the XZ plane. `origin` is `[x, z]` of sample `(0, 0)`. `cell` is the sample
spacing in metres (`> 0`). `width` and `height` are the cell counts (`1` to 512). `samples` is
row-major and holds `(width + 1) * (height + 1)` numbers. `levels` is a non-empty list of at most
64 scalar values. `smooth` is the number of Chaikin passes: the library default is `0`, and an
agent call that omits `smooth` uses `2` (at most 8). A radius check belongs on `smooth: 0`,
because two passes shrink a polygonal circle. `min_length` drops a polyline shorter than that
many metres (default `0`).

The answer:

```text
{ "count": 3, "contours": [ { "level": 3.25, "closed": true, "points": [[x, y, z], ...] } ] }
```

Every point's `y` equals `level`. A closed line repeats its first point. The method is read-only,
heavy, and needs no world.

| `data.code` | When |
|---|---|
| `F0002` | `grid` or `levels` is missing or the wrong JSON type, or `smooth` is not an integer, or `min_length` is not a number. |
| `gen_invalid_recipe` | `grid` or `levels` does not parse (an unknown field, a wrong type inside the grid). The message names the field. |
| `gen_invalid_input` | A non-finite value, a cell that is not `> 0`, an empty `levels` list, a sample count that does not match the grid, or a negative `min_length`. |
| `gen_too_large` | A side above 512, more than 64 levels, more than 8 smooth passes, or more contour points than the vertex limit. |

`contour_of_a_cone_is_circles` (`crates/regia_gen/src/contour.rs`) checks that three levels of a
cone are closed circles. Pass `smooth: 0` for that check.
