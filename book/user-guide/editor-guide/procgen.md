# Procedural graph

The **Procedural Graph** tab edits a `.pgraph.ron` as a node canvas: generators, filters and
spawners wired together, previewed live on a bound generator, with per-node visualization in the
viewport, a data table per node, and Paint and Box tools for masks. Double-click a `.pgraph.ron`
or `.pgvar.ron` in the [Content browser](content-browser.md) to open it. Edits go on the shared
undo stack. Depth: [Procedural graph editor](../../agents/procgen-editor.md), [Procedural
generators](../../agents/procgen.md).

The node keys act on the selected nodes while no text field has focus.

| Action | Control / gesture / key | Agent method |
|---|---|---|
| New, open | **New…** ▸ **Create**; double-click in Content | `editor.procgen_new`, `editor.procgen_open` |
| Save | **Save** | `procgen.graph_edit` with `"save": true` (an atomic write); `editor.procgen_reload` rereads the file |
| Add a node | Palette buttons; right-click the canvas ▸ **Add node**; drop a wire on empty canvas ▸ **Add and connect** | `procgen.graph_edit`, `editor.procgen_palette` |
| Connect, disconnect | Drag between pins | `procgen.graph_edit` |
| Select | Click; Shift+click adds; drag a box on empty canvas | `editor.procgen_select` |
| Visualize a node in the viewport | Node menu ▸ **Visualize (V)**, V | `editor.procgen_visualize` |
| Inspect a node's data | Node menu ▸ **Inspect (I)**, I | `editor.procgen_table` |
| Bypass a node | Node menu ▸ **Bypass (B)**, B | `procgen.graph_edit` |
| Delete | Node menu ▸ **Delete**, Delete | `procgen.graph_edit` |
| Duplicate | Node menu ▸ **Duplicate**, Ctrl+D | `procgen.graph_edit` |
| Copy, paste | Ctrl+C, Ctrl+V (the selection as RON text) | `procgen.graph_edit` |
| Enter a subgraph | Double-click a call node | `editor.procgen_expand`, `editor.procgen_collapse` |
| Parameters, seed, preview | The settings and parameters panes | `editor.procgen_param`, `editor.procgen_seed`, `editor.procgen_preview`; `procgen.run` |
| Panel state, tools | the panel; Paint and Box tools | `editor.procgen_state`, `editor.procgen_panel`, `editor.procgen_tool`, `editor.procgen_tool_cancel` |
| Undo, redo | Ctrl+Z, Ctrl+Shift+Z / Ctrl+Y | `editor.undo`, `editor.redo` |

## Keyboard and accessibility notes

Selection, visualize, inspect, bypass, delete, duplicate, copy and paste have keys. The **Nodes**
column left of the palette is the keyboard route for the canvas (ADR-7383): one row per node,
named `<type> #<id>` (only the rows in view are drawn). With a row focused:

| Key | Does | Agent method |
|---|---|---|
| Up / Down | Focus the previous / next node | none |
| Enter or Space | Select the node and frame it on the canvas | `editor.procgen_state` reads the selection |
| Ctrl+Shift+A, or **Add node…** | Open the palette filtered to types that accept the node's first output: type to filter, Enter adds the first match, wired from that output, one undo step | `procgen.graph_edit` `add_node`, `connect` |
| **Connect…** | Type the output pin (empty: the first), the target (`12`, `#12` or its row name) and the input pin; Enter connects | `procgen.graph_edit` `connect` |
| Delete, or **Delete node** | Remove the focused node (with a row focused, Delete acts on that row, not the selection) | `procgen.graph_edit` `remove_node` |
| Alt+Left / Right / Up / Down | Move the node 16 units, one undo step per press | `procgen.graph_edit` `move` |

The canvas is announced as "Procedural graph canvas, N nodes". Box select stays mouse-only. The
boolean settings' checkboxes carry their setting's name for screen readers since this guide's
review. Findings: [Editor accessibility](accessibility.md).

More procedural views (parameters tab, badges, biome overlay, plant preview) are on the branches
`lane/sprint-post05-procgen-editor` and `lane/sprint-post05w4-editor-polish`.
