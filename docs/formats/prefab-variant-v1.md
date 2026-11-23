# `.variant.ron`, prefab variant, format version 1

A prefab variant: a base prefab (or another variant) and the edits that turn it into the variant
(`regia_scene::prefab_variant::PrefabVariantFile`, registry name `prefab-variant`). Design:
[`docs/specs/83-prefab-profiler-volumes.md`](../specs/83-prefab-profiler-volumes.md) § 2 (ADR-2323).

```ron
PrefabVariant(
    format_version: 1,
    base: (
        prefab: "asset:3f7a1c2e-8b64-4a35-9d20-6f1e2b3c4d5a",
        overrides: [(target: "9b2e6f0a-1e42-4c11-8d5a-3c2f7e10d0aa", component: "game::Lamp", path: ".intensity", value: 250.0)],
        added_components: [(target: "9b2e6f0a-1e42-4c11-8d5a-3c2f7e10d0aa", component: "game::Flicker", value: (rate: 2.5))],
        removed_components: [],
        removed_children: [],
    ),
)
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `format_version` | `u32` | `1` | A file without it is version 0 and loads as 1; a newer one is refused. |
| `base` | prefab instance record | required | The same record a level stores for a prefab instance: `prefab` (the base's asset id), `overrides`, `added_components`, `removed_components`, `added_children`, `removed_children`. Targets are the base's local ids. |

Instantiating a variant resolves the chain to the base template, spawns it, applies each step's record
base-most first, then the instance's own record. No values are copied, so a change to the base reaches
every variant. Chains are capped at 16 steps; a cycle, a missing base or a nil id is refused.

**Versioning** (`docs/specs/12-assets.md` § 4a): the serde shape is hashed by `cargo xtask
check-formats` (entry `prefab-variant`); `prefab_variant_migration_sources()` is `[0]`. `regia
upgrade-project` re-saves a version-0 file at version 1. Fuzz target: `prefab-variant`.
