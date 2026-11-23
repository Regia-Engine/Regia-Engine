# Material graph

The **Material Graph** tab edits a `.matgraph.ron` as a node canvas: a box per node with a pin
per input, a **Surface** box with the surface outputs (base color, alpha, metallic, roughness,
normal, occlusion, emissive, world position offset), a live preview sphere, and each compile
error shown on the node it belongs to. Material functions (`.matfunc.ron`) and instances
(`.matinst.ron`) open as more tabs in the same panel. The simpler **Material** tab edits a
`.material.ron`'s parameters (base color, metallic, roughness) with a preview. Walk-through:
[Your first material graph](../../tutorials/editor-first-material.md). Depth: [Material graph
editor](../../agents/material-graph-editor.md), [Material graphs](../../agents/material-graph.md),
[Materials](../materials.md).

## Material Graph tab

| Action | Control / gesture / key | Agent method |
|---|---|---|
| New graph | **New** | `editor.matgraph_open` with `create` |
| Open | Type a path, **Open** | `editor.matgraph_open` |
| Save, save as | **Save**, **Save as** (the path field's path) | `editor.matgraph_save` |
| Undo, redo | **Undo**, **Redo**; Ctrl+Z, Ctrl+Shift+Z / Ctrl+Y | `editor.undo`, `editor.redo` |
| Add a node | Right-click the canvas ▸ **Add node** | `editor.matgraph_edit` `add` |
| Connect, disconnect | Drag from an output pin to an input pin; the canvas's wire gestures disconnect | `editor.matgraph_edit` `connect`, `disconnect` |
| Move a node | Drag it (one undo step per drag) | `editor.matgraph_edit` `move` |
| Drive a surface output | Wire into the **Surface** box, or right-click a node ▸ **Drive output** | `editor.matgraph_edit` `output` |
| Duplicate, delete a node | Right-click ▸ **Duplicate**, **Delete** | `editor.matgraph_edit` `add`, `remove` |
| Functions | Right-click ▸ **Collapse into function…**, **Expand inline**, **Open function** | `editor.matgraph_function_collapse`, `editor.matgraph_function_expand`, `editor.matgraph_function_open` |
| Parameters, textures, switches, shading | Side column: edit the block's RON, **Apply** | `editor.matgraph_edit` `params`, `textures`, `switches`, `shading` |
| Compile status | The header shows **compiles** or the error count; click an error row to select its node | `editor.matgraph_state`, `editor.matgraph_compile` |
| Preview | The side column's sphere (needs a GPU) | `editor.matgraph_preview` |
| Instance overrides | **Override**, **Clear override** | `editor.matgraph_instance_set` |
| Tabs | The tab strip; **Close tab** | `editor.matgraph_tabs` |

The runtime twins `material.graph_get`, `material.graph_set` and `material.compile` read and
write graphs without the panel ([Material graphs](../../agents/material-graph.md)).

## Material tab

| Action | Control | Agent method |
|---|---|---|
| Edit base color, metallic, roughness | The fields | `editor.edit_material` |
| Save | **Save material** | `editor.edit_material` (saves) |
| Undo, redo | **Undo**, **Redo** | `editor.undo`, `editor.redo` |
| Reload | **Reload from disk** | none |

## Keyboard and accessibility notes

The **Nodes** column beside the canvas is the keyboard route (ADR-7383). It lists every node by
name (only the rows in view are drawn, so large graphs stay fast); Tab reaches it. With a row
focused:

| Key | Does | Agent method |
|---|---|---|
| Up / Down | Focus the previous / next node | none |
| Enter or Space | Select the node and frame it on the canvas | none |
| Ctrl+Shift+A, or **Add node…** | Open the palette: type to filter, Enter adds the first match (or Tab to a kind and press Enter). The node is placed to the right and its first input is wired from the focused node, one undo step | `editor.matgraph_edit` `add`, then `connect` |
| **Connect…** | Type the target node's name (or `Surface` / `Outputs` for the sink) and its input pin; Enter connects | `editor.matgraph_edit` `connect`, `output` |
| Delete, or **Delete node** | Remove the node | `editor.matgraph_edit` `remove` |
| Alt+Left / Right / Up / Down | Move the node 16 units, one undo step per press | `editor.matgraph_edit` `move` |

The buttons act on the last focused row, so Shift+Tab from a row reaches them. An instance tab is
read-only: the list selects and frames only. The canvas is announced as "Material graph canvas, N
nodes". Box select and the wire drag gestures stay mouse-only. Findings: [Editor
accessibility](accessibility.md).
