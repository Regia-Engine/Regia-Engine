# `.ui.ron` format (v11)

Frozen layout/authoring format for runtime UI trees (`docs/specs/18-runtime-ui.md` § 5).
Parsed by `regia_ui::parse_ui_ron`, written by `regia_ui::write_ui_ron` (the UI designer's canonical
writer, `docs/specs/81-ui-designer.md`). **Current version: 11** (ADR-9980; v10 ADR-2958).

| Version | Change | Migration |
|---------|--------|-----------|
| 1 | first frozen shape (M7) | — |
| 2 | menu widgets `popup_menu`, `context_menu`, `menu_bar` and their entries (v0.1.29, ADR-332, ADR-1381) | built in: v1 files load unchanged; `regia upgrade-project --write` (or `regia_ui::upgrade_ui_ron`) rewrites only the root `version` number |
| 3 | view-model binding (ADR-1811): node fields `bindings` and `command`; bind fields `mode` (`one_way`, `two_way`, `one_time`) and `invert`; bind targets `value`, `input`, `selected`, `visible`, `enabled` | built in: every addition is optional or a new variant, so v2 (and v1) files load unchanged; the v2 → v3 step rewrites only the root `version` number (v1 files take both steps) |
| 4 | text effects (ADR-2045): text-style fields `letter_spacing` (em), `weight` (1 to 1000), `underline`, `strikethrough`, `features` (OpenType settings such as `"smcp"`, `"liga=0"`) and `effects` (layers `(color, offset: (x, y), width, blur)`, UI px) | built in: every addition is optional, so v3 (and older) files load unchanged; the v3 → v4 step rewrites only the root `version` number. Unset fields are not written, so a v3 tree writes the same text apart from the number |
| 5 | backdrop blur (ADR-2129; written as v4 by its lane, renumbered at the v0.2.13 integration, ADR-2219): node field `backdrop_blur` (Gaussian sigma in UI px) | built in: the field is optional, so v4 (and older) files load unchanged; the v4 → v5 step rewrites only the root `version` number |
| 6 | collection item templates and converters (ADR-2489): node field `repeat: (source, key?)`; bind field `converter`; binding sources starting with a dot read the collection item | built in: every addition is optional, and no older file means anything by a dotted source (it never resolved), so older files load unchanged; the v5 → v6 step rewrites only the root `version` number |
| 7 | group layers (ADR-2866): node fields `group_opacity` (isolated group opacity, 0..1), `clip_shape` (`rect` or `rounded`) and `mask: (image | asset, fit?)` (`fit` `stretch`, `contain`, `cover`, `tile`) | built in: every addition is optional, so v6 (and older) files load unchanged; the v6 → v7 step rewrites only the root `version` number |
| 8 | components (ADR-3147, `docs/specs/111-ui-components.md`): top-level `component: (params, slots)`; node fields `use`, `params`, `bind_root`, `slots` and `slot` | built in: every addition is optional, and no older file has a `use` node, so v7 (and older) files load unchanged; the v7 → v8 step rewrites only the root `version` number |
| 9 | named pictures (ADR-3148, written as v8 by its lane, renumbered at the v0.2.19 integration): `image` fields `asset` (manifest path of a cooked image or texture, loaded through the asset server in the background) and `icon` (a name in the `IconAtlas`), optional `nine_slice: (left, top, right, bottom)` insets (source px); `mask` field `icon`; `image.id` may be left out when `asset` or `icon` names the picture | built in: every addition is optional and a v8 `image` always named a renderer id, which v9 reads the same way, so v8 (and older) files load unchanged; the v8 → v9 step rewrites only the root `version` number |
| 10 | game widgets (ADR-2958, written as v6 by its lane, renumbered at the integ-v049 recovery): widget variants `lobby_list`, `scoreboard`, `radar`, `marker_layer` | built in: every addition is a new widget variant, so v9 (and older) files load unchanged; the v9 → v10 step rewrites only the root `version` number |
| 11 | text input options and image fit (ADR-9980): `text_input` takes an optional third element `(max_chars?, secret?, read_only?, filter?)`; `image` field `fit` (`stretch`, `contain`, `cover`) | built in: both are optional, the two-element `text_input` still reads (no limits) and an image without `fit` reads `preserve_aspect` as before (`true` = `contain`, `false` = `stretch`), so v10 (and older) files load unchanged; the v10 → v11 step rewrites only the root `version` number |
| 11 (fold) | the UI designer's additions, folded into v11 before it shipped (ADR-7751 under ADR-7724's fold rule): bind fields `theme` (a theme token) and `animation` (`(from, to, duration, delay?, ease?, repeat?)`), with `source` then optional; `repeat` field `row_height` (`list` default, `template`); image `fit: tile` | no step: every addition is an optional field or an appended variant, so every v11 file written before the fold (the frozen corpus `ui/v11.ui.ron`) loads unchanged with the same meaning; the corpus adds `ui/v11-designer.ui.ron` as a second v11 entry; the v10 → v11 step still rewrites only the root `version` number. A dev build from before the fold ignores `theme`/`animation` (no released build reads v11) |
| 11 (fold) | containers, markup and focus neighbors (ADR-7827, folded into v11 before the v0.6.0 tag under the ADR-7724 rule): widget variants `split((axis?, ratio?, min_ratio?, max_ratio?))`, `foldable((title?, open?, group?))`, `tabs((tabs, active?))`; text field `markup`; node field `nav: (up?, down?, left?, right?)` | none needed: every addition is optional or a new variant, so every earlier v11 file reads with the same meaning (corpus `ui/v11.ui.ron` unchanged; `ui/v11-parity.ui.ron` exercises the fold); the v10 → v11 step already writes the final shape |
| 11 (fold) | layout variants (ADR-7858, the same v11 fold under ADR-7724): top-level `variants: [(name?, when: (orientation?, min_width?, max_width?, min_aspect?, max_aspect?), overrides: { "<node>": { "<field>": <value> } })]`; the first variant whose `when` matches the window (logical px) applies on top of the base | none needed: the field is optional, so every earlier v11 file reads with the same meaning (`ui/v11-variants.ui.ron` exercises it); the v10 → v11 step already writes the final shape |
| 11 (fold) | screen back, subtree disable and pivot (ADR-8252, folded into v11 before the v0.6.0 tag): file field `back` (`ignore` or `pop`); node field `disabled_subtree`; node field `pivot: (x, y)` | none needed: every addition is optional, so every earlier v11 file reads with the same meaning (corpus `ui/v11.ui.ron` unchanged; `ui/v11-screen.ui.ron` exercises the fold). The version stays 11 |

