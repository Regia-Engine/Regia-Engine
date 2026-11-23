# Layers

The **Layers** tab works with the data layers of the open level: named groups of entities (night
props, a quest's actors) that a game switches together. In the editor you show, hide, load and
unload a layer to work on one group at a time, choose which layers new entities join, add the
selection to a layer, define and bind layers, and edit a placed level (a saved level placed inside
this one) on its own. Depth: [Levels: per-entity files, data layers and placed
levels](../../agents/levels-layers-instances.md).

## Open it

The tab is in the default bottom group (**View ▸ Reset layout** restores a closed one). An agent
opens it with `editor.layout_apply {"open": "layers"}`. With no layer bound by the level the table
says "This level binds no layers."

## Walk-through: work on one layer

1. Open **Layer definitions** and press **Bind** next to a `.layers.ron` file of the project to bind it to the
   open level. If there is none, type a **file** (relative to `assets/`) and a **name** and press **Create layer**;
   tick **Editor only** for a layer that is absent in play.
2. The table lists each layer with columns **layer**, **visible**, **loaded**, **current**, **initial** and
   **members**. Untick **visible** to hide a layer's members (the layer is parked: still there, hidden, not
   simulated). Untick **loaded** to remove them from the world (the layer is absent). Tick **loaded** again to bring
   them back parked, and **visible** to show them.
3. Select entities and press **add selection** or **remove selection** on a layer's row. This is one undo step.
4. Tick **current** on a layer: while the current set is not empty, entities you place join it. The line above the
   table reads `New entities join: <names>`; **clear** empties the set.
5. Pick the layer's state at load time in the **initial** drop-down (`absent`, `parked` or `live`). This one is saved
   with the level (one undo step, written on the next save); the **visible** and **loaded** ticks are not.
6. Press Play: the session starts from the **initial** states again, whatever you toggled.

The **visible**, **loaded** and **current** toggles are a working state: they change no file and not the level's
initial states.

## Placed levels

Under the toolbar, a **Create placed level** section moves the selected entities into a new level file at
`assets/<path>`, picking a pivot (**lowest center**, **center**, or **primary selection**), and puts one instance in
their place (**Create from selection**). With a placed-level instance selected, the tab shows `Placed level <name>`
with **Edit placed level** and **Break placed level**. Editing opens that level on its own with a banner `Editing
placed level <path>`; **Commit** saves it and reopens the host, **Discard** reopens the host unchanged. Breaking
replaces the instance with ordinary entities.

## What is on screen

| Action | Control / key | Agent method |
|---|---|---|
| Show or hide a layer | **visible** checkbox | `editor.layers_panel_set` with `layer` and `visible` |
| Load or unload a layer | **loaded** checkbox | `editor.layers_panel_set` with `layer` and `loaded` |
| Set the layers new entities join | **current** checkboxes, **clear** | `editor.layers_panel_set` with `current` |
| Add or remove the selection | **add selection**, **remove selection** | `editor.layers_panel_set` with `add_selection`, `remove_selection` |
| Initial state at load | **initial** drop-down | `editor.layer_initial` |
| Create a layer (and its file) | **Create layer**, **Editor only** | `editor.layer_create` |
| Bind or unbind a definition file | **Bind**, **Unbind** | `editor.layer_bind` |
| Delete a layer | **Delete layer** | `editor.layer_delete` |
| Rescan `.layers.ron` files | **Refresh** | none (view only) |
| Create a placed level | **Create from selection** | `editor.placed_create` |
| Edit a placed level on its own | **Edit placed level**, **Commit**, **Discard** | `editor.placed_edit` |
| Break a placed level | **Break placed level** | `editor.break_placed` |
| Read the tab | the tab | `editor.layers_panel_state` |

The run-time view of the same layers is `level.layers` and `level.set_layer_state`.

## Keyboard

None of its own. Checkboxes, drop-downs and buttons are focusable (Tab, then Space or Enter).

## Known limits

- **Create layer** and **Delete layer** rewrite the definition file directly and are not undo steps; deleting a
  layer is refused while a level that binds it has members in it (the error line names the reason).
- Editing a placed level needs a saved host level (the host is closed during the session, not dimmed).
- The table lists at most 100 layer findings (yellow lines) and then says how many more there are; an agent's
  `editor.layers_panel_state` has them all.
- Errors from an action show on a red line at the top of the tab.

## See also

[Outliner](outliner.md) (the row menu has the same placed-level items), [Problems](problems.md), [Levels: per-entity
files, data layers and placed levels](../../agents/levels-layers-instances.md).
