# Outliner hierarchy edits

The editor's Outliner shows the level as a tree. A human drags rows to reparent them and uses the context menu
or hotkeys to group, duplicate and delete; an agent makes the same edits with the methods below. Both paths call
the same code (`regia_editor::hierarchy_ops`), so each edit is **one entry** on the editor's shared undo stack,
whether it moves one entity or fifty. `editor.undo` / `editor.redo` step it.

`editor.reparent`, `editor.duplicate`, `editor.delete` and `editor.group` fall back to the editor selection
when `entities` is absent (`[]` is an error, not the selection). `editor.select_hierarchy` falls back the same
way when `entities` is absent, but an empty array is not an error: `mode` `"set"` clears the selection and
`"add"` leaves it. `editor.reparent`, `editor.duplicate` and `editor.delete` are one method each,
shared with the undo surface ([Undo and redo](undo-redo.md)) and the drag-and-drop page
([Hierarchy drag and drop](editor-hierarchy.md)); they also take `entity` (one id) and accept ids from before
an undo (ADR-1354). Ids come from `editor.hierarchy` or `scene.query_entities`. Editor-internal entities
(window markers, bookkeeping) are never listed and cannot be edited.

## `editor.hierarchy`

The tree as the Outliner draws it: roots and children sorted by name.

