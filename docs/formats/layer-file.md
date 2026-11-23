# Layer file (`<level>.<layer>.layer.ron`), format `layer-file` v1

One data layer's entity records stored in their own file next to the level (spec 37 § 12, ADR-7792), so
two people can edit different layers of one level without touching the same file.
`regia_scene::layer_file::LayerFile`, `LAYER_FILE_FORMAT_VERSION = 1`.

```ron
LayerFile(
    format_version: 1,
    scene_format_version: 13,
    layer: "night_props",
    entities: [
        (
            at: 2,
            entity: (
                guid: "8f1c3a00-0000-0000-0000-000000000002",
                name: "Lamp",
                components: {
                    "regia_scene::layers::LayerMembership": (layers: ["night_props"]),
                    "regia_scene::transform::Transform": (translation: (1.0, 0.0, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)),
                },
            ),
        ),
    ],
)
```

- The file name is the reference: the level `forest.level.ron` (or `forest.level.d/`) owns every
  `forest.<layer>.layer.ron` beside it whose `<layer>` is a valid layer name (`[a-z0-9_]{1,48}`, so it has no `.`
  and the match is unambiguous). The level file itself has no field for it (no level format change).
- `layer`: the layer, equal to the middle of the file name.
- `entities`: records exactly as a level's `SceneFile` holds them, each with `at`, its position in the whole
  level's record list when it was split off. Merging puts every record back at its `at` in ascending order, so
  merging an unchanged split gives the original record order and bytes; records added later sit at the end.
- `scene_format_version`: the scene version of the records; an older one is migrated when the file is merged.
- A newer `format_version` or `scene_format_version` is `unsupported_version`. A guid that is also in the level or
  in another layer file is `duplicate_entity`.
- Readers: `regia_scene::level_dir::read_level_as_stored` (and so `read_level_source`, the runtime's level loading,
  the stream cook and the bake tools) fold every layer file into the level. `regia package` stages a level with layer
  files folded in and never ships a `.layer.ron`.
- Writer: the editor (`editor.layer_file`, the Layers panel; a level save writes each member to its layer file).
  A record goes to the file of the first layer (by name) it belongs to that is stored in its own file. Only the
  single-file level form is split.

Migration: v1 is the first version. A shape change bumps `LAYER_FILE_FORMAT_VERSION`, adds a migrator and an
`upgrade-project` entry, and re-records the hash (`cargo xtask check-formats --update`).
