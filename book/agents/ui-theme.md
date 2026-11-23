# UI themes (agents)

A UI theme is a `.theme.ron` file of design tokens: colors, spacing, radius, type scale, sizes, elevation and
motion, plus per-widget looks. A theme can extend another, so a game re-skins the default by listing only what
changes. Format: `docs/formats/theme-ron.md`. Design: `docs/specs/ui-default-design.md`.

Built-in themes: `regia-dark` (default), `regia-light`, `regia-high-contrast`, `regia-light-high-contrast`
(`crates/regia_ui/assets/themes/`). Games put theme files in `assets/ui/themes/`; `UiPlugin` loads them at
startup (parents before children). `regia upgrade-project --write` rewrites old `UiTheme`-shaped `.theme.ron`
files as v1.

## Methods

Every method goes through the runtime UI's agent hook (`UiAgentHost`, the same one as [ui](ui.md)), so an app
without `regia_ui` answers `ui_not_installed`. Reads need `read`; `ui.theme_set`, `ui.theme_select` and
`ui.theme_reload` need `edit_scene`; `ui.theme_set { persist: true }` also needs `assets` (it writes a file).
Writes land on the **next frame** as one theme generation, with the colors cross-fading over `motion.base` (instant
with reduced motion). Wait for them with `wait_until` (below).

When `theme` is omitted, a method uses the theme the UI resolves against (after the high-contrast and fallback
rules), as `ui.theme_list` reports in `resolved`.

### `ui.theme_list`

No parameters. Returns every loaded theme `{ name, extends, scheme, density, path, builtin, error }` (`path` is the
file it came from, `error` the last load error `{ key, message }`), then `active` (the selected name), `resolved`,
`density`, `high_contrast`, `reduced_motion`, `generation`, `error` (why the selected theme could not be resolved),
`files` (every watched file `{ path, theme, loads, error }`, including files that never loaded) and `watching`.
`slots` lists the named token slots widgets read (see "Token slots" below) as `{ key, kind, used_by, fallback,
value }`, `value` being the resolved theme's value or null when it leaves the slot to the engine fallback.

### `ui.theme_get`

| Param | Type | Meaning |
|---|---|---|
| `theme` | string | Theme name. |
| `keys` | string or list | Globs over token keys (`*` any run, `?` one character), e.g. `"color.*"`. Default: every token. |
| `resolved` | bool | Include resolved values (default `true`). |
| `type` | string | A widget type or variation, e.g. `"Button.primary"`. |
| `state` | string or list | With `type`: `normal`, `hovered`, `pressed`, `focused`, `disabled`, `selected`, `error`, `touch`. |
| `size` | string | With `type`: `sm`, `md` (default), `lg`. |
| `density` | string | `comfortable` or `compact` (default: the active density). |

Returns `{ name, extends, format_version, scheme, density, chain, tokens, rules, types?, resolved? }`. `rules` lists
the chain's style rules (`.theme.ron` v2), root theme first, as `{ selector, specificity, state, props }`: `selector`
is display text that is never parsed back (`Toolbar Button`, `[danger-zone] :hover`, `Button.primary
:focus_visible`), `specificity` the `[variation + classes, widget + inside]` pair the resolver sorts by, `state` the
rule's state or null, `props` the raw properties. Each token is
`{ raw, from, density?, value }`: the expression as written (`"lighten(@color.accent, 0.06)"`), the theme in the chain
that defines it (`density` when a density overlay does), and the resolved value (colors as `#rrggbb[aa]`, durations
as `"80ms"`, easings as written). With `type`, `types` holds the authored definitions of the type's variation chain
root first, and `resolved` is `{ type, chain, state, size, props }`: the look the resolver gives a node of that
type in that state (background, border, radius, text color, focus ring, opacity, height, padding_x, `padding`
as `{ left, top, right, bottom }`, `min_width`, `min_height`, `max_width`, `max_height`, translate, scale, ...),
computed by the same resolver the live UI uses.

