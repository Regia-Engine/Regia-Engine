# Prefab overrides and Edit in place (agents)

An instance of a prefab can differ from its prefab: changed fields, added or removed components, removed prefab
entities and children the level added under it. The **Overrides window** lists every one of these for one instance
and applies or reverts them; **Edit in place** opens the instance's prefab for editing with the rest of the level
still drawn around it. Design: UI parity addendum § 9.1-9.2 (ADR-7790); prefab formats and variants:
[Prefab variants (agents)](prefab-variants.md); the isolated prefab edit mode: [Editor surface](editor.md).

## The Overrides window

Open it with Edit ▸ Prefab overrides… or the command palette (Prefab: Overrides…) for the selected entity, the
Outliner's override dot on an instance row, or Overrides… beside an override in the Inspector. It shows the instance the entity belongs to: the
level-authored instance root, with the overrides of every prefab nested inside it.

Each row has the entity, what differs, the prefab's value and the instance's value. Field rows have **Apply** and
**Revert**; **Apply all** and **Revert all** do every field row at once. Rows for added or removed components and
children are listed only: writing those into a prefab file has no path yet, and the row says so.

**Apply to** chooses where an Apply writes. Each field row has its own valid targets:

| Target | When it is offered | What Apply writes |
|---|---|---|
| `nested` | the entity belongs to a prefab nested inside the instance, the nested prefab is a `.prefab.ron` in the project, and the prefab that nests it does not override that field | the entity's own record in the nested prefab's file; the outer file is untouched; every live copy of the nested prefab that has no override of its own takes the value |
| `outer` | the instance's prefab is a `.prefab.ron` in the project | the outer prefab's file: the entity's record, or for an entity of a nested prefab an outer-wins override on the nested instance record |
| `variant` | the instance is of a variant with a `.variant.ron`, and the field is on the variant's own entities | the variant file's edit list; the base is untouched |
| `base` | the instance is of a variant whose base is a `.prefab.ron` in the project, and no variant step overrides the field | the base file; instances of the base and of every variant that leaves the field alone take the value |

The picker starts on "Each row's default": the target the Inspector's Apply picks by itself (`variant` for an instance
of a variant, else `outer`). A target a row cannot take leaves that row on its default. A prefab with no file in the
project gets the session-only Apply (the loaded template changes, no file is written).

Every Apply and Revert is one undo step, and so is a whole batch: Apply all writing two files is undone by one undo,
which puts both files' bytes, the overrides and the other instances' values back. A batch with a row that fails
reverts the rows it already did and reports the failure.

## Edit in place

Edit in place (Edit ▸ Edit prefab in place, the window's button, the command palette's Prefab: Edit in place, or
the method below) opens the prefab
of an instance in prefab edit mode without leaving the level:

- the rest of the level stays drawn, dimmed and read-only: it cannot be picked or selected and is not in the Outliner
  (the dim is an editor-only tint, nothing of it is saved; empty entities keep their proxy cubes, dimmed too);
- levels opened beside the host are dimmed and cannot be picked either; the close takes that off again (a lock you set
  on one of their entities stays);
- the prefab's contents are pickable in the viewport and listed in the Outliner as roots;
- a level save (File ▸ Save level, Save level as, `editor.save_level`) is refused until the prefab is closed: the level in the world carries the session's state (the hidden instance), and the undo history is the prefab's;
- the prefab's contents are plain editable entities placed at the instance's world transform; the instance itself is
  hidden while it is open;
