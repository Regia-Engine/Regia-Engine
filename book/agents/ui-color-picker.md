# UI color picker (agents)

ADR-273 W11, ADR-1842. Code: `regia_ui::color_picker`.

`ColorPicker` is the model of the usual picker: a saturation/value square, a hue bar and an alpha bar. It stores HSV so a
gray keeps its hue while the player drags in the square, and hex edits round-trip exactly for 8-bit sRGB colors
(`#RRGGBB`, or `#RRGGBBAA` when the quantized alpha byte is below 255). Pointer positions are fractions of the square or bar, so any pixel
size works. `color()` returns the color in the UI's linear space. `spawn_color_picker` spawns the drawn widget (a column with the
saturation/value square, 16 px hue and alpha bars, a swatch and a hex label) bound to a `ColorPickerView`, and `ui.color_picker`
drives it.

## Methods

- `ui.color_picker { entity, hex?, square?, hue?, alpha? }`: with only `entity` it reads (a node with no picker gets a
  white one). `hex` sets the color; `square: [x, y]` sets saturation (x) and value (1 - y); `hue` and `alpha` are bar
  fractions. Returns `{ entity, color: { hex, hue, saturation, value, alpha, square, hue_pos } }`. A failed call changes
  nothing. Mutating; permission `edit_scene`.

```json
{"method": "ui.color_picker", "params": {"entity": 4294967310, "hex": "#336699"}}
```

## Errors

| Code | Meaning |
|---|---|
| `bad_entity` | `entity` is not entity bits |
| `unknown_entity` | the entity does not exist |
| `bad_color` | `hex`, `square`, `hue` or `alpha` is malformed |

`entity` is required integer entity bits from `ui.query`. `hex` is an sRGB string in `#RRGGBB` or `#RRGGBBAA` form;
`square` is two numeric fractions `[x, y]`, and `hue` and `alpha` are numeric fractions; picker positions clamp to the
0–1 range. Updates are applied to a copy and committed only when all supplied values are valid.

## Failure example

Captured CLI output from `regia agent call ui.color_picker '{"entity":4294967310}'`:

```text
error: method call failed: the entity does not exist (-32001): {"code":"unknown_entity","hint":"take entity ids from a ui.query result"}
```

## Workflow

Get a picker node's entity bits from `ui.query`, read its current color with `ui.color_picker {"entity": ...}`, then
set `hex` or a bar fraction. Read it again and use the returned hex value for the displayed sRGB color.
