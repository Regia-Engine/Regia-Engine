# UI designer and view-model binding

The editor's **UI Designer** tab edits a runtime UI layout (`.ui.ron`, see [UI](../user-guide/ui.md)) the way it will
look in the game: a node tree on the left, a preview laid out by the runtime's own layout in the middle, and an
inspector with bindings and lint on the right. Layouts bind to *view models*, plain game resources, so the game never
touches widget entities. Spec: `docs/specs/81-ui-designer.md`; decisions: ADR-1811, ADR-2489.

## View models (game side)

A view model is a reflected resource the game registers:

```rust,wrap
# use regia::prelude::*;
# use regia_core::math::{Vec3, Quat};
# use regia_scene::Transform;
# let mut commands: Commands = todo!();
# let world: &mut World = todo!();
# let app: &mut App = todo!();
# use regia_ui::ViewModelAppExt;
#[derive(Resource, Reflect, Default)]
#[reflect(Resource, Default)]
struct SettingsVm { volume: f32, muted: bool, name: String, dirty: bool }

app.register_view_model::<SettingsVm>(); // regia_ui::ViewModelAppExt
```

`register_view_model` registers the type, inserts the default resource when there is none, and lists the model and
its fields in `ViewModels` (the designer's binding picker and lint use it). A binding path is `"Model.field"` (nested
fields with more dots).

## Bindings and commands in `.ui.ron` (v3)

```ron,ui
(
    version: 4,
    root: (
        children: [
            (name: "volume", widget: slider(0.5, 0.0, 1.0, 0.05),
                bindings: [(source: "SettingsVm.volume", target: value, mode: two_way)]),
            (name: "mute", widget: toggle(false),
                bindings: [(source: "SettingsVm.muted", target: checked, mode: two_way)]),
            (name: "apply", widget: button, text: (spans: ["Apply"]), command: "settings.apply",
                bindings: [(source: "SettingsVm.dirty", target: enabled)]),
        ],
    ),
)
```

| Field | Meaning |
|---|---|
| `bindings: [ … ]` | Any number of bindings on the node (the single `bind` still works). |
| `source` | `"Model.field"`. |
| `target` | `text` (default), `progress`, `opacity`, `checked`, `value` (slider), `input` (text input), `selected` (dropdown or tab bar), `visible`, `enabled`. |
| `mode` | `one_way` (default: model → widget when the value changes), `two_way` (also widget → model), `one_time` (pushed once). |
| `format` | `"{:03}"`, `"{:.1}"`, … as for `bind`. |
| `invert` | Negate a boolean for `checked`, `visible`, `enabled`. |
| `command` | Sent as a `UiCommand { name, node, value }` message on a click, or on a text submit (with the text). |

Two-way writes come from the widget's own `ValueChanged` / `Submitted` events, so a click, a key, a gamepad, a touch
and `ui.set_value` all write the model the same way. Only **registered** view models are written. A widget edit is
applied before the game's systems for the frame run; if the game writes the same field later in that frame, the
game's value wins and is pushed back to the widget. Integers round to nearest and must fit the field (a `u8` refuses
300). Binding problems are kept per binding (`ViewModels::status`), not logged every frame:

| Code | Meaning |
|---|---|
| `binding_unresolved` | The model, the resource or a field on the path does not exist. |
| `binding_not_view_model` | A two-way binding points at a resource that is not a registered view model; it acts one way. |
| `binding_type_mismatch` | A widget value cannot be stored in the field (wrong type, out of range). |
| `binding_item_outside_repeat` | A `.field` source on a node that is not in a repeat template; it pushes nothing. |
| `binding_converter_one_way` | A two-way edit through a converter with no way back; nothing is written. |
| `binding_theme_token` | A `theme` binding names a token the active theme does not have (or no theme resolved yet); it pushes nothing. |

In a running game, `ui.view_models {}` lists every registered model with its fields (`name`, `kind`, `type`,
current `value`) and every binding (`node`, `name`, `index`, `source`, `target`, `mode`, `invert`, the `last` value
pushed, and `code` / `error` when it failed), plus an `errors` count. To test a two-way binding, change the widget
with `ui.set_value` and read the field back with `ui.view_models` on the next frame.

## Collections (`.ui.ron` v6)

A node with `repeat` spawns its children once per item of a list. The children are the *template*:

```ron,ui
(
    version: 6,
    root: (
        children: [
            (name: "rows", repeat: (source: "InventoryVm.items", key: "id"), children: [
                (name: "row", widget: button, command: "inventory.equip", children: [
                    (text: (spans: ["-"]), bindings: [(source: ".name")]),
                    (text: (spans: ["0"]), bindings: [(source: ".count", format: "x{:02}")]),
                ]),
            ]),
        ],
    ),
)
```

| Field | Meaning |
|---|---|
| `repeat.source` | A `Vec` or array field: `"Model.field"`, or `".field"` of the enclosing repeat's item (nested repeats). |
| `repeat.key` | A path inside an item to its stable key (`"id"`; `"."` is the item itself). Without it items follow by position. |
| `source: ".name"` | A binding that reads the item; `"."` is the item itself. Other sources in a template stay ordinary bindings. |

The rows follow the list by keyed diff. An item whose key stays keeps its entities and every piece of widget state on
them, also when other items are removed or the list is reordered; a gone key despawns its row, a new key spawns one.
Changing one item writes one row. Key by something stable: keyed by position, removing an item in the middle shifts the
items after it through the rows, and the rows' state stays behind. A command from a row carries its key in
`UiCommand.item`, and a two-way binding writes the item the row belongs to now.

A host refreshes only on frames its model resource changed; an unchanged model costs one tick check. A row is real
entities: 1,000 items are 1,000 rows and every widget in them. For long collections put the repeat on a `list` widget.

### Virtualized lists

A `repeat` on a `list` widget materializes only the visible items plus 2 rows of overscan each side, so a
10,000-item inventory has about 15 rows alive:

```ron
(name: "feed", widget: list(0, 40.0), style: (size: (300, 400)), clip: true,
    repeat: (source: "FeedVm.items", key: "id"),
    children: [
        (name: "row", style: (size: (Percent(100), Percent(100))), children: [
            (name: "label", text: (spans: ["-"]), bindings: [(source: ".name")]),
        ]),
    ]),
```

- Every item is a row `row_height` tall (the second number of `list(…)`) holding one copy of the template. Size the
  template root `Percent(100)`; a fixed height other than the row height reports `repeat_on_list`.
- Scrolling and removing an item spawn nothing: a row whose item left the view is given to the item that entered it,
  and only the bindings whose value differs are written. State the model does not hold (an unbound toggle) stays with
  the row, not the item; bind it.
- The first visible item's key anchors the scroll position: inserting items above the view moves the offset by the
  inserted rows, so what the player is looking at stays put. If that item is removed, the next one takes its place.
