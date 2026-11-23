# .destructible.ron — destruction level 2 chunk asset v2

Library: `regia_physics::destruction` (`DestructibleAsset::{from_ron, to_ron, validate, box_grid}`,
`migrate_destructible`). Cook from a fracture set: `regia_asset_pipeline::destructible_cook`
(`cook_destructible`). Guide page: `book/agents/destruction.md`. Decisions: ADR-1597 (v1), ADR-2734 (v2:
structural load, falling sections).

A destructible is what the runtime loads to break an object along pre-fractured lines: the chunks
(stable name, mass, convex collision hulls, bounds, whether the world holds the chunk up, render LODs)
and the bonds between touching chunks (area, anchor, normal, and the accumulated impulse that breaks
the bond). Render meshes are either the chunks' own `lods` or the `.fracture.ron` the asset names in
`meshes` (chunk `i` here is chunk `i` there). All positions are in the asset's space, which is the
source kit's frame: metres, +Y up.

The `.meta` fracture cook (ADR-1967, `book/agents/fracture.md`) writes this text as a cooked
sub-asset, `<source>#fracture/destructible`, with each chunk's `lods` naming its cooked mesh;
`DestructibleAsset::load` reads it. A runtime cannot read a `.fracture.ron` (the type lives in the
desktop pipeline), so a game that draws chunks uses `lods`; `meshes` serves tools.

```ron
(
    format_version: 2,
    name: "wall",
    source: Some((kit: "wall_a", revision: "r3")),
    pattern_id: None,
    fracture_seed: Some(7),
    replaces_stage: Some("D2"),
    meshes: Some("props/wall.fracture.ron"),
    outer_material: Some("materials/brick.material.ron"),
    inner_material: Some("materials/brick_cut.material.ron"),
    material: (
        fx_tag: "concrete",
        damage_scale: 1.0,
        min_impulse: 50.0,
        decay_per_second: 0.0,
        detach_spin: 2.0,
    ),
    chunks: [
        (
            name: "frac_wall_a_0",
            parent: None,
            centroid: (-1.5, 0.25, 0.0),
            mass: 300.0,
            inertia: None,
            hulls: [[(-0.5, -0.25, -0.125), (0.5, -0.25, -0.125), ...]],
            bounds_min: (-2.0, 0.0, -0.125),
            bounds_max: (-1.0, 0.5, 0.125),
            support: true,
            lods: ["props/wall_a/frac_0_lod0.glb", "props/wall_a/frac_0_lod1.glb"],
            fx_tag: None,
            small: false,
            lifetime_secs: None,
            edge_heat: None,
        ),
    ],
    bonds: [
        (a: 0, b: 1, area: 0.125, anchor: (-1.0, 0.25, 0.0), normal: (1.0, 0.0, 0.0), break_impulse: 1250.0,
         max_load: Some(4000.0)),
    ],
    // v2, optional: gravity loads the bonds; overloaded bonds fail.
    load: Some((gravity: 9.81, iterations: 64, max_load_per_area: 30000.0, failure_rate: 1.0)),
    // v2, optional: unsupported sections fall as one body; a free object is a free body.
    falling: Some((min_section_chunks: 2, free_body: true)),
)
```

