# Animation editor: Anim Graph, Clip and Skeleton

Panel ids: `anim_graph`, `anim_clip` and `anim_skeleton`.

Three tabs edit a character's animation: **Anim Graph** builds the blend tree and state machines
(`.animgraph.ron`), **Clip** edits a clip's notifies, curves, property tracks and root-motion mode,
and **Skeleton** adds sockets and IK chains to the rig. Clip and Skeleton write an authoring
sidecar, `<source>.animedit.ron`, next to the imported glTF; the graph's canvas positions go to an
editor-only `.animgraph.layout.ron` next to the graph. Depth:
[Animation editor (agents)](../../agents/animation-editor.md); the runtime side is
[Animation](../animation.md).

## Open it

All three tabs are in the default layout's bottom group; **View ▸ Reset layout** brings back a
closed one. An agent brings one forward with `editor.layout_apply`:

```json
{ "method": "editor.layout_apply", "params": { "open": "anim_graph" } }
```

The ids are `anim_graph`, `anim_clip` and `anim_skeleton`. There is no palette command for these
tabs.

## Build a graph

1. Open the **Anim Graph** tab. Type a path such as `graphs/x.animgraph.ron` into the path field
   and press **Open**, or press **New** for an empty graph.
2. Pick a character in **Rig:** (**Demo biped**, **Selected entity**), or type a `.glb` path and
   press **Load rig**. The preview on the right shows it.
3. In the left panel, add **parameters** (a kind from the drop-down, then **Add**): float, int,
   bool, vec2, vec3 or trigger. A trigger shows a **fire** button; **×** removes a declaration.
4. Right-click empty canvas for **Add node** and a list of node kinds. Drag from a pin to another
   node's input to connect. Right-click a node for **Make output**, **Open states** (state
   machines), **Duplicate** (not for state machines) and **Delete**. A state machine node also
   has an **Open states ›** button that opens its states canvas.
5. Select a node, state or transition to edit it in the inspector under the parameters.
6. Press **Play** in the preview, move parameter values, and watch the active state. **Restart**
   resets it; **speed** scales time; **Apply root motion** moves the preview character.
7. Press **Save**. It refuses an invalid graph and drops nodes no output reaches.

While a play-in-editor session runs, the preview shows the play world's animator running this
graph (an orange `PIE: debugging ...` line) with its live parameter values and active node
([Play-in-editor](play-in-editor.md)).

## Edit a clip

1. Open the **Clip** tab and pick a clip in the clip drop-down; **Play** runs it and the readout
   shows the time and the clip length.
2. Click or drag the ruler to scrub. Double-click the ruler to add a notify; drag a notify marker
   to move it. Select a notify to edit its **Name**, **Time** and **Payload (RON)** or press
   **Delete**.
3. In **Curves** and **Property tracks**, pick one to edit its keys, or type a name (a property
   track takes a path such as `Component.path`) and press **+ curve** or **+ property track**.
4. Choose a **Root motion** mode: None, XZ, XYZ or Yaw.
5. Type a `.animedit.ron` path if the rig has none and press **Save edits**.

The first edit of an imported clip copies its imported notifies into the sidecar, so they are kept.

## Edit a skeleton

1. Open the **Skeleton** tab. The header counts joints, sockets and IK chains.
2. Select a joint in the tree (arrows fold branches) or click it in the preview.
3. **Add socket here** adds a socket on that joint; **Add IK chain ending here** adds a two-bone
   chain (the joint needs a parent and a grandparent, or a message says so).
4. Edit sockets (name, joint, offset, rotation) and IK chains (name, joints, pole parameter) in
   their lists; **Delete socket** and **Delete chain** remove them.
5. Save the sidecar with **Save edits** on the **Clip** tab.

## Actions and agent methods

| Action | Control | Agent method |
|---|---|---|
| New, open, save a graph | **New**, **Open**, **Save** | `editor.anim_open`, `editor.anim_save` |
| Undo, redo | **Undo**, **Redo** buttons, Ctrl+Z | `editor.undo`, `editor.redo` |
| Declare a parameter | The parameter list, **Add** | `editor.anim_param` |
| Add, edit, remove a node | Canvas menu, inspector, **Delete** | `editor.anim_add_node`, `editor.anim_set_node`, `editor.anim_remove_node` |
| Connect, disconnect, set the output | Drag pins, **Make output** | `editor.anim_connect`, `editor.anim_disconnect` |
| States and transitions | The states canvas | `editor.anim_state`, `editor.anim_transition` |
| Read the graph with its validation | The inspector | `editor.anim_graph` |
| Choose the preview character | **Rig:**, **Load rig** | `editor.anim_rig` |
| Set preview parameters and advance | The preview | `editor.anim_preview` |
| Read the play world's animator | The PIE line | `editor.anim_debug` |
| Edit a clip (notifies, curves, root motion) | **Clip** tab | `editor.anim_clip` |
| Add sockets and IK chains | **Skeleton** tab | `editor.anim_skeleton` |
| Save the sidecar | **Save edits** | `editor.anim_save_edits` |
| Sample a clip into a scratch rig | none | `editor.sample_into` |
| See a panel as an image | none | `editor.capture_panel` with `anim_graph`, `anim_clip` or `anim_skeleton` |

## Keyboard

Ctrl+Z and Ctrl+Y undo and redo graph, clip and skeleton edits in order with level edits (the
shared undo stack). In the **Clip** tab, **F** refits the ruler to the whole clip. The tabs have no
other keys of their own.

## Known limits

- Two documents are open at a time: one graph and one rig's sidecar.
- Graphs are trees: connecting a node that already feeds something moves it.
- A saved graph drops unreachable nodes; the autosave keeps them for recovery.
- The editor-only layout file keeps canvas positions; the runtime ignores it.

See also: [Montage](montage.md), [Inertialized transitions](../inertialization.md),
[Editor guide](index.md).
