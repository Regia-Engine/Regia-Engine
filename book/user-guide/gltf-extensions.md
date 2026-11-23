# glTF extensions

Regia Engine reads these glTF material extensions on import and keeps the rest.

| Extension | What happens |
|---|---|
| `KHR_materials_clearcoat` | becomes the material's clearcoat (factor, roughness, normal map) and renders |
| `KHR_materials_anisotropy` | becomes the material's anisotropy (strength, rotation) and renders |
| `KHR_materials_emissive_strength`, `_ior`, `_transmission`, `_volume` | render; `transmissionTexture` is not read and warns |
| A transmissive material's `extras.regia_transmission_roughness` | frosted transmission, opt-in: `true` takes the material's `roughnessFactor`, a number is that roughness (0..=1); see [Rough transmission](../agents/rough-transmission.md) |
| `KHR_materials_iridescence` | becomes the material's thin film (factor, IOR, thickness range, both maps) and renders; see [Thin film](../agents/thin-film.md) |
| `KHR_materials_sheen`, `KHR_materials_specular`, `KHR_texture_transform` | stored in the import's extension table; the renderer does not apply them yet |
| anything else | kept as raw JSON in the table; the import logs one warning per file |

Sheen is applied per entity: `result.extensions.material(i)?.sheen?.fabric_sheen()` gives the
`FabricSheen` to put in `MaterialShading::sheen`.

## Reading your own extension

```rust,wrap
# use std::path::Path;
# use regia_asset::AssetId;
# use regia_asset_pipeline::gltf_extensions::GltfExtensionRegistry;
# use regia_asset_pipeline::{Forward, TextureCookQuality, import_gltf_path_with_handlers};
# let path = Path::new("model.glb");
# let parent = AssetId::NIL;
let mut handlers = GltfExtensionRegistry::new();
handlers.on_extension("VENDOR_node_tags", |target, json| {
    // target.scope, target.index, target.name; json is the block as authored
});
let result = import_gltf_path_with_handlers(&path, parent, Forward::Keep, "desktop", None,
    TextureCookQuality::High, &mut handlers)?;
```

A claimed extension is not reported as unknown. Details, ranges and limits are in
`docs/formats/gltf-extensions.md`. To see what a file uses without importing it, call
[`editor.model_extensions`](../agents/model-report.md).
