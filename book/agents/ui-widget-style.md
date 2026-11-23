# UI widget style: states, sizes and density

How a `regia_ui` widget decides what it looks like. This page is the model behind the `ui.theme_*`
methods ([UI themes](ui-theme.md); lane D7 of `docs/specs/ui-default-design.md`, § 6.6): `ui.theme_get`
with `type`, `state` and `size` returns the look the resolver gives such a node, and the `Themed`
component on a live UI node holds its resolved style.

## State order

For one property of one node, states compose in a fixed order and the later one wins:

`normal → size (sm / lg) → selected → hover → pressed → error → focused → disabled`

- `focused` adds the ring and nothing else by default, so focus never hides other state.
- `disabled` wins a contested property and suppresses hover and pressed. The focused ring still applies. Disabled applies last.
- Hover is skipped for touch input.
- `md` is the base; only `sm` and `lg` add a layer.

## What drives each state

| State | Source on the node |
|---|---|
| hover / pressed / focused | `Hovered` / `Pressed` / `Focused` markers (written by the input systems) |
| disabled | `Disabled` marker |
| selected | `Selected` marker (checked, open, current tab or row) |
| error | `Invalid` marker |
| size | `WidgetSize` component (`Sm`, `Md`, `Lg`); absent = the node keeps its authored size |

`UiDensity` (resource, `Comfortable` default, `Compact`) picks the size table: control height
32 / 44 / 56 vs 22 / 28 / 36, padding 12 / 16 / 24 vs 6 / 8 / 12 (`sm` / `md` / `lg`).

## Style properties

`VisualOverrides` carries `background`, `border_color`, `border_width`, `radius`, `text_color`,
`focus_ring`, `focus_ring_width`, `focus_ring_offset`, `padding_x`, `height`, `min_width`, `gap`,
`text_px`, `icon_color`, `elevation`, `opacity`, `translate`, `scale`. A `StyleClass` holds one set
per state (`base`, `sm`, `lg`, `selected`, `hover`, `pressed`, `error`, `focused`, `disabled`) plus
`parts`: named sub-elements (`thumb`, `track`, `fill`, `knob`, `indicator`, `row`, `item`) that each
have their own states. `ThemeClasses::resolve_part` resolves one part for a node.

## Reading the result

`apply_themes` writes `Themed` each frame: `background`, `border_color`, `border_width`, `radius`,
`text_color`, `focus_ring`, plus `opacity`, `height`, `padding_x`, `translate`, `scale`,
`focus_ring_width`, `focus_ring_offset`. A `wait_until` on the colour or opacity of a node is the
check that a state took effect.

## The disabled rule

Disabled widgets draw at `UiThemes::disabled_opacity` over their normal look (the spec value is
0.45, `DISABLED_OPACITY`) with the dimmed text colour. The field defaults to `DISABLED_OPACITY`;
1.0 turns the rule off.

## Not yet wired

`translate` and `scale` are resolved into `Themed` but not yet read by layout or extract (D6, D8). A theme's own
`height`, `padding_x` and `gap` reach layout (ADR-9340, [UI themes](ui-theme.md)); the `WidgetSize` token values in
`height` and `padding_x` stay resolved only. The focus ring colour, width and offset are drawn since D9: see
[UI focus ring](ui-focus-ring.md). `opacity` is read by extract today.

## Inspecting a resolved style

`ui.theme_get` accepts `theme` (defaults to the UI's resolved theme), `keys` (a token-key glob or list of globs, default
all tokens), and `resolved` (include resolved token values, default `true`). It can also take `type` (widget type or
variation, such as `Button.primary`), `state` (one or more of `normal`, `hovered`, `pressed`, `focused`, `disabled`,
`selected`, `error` or `touch`), `size` (`sm`, `md` or `lg`) and `density` (`comfortable` or `compact`). Its
`resolved.props` contains the values to compare with the state rules above. The handler reports `ui_theme_unknown` or
`ui_theme_type_unknown` with `F0003`, `ui_bad_param` or
`ui_unknown_state` with `F0002`, and `ui_not_installed` with `F0013` when runtime UI is unavailable.

Workflow: query `ui.theme_get` for `Button.primary` in `hovered` state at `md` size, focus or hover the matching node
through the registered UI input method, then call `seeing.screenshot {}` to review the rendered state.

Captured failure on lavapipe:

Request:

```json
{"jsonrpc":"2.0","id":2,"method":"ui.theme_get","params":{"type":"__capture_missing_widget_type__"}}
```

Response:

```json
{"jsonrpc":"2.0","id":2,"error":{"code":-32001,"message":"theme `regia-dark` defines no widget type or variation `__capture_missing_widget_type__`","data":{"code":"F0003","hint":"ui.theme_get without `type` lists the theme; types live under `types` and `variations` in the file","reason":"ui_theme_type_unknown"}}}
```