## Rules

- Top-level struct: `( version: 11, component?: ( ... ), root: ( ...node... ), variants?: [ ... ] )`.
- `variants` (ADR-7858): each `(name?, when: (...), overrides: {...})`. `when` takes `orientation` (`portrait`
  or `landscape`; square is landscape), `min_width` / `max_width` (logical window px) and `min_aspect` / `max_aspect`
  (width / height), all optional and inclusive. `overrides` maps a node key (a node name, the first match depth
  first, or a path `"/0/2"`, `"/"` the root) to field keys (dotted, as the designer's `set` takes them:
  `style.inset`, `opacity`, `widget`, `text`) and values in their written form (`null` resets the field). `children`
  and `name` cannot be overridden. An override that does not apply is skipped and logged.
- Unknown fields are rejected (`deny_unknown_fields`).
- RON extension **IMPLICIT_SOME** is enabled: bare values fill `Option<T>`
  (`name: "hud"` ≡ `name: Some("hud")`).
- Unit widget variants use snake_case (`widget: button`).
- Localization spans: `Loc("menu-start")` or literal strings.
- Files declaring a `version` newer than the current one (11) are rejected. v1 to v10 files are upgraded by
  the engine's built-in steps; any other older version needs a game-registered
  migrator in `UiMigrators` that upgrades the **source text** (the built-in steps then take it to
  v10). A game step registered for v1 to v9 is never used: the engine owns those steps.
