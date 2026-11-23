# glTF extensions

What the importer (`regia_asset_pipeline::gltf_import`, `regia_asset_pipeline::gltf_extensions`)
does with the glTF extensions a file declares (ADR-252, feature audit G9, ADR-3016). The cooked mesh format v2 is
unchanged. The cooked material format gained the thin film at v5 (`.material.ron` v4, ADR-1977).

## Which extensions a file may require

`asset.gltf_extensions { path }` reports this for one file ([the agent page](../../book/agents/gltf-extensions.md)).

| Extension | What the importer does |
|---|---|
| `KHR_mesh_quantization`, `EXT_meshopt_compression`, `EXT_mesh_gpu_instancing`, `EXT_texture_webp` | Applied when the file opens, in `regia_asset_pipeline::gltf_normalize` (below) |
| `KHR_materials_variants` | One `MaterialVariantSet` per mapped primitive (below) |
| `KHR_materials_clearcoat`, `_anisotropy`, `_emissive_strength`, `_iridescence`, `_ior`, `_transmission`, `_volume`, `REGIA_materials_detail` | Cooked into the material |
| `KHR_materials_sheen`, `_specular`, `KHR_texture_transform` | Read into the side table; the renderer has no parameter yet |
| `KHR_lights_punctual` | Read into `GltfExtensionTable::lights` and `node_lights`; the editor's named hierarchy spawns them, the cook does not (below) |
| Anything else, `KHR_draco_mesh_compression` and `KHR_texture_basisu` included | **Required**: the import stops with `gltf_ext_unsupported`, naming the extension. **Optional**: the import falls back and warns; the name is in `GltfExtensionTable::fallbacks()` and its blocks in `unknown` |

An optional extension can only fall back when the file holds what it falls back to. A primitive whose
`POSITION` accessor has no data (`KHR_draco_mesh_compression` with no fallback accessors) and a texture with no
`source` (`KHR_texture_basisu` alone) are `gltf_ext_unsupported` even when the extension is optional.

The other import errors with a code: `gltf_meshopt_invalid`, `gltf_quantization_invalid`,
`gltf_instancing_invalid`. `ImportError::code()` gives the code; the message starts with it.

## Applied when the file opens

These rewrite the document in memory, so the cook, the tools, the scene export and the editor's readers all see
plain floats. A file that uses none of them keeps its document and buffers untouched, so its cook is unchanged.

**`KHR_mesh_quantization`.** Every integer `POSITION`, `NORMAL`, `TANGENT`, `TEXCOORD_n` and morph delta becomes a
float accessor: normalized values as glTF 2.0 § 3.11 says (`c / max` unsigned, `max(c / max, -1)` signed), plain
integers keep their value, for a node matrix or a texture transform to scale. `POSITION`'s `min` and `max`, which
the file stores in the integers, are recomputed. Sparse accessors are applied first. `UNSIGNED_INT` positions,
normals, tangents and morph deltas are `gltf_quantization_invalid`. A `BYTE` or `SHORT` `TEXCOORD_n` passes the
open's component-type check only in a file that names the extension in `extensionsUsed` or `extensionsRequired`
(ADR-5958); without it, it is refused (`component type its use does not allow`, ADR-2723). The extension's table has
no `COLOR_n`, `JOINTS_n`, `WEIGHTS_n` or index row, so those keep core glTF's component types with the extension
declared too: a signed or `UNSIGNED_INT` color, joint, weight or index, and an `UNSIGNED_INT` texture coordinate, are
refused the same way. An unsigned color without `normalized: true` still reads as normalized: glTF has no color
above 1 in an integer type, and leaving the flag out is a writer's mistake.

A quantizer scales UVs back with `KHR_texture_transform`, and the renderer has no UV transform, so when a primitive
holds a UV set as integers, the transform its material gives that set is baked into the vertices (`uv' = T · R · S ·
uv`). Every primitive of that material gets it on that set, floats too, since they share the one transform. If the
material's textures on the set disagree, the first wins in the order base color, metallic-roughness, normal,
occlusion, emissive, with a warning. `GltfLoadData::baked_uv_materials` and the agent report list these materials.
The side table and the raw JSON still show the file's block, so a host that applies transforms itself skips them;
scene export does. A float UV set on any other material keeps its transform in the side table only.

