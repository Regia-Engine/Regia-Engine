# Bulk Edit

Panel id: `bulk_edit`.

The Bulk Edit panel edits many objects at once as a spreadsheet. Each row is an entity (or a data
asset); each column is a field of one component (or of the asset's type).

## Opening it

- Select entities and choose **View ▸ Bulk Edit**, or right-click a row in the Outliner and choose
  **Edit in grid** (the clicked rows or the selection).
- Right-click a data asset (`.data.ron`) in the Content Browser and choose **Edit in grid**: the
  rows are every data asset of the same type in that folder.

The **Component** list switches to another component the rows share. Nested fields show as their own
columns (`offset.x`, `offset.y`, `offset.z`).

## Working in the grid

- **Edit a cell** with its usual Inspector widget (drag a number, tick a box, pick an enum value), or
  press Enter on the cursor cell and type.
- **Sort**: click a column header (again to reverse, a third time to go back to the opened order).
- **Filter**: type in **Filter rows**; rows whose name or shown cells contain the text stay.
- **Columns**: open **Columns** to search the fields, show or hide them and pin them. Pinned
  columns stay at the left while the others scroll. Right-click a header to pin or hide it.
- **Resize** a column by dragging the right edge of its header, or with Alt+Left and Alt+Right.
- **Copy and paste**: Ctrl+C copies the selected cells as tab-separated text, which a spreadsheet
  pastes as cells; Ctrl+V pastes a block copied from a spreadsheet at the cursor.
- **Fill down**: select a range (Shift+arrows) and press Ctrl+D or **Fill down**: the top row is
  copied into the rows below.

Each paste, fill or cell edit is one step in **Edit ▸ Undo**, however many cells it changes. If one
pasted value does not fit its field (text in a number column), nothing is pasted and the panel says
which cell. Data assets are written to disk with **Save**. **Close** keeps a grid with unsaved asset edits open and says which asset; **Discard and close** drops the edits and closes it.

## Keys

| Key | Does |
|---|---|
| Tab (from the toolbar) | Moves into the grid |
| Arrows | Move the cell cursor; with Shift, extend the selection |
| Tab / Shift+Tab | Next / previous cell |
| Enter or F2 | Edit the cell as text; Enter commits, Escape cancels |
| Ctrl+C / Ctrl+V | Copy / paste the selected cells |
| Ctrl+D | Fill down |
| Alt+Left / Alt+Right | Narrow / widen the column |
| Escape | Leave the grid |

Large selections stay fast: only the rows on screen are drawn.

Agents use the same grid through `editor.bulk_edit_open`, `editor.bulk_edit_set`,
`editor.bulk_edit_state`, `editor.bulk_edit_view` and `editor.bulk_edit_save`
([Bulk Edit grid](../../agents/bulk-edit.md)).