- The list widget still owns scrolling, `ui.scroll_to`, selection (rows get `Selected`), the keyboard cursor and
  focus. `ListHeights` and `ListTiles` on the list work as for an ordinary list.

`ui.collection_state { keys? }` reports each repeat node, in spawn order:

```json
{ "hosts": [{ "node": 4294967301, "name": "rows", "source": "InventoryVm.items", "key": "id", "instances": 3,
              "passes": 2, "spawned": 0, "despawned": 1, "moved": false, "touched": 0, "code": null, "error": null,
              "keys": ["1", "3", "4"] }],
  "instances": 3, "errors": 0 }
```

`spawned`, `despawned`, `moved`, `touched` and `recycled` describe the last pass; `passes` counts the frames the model
(or, for a virtualized list, the view) changed. Every host has `virtual`; a virtualized one adds the window:

<!-- not-a-request -->
```json
{ "virtual": true, "items": 10000, "materialized": 15, "spare_rows": 0, "first_index": 198,
  "first_visible_key": "200", "spawned_total": 15, "recycled_total": 30, "instances": 15 }
```

`spare_rows` are hidden rows kept for items that scroll in (the window is shorter at the ends of the list).

`instances` and `keys` cover the materialized rows only. `keys` (the first 256) comes with `keys: true`. Error codes:

| Code | Meaning |
|---|---|
| `repeat_unresolved` | The model, a field on the path or the enclosing item does not exist. Existing rows stay. |
| `repeat_not_a_list` | The source is not a list or an array. |
| `repeat_outside_template` | A `.field` source on a repeat with no enclosing repeat. |
| `repeat_bad_key` | An item has no field at the key path; nothing is spawned or changed. |
| `repeat_duplicate_key` | Items repeat a key; they follow by occurrence (`a`, `a#2`). |
| `repeat_capped` | The list is longer than 65,536 items; the rest are not shown. |

## Converters

A binding can transform its value with `converter`, before `format` and `invert`:

| `.ui.ron` | JSON (`editor.ui_designer_edit`) | Shows | Two way |
|---|---|---|---|
| `number(precision: 2, group: ",")` | `{"number": {"precision": 2, "group": ","}}` | `1234.5` as `1,234.50`; `group` is optional | yes |
| `clamp(min: 0, max: 100)` | `{"clamp": {"min": 0, "max": 100}}` | The number, limited to the range | yes |
| `map_range(from: (0, 100), to: (0, 1), clamp: true)` | `{"map_range": {"from": [0, 100], "to": [0, 1], "clamp": true}}` | The number, mapped linearly; `clamp` is optional | yes, the inverse map |
| `bool_to_visibility` | `"bool_to_visibility"` | `false` for false, `None`, empty text and zero, else `true`, for `visible`, `enabled`, `checked` | no |
| `enum_text(table: [("Idle", "Waiting")], default: "?")` | `{"enum_text": {"table": [["Idle", "Waiting"]], "default": "?"}}` | The text for the value or its variant name, else `default`, else the value | no |
| `loc_key(prefix: "state-")` | `{"loc_key": {"prefix": "state-"}}` | The Fluent message `prefix` + variant name; re-resolves on a language switch | no |

Text a converter cannot read passes through; bad bounds never panic (lint `converter_bad_settings`). There is no
scripting: this is the whole set.

## Theme tokens, animations, template row height and tiled images (`.ui.ron` v11)

Folded into v11 before it shipped (ADR-7751): every v11 file written before still loads unchanged.

- **Theme-token binding**: `(theme: "color.accent")` reads a token of the active theme instead of a field and follows
  a theme switch (and a density or contrast change). A colour shows as `#rrggbb` (`#rrggbbaa` below full alpha), a
  number or a duration as its number. An unknown token reports `binding_theme_token`.
- **Animation binding**: `(target: opacity, animation: (from: 0.25, to: 1, duration: 0.8, delay: 0.1, ease:
  "quad_in_out", repeat: ping_pong))` pushes a number on the game clock (`Time`: pause, time scale and a fixed step
  reproduce). `repeat` is `once` (holds `to`), `loop` or `ping_pong`; `ease` is a tween ease name (linear when absent
  or unknown). Reduced motion shows `to` at once. A finished `once` animation stops writing its target.
- **Row height from the template**: `repeat: (source: "InboxVm.messages", key: "id", row_height: template)` on a
  `list` makes the virtualized rows as tall as the first template node's fixed height (`style: (size: (Auto, 56))`)
  instead of the list's `row_height`. `ui.collection_state` reports `row_height` (`list` or `template`) and `row_px`.
- **Tiled image**: `image: (id: 7, fit: tile)` repeats the picture at its native size from the node's top-left
  corner, clipped to the node.

