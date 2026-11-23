# FCHS: convex hull set v1

Library: `regia_physics::hull_set` (`HullSet::{from_hulls, to_bytes, from_bytes, to_collider_shape}`). Cook:
`regia_asset_pipeline::collision_cook`. Decision: ADR-2301.

The cooked output of the glTF importer's `collision: ConvexDecomposition` setting: one sub-asset per mesh
primitive at `collision/<node>/primitive_<i>` (the `mesh/` prefix of the primitive's own path swapped for
`collision/`), type path `regia_physics::HullSet`, wrapped in the normal cooked header with the mesh as its
dependency. The payload is little endian:

| Bytes | Content |
|---|---|
| 4 | magic `FCHS` |
| 4 | `format_version` (u32, 1) |
| 4 | hull count (u32, at most 64) |
| per hull | vertex count (u32, 4 to 4096), then that many x, y, z `f32` triples |

Only vertices are stored. `HullSet::to_collider_shape` gives a `ColliderShape::ConvexHull` for one hull or a
`Compound` of them, which rapier hulls on load. Hulls are in the primitive's object space. `from_bytes`
rejects a bad magic, another version, a truncated payload, a count over its limit, a non-finite vertex and
trailing bytes.

`.meta` settings (RON, in the glTF importer settings):

```ron
(collision: ConvexDecomposition, collision_hulls: (max_hulls: 8, resolution: 64, max_concavity: 0.01))
```

`collision` is `None` (the default: no hulls cooked) or `ConvexDecomposition`. `collision_hulls` takes the
`regia_physics::convex_decomposition::DecompositionParams` fields and defaults. A primitive with no volume
(flat or empty) logs a warning and gets no hull set. The cache key covers the mesh bytes, the settings and the
cooker version, so a changed setting recooks only the hulls.

Migration: none at v1 (`hull_set_migration_sources()` is empty). The format is derived data: a layout change
bumps `HULL_SET_FORMAT_VERSION` and the cooker version, and the importer re-cooks the hulls from the mesh. It
has no serde shape, so `cargo xtask check-formats` fingerprints a description of the layout
(`hull_set_schema_fingerprint()`, registered as `hull-set`). `regia upgrade-project` reports the version of
every cooked hull set in the dev cache (`.fckd` files whose type path is `regia_physics::HullSet`, read by
`cooked_hull_set_file_version`). The fuzz target `hull-set` (`agent.fuzz`) decodes, re-encodes and builds the
collider of mutated payloads.
