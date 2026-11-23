# UI lists and scrollbars (agents)

Guide topic: `agent.guide {"topic": "ui_list"}`. ADR-273 lane 8, ADR-344.

## Lists

A list (`ListView` with `ListItems` or `ItemCount`) owns its data. Only the visible rows exist as entities, plus
the overscan, so 10 000 items cost a few dozen nodes. Row `i` always lives in pool slot `i % pool_len`: scrolling by
one row rebinds one entry and spawns nothing. Each rebind writes a `ListEntryBind { list, row, index }` message, so a
game can fill custom entries (mark the list `CustomEntries` to switch the default label off).

Optional components on the list entity:

| Component | Effect |
|---|---|
| `ListHeights(Vec<f32>)` | Per-item row heights (UI px); items past the end use `ListView::item_height`. A prefix-sum table makes the row at an offset a binary search. |
| `ListTiles { tile_size, gap, columns }` | Tile mode: items wrap into columns (`columns: None` fits as many whole tiles as the width allows). |
| `ListSelection::new(mode)` | `none`, `single` or `multi`. Click selects, Ctrl toggles, Shift extends from the anchor. Rows of selected items carry the `Selected` marker. |
| `ListHeaders(Vec<usize>)` | Items that are section headers (ascending). The header of the section at the top sticks there until the next header pushes it out. Rows only; not in tile mode. |
| `ScrollToItem { index, align }` | A one-shot request; `ui.list`/`ui.scroll_to` insert it. |

Keyboard and gamepad: arrow keys and the d-pad move focus between rows as before and the list cursor follows (with
Shift held in a multi list the selection extends). Home, End, PageUp and PageDown jump (the page is the number of
items in the viewport), Ctrl+A selects all in a multi list, and typing letters jumps to the next item whose label
starts with them (case-insensitive; the typed prefix expires after one second of game time). Repeating one letter
cycles through the matches.

Messages: `ListSelectionChanged { list, selected }`, `ListActivated { list, index }` (double-click).

## Scrollbars

Add `Scrollbars` to a scroll view (or list). It draws a track and a draggable thumb on the vertical edge (and the
horizontal edge with `horizontal: true`) when the content overflows. Dragging the thumb scrolls; a click on the track
centers the thumb on the click. `auto_hide: Some(seconds)` hides the bars after that long without scrolling or
pointer contact. The bars are siblings of the scroll view, so the view needs a parent node that does not scroll
itself. They carry the theme classes `scrollbar-track` and `scrollbar-thumb`; set `track_color` / `thumb_color` to
`None` to leave the colors to the theme. A scroll view without `Scrollbars` keeps its thin, non-interactive thumb.

Focus moving to a node inside a scroll view scrolls the node into view on both axes, once per focus move.

## Methods

- `ui.list { entity, action?, ... }` where `entity` is the node id from `ui.query` (`node` works as an alias).
  - `action: "info"` (default): `{ entity, count, range: [start, end], live_rows, spawned_rows, rebinds, total_height,
    offset, layout, selection: { mode, selected, cursor }, headers }`. `count` is the current item count
    (`ListItems`, or `ItemCount` when there are no items) and `total_height` is that count's content height,
    including before the first virtualise pass. `layout` is `rows`, `variable_rows` or `tiles`;
    `range` is the half-open range of items with live rows, and it stays empty until a virtualise pass
    lays the list out.
  - `action: "items"` with `start` (default 0) and `limit` (default 100, at most 500): `{ entity, start, items: [{ index,
    label, selected }] }`.
  - `action: "select"` with optional `mode` (`none`, `single`, `multi`: adds a selection model when the list has none),
    `op` (`set` default, `add`, `toggle`, `range` from the first to the last index, `all`, `clear`) and `indices`
    (or one `index`). Returns the same shape as `info`. Applies at once. A call with only `mode` changes the mode and
    keeps the selection.
  - `action: "scroll_to"` with `index` and `align` (`nearest` default, `start`, `center`, `end`): `{ entity, index,
    queued: true }`. The scroll applies in the next frame's layout pass.
- `ui.scroll_to { entity, index? | reveal? | x?, y? | fraction?, align? }` scrolls a list or any scroll view. `index`
  is a list item (queued, as above). `reveal` is a node id to bring into view. `x` and `y` are absolute offsets in
  physical pixels; `fraction` is the vertical position from 0 (top) to 1 (end). Offsets clamp to the content. Returns
  `{ entity, offset: [x, y], max: [x, y] }` and applies at once.

Both are mutating (permission `edit_scene`).

```json
{"method": "ui.list", "params": {"entity": 4294967301}}
{"method": "ui.list", "params": {"entity": 4294967301, "action": "select", "mode": "multi", "op": "range", "indices": [3, 9]}}
{"method": "ui.list", "params": {"entity": 4294967301, "action": "items", "start": 100, "limit": 20}}
{"method": "ui.scroll_to", "params": {"entity": 4294967301, "index": 5000, "align": "center"}}
{"method": "ui.scroll_to", "params": {"entity": 4294967310, "fraction": 1.0}}
```

## Errors

| Code | Meaning |
|---|---|
| `not_a_list` | The entity exists but is not a `ListView`. |
| `unknown_entity` | The entity (or `reveal` node) does not exist. |
| `index_out_of_range` | An item index is past the last item. |
| `no_selection` | `select` on a list without a selection model (or with mode `none`); pass `mode`. |
| `not_scrollable` | `ui.scroll_to` on an entity without a scroll offset. |

Bad or missing parameters answer `F0002` as for the other `ui.*` methods.

`unknown_entity`, `not_a_list` and `not_scrollable` are the listed `error.data.code` reasons on JSON-RPC `-32001`;
`index_out_of_range` and `no_selection` are `error.data.code` reasons on `-32602`. `indices` is limited to 4096
entries, and `align` is `nearest`, `start`, `center` or `end`.

Workflow: find a `ListView` with `ui.query`, read its `count` and `range` with `ui.list`, fetch a short item window,
then call `ui.scroll_to` for an item using `align:"center"` and inspect the next frame.

Captured failure on lavapipe:

Request:

```json
{"jsonrpc":"2.0","id":7,"method":"ui.list","params":{"entity":0}}
```

Response:

```json
{"jsonrpc":"2.0","id":7,"error":{"code":-32602,"message":"`entity` (or `node`) must be the node id from ui.query","data":{"code":"F0002","hint":"check the method's parameters in the agent guide"}}}
```
