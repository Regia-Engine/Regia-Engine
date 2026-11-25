# Data Asset

Panel id: `data_asset`.

The **Data Asset** tab edits one `.data.ron` file: a single reflected value of a type your game
registers as a data asset (the game derives `Asset` and registers it with `register_data_asset` or
`#[reflect(Asset)]`). The panel draws the value with the same reflection widgets the
[Inspector](inspector.md) uses, so numbers, text, lists, maps and nested structs all edit in place.
Every edit is one step on the editor's undo stack. Depth: [Editor creators and the Data Asset
panel (agents)](../../agents/editor-creators.md).

## Open it

- Double-click a `.data.ron` in the [Content browser](content-browser.md).
- Create one with **New ▸ Data asset** in the Content browser and pick a type from its list. The new file is created in
  the folder shown and opens in the panel at once.
- An agent calls `editor.data_asset_open {"path": "assets/..."}`.

Any of these brings the tab forward, adding it when it was closed. The tab is not in the default
layout and the command palette has no `open` command for it; with nothing open it says to
double-click a `.data.ron` or use **New ▸ Data asset**.

## Edit a data asset

1. Open the file. The header shows its project path (with `*` when it has unsaved edits) and the
   type's name.
2. Change fields in the form. Typing in a field, dragging a number and adding or removing list
   elements each change the value; consecutive edits of one field by dragging merge into a single
   undo step.
3. Press **Save** to write the file (it is enabled only while there are unsaved edits).
4. Press **Revert** to reload the file and drop unsaved edits.

| Action | Control / key | Agent method |
|---|---|---|
| List the types New offers | **New ▸ Data asset** in the Content browser | `editor.data_asset_types` |
| Create a data asset | **New ▸ Data asset** and then a type from its list | `editor.create_asset` with `kind: "data_asset"` and `type` |
| Open | double-click the file in the Content browser | `editor.data_asset_open` |
| Edit a field | the form fields | `editor.data_asset_set` (`field` is a reflect path such as `weights[1]`) |
| Undo and redo | Ctrl+Z, Ctrl+Shift+Z | `editor.undo`, `editor.redo` |
| Save | **Save** | `editor.data_asset_save` |
| Revert | **Revert** | `editor.data_asset_open` with `discard: true` and the open path |

## Keyboard

The tab has no shortcuts of its own besides the shared Ctrl+Z and Ctrl+Shift+Z ([Editor
guide](index.md#keys-that-work-everywhere)). Fields, **Save** and **Revert** take focus with Tab.

## Known limits

- Only types registered as data assets can be created or opened; any other type is refused with
  `data_asset_unknown_type`, because the game's loader would refuse the file. The **New** list
  hides standard-library and math types.
- Opening a second file while the open one has unsaved edits is refused (`data_asset_unsaved`);
  save it, revert it, or pass `discard: true`.
- Fields the file leaves out keep the type's defaults.
- Only one data asset is open at a time.

See also: [Data tables](../data-tables.md), [Content browser](content-browser.md).
