# .fracture.ron — pre-fractured destruction set v2

Library: `regia_asset_pipeline::fracture` (`fracture_mesh`, `fracture_source`,
`FractureSet::{to_ron, from_ron}`, `migrate_fracture`). Guide page: `book/agents/fracture.md`.
Decisions: ADR-1567, ADR-1967 (v2). Registered as `fracture` in `cargo xtask check-formats` and
`regia upgrade-project`.

A fracture set is the output of the destruction tool: the chunks a closed mesh was split into and
the breakable joints between touching chunks. It is written by a tool or a build script; a shipped
game reads the cooked chunk meshes and `.destructible.ron` instead, because the runtime does not
link the pipeline (`regia_asset_pipeline::fracture_cook`). All positions are in the source mesh's
space (metres); counter-clockwise triangles seen from outside.

```ron
(
    format_version: 2,
    source_volume: 8.0,
    chunks: [
        (
            site: (-0.5, 0.0, 0.0),
            positions: [(-1.0, -1.0, -1.0), ...],
            indices: [0, 2, 1, ...],
            interior_start: 10,
            volume: 4.0,
            centroid: (-0.5, 0.0, 0.0),
            bounds_min: (-1.0, -1.0, -1.0),
            bounds_max: (0.0, 1.0, 1.0),
            interior_area: 4.0,
            render: (
                positions: [(-1.0, -1.0, -1.0), ...],
                normals: [(0.0, 0.0, -1.0), ...],
                tangents: [(1.0, 0.0, 0.0, 1.0), ...],
                uvs: [(0.0, 0.0), ...],
                indices: [0, 1, 2, ...],
                interior_start: 10,
            ),
        ),
    ],
    joints: [
        (a: 0, b: 1, contact_area: 4.0, anchor: (0.0, 0.0, 0.0), normal: (1.0, 0.0, 0.0), break_force: 2000000.0),
    ],
)
```

| Field | Meaning |
|---|---|
| `format_version` | 2. A v1 file loads through `migrate_fracture`; another value is `UnsupportedVersion`. |
| `source_volume` | Volume of the source solid, cubic metres. |
| `chunks[].site` | The Voronoi site the chunk grew from. |
| `chunks[].positions` / `indices` | A welded, closed triangle mesh, for hulls and closure checks. |
| `chunks[].interior_start` | Index of the first cap triangle (not of its first index). Triangles before it are pieces of the source surface; the rest are the capped interior faces. |
| `chunks[].volume`, `centroid`, `bounds_*`, `interior_area` | Volume, centre of mass at uniform density, bounds and cap area. |
| `chunks[].render` | What to draw: the same triangles in the same order, with vertices split where normals or UVs differ. |
| `render.positions`, `normals`, `uvs` | One entry per vertex; unit normals. Surface pieces carry the source's attributes, interpolated; cut faces the normal of their plane and a planar projection. |
| `render.tangents` | MikkTSpace tangents, perpendicular to the normal, handedness in `w` (+1 or -1). |
| `render.indices`, `interior_start` | The triangles; `interior_start` equals the chunk's. |
| `joints[].a`, `b` | Chunk indices, `a < b`; one joint per touching pair, sorted. |
| `joints[].contact_area` | Area of the face the two chunks share. |
| `joints[].anchor`, `normal` | Area-weighted centre of that face; unit normal from `a` toward `b`. |
| `joints[].break_force` | `strength_per_area x contact_area`, clamped to the settings' limits, newtons. |

Chunk order is site order; chunks dropped for being smaller than `min_chunk_volume_fraction` leave
no gap. The same mesh, settings and seed produce byte-identical RON.

`FractureSet::from_ron` refuses a non-finite number, and a render mesh whose arrays differ in length,
whose indices leave its vertices, or whose triangle count or `interior_start` differ from its chunk's.

## Versioning

- **v1** (ADR-1567): positions only.
- **v2** (ADR-1967): adds `chunks[].render`. The migrator keeps everything else as it was. The source
  mesh is gone by then, so it works from the chunk alone: crease-smoothed normals and box-projected
  UVs on the surface, the planar projection on the cut faces. `regia upgrade-project --write`
  rewrites v1 files; fracturing again from the source mesh keeps the mesh's own normals and UVs.
