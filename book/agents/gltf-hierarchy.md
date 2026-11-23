# glTF hierarchy and wireframe

## `editor.spawn_gltf_hierarchy`

Spawns a glTF/GLB as a named entity tree. Params: `path` (project-relative, required),
`editor_only`, `keep_sockets` (default true), `skins` (default true), `root_name`,
`show_collision` (default false), `lights` (default true).

```json
{ "method": "editor.spawn_gltf_hierarchy", "params": { "path": "models/tank.glb", "root_name": "Tank" } }
```

Returns `root`, `nodes[]` (`name`, `gltf_index`, `entity`, `parent`, `triangles`, `materials`,
`socket`, `collision`, `skin`) and a `report` (`nodes`, `mesh_entities`, `skinned_meshes`,
`sockets`, `collision_nodes`, `hidden_collision_meshes`, `skeletons`, `joints`, `triangles`,
`duplicate_names`, `warnings`). Use `entity` with
`scene.get_entity` / `editor.set_transform`. Not undoable: delete `root` to remove the model.
Names are not unique in glTF; check `duplicate_names`. Skins become `SkinnedMesh`; meshes with
morph targets get weights you can set with `anim.set_morph_weights`.

Nodes named `collision_*` (collision hulls such as `collision_turret_0`) keep their entity, name
and transform and carry `GltfCollisionNode`, but their meshes are not drawn and their triangles
are not counted (ADR-1622): `collision` is `true` and `triangles` 0 on their rows, and
`hidden_collision_meshes` counts them. `show_collision: true` draws them for debugging. Check it
worked: `editor.capture_viewport` with and without `show_collision` and compare.

## The collision layer: `editor.collision_layer`

By default the loaders drop `collision_*` meshes. A tool that wants them as a marked layer sets
the loader option `keep_collision_nodes` (ADR-1658):

```rust,ignore
use regia_editor::mesh_source::{GltfLoadOptions, load_gltf_model_with_options};
let model = load_gltf_model_with_options(
    &path, &default_material, &|_| true,
    GltfLoadOptions { keep_collision_nodes: true },
)?;
// model.parts: what renders (unchanged). model.collision_parts: the hulls, one merged part.
```

`LoadedModel::collision_parts` is empty by default. A renderer draws `parts` only, so the hulls
never show unless a tool draws `collision_parts` itself as an overlay (for example with a
wireframe or translucent material). The cook is unchanged: it still skips `collision_*` meshes,
and no stored format changes.

`editor.collision_layer { path }` (read-only) loads with the option on and returns
`has_collision`, `collision_triangles`, `collision_vertices`, `collision_bounds` (`{min, max}` or
`null`) and `render_triangles`. A missing or unreadable file is `not_found`. To look at the
hulls in the viewport, spawn the model with `show_collision: true` and capture it.

## `editor.capture_viewport { debug_view: "wireframe" }`

Adds the meshes' edges (linear green) over the lit frame. `lit` is the default. Check it
worked: capture with and without and compare, or count the wire colour in the PNG.

## Surface debug views: `uv_checker`, `texel_density`, `material_id`

`editor.capture_viewport { debug_view }` also takes these modes. They replace the shading with
exact (unlit, display-referred) colors; capture `lit` afterwards to compare.

| `debug_view` | Extra params | Reads as |
| --- | --- | --- |
| `uv_checker` | `tile_m` (default 0.25; `0` = UV space) | A 4 x 4 checker; one cell is `tile_m` meters of surface. Equal square cells = even UVs; stretched cells = stretch. Red grows along U, green along V. |
| `texel_density` | `target_px_per_m` (512), `tolerance` (0.1) | Per triangle: blue below the target, green within `tolerance` (0.1 = 10 percent), red above. No base-colour texture counts as 1024 px. |
| `material_id` | none | One stable color per material name. The legend is `info.debug_legend`: `[{ name, color, hex }]`. |

```json
{ "method": "editor.capture_viewport", "params": { "debug_view": "texel_density", "target_px_per_m": 1024, "tolerance": 0.1 } }
```

Check it worked: count blue, green and red pixels in the PNG (a model at its budget is mostly
green), or compare with the `lit` capture. Material names come from the `MaterialLabel`
component, which `spawn_gltf_hierarchy` sets from the glTF material name; an unlabeled mesh
shows as `unnamed #rrggbb` (its base color).
