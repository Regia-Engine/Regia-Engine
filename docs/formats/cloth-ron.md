# Cloth and soft-body assets (`.cloth.ron`, `.soft.ron`, `.cloth.bin`, `.soft.bin`)

Spec 55 § 4, ADR-2083. All four formats are version 1 and registered with `cargo xtask check-formats` as `cloth`,
`soft`, `cooked-cloth` and `cooked-soft`, and with `regia upgrade-project`. Loaders read every older version and
answer `cloth_unsupported_version` only for newer ones. Cooked files are rebuilt from source, never migrated.
Types: `regia_physics::cloth::asset`. Cooker: `regia_asset_pipeline::cloth_cook` (desktop only).

## `.cloth.ron` (v1)

```ron
(
  format_version: 1,
  name: "cape",
  skeleton: Some("hero.glb"),                       // None: the cloth follows its entity
  sim_mesh: (source: "cape_sim.glb", node: "Cape_Sim"),
  render_mesh: Some((source: "hero.glb", node: "Hero", section: 2)),   // None: draw the sim mesh
  params: (substeps: 8, pull_compliance: 0.001, wind_model: Aerodynamic(drag: 1.0, lift: 0.3)),
  maps: {
    "reach": Sparse(default: 0.0, entries: [(12, 0.25), (13, 0.30)]),
    "standoff": Dense([/* one value per sim vertex */]),
  },
  constraint_sets: [(name: "stretch", kind: Stretch, compliance: 0.0),
                    (name: "bend", kind: Bend, compliance: 0.01)],
  proxies: ["spine_cap", "thigh_l_cap"],
  lods: [(sim_mesh: Some((source: "cape_sim_lod1.glb", node: "Cape_Sim_LOD1")), overrides: (substeps: Some(4)))],
)
```

| Field | Meaning |
|---|---|
| `format_version` | 1. |
| `name` | Asset name (non-empty); `physics.cloth_edit_map` addresses assets by it. |
| `skeleton` | The skeleton source the sim mesh is skinned to; `None` for a cloth that follows its entity. |
| `sim_mesh` | glTF file (relative to the `.cloth.ron`) and node. All primitives are merged and welded by exact position. A non-skinned node's world transform is applied. At most 65,535 vertices. |
| `render_mesh` | Optional glTF node and primitive index (`section`) drawn as the cloth; every render vertex is bound to its nearest sim triangle. |
| `params` | `ClothParams`; every field has a default (below). |
| `maps` | Per-vertex maps by name: `reach`, `standoff`, `pose_pull`, `sim_mix`, `mass_scale`, `layer`. `Dense([..])` (one value per welded sim vertex) or `Sparse(default, entries: [(vertex, value)])`. Unknown names fail to load. |
| `constraint_sets` | `(name, kind, compliance, scale?)`; `kind` is `Stretch` (triangle edges), `Shear` (across interior edges), `Bend` (dihedral on interior edges), `Tether` (switch on tethers) or `Volume` (soft bodies only: refused here). `scale` is an optional per-vertex map (`Dense`/`Sparse`) multiplying compliance by the mean over the constraint's vertices. Empty: stretch, shear and bend with the parameter compliances. |
| `proxies` | Names of `ClothBodyProxy` components the cloth collides with; empty means every proxy. |
| `lods` | LOD 1 and up: an optional coarser `sim_mesh` (maps resampled from LOD 0 by nearest rest vertex, ties to the lower index) and parameter `overrides` (`damping`, `substeps`, `iterations`, `stretch_compliance`, `shear_compliance`, `bend_compliance`, `collision_thickness`, `self_collision`, `wind_model`; each `Some(..)` or absent). |

`params` fields and defaults: `gravity_scale` 1.0 (-4..4), `gravity_override` None (`Some((x, y, z))` as an array),
`damping` 0.1 (0..20), `stretch_compliance` 0.0, `shear_compliance` 1e-4, `bend_compliance` 1e-2, `substeps` 8
(1..256), `iterations` 1 (1..64), `pull_compliance` 1e-3, `inertia_scale` 1.0 (0..1), `strain_cap` None
(`Some((max, min))`), `tether` None (scale >= 1), `collision_thickness` 0.01, `friction` 0.3 (0..2),
`self_collision` `Off` (`Spheres(radius)`, `VertexTriangle(radius)`), `self_friction` 0.2, `wind_model`
`Aerodynamic(drag: 1.0, lift: 0.3)` (or `Push(gain)`), `air_density` 1.2, `teleport_distance` 2.0, `tear` None
(`Some((strain, max_per_step))`), `density` 0.2 kg/m² (a particle weighs a third of its triangles' area times
this, times its `mass_scale`).