### `ui.theme_set`

| Param | Type | Meaning |
|---|---|---|
| `theme` | string | Theme to change. |
| `tokens` | object | Token key to a number or a string expression; `null` removes the key so the parent's value applies. |
| `types` | object | Type name to `{ extends?, props?, hover?, pressed?, focused?, focus_visible?, disabled?, selected?, error?, sizes? }`; each layer merges per property (`null` removes a property, a `null` type removes the type). `focus_visible` (v2) applies over `focused` only for keyboard or gamepad focus. |
| `variations` | object | Variation name (`"Button.primary"`) to the same shape. |
| `reset` | bool | Start from the theme's original (its last good file, or the built-in) before the edits. |
| `persist` | bool | Also write the theme's `.theme.ron` (needs `assets`). |

The whole theme chain is checked before anything changes: a bad value fails with `ui_theme_invalid` and the key
path at the start of the message (`tokens.color.accent-hover: unknown reference ...`), and the live theme is
untouched. On success it returns `{ theme, changed, warnings, generation, applies_at_generation, persisted }`:
`changed` lists key paths (`tokens.color.accent`, `types.Button`), `warnings` the lint findings of the new theme
(same shape as `ui.theme_lint`), and `applies_at_generation` the generation that shows the change (equal to
`generation` when nothing changed). `persist` writes the file atomically (temporary file, then rename) through the
same serializer the format uses, so it reloads to the same theme; comments in the file are not kept. Only themes
loaded from a file can be persisted; built-ins are engine assets.

### `ui.theme_select`

| Param | Type | Meaning |
|---|---|---|
| `theme` | string, required | Theme to select. |
| `density` | string or null | `comfortable`, `compact`, or `null` to follow `UiDensity` and the theme. |
| `high_contrast` | bool | Use `<theme>-high-contrast` when loaded, else `regia-high-contrast`. |
| `reduced_motion` | bool | Swap at once instead of cross-fading. |

Returns `{ previous, selected, generation, applies_at_generation }`; `previous` and `selected` are
`{ theme, density, high_contrast, reduced_motion }`.

### `ui.theme_preview`

| Param | Type | Meaning |
|---|---|---|
| `theme` | string | Theme to show. |
| `tokens`, `types`, `variations` | object | Overrides for the preview only, same shape as `ui.theme_set`. |
| `page` | string | `"theme-sheet"` (default), an `assets://` path or a file path of a `.ui.ron`. |
| `size` | `[w, h]` | Pixels, 1 to 4096 each (default `[1280, 720]`). |
| `scheme` | string | `dark` or `light`: shows the theme's `-dark`/`-light` sibling, else the built-in of that scheme. |
| `density` | string | `comfortable` or `compact`. |
| `device` | string | Safe-area profile for `SafeAreaPadding` roots: `desktop` (default, no inset), `tv` (5 % title-safe), `phone` (the portrait or landscape render-out profile). |

The preview runs in a separate headless UI app: the overrides are never applied to the live UI and the theme
generation does not move. The page is laid out for `size` through the render-out path (ADR-202), drawn into an
offscreen target and returned as `{ width, height, theme, page, scheme, density, device, generation, mime,
png_base64, path }`; `path` is the PNG written under the scratch output directory — the system
temp directory (`regia-theme-preview/`) on desktop, the app-private data directory on Android
(the system temp directory is not writable for an app there).

The theme sheet shows every button variation (`button`, `primary`, `danger`, `ghost`) in every state (normal,
hovered, pressed, focused, selected, disabled), toggles off and on, a slider, a progress bar, list rows (normal,
hovered, selected) and body text on surfaces.

### `ui.theme_lint`

`{ theme? }`. Returns `{ theme, scheme, density, clean, findings }`; each finding is `{ kind, key, message, pair?,
ratio?, required?, value? }` with `key` the key path at fault:

