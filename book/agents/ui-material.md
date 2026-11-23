# UI materials (agents)

ADR-273 T09, ADR-2045. Code: `regia_ui::ui_material`.

A `UiMaterial` component gives a UI node a procedural fill, drawn over its background and under its image, text,
children and border, inside its rounded corners. A material is one of a small set of built-in effects with two colors
and four numbers, not custom shader code: each kind is one branch of the UI overlay shader (kind 8), so a material
needs no pipeline, bind group or draw call of its own and batches with the rest of the UI. Nodes without a material
draw as before.

| Kind | Look | `params` [0, 1, 2, 3] |
|---|---|---|
| `shine` | a soft band sweeping across (button highlight) | speed (sweeps per s), band width (fraction), angle (degrees), pause (fraction of a cycle) |
| `radial_wipe` | `color2` fills a clockwise sector from 12 o'clock (cooldowns, radial progress) | fraction (0 to 1), edge softness (radians), -, - |
| `dissolve` | value noise against a threshold; pixels under it vanish, the edge takes `color2` | threshold (0 to 1), cell size (px), edge width (noise units), - |
| `scanlines` | horizontal bands | period (px), scroll speed (px per s), strength (0 to 1), - |
| `pulse` | `color` to `color2` and back | period (s), -, -, - |

`shine`, `scanlines` and `pulse` use time. With `animate` on, `tick_ui_materials` advances `time` from the game clock
(paused clock, paused material) and the node redraws each frame; static kinds never mark their node changed.
`regia_ui::ui_material::material_mix` is the CPU model of the shader's mix for every kind but `dissolve` (whose noise is
only defined on the GPU).

## Methods

- `ui.material { entity, kind?, color?, color2?, params?, time?, animate?, remove?, probe? }`: with only `entity` it
  reads (`material` is null when the node has none). The other parameters edit; a new `kind` starts from its default
  parameters unless the call sets `params`; `remove: true` takes the material off. `probe: [x, y]` (px from the node's
  top-left) returns `mix`, the 0-to-1 blend from `color` to `color2` the shader computes there (null for `dissolve`).
  `entity` is the required id from `ui.query`; `params` is exactly four finite numbers (absolute value at most
  1,000,000), `time` is 0 to 3,600 seconds, and `probe` is two finite pixel coordinates. Colors accept `#RRGGBB[AA]`
  or four finite linear RGBA channels.
  A failed call changes nothing. Mutating; permission `edit_scene`.

```json
{"method": "ui.material", "params": {"entity": 4294967310, "kind": "radial_wipe", "params": [0.4, 0.02, 0, 0],
  "color": "#1F2937", "color2": "#60A5FA", "probe": [40, 4]}}
```

Reply: `{ entity, material: { kind, color, color2, params, time, animate } | null, mix, kinds }`.

## Errors

| Code | Meaning |
|---|---|
| `bad_entity` | `entity` is not entity bits |
| `unknown_entity` | the entity does not exist |
| `bad_material` | `kind` is not one of the kinds above |
| `bad_color` | `color` or `color2` is not `#RRGGBB[AA]` or `[r, g, b, a]` |
| `bad_param` | `params`, `time` (0 to 3600), `animate`, `remove` or `probe` is malformed |

`error.data.code` carries the lower-case reason from the table; invalid values use JSON-RPC `-32602` and a missing
entity uses `-32001`.

## Showcase

The `ui_gallery` Parity page shows one swatch per kind (nodes `gallery.parity.material.<kind>`).

Workflow: use `ui.query` to find a gallery swatch, set its kind and two colors with `ui.material`, then read the node
back and probe a point inside its bounds to inspect the reported mix.

Captured failure on lavapipe:

Request:

```json
{"jsonrpc":"2.0","id":8,"method":"ui.material","params":{"entity":0}}
```

Response:

```json
{"jsonrpc":"2.0","id":8,"error":{"code":-32602,"message":"`entity` must be entity bits from ui.query","data":{"code":"bad_entity","hint":"take entity ids from a ui.query result"}}}
```
