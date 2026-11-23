# Pre-fractured destruction (agents)

The asset pipeline can split a closed triangle mesh into **Voronoi chunks** ahead of time
(destruction level 2: pre-fractured meshes that a game swaps in and throws as debris). The tool is
`regia_asset_pipeline::fracture::fracture_mesh`; it is deterministic by seed and has no runtime
cost. Format: `.fracture.ron` v2 (`docs/formats/fracture.md`).

What it makes:

- **Chunks.** N seeded sites are placed inside the solid (best-candidate sampling, so they spread
  out). Each chunk is the solid clipped by the half-spaces closer to its site than to any other
  site, and every cut is capped, so each chunk is a closed mesh. The first `interior_start`
  triangles are pieces of the original surface (keep the original material there); the rest are the
  capped interior faces (give them a "broken" material). The solid may be concave.
- **Joints.** Two chunks that share a cap are joined. `contact_area` is the area of that face,
  `anchor` its center, `normal` points from chunk `a` to chunk `b`, and
  `break_force = strength_per_area x contact_area` (clamped to the limits in `JointSettings`).
  `FractureSet::adjacency()` is the chunk graph.
- **Physics.** `FractureSet::chunk_descs(density)` gives `regia_physics::ChunkDesc` values (convex
  hull of each chunk, mass from volume) for `DebrisSpawner`.
- **Render meshes.** `chunk.render` is what to draw: the same triangles with normals, UVs and
  tangents, split where those differ. Give `fracture_source` the mesh's own normals and UVs and the
  surface pieces interpolate them, seams included; `fracture_mesh`, or a source without them, gets
  normals smoothed across edges flatter than `crease_degrees` (default 40) and a box projection at
  `surface_uv_scale`. A cut face gets its plane's normal and a planar projection at
  `interior_uv_scale` UV units per metre (default 1): no stretch, the same texel density on every
  cut, and the two sides of one cut show the same pattern. Tangents are MikkTSpace, as on any cooked
  mesh, and always perpendicular to the normal, even on a sliver.

Volume is conserved: the chunk volumes add up to the solid's, within `f32` rounding (`volume_error`
below is about `1e-6`). Chunks smaller than `min_chunk_volume_fraction` of the solid are dropped.

## In the cook: `.meta` settings

Set `fracture` in a glTF's (or FBX's, or OBJ's) `.meta` settings and `regia cook` makes the chunks
from then on; a shipped game needs nothing else, because the runtime reads the cooked outputs and
does not link the pipeline.

```text
settings: "(fracture: Some((chunks: 24, seed: 7, density: 2400.0, fx_tag: \"concrete\")))"
```

| Field | Default | Meaning |
|---|---|---|
| `chunks` | 24 | Chunks to make, 1 to 1024. |
| `seed` | 1 | Seed of the site sampling. |
| `mesh` | the primary mesh | `Some("mesh/<node>/primitive_<i>")` picks another. |
| `strength_per_area` | 500000 | Newtons of break force per square metre of contact. |
| `density` | 2400 | Kilograms per cubic metre. |
| `bond_seconds` | 0.01 | Break impulse = break force x this. |
| `support_below_y` | the lowest centimetre | Chunks at or below this height are held by the world. |
| `free` | false | No support chunks. |
| `fx_tag` | `"default"` | Sound/VFX tag. |
| `interior_uv_scale` | 1 | UV units per metre on the cut faces. |

The cook adds two kinds of sub-asset to the source:

- `fracture/chunk_<n>`, a cooked mesh per chunk: material slot 0 is the surface, slot 1 the cut faces.
- `fracture/destructible`, the `.destructible.ron` text; its chunks name their meshes in `lods`
  (`assets/wall.glb#fracture/chunk_3`). Load it with `DestructibleAsset::load(&mut server, id)`,
  where `id` comes from the manifest (`<source>#fracture/destructible`).

The fracture runs only when the mesh or the settings change. A mesh that cannot be fractured fails
that source's cook with its `fracture_*` code; other sources go on. These are derived data: the
migration rule is a recook.

## The Fracture panel (editor)

The **Fracture** tab sets the same fields on a box or a project mesh, previews on a worker thread and
draws the chunks in the viewport, pulled apart by the **Explode** slider. **Save** writes
`<mesh>.fracture.ron` and `<mesh>.destructible.ron` (a box saves as `assets/fracture.*`), and **Apply
to .meta** stores the settings in the mesh's `.meta`, keeping its other settings. The panel and these
methods share one state, so what an agent sets is what the panel shows:

