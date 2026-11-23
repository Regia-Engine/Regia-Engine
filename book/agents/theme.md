# Editor theme

The editor paints with one set of **theme tokens**: the egui chrome (fills, strokes, text,
selection), the viewport overlays (grid, selection outline, icons, gizmo axes, audio ranges)
and the panel accents (animation canvases, the "unsaved" label, log levels). A host can restyle
the whole window by changing the tokens. The default tokens are **Piercing Graphite**, the
PiercingXX brand's dark look (ADR-8585): graphite surfaces, Signal white reserved for the selection
(inverted: a white block with dark text), links, the focus ring and the active item, the brand's
warning, error, success and info colors on small marks, flat surfaces with a lit top edge, and the
brand fonts. The other built-in looks are **AMOLED Night**, **Paper** and **High contrast**
([Built-in looks](#built-in-looks)).

## Methods

| Method | What it does |
|---|---|
| `editor.get_theme` | Returns `{ tokens, is_default, theme_source }`. `tokens` is in the shape `editor.set_theme` reads; `theme_source` is `default` or `ui:<name>`. |
| `editor.set_theme { tokens?, reset?, from_prefs?, ui_theme? }` | Changes the keys named in `tokens`; the rest keep their value. `reset: true` starts from the Regia Engine default (and `theme_source` back to `default`); `from_prefs: true` starts from the theme the preferences compose (the Theme choice, the saved custom theme, the diagnostic colors), dropping unsaved edits (the Theme page's Revert edits). `ui_theme` skins the editor from a UI theme (below). The window restyles on the next frame. |
| `editor.theme_save { name }` | Saves the current tokens as a custom theme and makes it active ([Saved themes](#saved-themes)). |
| `editor.theme_import { path \| text, name? }` | Imports a theme file into the saved themes and makes it active. |
| `editor.theme_export { path, name? }` | Writes the current tokens, or the saved theme `name`, to a theme file. |
| `editor.theme_list` | The built-in themes, the saved ones, and which is active. |
| `editor.color_vision { mode?, deficiency?, severity? }` | View ▸ Color Vision: simulates or compensates a color-vision deficiency on the editor chrome and the viewport ([Color vision](#color-vision)). |

Colors are `"#rrggbb"` or `"#rrggbbaa"`. Unknown keys are ignored; a known key with a bad value
returns `invalid_params` and changes nothing. `flat`, when present, must be a bool. `colors`,
`overlay`, `widgets`, `accents`, `data_kinds` and `fonts`, when present, must be objects. A color named both
inside `colors` and at the top level uses the top-level value.

## Token keys

Top level: `ground`, `raised`, `panel`, `well`, `line`, `shade`, `muted`, `strong`, `text`,
`signal` (the accent: the selection fill), `on_signal` (text on it), `warn`, `error`, `lit_edge`
(a 1 px line of this color along the top of windows and menus; fully transparent draws none), `flat`
(no window or popup shadows; corner radius 2 on controls and 4 on windows). The brand names `ink`, `graphite` and `slate` are accepted for
`ground`, `panel` and `well`, and the colors may sit under a `"colors"` object, so a brand
`colors.json` loads directly.

Sections:

- `overlay`: `grid_major`, `grid_minor`, `selection_outline`, `bounds`, `icon_light`,
  `icon_camera`, `icon_decal`, `icon_audio`, `gizmo_x`, `gizmo_y`, `gizmo_z`, `gizmo_ring`,
  `audio_min`, `audio_max`, `audio_cone`, `reverb_zone`.
- `widgets`: `idle`, `hover`, `pressed`, `open`, `button_text`, `hover_text`, `open_text`,
  `hover_line`, `link`, `code`, `edge` (the outline of a control at rest; when it is not fully
  transparent, keyboard focus also draws a 2 px `signal` ring).
- `accents`: `active`, `select`, `alert`, `ok`, `unsaved`, `error_text`, `log_error`,
  `log_warn`, `log_info`, `play_mode`, `play_server`, `play_client_1` to `play_client_4` (the play-mode tint, ADR-7829).
- `data_kinds`: the procedural graph canvas's pin colors (ADR-7239), `points`, `table`, `spline`,
  `polygon`, `shape`, `ground`, `mask`, `mesh`, `plant_skeleton`, `plant_mesh`. The default set is
  colorblind-safe; see [Procedural graph editor](procgen-editor.md).
- `fonts`: `proportional`, `monospace`, `display`, `prose`, each a name or `{ name, file }`. `file`
  is a `.ttf`/`.otf` path on the machine running the editor. The editor ships three faces (SIL OFL
  1.1): `"JetBrains Mono"`, `"Space Mono"` and `"Geist"`; a font with one of these names and no
  `file` uses them. Another name without a file, or a missing file, keeps egui's default font (a
  missing file is logged). A theme font goes first in its family and egui's fonts stay behind it, so
  a character it lacks still shows. The display family is `egui::FontFamily::Name("display")` and
  headings use it; the prose family is `egui::FontFamily::Name("prose")`. Both fall back to the
  proportional font. The default look: JetBrains Mono (proportional and monospace), Space Mono
  (display), Geist (prose).

Absent keys take Piercing Graphite's values. A saved theme file is different: its absent keys keep
the values they had when the file format was defined (the pre-brand look,
`ThemeTokens::regia_classic()`), so a theme saved before ADR-8585 looks as it did.

The brand kit's editor files (`piercingxx-branding/ui/dist/regia/editor/*.json`) load as they are
with `ThemeTokens::from_json`.

## Built-in looks

`appearance.theme` (Preferences ▸ Appearance ▸ Theme, `editor.prefs_set`, `editor.theme_list`'s
`builtin`):

| Choice | Look |
|---|---|
| `default` | **Piercing Graphite**: ground `#131316`, panels `#18181B`, windows and menus `#1F1F23`, fields `#0F0F12`, hover `#26262B`, lines `#2B2B30`, control outlines `#7C7C84`, text `#E7E7E8`, secondary text `#A8A8B0`, Signal `#FFFFFF` |
| `amoled-night` | **AMOLED Night**: true black ground, panels `#09090B`, windows `#131316`, fields `#18181B`, white text at the brand's opacity stops |
| `paper` | **Paper** (light): ground `#F3EEE2`, ink text, Signal is ink `#000000` (the selection is an ink block with paper text), the light status colors |
| `high-contrast` | **High contrast** ([below](#high-contrast)) |
| `regia-dark`, `regia-light` | The runtime UI themes, bridged ([below](#skinning-the-editor-from-a-ui-theme)) |

Every built-in look meets WCAG 2.x AA on every pair `editor.a11y_contrast` measures
([Editor accessibility](editor-a11y.md)).

## Skinning the editor from a UI theme

The editor look is Piercing Graphite until you opt in. `ui_theme` takes a UI theme name
(`regia-dark`, `regia-light`, `regia-high-contrast`, `regia-light-high-contrast`; a `ui:` prefix is allowed) or an object of role colors, maps the
roles onto the tokens and records `theme_source = "ui:<name>"` (`"ui:custom"` for an object).
`ui_theme: "default"` returns to Piercing Graphite. Tokens named in the same call apply on top.

| Editor token | UI role |
|---|---|
| `ground`, `raised` | `color.surface-overlay` |
| `panel` | `color.surface` |
| `well` | `color.surface-sunken` |
| `line` | `color.border` |
| `muted` | `color.text-muted` |
| `text` | `color.text` |
| `strong` | `color.text` lightened by 0.05 (OKLab) |
| `signal` / `on_signal` | `color.accent` / `color.on-accent` |
| `warn` / `error` | `color.warning` / `color.danger` |

Roles the object lacks come from `regia-dark`. Widget and accent colors follow the roles too, so a
light theme draws light controls and dark accents: controls rest on `color.surface-raised` with a
`color.border-strong` outline (or the text mixed 48 % into the panel), hover and press are the text
mixed 8 % and 14 % into the panel, links and the active and selected item use `color.accent`, and
the status colors use `color.warning`, `color.danger`, `color.success` and `color.info`
(translucent roles are flattened over the panel). Fonts, `flat` and the lit edge are the default
look's. Overlay, gizmo and graph pin colors keep their defaults. `editor.<section>.<key>` entries
override any of these, for example `"editor.overlay.grid-major": "#102030"` (sections `overlay`,
`widgets`, `accents`).

```console
$ regia agent call editor.set_theme '{"ui_theme":"regia-light"}'
$ regia agent call seeing.screenshot --out light-editor.png
$ regia agent call editor.set_theme '{"ui_theme":{"color.accent":"#e8793a","color.on-accent":"#140a04"}}'
$ regia agent call editor.set_theme '{"ui_theme":"default"}'
```

Only the built-in theme set resolves by name; a project's own `.theme.ron` (child theme) is not
looked up. Pass its resolved `color.*` roles as an object instead.

## Example: invert the selection and check

```console
$ regia agent call editor.set_theme '{"tokens":{"signal":"#ffffff","on_signal":"#000000","overlay":{"selection_outline":"#ffffff"}}}'
$ regia agent call seeing.screenshot --out themed.png
$ regia agent call editor.capture_viewport '{"view":"perspective"}'   # selection_highlight follows the themed outline
$ regia agent call editor.set_theme '{"reset":true}'
```

## For hosts

In Rust: `regia_editor::theme::ThemeTokens::from_json(text)?`, insert it as a world resource
(or call `theme::apply(&ctx, &tokens)` on your own egui context), read colors with
`theme::tokens(world)` (world code) or `theme::ui_tokens(ctx)` (code that only has a `Ui`).

Your own fonts (ADR-8888): the theme merges its font table over yours. Every face and family you
installed stays; the theme puts its faces first in the families it sets (your chain stays behind
them), appends its symbol and script fallbacks, and defines only `display` and `prose` itself. If you
install fonts at start-up, before egui's first pass or in the same pass as the first `apply`, call
`theme::set_host_fonts(&ctx, defs)` instead of `ctx.set_fonts(defs)`: the theme cannot read a table
egui has not installed yet. Fonts you set with `ctx.set_fonts` in an earlier pass than the first
`apply` are kept without it. `theme::font_definitions_over(base, &tokens.fonts)` returns the merged
table if you install fonts yourself.

## High contrast

`appearance.theme: "high-contrast"` (Preferences ▸ Theme, or `editor.prefs_set`) is the editor's own high-contrast
theme (ADR-7838). Its colors follow one rule: pure black surfaces, pure white text and lines, and one accent (pure
yellow, `#ffff00`) for the selection, hover, links and warning-level text. Controls at rest are black with a white
outline (a control filled with the panel color gets the `line` color as its boundary). Every text pair meets WCAG
AAA (7:1; large text needs 4.5:1, so the body-text bar covers headings), checked by
`editor.a11y_contrast { theme: "high-contrast", level: "aaa" }` ([Editor accessibility](editor-a11y.md)). Viewport
overlay and graph pin colors keep their defaults: they carry meaning over the 3D view. `theme_source` reports
`ui:high-contrast`. The UI theme set's `regia-high-contrast` (through `ui_theme`) is a different, runtime UI theme.

## Saved themes

The Theme page of Preferences edits every token with a live preview (a color button and a hex field per token; the
accent first), and **Save theme as…** writes the result to `<user config>/regia/themes/<name>.editor_theme.ron`
and makes it the active theme through the `appearance.custom_theme` preference, so it comes back in the next session.
**Import theme…** copies a theme file into the saved themes; **Export theme…** writes one to share. Agents do the
same with the methods above. A name is 1 to 64 letters, digits, spaces, `-` or `_`.

The file (a new per-user format, `format_version` 1; a missing version reads as 1, a newer one is refused):

```ron
(
    format_version: 1,
    name: "Studio Night",
    tokens: { "signal": "#123456", "panel": "#101010", "accents": { "ok": "#00aa00" } },
)
```

`tokens` has the shape `editor.get_theme` answers; absent keys keep the Regia Engine default, unknown keys are
ignored. `editor.theme_import` and `editor.theme_export` take a `path` under the project or the temp directory, as
`editor.keymap_import` does.

```console
$ regia agent call editor.set_theme '{"tokens":{"signal":"#e8793a","on_signal":"#140a04"}}'
$ regia agent call editor.theme_save '{"name":"Ember"}'
$ regia agent call editor.theme_export '{"path":"shared/ember.editor_theme.ron"}'
```

Diagnostic colors (Preferences ▸ Theme ▸ Diagnostic colors, or the `appearance.color_*` preferences) apply on top
of the theme: error sets `error`, `accents.error_text` and `accents.log_error`; warning sets `warn`,
`accents.unsaved` and `accents.log_warn`; info sets `accents.log_info`; success sets `accents.ok`. The Output Log,
the Problems panel, status lines and the content browser's badges read those tokens.

| Code | When |
|---|---|
| `bad_theme_name` | the name is empty, longer than 64 bytes, or has other characters |
| `theme_parse` | the file is not a theme file, or a token value does not parse |
| `theme_newer` | the file's `format_version` is newer than this editor reads |
| `theme_not_found` | `editor.theme_export { name }` names no saved theme |
| `theme_io` | reading or writing failed, or the editor has no user config directory (tests, a bare world) |
| `bad_path` | `path` is missing, or an export path does not end in `.editor_theme.ron` |
| `path_outside_project` | `path` leads outside the project and the temp directory |

## Color vision

View ▸ Color Vision has Off, Simulate protanopia / deuteranopia / tritanopia and Compensate for each. The choice
applies to the editor chrome and the viewport at once, so a designer checks panels and scene with the same eyes. The
chrome goes through the published simulation matrices `render.color_vision` uses (`regia_render::color_vision`,
ADR-2646) per vertex after egui tessellates the frame; pictures drawn from textures (the viewport image,
thumbnails) keep their pixels, and the viewport gets the same filter as a LUT on its views.

```console
$ regia agent call editor.color_vision '{"mode":"simulate","deficiency":"deuteranopia"}'
$ regia agent call seeing.screenshot --out deutan-editor.png
$ regia agent call editor.color_vision '{"mode":"off"}'
```

The answer is `{ mode, deficiency, severity, active, chrome, viewport, choices }`. Codes: `color_vision_bad_params`
(not an object, or an unknown key), `color_vision_bad_mode`, `color_vision_bad_deficiency`,
`color_vision_bad_severity` (outside 0 to 1).
