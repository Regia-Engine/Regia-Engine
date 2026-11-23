# Asset registry

The asset registry answers questions about the project's cooked assets without loading any of
them: which assets carry a tag, which are textures, what a model depends on, what uses a texture.
Tools, the editor, agents and game code all read the same file.

## What the cook records

`regia cook` (and the editor's cook and live cook) writes `asset_registry.bin` into the cooked cache
(`.regia/cooked/<platform>/`) right after the manifest. For every cooked asset, including sub-assets
of a glTF such as `assets/tree.glb#material/Bark`, it records:

| Field | Where it comes from |
|---|---|
| id, path, type | the manifest |
| dependencies | the cooked file's header (only the first bytes of the file are read) |
| tags | automatic tags plus your rules (below) |
| size | the cooked file's length |

Automatic tags: `ext` (the source's extension in lower case), `folder` (its folder under `assets/`,
absent at the root) and `importer` (the importer that cooked it).

An unchanged cook leaves the file untouched. The live cook (`regia run --dev`, the editor) updates
only the records of the sources it re-cooked.

Packaging writes one registry per pak next to it (`base.fpak` → `base.registry.bin`) and ships it
in the package's `content/` folder, so a packaged game can query its own assets.

## Tagging assets

Write rules in `config/asset_tags.ron` (format reference: `docs/formats/asset-tags-ron.md`):

```ron
(
    format_version: 1,
    rules: [
        (paths: ["assets/props/forest/**"], tags: { "biome": "forest" }),
        (paths: ["assets/ui/**"], tags: { "group": "ui", "streaming": "always" }),
    ],
)
```

A rule applies to every asset whose source path matches one of its globs (`*` stays inside a
folder, `**` crosses folders, `?` is one character). Rules apply in file order; a later rule
replaces an earlier rule's value for the same key, and your rules replace automatic tags with the
same key. Sub-assets take their source's tags. Tag keys must not be empty or contain `=` or spaces.
A file with a mistake is reported as one cook warning and the registry is written without your tags.

Changing the rules takes effect for a source the next time it is cooked; run `regia cook` to retag
everything.

## Querying from the command line

```sh
regia assets query --tag biome=forest                 # every forest asset
regia assets query --type CookedTexture --path 'assets/ui/**'
regia assets query --contains moss --json             # machine-readable
regia assets query --deps assets/props/tree.glb --transitive
regia assets query --refs textures/moss.png           # what uses this texture
regia assets tags                                     # tag keys and counts
regia assets tags --key biome                         # one key's values
```

`--platform` picks the cooked platform (default: this machine's), `--project` the project folder,
and `--registry FILE` reads a shipped `base.registry.bin` instead. Filters combine with AND;
`--limit` caps the printed list (the total is always shown).

## Querying from code

```rust,wrap
# use std::path::Path;
# use regia_asset::AssetId;
# let cache_dir = Path::new("cache");
# let texture_id = AssetId::NIL;
use regia_asset::registry::{AssetRegistry, RegistryQuery, TagFilter, cache_registry_path};

let registry = AssetRegistry::read_from(&cache_registry_path(&cache_dir))?.unwrap_or_default();
let forest = registry.query(&RegistryQuery {
    tags: vec![TagFilter::parse("biome=forest")?],
    ..Default::default()
});
for asset in forest.entries {
    println!("{} ({})", asset.path, asset.type_path);
}
let users = registry.referencers(texture_id, true);
```

`LoadedAssetRegistry::load(cache_dir, content_dir)` reads the cache registry and the per-pak
registries together, as the running app does. Agents use the `registry.*` methods
([asset registry for agents](../agents/asset-registry.md)).
