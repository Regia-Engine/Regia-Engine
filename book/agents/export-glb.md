# Exporting a level, prefab or entities to glTF (`asset.export_glb`, `regia export glb`)

An artist can take a placed level, a prefab or an editor selection back to a DCC tool as one `.glb`
(glTF 2.0 binary). The same exporter runs from the editor menu (**File → Export selection to glTF…**),
the CLI and the agent protocol.

## What is written

| Regia | glTF | Notes |
|---|---|---|
| Entity hierarchy and `Transform` | Nodes with `translation`, `rotation`, `scale` | The entity's guid is in the node's `extras.regia_guid`. |
| `MeshSource` (a `.gltf`/`.glb` under the project) | Mesh primitives, node transforms of the source baked in | `NORMAL`, `TANGENT`, `TEXCOORD_0`, `TEXCOORD_1`, `COLOR_0` are kept. One mesh per source file and material override, shared by every node that uses it. |
| `MeshSource` of `builtin:cube`, `builtin:plane`, `builtin:sphere` | A generated mesh | |
| The source glTF's materials | Metallic-roughness materials | Factors, `alphaMode` (`BLEND`, `MASK` with cutoff), `doubleSided`, emissive (with `KHR_materials_emissive_strength` above 1), normal scale, occlusion strength, and `KHR_texture_transform`. |
| `MeshSource.material` (a `.material.ron`) | One metallic-roughness material for the mesh | Base color, metallic, roughness, emissive, and the base color, normal, ORM and emissive maps. A packed ORM map is written as both the metallic-roughness and the occlusion texture. Other material kinds (`.matinst.ron`, `.matgraph.ron`) keep the mesh's own materials, with a warning. |
| Textures | Embedded PNG or JPEG | PNG and JPEG files are embedded byte for byte; a glTF source's images are re-encoded as PNG with the same pixels. Each image is written once. |
| `PointLight`, `SpotLight`, `DirectionalLight` | `KHR_lights_punctual` | Point intensity lumens → candela (÷ 4π); spot candela and directional lux as is. A directional light's world `direction` becomes the node's rotation (glTF lights shine down local −Z). Cone angles are clamped to the specification's `0 ≤ inner < outer ≤ π/2`. |
| `Camera` | `cameras` | Vertical field of view, near and far (an infinite far is omitted). An orthographic camera is square (`ortho_size` both ways) because the aspect ratio belongs to the viewport. |
| Prefab instances in a level | The prefab's entities under the instance node | Found through the `.meta` sidecars under `assets/`. Overrides and component edits of the instance are not applied (a warning counts them). |

A `MeshSource` file that is gone comes from the project's cook, when the cache has it (see *Missing sources*).
Skinned meshes and animation clips are written with `skins` and `animations` (see *Skins and clips*).
Everything else (terrain, particles, scripts, physics) is not exported.

## Missing sources

A mesh, material or texture file that is no longer there is rebuilt from the cooked sub-assets in
`.regia/cooked/<platform>/`, the host platform's cache first (ADR-3016). The `.meta` beside the missing file names the
ids; with the `.meta` gone too, the asset database's row does. Each use adds a warning.

| Warning | Written from the cook |
|---|---|
| `mesh_from_cooked` | LOD 0 of the model: positions, normals, tangents, the first UV set, vertex colors, and its materials. Every primitive is in the entity's one mesh, as for a source. |
| `mesh_from_cooked_rigid` | The same, and the cooked mesh also had a skin or morph targets, which are not exported. |
| `material_from_cooked` | A `.material.ron` that is gone: its factors and maps. |
| `texture_from_cooked` | Level 0 of the cooked texture, decoded and embedded as PNG. Block compression is lossy, so the pixels are near the source, not equal to it. A cooked two-channel normal map gets its Z back. |