- `spawn_ui` uses the root node's `style` and `children`, and the file field `back` (ADR-8252),
  which inserts the screen's back policy on that root. Set other root-node fields (`safe_area`,
  `background`, …) on a child. The UI designer's lint reports them (`root_field_ignored`).

## Node fields

| Field | Type | Notes |
|-------|------|--------|
| `name` | `Option<String>` | Debug / binding id |
| `classes` | `[String]` | Theme class names |
| `style` | `Style` | taffy subset; see `regia_ui::style` |
| `background` | color hex or `(from, to, angle)` | |
| `border` | `(color, width, radius)` | |
| `shadow` | `(color, blur, offset)` | |
| `image` | `(id?, asset?, icon?, tint?, preserve_aspect?, fit?, nine_slice?)` | the picture is named by at most one of `id` (a renderer image id that game code registers; 0 or absent when another names it), `asset` (v9: a manifest path such as `assets/ui/panel.png`; the cook packs it, `regia assets validate` reports a path no asset matches as `ui_image_missing`) and `icon` (v9: an `IconAtlas` name without `@size`; the drawing that best fits the node's height is used). Until an asset loads the node draws a faint placeholder; a picture that cannot load (missing, not an image, an unknown icon) logs one warning and draws a magenta and black fallback. `nine_slice: (l, t, r, b)` (v9) draws the picture as the node's nine-slice background: corners keep their size, edges and middle stretch; `tint`, `preserve_aspect` and `fit` then do not apply (a nine-slice image draws the same quads with any `fit` or none, ADR-7451). An empty `asset` or `icon`, two names, or a negative inset is refused when the file loads. `fit` (v11, ADR-9980): `stretch` fills the node, `contain` keeps the aspect and letterboxes, `cover` keeps the aspect, fills the node and crops the picture's overflow, centred (a 2:1 picture in a square node shows its middle half); without `fit`, `preserve_aspect: true` (the default) is `contain` and `false` is `stretch`, and `fit` wins when both are written. `tile` (v11 fold, ADR-7751) repeats the picture at its native size (source px of its UV rect times the UI scale) from the node's top-left corner, clipped to the node; at most 4,096 copies (`MAX_IMAGE_TILES`; a tinier picture tiles at a larger uniform size); nothing draws until the picture's size is known |
| `text` | `(spans, align?, wrap?, overflow?, line_height?, style?, markup?)` | `markup: true` (v11 fold, ADR-7827) reads the literal spans as rich-text markup: `<b>`, `<i>`, `<u>`, `<s>`, `<color=#rrggbb>`, `<size=px>`, `<icon=name>` and their closing tags (`regia_ui::parse_markup`); a `loc:` span stays a key; off by default, so `<` is plain text |
| `opacity` | `f32` | |
| `backdrop_blur` | `f32` | v4: frosted glass, Gaussian sigma in UI px behind the node (`BackdropBlur`); off when absent |
| `clip` | `bool` | |
| `interactable` | `bool` | |
| `disabled` | `bool` | |
| `z` | `i32` | |
| `safe_area` | `bool` | roots |
| `widget` | `WidgetDef` | see below |
| `bind` | `(source?, target?, format?, mode?, invert?, converter?, theme?, animation?)` | `Bind` |
| `bindings` | `[(source?, target?, format?, mode?, invert?, converter?, theme?, animation?)]` | v3: view-model bindings, several per node (`ViewBindings`); `theme` and `animation`: v11 fold, see Theme and animation bindings |
| `command` | `String` | v3: sent as `UiCommand` on click or text submit |
| `repeat` | `(source, key?, row_height?)` | v6: `children` spawn once per item of the list at `source`, keyed by `key`; `row_height` (v11 fold, ADR-7751) on a `list` host: `list` (default, the list's row height) or `template` (the first template node's fixed `style.size` height) |
| `group_opacity` | `f32` | v7: the node and its subtree composite once at this opacity (`GroupOpacity`) |
| `clip_shape` | `rect` \| `rounded` | v7: `rect` = `clip: true`; `rounded` clips children to the node's rounded rect (`RoundedClip`) |
| `mask` | `(image?, asset?, icon?, fit?)` | v7: multiply the node by an image's alpha, named by exactly one of `image` (a renderer image id, `UiMask`), `asset` (the manifest path of a cooked image such as `assets/ui/star.png`, `UiMaskAsset`; since v9 loaded in the background like an `image` asset, the node unmasked until it lands) and `icon` (v9: an `IconAtlas` name, `UiMaskIcon`); `fit` `stretch` (default), `contain`, `cover`, `tile` |
| `use` | `String` | v8: this node is an instance of the component at this path under `assets/` (see Components) |
| `params` | `{name: value}` | v8, instances: values for the component's declared parameters |
| `bind_root` | `String` | v8, instances: the view-model path the component's `$` sources resolve against (`"."`-prefixed: item-relative) |
| `slots` | `{name: [Node]}` | v8, instances: content per declared slot (plain `children` fill the `default` slot) |
| `slot` | `String` | v8, component documents: this node is the outlet of that slot |
| `nav` | `(up?, down?, left?, right?)` | v11 fold (ADR-7827): focus neighbors by node `name` (first match in the document, depth first). The runtime writes them into the node's `NavLinks` when the document spawns; a declared neighbor wins over the geometric search while the target is navigable (focusable, enabled, shown), otherwise geometry decides. Only a focusable node (a button, toggle, slider, text input, dropdown, popup menu or `focusable`) uses them |
| `children` | `[Node]` | With `repeat`, the item template |

## Bindings (v3)

`source` is `"Model.field[.field…]"` of a reflected resource; two-way writes need a registered view model
(`App::register_view_model`). `target`: `text` (default) · `progress` · `opacity` · `checked` · `value` (slider) ·
`input` (text input) · `selected` (dropdown, tab bar) · `visible` · `enabled`. `mode`: `one_way` (default) ·
`two_way` · `one_time`. `invert: true` negates a boolean for `checked`, `visible` and `enabled`. See
`book/agents/ui-designer.md`.

## Collections and converters (v6)

```ron
(name: "rows", repeat: (source: "InventoryVm.items", key: "id"), children: [
    (name: "row", widget: button, command: "inventory.equip", children: [
        (text: (spans: ["-"]), bindings: [(source: ".name")]),
        (widget: progress(0.0), bindings: [(source: ".durability", target: progress,
            converter: map_range(from: (0, 100), to: (0, 1), clamp: true))]),
    ]),
]),
```

`repeat.source` is `"Model.field[.field…]"`, or `".field"` of the enclosing repeat's item. `repeat.key` is a path inside
an item (`"."` is the item itself); without it items are keyed by position. A binding `source` starting with a dot
reads the item (`"."` is the item itself).

`converter`, run on the value before `format` and `invert`: `number(precision: 2, group: ",")` (`group` optional) ·
`clamp(min: 0, max: 1)` · `map_range(from: (a, b), to: (c, d), clamp: true)` (`clamp` optional) · `bool_to_visibility` ·
`enum_text(table: [("Idle", "Waiting")], default: "?")` (`default` optional) · `loc_key(prefix: "state-")` (`prefix`
optional). See `docs/specs/81-ui-designer.md` § 2.6.

## Theme and animation bindings (v11 fold, ADR-7751)

```ron
(name: "accent", text: (spans: [""]), bindings: [(theme: "color.accent")]),
(name: "pulse", opacity: 1, bindings: [(target: opacity,
    animation: (from: 0.25, to: 1, duration: 0.8, delay: 0.1, ease: "quad_in_out", repeat: ping_pong))]),
```

A binding reads exactly one of `source`, `theme` and `animation`; `theme` wins over `animation`, and either wins over
`source` (the designer lints `binding_source_ignored`). `theme` is a token of the active theme; the value follows a
theme switch: a colour reads `#rrggbb` (`#rrggbbaa` below full alpha), a number or a duration its number, text itself.
An unknown token pushes nothing and reports `binding_theme_token`. `animation` pushes a number on the game clock:
`from` → `to` over `duration` seconds after `delay` (default 0), eased by `ease` (a tween ease name such as
`quad_in_out`; default and unknown names are linear), `repeat` `once` (default; holds `to`) · `loop` · `ping_pong`.
Reduced motion pushes `to` at once. `format`, `converter` and `invert` apply as for a field. Neither kind writes
back: `two_way` acts as `one_way`.

## Components (v8)

A component is a `.ui.ron` document other documents instantiate by path; editing it edits every instance
(`docs/specs/111-ui-components.md`).

```ron
// assets/ui/health_bar.ui.ron
(
    version: 8,
    component: (
        params: [
            (name: "label", type: string, default: "HP", targets: [(node: "Label", property: text)]),
            (name: "fill", type: color, default: "#30C060", targets: [(node: "Fill", property: background)]),
        ],
        slots: ["badge"],
    ),
    root: (name: "HealthBar", children: [
        (name: "Label", text: (spans: ["HP"]), bindings: [(source: "$.name")]),
        (name: "Fill", widget: progress(1.0), bindings: [(source: "$.hp", target: progress)]),
        (name: "Badge", slot: "badge"),
    ]),
)
// an instance in another document
(use: "ui/health_bar.ui.ron", name: "P1Health", bind_root: "Party.p1",
 params: {"label": "P1", "fill": "#D04040"}, slots: {"badge": [(text: (spans: ["!"]))]})
```

- `params`: `name`, `type` (`string`, `number`, `bool`, `color`), `default` and `targets: [(node, property)]`; `node`
  is a node `name` of the component (`""` the root). Properties: `text`, `command`, `class` (appended), `background`,
  `border_color`, `tint`, `opacity`, `width`, `height` (px), `progress`, `visible`, `disabled`.
- An instance may set `name`, `classes` (appended), `style` (replaces the component root's), `params`, `bind_root`,
  `slots` and `children`; anything else is `ui_component_override`.
- A binding or repeat source starting with `$` is relative to the instance's `bind_root` (`$.hp` with
  `bind_root: "Party.p1"` is `Party.p1.hp`).
- Loading expands every instance (at most 8 components deep, 4,096 instances per document). A failing instance
  becomes a visible placeholder node named `ui_component_error` and a diagnostic: `ui_component_missing`,
  `ui_component_invalid`, `ui_component_cycle`, `ui_component_depth`, `ui_component_unknown_param`,
  `ui_component_param_type`, `ui_component_unknown_slot`, `ui_component_override`, `ui_component_bad_path`,
  `ui_component_too_large` (errors) and `ui_component_no_bind_root` (warning). See
  `book/agents/ui-components.md`.

## Widgets

`button` · `toggle(bool)` · `slider(v,min,max,step)` · `progress(v)` ·
`text_input(placeholder, multiline, options?)` · `dropdown([opts], selected?)` ·
`tab_bar([tabs], active)` · `modal(open, close_on_cancel)` ·
`tooltip(text, delay)` · `scroll(inertia, scrollbar)` · `list(count, row_h)` ·
`focusable` · `stick(action, radius)` · `v_button(action)` ·
`popup_menu([entries])` · `context_menu([entries])` · `menu_bar([entries])` ·
`lobby_list((...))` · `scoreboard((...))` · `radar((...))` · `marker_layer((...))` ·
`split((...))` · `foldable((...))` · `tabs((...))`

Containers (v11 fold; ADR-7827), every field optional; the writer leaves defaults out.

- `split((axis, ratio, min_ratio, max_ratio))`: the node's first two children are panes side by side (`axis: horizontal`,
  the default) or stacked (`vertical`), with a draggable divider between them. `ratio` (default 0.5) is the first pane's
  share; a drag keeps it within `min_ratio` (default 0.1) and `max_ratio` (default 0.9), so `min_ratio` and `1 - max_ratio`
  are the panes' minimum sizes. A third and later child gets no share (the designer's lint: `split_panes`).
