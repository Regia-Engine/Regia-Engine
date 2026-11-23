# Undo and redo in the editor

Every edit the editor makes, from a panel or an agent, is one entry on one shared undo stack. These
calls let an agent make the structural edits the outliner and inspector make, read the stack, and
step it. Each call below is one undo entry: an agent edit never merges into the edit before or
after it, unless it is inside an explicit transaction. See [shared history and recovery](undo.md)
for the history panel, touched details, jump-to navigation and transaction methods.

| Method | What it does |
|---|---|
| `editor.history {}` | `{ undo, redo, undo_depth, redo_depth, dirty, cap, remapped }`: `undo` and `redo` list what each step would do, next first ("Transform gizmo", "Set PointLight.intensity", "Delete 2 entities"); `dirty` is whether the level differs from its last save; `cap` is the history limit (500); `remapped` counts entity ids that an undo or redo changed. Read only. |
| `editor.spawn { name?, translation?, rotation?, scale?, parent? }` | Creates an entity in the open level (default name "New entity", at the origin, a root). With `parent`, the create and the parenting are one entry. Returns `{ entity, guid, name, parent, undoable }`. |
| `editor.delete { entities? }` | Deletes entities with their subtrees as one entry (`entities` absent = the selection). Undo brings them back with the same guids, components, level, parent and place among their siblings. Deleted entities leave the selection. An empty list is `F0002` with `data.hierarchy_error` `hierarchy_no_entities`. |
| `editor.reparent { entity, parent? }` | Moves an entity under `parent`, or to the root when `parent` is absent or `null`, as one entry. It is the outliner's move ([hierarchy drag and drop](editor-hierarchy.md)): `entities` moves several at once and the world pose is kept unless `keep_world: false`. A cycle or the current parent is an `invalid_params` error (`data.reparent_error`), with no entry. |
| `editor.duplicate { entity }` | Copies the subtree with fresh guids, 1 m along +x, under the same parent. Returns the copy's root as `entity`. `entities` (or the selection) and `offset` duplicate several at once ([Outliner hierarchy edits](hierarchy.md)); the copies become the selection. |
| `editor.add_component { entity, component, value? }` | Adds a reflected component: its default, or `value` in the JSON form `scene.get_entity` returns, with every field. If the entity already has it, the call is an error: use `editor.set_property`. |
| `editor.remove_component { entity, component }` | Removes a component; undo restores its value. |
| `editor.undo {}` / `editor.redo {}` | Step the stack: `{ undone \| redone, remaining, description, dirty }`. `description` is what was undone or redone. |

`editor.set_transform` and `editor.set_property` (on the [editor surface](editor.md) page) are
one entry per call too.

## Ids across undo

Undoing a delete, or redoing a create, spawns the entities again: same guids, new entity ids.
The editor maps the old ids to the new ones, so the ids an agent already holds keep working in
every `editor.*` call that takes an entity, and the undo entries recorded before the delete keep
reaching the entity. Results always report the current id. Hosted scene-edit calls also follow
this map in the edit world. Scene read calls and runtime/play worlds use current ids: after an
undo, look the entity up again with `scene.query_entities` before making those calls.

## Dirty and saved

`dirty` compares the current state with the saved one, not the number of entries. Undoing back
to the last save makes the level clean again. Redoing past it, or editing after undoing below it,
makes it dirty. A level restored from an autosave stays dirty until it is saved.

## Errors

| Code | When |
|---|---|
| `F0002` (`invalid_params`) | A missing or malformed parameter; an empty `entities`; a reparent cycle; `editor.add_component` on an entity that already has the component, or a `value` missing a field. A `dry_run` or `keep_world` on `editor.reparent` that is not a JSON boolean is not this error: `dry_run` counts as false and `keep_world` counts as true. |
| `F0003` (`not_found`) | The entity does not exist (an undo may have removed it), the component type is not registered, or the entity lacks the component to remove. |
| `F0001` (`internal`) | The spawn or insert did not take effect (see `logs.read`). |

## Example: build, check, undo everything

```console
$ BASE=$(regia agent call editor.spawn '{"name":"Base"}' | jq .entity)
$ ROCK=$(regia agent call editor.spawn "{\"name\":\"Rock\",\"parent\":$BASE,\"translation\":[1,0,0]}" | jq .entity)
$ regia agent call editor.add_component "{\"entity\":$ROCK,\"component\":\"PointLight\"}"
$ regia agent call editor.delete "{\"entities\":[$BASE]}"
$ regia agent call editor.history
$ regia agent call editor.undo        # Base and Rock are back, under the same ids
$ regia agent call editor.set_transform "{\"entity\":$ROCK,\"translation\":[2,0,0]}"
```

## In the editor

Ctrl+Z undoes, and Ctrl+Shift+Z or Ctrl+Y redoes (Cmd on macOS). These shortcuts do nothing while
a text field has focus, since the field has its own undo, or while the game is playing. The Edit
menu shows the next step's description. A gizmo drag, a slider scrub or a node drag is one entry.
The entry ends when the mouse button is released, so two drags are two entries.

## Asset panels on the shared stack

The asset panels' edits are entries on the same stack (P-322, ADR-8365): curve, chooser and data
tables, the car, trailer and aircraft in Vehicle Tuning, the Theme panel's draft, the UI Animation
editor's library and the UI designer's documents. `editor.undo` and Ctrl+Z step them like scene
edits. Each panel's own Undo button or method steps the same stack, but only when the next step is
an edit of the document that panel has open (otherwise it answers its nothing-to-undo error, as
before; the editor's Undo steps everything):
`editor.theme_panel_set { history: "undo" }`, `editor.ui_anim_edit { op: "undo" }`,
`editor.ui_designer_edit { op: "undo" }` and `editor.vehicle_tuning_undo { asset: "trailer" }`
(or `"aircraft"`); each answers with the state after the step, and while a transaction is open
they are refused with `transaction_active`, like `editor.undo`. The car's
`editor.vehicle_tuning_undo` and `editor.tables_panel_undo` step the shared stack whatever its next
step is. A discrete edit is one entry;
a drag is one entry however many frames it runs (the UI designer's drag of a field on several
selected nodes included). The panels' `undo` / `undo_depth` counts are the shared stack's.

A panel edit joins the stack at the next request boundary (before the next agent request, and at
the start of every frame), so the next `editor.history` lists it. An entry for a theme draft, UI
animation library or UI document that is no longer open changes nothing when stepped (the entry
stays); a table, car, trailer or aircraft entry opens its file again as it was left.

