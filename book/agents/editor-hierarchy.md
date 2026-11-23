# Hierarchy drag and drop (`editor.reparent`)

In the Outliner, drag a row onto another row to parent it there, or onto the empty space under
the tree (it reads **Move to root** during a drag) to unparent it. The rest of the panel (search,
range select, hotkeys, group/duplicate/delete) is in [Outliner hierarchy edits](hierarchy.md).

- **What moves.** Drag a selected row and the whole selection moves with it. Drag an unselected
  row and only that entity moves. A selected entity whose parent is also selected stays under
  its parent: the parent moves and the child rides along.
- **Where it ends up.** By default each entity keeps its place in the world: it gets the local
  transform that puts it where it was under the new parent. Hold **Alt** while dropping to keep
  the local transform instead, so the entity moves with its new parent. A parent that combines
  non-uniform scale with a rotation can shear its children; that shear cannot be stored in a
  translation-rotation-scale transform, so it is dropped.
- **Refused drops.** A row inside the dragged subtree (the drop would put an entity under itself)
  is outlined in the error color and the drop does nothing. A drop onto the current parent
  changes nothing and pushes nothing.
- **Undo.** The whole drop is one undo entry: one Ctrl+Z (or `editor.undo`) restores every parent
  and every local transform.

## `editor.reparent`

Mutating, permission `edit_scene`. The same move as a drop. This is the only `editor.reparent`:
the undo surface (`undo-redo.md`) and the Outliner surface (`hierarchy.md`) both describe this
method (ADR-1354).

| Param | Type | Meaning |
|---|---|---|
| `entities` | array | Entity ids (from `editor.hierarchy` or `scene.query_entities`). |
| `entity` | number | One entity id (the same as `entities: [id]`). |
| `parent` | number or null | The new parent; null or absent moves to the root. |
| `keep_world` | bool | Keep each entity's world pose (default true); false keeps the local transforms. A value that is not a JSON boolean counts as true. |
| `dry_run` | bool | Validate and report without applying (default false). A value that is not a JSON boolean counts as false, so the move is applied, and is not `F0002`. |

With neither `entities` nor `entity`, the editor selection moves. `entities: []` (or an empty
selection) is refused. Ids from before an undo follow the re-spawned entity.

```json
{ "method": "editor.reparent", "params": { "entities": [4294967301], "parent": 4294967298 } }
```

Returns `{ moved, unchanged, carried, pose_not_kept, parent, applied, keep_world, entities,
undoable }`:

- `moved` lists the ids of the top-most entities that change parent (ADR-1354 made it a list; it
  was a count).
- `unchanged`: given, but already under `parent`. `carried`: given, but an ancestor of theirs is
  also given, so they ride along. `pose_not_kept`: moved under a parent whose world matrix is
  singular (a zero scale), so they keep their local transform.
- `applied` is false for a `dry_run`.
- `entities[]` has one entry per id passed in: `{ entity, parent, translation, rotation, scale,
  world_translation }`. `translation`, `rotation` and `scale` are the local transform after the
  move; `world_translation` is composed from the parent chain.
- `undoable` is the undo depth after the call.

`scene.reparent` stays the runtime's plain reparent: it is not undoable, and it keeps the local
transform.

## Error codes (`data.reparent_error`, `data.hierarchy_error`)

`data.code` is the catalog code: `F0002` for a refused move, `F0003` for a missing entity.

- `reparent_cycle` / `hierarchy_cycle`: the parent is one of the moved entities or inside one of
  their subtrees.
- `reparent_nothing_to_move`: every entity is already under that parent (no `hierarchy_error`).
- `reparent_target_gone` / `hierarchy_parent_missing`: the parent does not exist.
- `reparent_entity_gone` / `hierarchy_entity_missing`: one of the entities does not exist.
- `hierarchy_no_entities` (no `reparent_error`): `entities` is empty, or absent with an empty
  selection.

A refused call changes nothing and pushes nothing on the undo stack.

## How to check a result

`world_translation` should be unchanged when `keep_world` is true. `editor.undo` should then return
`undone: true`, and `scene.get_entity` should show the old parent again.
