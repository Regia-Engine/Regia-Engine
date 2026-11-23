# Theme panel (editor)

The editor's **Theme** tab edits a runtime UI `.theme.ron` (the files described in [UI themes](ui-theme.md)). It
holds a *draft*: a working copy of one theme that you change token by token, lint, and save. It never touches the
running game's theme; the file you save is what a game loads (`[ui] theme = "my-game"`).

The editor window itself stays on the [editor theme](theme.md) tokens. This panel is for the theme your game ships.

## What the panel shows

- **Open / New child.** Open a built-in or loaded theme, or start an empty child of one (the re-skin workflow: the
  child inherits every token and you set only what differs).
- **Token rows**, grouped by section (Color, Spacing, Shape, Type, Motion, Size, Other). Colors have a color picker,
  numbers a drag value, other tokens a text box. A row you have set shows a `reset` button; an inherited row says so
  on hover. Changing `color.accent` restyles every derived token (hover, pressed, selection) because those are
  expressions of it.
- **Density** (comfortable or compact): which size tokens the rows and the lint resolve for.
- **Swatches**: the main color roles drawn as foreground on background, so a change is visible at once. This is not
  the full widget sheet, and the panel does not embed it: render the sheet with `ui.theme_preview` ([UI
  themes](ui-theme.md)), passing the same token edits as `tokens` overrides.
- **Lint**: contrast pairs against WCAG 2.x, touch target size, unknown token keys, resolve errors.
- **Save**: writes the draft through a temp file and a rename. Built-in names (`regia-dark`, `regia-light`,
  `regia-high-contrast`, `regia-light-high-contrast`) are refused; copy a built-in by renaming the draft first.

An edit is checked against the whole `extends` chain before it is accepted. A value that does not parse, a missing
reference, or a type mismatch (a duration where a color is expected) is rejected and the draft stays as it was.

## Methods

Mutations need `edit_scene`; opening a file and saving need `assets`. Relative paths are relative to the project
root; `..` is refused.

| Method | What it does |
|---|---|
| `editor.theme_panel_state { filter?, group?, limit? }` | Draft name, `extends`, density, `dirty`, `revision`, the openable `themes`, and token rows `{ key, group, kind, value, raw, inherited }`. `value` is resolved (colors `#rrggbb`, or `#rrggbbaa` when the color is not opaque, durations in ms); `raw` is the draft's own expression, `null` when inherited. `limit` defaults to 200; `truncated` says more exist. |
| `editor.theme_panel_open { theme? \| path? \| new?, density? }` | Make a loaded theme, a `.theme.ron` file or `new: { name, extends }` the draft. Give exactly one of the three. |
| `editor.theme_panel_set { tokens?, rename?, density? }` | `tokens` is `{ key: value }` with a number or an expression string (`"#e8793a"`, `"80ms"`, `"@color.accent"`, `"lighten(@color.accent, 0.06)"`); `null` resets a key. `tokens` and `rename` are restored together when either fails (the draft text and `revision` go back). `density` is applied first and is not restored, including when no draft is open (`theme_panel_no_draft`). Returns `changed` and lint `warnings`. |
| `editor.theme_panel_lint {}` | `{ clean, findings: [{ code, severity, key, message }] }`. Codes: `contrast`, `touch_target`, `unknown_key`, `resolve_error`. |
| `editor.theme_panel_save { path? }` | Writes the draft. Without `path`, saves back to the file it was opened from. |

`revision` increases on every accepted change; a client can poll `editor.theme_panel_state` until it passes the value
it saw before.

## Errors

Error `data.code` values are lowercase and stable:

| Code | Meaning |
|---|---|
| `theme_panel_no_draft` | No theme is open; call `editor.theme_panel_open`. |
| `theme_panel_unknown_theme` | No loaded theme has that name (`themes` in the state lists them). |
| `theme_panel_invalid_token` | A token value failed to parse or resolve; the message carries the key path. Nothing changed. |
| `theme_panel_unknown_token` | `null` reset of a key the draft does not set. |
| `theme_panel_invalid_name` / `theme_panel_name_taken` | A new or renamed theme name is not letters, digits, `-`, `_`, or already exists. |
| `theme_panel_builtin_name` | The draft still has a built-in name; rename it before saving. |
| `theme_panel_parse_error` | A file failed to load, or the text to save did not parse back. |
| `theme_panel_io_error` | A file could not be read or written. |
| `theme_panel_no_path` | `editor.theme_panel_save` without `path` and the draft has no file yet. |

