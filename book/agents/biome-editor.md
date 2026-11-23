# Biome editor (agents)

The **Biome** panel (Window ▸ Biome) edits a `.biome.ron` (spec 65 § 3; the format and
what each field does: [Biomes](biomes.md)) as typed forms: the biome's name, color, map key and priority; its
layers (rank, sharing, the candidate source with its density noise, channels and their rules, child depth);
each layer's entries (spawn, weight, the channels it takes, filters with a small curve of their density
window, footprint, placement, render settings and children); and the grass multipliers. Numbers drag,
switches toggle, enums are drop-downs, text fields write when you press Enter or leave them, and every list
has ▲ ▼ **Remove** and **Add**. Desktop editor only.

Create one with **Content ▸ New ▸ Biome** (or `editor.create_asset { kind: "biome", path }`): a file named
after itself with no layers, in the canonical form. The procedural **Area** tool picks its biome from the
project's `.biome.ron` files in a drop-down (empty: the tool preset's own `biome` parameter).

## How edits work

A field is named by a **path** into the definition's serde shape, the same paths the format's rule check
reports: `name`, `color`, `map_key`, `layers/<layer>/rank`, `layers/<layer>/source/Ground/spacing_m`,
`layers/<layer>/channels/<channel>/rule`, `layers/<layer>/entries/<entry>/filters/0/Slope/max_deg`,
`layers/<layer>/entries/<entry>/children/0/count`, `grass/<class>/density`. A list element is named by its
`name` (layers, channels, entries), its `class` (grass) or its index; `#<index>` is always the index (with
layers named `1` and `0`, `layers/1` is the layer named `1` and `layers/#1` the second layer; the form addresses
list elements this way); an enum's variant is a path step (`source/Ground/...`). Values are JSON: numbers, booleans, strings, tuples as arrays (`color: [0.2, 0.5, 0.1]`),
`null` for an unset option, an enum as `"Rest"` or `{ "Slope": { "max_deg": 30 } }` (fields left out take
the format's defaults, as in the file).

- An edit the definition's types refuse changes nothing (`biome_panel_bad_value`).
- An edit that would add a rule violation changes nothing and answers the rule's code: `biome_invalid`
  (a duplicate name, a negative weight, `upright` outside 0..1, a filter window with `min > max`, ...) or
  `biome_entry_unknown` (a child naming no entry of its layer). So once the opened file was valid, every state
  the panel holds is one the runtime loader (`BiomeDef::from_ron`) accepts. Issues are counted per code: in a
  file opened with issues, renaming or moving the element an issue sits in adds no issue and is allowed; an edit
  that adds an issue of some code is refused.
- Renaming an entry (`set .../entries/<entry>/name`) renames the children that name it; renaming a channel
  renames it in the layer's entries.
- **Undo** keeps 128 steps, one per edit; a drag (`merge: true` on the same path as the step before) is one
  step however many frames it lasts. In the form a drag ends when the pointer is released, so two drags on one
  field are two steps. Undoing every step returns the file's definition (not dirty). In the editor the steps
  are entries of the shared undo history ([shared history](undo.md)). Edit ▸ Undo and Ctrl+Z step them, and
  the Undo History panel lists them by file. `op: "undo"` / `"redo"` and the form's buttons step that history
  too, and `undo_depth` / `redo_depth` report its depths.
- **Unsaved edits are never dropped silently**: opening a file or starting a new biome while the open one is
  dirty is refused (`biome_panel_unsaved`, nothing changes) unless `discard: true`. In the form the first click
  shows the refusal and a **Discard edits and open ...** (or **... start ...**) button; that second click drops
  the edits, **Keep editing** keeps them. The panel has no autosave: save first.
- **Save** writes the canonical text (`BiomeDef::to_ron`, every field written) atomically. RON comments in the
  opened file are **not** kept: the panel counts them when the file opens and shows a warning line
  ("Saving rewrites the file in the canonical form: its N RON comments will be lost."); `comments` and
  `warning` say the same to an agent. A file that breaks a rule opens (its issues are listed) but saves only
  once they are fixed.
- A file that holds a NaN or infinite number cannot be edited through the form (`biome_panel_non_finite`):
  JSON has no such numbers. Edit its text.

## Methods

| Method | What it does |
|---|---|
| `editor.biome_panel_open { path, new?, discard? }` | Open a project-relative `.biome.ron` (a file with rule issues opens too), or with `new: true` start a biome named after the file that save writes to `path`. Refused with `biome_panel_unsaved` while the open biome has unsaved edits, unless `discard: true`. Returns the state. |
| `editor.biome_panel_get` | The state: `{ path, dirty, generation, undo_depth, redo_depth, comments, canonical, warning, def, issues: [{ code, path, message }], error }`; `def` is the definition as JSON (the same shape the paths walk). |
| `editor.biome_panel_edit { op, path?, value?, index?, to?, merge? }` | `set { path, value, merge? }` writes a field or a whole section; `add { path, value, index? }` inserts into the list at `path` (at the end or at `index`); `remove { path }` removes the list element `path` names; `move { path, to }` moves it to index `to`; `undo`; `redo`. Returns the state. |
| `editor.biome_panel_save` | Write the definition to its file, canonical and atomic. Returns `{ written, bytes }`. |

Example: a reed layer that grows within 4 m of water.

```json
{ "method": "editor.biome_panel_edit", "params": { "op": "add", "path": "layers", "value": { "name": "reeds" } } }
{ "method": "editor.biome_panel_edit", "params": { "op": "add", "path": "layers/reeds/entries",
  "value": { "name": "reed", "spawn": { "Mesh": "meshes/reed.glb" } } } }
{ "method": "editor.biome_panel_edit", "params": { "op": "set", "path": "layers/reeds/entries/reed/filters",
  "value": [{ "WaterDistance": { "max_m": 4.0, "feather": 1.0 } }] } }
{ "method": "editor.biome_panel_save", "params": {} }
```

## Error codes

Errors are `invalid_params` (`not_found` for `biome_panel_file_not_found`) with a lowercase `data.reason`:

| Code | When |
|---|---|
| `biome_panel_no_project` | No project is open (open, save). |
| `biome_panel_unsaved` | Open or new while the open biome has unsaved edits and `discard` is not `true`; nothing changes. |
| `biome_panel_bad_path` | The path is not a relative `.biome.ron` inside the project. |
| `biome_panel_file_not_found` | The file does not exist or cannot be read. |
| `biome_panel_no_biome` | An edit or save with no biome open. |
| `biome_panel_bad_op` | `op` is not `set`, `add`, `remove`, `move`, `undo` or `redo`. |
| `biome_panel_bad_field` | The path names no field or element. |
| `biome_panel_bad_value` | The value does not fit the field's type, or `index` / `to` is out of range. |
| `biome_panel_read_only` | `format_version` is written by the editor. |
| `biome_panel_not_a_list` | `add`, `remove` or `move` on a field that is not a list. |
| `biome_panel_non_finite` | The definition holds a NaN or infinite number. |
| `biome_panel_nothing_to_undo`, `biome_panel_nothing_to_redo` | The stack is empty. |
| `biome_panel_io_failed` | The save could not write the file. |
| `biome_invalid`, `biome_entry_unknown` | The edit would break a format rule; the file's own code on open (not RON, newer than this build) and on a save with issues. |

## Verify

`cargo test -p regia_editor --lib biome_panel` (ops write the bytes `to_ron` gives the expected definition;
a 50-op random sequence keeps every state loadable and a full undo restores the original bytes; renames,
refusals, the comment warning) and `cargo test -p regia_editor --test panels_over_agent biome` (the methods
through the real dispatcher, from `editor.create_asset { kind: "biome" }` to a saved file the loader reads).