- `foldable((title, open, group))`: a header showing `title` (`"loc:<key>"` for a key) above the node's children, which show
  only while `open` (default `false`). Foldables with the same `group` under one parent are an accordion: opening one
  closes the others.
- `tabs((tabs, active))`: a tab bar with one button per label above the node's children; child `i` is the page of tab `i`
  and only the `active` page (default 0) shows.

Menu entries (v2; ADR-332, ADR-1381): `(id, label, shortcut, enabled, checkable,
checked, submenu: [entries])`, every field optional (`enabled` defaults to `true`), or `(separator: true)`. A
`menu_bar`'s entries are its menus: each entry's `label` is the title and its `submenu` the menu.

Text input options (v11; ADR-9980): `text_input("PIN", false, (max_chars: 4, secret: '*', read_only: false, filter: integer))`,
every field optional and the whole element too. `max_chars` is the most characters the field takes (Unicode scalar values,
whole grapheme clusters; `0`, the default, is no limit; typing and pasting past it add nothing); `secret` is the mask character a
password field draws instead of its text (masked text is never copied or cut); `read_only` lets the text be selected but not
changed; `filter` is `any` (default), `integer` (an optionally signed whole number) or `numeric` (an optionally signed decimal
number), and an edit the filter refuses is dropped whole. The writer leaves the element out while every option is at its
default, so `text_input("Name", false)` stays as written.