| `kind` | What |
|---|---|
| `contrast` | A text or boundary pair below its WCAG 2.x contrast ratio, in the theme's scheme. `pair` is `[foreground, background]`, `ratio` the measured ratio, `required` 4.5 (text) or 3 (boundaries, slider fill, focus ring). Pairs: text, muted text (also on the overlay surface in dark themes and the sunken one in light themes), on-accent on accent, on-danger on danger, accent, danger, warning, success and border-strong on surface, focus ring on background. Translucent colors are composited first. Disabled text is exempt (WCAG 1.4.3). |
| `unknown_reference` | A reference no theme in the chain defines. |
| `reference_cycle` | References that loop. |
| `unreachable_reference` | References cut off by a missing `extends` parent, or a variation whose base does not exist. |
| `unknown_key` | A token key or widget property the engine does not define (kept in the file). |
| `touch_target` | `size.control.md`, `size.control.lg`, `size.row` or an interactive type's `height` under `size.touch-min` (`required`), checked in the `comfortable` density only: `compact` is pointer-only. |

### `ui.theme_reload`

`{ theme? }`: re-reads the file a theme came from, or every theme file. Returns `{ reloaded: [{ theme, path,
changed }], failed: [{ theme, path, key, message }], skipped: [{ theme, reason }], generation,
applies_at_generation }`. A built-in or a theme set in code is `skipped` with reason `built_in` or `no_file`.

## Waiting for a change

`play.wait_until` and `play.expect` take `{ "predicate": "ui.theme_generation", "at_least": n }` (or `op` and
`value`, with `op` one of `gte` (default), `gt`, `eq`, `lt`, `lte`). Take `n` from `applies_at_generation`:

```json
{"method": "ui.theme_set", "params": {"tokens": {"color.accent": "#e8793a"}}}
{"method": "play.wait_until", "params": {"condition": {"predicate": "ui.theme_generation", "at_least": 7}}}
```

## Hot reload

In the editor and dev builds (`regia run --dev`, whenever the asset watcher runs), a saved `.theme.ron` that a
theme was loaded from is re-read within the watcher's look interval (every 30 frames by default): a good file swaps
in with a cross-fade, and every theme that extends it follows. A file that does not parse or resolve keeps the last
good theme, logs one warning with the key path, shows in the editor's watch status line and is reported by
`ui.theme_list` and `ui.theme_reload`. Shipping builds run no watcher and read themes once. Rust hosts and tests
control it through the `ThemeFiles` resource (`watch: Auto | On | Off`, `poll_frames`) and load extra files with
`regia_ui::theme_agent::load_theme_file`.

## Error reasons

Errors carry `data.reason`:

