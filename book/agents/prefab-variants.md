# Prefab variants (agents)

A prefab variant is a prefab defined as another prefab plus edits: a `.variant.ron` file names its base (a
`.prefab.ron` or another variant) and stores only what differs, as the same edit lists an instance has (field
overrides, added and removed components, added and removed children). Design: `docs/specs/83-prefab-profiler-volumes.md`
§ 2 (ADR-2323). File format: `prefab-variant` v1, below.

What an instance of a variant gets, in order:

1. the base prefab's entities (the bottom of the chain);
2. each variant step's edits, base-most first (a variant of a variant applies its base's edits first);
3. the instance's own overrides.

So the instance wins over the variant, and the variant over its base. The base's local ids stay valid: an override
written for the base also works on an instance of any variant. A change to the base shows up in every variant the
next time its instances are built (the editor rebuilds them when the base file changes). A variant chain longer than
16 steps, or one that loops, is refused.

## Methods

These are editor methods (registered by `regia_editor::prefab_variant_edit`); paths are project-relative (a leading
`assets/` is optional).

| Method | Permission | What it does |
|---|---|---|
| `editor.create_prefab_variant { base, path, from_entity? }` | edit scene | Writes a new `.variant.ron` (and its `.meta`) on `base`. With `from_entity` (an instance root of the base), the variant keeps that instance's overrides and removed children: "save this instance as a variant". |
| `editor.prefab_variant { path }` | read | The variant's id, base, `chain` (ids from the variant down to the base prefab), its overrides and the counts of its other edits. |
| `editor.set_variant_override { path, target, component, field?, value? }` | edit scene | Sets one override (`field` defaults to `"."`, the whole component; `value` is RON text, a JSON number or bool is used as is) or, with `value` null or left out, clears it. Writes the file and rebuilds every live instance of the variant and of the variants built on it. |
| `editor.prefab_apply_override { entity, component, field?, target? }` | edit scene | The inspector's Apply on one override of an instance: writes the value into the instance's `.prefab.ron`, or into its `.variant.ron` for an instance of a variant (below). Answers `{ edited, undo_depth, file }` (`file`: project-relative, `null` for the session-only Apply). `target` (`nested`, `outer`, `variant`, `base`) writes into an explicit target instead of the default ([Prefab overrides](prefabs.md), ADR-7790); errors `apply_target_unknown`, `apply_target_invalid`. |

In the editor (ADR-7614): the Outliner's row menu on a prefab instance has *Create variant…* (a dialog asks for the
file; the variant keeps the instance's overrides, as `from_entity` does) and the Content Browser's menu on a
`.prefab.ron` or `.variant.ron` has *Create variant…* (an empty variant of it). Both run
`editor.create_prefab_variant`'s code and write the same file; creating a variant is one undo step (the undo removes the
new file and its `.meta`, a redo writes them back with the same id), from the UI and from the agent method alike. The
step only touches files: it does not mark a saved level unsaved. For an
entity of an instance of a variant, the Inspector lists under each component the fields the variant overrides, each with
a marker and *Clear override*, which runs `editor.set_variant_override` with `value: null` (the same file, one undo
step).

`editor.spawn_prefab { path }` spawns a variant like a prefab; the base chain is loaded from the project by id
when it is not loaded yet. Instances of a variant record the variant's id in `PrefabInstance::prefab`.

Prefab edit mode opens prefabs, not variants: open the variant's base, or change the variant with
`editor.set_variant_override`.

A game without the editor registers variants itself, as it does templates: parse the file with
`PrefabVariantFile::from_ron_str`, build the step with `PrefabVariant::from_file(asset, &file)` and call
`PrefabLibrary::insert_variant`. A level instance of a variant waits as a `PendingPrefab` until every step of its
chain is registered.

Results:

- `editor.create_prefab_variant`: `{ path, asset, overrides }`.
- `editor.prefab_variant`: `{ asset, base, format_version, chain, overrides: [{ target, component, path, value }],
  added_components, removed_components, added_children, removed_children }`. The four edit fields are counts
  (the length of that list in the file), not the lists. The guid list of removed children stays in the
  `.variant.ron` file.
- `editor.set_variant_override`: `{ path, overrides, instances_updated }`. `instances_updated` counts the level-authored instance roots rebuilt, once each, even when a root holds instances of both a base and its variant.

## Error reasons (`data.reason`)

| Reason | Code | When |
|---|---|---|
| `no_project` | F0003 | No project is open. |
| `bad_path` | F0002 | A path is missing, leaves the project (`..`, absolute), or does not end in `.variant.ron` where a variant is needed. |
| `base_missing` | F0003 | The base file does not exist. |
| `not_a_prefab` | F0002 | The base is not a `.prefab.ron` / `.variant.ron`, or the prefab does not parse or bake. |
| `variant_exists` | F0002 | The new variant's file already exists. |
| `variant_missing` | F0003 | The variant file cannot be read. |
| `bad_variant` | F0002 | The variant file does not parse, is newer than the engine, or names itself as its base. |
| `entity_missing` | F0003 | `from_entity` is not a live entity. |
| `not_an_instance` | F0002 | `from_entity` is not a prefab instance root. |
| `instance_of_other_prefab` | F0002 | `from_entity` instantiates another prefab than `base`. |
| `bad_value` | F0002 | `target` or `component` missing or nil, or `value` is not RON. |
| `chain_broken` | F0003 | A base of the chain is neither loaded nor in the project, the chain loops, or it is longer than 16. |
| `io_failed` | F0001 | A file or `.meta` could not be written. |
| `instance_update_failed` | F0002 | `editor.set_variant_override`: the live instances cannot spawn with the edit (a template that nests itself); the file is put back and the instances keep the old variant. |

