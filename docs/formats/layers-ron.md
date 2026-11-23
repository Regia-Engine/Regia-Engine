# Layer definitions (`*.layers.ron`), format `layers` v1

Named data layers (spec 37 § 4). `regia_scene::layers::LayerDefs`, `LAYERS_FORMAT_VERSION = 1`.

```ron
LayerDefs(
  format_version: 1,
  layers: [
    (name: "night_props", kind: Runtime, debug_color: (0.1, 0.1, 0.5), parent: None),
    (name: "quest_a",     kind: Runtime, debug_color: (0.8, 0.3, 0.1), parent: None),
    (name: "quest_a_boss",kind: Runtime, debug_color: (0.9, 0.1, 0.1), parent: Some("quest_a")),
    (name: "blockout",    kind: EditorOnly, debug_color: (0.5, 0.5, 0.5), parent: None),
  ],
)
```

- `name`: ASCII `[a-z0-9_]{1,48}`, unique per asset; the stable key.
- `kind`: `Runtime` (cooked, switchable by gameplay) or `EditorOnly` (dropped from cooked output
  with its members unless they also belong to a runtime layer; `strip_editor_only`).
- `parent`: a child layer is never more active than its parent. A missing parent or a cycle is
  `layer_parent_invalid` (fatal for the asset).
- A newer `format_version` is refused.

A level binds definitions through its `regia_scene::layers::LevelLayers` resource (stored in the
level's `resources`, no scene version bump):

```ron
"regia_scene::layers::LevelLayers": (
    defs: ["asset:6f1e2b3c-4d5a-4a35-9d20-3f7a1c2e8b64"],
    entries: [
        (layer: "night_props", initial: Absent, editor_visible: true, editor_resident: true, peers: Everyone),
    ],
),
```

A definition asset carries its id in a `.meta` sidecar next to it (`forest.layers.ron.meta`, field `id`), which is what
`asset:<guid>` names. `LevelLayers` also has `embedded: [LayerDefs]`, empty in authored levels and left out of the text:
`regia package` moves the bound definitions there, without their `EditorOnly` layers, because a package ships no `.meta`
files. Embedded definitions are searched first.

An entity's membership is its `regia_scene::layers::LayerMembership` component:
`"regia_scene::layers::LayerMembership": (layers: ["night_props"])`.

Migration: v1 is the first version. A shape change bumps `LAYERS_FORMAT_VERSION`, adds a migrator
and an `upgrade-project` entry, and re-records the hash (`cargo xtask check-formats --update`).