From an agent: `editor.ui_designer_edit {op: "bind_source", node, binding?, theme | animation | source}` sets where
binding `binding` reads (exactly one of the three; the others are cleared), `{op: "repeat", …, row_height: "list" |
"template"}` sets the row height (absent keeps the node's), and `{op: "image", node, fit: "tile"}` sets the fit
(`null` leaves `fit` unwritten). In the inspector: the binding's **reads** picker (field, theme, animation) with the
token or the animation's numbers, ease and repeat; the repeat's **row height** picker on a list; and `tile` in the
image fit picker.

```text
editor.ui_designer_edit {"op": "bind_source", "node": "#accent", "theme": "color.accent"}
editor.ui_designer_edit {"op": "bind_source", "node": "#pulse", "animation": {"from": 0, "to": 1, "duration": 0.8, "repeat": "ping_pong"}}
editor.ui_designer_edit {"op": "repeat", "node": "#messages", "source": "InboxVm.messages", "key": "id", "row_height": "template"}
editor.ui_designer_edit {"op": "image", "node": "#pattern", "id": 7, "fit": "tile"}
```

## What the designer shows

- **Palette**: one chip per preset. Drag a chip onto a hierarchy row or a preview node: the upper and lower quarter
  place the new node before or after it, the middle makes it a child. One undo step; the new node is selected.
- **Hierarchy**: every node with its kind, `#name` and repeat source. Add a preset under the selection (`panel`, `text`, `button`,
  `toggle`, `slider`, `progress`, `text_input`, `dropdown`, `image`, `list`, `scroll`, `lobby_list`, `scoreboard`, `radar`, `minimap`, `marker_layer`, `split`, `foldable`, `tabs`; the preview fills the game widgets with invented rows, [Game widgets](ui-game-widgets.md)), duplicate, remove, move up
  and down, or drag a row: the upper and lower quarter of a row place the node before or after it, the middle makes
  it a child. Ctrl+Z undoes, Ctrl+Shift+Z and Ctrl+Y redo (200 steps) while the pointer is over the panel and no
  text field has focus; elsewhere the keys stay the scene's undo.
- **Preview**: the layout at a window size preset (1920x1080, 1280x720, 1280x800 handheld, 2400x1080 and 1080x2400
  phones with their safe-area insets) or a custom size: the **size** fields under the preview's toolbar take any
  width and height (1 to 8192 px) and the **safe area** fields the left, top, right and bottom insets (whole px,
  limited to the window as the runtime limits its `SafeArea`; insets that leave no area are refused). Size, insets,
  locale and theme are the open document's view state: they stay while it is open, are not undo steps and are never
  written to the file (`editor.ui_designer_preview`'s `size` and `safe_area` set the same). The green frame is the safe area; nodes that cross it are
  drawn red. Text shows localized spans in the chosen locale from the project's `assets/localization/<locale>/*.ftl`
  (missing keys as `⟦key⟧`), optionally pseudo-localized to find clipped or unlocalized text. Rects are the
  runtime's: the same file at the same size lays out the same in a game. The pixels are a schematic (fills, borders,
  text); tick **Themed** to see the document drawn by the runtime UI renderer with the project's theme (GPU, offscreen;
  lavapipe works), redrawn when the document or the settings change. With **Themed** on, the theme picker beside it
  lists **active theme** (the editor's active theme, the default) and every loaded theme: the built-ins and the
  project's `assets/ui/themes/*.theme.ron`; the pick is the document's `theme` setting, which `theme` on
  `editor.ui_designer_preview` and `editor.ui_designer_render` sets too. A repeat shows sample items (default 3, the `items` box sets it): every row comes from the
  template and shares its selection, and the sample row of a `.name` binding fills all of them. `samples` can set one
  item (`"Vm.items[1].name"`) or one host's count (`"Vm.items[]#count"`). Converters run on samples. A repeat on a
  `list` places its sample rows as the list does, and rows past what the list materializes at the top are hidden.
- **Inspector**: name, classes, text and localization key, command, the widget's typed fields (see *Widget fields*
  below; the widget as JSON under *Advanced*), an image's tint and keep-aspect, typed layout rows (display,
  position, direction, justify, align, size, min and max size, gap, margin, padding, inset, grow, shrink, opacity,
  wrap, clip, interactable, disabled, background color; one drag on a number is one undo step), the repeat source and
  key (with a picker of list fields), the bindings (source with a model and field picker when view models are
  registered, item fields in a template, target, mode, invert, format, the converter picker, and a sample value
  that feeds only the preview), the **Picture** rows (the node's `image` and `mask`: a picker of the project's
  pictures under `assets/` and of the icon atlas's icons, the name as text, nine-slice insets and the mask fit; see
  [UI pictures by asset path or icon](ui-image-assets.md)), and a JSON field editor for any other field
  (`style.size` = `[200, "Auto"]`). A failed edit stays on screen until you dismiss it. Both previews show a picture
  named by asset path from the project's source file.
- **Several nodes**: Ctrl+click a row or a preview node to add it to the selection (or take it out), Shift+click a
  row to select a range. The inspector then shows the shared layout rows (a change goes to every selected node as one
  undo step; fields whose values differ are listed as mixed), align (left, center, right, top, middle, bottom),
  distribute (3 or more nodes), Group (siblings into a new panel), Duplicate and Delete.
- **Clipboard**: Ctrl+C, Ctrl+X, Ctrl+V (or Copy, Cut, Paste in the hierarchy) copy the selected subtrees as node
  JSON, kept when you open another document; a paste lands after the selection, names it would duplicate get `_2`,
  `_3`, and bindings are kept. Ctrl+D duplicates, Delete removes. One undo step each.
- **Lint**: the findings below, refreshed on every change.
- **Save**: rewrites only what changed in the file it was opened from: comments, key order and every other line stay
  byte for byte. A new field goes on its own line after its neighbor; a removed node takes its comments along; a moved
  node carries them. The text is parsed back and compared before a temp file and a rename put it in place. If the
  result would differ, the canonical form (only fields that differ from their defaults) is written and the panel says
  the comments were not kept. A new document is written in the canonical form.

## Moving and resizing in the preview

Drag a node in the preview to move it; drag one of the eight handles of a lone selected node to resize it. The node
moves live and the whole drag is one undo step. What is written depends on the node:

| Node | Move writes | Resize writes |
|---|---|---|
| `position: absolute` | `style.inset` (the sides that are set; with none, `left` and `top`) | `style.inset` for a moved left or top edge, `style.size` |
| In the flow | `style.margin`, or a new place among its siblings when its centre lands in a sibling's slot (upper half: before, lower half: after) | `style.size`, and `style.margin` for a moved left or top edge |