| Reason | When |
|---|---|
| `ui_not_installed` | The app has no runtime UI hooked to the agent. |
| `ui_bad_param` | A parameter has the wrong type or value (the message names it). |
| `ui_unknown_state` | `state` names no widget state. |
| `ui_theme_unknown` | No theme of that name is loaded (the hint lists the loaded ones). |
| `ui_theme_required` | `ui.theme_select` without `theme`. |
| `ui_theme_invalid` | The change would not load: the message starts with the key path; nothing was applied. |
| `ui_theme_type_unknown` | `ui.theme_get` `type` names no type or variation of the theme. |
| `ui_theme_resolve_failed` | The resolver could not run for `ui.theme_get` (a bug; report it). |
| `ui_theme_no_original` | `reset` on a theme that came from neither a file nor the built-ins. |
| `ui_theme_not_persistable` | `persist` on a theme without a file (built-ins, themes set in code). |
| `ui_theme_write_failed` | The file could not be written; nothing was applied. |
| `ui_theme_page_not_found` | `ui.theme_preview` `page` cannot be read. |
| `ui_theme_page_invalid` | The page is not a loadable `.ui.ron`. |
| `ui_theme_preview_no_gpu` | No GPU adapter for the offscreen preview (install lavapipe). |
| `ui_theme_preview_failed` | The offscreen readback failed. |
| `editor_headless` | `ui.theme_preview` on a host that opened no GPU on purpose (the headless editor's `--no-gpu`, or none found there): the host's own code, and no preview device is opened (ADR-5180). The other `ui.theme_*` methods still answer. |

A `persist` without the `assets` permission is the protocol's permission error (`F0012`).

## Re-skinning in a file

```ron
(
    format_version: 1,
    name: "my-game",
    extends: Some("regia-dark"),
    tokens: { "color.accent": "#e8793a", "color.on-accent": "#140a04", "radius.md": 3 },
)
```

Hover, pressed, selection and focus colors follow the accent because the parent derives them from it. A bad value
is reported as `theme "my-game": tokens.color.accent: ...` with the key path, and the previous theme stays active.
The same edit through the agent: `ui.theme_preview { tokens: { "color.accent": "#e8793a" } }` to look,
`ui.theme_lint` to check, then `ui.theme_set { theme: "my-game", tokens: {...}, persist: true }`.

## Runtime resolution (lane D6)

The running UI resolves every classed node from the active theme (ADR-289):

- `ActiveTheme` (resource) selects the theme by name, plus `density`, `high_contrast` and `reduced_motion`. A write
  swaps the theme next frame and cross-fades colors over `motion.base`. `ui.theme_select` writes it.
- A node's classes pick widget types and variations: `button` is `Button`, `primary` is `Button.primary`, `surface`
  is `Panel.surface`; a class may name a type directly (`"Button.ghost"`). For each property the first hit wins: the
  node's `ThemeOverride`, the matching style rules (most specific first), the variation chain nearest first, the
  type, the theme defaults. Disabled applies last at 0.45 opacity. A `.ui.ron` button widget puts its own `button`
  class first, so an authored `classes: ["primary"]` shows `Button.primary` (ADR-8303).
- Style rules (`.theme.ron` v2, ADR-8105 (e), ADR-8303) are structured selectors: `inside` (an ancestor's widget
  type, any depth), `widget`, `variation` (needs `widget`), `classes` (all of them) and `state`. They apply in
  ascending specificity `(variation + classes, widget + inside)`, then parent before child theme and file order.
  `ui.inspect` names a winning rule as `rule <selector>` and lists the node's matching rules under `theme.rules`;
  the UI designer's Style section shows the same list. Format: `docs/formats/theme-ron.md`.
- `focus_visible` layers (v2) show only while the focus came from the keyboard or a gamepad (`FocusRingPolicy`, by
  default `navigation_only`): Tab shows them, a click hides them and keeps the focus, in the running game and in the
  UI designer's live preview.
- `ThemeScope { theme }` on a node themes its subtree with another loaded theme.
- `ThemeResolver::stats()` reports the theme generation and how many nodes and styles were resolved last frame; an
  idle frame reports zero. `ui.theme_generation` reads the same generation.

## Motion (lane D8)

A widget type's `transition` names a duration and an easing, both usually tokens:
`"transition": ["@motion.fast", "@ease.standard"]`. The resolver animates the fill over that duration and shapes the
progress with the easing curve (`cubic-bezier(x1, y1, x2, y2)` as in CSS, or `linear`; an entry that is not an easing
means linear). A theme swap cross-fades every color over `motion.base` on a linear curve.

**Reduced motion.** `ActiveTheme { reduced_motion: true, .. }` makes state transitions (hover, press, focus, selected)
jump to their end value and a theme swap apply at once. Opacity fades, when a widget has them, cap at `motion.fast`
instead of being removed (`regia_ui::theme_motion::reduced_duration`). The pressed nudge (`translate: [0, 1]` in a
`pressed` layer) is a state change, so it is instant either way.

`regia_ui::theme_motion::sample(easing, t)` gives the eased progress for tests and tools; `exit_duration` is the
0.75 x rule for `ease.exit`. `Themed::anim_ease` holds the control points of the transition now running, so an agent
reading a node mid-fade can tell which curve drives it.

## Layout box and token slots (ADR-7208)

**Padding and min/max size.** A widget type, variation or `ThemeOverride` may set `padding` (one number for every
edge, two for vertical then horizontal, four for top, right, bottom, left as in CSS), `min_width`, `min_height`,
`max_width` and `max_height` (UI px). Unlike the other properties they reach layout: where the theme sets one, it
replaces the node's own `Style` value (padding mirrors in right-to-left layout like the style's own padding), and a
theme change relayouts only the nodes whose box moved. Where the theme sets none, the node's style is used as is.
**Height, inline padding and gap (ADR-9340).** A theme's own `height` (UI px) becomes the node's height,
`padding_x` its left and right padding (a full `padding` wins over it), and `gap` the row and column gap between its
children. The `size.control.*` and `size.pad-x.*` values a node with a `WidgetSize` resolves in `height` and
`padding_x` without a theme property do not move layout: widgets size themselves from `UiDensity`. `ui.theme_get`
reports what reaches layout under `resolved.props.layout` (`{ height, padding_x, gap }`, `null` where the theme sets
nothing).

