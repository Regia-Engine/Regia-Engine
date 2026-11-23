# `.levelvariant.ron`, level variant, format version 1

A level variant: a base level (or another variant) and only the differences that turn it into the
variant (`regia_scene::level_variant::LevelVariantFile`, registry name `level-variant`, `.meta` kind
`level_variant_ron`). Design: ADR-7823. Agent guide: [level variants](../../book/agents/level-variants.md).

```ron
LevelVariant(
    format_version: 1,
    base: "asset:3f7a1c2e-8b64-4a35-9d20-6f1e2b3c4d5b",
    overrides: [(target: "9b2e6f0a-1e42-4c11-8d5a-3c2f7e10d0ab", component: "regia_scene::transform::Transform", path: ".translation", value: (5.0, 0.0, 0.0))],
    added_components: [(target: "9b2e6f0a-1e42-4c11-8d5a-3c2f7e10d0ab", component: "game::Flicker", value: (rate: 2.5))],
    removed_components: [(target: "9b2e6f0a-1e42-4c11-8d5a-3c2f7e10d0ab", component: "game::Tags")],
    record_edits: [(target: "9b2e6f0a-1e42-4c11-8d5a-3c2f7e10d0ab", field: "name", value: Some("Night gate"))],
    added_entities: [(guid: "1c0ffee0-0000-4000-8000-000000000002", name: "Crate", components: {})],
    removed_entities: ["1c0ffee0-0000-4000-8000-000000000003"],
    resources: {"game::Weather": (rain: true)},
    removed_resources: ["game::Fog"],
)
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `format_version` | `u32` | `1` | A file without it is version 0 and loads as 1; a newer one is refused (`newer_version`). |
| `base` | asset ref | required | The base level (`.level.ron` or `.level.d`) or variant, by the id in its `.meta`. |
| `overrides` | list of `(target, component, path, value)` | `[]` | Field overrides on base entities, the prefab instance override record. `path` is a field path (`.translation`, `.a.b`, `.0`, `[2]`); `""` replaces the whole component. |
| `added_components` | list of `(target, component, value)` | `[]` | Components the variant adds to base entities. |
| `removed_components` | list of `(target, component)` | `[]` | Components the variant takes off base entities. |
| `record_edits` | list of `(target, field, value)` | `[]` | A base entity's `name`, `parent` or `prefab_instance` set to the RON `value` (an `Option`). |
| `added_entities` | list of entity records | `[]` | Whole scene entity records; a parent may be a base entity. |
| `removed_entities` | list of guids | `[]` | Base entities removed, each with its descendants. |
| `resources` | map type path → RON | `{}` | Resources the variant sets (whole values, e.g. the level settings). |
| `removed_resources` | list of type paths | `[]` | Base resources the variant drops. |

**Resolving.** The loader walks the chain to a plain level and applies each step base-most first:
record edits, removals (with descendants, as parented after the record edits), component removals and
additions, field overrides, added entities, resources. Overrides are applied to the component's RON
text (a missing struct field is appended), so resolving needs no type registry. An edit whose target is
gone from the base is skipped with a warning; nothing is copied from the base, so a base change reaches
the variant unless the variant overrides the same field. Refusals (reasons): a chain that reaches an id
again (`variant_loop`), more than 16 steps (`chain_too_long`), a missing base (`base_missing`), a result
that is not a valid level, such as an added entity parented to a removed one (`resolved_invalid`), a
file that does not parse or validate (`bad_variant`).

**Packages.** A package has no `.meta` index, so `regia package` writes a variant flattened under its
own name: the resolved level as a `SceneFile(…)` document, staged like any level, with its sidecar. The
loader reads a `.levelvariant.ron` that starts with `SceneFile` as the level it holds.

**Versioning** (`docs/specs/12-assets.md` § 4a): a new format of v0.6.0; the serde shape is hashed by
`cargo xtask check-formats` (entry `level-variant`); `level_variant_migration_sources()` is `[0]`.
`regia upgrade-project` re-saves a version-0 file at version 1. Fuzz target: `level-variant`.