Game widgets (v6; ADR-2958, `book/user-guide/game-widgets.md`), every field optional. A word may be `"loc:<key>"`.

- `lobby_list((title, refresh, create, empty, ready, leave, launch))`: words; an empty one keeps the default.
- `scoreboard((title, columns: [(key, title, width)], sort_column, descending, group_by_team, hold_action, from_rules))`:
  `sort_column` defaults to `Some(0)`, `descending`, `group_by_team` and `from_rules` to `true`; a `hold_action` hides the
  board until that input action is held; `width: 0` is the default width.
- `radar((range, world_rect, square, rotate, map_image, map_rect, layer_mask, max_blips, show_north))`: without `world_rect`
  a radar of `range` metres (default 50), circular unless `square`; with `world_rect` (`(min_x, min_z, max_x, max_z)`) a
  rectangular minimap of that area. `map_image` is an image id (`0` draws none); `map_rect` is the area it covers, for a
  minimap by default the `world_rect`. `max_blips` defaults to 128, `show_north` to `true`.
- `marker_layer((layer_mask, edge_margin, max_markers))`: `edge_margin` defaults to 24 UI px, `max_markers` to 256.

Everything in a radar or a marker layer is positioned by hand, so neither can size itself. With no `size` and not `absolute`,
a `radar` is 200 px square and a `marker_layer` fills its parent. Names of players and lobbies and score values are data, never
read as `"loc:<key>"`.

## Migrators

```rust
migrators.register(0, "my-game", Arc::new(|text| {
    *text = text.replacen("version: 0", "version: 1", 1);
    // …structural edits on `text`…
    Ok(1) // the built-in v1 → v2 → v3 → v4 steps run next
}));
```

Migrators rewrite RON **text** (not `ron::Value`) so unit enum names survive. A step must
return a higher version than it was registered for; one that does not is an error.

## Example

See [`ui-ron.example.ui.ron`](./ui-ron.example.ui.ron).
