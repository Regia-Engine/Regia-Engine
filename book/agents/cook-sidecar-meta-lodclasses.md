# Cook image metadata and LOD classes

`regia cook --project <project>` creates missing `.meta` files for external images referenced by
`.gltf` or `.glb` sources (ADR-8054). The same preparation runs on live reimports. Existing metadata
is retained, including IDs, settings and provenance. Buffers, embedded images and data URIs do not
receive image metadata. A missing external image remains an import error.

The source image's ID identifies its authoring record. Each glTF still owns its cooked texture
sub-assets, whose IDs derive from that glTF's metadata. Commit source metadata and preserve it in
staging projects; do not replace existing metadata just to change a compression setting.

Use the existing agent surface after cooking:

```json
{"method":"editor.asset_provenance","params":{"asset":"assets/props/paint.png"}}
{"method":"assets.import_rules","params":{"path":"assets/props/paint.png"}}
{"method":"assets.list","params":{"query":"assets/props/crate.gltf#"}}
```

`editor.asset_provenance` should return `meta: true` and the same `id` after a second cook.
`assets.list` shows the owning glTF's texture sub-assets; the image has no duplicate standalone
cooked entry. `editor.reimport` re-cooks an existing source while retaining its metadata. See
[Asset provenance](asset-provenance.md), [Import rules](import-rules.md), and [Assets](assets.md)
for method parameters and permission requirements. Malformed metadata fails the source cook and
is reported by `editor.asset_provenance` as `meta_unreadable`; a missing source gives
`asset_not_found`. No new protocol methods or error codes are introduced.

Place `lod_classes.toml` at the project root, beside `Regia.toml`:

```toml
version = 1
[classes]
crate = [1.3, 0.6, 0.2, 0.04]
```

For `assets/props/crate.glb`, authored `Stem_LOD0` through `Stem_LOD3` use these screen-size
thresholds. Class selection is the first root-node `lod_class` extra in the default scene, then
the first matching `[[assign]]` path rule, then an exact match between the filename stem and a
class name. An explicitly unknown class still warns and uses defaults. A node's own
`lod_screen_size` overrides its class threshold. See [glTF conventions](../../docs/formats/gltf-conventions.md).

The table is a cook input: editing or removing it invalidates affected cache entries. A project
staging script must stage the table too. The acceptance tests `sidecar_image_gets_meta` and
`lod_class_table_reaches_cook` inspect metadata and cooked LOD payloads with deterministic fixtures.
