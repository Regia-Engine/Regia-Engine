# Tables

The **Tables** tab opens three kinds of gameplay data table from the project's `assets/` folder and edits them:
**Curves** (`.curves.ron`, `.curves.csv`: named curves of keys), **Chooser** (`.chooser.ron`: a table of rows and
columns that picks a result from a context) and **Data** (`.table.ron`, `.table.csv`: typed rows of one struct,
one row per key). Edits change the table in memory with the same library calls as the agent methods; **Save**
writes the file. Depth: [Editor data panels (agents)](../../agents/editor-data-panels.md), [Curve tables
(agents)](../../agents/curve-tables.md), [Choosers (agents)](../../agents/choosers.md), [Data tables
(agents)](../../agents/data-tables.md), [Data tables](../data-tables.md).

## Opening it

The tab sits in the group under the viewport in the default layout; **View ▸ Reset layout** brings it back if you
closed it. An agent brings it forward with `editor.layout_apply {"open": "tables"}`. It has no palette command and no
key.

## Walk-through: tune a curve

1. Type a path under `assets/` (for example `tables/falloff.curves.ron`) in the box and click **Open**. A
   `.curves` file opens on the **Curves** tab, a `.chooser.ron` on **Chooser** and a `.table` file on **Data**.
2. Click a curve in the list to select it. The graph overlays every curve and draws the selected one thicker with
   its keys. Drag a key to move it; double-click empty graph space to add a key on the curve; click a key to
   select it. Right-drag or middle-drag pans the graph and the mouse wheel zooms around the pointer.
3. A selected key shows tangent handles: drag one to change the tangent. Hold Alt to break the tangent so only the
   dragged side moves.
4. With a key selected, type exact **x** and **y** values, click **Auto tangents** to return the tangents to the
   derived ones, or **Delete key**.
5. Use **Add curve** (type a name first), **Rename** and **Delete curve** to manage curves, and the **interp**, **pre** and
   **post** (extrapolation) drop-downs per curve. The **Grid** section lists one column per x value and one row per curve, with a
   **+** to add a key; the CSV box at the bottom (**Replace the table with this CSV**) replaces the whole table.
6. Click **Save**. The title shows a `*` after the path while there are unsaved edits.

## Walk-through: test a chooser

1. Open a `.chooser.ron`. The **Test context** strip has a box per declared input and a **seed** drag value; type a
   value and leave the box to apply it.
2. The panel re-evaluates after every change. The line under the strip reads `row <id> → <result>` (or
   `catch-all → <result>`, or **no result**). In the grid, cells are green when they pass, red when they fail and
   amber when they score, and the selected row is marked blue.
3. Edit the table: the **on** check boxes enable or disable a column or a row, **▲**/**▼** reorder rows, **✕** removes a
   row or a column, the result box edits a row's result as RON, **Add row** adds a row and **Add column** adds a
   column of the chosen kind (the kind list, and for a distance column its influence, maximum distance, **wrap** and
   **drop beyond** settings). The **Options** bar sets the mode, the result type, the catch-all, the **near-best
   band** and **recency**.
4. Click **Save**.

## Walk-through: fix a data table

1. Open a `.table.ron` or `.table.csv`. The line above the grid shows the row type, the row count and where the
   schema came from.
2. Edit a cell as text and leave the box to apply it. A cell with an error is filled red-brown (a warning, amber), and
   hovering it shows the issue; an edit that would add a schema error is refused with the error shown, and the cell
   returns to its stored text.
3. Use **▲**/**▼** to move a row, **✕** to remove one, and type a key and click **Add row** for a new one. Remaining
   problems are listed under the grid.
4. Click **Save**.

## Actions

| Action | Control / key | Agent method |
|---|---|---|
| Open a table | Path box, **Open** | `editor.tables_panel_open` |
| Read the panel (all three tabs) | The panel | `editor.tables_panel_get` |
| Add, move, delete keys, tangents, curves; paste CSV | The graph, key boxes, curve buttons, CSV box | `editor.tables_panel_curve_edit` |
| Edit chooser cells, rows, columns, options | The grid, **Add row**, **Add column**, **Options** bar | `editor.tables_panel_chooser_edit` |
| Set the chooser's test context and seed | **Test context** strip | `editor.tables_panel_set_context` |
| Edit data cells, rows | The data grid, **Add row** | `editor.tables_panel_data_edit` |
| Undo the last edit of the shown tab | **Undo** | `editor.tables_panel_undo` |
| Save | **Save** | `editor.tables_panel_save` |
| Rename a data row's key, clear a data cell to its default | none (agent only) | `editor.tables_panel_data_edit` (`rename_row`, `clear_cell`) |

## Keyboard

The panel handles no keys of its own. Tab and Shift+Tab move between its controls, and Space or Enter presses the
focused button ([Editor guide](index.md)). The graph is a pointer canvas: the same curve edits are available from the
**Grid** section and the key boxes, and from `editor.tables_panel_curve_edit`.

## Known limits

- The panel's **Undo** is its own: it keeps 32 steps per open table and does not use Ctrl+Z.
- Edits never write the file until **Save**; each of the three tabs holds one open table at a time.
- A `.csv` table is written back as CSV; a `.table.ron` is written as canonical RON, and its comments are not kept.
- Paths are relative to the project's `assets/` folder.

## See also

[Data tables](../data-tables.md), [Editor data panels (agents)](../../agents/editor-data-panels.md), [Choosers
(agents)](../../agents/choosers.md).