Params: `root` (only this entity's subtree), `max_depth` (levels below the roots; `0` = roots only), `filter`
(keep entities whose name contains this text, case-insensitive, and the ancestors that lead to them, like the
panel's search box).

Returns `roots[]` of `{ entity, name, guid, parent, selected, child_count, children[] }` and `count`.
`count` is the number of nodes in the filtered tree, including descendants that `max_depth` leaves out of
`children`. `max_depth: 0` returns the roots with `children: []` and a `count` that still includes every
descendant. `child_count` is the full number of children even when `max_depth` cuts `children` off. Each node also
carries the row's `kind` (or null), derived `role`, `unclassified` flag and grouping `category`
([Entity kinds](entity-kinds.md), ADR-2488).

```json
{ "method": "editor.hierarchy", "params": { "max_depth": 1 } }
```

## `editor.reparent`

The Outliner's drag and drop. Moves `entities` under `parent` (`null` or absent = the root).

Params: `entities` (or `entity`), `parent`, `keep_world` (default `true`: each entity gets the local transform
that keeps its world pose, so nothing moves on screen; `false` keeps the local transforms, so entities move with
their new parent), `dry_run` (a JSON boolean: validate and report, change nothing). A `dry_run` or `keep_world`
that is present but not a JSON boolean is not `F0002`. `dry_run` then counts as false, so the move is applied,
and `keep_world` counts as true.

Returns `moved` (ids that change parent), `unchanged` (already under `parent`), `carried` (given, but an ancestor
of theirs is also given: they travel inside it and keep their place in its subtree), `pose_not_kept` (the parent's
world matrix is singular, for example a zero scale: those keep their local transform), `parent`, `applied`,
`undoable` (undo depth after the call), and `keep_world` and `entities[]` (each given entity's parent and local
and world translation after the call; see [Hierarchy drag and drop](editor-hierarchy.md)).

The call is all or nothing: if any entity would land in its own subtree, nothing moves. A call where every entity
is already under `parent` is refused with `data.reparent_error` `reparent_nothing_to_move` and pushes nothing.

```json
{ "method": "editor.reparent", "params": { "entities": [4294967301, 4294967302], "parent": 4294967300 } }
```

Check it worked: `editor.hierarchy { "root": <parent> }` lists the entities as children, and a capture or
`scene.get_entity` shows the same world positions as before.

## `editor.group`

Creates an empty entity named `name` (default `"Group"`) at the centroid of the entities' world positions,
under their common parent (the root when their parents differ), and moves them under it keeping their world
poses. One undo entry; the group becomes the selection.

Returns `{ group, members, parent, undoable }`. `members` are the top-level entities given (descendants of other
members stay where they are inside them).

## `editor.duplicate`

Duplicates the entities with their subtrees: fresh guids, each copy under the same parent as its original,
its local translation moved by `offset` (default `[1, 0, 0]`). One undo entry; the copies become the selection.

Params: `entities` or `entity` (one id), `offset`. Returns `{ entities, entity, source, parent, undoable }`:
`entities` are the copies' roots in the order given; `entity`, `source` and `parent` describe the first copy. Only scene entities (with an
`EntityGuid`) can be duplicated; `scene.duplicate` copies the components of any entity without undo.

## `editor.delete`

Deletes the entities with their subtrees. One undo entry; undo brings them back under their old parents (with
their guids, so earlier undo entries still find them) and redo deletes them again. Entities without an
`EntityGuid` are deleted but cannot come back.

Returns `{ deleted, undoable }`.

## `editor.select_hierarchy`

Selects relatives: `scope` `"subtree"` (each entity and all descendants), `"children"` (direct children) or
`"parent"`. `mode` `"set"` (default) replaces the selection, `"add"` extends it. Returns the selection like
`editor.get_selection`. `entities: []` with `mode` `"set"` succeeds and leaves the selection empty. `entities: []`
with `mode` `"add"` succeeds and does not change the selection. Neither call is `hierarchy_no_entities`.

## Error codes

Refusals carry the catalog code in `error.data.code` (`F0002` bad parameters, `F0003` not found, `F0001`
internal; see [protocol](protocol.md)) and one of these in `error.data.hierarchy_error`:

| Code | Meaning |
|---|---|
| `hierarchy_no_entities` | `entities` is empty, or absent with an empty selection, on `editor.reparent`, `editor.duplicate`, `editor.delete` or `editor.group`. Not on `editor.select_hierarchy` |
| `hierarchy_entity_missing` | an entity (or `root`) does not exist |
| `hierarchy_entity_internal` | an entity is editor-internal |
| `hierarchy_parent_missing` | `parent` does not exist |
| `hierarchy_parent_internal` | `parent` is editor-internal |
| `hierarchy_cycle` | `parent` is one of the entities or inside one of their subtrees; for delete, duplicate and group, an entity sits on a `ChildOf` cycle (malformed data: reparent one of them to the root first) |
| `hierarchy_no_guid` | duplicate of an entity without an `EntityGuid` |
| `hierarchy_scene` | the scene snapshot or spawn behind a duplicate or group failed (see `logs.read`) |
| `hierarchy_feed_owned` | the entity is owned by a content feed (`FeedOwned`); bake it with `dcc_feed.bake`, or edit it in the content tool |

## The panel

- The search box keeps rows whose name contains the text (case-insensitive) and their ancestors, drawn open; the
  collapse state comes back when the search is cleared. Drag and drop works on the filtered rows.
- Click selects; Ctrl-click toggles; Shift-click selects the drawn rows from the primary selection to the clicked
  row; Ctrl+Shift-click adds that range.
- Dragging a selected row drags the whole selection; dragging an unselected row drags that row alone. Dropping on
  a row reparents under it (the row outlines in the error color when the drop would create a cycle and is
  refused); dropping on the empty space under the tree moves to the root. A drop keeps world poses; hold Alt
  while dropping to keep the local transforms instead.
- The context menu acts on the selection when the clicked row is part of it: Duplicate, Group, Unparent, Select
  subtree, Select children, Delete (Rename and Create empty child for one row).
- With the pointer over the panel: Delete, Ctrl+D (duplicate), Ctrl+G (group), Ctrl+Z (undo), Ctrl+Shift+Z or
  Ctrl+Y (redo).

`regia_editor::outliner::OutlinerRows` (a world resource) records where the rows, their toggles, the search box
and the root drop zone were drawn, and the search text, for tools and tests
that drive the panel with synthetic pointer input. `seeing.screenshot` shows the panel itself.

During a multi-client play-in-editor session, `editor.hierarchy { client }` answers for that client's world
(`client` is the 0-based index of `editor.pie_net_clients`).
