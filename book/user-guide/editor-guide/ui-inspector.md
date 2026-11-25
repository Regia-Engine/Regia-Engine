# UI Inspector

Panel id: `ui_inspector`.

The **UI Inspector** tab answers "why does this widget look like this, and which widget is that?"
for the interface of a running game. It shows the live UI tree, a map of every node's rectangle,
and for the selected node its resolved properties, where each value came from (the theme, the node
itself, an animation or a binding), its bindings, focus and accessibility data. It only reads; it
never changes the game's UI. Depth: [UI runtime inspector (agents)](../../agents/ui-inspector.md),
[UI layout debug overlay](../../agents/ui-debug-overlay.md), [UI designer](ui-designer.md).

## Open it

The tab is part of the default layout (the group under the viewport); **View ▸ Reset layout**
brings it back when closed. An agent uses `editor.layout_apply {"open": "ui_inspector"}`. There is
no menu item and no command palette `open` command for it.

## Inspect a node

1. Choose what to inspect. For the play-in-editor world, press Play (Alt+P), then click **PIE
   world** at the top of the tab. For a game running separately, type its project directory into
   the box ("project dir of a running game") and press **Attach**; the tab connects over that
   game's agent socket. A failed connection shows its error in red. Until one is chosen the tab
   says "Press Play, then choose "PIE world", or attach to a running game."
2. The left column is the **tree**. A `>` or `v` button expands or collapses a node; click a node's
   label to select it. The search box above the tree ("search kind, name, text, class, id") keeps
   matching nodes and their ancestors.
3. The right column shows the **layout map**: every node's rectangle scaled to fit, with the
   selected node outlined in yellow. Turn **Pick** on and hover the map to select the topmost node
   under the pointer (the game's own z-order and layers decide which that is); a click on the map
   picks too.
4. Below the map the property table describes the selected node: its type, name, rectangle and z,
   its accessible name and role, and whether it can take focus. The **visual**, **content** and
   **layout** groups each list property, value, source (`default`, `theme`, `inline`, `animation`
   or `binding`, each in its own color) and detail (for a theme value, the layer that won). A
   **bindings** group lists each binding's source, target, mode and last value.
5. Selecting a node also outlines it in the game itself, through the game's UI debug overlay.
   Selecting nothing clears the outline.
6. The numeric field with the `Hz` suffix sets how often the tree and the selection refresh (0.5 to
   10 Hz; it starts at 4).

| Action | Control / key | Agent method |
|---|---|---|
| Read the tree | the tree column | `ui.query` |
| Inspect one node | click its label | `ui.inspect` |
| Find the node under a point | **Pick** and the layout map | `ui.pick_at` |
| Outline a node in the game | selecting a node | `ui.highlight` |
| Choose the PIE world or attach to a game | **PIE world**, **Attach** | none (the tab's own state; an agent calls the `ui.*` methods on the game directly) |

## Keyboard

The tab has no shortcuts. The expand buttons, node labels, search box, **Attach**, **PIE world**
and **Pick** take focus with Tab. The layout map is a drawn shape that responds to the mouse only;
the tree gives the same selection from the keyboard.

## Known limits

- The tab reads the live UI; with no session running and nothing attached it shows nothing.
- It asks for at most 1,000 nodes per refresh, the limit of `ui.query`.
- It needs a game with the agent protocol on to attach to a separate process.
- **Pick** and the tree only select; to change a style, edit the theme or the node in the [UI
  designer](ui-designer.md).

See also: [Game UI](../ui.md).