| Method | What it does |
|---|---|
| `editor.fracture_panel_get` | The source, settings, whether a run is going, the result and the last error. |
| `editor.fracture_panel_set` | Any of `box` or `mesh`, `chunks`, `seed`, `strength_per_area`, `density`, `bond_seconds`, `interior_uv_scale`, `fx_tag`, `free`, `explode`, `show_gizmo`, `origin`. |
| `editor.fracture_panel_run { wait? }` | Runs on the worker thread. With `wait: true` it answers when done. |
| `editor.fracture_panel_save { out }` | Writes `<out>.fracture.ron` and `<out>.destructible.ron`, `out` relative to the project. |
| `editor.fracture_panel_apply_meta` | Stores the settings in the source mesh's `.meta`. |

Check your work: `result.chunks` matches, `result.volume_error` is tiny, and `running` is false.
Beyond the `fracture_*` reasons below, these methods answer `F0002` with:

| `reason` | Meaning |
|---|---|
| `fracture_panel_busy` | A run is going. |
| `fracture_panel_no_result` | Save before any run finished. |
| `fracture_panel_no_project` | A project mesh with no project open. |
| `fracture_panel_no_mesh` | Apply to `.meta` on a box. |
| `fracture_panel_bad_path` | A path that is absolute, empty, or climbs out of the project. |
| `fracture_panel_meta` | The `.meta` or its settings did not parse (nothing is changed). |
| `fracture_panel_io` | A file could not be read, imported or written. |

### Supports, test break, bonds and Place (ADR-9750)

Under a result the panel has four more sections, each with its methods:

- **Supports**: which chunks the world holds up. *Rule* is the cook's rule from the settings (the
  lowest centimetre; a `support_below_y` plane, set with the *Plane* box; none with *Free object*).
  *Picked*: turn on *Click chunks* and click chunks in the viewport to toggle them (support chunks
  draw blue). *Box*: every chunk whose bounds touch the box. Picks and the box go into the saved
  `.destructible.ron` as per-chunk `support` flags. Only the plane lives in the mesh `.meta`, so only
  the plane survives a re-cook; **Apply to .meta** refuses picks and boxes
  (`fracture_panel_meta_supports`) rather than drop them. A new run clears picks (chunk indices
  name one result).
- **Test break**: the break model of the saved cook (`DestructionWorld`, seed 0, at the panel's
  `origin`) inside the editor. Turn on *Click to hit* and click the preview: an explosion of the
  set *Impulse* and *Radius* lands where you clicked, and the model runs 4 fixed ticks at 60 Hz.
  Chunks knocked loose draw red, chunks that fell because they lost their support draw violet, and
  broken bonds go grey. The panel shows the counts and the `structure_hash`. A game world fed the
  saved `.destructible.ron`, the same pose, seed 0 and the same hits, each followed by 4 steps of
  1/60 s, has the same hash, bit for bit. **Reset** starts whole again; changing a setting, the
  supports or the source resets it too.
- **Bonds**: *Heat view* draws each bond across its face, colored by its break impulse (red is the
  weakest, through yellow to cyan the strongest); hovering one in the viewport names it, and the
  panel lists the weakest ten. The hover follows the pointer: off the bond (and off the list row) it
  clears, and `editor.fracture_bonds` answers `hovered: null` (ADR-7451). The bonds are cooked once
  per run, settings and support pick, not on every repaint.
- **Place**: after a **Save**, spawns an entity named after the file with a `DestructibleSource`
  (`asset`: the saved `.destructible.ron`, `seed`: 0) at the viewport drop point. It is one undo
  step and is saved with the level. In a play with `DestructionPlugin`, the entity's destructible is
  defined at its `Transform` the first frame an `AssetServer` can read the asset
  (`DestructibleSourceStatus` reports `Defined(id)` or `Failed { code, message }`). The Inspector
  shows the component's `asset` and `seed`. `asset` may also be a cooked sub-asset
  (`assets/wall.glb#fracture/destructible`), whose chunks name the cooked chunk meshes.

**Sites.** `grid: [x, y, z]` cuts a box source into a regular grid of cells (the *Grid sites* box):
the Voronoi cells of cell-center sites are the cells, numbered x fastest, then y, then z, so a
3x3x1 wall gives nine bricks with the bottom row 0, 1, 2. `sites: [[x, y, z], ...]` gives explicit
sites for any source. `impact: { at, bias }` (ADR-9870) biases the seeded sampling toward a hit at
`at`: `bias` 0 is even over the solid, 1 and 2 pull harder (per unit volume, the site density falls
off as `1/r^bias` with the distance `r` from `at`, flat within 1 % of the source's diagonal of the
hit; ADR-7550), so the chunks are fine debris at the hit and grow with distance; `at` is usually on
the surface and need not be inside (a run with a hit far outside the source at bias 1 or 2 fails
with `fracture_site_sampling`).
`clustered: { clusters, spread }` draws `clusters` clump centers in the solid and spreads the sites
`spread` (a fraction of the source's half-diagonal, 0.001 to 1: a narrower clump cannot keep its
sites apart, ADR-7550) around them: clumps of fine chunks
between large ones. A field left out of `impact`/`clustered` keeps the panel's current value for
it. All are preview choices the `.meta` does not store (**Apply to .meta**
answers `fracture_panel_meta_sites`); setting one clears the others, `null` clears it, and the
chunk count stays `chunks`. One call sets at most one site choice (two answer `F0002`), a refused
call changes nothing, and lowering `chunks` below the clump count clamps the clump count to it
(ADR-7550).

