# Color-vision filters (agents)

ADR-2646. `regia_render::color_vision` filters the rendered image for color-vision deficiencies:

- **simulate**: show the image as a viewer with protanopia, deuteranopia or tritanopia sees it. A design check:
  turn it on and confirm that nothing important is told by color alone.
- **compensate**: shift colors the viewer cannot tell apart into ones they can (daltonization). A player option.

The engine-wide accessibility review is `docs/accessibility-review.md`.

## How it works

`ColorVisionFilter { mode, deficiency, severity }` (off by default). The simulation uses the published severity-1
matrices of Machado, Oliveira and Fernandes (2009) on linear RGB; a lower severity blends the matrix with the
identity. Compensation adds back the part of each color the viewer misses into channels they can see (after
Fidaner, Lin and Ozguven, 2005). Sources are in `docs/provenance.md`.

The filter is delivered as a 3D LUT through the post stack's existing LUT step (after the tonemap and the grade):
right after extraction, `apply_color_vision` composes the filter over each view's final LUT (the level look's LUT
and the post-volume blend are already in it) on that LUT's grid, or builds a 33³ table when the view has none. The
last four tables are cached by their inputs, so a steady frame composes nothing. Off (or severity 0), no view is
touched.

What it does not filter: the UI (drawn after the post stack), and views whose post stack is off.

## Persistence

`GameSettings.accessibility.color_vision` (missing in older files: off) is saved with the other settings and
copied into the `ColorVision` resource when the settings change.

## Method

- `render.color_vision { mode?, deficiency?, severity? }`. With no parameters it reports. A change sets the given
  fields and keeps the others, in `GameSettings` (persisted like `settings.set`) and in the live resource; the next
  frame's views carry the composed LUT. Returns `{ mode, deficiency, severity, active, views_last_frame, composed,
  cache_hits, live, persisted }`. Mutating; permission `edit_scene`.

| Parameter | Values |
|---|---|
| `mode` | `off`, `simulate`, `compensate` |
| `deficiency` | `protanopia`, `deuteranopia` (the default), `tritanopia` |
| `severity` | `0` (no effect) to `1` (full dichromacy, the default) |

```json
{"method": "render.color_vision", "params": {"mode": "simulate", "deficiency": "deuteranopia"}}
{"method": "seeing.screenshot", "params": {}}
{"method": "render.color_vision", "params": {"mode": "off"}}
```

## Errors

| Code | Meaning |
|---|---|
| `color_vision_bad_params` | The parameters are not `{ mode?, deficiency?, severity? }`, or a value has the wrong type |
| `color_vision_bad_mode` | `mode` is not `off`, `simulate` or `compensate` |
| `color_vision_bad_deficiency` | `deficiency` is not `protanopia`, `deuteranopia` or `tritanopia` |
| `color_vision_bad_severity` | `severity` is not a number from 0 to 1 |

## Limits

- The simulation models dichromacy; lower severities are a linear approximation, not the paper's per-severity
  matrices.
- Compensation helps tell colors apart; it does not restore them. Games still must not carry essential
  information by color alone (shape, icon or text as well).
