# Conversation

The **Conversation** tab edits a `<id>.conv.ron` conversation as a graph: blocks of lines between labels, branch
nodes for `If` and `Choice` steps, and End nodes, laid out automatically. An inspector edits the selected node, a
findings list shows what the conversation validator reports on the nodes, and a preview runs the conversation from any
node with conditions you set by hand. Depth: [Conversation editor (agents)](../../agents/conversation-editor.md),
[Conversation (agents)](../../agents/conversation.md), [Dialogue](../dialogue.md).

## Opening it

The tab sits in the group under the viewport in the default layout; **View ▸ Reset layout** brings it back if you
closed it. An agent brings it forward with `editor.layout_apply {"open": "conversation"}`. It has no palette command
and no key.

## Walk-through: add a line and preview it

1. Type a path such as `assets/conversations/guard.conv.ron` in the box and click **Open**. If the file does not
   parse, the tab says **Read-only: the file does not parse** and shows the error; fix the file in a text editor and
   open it again.
2. Click a node in the graph. The inspector on the right (**Node**) shows its id and, for each step in the block,
   its fields: **Speaker**, **Text key** (with the text for that key in the source locale, or a note that there is
   none), **Note**, **Label**, **Goto**, **If** (with its then and else targets), **Prompt key**, options for a
   choice, or **Event**. **Up**, **Down** and **Delete** move or remove a step.
3. Under **Insert after this node:**, type a line id, speaker and text key and click **Line** to add a line after
   the node's last step. Type a label, event or condition name and click **Label**, **Goto**, **If**, **Choice**,
   **Event** or **End** to add that step.
4. To change where a branch goes, drag the branch's port onto another node that has a label. A node with no label
   cannot be a jump target: the tab says to add a **Label** step first.
5. Watch the toolbar: **N error(s), M warning(s)** counts the validator's findings, which are listed under
   **Findings**; hovering a finding shows its fix, and clicking it goes to its node.
6. In **Preview**, tick the conditions the conversation names (the **true** boxes; **Add** adds a condition by name),
   then click **Start** (or select a node and click **From selected**, or double-click a node). **Next** steps past a
   line; for a choice, click an option. **Show node** jumps to the running node, **Stop** ends the run, and **Play
   voice** plays a line's voice clip.
7. Click **Save**.

Every edit is one undo step: **Undo** and **Redo** on the toolbar (their hover text names the step) use the editor's
shared undo stack, so Ctrl+Z and Ctrl+Y work too. A conversation you did not edit is saved with the bytes that were
read; an edit writes the asset's canonical form.

## The graph

Drag empty space to pan, use the mouse wheel to zoom and drag a node to move it (one undo step; the position is kept
in a layout file under `.regia/conversation_layout/`). **Auto place** in the inspector (shown for a node you moved) returns it to its automatic position,
and **Fit** in the toolbar shows the whole graph. **Search** counts the nodes whose lines, labels, jumps or conditions contain the text, and **Go** selects
the first hit. **Preview from here** in the inspector starts a preview at the selected node.

## Actions

| Action | Control / key | Agent method |
|---|---|---|
| Open a conversation | Path box, **Open** | `editor.conversation_open` |
| Read the graph, findings and layout | The panel | `editor.conversation_graph` |
| Edit steps, labels, jumps, options | The inspector, **Insert after this node:**, drag a port | `editor.conversation_edit` |
| Save | **Save** | `editor.conversation_save` |
| Run the preview a step at a time | **Start**, **Next**, an option button | `editor.conversation_preview_step` |
| Undo, redo | **Undo**, **Redo**, Ctrl+Z, Ctrl+Y | `editor.undo`, `editor.redo` |

## Keyboard

The panel handles no keys of its own. Tab and Shift+Tab move between its controls, and Space or Enter presses the
focused button ([Editor guide](index.md)). The graph is a pointer canvas; the same edits are available from the
inspector and the findings list, and from `editor.conversation_edit`.

## Known limits

- A file that does not parse opens read-only.
- There is no **New** button: create the `.conv.ron` file first, then open it.
- A jump needs a labeled target node.
- The preview mocks conditions with the check boxes; it does not run game code.

## See also

[Localization](localization.md), [Dialogue](../dialogue.md), [Conversation editor
(agents)](../../agents/conversation-editor.md).
