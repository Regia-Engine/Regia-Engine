# UI accessibility settings and right-to-left layout (agents)

ADR-273 lane 2, ADR-341.

## Settings

`GameSettings.accessibility` has three UI fields; the framework copies them into the `regia_ui::UiAccessibility`
resource whenever the settings change.

| Field | Meaning |
|---|---|
| `text_scale` | Multiplier on every text size (`1.0` = as authored). The UI limits it to 0.5 through 3.0. The device text floor (`UiDevicePolicy::min_text_px`, 12 px on the Deck) still applies afterwards. |
| `reduced_motion` | Animations jump to their end value: tweens, theme cross-fades (a theme swap applies at once; copied into `ActiveTheme`, UI D15) and [motion presets](ui-motion.md) (presets keep a short fade). The same flag also stops framework camera shake, handheld sway and walk bob, and forces motion blur off (ADR-3464, [accessibility](../user-guide/accessibility.md)). |
| `high_contrast` | The theme switches to its high-contrast variant (`<theme>-high-contrast`, else `regia-high-contrast`; UI D15, [theme switch](ui-theme-switch.md)). `regia-light` has `regia-light-high-contrast`, so a light-scheme player stays light. |
| `color_scheme` | `GameSettings` only: `None` (default) leaves the theme to the game; `Some(Dark)`, `Some(Light)` or `Some(System)` sets `ThemeMode::follow` ([theme switch](ui-theme-switch.md)). |

`reduced_motion`, `high_contrast` and `color_scheme` are missing in older settings files and load as `false` /
`None`. Changing `text_scale` re-shapes all text and re-runs layout on the next frame.

Set them with `settings.set` (`accessibility.text_scale`, `accessibility.reduced_motion`,
`accessibility.high_contrast`, `accessibility.color_scheme`) or with `ui.accessibility` (all but `color_scheme`).

## Right-to-left layout

A node's `text_dir` (`auto`, `ltr`, `rtl`) now decides its layout direction. `rtl` always mirrors, `ltr` never does,
and `auto` inherits its parent's direction; a root with `auto` mirrors when the active locale is a right-to-left
language (ar, he, fa, ur, ps, sd, yi, dv, ug, ckb). A mirrored node lays out with CSS `direction: rtl` (rows run from
the right, `start` is the right edge, grid columns run right to left) and the left and right edges of `margin`,
`padding`, `border` and `inset` swap (ADR-518). Switching the locale re-lays out the tree. `ui.query` reports each
node's direction as `layout_dir`. The details are in [UI layout edge cases](ui-layout.md).

## Methods

- `ui.accessibility { text_scale?, reduced_motion?, high_contrast? }`. With no parameters it reports. With any of them it
  changes those fields in the live resource, and in `GameSettings` only when that resource is already in the world
  (`persisted` is false otherwise), then reports.
  Returns `{ text_scale, effective_text_scale, reduced_motion, high_contrast, locale, locale_rtl, persisted }`.
  Mutating; permission `edit_scene`.

A partial update starts from `GameSettings.accessibility` when settings are installed, preserving
omitted fields, and writes the same three values to settings and the live resource. Without settings,
it starts from the live resource, or the defaults when the live resource is absent.

```json
{"method": "ui.accessibility", "params": {}}
{"method": "ui.accessibility", "params": {"text_scale": 1.5, "reduced_motion": true}}
```

## Errors

| Code | Meaning |
|---|---|
| `ui_bad_text_scale` | `text_scale` is not a number from 0.5 to 3.0 |
| `ui_bad_flag` | `reduced_motion` or `high_contrast` is not a boolean |