Lengths keep their kind (points stay points, a percentage stays a share of the parent). The move snaps to the
parent's edges and centre, the siblings' edges and centres, and a grid (the toolbar's **Snap** and **grid**); guides
show what it snapped to, and the offset and size show in UI px while you drag. Shift keeps the aspect ratio of a
resize; Alt turns snapping off for one drag. Guides are not saved. The wheel zooms about the pointer, a middle drag
pans, **Fit** and **100 %** reset the view; rulers show frame px and the phone and handheld sizes get a device frame
(the toolbar's **Rulers** and **Device** boxes).

With several nodes selected (Ctrl+click), dragging one of them moves them all by the same delta, as one undo step. A
selected node inside another selected node rides along with it. The snap works on the group's bounding box: against
the shared parent's edges, the parent's other children and the grid (against the window when the nodes have
different parents). Each node then writes what its own move writes (table above), but a node in the flow is never
reordered by a group drag; it writes its margin. The selection stays as it was. The agent's
`transform {nodes?, delta: [dx, dy], snap?}` does the same in frame px (without `nodes`, the selection moves).

## Widget fields

`set {node, key: "widget.<field>", value}` changes one field of the node's widget, decoded by the format's own types:
the typed inspector rows make the same edit, and the result saves the same bytes as `set {key: "widget"}` with the
whole widget. Other `widget.*` keys are plain JSON paths (`widget.scoreboard.title`).

| Widget | Fields (`widget.<field>`) |
|---|---|
| `button`, `focusable` | none |
| `toggle` | `checked` (bool) |
| `slider` | `value`, `min`, `max`, `step` (numbers) |
| `progress` | `value` (number) |
| `text_input` | `placeholder` (text), `multiline` (bool); `.ui.ron` v11: `max_chars` (whole number, `0` = no limit), `secret` (one character, or `null` for none), `read_only` (bool), `filter` (`"any"`, `"integer"`, `"numeric"`) |
| `dropdown` | `options` (list of texts), `selected` (index or `null`) |
| `tab_bar` | `tabs` (list of texts), `active` (index) |
| `modal` | `open`, `close_on_cancel` (bools) |
| `tooltip` | `text` (text), `delay` (seconds) |
| `scroll` | `inertia` (seconds), `scrollbar` (bool) |
| `list` | `items` (count), `row_height` (UI px) |
| `stick` | `action` (text), `radius` (UI px) |
| `v_button` | `action` (text) |
| `popup_menu`, `context_menu`, `menu_bar` | `entries` (JSON list of `{ id?, label, separator?, enabled?, checkable?, checked?, shortcut?, submenu? }`) |
| `lobby_list` | `title`, `refresh`, `create`, `empty`, `ready`, `leave`, `launch` (texts; `"loc:<key>"` allowed) |
| `scoreboard` | `title` (text), `columns` (JSON list of `{ key, title, width }`), `sort_column` (index or `null`), `descending`, `group_by_team` (bools), `hold_action` (text), `from_rules` (bool) |
| `radar` (also the minimap) | `range` (m), `world_rect` (`[min_x, min_z, max_x, max_z]` or `null`), `square`, `rotate` (bools), `map_image` (id), `map_rect` (`[…]` or `null`), `layer_mask`, `max_blips` (whole numbers), `show_north` (bool) |
| `marker_layer` | `layer_mask` (whole number), `edge_margin` (UI px), `max_markers` (whole number) |
| `split` | `axis` (`"horizontal"`, `"vertical"`), `ratio`, `min_ratio`, `max_ratio` (shares 0..1 of the first pane) |
| `foldable` | `title` (text; `"loc:<key>"` allowed), `open` (bool), `group` (text, or `null`/empty for none) |
| `tabs` | `tabs` (list of texts), `active` (index) |

A text input with no options reads them as their defaults; setting one writes the options element
(`text_input("PIN", false, (max_chars: 4))`), and putting every option back to its default drops it again. A
`max_chars` below 0, a `secret` of more than one character and an unknown `filter` are `ui_designer_bad_value`.

An image's `image.tint` (`[r, g, b, a]`), `image.preserve_aspect` and, since `.ui.ron` v11, `image.fit` (`"stretch"`,
`"contain"`, `"cover"`; `null` removes it so `preserve_aspect` decides) are ordinary `set` keys. `cover` fills the node and
crops the picture's overflow, centred; `"tile"` is refused (`ui_designer_bad_value`): only a `mask` tiles. The format has
no stored selection on a list (it is runtime state).

## Containers, markup and focus neighbors (`.ui.ron` v11)

ADR-7827 folds three containers, markup text and focus neighbors into `.ui.ron` v11 ([format](../../docs/formats/ui-ron.md)):

- **Containers**: the `split`, `foldable` and `tabs` presets add the container with its parts. A split's two children are
  its panes (the preset adds two empty panels; the runtime puts a draggable divider between them); a foldable's children
  are its body, under a header showing its title (foldables sharing a `group` under one parent are an accordion); a tabs
  container's child `i` is the page of tab `i` (the preset adds one text page per tab), and the preview shows only the
  active page and only the bodies of open foldables. Their fields are typed rows (*Widget fields* above).
- **Markup**: the **Markup** box under the text rows sets `text.markup`; with it on, `<b>`, `<i>`, `<u>`, `<s>`,
  `<color=#rrggbb>`, `<size=px>` and `<icon=name>` in the literal text are read by the runtime's markup parser, and the
  row below the box shows the parsed text (emphasis, the markup's colors and sizes). Agents: `set {key: "text.markup",
  value: true}` (`null` turns it off).
- **Navigation**: the inspector's **Navigation** section has one row per direction (up, down, left, right) naming the
  node that keyboard or gamepad navigation reaches from this one. Type the node's name (Enter applies; an empty row
  clears the neighbor), or press **Pick** and click the node in the preview (the next click names it and does not change
  the selection; Pick again cancels). Neighbors are stored by name, so a picked node needs a name. The toolbar's **Show
  navigation** draws an arrow from each node to each declared neighbor (an unknown name draws a short arrow in the error
  color); it is a view setting and never changes the file. Agents: `set {key: "nav.down", value: "quit"}` (`null`
  clears). In a running game, `ui.nav_graph` shows each node's `name` and the neighbors it `declared`, beside the
  resolved `links` ([UI navigation](ui-navigation.md)). A declared neighbor wins over the geometric search only while
  its target is navigable.

## Methods

Mutations need `edit_scene`; opening and saving need `assets`. Relative paths are relative to the project root; an
absolute path must lie under the project or the temp dir, and `..`, drive letters, UNC forms and symlinks leading out
are `path_outside_project` ([Request paths](safety.md#request-paths), ADR-7388). A save path (and a `new` document's
`path`) must end in `.ui.ron` (`ui_designer_bad_value`).
Nodes are addressed by path (`"/"` is the root, `"/0/2"` child 2 of child 0) or by `"#name"` (first match).

| Method | What it does |
|---|---|
| `editor.ui_designer_open { path? , new? }` | Open a `.ui.ron` (older versions are migrated in memory; the file is untouched until a save) or start an empty document (`new: true`, optional `path` to save to). Returns the summary and the `tree`. |
| `editor.ui_designer_state { node? }` | Summary (`path`, `version`, `opened_version`, `dirty`, `revision`, `undo`, `redo`, `had_comments`, `last_save_canonical`, `comments_lost`, `selection`, `selected`), the `tree` (`path`, `depth`, `name`, `kind`, `classes`, `bindings`, `command`, `children`, `repeat`, `in_template`), the `presets` and `sizes`, and one node's `fields` in full (the selection, or `node`). With several nodes selected, `shared` (dotted field -> the value they all have) and `mixed` (the fields whose values differ). |
| `editor.ui_designer_edit { op, … }` | `add {parent?, index?, preset \| node}`, `remove {node}`, `move {node, parent, index?}`, `duplicate {node}`, `set {node, key, value}`, `repeat {node, source?, key?, row_height?}` (the node's children become the template; no `source` removes the repeat; `row_height` `list` or `template`, absent keeps the node's), `converter {node, binding?, converter}` (`binding` counts the single `bind` first, then `bindings`; default 0; `converter` as in the table above, `null` clears), `bind_source {node, binding?, source \| theme \| animation}` (exactly one; see *Theme tokens, animations*), `image {node, asset \| icon \| id, nine_slice?, preserve_aspect?, fit?, remove?}` and `mask {node, asset \| icon \| id, fit?, remove?}` (name the picture by manifest path, icon-atlas name or renderer id; `nine_slice` `[l, t, r, b]` source px, `null` removes it; `remove: true` drops the field), `select {node?}`, `undo`, `redo`. Several nodes: `select_many {nodes}` (the first is the primary); `set`, `remove` and `duplicate` take `nodes` (and `set` without `node` works on the selection) as one step; `align {nodes?, align}` (`left`, `center`, `right`, `top`, `middle`, `bottom`, `distribute_h`, `distribute_v`; 2 nodes or more, 3 to distribute); `group {nodes?, name?}` (siblings into a new panel at the first one's place). `transform {node, rect, mode?, snap?, keep_aspect?, reorder?}` moves (`mode: "move"`, default; the size in `rect` is ignored) or resizes a node to `rect` (`[x, y, w, h]` in the preview frame's px, as `editor.ui_designer_preview` reports) as the preview drag does; `snap` is `true` (the parent's edges, the siblings and an 8 px grid within 6 px), `{edges?, siblings?, grid?, threshold?}` or `false` (default: the rect is taken as given); `reorder: false` makes a flow node's move write its margin. It returns `rect` (after snapping), `writes` (`inset`, `size`, `margin`, `order`), `guides` (`[{ axis, at, span, source }]`) and `changed`. `transform {nodes?, delta, snap?}` (no `node`, no `rect`) moves every top-level node of `nodes` (default: the selection) by `delta` (`[dx, dy]` frame px) as one step, snapping the group's bounding box, and returns `delta` (after snapping), `rect` (the group's box), `guides`, `moved` (the paths that moved) and `changed`. `copy {nodes?}` and `cut {nodes?}` return `clipboard` (the nodes as JSON) and keep it for `paste {text?, parent?, index?}`, also after another document is opened. `transform` with both `rect` and `delta`, `transform` with `rect` and a `nodes` list, and any op given both `nodes` and `node` (neither `null`), is refused (`F0002`, invalid params) and changes nothing. A `parent` of `null` on `add` and `paste` is the default parent (the selection; for `paste` the paste target), as when the key is absent. Each mutating op is one undo step. `select` and `select_many` only change the selection and do not push a step; `undo` and `redo` move the existing stack. A failed op changes nothing. `set` takes a dotted `.ui.ron` field (`name`, `classes`, `style.padding`, `bindings`, `command`, `widget`, `widget.<field>`, `text`) and a JSON value in the `.ui.ron` shape; `null` resets the field; `text` also takes a plain string. |
| `editor.ui_designer_images {}` | The document's pictures: `references: [{ node, name, field, kind, value, found }]` (`kind` `asset`, `icon` or `id`; `found` says whether the project has the file under the spelling the runtime looks up, or the icon atlas the icon, `null` when that cannot be told: `./assets/ui/a.png` or a name with leading or trailing spaces is `false`, because the game does not find it), `assets` (the distinct paths the cook packs), `missing`, and the picker lists `pictures` (the project's `png`, `jpg`, `jpeg`, `tga` and `bmp` under `assets/`) and `icons`. |
| `editor.ui_designer_lint {}` | `{ schema, errors, findings: [{ code, severity, node, message }] }`; `schema` says whether view models were known. |
| `editor.ui_designer_preview { size?, safe_area?, locale?, pseudo?, samples?, repeat_count?, visible_only?, theme? }` | `{ size, safe_area, scale, outside_safe_area: [path], truncated, nodes: [{ path, item, name, kind, rect, clip, visible, text, outside_safe_area }], locale, pseudo, theme, revision }`. `theme` names the theme the themed preview draws with (`null`: the active one); it is only stored here. `rect` is `[x, y, w, h]` in physical px. `samples` maps a binding source to a value shown in its target; under a repeat, `"Vm.items[].name"` (every item), `"Vm.items[1].name"` (one item), `"Vm.items[]#count"` (the item count). `repeat_count` is the default item count (3; at most 256). A row reports its template's `path` and its `item`; `truncated` says items were left out to stay under 8,192 nodes. `lists: [{ path, items, row_height, materialized, first_visible_key }]` reports each repeat on a `list`: how many rows it materializes at the top of its range and the first visible key (its `"Vm.items[0].id"` or `"Vm.items[].id"` sample, else `"0"`); rows past `materialized` are `visible: false`. Settings stick to the document. |
| `editor.ui_designer_render { size?, safe_area?, locale?, pseudo?, samples?, repeat_count?, theme? }` | Draw the same document with the runtime UI renderer and the project theme (the editor's themes, then `assets/ui/themes/*.theme.ron`; `theme` picks another and, like every setting, sticks to the document; a theme that does not draw does not stick) offscreen. `{ width, height, theme, theme_errors, drawn_pixels, mime, png_base64, revision }`; `drawn_pixels` counts pixels that differ from the backdrop (0: nothing drawn). Needs a Vulkan device; lavapipe counts. |
| `editor.ui_designer_save { path?, canonical? }` | Write the document, keeping the opened file's comments and key order; `canonical: true` writes the canonical form instead. Returns `saved`, `last_save_canonical` and `comments_lost`. |

Lint codes:

| Code | Severity | Meaning |
|---|---|---|
| `duplicate_name` | warning | Two nodes share a name; `#name` and `ui.query` find the first. |
| `binding_empty_source` | error | The source is not `Model.field`, or a `theme` token is empty. |
| `binding_target_mismatch` | error | The target does not fit the node (e.g. `value` without a slider). |
| `binding_two_way_read_only` | warning | `two_way` on a target the UI never changes (`text`, `progress`, `opacity`, `visible`, `enabled`), or on a theme or animation binding (nothing to write back). |
| `command_not_interactable` | error | A `command` on a node that gets no clicks. |
| `root_field_ignored` | warning | `spawn_ui` uses only the root's `style` and `children`; other root fields (`safe_area`, `name`, …) have no effect. |
| `binding_unknown_model` | warning | No registered view model of that name (only when the game registered some). |
| `binding_unknown_field` | error | The model or the item has no such field. |
| `binding_item_outside_repeat` | error | A `.field` source on a node outside a repeat template. |
| `repeat_bad_source` | error | The repeat source is not `Model.field` or `.field`. |
| `repeat_outside_template` | error | A `.field` repeat source with no enclosing repeat. |
| `repeat_not_a_list` | error | The field is not a list (only when the game registered view models). |
| `repeat_unknown_field` | error | The model or the item has no such field. |
| `repeat_empty_template` | warning | A repeat with no children spawns nothing. |
| `repeat_no_key` | warning | Items follow by position: a removal in the middle shifts the rows' state. |
| `repeat_on_list` | warning | A repeat on a `list` is virtualized into rows of the list's row height; a template root with another fixed height overlaps or leaves gaps. |
| `converter_bad_settings` | warning | Reversed or non-finite bounds, a zero-width input range, an empty table. |
| `converter_target_mismatch` | warning | The converter's result does not fit the target (e.g. `loc_key` on `progress`). |
| `converter_one_way` | error | `two_way` through a converter with no way back. |
| `binding_bad_animation` | warning | An animation with an unknown ease, a duration that is not a positive number, a negative or non-finite delay, or non-finite ends. |
| `binding_source_ignored` | warning | A binding writes more than one of `source`, `theme`, `animation`; `theme` wins, then `animation`. |
| `repeat_template_height` | warning | `row_height: template` on a repeat that is not a `list`, or whose first template node has no fixed height. |
| `split_panes` | warning | A split without exactly two children (a third and later child gets no share). |
| `split_ratio` | warning | `ratio`, `min_ratio` or `max_ratio` outside 0..1, or `min_ratio` above `max_ratio` (the runtime clamps). |
| `tabs_pages` | warning | The number of tabs and of child pages differ. |
| `tabs_active` | error | `active` is past the last tab. |
| `nav_unknown_target` | error | A `nav` neighbor names no node of the document. |
| `nav_target_not_focusable` | warning | A `nav` neighbor names a node that is not focusable; navigation falls back to geometry. |
| `nav_not_focusable` | warning | `nav` on a node that is not focusable: its neighbors are never used. |
| `nav_self` | warning | A `nav` neighbor names the node itself. |

Errors carry `data.code`:

| Code | Meaning |
|---|---|
| `ui_designer_no_document` | Nothing is open; call `editor.ui_designer_open`. |
| `ui_designer_bad_node` | No node at that path or name, or the root where a child is needed. |
| `ui_designer_bad_op` | Unknown `op`. |
| `ui_designer_bad_value` | The value does not decode as that field, an unknown field, a bad index, preset, converter or `repeat_count`. |
| `ui_designer_bad_move` | A node cannot move into its own subtree, or the index is past the end. |
| `ui_designer_nothing_to_undo` / `ui_designer_nothing_to_redo` | The history is empty that way. |
| `ui_designer_parse` | The file is not a loadable `.ui.ron` (or the written text would not load). |
| `ui_designer_io` | Reading or writing the file failed, or a save has no path. |
| `ui_designer_bad_size` | Preview size outside 1..=8192, unknown preset, or insets that leave no area. |
| `ui_designer_unknown_theme` | `editor.ui_designer_render`: no loaded theme of that name. |
| `ui_designer_no_gpu` | `editor.ui_designer_render`: no Vulkan device (install lavapipe or a driver); `editor.ui_designer_preview` needs none. |
| `ui_designer_render_failed` | `editor.ui_designer_render`: the offscreen readback failed. |
| `editor_headless` | `editor.ui_designer_render` on a headless editor without an adapter (`--no-gpu`, or none found): it opens no device of its own (ADR-5180); `editor.ui_designer_preview` still answers. |

## Example

```json
{ "method": "editor.ui_designer_open", "params": { "path": "assets/ui/settings.ui.ron" } }
{ "method": "editor.ui_designer_edit", "params": { "op": "add", "parent": "/", "preset": "slider" } }
{ "method": "editor.ui_designer_edit", "params": { "op": "set", "node": "/3", "key": "bindings",
  "value": [{ "source": "SettingsVm.volume", "target": "value", "mode": "two_way" }] } }
{ "method": "editor.ui_designer_lint", "params": {} }
{ "method": "editor.ui_designer_preview", "params": { "size": "2400x1080", "samples": { "SettingsVm.volume": 0.8 } } }
{ "method": "editor.ui_designer_save", "params": {} }
```

A list with sample rows:

```json
{ "method": "editor.ui_designer_edit", "params": { "op": "add", "parent": "/", "preset": "panel" } }
{ "method": "editor.ui_designer_edit", "params": { "op": "add", "parent": "/3", "preset": "text" } }
{ "method": "editor.ui_designer_edit", "params": { "op": "repeat", "node": "/3", "source": "InventoryVm.items", "key": "id" } }
{ "method": "editor.ui_designer_edit", "params": { "op": "set", "node": "/3/0", "key": "bindings", "value": [{ "source": ".name" }] } }
{ "method": "editor.ui_designer_preview", "params": { "repeat_count": 5, "samples": { "InventoryVm.items[].name": "Sword" } } }
```

In the running game, check the rows with `ui.collection_state`.

Then check the result in the running game with `ui.query` and `ui.set_value` (the two-way path), see [UI](ui.md).

Moving, aligning and pasting:

```json
{ "method": "editor.ui_designer_edit", "params": { "op": "transform", "node": "#hp", "rect": [60, 80, 300, 40], "mode": "move", "snap": true } }
{ "method": "editor.ui_designer_edit", "params": { "op": "transform", "node": "#hp", "rect": [60, 80, 420, 40], "mode": "resize", "snap": { "grid": 10 } } }
{ "method": "editor.ui_designer_edit", "params": { "op": "select_many", "nodes": ["#hp", "#mana", "#stamina"] } }
{ "method": "editor.ui_designer_edit", "params": { "op": "set", "key": "opacity", "value": 0.8 } }
{ "method": "editor.ui_designer_edit", "params": { "op": "align", "align": "left" } }
{ "method": "editor.ui_designer_edit", "params": { "op": "set", "node": "#volume", "key": "widget.max", "value": 2.0 } }
{ "method": "editor.ui_designer_edit", "params": { "op": "set", "node": "#pin", "key": "widget.max_chars", "value": 4 } }
{ "method": "editor.ui_designer_edit", "params": { "op": "set", "node": "#pin", "key": "widget.secret", "value": "*" } }
{ "method": "editor.ui_designer_edit", "params": { "op": "set", "node": "#banner", "key": "image.fit", "value": "cover" } }
{ "method": "editor.ui_designer_edit", "params": { "op": "transform", "nodes": ["#hp", "#ammo", "#map"], "delta": [10, 20], "snap": true } }
{ "method": "editor.ui_designer_edit", "params": { "op": "copy", "nodes": ["#hp"] } }
{ "method": "editor.ui_designer_open", "params": { "path": "assets/ui/pause.ui.ron" } }
{ "method": "editor.ui_designer_edit", "params": { "op": "paste", "parent": "/" } }
```


## Layout rows and preview scale (w12)

Layout has a 4 × 4 anchor preset picker. With **Keep position** checked, applying a preset
recomputes offsets from the current runtime layout and keeps the border rectangle at this
preview size. Uncheck it to **Snap to preset** with zero offsets. Each preset is a named,
keyboard-focusable button. This is one undo step; changing preview size remains view state.
The root fills the window and cannot receive a parent-relative preset.

```json
{"method":"editor.ui_designer_edit","params":{"op":"anchor_preset","node":"/0","preset":"bottom_right","keep_position":true}}
```

`preset` accepts the 16 names in [UI anchors](ui-anchors.md); `keep_position` defaults to true.
Keep position uses the measured size for fixed axes, even when the previous size was automatic.
Snap keeps automatic sizes on edge-pinned axes and measures them when centering needs a fixed size.
`ui_designer_bad_node` refuses a missing node or the document root; `ui_designer_bad_value` refuses an unknown preset,
invalid geometry, a non-boolean `keep_position`, or a layout whose rectangle cannot be preserved.
Keep position remeasures an automatic parent after the child leaves normal flow before committing
one edit. Errors leave the document unchanged.
Pivot is a numeric layout row (`pivot`, ADR-8252), not a Shift-click. Shift-click stays selection.
The runtime still exposes `AnchorPreset::pivot_ratio`.

The aspect-ratio row locks/unlocks `style.aspect_ratio` and edits a positive ratio.
Text effects have typed color, width, X/Y offset and blur fields, named Up/Down/Remove layer
buttons and Add outline/Add shadow buttons. They edit `text.style.effects`, front to back,
with the runtime's four-layer limit for additions. Agents use the existing `set` operation:

```json
{"method":"editor.ui_designer_edit","params":{"op":"set","node":"/0","key":"style.aspect_ratio","value":1.7777778}}
{"method":"editor.ui_designer_edit","params":{"op":"set","node":"/0","key":"text.style.effects","value":[{"color":[0,0,0,1],"width":1,"offset":[0,0],"blur":0}]}}
```

The preview shows its effective scale, for example **Scale 1.50x**. Both
`editor.ui_designer_preview` and `editor.ui_designer_render` accept:

- `scale`: manual scale in 0.05..=16; null returns to the scale rule. Manual scale bypasses the DPI curve.
- `scale_mode`: a runtime `ScaleMode` JSON value, or null for the runtime default.
  Examples: `"ConstantPixelSize"`, `"ConstantPhysicalSize"`,
  `{"ScaleWithScreen":{"reference":[1920,1080],"match_width":0.5}}`,
  `{"IntegerScale":{"reference":[320,180]}}`. References must be positive and finite;
  `match_width` must be in 0..=1.
- `dpi`: simulated display DPI, 24..=1536, default 96. The physical-size mode reads the
  project's existing DPI curve; the screen-size modes use the preview resolution.
- `box_model`: show margin and padding bands for the selection on the canvas.

These controls never dirty the UI document. Preview replies include `scale` (effective),
`scale_override`, `scale_mode`, `dpi` and `box_model`; each node includes its effective
`layout_dir` (`ltr` or `rtl`). When box-model preview is enabled, each node also has
`box_model_bands: [margin_rects, padding_rects]` in physical pixels, computed from its spawned
style so component inheritance is included; otherwise this field is null. Bad scale parameters return `ui_designer_bad_value`
without replacing the previous settings. Bands share the runtime inspector's geometry,
with theme warning/success tints for margin/padding; like the runtime inspector, they show
point-valued spacing only. Percentage and auto spacing are not visualized.

## Project UI scale settings

Project Settings → UI edits the display-DPI curve and the project scale rule with keyboard-operable
radios and numeric rows. Apply writes `assets/ui/project.uisettings.ron` atomically; Reload discards
the draft. Each write adds one editor undo entry, keeps `layer_budget_bytes`, and invalidates cached
previews. Canonical RON replaces comments. v2 (ADR-8252) stores an optional `scale_mode`. The
`dpi_curve` still maps display DPI to scale for `ConstantPhysicalSize` roots. There is no resolution
curve. A root the game already set to something other than the engine default keeps that mode; a
root still on the engine default takes the saved rule. Preview rule overrides stay on
`editor.ui_designer_preview` and win over the saved rule.

`editor.ui_scale_settings { action?, dpi_curve?, scale_mode? }` requires `edit_scene`. `action` is
`get` (default) or `set`. A set sends `dpi_curve`, `scale_mode`, or both. An omitted field stays.
Null clears that field. `dpi_curve` is a nonempty array of `[display DPI, scale]` points with
strictly increasing positive DPI and positive finite scales. `scale_mode` is a `ScaleMode` JSON
value: `"ConstantPixelSize"`, `"ConstantPhysicalSize"`,
`{"ScaleWithScreen":{"reference":[1920,1080],"match_width":0.5}}`, or
`{"IntegerScale":{"reference":[320,180]}}`. References must be finite and above zero. `match_width`
must be in 0..=1. Get returns defaults when the file is absent.

```json
{"method":"editor.ui_scale_settings","params":{"action":"get"}}
{"method":"editor.ui_scale_settings","params":{"action":"set","dpi_curve":[[96,1],[144,1.5],[192,2]]}}
{"method":"editor.ui_scale_settings","params":{"action":"set","scale_mode":{"IntegerScale":{"reference":[320,180]}}}}
```

The result is `{path, version, dpi_curve, scale_mode, layer_budget_bytes, applied}` (`applied:false`
on get). Errors: `no_project` (open a project), `ui_scale_action` (bad action), `ui_scale_curve`
(invalid or missing curve, or an invalid `scale_mode`; a set that sends neither field uses this
same code), `ui_scale_read` (unreadable/invalid existing file), `ui_scale_write` (write failure),
and the shared path-validation error `path_outside_project`. A refused edit leaves the file and undo
stack unchanged. Schema/envelope errors retain the protocol's shared `F0002` code.

## Screen back, subtree disable and pivot

`.ui.ron` stays at version 11. These fields are a fold into that version (ADR-8252): every earlier
v11 file still loads. `back` is a file field, `ignore` or `pop`. Absent leaves the stack default.
`pop` pops the screen on Escape or gamepad B while it is the top screen. `disabled_subtree` on a
node disables pointer and focus input for that node and its descendants. Put it on a child: the
document root ignores node fields other than style and children. `pivot` is an optional `[x, y]`
fraction of the node's layout rect. Absent keeps the centre `[0.5, 0.5]`. Any finite ratio is kept,
including one outside 0..=1.

The designer shows **Disable subtree** and **Pivot** on the layout rows, and **Screen back**
(**Stack default**, **Ignore**, **Pop**) when the root is selected. All of them are in the Tab
order. There is no Shift-click pivot: Shift-click stays selection.

```json
{"method":"editor.ui_designer_edit","params":{"op":"set","node":"/0","key":"disabled_subtree","value":true}}
{"method":"editor.ui_designer_edit","params":{"op":"set","node":"/0","key":"pivot","value":[0,1]}}
{"method":"editor.ui_designer_edit","params":{"op":"set","key":"document.back","value":"pop"}}
```

`document.back` accepts `"pop"`, `"ignore"`, or null. A bad value is `ui_designer_bad_value`.

## Interactive preview and style history (ADR-7811)

Choose **Play** in the canvas toolbar to operate the preview with the existing samples. The
preview runs its own runtime UI and reflected sample model: hover, presses, focus, scrolling,
text input, toggles and lists use the runtime widget systems. Commands appear in a recorded
list; they do not invoke game code. Two-way bindings write only to the private sample model.
Text input writes its two-way value on submission, as it does in the game. **Esc** or **Play**
returns to editing and discards those transient values. Document edits and designer shortcuts
are disabled during playback. Tab reaches Play and the canvas; inside the focused canvas, Tab
and arrow keys navigate runtime widgets. Escape always leaves playback.

`editor.ui_designer_preview` adds optional `interactive: boolean` and `input: object` parameters.
Omitting `interactive` keeps the current mode. With playback active, `input` takes `node` (an
authored path or `#name`) and `action`: `click`, `focus`, `type`, `set_value`, `list` or `scroll_to`.
`type` takes `text` and replaces the field's content, then submits it in a separate runtime
frame. `set_value` takes the runtime widget's `value`. `list` selects `index` using the list's
current selection mode (optional `mode`: `none`, `single`, or `multi` changes only the scratch list); `scroll_to` reveals `index`. Repeated nodes resolve to their first
sample instance. Returned `interactive`, `commands` and `samples` report mode, recent command
events and current sample strings. Reopening a document or stopping playback resets the session.
Changing document inputs or the selected theme rebuilds the scratch runtime.
`editor.ui_designer_render` renders that same live instance when playback is active.

```json
{"method":"editor.ui_designer_preview","params":{"interactive":true,"samples":{"SettingsVm.name":"Ada"}}}
{"method":"editor.ui_designer_preview","params":{"input":{"action":"type","node":"#name","text":"Grace"}}}
{"method":"editor.ui_designer_preview","params":{"interactive":false}}
```

The selected node's **Style** section shows each property and its winning source. Earlier
sources remain visible with struck-through text. Theme entries identify the class or variation,
state and referenced token names. **Open token** opens the Theme panel filtered to that token;
it preserves an unsaved theme draft by refusing to replace it. Both the section and token buttons
are keyboard operable.

`editor.ui_designer_style_sources {node?, open_token?}` returns the preview node's runtime
inspection, including `properties[].sources`: ordered `{source, kind, detail, value, tokens?,
winning}` records. `node` defaults to the selection. `open_token` must name a token present in
those records and opens the same Theme panel path. Permission: `read` (view state only).
Only the last source wins; `kind` distinguishes `class`, `variation`, `token`, `inline`,
`default`, and runtime binding/animation overrides. No content format changes.

Errors use the existing designer codes: `ui_designer_no_document`, `ui_designer_bad_node`,
`ui_designer_bad_value` (invalid input, unavailable widget action, token or dirty theme draft),
`ui_designer_bad_size`, `ui_designer_parse`, and `ui_designer_unknown_theme`. The render route
also retains its existing device/readback errors. A rejected live input never edits the document.

## Layout variants (ADR-7858)

A document can carry **layout variants**: override sets chosen by the window's orientation and size. At run time the
first variant whose `when` matches the window (logical px: physical size over the OS scale) applies on top of the
base; with no match the base shows. When the window changes so that another variant matches (a resize, a rotation),
each window root spawned from the document is laid out again once, with focus and scroll kept by node key; world-space
and camera-target roots keep the variant they spawned with, also when they share the document. A variant changes
fields only, never the tree: `children` and `name` cannot be overridden.

```ron
variants: [
    (
        name: "phone portrait",
        when: (orientation: portrait, max_width: 1200.0),
        overrides: {
            "play": { "style.inset": [40.0, 900.0, "Auto", "Auto"] },
            "art": { "style.display": "none" },
        },
    ),
],
```

`when` takes `orientation` (`portrait`: taller than wide; a square window is landscape), `min_width`, `max_width`,
`min_aspect` and `max_aspect` (width / height); every part is optional and every bound inclusive. An override's node
key is a node name or a path from the root (`"/0/2"`); its field key is the dotted key `editor.ui_designer_edit`
`set` takes, and its value has the same shape (`null` resets the field to its default).

In the designer, the canvas toolbar's **Variant** row lists **Base**, then each variant. Selecting a variant switches
the preview to a size where the game shows it; while a variant is selected, every field edit (inspector rows, canvas
drags, `editor.ui_designer_edit` `set`) writes that variant's overrides only, so the base stays byte for byte, and
structure edits (add, remove, move, duplicate) are refused with `ui_designer_variant_active`. The inspector lists the
selected node's overridden fields, highlighted, each with a **Reset** button (and **Reset all**). With **Base**
selected, the preview still shows the variant its size picks (the toolbar says which), so resizing the preview from
landscape to portrait shows the portrait layout. **Add portrait** and **Add landscape** add a variant and select it;
**Remove variant** deletes the selected one. Every control is a focusable button: Tab to it, then Space or Enter.

`editor.ui_designer_variant` is the agent route:

```json
{ "method": "editor.ui_designer_variant", "params": { "action": "add", "name": "phone portrait", "when": { "orientation": "portrait", "max_width": 1200 } } }
{ "method": "editor.ui_designer_edit", "params": { "op": "set", "node": "#play", "key": "style.inset", "value": [40, 900, "Auto", "Auto"] } }
{ "method": "editor.ui_designer_variant", "params": { "action": "list" } }
{ "method": "editor.ui_designer_variant", "params": { "action": "reset", "node": "#play", "key": "style.inset" } }
{ "method": "editor.ui_designer_variant", "params": { "action": "select" } }
{ "method": "editor.ui_designer_variant", "params": { "action": "remove", "index": 0 } }
```

`action` is `list` (the default), `add` (`name`, `when`; one undo step; selects the new variant), `select` (`index`,
none for Base; view state, no undo step), `remove` (`index`; one undo step) or `reset` (`node`, optional `key`; one
undo step). Every action answers `{ variants: [{ index, name, label, when, describe, overrides: { <node>: [keys] },
sample_size }], active, shown, preview: { size, safe_area }, revision, undo }`, plus `index` and `reachable` (add),
`reachable` (select: `false` when an earlier variant matches every size this one does, so no preview size shows it;
the size is kept) and `removed` (reset).

Error codes (`data.code`): `ui_designer_bad_variant` (no such variant, or reset with Base selected),
`ui_designer_bad_when` (a bound is not a finite number >= 0, or a minimum is above its maximum),
`ui_designer_variant_active` (a structure edit while a variant is selected), `ui_designer_variant_locked`
(`name` or `children` set in a variant), `ui_designer_no_override` (reset of a field the variant does not override),
plus the shared `ui_designer_no_document` and `ui_designer_bad_node`.

In the running game, `ui.inspect` reports the variant a node's root shows as `layout_variant: { template, index,
name, variants, nodes, node_overridden, problems }` (`null` for a root built in code); `problems` lists overrides
that did not apply (an unknown node, a value of the wrong shape), which are also logged once per change.