**`EXT_meshopt_compression`.** Each compressed view decodes into the buffer and offset the view names, so its
accessors stay as they are. Modes `ATTRIBUTES`, `TRIANGLES` and `INDICES` and filters `NONE`, `OCTAHEDRAL`,
`QUATERNION` and `EXPONENTIAL` are read (ADR-3016). The stream wins over any bytes at that place. A buffer marked
`fallback: true` is never read from the file, with or without a `uri`. `TRIANGLES` may rotate a triangle's corners,
as the codec's encoder does; the winding is kept. Checked first, so a hostile file gets an error and not a large
allocation or a C `assert`: stride and count per mode and filter, `count * byteStride` equal to the view's
`byteLength`, the decoded size against the stream length (`MAX_DECODE_RATIO`) and the view's offset against what was
decoded.

**`EXT_mesh_gpu_instancing`.** A node's `TRANSLATION`, `ROTATION` and `SCALE` become one matrix per instance (a
missing attribute is the identity for it, and a `_CUSTOM` attribute is ignored). The cook writes them to the
`prefab/instances` sub-asset ([prefab-instances.md](prefab-instances.md)); the geometry is cooked once. Attributes
may be quantized or compressed. Counts that disagree, a wrong accessor type, a zero or non-finite instance are
`gltf_instancing_invalid`. A node with no mesh, a skinned node or a block with no known attribute draws once, with a
warning. The editor's named hierarchy spawns a child entity per instance, and its merged model and the scene export
bake up to `MAX_BAKED_INSTANCES` copies.

**`EXT_texture_webp`.** The texture's image is the extension's `source`, which wins over a fallback `source`. WebP
images decode with the `image` crate, on every path (embedded, `data:` URI, external file). A fallback image stays at
its index and cooks like any other image of the file.

## `KHR_materials_variants`

`GltfExtensionTable::variant_names` and `variants` hold the file's variant names and each primitive's mappings. A bad
mapping (a material or variant index out of range, a variant mapped twice) is dropped with a warning.

The cook writes one `MaterialVariantSet` per mapped primitive. Its `slot` is the primitive's mesh sub-path without
`mesh/` (`Body/primitive_0`), and it lists every variant name of the file: a variant the primitive has no mapping for
draws the primitive's default material, the extension's rule. So one name selects the whole model:
`regia_asset::PrefabVariantSelection` holds a model's sets, `select(name)` switches and `material(slot)` gives what a
slot draws, with `None` for the material it was authored with. `PrefabInstance` still switches one set. The naming
convention (`hull_Rem`, `hull_A52`, [gltf-conventions.md](gltf-conventions.md)) still groups materials beside these.

## Read where the renderer has a parameter

| Extension | Result | Renderer |
|---|---|---|
| `KHR_materials_clearcoat` | `MaterialAssetRon::clearcoat` = `{ factor, roughness, normal_map }` when `clearcoatFactor > 0` | second specular lobe (`HardSurfaceFactors::clearcoat`) |
| `KHR_materials_anisotropy` | `MaterialAssetRon::anisotropy` = `{ strength, direction: [cos rot, sin rot] }` when `anisotropyStrength != 0` | anisotropic GGX (`HardSurfaceFactors::anisotropy`, rotation `atan2(dir.y, dir.x)`) |
| `KHR_materials_emissive_strength` | `StandardMaterialParams::emissive_strength` | yes |
| `KHR_materials_iridescence` | `MaterialAssetRon::thin_film` = `{ factor, factor_map, ior, thickness_min, thickness_max, thickness_map }` when `iridescenceFactor > 0` (ADR-1977) | thin film over the base specular (`LabMaterial::thin_film`; [the agent page](../../book/agents/thin-film.md) lists what it does not cover) |
| `KHR_materials_ior`, `_transmission`, `_volume` | as before (ADR-069 and later); `transmissionTexture` is not read (one warning per material) | yes |
| A transmissive material's `extras.regia_transmission_roughness` | `MaterialAssetRon::transmission_roughness`: `true` takes `roughnessFactor`, a number is that roughness clamped to 0..=1 (ADR-2732) | frosted transmission (`LabMaterial::transmission_roughness`) |
| `REGIA_materials_detail`, or the material's `extras.regia_detail` | `HardSurfaceParams::detail` (`DetailMapParams`) | the tiling detail layer |