## Example

```json
{ "method": "editor.theme_panel_open", "params": { "new": { "name": "my-game", "extends": "regia-dark" } } }
{ "method": "editor.theme_panel_set", "params": { "tokens": { "color.accent": "#e8793a", "color.on-accent": "#140a04", "radius.md": 3 } } }
{ "method": "editor.theme_panel_lint", "params": {} }
{ "method": "editor.theme_panel_save", "params": { "path": "assets/ui/themes/my-game.theme.ron" } }
```

Check it worked: `editor.theme_panel_state { "filter": "color.accent" }` shows the new value with `inherited: false`
and `dirty: true`; after the save, `dirty` is `false` and the file loads again with `editor.theme_panel_open { path }`.

## Types, states and variations (v0.6 UI parity)

The **Widget styles** tree selects a type or variation, then a state. Its typed property rows show inherited
expressions and highlight local overrides. Numbers can be typed or adjusted; colors have both an expression
field and a picker; lists use JSON arrays. **Reset override** removes just the draft's key. **New variation**
takes a dotted name such as `Button.alert` and an existing base such as `Button` or `Button.primary`. The
root-first variation chain is shown below the selection. **Pin style** remembers a type/state; **Apply to all
states** copies that effective property box to each supported state in one undo step.

`editor.theme_panel_set` additionally accepts `types` and `variations`, using the runtime theme patch shape:

```json
{ "variations": { "Button.danger": { "extends": "Button", "props": { "background": "#ca1234" } } } }
{ "types": { "Button": { "hover": { "radius": 12 }, "sizes": { "sm": { "height": 30 } } } } }
{ "types": { "Button": { "hover": { "radius": null } } } }
```

Definitions merge per key. `null` removes a draft definition or property, exposing inheritance again. Supported
layers are `props` (normal), `hover`, `pressed`, `focused`, `disabled`, `selected` (checked), `error`, and
`sizes.<name>`. `extends` is a type/variation name or `null` to use the implicit base. Cycles and invalid references
reject the whole operation. Existing inherited variations retain their other inherited properties when edited.

Additional `editor.theme_panel_set` parameters:

| Parameter | Contract |
|---|---|
| `selection` | `{ "type": "Button.danger", "state": "pressed" }`; view selection only. State aliases: normal/props, hovered/hover, checked/selected, plus the other layers above. |
| `apply_all_states` | A `{ type, state }` selection; copy its effective property box to every supported state, one undo entry. Must be the only parameter. |
| `history` | `"undo"` or `"redo"`, restoring a whole draft operation. Must be the only parameter. Opening another theme clears this history. Saving does not clear it. |
| `preview_page` | Built-in gallery kind or a loaded document page name. Must be the only parameter. |
| `preview_document` | Project-relative or permitted absolute `.ui.ron` path; add/reload and select that preview page. Must be the only parameter. |

A successful call with tokens, types, variations and/or rename creates one undo entry and one draft revision.
A rejected call preserves draft, selection, history and theme collection. Density keeps the older behavior
described above: it applies before validation. View-only operations do not dirty the theme.
`editor.theme_panel_state` additionally returns merged `types`, `variations`, current `selection`, root-first
`chain`, effective `properties`, `gallery` page names and `preview` (current `page`, `pages`, `error`, and the
last rendered `picks` with accessible labels, type/state selections and pixel rectangles).

## Live gallery

Expand **Live widget gallery** to preview runtime widgets in normal, hovered, pressed, focused, disabled,
checked and error states. Choose a widget page or add a user `.ui.ron` document. The runtime UI renderer uses
the unsaved draft and selected density. The selected variation is applied to compatible built-in samples.
Hovering or pressing a sample updates its runtime state styling. Clicking selects the sample's runtime class
and state in the style editor. The focusable sample list supplies the same selection with Up/Down and
Enter/Space. Text fields, property actions and import controls also work from the keyboard and use the editor's
UI scale and theme colors.