## `.soft.ron` (v1)

```ron
(format_version: 1, name: "ball",
 model: Pressure(pressure: 1.0, volume_compliance: 0.0),
 surface: (source: "ball.glb", node: "Ball"),
 params: (substeps: 8, particle_mass: 0.01),
 maps: {})
```

`model` is `Pressure(pressure, volume_compliance)`, `Tetra(tets: [[a, b, c, d]], cell: Some(0.05),
volume_compliance)` (empty `tets`: the cooker generates them at `cell` meters) or `ShapeMatch(clusters: [[..]],
stiffness, beta)` (empty `clusters`: one cluster of every vertex). `surface` must be closed for `Pressure` and the
generator (`soft_not_closed`). `params`: `substeps` 8, `iterations` 1, `damping` 0.1, `particle_mass` 0.01,
`edge_compliance` 1e-4, `collision_thickness` 0.01, `gravity_scale` 1.0. `maps`: `reach`, `pose_pull`,
`mass_scale` only. A `reach` or `pose_pull` map attaches the body to a pose (flesh, spec 55 § 2.8): the surface
glTF's first joint set skins it, and a generated tet vertex takes the nearest surface vertex's influences. A body with
neither map is free: `mass_scale` alone does not attach it.

## Cooked `.cloth.bin` / `.soft.bin` (payload v1)

The standard cooked wrapper (`FCKD`, `docs/specs/12-assets.md` § 4) with type paths
`regia_physics::cloth::ClothAsset` / `regia_physics::cloth::SoftAsset`, then a little-endian, 4-byte aligned
payload. Strings are `u32 length` + UTF-8 + zero padding to 4 bytes; arrays are `u32 count` + items.

Cloth: `"RCLO"`, `u32 version`, `name`, proxies (`u32 count` + strings), `u32 lod_count`, then per LOD:

| Block | Layout |
|---|---|
| Header | `u32 particles, u32 triangles, u32 constraints, u32 flags` (bit 0 pose maps, bit 1 separate render mesh, bit 2 tearable, bit 3 tethers) |
| Rest positions | `f32[3]` per particle (model space) |
| Triangles | `u32[3]` per triangle |
| Constraints | `u8 kind (0 stretch, 1 shear, 2 bend), u8 pad, u16 reserved, u32 a, u32 b, u32 c, u32 d, f32 rest, f32 compliance`, runs by kind, each sorted by `(a, b)` |
| Maps | six arrays (`u32 count` + `f32`s, empty when unused): `reach, standoff, pose_pull, sim_mix, mass_scale, layer` |
| Joint weights | `u32 count` + `u8[4]` joints + `u8[4]` weights (sum 255) per particle; count 0 when not skinned |
| Render binding | `u32 count` + `u32 triangle, f32 u, f32 v, f32 d, f32[2] uv` per render vertex; then `u32 count` + `u32[3]` render triangles |
| Params | canonical RON of `ClothParams` (string) |

Soft: `"RSOF"`, `u32 version`, `name`, `u32 particles` + rest positions, `u32 count` + surface triangles, the model
(`u8 kind` + 3 pad: 0 `f32 pressure, f32 volume_compliance`; 1 `f32 volume_compliance, u32 count, u32[4]` tets; 2
`f32 stiffness, f32 beta, u32 count` + clusters as `u32 count, u32[]`), the surface binding (`u32 count` + `u32
tet, f32[4] weights`; empty unless tetra), UVs (`u32 count` + `f32[2]`), maps `reach, pose_pull, mass_scale`, the
skin (`u32 count` + `u8[4]` joints, `u8[4]` weights out of 255; empty or one per particle), and the params RON.

The loader checks every index, map range and parameter, and refuses truncation and trailing bytes
(`cloth_asset_invalid`). The same source and asset id always cook to the same bytes.
