# Bulk Edit grid (`editor.bulk_edit_*`)

The Bulk Edit panel (View ▸ Bulk Edit, or **Edit in grid** on an Outliner row or a data asset in the
Content Browser) shows many objects as a spreadsheet: one row per entity (or per `.data.ron` asset of
one type), one column per reflected field of a component (or of the asset type). Nested structs are
flattened into dotted columns (`offset.x`, `aim.yaw`); a quaternion stays one cell. Agents drive the
same grid with five methods (ADR-7787).

Rows are addressed in **view order**: after the filter and the sort. Columns are addressed by
**path** or by their index in the shown order (pinned columns first). A cell's **text** is a string
field's text, or the reflect serializer's compact JSON for anything else (`1.5`, `true`, `Calm`
for a unit enum variant, `[1.0,0.0,0.0]`), so a block copied to a spreadsheet and pasted back is
unchanged.

Every write is **one undo step** on the editor's undo stack (`editor.undo` reverts the whole range):
entity rows get the same `SetProperty` commands the Inspector pushes, grouped; asset rows get one
asset edit and are written to disk by `editor.bulk_edit_save`. A write is checked cell by cell
before anything changes: one value that does not fit its field refuses the whole call.

## `editor.bulk_edit_open`

Params: `entities` (entity ids; default the editor selection) or `assets` (project-relative
`.data.ron` paths, all of one type), `component` (short or full type path; for entities it defaults
to the open grid's component when all rows have it, else the first component they share; for assets
it is optional and must name their type), `columns` (paths to show, in order; default every field),
`discard` (drop the open grid's unsaved asset edits first).

```json
{ "method": "editor.bulk_edit_open",
  "params": { "entities": [4294967360, 4294967361], "component": "PointLight", "columns": ["intensity", "range"] } }
```

Returns the `editor.bulk_edit_state` result. Opening brings the panel forward.

## `editor.bulk_edit_set`

Exactly one of:

- `cells`: `[{ row | key, column, value | text }]`. `row` is a view row; `key` is an entity id or an
  asset path; `column` is a path or a shown column index; `value` is reflect JSON, `text` is cell
  text.
- `tsv` with `row` and `column` (defaults 0): tab-separated rows of cell text pasted with its
  top-left cell there, as Ctrl+V does. A quoted cell may hold tabs, line breaks and doubled quotes.
  Cells past the last row or column are clipped and counted.
- `fill_down`: `{ row, column, rows?, columns? }`: the top row of the range is copied into the rows
  below it, as Ctrl+D does.

```json
{ "method": "editor.bulk_edit_set", "params": { "tsv": "1\t2\t3\n4\t5\t6\n", "row": 0, "column": 0 } }
```

Returns `{ set, changed, skipped, clipped, undo_depth }`: `set` cells written to, `changed` cells
whose value differed (no undo entry when 0), `skipped` cells on rows without the component.

## `editor.bulk_edit_state`

Read-only. Params: `offset` and `limit` (a window of view rows; default 0 and 100), `range`
(`{ row, column, rows?, columns? }`: also return `tsv`, the cells as Ctrl+C copies them).

Returns `{ source, component, columns: [{ path, pinned, width }], available, sort, filter,
rows_total, rows_shown, offset, rows: [{ row, key, label, cells }], cursor, range, drawn_rows,
dirty, tsv? }`. `cells` are cell texts, `null` where the row lacks the component. `drawn_rows` is the
number of rows the panel laid out in its last frame (only the rows in view are laid out, so a grid
of 10 000 rows draws a few dozen). `dirty` lists asset rows with unsaved edits.

## `editor.bulk_edit_view`

View state only, never an undo step. Params: `columns` (shown paths, in order), `pinned` (the
shown columns kept at the left and always in view), `sort` (`{ column, descending? }`, or `null` for
the opened order; numbers sort by value, other cells by text, rows without the component last),
`filter` (rows whose label or a shown cell contains the text, any case), `widths` (`{ path: points }`,
36 to 640), `cursor` (`{ row, column, rows?, columns? }`: the cursor and the selected range),
`close` (close the grid; `discard: true` drops unsaved asset edits). Returns the state, or
`{ closed: true }`.

## `editor.bulk_edit_save`

Writes the asset rows with unsaved edits (temporary file and rename). Returns
`{ saved: [path], bytes }`. Entity rows are saved with the level.

## `editor.bulk_edit`

The Content Browser selection's **Edit selection in grid** (P-145, ADR-8147): opens the grid with
the selected data assets as rows, in selection order. `assets` (project-relative paths) defaults
to the Content Browser selection; files that are not `.data.ron` are skipped and listed in
`skipped`. The data assets must share one type (`bulk_edit_mixed_types`); a selection with no data
asset is `bulk_edit_no_rows`. `discard: true` drops the open grid's unsaved asset edits first
(otherwise `bulk_edit_unsaved`). Returns the `editor.bulk_edit_state` result plus `skipped`.

```json
{ "method": "editor.bulk_edit", "params": { "assets": ["assets/loot/a.data.ron", "assets/loot/c.data.ron"] } }
```

In the editor: select several assets in the Content Browser, then **Edit selection in grid** in a
selected row's menu or in the selection bar above the list (Tab reaches its buttons).

## Errors

Refusals carry `data.reason`:

| Reason | When |
|---|---|
| `bulk_edit_not_open` | No grid is open. |
| `bulk_edit_bad_params` | A parameter has the wrong shape, or `set` got none or several of `cells`, `tsv`, `fill_down`. |
| `bulk_edit_no_rows` | No entities (empty selection) or no assets. |
| `bulk_edit_entity_gone` | An entity id does not exist. |
| `bulk_edit_unknown_component` | `component` is not a registered reflected component. |
| `bulk_edit_component_on_none` | None of the entities has the component, or they share none. |
| `bulk_edit_mixed_types` | The assets are not all of one type, or not of `component`. |
| `bulk_edit_bad_row` | A row is outside the shown rows or a key is not in the grid. |
| `bulk_edit_bad_column` | A column is not a field of the type or not shown. |
| `bulk_edit_bad_value` | A value does not fit its field (nothing was written). |
| `bulk_edit_feed_owned` | A row is owned by the DCC feed. |
| `bulk_edit_unsaved` | Asset rows have unsaved edits (save, or pass `discard: true`). |
| `bulk_edit_not_assets` | `save` on an entity grid. |
| `bulk_edit_no_project` | Asset rows with no project open. |
| `bulk_edit_io` | The asset folder could not be listed. |
| `bulk_edit_no_registry` | The type registry is missing (an editor set-up fault). |
| `data_asset_*` | Reading or writing a `.data.ron` failed (see [Asset creators](editor-creators.md)). |

## Keyboard (the panel)

With the grid focused (Tab to it, or click a row label): arrows move the cell cursor and Shift+arrow
extends the range; Tab and Shift+Tab move to the next and previous cell; Enter or F2 edits the cell
as text (Enter commits and moves down, Tab commits and moves right, Escape cancels); Ctrl+C and
Ctrl+V copy and paste the range; Ctrl+D fills down; Alt+Left and Alt+Right narrow and widen the
cursor's column; Escape leaves the grid.