| Method | What it does |
|---|---|
| `editor.fracture_panel_set` | Also takes `supports` (`"rule"`, `"none"`, `[chunk, ...]`, `{ "below_y": y }` — `null` clears the plane, like the *Plane: chunks below y* checkbox off, `{ "box": [[min], [max]] }`), `sites`, `grid`, `impact` (`{ "at": [x, y, z], "bias": 0\|1\|2 }`), `clustered` (`{ "clusters": n, "spread": s }`; null clears either), `click` (`off`, `supports`, `test_hit`), `hit_impulse`, `hit_radius`, `show_bonds`. The state reports `sites`, `impact`, `clustered`, `supports { pick, chunks }` and `tools { click, hit_impulse, hit_radius, show_bonds, hover_bond, test, saved }`. |
| `editor.fracture_test_hit { point, impulse?, radius? }` | One hit and 4 ticks. Answers `{ hits, ticks, bonds, bonds_broken, broken_bonds: [{ bond, a, b }], chunks, detached, collapsed, falling, attached, structure_hash }`. |
| `editor.fracture_test_reset` | Drops the test break (and its debris preview). Answers `{ reset }`. |
| `editor.fracture_test_debris { steps?, floor_y? }` | **Run debris** (ADR-7745): replays the hits in a preview world the way a game runs them (the destruction world, its `DestructionPhysics` bridge and a rapier world, a fixed floor whose top is at `floor_y`, by default the structure's lowest point (give a lower one for a structure that stands on something), 60 Hz, 9.81 m/s² down) and runs `steps` more fixed ticks (default 60, at most 600). Answers `{ steps, floor_y, chunks, moved, above_floor, min_clearance, digest, debris: [{ chunk, start, end, rotation, min_clearance, live }] }`: `moved` counts chunks whose center ended more than 5 cm from where it sat; `above_floor` says every chunk's hulls stayed above the floor (within 10 cm: a heavy chunk sinks up to about 5 cm into rapier's soft contact for a step) on every step; `live` is false for a chunk the bridge's budget already expired (its last pose is kept). The viewport draws each chunk's box at its end pose in orange. A new hit drops the preview; the same hits and steps give the same `digest`. Codes: `fracture_test_no_hits` (no hit yet), `fracture_test_bad_steps`, `fracture_test_bad_floor`, `destruction_*`. A chunk knocked loose with something under it stays where it is: hold the wall by its corners (`supports`) and give a lower floor to see one fall. |
| `editor.fracture_bonds { top? }` | `{ bonds, weakest: [{ bond, a, b, break_impulse, area, anchor, normal, broken }], hovered }`, weakest first (default 10). |
| `editor.fracture_place { at? }` | Spawns the `DestructibleSource` entity (`at` defaults to the viewport drop point). Answers `{ entity, guid, name, asset, seed, at }`. Undo with `editor.undo`. |

Check your work on a wall:

```json
editor.fracture_panel_set { "box": [1.5, 1.5, 0.25], "grid": [3, 3, 1] }
editor.fracture_panel_run { "wait": true }
editor.fracture_panel_set { "supports": [0, 1, 2] }
editor.fracture_panel_save { "out": "assets/wall" }
editor.fracture_test_hit { "point": [0.0, 1.0, 0.0], "impulse": 9000, "radius": 0.5 }
→ { "bonds_broken": 3, "broken_bonds": [{ "a": 4, "b": 7 }, { "a": 6, "b": 7 }, { "a": 7, "b": 8 }],
    "detached": [7], "collapsed": [], "falling": [7], "structure_hash": "…" }
editor.fracture_test_debris { "steps": 120 }
→ { "chunks": 1, "moved": 0, "above_floor": true, "floor_y": -1.5, "digest": "…" }
editor.fracture_place { "at": [0.0, 0.0, -5.0] }
```

Each bond of that wall breaks at 500000 N/m² x 0.5 m² x 0.01 s = 2500 N s; the hit puts 9000 N s on
brick 7, shared by its three bonds. More `reason` codes:

| `reason` | Meaning |
|---|---|
| `fracture_panel_bad_chunk` | A support pick names a chunk past the last result. |
| `fracture_panel_bad_grid` | A grid with a zero count or more than 1024 cells. |
| `fracture_panel_bad_impact` | `impact` with a `bias` other than 0, 1 or 2, or a non-finite `at`. |
| `fracture_panel_bad_clusters` | `clustered` with no clumps, more clumps than chunks, or a `spread` outside [0.001, 1] (a narrower clump cannot keep its sites apart). |
| `fracture_panel_grid_needs_box` | `grid` on a mesh source (give `sites`). |
| `fracture_panel_meta_supports` | Apply to `.meta` with picked or boxed supports. |
| `fracture_panel_meta_sites` | Apply to `.meta` with a site choice (explicit, grid, impact or clustered). |
| `fracture_panel_not_saved` | Place before any Save. |
| `fracture_panel_place` | The entity could not be built (the component type is not registered). |
| `destruction_*` | The cook or the break model refused the settings or the hit (a radius of 0). |
| `fracture_test_no_hits` | Run debris before any test hit (ADR-7745). |
| `fracture_test_bad_steps` | Run debris with `steps` of 0 or above 600. |
| `fracture_test_bad_floor` | Run debris with a `floor_y` that is not finite or is above the structure's lowest point. |

`DestructibleSourceStatus::Failed` codes: `destructible_source_unknown` (no manifest entry and no
such file under the project), `destructible_source_read`, or the asset's `destruction_*` code.

## `editor.fracture_preview` (read-only, editor host)

Runs the tool on a box or on an inline mesh and reports the result. Nothing is written. It is an editor method (the pipeline is desktop-only and the runtime protocol cannot depend on it), so it needs the editor host; a plain game build answers `METHOD_NOT_FOUND`.

```json
editor.fracture_preview { "box": [1.0, 0.5, 0.75], "chunks": 8, "seed": 11, "strength_per_area": 500000 }
→ { "source": "box", "chunks": 8, "source_volume": 3.0, "chunk_volume_total": 3.0, "volume_error": 0.0,
    "chunk_list": [ { "volume": 0.41, "triangles": 38, "interior_triangles": 22, "interior_area": 3.1, "centroid": [..] } ],
    "joints_total": 17, "joints_truncated": false,
    "joints": [ { "a": 0, "b": 3, "contact_area": 0.62, "break_force": 310000.0, "anchor": [..] } ],
    "hash": "5e0c1b7a2f3d4c88" }
```

Parameters: `box` (`[x, y, z]` half extents) or `mesh` (`{ "positions": [[x, y, z], ...], "indices": [i, ...] }`,
at most 100000 vertices and 300000 indices), exactly one; `chunks` 1 to 1024 (default 8); a `chunks` outside that range, or one that is not a non-negative integer, is plain `F0002` with no `data.reason` (the handler returns before the library); `seed` (default 0); `strength_per_area`
(newtons per square metre, default 500000). The same inputs give the same `hash`; compare it across
runs to check determinism. At most 256 joints are listed (`joints_total` counts all of them).

Errors are `F0002` with a stable `data.reason`:

| `reason` | Meaning |
|---|---|
| `fracture_empty_mesh` | The mesh has no triangles. |
| `fracture_bad_index_count` | The index count is not a multiple of 3. |
| `fracture_index_out_of_range` | An index points past the vertex list. |
| `fracture_non_finite` | A position is NaN or infinite. |
| `fracture_degenerate_triangle` | A triangle uses one (welded) vertex twice. |
| `fracture_not_manifold` | An edge is shared inconsistently (a doubled triangle). |
| `fracture_not_closed` | The mesh has holes. |
| `fracture_inside_out` | The triangles wind clockwise from outside. |
| `fracture_zero_volume` | The mesh encloses no volume. |
| `fracture_invalid_settings` | The fracture library rejected the settings (thresholds, strengths, box extents, an impact `bias` other than 0/1/2, clumps outside 1 to the chunk count, a clump `spread` outside [0.001, 1], or a chunk count when the library is called directly). The agent method's own chunk-count check is plain `F0002` with no `data.reason`. |
| `fracture_site_sampling` | No point inside the solid was found for a site (a hit far outside the solid at impact bias 1 or 2). |
| `fracture_site_outside` | An explicit site is outside the solid. |
| `fracture_duplicate_sites` | Two explicit sites coincide. |
| `fracture_cap_failed` | A cut could not be closed (a mesh the closed check let through). |
| `fracture_bad_attributes` | Source normals or UVs do not match the mesh, or hold a zero normal or a non-finite value (library, cook). |
| `fracture_tangents` | The tangent generator refused a chunk. |
| `fracture_format` | A `.fracture.ron` is malformed (library only). |
| `fracture_unsupported_version` | A `.fracture.ron` has another `format_version` (library only). |

A parameter of the wrong type is plain `F0002` without a reason.

Check your own work: `chunks` equals the request, `volume_error` is tiny, `joints_total` is above
zero for a solid block, and a second call with the same arguments returns the same `hash`.