With nothing in the cache the node is exported without a mesh (`mesh_missing`), or the slot is left empty
(`texture_not_found`, `texture_skipped`). An instanced source (`EXT_mesh_gpu_instancing`) writes each copy into the
mesh, up to 4096 per node (`instances_truncated`).

## Axes

Regia looks down −Z; glTF assets face +Z (ADR-009). The importer bakes a half turn about +Y above the
scene. The exporter is its inverse: by default the first node is **`Regia_to_glTF`**, rotated 180° about +Y,
with the whole scene below it, and mesh vertices from a source glTF are written as the importer cooked
them. A prop placed at the origin with no rotation comes out as the artist authored it, and importing the
file again gives the same Regia-space positions and node transforms. `forward: keep` (CLI `--keep-axes`)
writes Regia axes with no root node.

## Selection

`entities` lists entity guids. Each is exported with its descendants. An exported entity whose parent is
not exported keeps its **world** transform. An unknown guid is `glb_unknown_entity`.

## Agent: `asset.export_glb`

Permission `assets`. Parameters: `level` or `prefab` (a project-relative path; with neither, the live open
level is snapshotted), `entities?`, `out` (project-relative, ends in `.glb`), `forward?` (`convert` or
`keep`), `skins?`, `animations?`. While a sequence is previewed (the Sequencer's Preview), the live level
and File → Export selection export the values as they were before the preview, as a save does; the preview
stays on.

```json,ignore
{ "jsonrpc": "2.0", "id": 1, "method": "asset.export_glb",
  "params": { "level": "assets/maps/yard.level.ron", "out": "export/yard.glb" } }
```

The answer is `{ ok, out, bytes, nodes, meshes, triangles, materials, images, lights, cameras, skins,
animations, warnings }`. `warnings` lines start with a code: `mesh_missing`, `mesh_unreadable`,
`mesh_from_cooked`, `mesh_from_cooked_rigid`, `material_from_cooked`, `texture_from_cooked`, `cooked_unreadable`,
`path_rejected`, `texture_skipped`, `texture_not_found`, `material_kind_unsupported`,
`material_unreadable`, `prefab_not_found`, `prefab_overrides_ignored`, `transform_unreadable`,
`light_unreadable`, `camera_unreadable`, `primitive_mode_skipped`, `instances_truncated`.

### Errors

Errors carry `data.reason`:

| Reason | Meaning |
|---|---|
| `export_params` | A missing or malformed parameter (`out`, `entities`, `forward`, both `level` and `prefab`). |
| `export_out_invalid` | `out` does not end in `.glb`. |
| `export_no_level` | No `level`/`prefab` was named and the open level cannot be snapshotted. |
| `glb_read_failed` | The scene file could not be read. |
| `glb_parse_failed` | The scene file did not parse or migrate. |
| `glb_unknown_entity` | A guid in `entities` is not in the scene. |
| `glb_nothing_to_export` | The scene or the selection has no entities. |
| `glb_write_failed` | The `.glb` could not be written. |
| `glb_non_finite` | A mesh, morph target or animation in the scene holds a NaN or infinite float, which serde_json would write as `null` (an invalid glTF). |
| `path_outside_project` | `level`, `prefab` or `out` leaves the project (`..`, absolute path, symlink). |

## CLI

```text
regia export glb --level assets/maps/yard.level.ron --out yard.glb
regia export glb --prefab assets/prefabs/lamp.prefab.ron --out lamp.glb
regia export glb --level assets/maps/yard.level.ron --entities <guid>,<guid> --out selection.glb
```

`--project DIR` names the project root (default: the nearest `Regia.toml`, else the parent of `assets/`),
`--keep-axes`, `--skins`, `--animations`, and `--json` for a summary object. The file is written through a
temporary file and a rename.

## Determinism

The same scene and assets give identical bytes: entities, materials and images are written in file order,
keys are sorted, and PNG encoding is fixed.

## Checking the file

Open the `.glb` in any glTF viewer, or import it again: `regia cook` cooks it like any `.glb`.