Strength, factors and roughness are clamped to their glTF ranges; a non-finite number reads as the
glTF default. `clearcoatNormalTexture` is cooked as a normal map and is a dependency of the cooked
material.

A glTF file has one roughness for its reflection and transmission. A transmissive material stays clear
glass unless it opts in with `extras.regia_transmission_roughness`, because importing `roughnessFactor`
for every file would blur every existing glass asset on its next cook (ADR-2075, ADR-2732). With `true`,
the one number is `roughnessFactor`; the metallic-roughness texture still varies only the reflection.

`KHR_materials_iridescence` takes the extension's defaults for an absent property: factor 0, IOR 1.3,
thickness 100 to 400 nm. A factor of 0 cooks exactly like no extension. `iridescenceTexture` is read
from R and `iridescenceThicknessTexture` from G; both are cooked linear with all their channels and
are dependencies of the cooked material. The runtime packs them into one map (R coverage, G thickness)
that uses the first UV set; `texCoord` and `KHR_texture_transform` on them stay in the side table.

## The material's detail layer: `REGIA_materials_detail`

The fine surface layer of a material (`.material.ron`: `hard_surface.detail`) can be authored in the
glTF itself, so fine paint or surface detail on a hull is not limited by its texture atlas (ADR-2000).
A vendor extension for writers that can emit one, or the same object under `extras.regia_detail` for
those that can only write custom properties (a DCC's custom property on the material). The extension
wins when a material has both. Declare the extension in `extensionsUsed`; it never needs to be in
`extensionsRequired`, because a reader without it still has the base material.

```json
"extensions": {
  "REGIA_materials_detail": {
    "normalTexture": { "index": 0 },
    "albedoTexture": { "index": 1 },
    "roughnessTexture": { "index": 2 },
    "maskTexture": { "index": 3 },
    "uvScale": [64.0, 32.0],
    "albedoStrength": 0.25,
    "normalStrength": 0.75,
    "roughnessStrength": 0.5,
    "fadeStart": 2.0,
    "fadeEnd": 12.0
  }
}
```

| Field | `DetailMapParams` | Default | Range |
|---|---|---|---|
| `normalTexture` | `normal`: tangent-space detail normal | none | a glTF texture |
| `albedoTexture` | `albedo`: RGB centered on 0.5, multiplied by two | none | a glTF texture |
| `roughnessTexture` | `roughness`: linear 0..1 | none | a glTF texture |
| `maskTexture` | `mask`: white applies the detail fully | none | a glTF texture |
| `uvScale` | `uv_scale`: detail UV over the base UV | `[4, 4]` | each above 0 |
| `albedoStrength` | `albedo_strength` | 1 | 0..1 |
| `normalStrength` | `normal_strength` | 1 | 0..2 |
| `roughnessStrength` | `roughness_strength` | 1 | 0..1 |
| `fadeStart` | `fade_start`: distance where the weight is still 1 | 0 | 0 or more |
| `fadeEnd` | `fade_end`: distance where it reaches 0; 0 keeps it on | 0 | 0 or more |

**Channels.** The block-compressed runtime path reads two packed maps: `albedoTexture` RGB with the
detail roughness in **alpha**, `normalTexture` RGB with the detail mask in **alpha**. So author one RGBA
image for each pair: `roughnessTexture` and `maskTexture` as separate images are honored only by images
small enough to stay RGBA8 in the cook and by the uncompressed path, and are ignored beside a
block-compressed map (use the alpha channels there). All four are cooked as linear color, not as normal
maps, because a two-channel normal format would drop channels the detail layer reads. A bad number reads
as its default or is clamped; a texture index that does not resolve leaves that slot empty.

No cooked or authored format changed: `hard_surface.detail` has been part of the material since
format 2. The glTF importer moved to 1.10.0, so glTF files recook once.

## Read into the side table (the renderer ignores them for now)

`GltfImportResult::extensions` (`GltfExtensionTable`) is not part of any cooked file.

| Extension | Side-table field | Why not rendered |
|---|---|---|
| `KHR_materials_sheen` | `materials[i].sheen` `{ color, roughness }`; `fabric_sheen()` gives the renderer's `FabricSheen` | Sheen is a per-entity `MaterialShading::sheen`, not a material field; a host attaches it |
| `KHR_materials_specular` | `materials[i].specular` `{ factor, color }` | the BRDF has no specular colour or factor; `KHR_materials_ior` already sets F0 |
| `KHR_texture_transform` | `materials[i].textures[slot].transform` `{ offset, rotation, scale, tex_coord }`; `TextureTransform::apply(uv)` | the forward shader has no UV transform (over quantized UVs it is baked, above) |
| textures of the extensions above | `materials[i].textures["clearcoat" \| "clearcoat_roughness" \| "clearcoat_normal" \| "sheen_color" \| "sheen_roughness" \| "specular" \| "specular_color" \| "anisotropy"]` as glTF image indices (the cook writes `texture/<image>`) | not sampled by the shader |

The five core slots (`base_color`, `metallic_roughness`, `normal`, `occlusion`, `emissive`) appear in
`textures` only when they carry a `KHR_texture_transform`. A `texCoord` in the transform block overrides
the texture's.

## Punctual lights

`GltfExtensionTable::lights` holds each light in the extension's units, with its defaults filled in: white, intensity 1,
no range, a spot's cone 0 to π/4. Numbers no renderer could use are clamped: a negative intensity or color to 0, a range
at or below 0 to none, a spot's outer cone into (0, π/2] and its inner cone below that. A light of an unknown type is
dropped, and `node_lights` stays right because it holds positions in `lights`, not in the file.

`spawn_gltf_hierarchy` turns them into components on the node's entity. A point light's candela becomes lumens (× 4π),
the reverse of scene export; spot candela and directional lux carry over. The engine's local lights need a finite cutoff,
so a light with no `range` ends where the inverse-square law takes it to 1 lux (`sqrt(candela)` meters, 1 to 1000), which
puts the engine's default point light at about 8 m against its 10 m. Shadows stay off, the components' default for local
lights; a directional light keeps the component's default of casting. Direction comes from the node's -Z in world space
at spawn. The renderer draws one directional light, so a model's joins only a world that has none: otherwise the node stays
empty and the report warns, and a model never replaces the scene's sun.

Nothing cooks these: there is no light in a cooked model, and a host wanting them in a pak reads
`GltfImportResult::extensions`.

## Everything else is kept

Any extension block that is neither read above nor claimed by a handler is
stored in `GltfExtensionTable::unknown` with its scope (`asset`, `scene`, `node`, `mesh`, `primitive`,
`material`, `texture`, `image`, `camera`, `skin`, `animation`), index, name and raw JSON. The import logs
**one** `tracing` warning per file naming every unknown extension.
At most 4096 blocks are kept (`MAX_UNKNOWN_EXTENSIONS`); the rest are counted in `dropped_unknown`.
Blocks on a `textureInfo` other than `KHR_texture_transform` are not visited.

## Handler hook

```rust,ignore
let mut handlers = GltfExtensionRegistry::new();
handlers.on_extension("VENDOR_node_tags", |target, json| {
    println!("{:?} {} {:?}: {json}", target.scope, target.index, target.name);
});
handlers.register(MyHandler); // impl GltfExtensionHandler { handles(&self, name), handle(&mut self, target, name, json) }
let result = import_gltf_path_with_handlers(path, parent, forward, "desktop", None, quality, &mut handlers)?;
```

A handler is called for every block of an extension it claims (known extensions too), in document
order: root, scenes, nodes, textures, images, cameras, skins, animations, then meshes with their
primitives, then materials. A claimed extension is not "unknown".
`import_gltf_glb_bytes_with_handlers` is the in-memory form. `GltfExtensionTable::from_bytes` reads a
table without importing.

## Hostile input

The table is built from `serde_json` values with bounds-checked access: wrong types, negative or huge
indices, non-finite numbers, deep nesting (serde_json's depth limit), truncated GLB framing and a
4096-entry cap all produce a smaller table, never a panic.