The gallery demonstrates styling; it does not execute a user's game bindings. It requires an available render
device (lavapipe is supported). The existing token swatches still work without a GPU. `focus_visible` is not a
separate layer at this base; the focused sample shows the runtime navigation focus ring. Gallery view state
and added documents are not written into `.theme.ron`.

## Selective import

`editor.theme_panel_import { from, items, with_values? }` needs `assets`. `from` is a loaded or built-in theme name,
a theme file path, or `"editor"`. File paths obey the same project/temp confinement as other theme file methods.
`items` is a nonempty array of `{ type, state?, kind? }` selectors. Omitted state selects all states and size
layers. `kind` defaults to `all`; other kinds are `colors`, `sizes`, `fonts`, `borders`, `paddings` and `other`.

```json
{ "method": "editor.theme_panel_import", "params": {
  "from": "regia-light",
  "items": [{ "type": "Button", "state": "normal", "kind": "colors" }]
} }
```

This copies exactly `types.Button.props.background`, `types.Button.props.border_color` and
`types.Button.props.text_color`. It copies no tokens, sizes, sibling types or other states. The response includes
sorted `copied` paths. Expressions resolve against the source before copying, so missing source-only token
names cannot leak into the destination. A compound `border` or `focus_ring` is a whole `borders` item, never a
partial colors import; padding fields are `paddings`; font family/text role/size properties are `fonts`.

Use `type: "tokens"` for global tokens, filtered by kind (colors, sizes, fonts/text, other); use `type: "fonts"`
for font family definitions. Neither accepts a state. `with_values` defaults to true. False creates selected
keys with their current destination values, falling back to the built-in default; if neither exists the whole
import is rejected. Imports copy property keys, not variation ancestry; an existing destination variation
keeps its base, and a new one uses its implicit base. Every successful import is one undo step. Failed selectors,
invalid sources or missing references leave the entire draft and its history unchanged.

The editor source maps the documented UI-role bridge's ten unambiguous color roles and Button normal/hover/
pressed colors into a sparse runtime theme. Unmapped keys are absent; requesting only unmapped items fails
without editing the draft. It does not import editor-only gizmo colors, font files or chrome layout. In the panel, choose a source, type (blank uses the selection), state (`all` selects all),
kind, and **Copy values**, then **Import items**.

Additional stable error codes (prefixed with `theme_panel_` in protocol replies):

| Code | Meaning |
|---|---|
| `theme_panel_invalid_state` | The selected state is not supported by the current theme format. |
| `theme_panel_unknown_type` | The selected or imported type/variation does not exist. |
| `theme_panel_invalid_import` | Invalid selector kind, no matches, invalid source, or a names-only item has no destination/default value. |
| `theme_panel_empty_history` | There is no undo/redo entry. |
| `theme_panel_unknown_page` | The requested gallery/document page is not loaded. |
| `theme_panel_preview_failed` | Runtime preview creation, layout or readback failed. |

Malformed JSON parameter shapes use the standard JSON-RPC invalid-parameters response (`F0002`). File read/parse failures use the existing
`theme_panel_io_error` / `theme_panel_parse_error` codes. No format migration is needed.

For a headless acceptance screenshot, call `editor.theme_panel_state { "preview": true }`. The response's
`preview` includes `width`, `height`, `mime: "image/png"`, `png_base64` and fresh `picks` from the exact same
runtime render path as the panel. This read does not edit the draft or its revision. Without `preview` (or false),
state reads do not create a GPU device.

**Add type** creates an empty widget type or part (for example `Slider.thumb`), equivalent to
`editor.theme_panel_set { "types": { "Slider.thumb": { "props": {} } } }`. Adding a type describes a style;
it does not register a new runtime widget implementation.

The import host-extension contract is synchronous: success commits one draft/history operation and returns
the exact sorted copied paths; rejection commits nothing. It uses the dispatcher’s default rate class and
host-extension parameter caps (65,536 array items and 1 MiB per string), plus the schema’s `assets` permission.
The runtime contract table intentionally covers built-in methods; editor extensions use these default caps.
