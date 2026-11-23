# UI drawn tree view, data grid and color picker (agents)

ADR-273 W10/W11, ADR-2045. Code: `regia_ui::parity_widgets` over the models of `regia_ui::tree` and
`regia_ui::color_picker` (ADR-1842).

- **Tree view** (`spawn_tree_view(commands, parent, roots, height)`): a virtualised list whose rows draw a chevron
  (U+25B6 closed, U+25BC open) at `depth × indent` and the label after it. A click on the chevron expands or
  collapses, a double click anywhere on the row does too, and every click selects (the row takes the theme's
  `Selected` look). Only visible rows exist, so 10 000 nodes cost one entity per visible row. Drive and read it with
  `ui.tree` on the tree entity ([UI tree view and data grid](ui-tree.md)). With `TreeView::reorder` on, rows are
  drag sources and drop targets: the top quarter of the row under the pointer drops before it, the bottom quarter
  after it, the middle inside it (as its last child); a bar or box marks the spot while dragging. A drop sends a
  `TreeDropped` message and moves the node with `UiTree::move_node` (a node never goes into its own subtree).
  Off by default: a game that wants to decide itself leaves `reorder` off and listens to `DragDropEvent`.
- **Data grid** (`spawn_data_grid(commands, parent, grid, height)`): a header row of buttons over a virtualised body
  whose rows have one clipped cell per column. Clicking a heading sorts by that column (numbers numerically, then
  text case-insensitively; again flips the direction) and the heading shows ▲ or ▼. Drive it with `ui.data_grid`.
- **Color picker** (`spawn_color_picker(commands, parent, picker, size)`): a saturation/value square (the pure hue
  under a white-to-clear and a clear-to-black gradient), a six-segment hue bar, an alpha bar, marker rings, a swatch
  and a `#RRGGBB[AA]` label. Pressing or dragging in a part picks there. Drive it with `ui.color_picker` on the
  picker entity ([UI color picker](ui-color-picker.md)).
- **Separator** (`spawn_separator(commands, parent, vertical)`): a 1 px rule with the `separator` class.
- **Search box** (`SearchBox { target, items, limit }` next to a `TextInput`): the typed query filters `items` into the
  target list's rows with the fuzzy ranker (best first; an empty query lists every item in order); `shown` maps each
  row back to its item. It reranks when the query, the item list, or the limit changes, not on each caret blink.
  Drive it with `ui.search_box`.
- **Progress dialog** (`spawn_progress_dialog(commands, parent, title, cancellable)`): a modal panel with the title, a
  progress bar, a status line and an optional Cancel button. Set `ProgressDialog::progress` and `status`; Cancel sets
  `cancelled` and closes the modal. Drive it with `ui.progress_dialog`.

## Methods

- `ui.data_grid { entity, action?, columns?, rows?, column?, offset?, limit? }`: `entity` is the grid's list node or
  a drawn grid's outer node. `action: "query"` (default) returns the columns, the sort, the header labels and a page
  of rows in display order; `"set"` replaces `columns` (`[{ title, width }]`, width 8 to 2000 UI px, at most 64)
  and/or `rows` (arrays of strings, at most 100 000), creating the grid when the node has none (a drawn grid keeps
  its column count); `"sort"` sorts by `column`, and the same column again flips, as a header click does. `offset` and
  `limit` (default 100, most 500) page the reply. Mutating; permission `edit_scene`.

```json
{"method": "ui.data_grid", "params": {"entity": 4294967310, "action": "sort", "column": 1}}
```

Reply: `{ entity, columns, sort: { column, ascending } | null, total, headers, rows: [{ row, source, cells }], drawn }`.

- `ui.search_box { entity, query? }`: `query` sets the search text as typing would (at most 256 bytes). Returns
  `{ entity, query, target, total, shown: [{ index, label }] }`. Errors: `no_search_box`, `bad_query`. Mutating;
  permission `edit_scene`.
- `ui.progress_dialog { entity, progress?, status? }`: `progress` 0 to 1, `status` at most 512 bytes. Returns
  `{ entity, progress, status, cancelled, open }`. Errors: `no_progress_dialog`, `bad_param`. Mutating; permission
  `edit_scene`.

## Errors

| Code | Meaning |
|---|---|
| `bad_entity` | `entity` is not entity bits |
| `unknown_entity` | the entity does not exist |
| `bad_action` | `action` is not query, set or sort |
| `bad_grid` | the columns, rows or sort column are invalid, or a drawn grid would change its column count |
| `bad_param` | `offset` or `limit` is out of range |
| `no_grid` | the node has no data grid (create one with `set` and `columns`) |
| `no_search_box` | the node is not a search box (`ui.search_box`) |
| `bad_query` | `query` is not a string of at most 256 bytes (`ui.search_box`) |
| `no_progress_dialog` | the node is not a progress dialog (`ui.progress_dialog`) |

`offset` accepts 0 to 100,000 and `limit` accepts 0 to 500. For these direct handlers, `error.data.code` contains
the listed reason code: invalid request values use JSON-RPC `-32602`, while missing entities/widgets use `-32001`.

## Showcase

The `ui_gallery` Parity page shows a drawn tree (`gallery.parity.tree`), a unit table (`gallery.parity.grid`) and a
color picker (`gallery.parity.color_picker`) with separators between them, a search box over a command list
(`gallery.parity.search`, results in `gallery.parity.search_results`) and a progress dialog opened by the
"Progress dialog" button (`gallery.parity.progress`).

Workflow: find the grid node id with `ui.query`, set columns and rows with `ui.data_grid`, sort by a column, then
query an `offset`/`limit` page to confirm the displayed order.

Captured failure on lavapipe:

Request:

```json
{"jsonrpc":"2.0","id":5,"method":"ui.data_grid","params":{"entity":0,"action":"query"}}
```

Response:

```json
{"jsonrpc":"2.0","id":5,"error":{"code":-32602,"message":"`entity` must be entity bits from ui.query","data":{"code":"bad_entity","hint":"take entity ids from a ui.query result"}}}
```
