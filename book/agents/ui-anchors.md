# UI anchor presets (agents)

ADR-273 Y01, ADR-1842. Code: `regia_ui::anchors`.

An anchor preset places an out-of-flow node at an edge, corner or the centre of its parent with one name. It sets
`position: absolute` and the `inset` (plus a margin that centres the node on a centred axis), so the layout engine
needs nothing new and the placement holds at every resolution and UI scale. Right-to-left layout mirrors the insets
like any other node.

| Preset | Placement |
|---|---|
| `top_left`, `top_center`, `top_right` | top edge |
| `center_left`, `center`, `center_right` | middle band |
| `bottom_left`, `bottom_center`, `bottom_right` | bottom edge |
| `top_wide`, `bottom_wide` | spans the width at the top or bottom; width is cleared |
| `left_wide`, `right_wide` | spans the height at the left or right; height is cleared |
| `vcenter_wide`, `hcenter_wide` | spans one axis, centred on the other |
| `full_rect` | fills the parent |

`offsets` are `[left, top, right, bottom]` distances from the parent's edges in UI px. On a centred axis they shift the
node (`left - right`, `top - bottom`). A centred axis needs a fixed size on that axis (`Points`, or `Percent`, which is
centred exactly).

## Methods

- `ui.set_anchor_preset { entity, preset, offsets? }` applies the preset to the node's style and returns
  `{ entity, preset, inset, pivot_ratio }`. `pivot_ratio` is the pivot that makes a scale transform grow away from the
  anchor. Mutating; permission `edit_scene`.

```json
{"method": "ui.set_anchor_preset", "params": {"entity": 4294967310, "preset": "bottom_right", "offsets": [0, 0, 16, 16]}}
```

## Errors

| Code | Meaning |
|---|---|
| `bad_entity` | `entity` is not entity bits |
| `unknown_entity` | the entity does not exist or has no node style |
| `bad_preset` | `preset` is missing or not one of the names above |
| `bad_offsets` | `offsets` is not four numbers |
| `size_required` | a centred axis has an `auto` size |

`entity` is an integer id from `ui.query`; `preset` is one of the listed strings. Optional `offsets` is a four-number
array `[left, top, right, bottom]` in UI pixels, defaulting to `[0, 0, 0, 0]`. A centred axis requires an explicit
point or percent size before applying the preset.

## Failure example

Captured CLI output from `regia agent call ui.set_anchor_preset '{"entity":4294967310,"preset":"diagonal"}'`:

```text
error: method call failed: `preset` is missing or unknown (-32602): {"code":"bad_preset","hint":"use one of: top_left, top_center, top_right, center_left, center, center_right, bottom_left, bottom_center, bottom_right, top_wide, bottom_wide, left_wide, right_wide, vcenter_wide, hcenter_wide, full_rect"}
```

## Workflow

Choose a node from `ui.query`, set its width and height when the chosen preset centers either axis, then apply the
preset and optional edge offsets. Query the node again after layout to check the resulting rectangle at the target
resolution.