- **Save** writes the `.prefab.ron` (only the prefab's own entities, with one root) and rebuilds every instance of it,
  the ones drawn around it included;
- **Close** drops the context and brings the level back as it was, with its undo history cleared as for the isolated
  mode.

A variant cannot be opened in place (variant edit mode is out of scope, spec 83 § 9): edit its base from an instance
of the base, or change the variant's overrides.

## Methods

| Method | Permission | What it does |
|---|---|---|
| `editor.prefab_overrides { entity }` | read | The window's rows for the instance `entity` belongs to: `{ root, prefab, rows }`, each row `{ kind, entity, name, component, field, prefab_value, instance_value, editable, targets: [{ target, label, file }], default_target }`. `kind` is `field`, `added_component`, `removed_component`, `added_child` or `removed_child`; values are RON text (structural rows describe the difference). Overrides a nesting prefab wrote on its nested record are template data, not rows. |
| `editor.prefab_overrides_apply { entity, rows?, target? }` | edit scene | Apply and Apply all: `rows` is `[{ entity, component, field?, target? }]` (the prefab entity whose field is overridden); omitted, every field row of the instance. `target` (`nested`, `outer`, `variant`, `base`) applies to every row that can take it; a row's own `target` wins. One undo step. Answers `{ applied, undo_depth, files }`. |
| `editor.prefab_overrides_revert { entity, rows? }` | edit scene | Revert and Revert all: the prefab's values come back on this instance. One undo step. Answers `{ reverted, undo_depth }`. |
| `editor.prefab_overrides_open { entity }` | read | Opens the window for a person watching (view only). Answers `{ open, root, rows }`. |
| `editor.prefab_apply_override { entity, component, field?, target? }` | edit scene | One field's Apply (see [Prefab variants](prefab-variants.md)); `target` as above, omitted for the default. |
| `editor.prefab_edit_in_place { entity }` | edit scene | Edit in place for the instance `entity` belongs to. Answers `{ editing, in_context: true }`. `editor.prefab_mode` answers `in_context: true` while it is open; `editor.save_prefab` and `editor.close_prefab` work as for the isolated mode. |

### Error reasons

Errors carry `data.reason`:

| Reason | Meaning |
|---|---|
| `not_found` | the entity does not exist (also a JSON-RPC not-found error) |
| `not_prefab_instance` | the entity is not a prefab instance or inside one |
| `override_absent` | the field has no override on this instance |
| `apply_target_unknown` | `target` is not `nested`, `outer`, `variant` or `base` |
| `apply_target_invalid` | the row cannot take that target (`editor.prefab_overrides` lists the valid ones) |
| `rows_empty` | the batch has no field rows |
| `prefab_apply_failed`, `prefab_template_missing`, `path_unknown`, `value_mismatch`, `feed_owned` | as for `editor.prefab_apply_override` |
| `prefab_variant_in_place` | Edit in place on an instance of a variant |
| `prefab_edit_failed` | Edit in place could not open the prefab (no project, no file, already editing, play running) |
| `prefab_in_place` | `editor.save_level` while a prefab is open in place: nothing is written; save or close the prefab, then save the level |
| `prefab_open` | `editor.save_level` (or an open level's own Save, `editor.levels_open { op: "save" }`) while a prefab is open on its own (`editor.open_prefab`): nothing is written; save or close the prefab, then save the level |

### Example

```json
{ "method": "editor.prefab_overrides", "params": { "entity": 4294967312 } }
{ "method": "editor.prefab_overrides_apply", "params": { "entity": 4294967312, "rows": [
  { "entity": 4294967315, "component": "Transform", "field": "translation", "target": "nested" },
  { "entity": 4294967313, "component": "Transform", "field": "translation", "target": "outer" } ] } }
{ "method": "editor.undo" }
```

The undo puts both files back.

## Keyboard and accessibility

Every control in the window is a focusable widget: Tab and Shift+Tab move between them, Enter or Space presses a
button, and the Apply to picker opens with Enter and moves with the arrow keys. Each row's Apply and Revert carry a
spoken name ("Apply Transform.translation"), and the Outliner's override dot is named "Overrides". The window scales
with the editor's UI scale and uses theme colors only.

## Known limits

- Added or removed components and children are listed, not applied or reverted from the window.
- The terrain of the context is drawn at full brightness: the terrain draw has no per-entity tint yet (ADR-8128). It
  cannot be picked.

## Logic in a prefab

A prefab carries its own logic graph through the `LogicGraphRef` component; in prefab edit mode the banner's
**Open graph** and **New graph for this prefab** (`editor.prefab_logic`) open it or create and attach one. See
[Prefab logic](prefab-logic.md).
