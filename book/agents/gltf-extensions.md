# glTF extensions on import (`asset.gltf_extensions`)

What the importer does with the extensions a `.gltf` or `.glb` names, and how to find out for one file before cooking
it. The rules are in [`docs/formats/gltf-extensions.md`](../../docs/formats/gltf-extensions.md).

## What is read

| Extension | Result |
|---|---|
| `KHR_mesh_quantization` | Integer positions, normals, tangents, UVs and morph deltas read as floats, normalized or not. Signed (`BYTE`, `SHORT`) UVs are accepted only when the file names the extension; colors, joints, weights and indices keep core glTF's types (ADR-5958). |
| `EXT_meshopt_compression` | Compressed views decode (`ATTRIBUTES`, `TRIANGLES`, `INDICES`, and the `OCTAHEDRAL`, `QUATERNION`, `EXPONENTIAL` filters). A `fallback` buffer is never read from the file. |
| `EXT_mesh_gpu_instancing` | One transform per instance on the prefab: the `prefab/instances` sub-asset ([format](../../docs/formats/prefab-instances.md)), with the geometry cooked once. |
| `KHR_materials_variants` | One `MaterialVariantSet` per mapped primitive, every variant name in every set. |
| `EXT_texture_webp` | The WebP image is the texture's image, over any fallback `source`. |
| `KHR_materials_*`, `REGIA_materials_detail` | Cooked into the material, or kept in the side table where the renderer has no parameter yet (`KHR_materials_sheen`, `KHR_materials_specular`). |
| `KHR_texture_transform` | Kept in the side table; the renderer does not apply it yet. |
| `KHR_lights_punctual` | Read into `GltfExtensionTable::lights` and `node_lights`. `editor.spawn_gltf_hierarchy` gives each node its light; a cooked model has none. |

## What is refused

A **required** extension the importer cannot read stops the import with `gltf_ext_unsupported`, naming it. That covers
`KHR_draco_mesh_compression`, `KHR_texture_basisu` and anything unknown. An **optional** one falls back with a warning,
and its blocks stay in `GltfExtensionTable::unknown`. An optional one with nothing to fall back on (positions with no
accessor data, a texture with no `source`) is still `gltf_ext_unsupported`.

## Agent: `asset.gltf_extensions`

Permission `assets`, read-only. Opens the file like the importer, without decoding images or cooking anything.
The editor registers it (`regia_editor::gltf_ext_agent`), so the runtime schema does not list it: the call below is
tagged `json,ignore` for the guide's example check.

```json,ignore
{ "jsonrpc": "2.0", "id": 1, "method": "asset.gltf_extensions",
  "params": { "path": "assets/props/forest.glb" } }
```

```json
{ "ok": true, "path": "assets/props/forest.glb",
  "extensions": [
    { "name": "EXT_mesh_gpu_instancing", "required": false, "support": "cooked" },
    { "name": "KHR_mesh_quantization", "required": true, "support": "cooked" },
    { "name": "VENDOR_extra", "required": false, "support": null }
  ],
  "fallbacks": ["VENDOR_extra"],
  "meshopt_views": 0, "quantized_accessors": 3, "baked_uv_materials": [],
  "instanced_nodes": [[0, 1200]],
  "variants": [], "variant_primitives": 0 }
```

`support` is `cooked`, `side_table`, or `null` when the importer cannot read the extension.
`instanced_nodes` lists `[node index, instance count]`. `baked_uv_materials` lists the materials whose
`KHR_texture_transform` was baked into quantized UVs; a host that applies transforms itself skips those.

### Errors

Errors carry `data.reason`; an extension's own error also carries `data.extension`.

| Reason | Meaning |
|---|---|
| `gltf_ext_params` | `path` is missing. |
| `gltf_ext_unsupported` | The file needs an extension the importer cannot read. `data.extension` names it. |
| `gltf_meshopt_invalid` | A compressed view breaks a rule of `EXT_meshopt_compression`, or its stream does not decode. |
| `gltf_quantization_invalid` | An integer attribute uses a component type nothing allows (`UNSIGNED_INT` positions, say). |
| `gltf_instancing_invalid` | An `EXT_mesh_gpu_instancing` block is broken: counts that disagree, a wrong accessor type, a non-finite instance. |
| `gltf_unreadable` | The file is not a readable glTF, or an image or buffer of it is missing. |
| `path_outside_project` | `path` leaves the project. |

A missing file is the not-found error `F0003`.

## From code

- `regia_asset_pipeline::gltf_extension_report(path)` is this report.
- `regia_asset_pipeline::open_gltf(path)` gives the document, buffers and images with every extension applied, for a
  tool that reads accessors; `GltfOpened::load_data.instances` has the instance transforms. `open_gltf_checked` is the
  same without images.
- `GltfImportResult::instances` and `GltfImportResult::material_variants` hold what the import made.
- `regia_asset::PrefabVariantSelection` switches a model's variants. A `.prefab` that spawns a cooked model has no
  variant field yet; a host keeps the selection as a component and reads `material(slot)`.

## Limits

- A cooked model drawn through `regia_render::cooked_model` shows one copy of an instanced node: that path does not read
  the instance set yet. The editor's named hierarchy draws them all, and its merged model and the scene export bake up to
  4096 copies per node.
- A cooked model has no lights: nothing cooks `KHR_lights_punctual`, so it reaches a pak only through a host reading
  `GltfImportResult::extensions`.
- `KHR_meshopt_compression`, the newer ratified name of the same codec, is not read: a file requiring it is refused.
- Basis and Draco have no decoder here, on purpose: neither is linked into the pipeline.

The fuzz targets `import.gltf_ext` and `import.meshopt` (`fuzz.targets`, in the editor) cover these paths.
