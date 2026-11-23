# Behavior tree

The **Behavior Tree** tab edits a `.bt.ron` behavior tree: a palette of node kinds and registered leaf tasks, the
tree as an indented outline, an inspector for the selected node and a list of validation problems. While the game
plays it shows the last status of every node (running, success or failure) for the agent you pick. Depth: [Behavior
tree editor (agents)](../../agents/behavior-tree-editor.md), [Navigation and AI](../navigation-ai.md).

## Opening it

The tab sits in the group under the viewport in the default layout; **View ▸ Reset layout** brings it back if you
closed it. An agent brings it forward with `editor.layout_apply {"open": "behavior_tree"}`. It has no palette
command and no key.

## Walk-through: build a guard tree

1. Click **New** for an empty tree, or type a path such as `assets/ai/guard.bt.ron` and click **Open**.
2. In the **Add:** row click **selector**. With no root yet the node becomes the root. The row's hover text says
   where a click adds the node: under the selected node, as the root, or as an unconnected node.
3. Select the selector in the outline, then add a **sequence** and a **condition**: each is added under the
   selected node when it can take a child (a decorator takes exactly one child; a leaf, condition, compare or
   state tree node takes none).
4. In the **Leaf:** row click a registered task to add a leaf that runs it (the row shows one **leaf** button when
   no tasks are registered).
5. Select a node to edit it in the inspector: its name and, for the kinds that use them, the blackboard key,
   comparison operator, value type, other key, seconds, repeat count, abort mode, parallel policy and task. **Move up**
   and **Move down** reorder it among its siblings, **Disconnect** detaches it from its parent, **connect under**
   attaches it to another node that can take a child, **Make root** makes it the root, **Remove** removes it with
   everything below it, and **Remove (keep children)** removes only the node.
6. The top bar says **compiles** or **N error(s)**; the problem list at the bottom names each problem, and clicking
   one selects its node. A node with problems is marked with a warning sign and its count in the outline.
7. Click **Save** (when the tree has a path; **Save as** writes to the path in the box). A tree with errors is
   saved too, so you can build it up through invalid states.
8. Press Alt+P. The line **Live state** appears when an agent runs a tree: choose the agent, and each node in the
   outline takes the color of its status. If the running tree differs from the open document the panel says so and
   does not color the nodes.

Every edit is one undo step; **Undo** and **Redo** on the toolbar show the step's name on hover, and Ctrl+Z and
Ctrl+Y use the editor's undo stack. **New** and **Open** warn the first time when the open tree has unsaved changes
("unsaved changes: press again to discard them"); press again to discard.

## Actions

| Action | Control / key | Agent method |
|---|---|---|
| Start a tree, open a file | **New**, path box and **Open** | `editor.bt_open` |
| Add, connect, reorder, change or remove nodes | **Add:** and **Leaf:** rows, the inspector | `editor.bt_edit` |
| Read the tree and its problems | The outline and problem list | `editor.bt_state` |
| Save | **Save**, **Save as** | `editor.bt_save` |
| Undo, redo | **Undo**, **Redo**, Ctrl+Z, Ctrl+Y | `editor.undo`, `editor.redo` |
| Check or edit a file without the panel | none | `ai.bt_validate`, `ai.bt_edit`, `ai.bt_get` |

## Keyboard

The panel handles no keys of its own. Tab and Shift+Tab move between controls, and Space or Enter presses the
focused button or selects the focused outline row ([Editor guide](index.md)).

## Known limits

- Node kinds are `selector`, `sequence`, `parallel`, `inverter`, `condition`, `compare`, `cooldown`, `repeat`,
  `timeout`, `service`, `state_tree` and `leaf`; leaf tasks are the ones the game registers (with none registered,
  the editor skips the task check).
- Nodes without a parent are listed under **Unconnected nodes** until you connect them.
- The outline is a list, not a canvas: there is no drag to move a node; reorder and connect with the inspector
  controls.

## See also

[State trees](state-trees.md), [Behavior tree editor (agents)](../../agents/behavior-tree-editor.md), [Navigation and
AI](../navigation-ai.md).
