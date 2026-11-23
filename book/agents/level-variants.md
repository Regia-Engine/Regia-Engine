# Level variants (agents)

A level variant is a level defined as another level plus differences: a `.levelvariant.ron` file names its base
(a `.level.ron`, a `.level.d` or another variant, by the id in its `.meta`) and stores only what differs: entities
it adds, base entities it removes (with their descendants), field overrides by entity guid, component and field
path (the prefab instance override record), components it adds or removes, record edits (an entity's name, parent
or prefab instance record) and resources it sets or drops. Design: ADR-7823. File format:
[`level-variant` v1](../../docs/formats/level-variant-v1.md).

What the variant's level is, in order:

1. the plain level at the bottom of the chain;
2. each variant step's differences, base-most first (a variant of a variant applies its base's differences first).

Nothing is copied from the base, so a change to the base shows up in every variant the next time it loads, unless
the variant overrides the same field. A chain that comes back to an id it already passed is refused
(`variant_loop`); so is one longer than 16 steps (`chain_too_long`).

## In the editor

- **File ▸ New level variant…** picks the base level, then the new `.levelvariant.ron` (default
  `<base>_variant.levelvariant.ron` next to the base), writes it with its `.meta` (and a `.meta` for the base when it
  had none, holding the id the base already loaded under) and opens it.
- **Open level** opens a variant like any level: you edit the resolved level.
- **Save level** writes only the differences from the base. Values are compared in the form a save writes them, so
  a base field spelled differently in its file is not a difference, and struct fields are compared one by one: a
  moved entity's override is its `Transform` `.translation`, and a later base change to its scale still flows in.
- **Marks**: the Outliner shows ◆ on an entity that differs from the base and + on one the variant added. The
  Inspector shows the entity's mark with **Revert entity to base**, and under each component the overridden fields
  with **Revert to base** per field. A revert is one undo step; the next save writes the variant without it.

All controls are menu items and buttons (keyboard reachable); marks use theme text styles only.

## Methods

These are editor methods (registered by `regia_editor::level_variant`); paths are project-relative (a leading
`assets/` is optional).

| Method | Permission | What it does |
|---|---|---|
| `editor.new_level_variant { base, path }` | edit scene | Creates the variant `path` (ending in `.levelvariant.ron`) of `base` with no differences and opens it. Answers `{ path, asset, level, base, chain }`. |
| `editor.level_variant_state` | read | The open variant: `path`, `asset`, `base`, `chain` (ids from the variant down to the plain level), the differences a save would write (`differences` total; `overrides`, `added_components`, `removed_components`, `record_edits` counts; `added_entities`, `removed_entities`, `resources`, `removed_resources` lists), `entities: [{ guid, name, mark, fields: [{ component, path }] }]` with `mark` one of `base`, `overridden`, `added`, and `unsaved`. `{ open: false }` when the open level is not a variant. |
| `editor.level_variant_revert { entity, component?, path? }` | edit scene | Revert to base: the whole entity (an added one is deleted, a removed one comes back), one component, or one field (`path`, e.g. `.translation`). `component` may be `#name` or `#parent` for the entity record. One undo step. Answers `{ reverted, differences }`. |

Opening and saving go through `editor.open_level` and `editor.save_level`; a variant saves its differences.

## Runtime and packages

`regia_framework::level_assets::load_level_source` accepts a `.levelvariant.ron`: it resolves the chain (bases found
by id in the project's `.meta` index) into a level in the `LevelLibrary` under the variant's id. `regia package`
writes each variant flattened under its own name (a package has no `.meta` index); the loader reads a
`.levelvariant.ron` that holds a `SceneFile(…)` as that level. A base edited on disk does not hot-reload the
variants built on it; reopen the variant.

## Error reasons

`data.reason` on a failed call:

| Reason | When |
|---|---|
| `no_project` | No project is open. |
| `bad_path` | A path is missing, leaves the project, or `path` does not end in `.levelvariant.ron`. |
| `base_missing` | The base file does not exist, or a level of the chain has no source with its id. |
| `not_a_level` | `base` is not a `.level.ron`, `.level.d` or `.levelvariant.ron`. |
| `variant_exists` | `path` already exists. |
| `no_variant_open` | The open level is not a level variant. |
| `entity_missing` | `entity` is not a guid. |
| `bad_field` | A `path` without its `component`, or a component type that is not registered. |
| `nothing_to_revert` | The entity, component or field does not differ from the base. |
| `variant_loop` | The chain comes back to an id it already passed (a variant naming itself included). |
| `chain_too_long` | The chain is longer than 16 steps. |
| `bad_variant` | The file does not parse or validate (nil base or target, an unknown record field, an added guid twice), or it holds a cooked level. |
| `newer_version` | The file was written by a newer engine. |
| `resolved_invalid` | The resolved level is not a valid level (for example an added entity parented to a removed one). |
| `io_failed` | A file could not be written. |

The loader (`LevelAssetError::Variant`) and `regia package` report the same reasons.
