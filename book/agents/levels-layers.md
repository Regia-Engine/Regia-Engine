# Levels: layer locks, layer files and several open levels

Three editor features of spec 37 § 12 (ADR-7792) for levels several people work on at once:

- **Layer locks**: a locked data layer's members cannot be picked or box-selected in the viewport.
  The Outliner still selects them. A lock is per-user view state, like an Outliner entity lock:
  it is kept per level in `.regia/editor_view.ron` (`locked_layers`), never in the level, and is
  not an undo step.
- **Layer files**: a data layer can be stored in its own file next to the level,
  `<level>.<layer>.layer.ron` (format `layer-file` v1, `docs/formats/layer-file.md`), so two people
  can edit different layers without touching the same file. The level does not list its layer
  files: the file name is the reference. Every reader of a level (the runtime, the cook,
  `regia package`, the bake tools, the editor) folds the layer files back in, so a level with layer
  files loads exactly like the merged level.
- **Several open levels**: the host level (File ▸ Open) plus any number of levels opened beside it,
  for example the levels the host places, and the host's layer files. Each is a *document* with its
  own unsaved flag and its own Save; closing a level asks only about its own changes. The
  **active level** decides where new entities go.

Methods: `editor.layer_lock`, `editor.layer_file`, `editor.levels_open`, `editor.active_level`.

In the editor these are in the Layers panel: each layer row has a **Lock** toggle (a lock glyph marks a
locked row) and **Store in its own file** / **Merge back into the level**; the **Open levels** section
lists every document with a `*` when it has unsaved changes, an **Active** toggle, **Save**, **Close** and
a path field with **Open beside** (Enter in the field opens too). Every control is reachable with Tab and
runs with Space or Enter.

## Task 1: lock a layer

```json
{ "method": "editor.layer_lock", "params": { "layer": "night_props", "on": true } }
```

answers `{ layer, locked, changed, host, active, documents, locked_layers }`. Members of the layer leave
the selection and stop being pickable in the viewport. `on: false` unlocks.

## Task 2: store a layer in its own file and merge it back

```json
{ "method": "editor.layer_file", "params": { "layer": "night_props", "separate": true } }
```

writes `levels/forest.night_props.layer.ron` with the layer's members and rewrites `levels/forest.level.ron`
without them. It answers `{ layer, separate, written, deleted, separated, documents, ... }`; the layer file is
now its own document (`kind: "layer"`). It works on the saved files: unsaved edits stay in the editor and the
next save puts each one in its file.

A record moves to the file of the first layer (by name) that it belongs to and that is stored in its own file.
Records in a member's subtree that have no layer membership of their own stay in the level.

```json
{ "method": "editor.layer_file", "params": { "layer": "night_props", "separate": false } }
```

merges the file back and deletes it. Merging back an unchanged split gives the level's original bytes: each
record in a layer file remembers its position in the whole level.

Only the single-file form (`.level.ron`) is split; a level directory (`.level.d/`) already stores one file per
entity (`layer_file_needs_file_form`).

## Task 3: open levels beside the host, edit one, save only it

```json
{ "method": "editor.levels_open", "params": { "op": "open", "path": "assets/levels/town_a.level.ron" } }
{ "method": "editor.levels_open", "params": { "op": "open", "path": "assets/levels/town_b.level.ron" } }
{ "method": "editor.levels_open", "params": {} }
```

The list answers `{ host, active, documents: [{ id, level, kind, layer, path, file, dirty, active }],
locked_layers }`. `kind` is `host`, `level` or `layer`. An edit of an entity of `town_a` marks only `town_a`
dirty (undo history entries carry the documents they changed, so undoing back to a save makes that document
clean again). Save one document by its id:

```json
{ "method": "editor.levels_open", "params": { "op": "save", "document": "<id from the list>" } }
```

`written` lists the files written: only that document's file, and none when its bytes were already the same.
File ▸ Save still saves the host and its layer files (each file only when its bytes changed).

Close a level opened beside the host:

```json
{ "method": "editor.levels_open", "params": { "op": "close", "level": "assets/levels/town_b.level.ron" } }
```

Unsaved edits of that level refuse the close (`level_dirty`) unless `discard: true`; other levels' edits never do.
The host level closes with File ▸ Open or New (`host_level`).

## Task 4: choose where new entities go

```json
{ "method": "editor.active_level", "params": { "set": "assets/levels/town_a.level.ron" } }
```

`set` takes `host`, a level id or the path of an open level. Entities the editor's create command makes (the Create menu,
`editor.create_entity`), a paste, a Content Browser drop (a prefab, a mesh, a sound) and a prefab instance made by
Create prefab from entities that belong to no level join the active level; without `set` the method answers the current choice. Closing
the active level makes the host active again.

## For the Outliner

While more than one document is open the Outliner lists each one's roots under a heading row (the file name, ` *`
when unsaved, `(active)` in the signal color for the active level; then "Not in an open level"). The heading is a
focusable button: Tab reaches it and Enter or Space opens or closes the group. With a root row selected, Left (when
the row has nothing open to close) closes its level's group, and Right opens it again. The "Group by category" view
keeps its own sections. Agents read and toggle the same rows with `editor.outliner_rows` (`open_levels`,
`close_levels`; [Editor panels at scale](editor-panel-timings.md)). While a prefab is open the Outliner keeps its plain
tree.

`regia_editor::layers_panel::group_by_level` groups the Outliner's tree by open document (one heading per level
and layer file: its file name, ` *` when unsaved, `(active)`; then "Not in an open level"), in the shape of the
Outliner's group-by-category view, so the Outliner draws it with the same code. `level_group_rows` answers the rows
alone and `layer_lock_glyph` the lock glyph for a member's row.

## Reasons

| Code | When |
|---|---|
| `layer_unknown` | The layer is not a layer of the open level. |
| `layer_not_separated` | `separate: false` for a layer that is not stored in its own file. |
| `layer_file_needs_file_form` | The host level is a `.level.d` directory. |
| `level_untitled` | The host level was never saved (save it before moving a layer, or Save As before a document save). |
| `no_host_level` | No level is open. |
| `level_already_open` | The level is the host or is already open beside it. |
| `level_not_open` | The level named by `close` or `set` is not open. |
| `host_level` | `close` named the host level. |
| `level_dirty` | `close` of a level with unsaved changes, without `discard`. |
| `document_unknown` | `save` named a document id that is not open. |
| `prefab_in_place`, `prefab_open` | `save` while a prefab is open in place or on its own: nothing is written; close the prefab first. |
| `level_not_found`, `level_parse_error`, `level_invalid`, `unsupported_version`, `duplicate_entity`, `level_io_error` | The level or a layer file could not be read or written (the level readers' reasons; `duplicate_entity` when a guid is in the level and a layer file). |

## Known limits

- A level opened beside the host opens at its own coordinates (it is not moved to where the host places it).
- A redo of a create after the active level changed puts the entity in the new active level.
- Content Browser drops still place into the host level.