## The file (`prefab-variant` v1)

```ron
PrefabVariant(
    format_version: 1,
    base: (
        prefab: "asset:8b6f2a11-0000-4000-8000-000000000001",
        overrides: [(target: "8b6f2a11-0000-4000-8000-000000000002", component: "regia_scene::components::Tags", path: ".", value: (["red"]))],
        removed_children: [],
    ),
)
```

A file without `format_version` loads as v1; a newer one is refused before its body is read. The `.meta` kind is
`prefab_variant_ron`.

## Example: a red lamp

```json
{"method":"editor.create_prefab_variant","params":{"base":"assets/props/lamp.prefab.ron","path":"assets/props/red_lamp.variant.ron"}}
{"method":"editor.set_variant_override","params":{"path":"props/red_lamp.variant.ron","target":"8b6f2a11-0000-4000-8000-000000000002","component":"my_game::Lamp","field":".color","value":"(1.0, 0.1, 0.1)"}}
{"method":"editor.spawn_prefab","params":{"path":"assets/props/red_lamp.variant.ron","translation":[2,0,0]}}
```

An `editor.set_variant_override` change records one scene-history entry. `editor.undo`
and `editor.redo` restore the exact variant file bytes, re-register the chain and refresh
its live instances and dependent variants. Unchanged file bytes add no entry. As with
other file-backed editor history, a later filesystem failure is logged by the command.

## Break link and Apply to the prefab file

Two instance edits work on plain prefabs too (spec 14 § A3, ADR-7203):

- `editor.prefab_break_link { entity }`: the instance (pass its root or any entity inside it) stops being an
  instance. The root's `PrefabInstance` and every prefab-internal entity's `PrefabLink` go; the entities stay
  with their current values, overrides included, and the next save writes them as plain entities. A scene
  entity under the instance (an added child) keeps whatever it is. Answers `{ root, unlinked, undo_depth }`;
  one undo step puts the link back. `not_prefab_instance` (`NOT_FOUND`, `data.reason`) when the entity is
  neither an instance nor inside one.
- Apply override (the inspector's Apply button on an override row) writes the value into the prefab's
  `.prefab.ron` when the open project has the file: atomically, in the current format, record names kept.
  An entity that comes from a prefab nested in this one gets an override on the nested instance record
  of this file (outer wins); the nested prefab's own file is not touched. The applying instance's override
  goes; every other live instance takes the new value unless it overrides that field itself. One undo
  restores the file bytes, the override and the values. A prefab registered only in code (no file) keeps
  the older session-only Apply.
- Apply on an instance of a variant (ADR-7263) moves the instance's override into the variant's own
  `.variant.ron` (its `base.overrides`, replacing an override of the same field), atomically and in the
  current format; the base `.prefab.ron` is not touched. Every other instance of the variant, or of a
  variant built on it, takes the value unless it (or a variant step above) overrides that field; instances
  of the base do not change. One undo restores the variant bytes, the override and the values. An override
  on a prefab nested inside the variant instance is refused (`prefab_apply_failed`).

`editor.prefab_apply_override` answers errors with `data.reason`: `not_prefab_instance`, `override_absent`
(`NOT_FOUND`, `F0003`: the entity is not in an instance, or that field has no override), and
`prefab_template_missing`, `path_unknown`, `value_mismatch`, `feed_owned`, `prefab_apply_failed`
(`F0002`), and with `target`: `apply_target_unknown` (not one of the four names) and `apply_target_invalid` (the
override cannot go there; `editor.prefab_overrides` lists each override's valid targets).

```json
{ "method": "editor.prefab_apply_override", "params": { "entity": 4294967312, "component": "Transform", "field": "translation" } }
→ { "edited": [4294967312], "undo_depth": 3, "file": "assets/props/crate_tall.variant.ron" }
```

The asset watcher sees the editor's own write and rebuilds nothing; an outside edit of the file still
rebuilds every instance, and each keeps its overrides.

The Create variant dialog is drawn by the shell (ADR-7744), so it shows with the Content Browser and the Outliner both
closed; the palette command `prefab.create_variant` opens it for the selected prefab instance. `prefab_variant_edit::dialog_open`
reports whether it is open. The Inspector marks an overridden field of a prefab instance beside its label (a filled
dot; a hollow one marks a field with an override inside it) and its right-click menu has **Revert override** and **Apply
to prefab** (`editor.prefab_override`). Every override without such a marker keeps its row with Revert and Apply under the
component (ADR-7794): an override of the whole component, of a list element or vector lane (its field shows only the
hollow dot), and any override of a component an extension or a Lua declaration draws.
