# State trees

Two tabs work with state trees, the hierarchical decision model of the AI system (`.statetree.ron` files).
**State Tree Editor** authors a tree: a node view of its states and transitions, a palette, an inspector for the
selected state and a live list of problems. **State Trees** debugs the trees that run in the game: which state is
active, which transitions hold, the blackboard and the recent trace. Depth: [State tree editor
(agents)](../../agents/state-tree-editor.md), [State trees (agents)](../../agents/state-tree.md), [Editor gameplay
panels](../../agents/editor-gameplay-panels.md).

## Opening them

Both tabs sit in the group under the viewport in the default layout; **View ▸ Reset layout** brings back a
closed one. An agent brings one forward with `editor.layout_apply {"open": "state_tree_editor"}` or
`{"open": "state_trees"}`. Neither has a palette command or a key.

## Walk-through: author a small tree

1. Open **State Tree Editor**. Click **New** for a tree with a root state named `Root`, or type a path such as
   `assets/ai/guard.statetree.ron` in the box and click **Open**.
2. In the **Palette**, type a name in **new state** and click **Add under Root** (the button names the selected state,
   or the root when none is selected). Do the same for a second state.
3. Click a state in the **Node view** to select it. The **Inspector** shows its name (**Rename**), **Parent**,
   **Selection** (First, Utility or Weighted), tasks, parameters, transitions and the enter condition. Conditions
   are typed as RON text and applied with **Apply**.
4. With a state selected the palette offers **New transition from** that state: choose a trigger and a target and
   click **Add transition**. The transition is drawn as an arrow in the node view.
5. Watch the toolbar: it says **valid** or **N error(s)**. Every problem is listed under **Problems**; a state with an
   error is outlined red in the node view, and clicking a problem selects its state.
6. Click **Save** (it is disabled while there are errors; **Save as** writes to the path in the box).
7. Press Alt+P. The **Play** section lists the agents whose `StateTreeSource` names the open file; pick one in
   **Agent** and the node view fills the active path green. Saving reloads the file into those agents.

Every edit is one undo step; **Undo** and **Redo** on the toolbar (their hover text names the step) and Ctrl+Z
and Ctrl+Y use the editor's undo stack. **New** and **Open** refuse the first time when the open tree has unsaved
changes and say so; press again to discard them.

## Walk-through: debug a running tree

1. Press Alt+P. **State Trees** lists every entity with a `StateTree` in the world it shows, one line per tree:
   name, status (`not_started`, `running`, `succeeded` or `failed`), `paused` when paused, the active path and the
   tick count. With no tree it says so and reminds you that trees run in play.
2. Click a tree. **Pause** (or **Resume** while paused), **Restart**, and **Send event** (type an event name first)
   steer it.
3. Under **States and transitions** the active states are green and bold; each transition line reads `-> target on
   trigger`, with its condition, priority and hold time, amber while it holds and blue once taken (with the tick).
4. **Blackboard** shows the entity's blackboard (or says it has none); **Trace** shows the recent trace newest first.

## Actions

| Action | Control / key | Agent method |
|---|---|---|
| Start a new tree, open a file | **New**, path box and **Open** (State Tree Editor) | `editor.statetree_open` |
| Edit states, transitions, tasks, includes, tree fields | Node view, Palette, Inspector, **Tree: fields, ambient tasks, includes** | `editor.statetree_edit` |
| Read the document and its problems | The node view and **Problems** | `editor.statetree_state` |
| Save | **Save**, **Save as** | `editor.statetree_save` |
| Undo, redo | **Undo**, **Redo**, Ctrl+Z, Ctrl+Y | `editor.undo`, `editor.redo` |
| Read the debugger | State Trees tab | `editor.state_tree_panel_get` |
| Choose the tree shown in detail | Click a tree in the list | `editor.state_tree_panel_select` |
| Pause, resume, restart, send an event | **Pause**/**Resume**, **Restart**, **Send event** | `editor.state_tree_panel_control` (`pause`, `resume`, `restart`, `send_event`) |
| Skip a state, pause when one is entered | **skip**, **pause on enter** check boxes (development builds only) | `editor.state_tree_panel_control` (`debug`) |
| Reload a tree from a file | none in the panel | `editor.state_tree_panel_control` (`reload`) |

## Keyboard

Neither panel handles keys of its own. Tab and Shift+Tab move between controls and Space or Enter presses the
focused button; the global keys work as everywhere ([Editor guide](index.md)). The node view is a drawn canvas
operated by pointer click only; the same edits are available from the keyboard through the Inspector, the Palette
and the Problems list, and the agent route is `editor.statetree_edit`.

## Known limits

- State Trees only shows trees that exist in the world it reads (the play world while a session runs, else the edit
  world), up to 512 trees.
- The **skip** and **pause on enter** switches exist only in development (debug) builds of the editor.
- A tree with validation errors is not saved: fix the errors first.
- Including another tree adds its states under a prefix; list the included root under a state with **List under**
  in the **Tree** section to reach it.

## See also

[Editor gameplay panels](../../agents/editor-gameplay-panels.md), [Navigation and AI](../navigation-ai.md), [Behavior
tree](behavior-tree.md).