```ron
types: { "Card": (props: { "padding": [12, 16], "min_width": 240, "max_width": 480 }),
         "Toolbar": (props: { "height": 40, "padding_x": 8, "gap": 4 }) }
```

**Token slots.** Widgets read these tokens by name; `ui.theme_list` lists them with the resolved values:

| Slot | Kind | Read by | When unset |
|---|---|---|---|
| `color.scrollbar-track` | color | `Scrollbars` presets with a track (classic) | `color.text` at the preset alpha |
| `color.scrollbar-thumb` | color | the scroll view's built-in thumb; `Scrollbars` preset thumbs | white at 35 % (built-in); `color.text` tint |
| `color.scrollbar-thumb-hover` | color | preset thumb under the pointer | `color.text` tint |
| `color.scrollbar-thumb-active` | color | preset thumb while pressed or dragged | `color.text` tint |
| `subtitle.text` | color | caption text while the player keeps the default color (white) | white |
| `subtitle.plate` | color | the caption box plate; its alpha multiplies the player's box opacity | black |
| `subtitle.plate-opacity` | number | the lowest caption box opacity | the player's opacity |
| `subtitle.font` | text | the caption font family (rows are measured and drawn in it) | the UI font |
| `subtitle.size` | number | multiplies the caption text size on top of the player's text scale (0.25 to 4) | 1 |
| `subtitle.outline` | color | the stroke around caption glyphs with the `Outline` background | black |
| `subtitle.outline-width` | number | the outline width in UI px (at most 8 physical px) | 2 |

The scrollbar slots are optional: no built-in theme sets them, so the default look is unchanged, and a custom
`Scrollbars` config (not a preset) is painted as stored. `regia-dark` (and so every built-in theme) sets
`subtitle.text` white and `subtitle.plate` black; the high-contrast themes set `subtitle.plate-opacity` to 1, so
turning high contrast on swaps captions to an opaque plate (a player who turned the box off keeps it off, and a text
color the player picked wins over `subtitle.text`). `regia-dark` also sets `subtitle.size` 1, `subtitle.outline` black
and `subtitle.outline-width` 2 (the previous size; the `Outline` background used to draw nothing and now strokes the
glyphs); `subtitle.font` is unset. `ui.theme_get` returns the theme's caption style as
`caption: { text, plate, plate_opacity, font, size, outline, outline_width }`. The linter treats the slot keys as
known.

```json
{"method": "ui.theme_set", "params": {"tokens": {"subtitle.size": 1.25, "subtitle.outline": "#202020"}}}
{"method": "ui.theme_get", "params": {"keys": ["subtitle.*"]}}
```
