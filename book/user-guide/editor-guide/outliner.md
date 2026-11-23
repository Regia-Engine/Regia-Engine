# Outliner

The **Outliner** tab (docked left) is the level's entity tree: every entity by name, children
under their parents. Search it, select from it, rename, reparent by dragging, group, duplicate
and delete. A multi-entity edit is one undo step. Depth: [Editor hierarchy drag and
drop](../../agents/editor-hierarchy.md), [Hierarchy](../../agents/hierarchy.md), [Entity
kinds](../entity-kinds.md).

Delete, Ctrl+D and Ctrl+G act while the pointer is over the Outliner and no text field has
focus; a user shortcut for `outliner.delete`, `outliner.duplicate` or `outliner.group`
(Preferences ▸ Keymap) replaces the built-in key.

| Action | Control / key | Agent method |
|---|---|---|
| Select | Click a row; Ctrl+click toggles, Shift+click selects a range, Ctrl+Shift+click adds a range | `editor.select`, `editor.get_selection` |
| Select everything | **Select all** | `editor.select` |
| Search | The search field (`kind:<name>`, `kind:none`, `kind:?` filter by kind) | `editor.hierarchy` |
| Group rows by category | **Group by category** | `editor.hierarchy` |
| Rename | Right-click ▸ **Rename…** | `editor.set_property` (the `Name` component) |
| New child | Right-click ▸ **Create empty child** | `editor.spawn` with `parent` |
| Duplicate | Right-click ▸ **Duplicate**, Ctrl+D | `editor.duplicate` |
| Group | Right-click ▸ **Group**, Ctrl+G | `editor.group` |
| Make a prefab of the selection | Right-click ▸ **Create prefab from selection…**, type the file (under `assets/`, ending in `.prefab.ron`), **Create prefab** | `editor.create_prefab` ([Create prefab from selection](../../agents/prefab-create.md)) |
| Reparent | Drag a row onto another row, or onto **Move to root**; hold Alt to drop without keeping the world transform | `editor.reparent` |
| Unparent | Right-click ▸ **Unparent** | `editor.reparent` with a null parent |
| Select a subtree or the children | Right-click ▸ **Select subtree**, **Select children** | `editor.select_hierarchy` |
| Delete | Right-click ▸ **Delete**, Delete | `editor.delete` |
| Undo, redo | Ctrl+Z, Ctrl+Shift+Z / Ctrl+Y | `editor.undo`, `editor.redo` |

The row's ▾ / ▸ button folds a subtree. With the selection made here, the
[Viewport](viewport.md)'s F frames it and the [Inspector](inspector.md) edits it, so the whole
select-and-edit loop works from the keyboard. Findings: [Editor accessibility](accessibility.md).
