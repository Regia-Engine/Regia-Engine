# UI layout debug overlay (agents)

ADR-273 U02, ADR-1842. Code: `regia_ui::debug_overlay`.

The overlay draws extra quads on top of the UI, so a screenshot shows it. It needs no shader and costs nothing while it is
off. With `outlines` every laid-out node gets a one-pixel outline. For the `highlight` node the margin box is tinted
orange, the padding box green and the content box blue, and the border box is outlined in white. Margin, border and
padding come from the node's style when they are fixed lengths (percent and auto count as zero) and are scaled by the
UI scale the node was laid out at.

With `focus_graph` (ADR-2045) the overlay also draws the focus graph of the active scope: every focus candidate is
outlined in cyan, and from the focused node a connector (a horizontal then a vertical bar, with a square on the target)
runs to where each arrow press would go: up green, down orange, left magenta, right blue. Remembered group children are
not used, so the overlay shows the geometric graph. It draws under the master switch `enabled`.

## Methods

- `ui.debug_overlay { enabled?, outlines?, highlight?, focus_graph? }` changes the overlay and requests a UI redraw.
  With no parameters it reports. `highlight` is entity bits from `ui.query`; `null` clears it. Returns
  `{ enabled, outlines, highlight, boxes, focus_graph, focus }` where `boxes` is `{ margin, border, padding, content }`,
  each `{ x, y, w, h }` in physical px, or `null` when nothing is highlighted, and `focus` (with the focus graph on) is
  `{ focused, candidates, edges: [{ direction, entity }] }`. Mutating; permission `edit_scene`.

```json
{"method": "ui.debug_overlay", "params": {"enabled": true, "outlines": true, "highlight": 4294967310}}
```

## Errors

| Code | Meaning |
|---|---|
| `bad_flag` | `enabled`, `outlines` or `focus_graph` is not a boolean |
| `bad_entity` | `highlight` is not entity bits |
| `unknown_entity` | the highlighted entity does not exist |

`enabled`, `outlines` and `focus_graph` are optional booleans. `highlight` is optional integer entity bits; `null`
clears it. `boxes` is `null` until the highlighted node has computed layout. The overlay's `enabled` master switch
must be true for either outlines or the focus graph to be drawn.

## Failure example

Captured CLI output from `regia agent call ui.debug_overlay '{"enabled":"yes"}'`:

```text
error: method call failed: `enabled` must be true or false (-32602): {"code":"bad_flag","hint":"pass a JSON boolean"}
```

## Workflow

Enable `outlines` and `focus_graph`, set `enabled` to true, and capture a screenshot to inspect layout and navigation.
Set `highlight` to a node id from `ui.query` to see its box model; pass `null` to clear the highlight, then disable the
overlay when done.