| Field | Meaning |
|---|---|
| `format_version` | Must be 2. A v1 file goes through `migrate_destructible` (only the number changes: v1 has none of the v2 fields, so it behaves exactly as before); newer ones fail with `destruction_version`. |
| `name` | Display name; not empty. |
| `source` | Optional `(kit, revision)` the set fractures (add-only provenance). |
| `pattern_id`, `fracture_seed` | Optional pre-scored pattern id and the seed, so the set can be made again. |
| `replaces_stage` | Optional damage stage the set replaces (`"D2"`). |
| `meshes` | Optional path of the `.fracture.ron` with the chunk render meshes. Default `None`. |
| `outer_material`, `inner_material` | Optional materials of the original surface faces and of the cut faces. |
| `material.fx_tag` | Key every break event carries for sound and VFX lookup. |
| `material.damage_scale` | Multiplier on every impulse that reaches the bonds (>= 0; 0 = indestructible). |
| `material.min_impulse` | Per chunk and per hit, impulses (N s) below this do nothing (>= 0). |
| `material.decay_per_second` | Bond damage heals by this many N s per second (>= 0; 0 = never). |
| `material.detach_spin` | Largest seeded spin of a detached chunk, rad/s per axis (>= 0). |
| `chunks[].name` | Stable name (`frac_<kit>_<n>`); unique in the set when not empty. Default empty. |
| `chunks[].parent` | Optional kit part or rig bone the chunk belongs to (data only in level 2). |
| `chunks[].centroid` | Centre of mass and pivot. |
| `chunks[].mass` | Kilograms, > 0. |
| `chunks[].inertia` | Optional principal moments about the centroid along the asset axes (kg m^2, each > 0); `None` derives them from the hulls. |
| `chunks[].hulls` | Collision proxy: 1 to 16 convex hulls of 4 to 256 points each, relative to `centroid` (several hulls are one compound collider). |
| `chunks[].bounds_min` / `bounds_max` | Bounds in the asset's space (navmesh invalidation). |
| `chunks[].support` | The world holds this chunk up. Chunks with no path of intact bonds to a support chunk collapse. |
| `chunks[].lods` | Render mesh references, LOD0 first (LOD0 and LOD1 are enough). Default empty (use `meshes`). |
| `chunks[].fx_tag` | Optional per-chunk sound/VFX tag (a glass chunk in a concrete set). |
| `chunks[].small` | A small chunk gets no debris body when it detaches; its effect hook still fires. Default false. |
| `chunks[].lifetime_secs` | Optional debris lifetime overriding the budget's `max_lifetime_secs` (>= 0). |
| `chunks[].edge_heat` | Optional hot-edge emission of the cut faces, 0 to 1 (render data). |
| `bonds[].a`, `b` | Chunk indices, `a < b`, each pair once. |
| `bonds[].area`, `anchor`, `normal` | Contact area (m^2), face centre, unit normal from `a` to `b`. |
| `bonds[].break_impulse` | Accumulated impulse (N s) that breaks the bond, > 0. |
| `bonds[].max_load` | v2. Optional load (N) the bond carries without damage when `load` is set (> 0); default `load.max_load_per_area * area`. Not written when `None`. |
| `load` | v2. Optional structural load; `None` (default, not written): bonds are loaded by hits only. |
| `load.gravity` | m/s^2 (>= 0, default 9.81). Each chunk's weight `mass * gravity` (finite) flows through the intact bonds to the support chunks. |
| `load.iterations` | Conjugate-gradient steps per solve, 1 to 4096 (default 64). A chain of `n` unsupported chunks is exact after `n`. |
| `load.max_load_per_area` | N per m^2 of bond area a bond carries without damage when it names no `max_load` (> 0, required). |
| `load.failure_rate` | A bond over its limit gains `(load - max_load) * failure_rate * dt` N s of damage per tick (>= 0, default 1; 0 reports loads but never breaks). |
| `falling` | v2. Optional; `None` (default, not written): a collapsing section breaks into its chunks and a free object stays fixed until it breaks. |
| `falling.min_section_chunks` | A piece of at least this many bonded chunks that leaves the structure falls as one body (>= 2, default 2). |
| `falling.free_body` | An object with no support chunks is a free dynamic body from its first tick on the server (default true). |

Rules checked by `validate`: 1 to 4096 chunks; every number finite; masses, inertia and break
impulses positive; material values non-negative; bounds ordered; 1 to 16 hulls of 4 to 256 points;
chunk names unique; `edge_heat` in 0..=1; bonds in range with `a < b` and no repeated pair; v2:
`max_load` positive, `load` values in range and every chunk's weight finite, `min_section_chunks`
at least 2. Every
field marked optional above may be left out of the file. The writer (`to_ron`) is deterministic: the same asset gives the same bytes.

## Versioning

| Version | Change | Migration |
|---|---|---|
| 1 | First shape (ADR-1597). | — |
| 2 | Optional `load` (structural load), `falling` (falling sections, free bodies) and `bonds[].max_load` (ADR-2734). | `migrate_destructible` rewrites `format_version: 1` to 2 and keeps every other byte; `regia upgrade-project --write` does the same to files on disk. |

`destructible_migration_sources()` is `[1]`. `cargo xtask check-formats` guards the shape (registry
name `destructible`). Fuzz targets: `destructible` (a v1 fixture, through the migrator) and
`destructible.load` (a v2 fixture with every new field).
