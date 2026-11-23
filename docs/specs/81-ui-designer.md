# Spec 81 — UI designer and view-model binding (v0.5.0, T2.13)

Status: proposed (ADR-1811, ADR-2489; direct manipulation, typed widget fields, multi-select and the clipboard
built in v0.6.0 Stage 2, ADR-9730, §§ 4.7-4.9; group drag, the text input options and image fit rows with `.ui.ron` v11,
ADR-9980). Builds on `specs/18-runtime-ui.md` § 5 (`.ui.ron`, `Bind`) and
`specs/19-editor.md` (panels, undo, agent surface). Master plan row "UI designer (WYSIWYG)", `specs/22-tier2.md` T2.13.

## 1. Purpose

Two halves that ship together:

1. **View-model binding (runtime, `regia_ui`).** A game exposes UI state as plain reflected resources (*view
   models*). A `.ui.ron` node binds widget properties to view-model fields, one way (model → widget), two way (a
   widget edit writes the field back) or once, and names a *command* that a click or a submit sends to the game.
   The layout file stays free of game code; the game never touches widget entities.
2. **UI designer (editor, `regia_editor`).** A WYSIWYG editor for `.ui.ron`: hierarchy, layout properties, theme
   classes, bindings and commands, a preview laid out by the runtime's own layout at several resolutions and safe
   areas, and a localization preview (locale keys, pseudo-localized text). It reads and writes the same frozen
   `.ui.ron` a game loads.

The pattern is the presentation-model / model-view-viewmodel split described publicly by M. Fowler ("Presentation
Model", 2004) and J. Gossman ("Introduction to Model/View/ViewModel pattern for building WPF apps", 2005). No other
engine's design or code is used (`docs/01-cleanroom.md`).

## 2. Runtime API (`regia_ui`)

### 2.1 View models

A view model is any `Resource + Reflect + Default` struct registered with

```rust
app.register_view_model::<HudVm>();       // trait ViewModelAppExt on App
```

which registers the type for reflection, inserts the default resource when absent and records the model in the
`ViewModels` resource (name = short type path, field list with kinds `number | integer | bool | text | other`).
A binding path is `"<Model>.<field>[.<field>…]"`, resolved through reflection exactly like the Tier 1 `Bind`
(`specs/18` § 5): any reflected resource works; registering is what makes a model visible to the designer and to
agents, and what makes two-way writes allowed (§ 2.3).

### 2.2 Bindings

`Bind` (existing component) gains two fields: `mode: BindMode` and `invert: bool`.

| `BindMode` | Behavior |
|---|---|
| `OneWay` (default) | Model → widget when the formatted value changes (the Tier 1 behavior). |
| `TwoWay` | As `OneWay`, plus a widget edit writes the field back (§ 2.3). |
| `OneTime` | Pushed once, when the source first resolves; later changes are ignored. |

`BindTarget` gains `Value` (`Slider.value`), `Input` (`TextInput` text), `Selected` (`Dropdown.selected`,
`TabBar.active`), `Visible` (`NodeStyle` display `None` when false) and `Enabled` (`Disabled` marker when false).
`invert` negates a boolean before `Checked` / `Visible` / `Enabled` (and before a two-way write of one).

A node can carry several bindings: the new component `ViewBindings(Vec<Bind>)`. The legacy single `Bind` component
keeps working unchanged. `update_bindings` (PostUpdate, `UiLayout` set, already chained before layout) refreshes
both.

### 2.3 Two-way writes

`collect_two_way_edits` (Update, after `ui_widgets`) reads this frame's `UiEvent`s: `ValueChanged(Bool | Number |
Index)` and `Submitted(Text)` on a node with a `TwoWay` binding whose target matches the value
(`Checked`←Bool, `Value`←Number, `Selected`←Index, `Input`←Text); `apply_two_way_edits`, chained after it, writes
them. The value is converted to the field's reflected
type (`f32`, `f64`, `i8..i64`, `u8..u64`, `usize`, `bool`, `String`; integers round to nearest and must fit) and
applied through reflection. Only fields of a **registered** view model are written; a two-way binding to another
resource is reported (`binding_not_view_model`) and left one way. The binding's `last` is set to the written value
so the next `update_bindings` does not echo it back. If the game writes the same field later in the same frame, the
game's value wins (it runs after the write-back and is pushed in PostUpdate).

### 2.4 Commands

`UiCommandName(String)` component; `emit_ui_commands` (Update, after `ui_widgets`) turns `UiEvent::Click` and
`UiEvent::Submitted` on such a node into a `UiCommand { name, node, value }` message (`value` is the submitted text
or `UiValue::None`). Games read `MessageReader<UiCommand>`; a command name is free text (`"inventory.equip"`).

### 2.5 Binding status

`ViewModels::status()` lists every bound node (entity, node name, path, target, mode, last value, last error) for
the agent surface. Errors are kept per binding (`binding_*` codes, § 4) instead of logged per frame.

### 2.6 Converters (ADR-2489)

`Bind.converter: Option<BindConverter>`, in `.ui.ron` the bind field `converter`. A converter is data from a fixed
set, with no scripting; it runs on the field's text before `format` and `invert`. Text it cannot read passes through,
and non-finite or reversed bounds never panic.

| `.ui.ron` | Model → widget | Widget → model (two way) |
|---|---|---|
| `number(precision: 2, group: ",")` | `1234.5` → `1,234.50`; `group` is optional; precision at most 64; an integer keeps every digit | the value as it is |
| `clamp(min: 0, max: 100)` | limited to the range; reversed bounds swap; a non-finite bound passes the value | limited to the range |
| `map_range(from: (0, 100), to: (0, 1), clamp: true)` | linear map; `clamp` is optional; a zero-width input range gives the output start | the inverse map; a zero-width output range has no way back |
| `bool_to_visibility` | `false`, `None`, empty text, zero and NaN → `false`; anything else → `true`, for `visible`, `enabled`, `checked` | none |
| `enum_text(table: [("Idle", "Waiting")], default: "?")` | the entry for the whole value, then for its variant name (`Moving { speed: 1.0 }` → `Moving`); then `default`; then the value | none |
| `loc_key(prefix: "state-")` | `prefix` + variant name as a Fluent message id. On a `text` target the span holds the key (`TextContent::Loc`), so it re-resolves on a language switch and a missing key shows `⟦key⟧` | none |

A two-way edit through a converter with no way back writes nothing and reports `binding_converter_one_way`.

### 2.7 Collection item templates (ADR-2489)

A node with `repeat: (source: "InventoryVm.items", key: "id")` is a *repeat host*. Its `children` are not spawned;
they are the *template*, spawned once per element of the list at `source` as children of the host. `source` is
`"Model.field[.field…]"` of a reflected resource (a `List` or `Array` through reflection: `Vec`, `VecDeque`, arrays), or
`".field"` relative to the item of the enclosing host (nested repeats). `key` is a path inside an item (`"id"`, `"."`
for the item itself). Without a key items are keyed by position.

- **Item bindings.** A binding source starting with a dot reads the item: `".name"`, `"."` for the item itself
  (`BindSource::Item`). Modes, targets, formats, inversion and converters work as for any binding. Other bindings in
  a template stay ordinary resource bindings. An item binding on a node no host generated reports
  `binding_item_outside_repeat` and pushes nothing.
- **Keyed diff.** `plan_keyed_diff(old, new)` maps each new position to the old instance with the same key: a kept key
  keeps its entities and every piece of widget state on them, a gone key despawns, a new key spawns, and a changed order
  only sorts the host's `Children` (other children a widget added stay first). Repeated keys become unique by
  occurrence (`a`, `a#2`) and report `repeat_duplicate_key`. A host keeps at most `MAX_REPEAT_ITEMS` (65,536) instances
  and reports `repeat_capped` past that.
- **Refresh.** A pass runs at the top of `update_bindings`, not as its own system: the schedule does not change, and a
  game with no repeat node has no `RepeatHosts` resource and pays one resource lookup. A host passes only when the root
  resource's change tick moved since its last pass: it reads the keys, applies the diff, then resolves every item
  binding and writes only what differs from the binding's `last`. A pass over a changed model writes one row when one
  item changed.
- **Commands and two-way writes.** A generated node with item bindings or a command carries `RepeatBound { host, key }`.
  `UiCommand.item` holds the key, so the game knows which row was clicked. A two-way edit writes the item the row
  belongs to now, by the row's current index (`"Vm.items.3.count"`).
- **Errors** (`repeat_*`, kept on the host, reported by `ui.collection_state`, not logged per frame):
  `repeat_unresolved`, `repeat_not_a_list`, `repeat_outside_template`, `repeat_bad_key` (spawns nothing),
  `repeat_duplicate_key`, `repeat_capped`. A host in error keeps its instances.
- `spawn_ui` ignores a `repeat` on the root, like the root's other fields. A `repeat` on a `list` widget is
  virtualized (§ 2.8).

### 2.8 Virtualized repeats (ADR-2715)

A repeat host that is a `list` widget (`widget: list(count, row_height)`) materializes only the items in the list's
visible range plus its overscan (2 rows each side by default), not the whole collection.

- **Rows.** Each materialized item is a *row*: an absolutely placed node `row_height` tall and as wide as the list
  (classes `list-item`, `ListRow { list, index }`, interactable and focusable), holding one copy of the template. The
  list's geometry is the list widget's own (uniform rows, `ListHeights`, `ListTiles`). The host carries
  `RepeatRows`, so the list widget keeps its scroll extent (it sets the list's `ItemCount` to the collection length),
  scroll-to, keyboard cursor, focus and selection (`Selected` on the rows), but spawns no row pool of its own.
- **Recycling.** A pass computes the visible range. A key already in the range keeps its row. A row whose item left
  the range goes to an item that entered it: its `RepeatBound` keys change, nested repeats inside it re-read their
  collection, one-time bindings push again, and the item bindings write only what differs from the old item. Only a
  range longer than the rows on hand (spares included) spawns. A shorter range (the ends of the list have no overscan
  past the edge) hides the surplus rows as spares, up to 2 x overscan + 2, and despawns the rest. Scrolling, and removing or inserting an
  item inside the view, spawn nothing. Row state the model does not hold (a toggle with no binding) follows the row,
  not the item: bind it.
- **Anchor.** After a pass the host records the first visible item's key and the offset past its top. When the
  collection changes, the scroll offset moves so that item keeps its place on screen (inserting above the view moves
  the offset down by the inserted rows). If the anchor item is gone, the next surviving item after it in the old order
  takes its place; failing that the offset stays. The offset is clamped to the new scroll range.
- **Refresh.** A virtualized host passes when the model's tick moved or the view moved (scroll offset, the list's rect
  or scale, row height, overscan, `ListHeights`). A scroll-only pass reuses the keys read on the last model change.
- `RepeatHost` reports `is_virtual`, `item_count` (whole collection), `instance_count` (rows), `first_index`,
  `anchor_key`, `spare_rows`, `spawned_total` and `recycled_total`; `roots(index)` takes a collection index.
- Two-way writes and commands work as in § 2.7: the row's key names the item, its index is the collection index.
- A template root with a fixed height other than the row height overlaps or leaves gaps (lint `repeat_on_list`).

## 3. Format: `.ui.ron` v3

The frozen format (`docs/formats/ui-ron.md`, `specs/12` § 4a) goes from version 2 to **3**:

- node field `bindings: [Bind]` — the view-model bindings (same shape as `bind`);
- node field `command: "<name>"` — the command sent on click / submit;
- `bind` / `bindings` entries gain `mode: one_way | two_way | one_time` (default `one_way`) and `invert: bool`
  (default `false`);
- `target` gains `value`, `input`, `selected`, `visible`, `enabled`.

Example:

```ron
( version: 3, root: ( children: [
    ( name: "hp", widget: progress(1.0), bindings: [ (source: "HudVm.health", target: progress) ] ),
    ( name: "vol", widget: slider(0.5, 0.0, 1.0, 0.05),
      bindings: [ (source: "SettingsVm.volume", target: value, mode: two_way) ] ),
    ( name: "ok", widget: button, text: (spans: ["OK"]), command: "settings.apply" ),
] ) )
```

**v6 (ADR-2489)** adds the node field `repeat: (source, key?)`, the bind field `converter` (§ 2.6) and item sources
(`".field"`). Each is optional and no older file means anything by them (a dotted source never resolved), so the v5 → v6
step only rewrites the root `version` number; `regia upgrade-project --write` and `regia_ui::upgrade_ui_ron` apply it.

**Migration note.** A v2 file means the same at v3 (every new field is optional, every new variant is new), so
the built-in v2 → v3 step only rewrites the root `version` number, as v1 → v2 does (ADR-1381); v1 files take both
steps. `regia upgrade-project --write` and `regia_ui::upgrade_ui_ron` apply it. The schema fingerprint changes; the
lead regenerates `docs/formats/format-hashes.ron` with `cargo xtask check-formats --update`.

**Writing.** `regia_ui::write_ui_ron(&UiFile) -> Result<String, UiError>` is the canonical writer (new): pretty
RON, implicit `Some`, and only non-default fields (default style fields, empty lists, `false` flags and `None`
options are omitted), so a designer save of an untouched hand-written file stays short. It round-trips:
`parse_ui_ron(write_ui_ron(f)) == f` for every `f` the parser accepts. It knows nothing of comments; the designer's
save keeps them (§ 4.5).

## 4. Editor: the UI designer (`regia_editor`)

### 4.1 Model (`ui_designer.rs`, headless, tested without egui)

`UiDesigner` resource: at most one open document (`UiDesign`): source path, the `UiFile`, revision, dirty flag,
selection, undo/redo stacks (snapshots of the `UiFile`, 200 deep), preview settings, sample values.

- **Node paths.** `"/"` is the root; `"/0/2"` is child 2 of child 0. A node can also be named `"#name"` (first
  match in depth-first order). Paths in replies are always the index form.
- **Edits** (each one undo step; all validate first and change nothing on error): `add` (a new node from a preset:
  `panel`, `text`, `button`, `toggle`, `slider`, `progress`, `text_input`, `dropdown`, `image`, `list`, `scroll`,
  `lobby_list`, `scoreboard`, `radar`, `minimap`, `marker_layer` (ADR-2958; the preview fills these with invented rows);
  or a full node as JSON) at a parent and index; `remove`; `move` (new parent and index; a node cannot move into its
  own subtree); `duplicate`; `set` one field by key path (`name`, `classes`, `style.size`, `text`, `widget`,
  `bindings`, `command`, …) to a JSON value, decoded with the format's own types (so anything the designer
  accepts, the loader accepts); `undo`, `redo`.
  A `set` of `text` to a plain string replaces the spans only; alignment and style stay. `apply_merged` lets the
  consecutive `set`s of one field share an undo step until `end_merge` (the panel: one drag on a number is one step).
  `drop_move(node, onto, zone)` turns a drop on a row (`DropZone::Before`, `Into`, `After`: the upper quarter, the
  middle, the lower quarter of the row) into the `move`, or nothing when the drop changes nothing.
- **Lint** (`lint()`): duplicate node names; a binding with an empty or dotless source; a binding target that does
  not fit the node (e.g. `value` without a slider); `two_way` on a non-editable target (`progress`, `opacity`,
  `text`, `visible`, `enabled`); a `command` on a node that is not interactable; with a view-model schema loaded
  (§ 4.3): unknown model, unknown field. Each finding: code, severity, node path, message.
- **Save** writes through a temp file and a rename; the result is parsed back before the rename, so a file the loader
  would refuse is never written. The text comes from § 4.5.
- `repeat_op` and `converter_op` build the `set` of a node's `repeat` and of one binding's `converter`; `drop_add` turns
  a palette drop into an `add` with the same zones as `drop_move`.

### 4.2 Preview

`preview(settings) -> PreviewFrame`: spawns the document into a private `World` with the runtime layout
(`run_ui_layout`), at the chosen size and safe area, and returns one rect per authored node (path, name, rect, clip,
text after localization, visible). Nodes are matched by a private name on a scratch copy, so the children widgets
add at run time (list rows, scrollbars) never shift the mapping. Settings: size presets `1920x1080`, `1280x720`, `1280x800` (handheld), `2400x1080` with a notch
safe area, `1080x2400` portrait, or custom; safe-area insets; locale (keys shown as `⟦key⟧` when no table is
loaded); pseudo-localization on/off (`regia_ui::loc_pseudo`); sample values per binding path, pushed into the bound
targets so text, bars and toggles show realistic content. The egui panel paints the frame (fills, borders, text,
selection outline) and picks the topmost node under a click. Layout is the runtime's, so the preview and the game
agree on every rect; the schematic pixels (no theme shading, no GPU) are enough to judge structure,
spacing, safe areas and text fit.

**Themed preview (ADR-2715).** With *Themed* on (panel) or through `editor.ui_designer_render`, the same scratch
document (samples applied, repeats expanded) is drawn by the runtime UI renderer into an offscreen texture with the
project's theme: the editor's `UiThemes` and `ActiveTheme`, then every `<project>/assets/ui/themes/*.theme.ron`
(`theme_agent::load_theme_file`; a theme whose parent loads later is retried once), optionally another `theme`. The
draw goes through the render-out API (`begin_render_out`, `SharedUiRenderer`, `OffscreenTarget`), not through
renderer internals, and never touches the live editor UI. The device is the editor's `RenderDevice`, else a headless
one made once (lavapipe works). The panel uploads the read-back pixels as an egui texture, redrawn only when the
document or the settings change; clicks still pick through the laid-out rects, which are the same layout.

A repeat on a `list` widget (§ 2.8) places its sample rows as the list places them (absolute, `index * row_height`, a
row tall unless the template root has a fixed height) and marks the rows past what the list materializes at the top
of its range as not visible. `PreviewFrame.lists` reports per such list its path, sample item count, row height,
materialized rows and first visible key (its key sample `"Vm.items[0].id"` or `"Vm.items[].id"`, else `"0"`).

A repeat host shows `repeat_count` sample items (default 3, at most 256; laid out nodes are capped at 8,192 and the frame
says `truncated`): the template is copied into the scratch document per item. Each copy reports its template's path
and an `item` index, so a click on any row selects the template node. Samples use source paths with brackets:
`"Vm.items[].name"` for every item, `"Vm.items[1].name"` for one, `"Vm.items[]#count"` for that host's item count,
`"Vm.sections[].items[].name"` when nested. Converters run on samples through the runtime's own `display_value`.

### 4.3 View-model schema

The designer reads the `ViewModels` registry of the editor world (filled when the game's dylib registers its
models). Without one, lint skips the model checks and the binding picker takes free text.

### 4.4 Panel

Tab `Panel::UiDesigner` ("UI Designer"):

- **Left, the palette and hierarchy**: a chip per preset, dragged onto a hierarchy row or a preview node (§ 4.1
  `drop_add`; a marker shows where it lands); select; add, remove, duplicate; up and down; drag a row to reorder (§ 4.1
  `drop_move`). A repeat host's row shows its source.
- **Center, the preview** with the size, safe-area, locale, pseudo and sample-item-count toolbar.
- **Right, the inspector** for the selected node: name, classes, text and localization key (one plain span; other
  text through the JSON row), command, the widget's typed fields (§ 4.9; the widget as JSON under *Advanced*), an
  image's tint and keep-aspect; typed layout rows (display, position, direction, justify,
  align items and self, size, min and max size, gap, margin, padding, inset, grow, shrink, opacity, wrap, clip,
  interactable, disabled, background color); repeat (source with a picker of the models' list fields, key); bindings
  (source with a model and field picker when `ViewModels` has models, item fields in a repeat template, target, mode,
  invert, format, the converter picker with typed settings, and a sample value that only feeds the preview); a free key +
  JSON row for every other field; the lint list.

Ctrl+Z undoes, Ctrl+Shift+Z and Ctrl+Y redo while the pointer is over the panel and no text field has focus;
elsewhere the keys stay the shell's scene undo. A failed edit stays on screen until dismissed. With several nodes
selected (§ 4.8) the inspector shows their shared layout rows (a change goes to all of them; mixed fields are listed),
align, distribute, group, duplicate and delete.

### 4.5 Saves keep the source text (ADR-2489)

`regia_ui::ron_merge::merge_ui_ron(source, old, new)` is a three-way text merge: `source` is the file as opened
(renumbered to the current version), `old` its tree, `new` the document. The canonical texts of `old` and `new` are
compared value by value; what is equal is copied from `source` byte for byte, what differs is rewritten in place.

- A span parser reads RON's shape: values with their byte ranges, and for each struct field and list element its
  *chunk*, which also holds the comment lines above it, the comma and a trailing comment on its line.
- Struct fields keep the source's order. A changed value merges inside it when both sides are structs or lists, or is
  replaced. A new field goes after the source field closest before it in canonical order, on its own line in a
  multi-line struct. A field reset to its default goes, with its comments.
- List elements align by exact text (longest common subsequence), then exact text elsewhere (a move, which carries its
  comments), then the same `name` (a move with an edit), then position between matched neighbors (an edit in place).
- New values are written on one line up to 100 bytes, else as the canonical writer laid them out, at the field's indent.
- The result is parsed and compared with `new`. If it differs, or the source is not readable, the merge is an error:
  the designer writes the canonical form, `last_save_canonical` says so, `comments_lost` says what it cost and the panel
  shows it. A save with no source (a new document), or with `canonical: true`, writes the canonical form.

Acceptance: open, edit one property, save, and every other line is byte-identical; saved again, undone, moved or with
fields added, the comments and the key order stay. The merge is pure text over the format's own writer output; it does
not know UI nodes, so a new format field needs no merge code.

### 4.6 Pictures by asset path or icon (ADR-3148)

`.ui.ron` v9 lets an `image` name its picture by manifest path (`asset: "assets/ui/panel.png"`) or by icon-atlas name
(`icon: "gear"`), with optional `nine_slice: (l, t, r, b)` insets, and a group `mask` by `icon` as well; the raw
renderer `id` stays for code that builds UI at run time. A designer-made document needs no game code to show a picture.

- **Run time** (`regia_ui::ui_image_assets`): an asset path resolves through the asset server's manifest and is read and
  decoded on the async task pool; the frame never waits. The node draws a faint placeholder until the load lands, then
  the picture (a cooked RGBA8 image as is, a block-compressed texture decoded on the CPU). The node holds a
  `UiImageHandle` (masks a `UiMaskHandle`) with the asset, status and error code. When the asset server reloads a held
  asset the file is read again and the new pixels replace the old under the same image id. A missing asset, one of
  another kind, a decode failure or an unknown icon logs one warning and draws the fallback (a magenta and black
  checker); a mask that cannot resolve leaves the node unmasked. Codes: `ui_image_not_found`, `ui_image_wrong_kind`,
  `ui_image_load_failed`, `ui_image_icon_missing`, `ui_image_no_asset_server`.
- **Cook and validation**: the cook's closure follows the typed `image` and `mask` fields of every top-level
  `assets/ui/*.ui.ron` (the runtime loads those at start), so a pack holds the textures a layout names even when nothing
  else references them. `regia assets validate` adds each layout to the asset set with the paths it names; the
  validator `ui_image_missing` fails a layout naming a path no cooked asset matches (and `ui_layout_invalid` for a layout
  that does not parse), and the pictures count as reachable for the orphan check.
- **Designer**: the inspector's Picture rows offer the project's source pictures under `assets/` and the icon atlas's
  names, the name as text, nine-slice insets and the mask fit; each change is one `image` or `mask` edit. Both previews
  show the picture: the schematic preview draws the source file over the node's rect, the themed preview registers the
  decoded source file with its scratch renderer (a path with no readable file draws the fallback, an icon the
  placeholder, since the designer has no icon pages). The document itself is never rewritten for a preview.

### 4.7 Direct manipulation in the preview (ADR-9730)

`ui_designer_canvas::plan_transform(root, frame, request)` turns a wanted rect (`[x, y, w, h]` in the preview frame's
px, `move` or `resize`) into one edit. The edit is computed over a laid-out frame of the same document:

- An **absolutely placed** node writes `style.inset` (the sides that are set move with their edges; with none set,
  `left` and `top` take the node's offset in the parent's padding box) and `style.size`. A node stretched by two set
  insets and an `Auto` size keeps its size.
- A node **in the flow** writes `style.size`, and `style.margin` for a move or a moved left or top edge. A move whose
  centre lands in a flow sibling's slot on the parent's main axis reorders it through `drop_move` instead (upper half:
  before, lower half: after).
- Lengths keep their kind: points move by the frame delta over `frame.scale`, a percentage by its share of the parent;
  values round to 0.01 UI px. The edit is one `set` of `style`, so a drag merged by `apply_merged` is one undo step.
- **Snapping** (editor state, never saved): the parent's edges and centre, the siblings' edges and centres, and a
  grid (spacing in UI px from the parent's corner). A move snaps the rect's left, centre or right (top, middle,
  bottom); a resize snaps the edges that moved. The nearest candidate within the threshold wins per axis, and the
  snaps that applied come back as guide lines. Shift keeps the aspect ratio; Alt turns snapping off for the drag.

The panel draws eight handles on a lone selected node. Dragging the node moves it, and dragging a handle resizes it,
live and merged into one undo step; a reorder applies on release. **Group drag** (ADR-9980): dragging a node that is
one of several selected nodes moves every top-level selected node (a selected descendant rides along with its
ancestor) by the same delta. `plan_group_move(root, frame, nodes, delta, snap)` snaps once, on the group's bounding
rect, against the shared parent's edges, its unselected children and the grid (against the frame alone when the nodes
have different parents), then moves each node as a transform without snapping or reordering (a flow node writes its
margin). The whole drag is one undo step (`apply_batch_merged`), and the selection is kept. While dragging, the panel shows the guides and the
offset and size in UI px. `align_ops` lines several nodes up by their laid-out rects: left, center, right, top,
middle and bottom to the selection's bounds; distribute keeps the outermost two and spaces the others evenly. Each
node moves as a transform without snapping or reordering.

The **canvas view** is panel state: the wheel zooms about the pointer (5 % to 1,600 %), a middle drag pans, *Fit*
fits the frame and *100 %* shows one frame pixel per screen point. Rulers along the top and left show frame px
(1-2-5 steps at least 48 points apart). The phone presets get a device body with a camera cutout in the safe-area
inset, and the handheld preset a wide bezel.

### 4.8 Several nodes and the clipboard (ADR-9730)

Ctrl+click (in the hierarchy or the preview) adds a node to the selection or takes it out; Shift+click in the
hierarchy selects the range in document order. `UiDesign::selected_nodes()` lists them, the primary first.
`apply_batch(ops)` applies several ops as one undo step (all or nothing); `set_many`, `remove_many`, `duplicate_many`,
`group` (siblings only, wrapped in a new panel at the first one's place, order kept) and `align` are built on it.

Copy (Ctrl+C), cut (Ctrl+X) and paste (Ctrl+V) work on the top-level selected nodes. The clipboard text is the format's
node JSON, a JSON array (the shape `add {node}` takes), held in `UiDesigner.clipboard` across documents and put on
the system clipboard. A paste lands after the primary selection. A pasted name the target document already has gets
`_2`, `_3`, …; bindings and every other field are kept. Ctrl+D duplicates and Delete removes the selection. Each is
one undo step.

Acceptance: an `opacity` set on three selected nodes is one undo step; a subtree copied in one document and pasted
into a second one saves, and the saved file loads with the runtime loader (`parse_ui_ron`) and lays out.

### 4.9 Typed widget fields (ADR-9730)

`ui_designer_widget` lists each widget kind's fields with their type. Fields are read from and written into the
widget's own JSON and decoded by the format's serde types, so a typed edit and the same edit typed as JSON save the
same bytes (tested for every field of every kind). `set` takes `widget.<field>` for a field of the node's current
widget; other `widget.*` keys stay plain JSON paths. The rows follow the format: `toggle` checked; `slider` value, min,
max, step; `progress` value; `text_input` placeholder, multiline, and since v11 (ADR-9980) max_chars (0 = no limit),
secret (one character or none), read_only and filter (`any`, `integer`, `numeric`; a combo), which read as their defaults
while the file has no options element and drop it again when every option is back at its default; `dropdown` options, selected; `tab_bar` tabs, active;
`modal` open, close_on_cancel; `tooltip` text, delay; `scroll` inertia, scrollbar; `list` items, row_height; `stick`
action, radius; `v_button` action; the menus' entries (JSON); `lobby_list` its seven words; `scoreboard` title,
columns, sort_column, descending, group_by_team, hold_action, from_rules; `radar` / minimap range, world_rect, square,
rotate, map_image, map_rect, layer_mask, max_blips, show_north; `marker_layer` layer_mask, edge_margin, max_markers;
since the v11 fold (ADR-7827) `split` axis (a combo), ratio, min_ratio, max_ratio; `foldable` title, open, group (empty
clears); `tabs` tabs, active. A struct widget's field reads as its default while the file leaves it out. An image has typed rows for its tint, *keep aspect* and, since v11, its fit (*keep aspect* leaves `fit` unwritten,
or `stretch`, `contain`, `cover`; `set {key: "image.fit"}` through the agent). Not in the format, so not a row: a list's
selection (runtime state).

### 4.10 Containers, markup and focus neighbors (ADR-7827)

Presets `split` (two empty panes), `foldable` (open, one text child) and `tabs` (two tabs, one text page each). The
preview spawns them through the runtime, so it shows only the active tab page and the bodies of open foldables. The
inspector's **Markup** box sets `text.markup` and shows the parsed text under it. The **Navigation** section names the
node's `nav` neighbors: a name row per direction (typed, Enter applies, empty clears) with a **Pick** toggle that makes
the next preview click name that neighbor (the clicked node must have a name; the selection does not change). The
canvas toolbar's **Show navigation** (view state) draws an arrow from each node to each declared neighbor. Lint:
`split_panes`, `split_ratio`, `tabs_pages`, `tabs_active`, `nav_unknown_target`, `nav_target_not_focusable`,
`nav_not_focusable`, `nav_self` (book/agents/ui-designer.md).

## 5. Agent-protocol surface

Editor methods (`book/agents/ui-designer.md`), all through `editor.*`:

| Method | Permission | Does |
|---|---|---|
| `editor.ui_designer_open {path}` / `{new: true}` | assets | Open a `.ui.ron` (migrated in memory) or start an empty document |
| `editor.ui_designer_state {node?}` | read | Document summary, the tree (paths, names, widget kinds, binding counts), the selected node in full JSON |
| `editor.ui_designer_edit {op, …}` | edit_scene | `add`, `remove`, `move`, `duplicate`, `set`, `repeat`, `converter`, `bind_source` (§ 12), `image`, `mask`, `select`, `select_many`, `transform`, `align`, `group`, `copy`, `cut`, `paste`, `undo`, `redo` (§§ 4.7-4.9; `set`, `remove`, `duplicate` take `nodes`; `transform {nodes?, delta}` moves a group, ADR-9980; `rect` with `delta`, or `nodes` with `node`, is refused as invalid params, and `parent: null` on `add`/`paste` is the default parent, ADR-7479) |
| `editor.ui_designer_images` | read | The document's picture references with `found`, the asset paths it names, how many are missing, the picker lists (§ 4.6) |
| `editor.ui_designer_lint` | read | Findings |
| `editor.ui_designer_preview {size?, safe_area?, locale?, pseudo?, samples?, repeat_count?}` | read | The laid-out frame (rects per node) and `lists` (virtualized list state) |
| `editor.ui_designer_render {size?, safe_area?, locale?, pseudo?, samples?, repeat_count?, theme?}` | read | The themed preview as a PNG (`width`, `height`, `theme`, `drawn_pixels`, `png_base64`) |
| `editor.ui_designer_save {path?, canonical?}` | assets | Write the document (§ 4.5) |

Runtime: `ui.collection_state {keys?}` reports per repeat node its source, key path, instance count, what its last pass
did (`spawned`, `despawned`, `moved`, `touched`, `recycled`; `passes` counts the frames the model or, for a
virtualized repeat, the view changed) and its error code. A virtualized repeat adds `virtual: true`, `items`,
`materialized`, `spare_rows`, `first_index`, `first_visible_key`, `spawned_total` and `recycled_total` (a plain repeat
says `virtual: false`).
`ui.image_assets {status?}` lists the nodes whose image or mask names an asset or an icon, with status and error code
(§ 4.6); a `status` other than `loading`, `ready` or `failed` is refused with `ui_image_status_unknown`.
`ui.view_models {}` lists every registered view model with its fields, kinds and current values, and the binding
status of § 2.5 (the line is ADR-5814's; the method already existed).

Error codes (`data.code`): `ui_designer_no_document`, `ui_designer_bad_node`, `ui_designer_bad_op`,
`ui_designer_bad_value`, `ui_designer_bad_move`, `ui_designer_nothing_to_undo`, `ui_designer_nothing_to_redo`,
`ui_designer_parse`, `ui_designer_io`, `ui_designer_bad_size`; `editor.ui_designer_render` also returns
`ui_designer_unknown_theme`, `ui_designer_no_gpu` and `ui_designer_render_failed`.

Lint codes: `duplicate_name`, `binding_empty_source`, `binding_target_mismatch`, `binding_two_way_read_only`,
`command_not_interactable`, `root_field_ignored`, `binding_unknown_model`, `binding_unknown_field`,
`binding_item_outside_repeat`, `repeat_bad_source`, `repeat_outside_template`, `repeat_not_a_list`,
`repeat_unknown_field`, `repeat_empty_template`, `repeat_no_key`, `repeat_on_list`, `converter_bad_settings`,
`converter_target_mismatch`, `converter_one_way`, `binding_bad_animation`, `binding_source_ignored`,
`repeat_template_height` (§ 12), `split_panes`, `split_ratio`, `tabs_pages`, `tabs_active`,
`nav_unknown_target`, `nav_target_not_focusable`, `nav_not_focusable`, `nav_self` (ADR-7827).

Runtime binding status codes (§ 2.5): `binding_unresolved`, `binding_not_view_model`, `binding_type_mismatch`,
`binding_item_outside_repeat`, `binding_converter_one_way`, `binding_theme_token` (§ 12). Runtime repeat codes: § 2.7.

## 6. Determinism

Bindings and commands run in fixed schedule positions and read only this frame's messages, so a replayed input
stream produces the same writes in the same order. The repeat pass adds no system and no fixed-step work; hosts refresh
in spawn order, instances in collection order, and the diff is a pure function of the two key lists. The designer's output is a pure function of the document
(`write_ui_ron` sorts nothing and adds nothing; field order is the struct order). The preview is a pure function of
document and settings (layout is deterministic for equal inputs).

## 7. Tests and acceptance

- `.ui.ron` v3: v2 and v1 fixtures upgrade by renumbering only and parse to the same tree; the v3 fixture loads; a
  file newer than the loader's `UI_RON_VERSION` is refused (this read "a v4 file" when v3 was current; v4 is now the
  text-effects version, ADR-2045, and the format is at v9, ADR-3148; ADR-5814); `write_ui_ron` round-trips every
  fixture and the docs example.
- Bindings: one-way, one-time, invert, visible/enabled targets; two-way slider/toggle/dropdown/text-input writes the
  view model through real `UiEvent`s; a two-way bind to an unregistered resource is not written and reports
  `binding_not_view_model`; integer conversion out of range reports `binding_type_mismatch`; commands from click and
  submit.
- Designer model: every edit and its undo restores the exact document; invalid edits change nothing; move into own
  subtree refused; lint codes; save + reload equal; preview rects match the runtime layout for the same file and
  size and move with the safe area.
- Agent: each method's happy path and its error codes, in the editor's agent tests.
- Collections (`regia_ui/tests/repeat.rs`): 1,000 items make 1,000 rows; changing one item touches one row (the other
  rows' `Text` change ticks do not move); removing a middle item keeps the other rows' entities, widget state and
  markers; reorders move rows without rebuilding them; inserts land in order; a new key is a new row; position and value
  keys; duplicate keys; arrays and multi-node templates; nested repeats; each error code; two-way writes reach the
  item after it moved; commands name the item; converters through real bindings and the Fluent path.
- Virtualized repeats (`regia_ui/tests/repeat_list.rs`): a 10,000-item collection keeps at most visible + 2 x overscan
  rows (counted over the world) and a long scroll spawns nothing; inserting above the view keeps the anchored row at the
  same screen position; removing the anchor hands it to the next item in place; removing a visible item recycles one
  row and spawns none; bindings in rows follow edits and recycling (one-time included); the list's selection marks the
  rows; `ui.collection_state` reports the window.
- Themed preview (`ui_designer_gpu.rs`, `ui_designer_agent.rs`): on lavapipe the texture has the requested size, its
  pixel count matches, it draws over the backdrop (numeric readback, no golden), an empty document draws nothing; an
  unknown theme is an error; the agent method returns a PNG.
- Merge (`regia_ui::ron_merge` tests, `ui_designer.rs`): the comment round trip; adds, removes, moves and resets
  keep other lines byte-identical; broken text is an error; the fuzz target `ui-repeat`.
- Panel, driven headless with egui input events: typing into a binding row edits the document; a sample changes the
  preview and not the document; undo and redo shortcuts follow the pointer and Ctrl+Shift+Z redoes; dragging a row
  moves the node in one undo step; errors stay on screen; layout row values round-trip through the format.

## 8. Platforms

The runtime half is plain ECS + reflection: Linux, Windows and Android aarch64 alike (no platform code). The
designer is editor-only (desktop: Linux, Windows; macOS testing-only). `regia_editor` stays out of every runtime
crate's dependency tree.

## 9. Performance budget

`update_bindings` is O(bound nodes) per frame and costs no binding clones when nothing changed: a read phase borrows
every binding, and only the changes reach the write phase. The source's resource is found with a hash lookup on the
short type name (`registrations_named`); only a short name shared by several types falls back to scanning the
registry. A HUD with 200 bindings must stay under 0.1 ms on the desktop reference (`tracing` span `ui_bindings`;
`cargo bench -p regia_ui --bench bindings_200`).

A repeat host costs one resource tick check per frame while its model is unchanged. A pass over a changed model costs
one key read per item and one read per item binding; only changed values reach the write phase. Numbers on the reference
are recorded in ADR-2489 (`cargo bench -p regia_ui --bench repeat_1000`: 1,000 rows, two item bindings each).
A virtualized repeat (§ 2.8) costs one view check per frame while nothing moves. A scroll pass costs one read per item
binding of the rows in range, and nothing per item out of range. A model change still reads every item's key (the
anchor and the positions need the whole order), so it is O(items) with one key string per item.
`collect_two_way_edits` and `emit_ui_commands` cost nothing without events. The designer preview re-lays out only
when the document or the settings change.

## 10. Out of scope (later)

| Item | Status |
|---|---|
| Bindings into theme tokens; animation bindings | built (ADR-7724 fold, ADR-7751): bind fields `theme` (`BindSource::Theme`, follows a theme switch, runtime code `binding_theme_token`) and `animation` (`BindSource::Animation`, `(from, to, duration, delay?, ease?, repeat?)` on the game clock), § 12 |
| Variable row heights measured from the template (a virtualized repeat uses the list's `row_height` or `ListHeights`) | built (ADR-7724 fold, ADR-7751) as a fixed row height taken from the template: `repeat` field `row_height: template` uses the first template node's fixed height; per-item measured heights stay out of scope (`ListHeights` covers them), § 12 |
| Image `fit: tile` | built (ADR-7724 fold, ADR-7751): `ImageFit::Tile` repeats the picture at its native size, clipped to the node, § 12 |

## 11. As planned (ADR-7752, UI parity)

Planned for v0.6.0; nothing here is built at the time of writing. Each item names the w12 job that builds it. Every item
ships with an agent method (a name marked "planned" is set finally by the building job), a `book/agents` page and a
`book/COVERAGE.md` row, is reachable from the keyboard, scales with `appearance.ui_scale` and takes its colors from
theme tokens. The runtime parts of this list are in spec 18 § 10.

- **Anchor preset grid** (3241). A grid of anchor presets in the node's Layout section, with a keep-position or a snap
  behavior when a preset is applied. `editor.ui_designer_edit` `set anchor`, `editor.ui_designer_anchor_preset`
  (planned).
- **Typed rows** for aspect ratio and text effects; margin and padding bands drawn on the canvas (3241). Same edit
  method.
- **Scale in the preview** (3241). The preview uses the project's scale rule, shown as "Scale 1.50x", or a manual scale;
  Project Settings edits the scale rule and the DPI curve. The scale is the open document's view state
  (`PreviewSettings`), never in the file. `editor.ui_designer_preview` takes `scale`.
- **Document containers and text** (3242). One document version step with a migrator (a `.ui.ron` row in the v0.6.0
  format-bump plan) adds split, foldable and tab containers, markup text (the runtime's existing markup parser applied
  to documents), navigation neighbors and a keyboard-focus-only style state. The designer palette lists the new
  containers and a Navigation section edits the neighbors. `editor.ui_designer_edit`.
- **Layout variants** (3243). A variant is chosen by orientation and size range and overrides base fields; the
  designer edits a variant's overrides on top of the base, with the override markers of the Inspector. Same document
  step as the previous item. `editor.ui_designer_variant` (planned).
- **Interactive preview** (3244). A preview mode driven by the sample model in which controls respond; the document is
  never written by it. `editor.ui_designer_preview` takes `interactive`.
- **Style sources** (3244). The Style section shows each style property's winning source and the losing ones.
  `editor.ui_designer_state` includes `style_sources`.
- **Running-game UI Inspector** (3244). Shows recent events per node and applies live style edits with "copy to
  document". `ui.inspect` events (spec 18), `editor.ui_designer_copy_live_style` (planned).
- **Theme editor** (3245). Previews every widget state live in a gallery, picks a widget to jump to its style,
  authors types, states and variations, and imports items from another theme. `editor.theme_panel_set`,
  `editor.theme_panel_state`, `editor.theme_panel_import` (planned).

Bindings into theme tokens and animation bindings are out of this plan: the v11 fold built them (§ 12, ADR-7751).

## 12. The v11 fold (ADR-7751)

The three § 10 items fold into `.ui.ron` v11 before it ships (ADR-7724's fold rule): optional fields and appended
variants only, so every earlier v11 file (the frozen corpus `ui/v11.ui.ron`) loads unchanged; the corpus adds
`ui/v11-designer.ui.ron`. `UI_RON_VERSION` stays 11; `upgrade_ui_ron` and `migrate_ui` are unchanged.

- **Theme-token binding.** `BindDef.theme: Option<String>` → `BindSource::Theme(token)`. Resolved from
  `ThemeResolver::resolved_theme()` (the active theme; a `ThemeScope` subtree theme is not consulted) and re-read only
  when the resolver's generation moves. Colors show as `#rrggbb` (`#rrggbbaa` below full alpha), numbers and durations
  as numbers, text as itself. An unknown token keeps the target and records `binding_theme_token`.
- **Animation binding.** `BindDef.animation: Option<AnimationBindDef>` → `BindSource::Animation(BindAnimation)`.
  `BindAnimation::sample(t)` is a pure function of the binding's clock (`Bind::clock`), which `update_bindings`
  advances by `Time::delta_secs` after the first push; `repeat` is `AnimationRepeat::{Once, Loop, PingPong}`; `ease`
  names a `tween::Ease`. Reduced motion (`motion::reduced_motion`) pushes the end value. A finished `once` stops
  writing. Theme and animation bindings never write back (`collect_two_way_edits` skips them). `theme` wins over
  `animation`, either over `source` (`source` is optional, not written when empty).
- **Row height from the template.** `RepeatDef.row_height: RepeatRowHeight::{List, Template}`. On a `list` host,
  `Template` sets the list's `ListView::item_height` to `RepeatDef::template_row_height(template)` (the first template
  node's fixed, positive `style.size` height) when the host is made; without one the list keeps its height. The
  designer preview places sample rows the same way. `ui.collection_state` adds `row_height` and `row_px` for a
  virtualized host.
- **Tiled image.** `ImageFit::Tile` (also `UiImageFit`): `extract::image_tiles` places copies the size of the image's
  UV sub-rect in source px times the UI scale, row by row from the rect's top-left, each clipped to the node's rect; at
  most `MAX_IMAGE_TILES` (4,096) copies (a tinier picture grows uniformly); nothing until the picture's size is known.
- **Designer and agent.** Inspector: the binding's **reads** picker (field, theme, animation) and its rows, the
  repeat's **row height** picker on a list, `tile` in the image fit picker. `editor.ui_designer_edit` ops:
  `bind_source {node, binding?, source | theme | animation}` (new), `repeat {…, row_height?}`, `image {…, fit?}`.
  Lint codes `binding_bad_animation`, `binding_source_ignored`, `repeat_template_height`.
- **Tests.** `regia_ui/tests/suite/v11_designer.rs` (both v11 corpus files load; a theme binding follows a switch;
  an animation drives opacity over fixed steps), `repeat_list.rs` (template row height), `group_layers.rs` (tile
  draw list), unit tests of `BindAnimation::sample` and `image_tiles`; editor lint and dispatcher tests; fuzz target
  `ui-designer`.

## 13. Interactive preview and inspection (v0.6.0, ADR-7811)

W12 parity P-263, P-257 and P-268 implement the adopted UI addendum §§ 13.7–13.8:

- Play routes canvas input to a private runtime UI app fed by the existing sample values.
  Commands are recorded; two-way writes target a private reflected sample model. Escape or
  the toggle discards runtime state and returns to editing. Authored bytes and undo history
  stay unchanged. The input mapping uses the runtime widget, focus, scroll and binding systems.
- The selected node's Style section uses runtime inspection to show ordered source values,
  retaining losing entries and their class/variation/token attribution. Token buttons open
  the Theme panel at that token. They preserve unsaved theme drafts.
- Runtime inspection adds a bounded event ring with simulation frame timestamps. Transient
  layout/text edits never save files. Explicit Copy to document checks the exact source node
  and spawn-time values, then uses one undoable designer batch and its existing atomic save.
  Generated repeat children and expanded component internals without an exact inline origin
  remain live-editable; deciding whether a copy edits a template, parameter or instance override
  is deferred rather than choosing an authoring destination implicitly.

Agent contracts and error codes are in `book/agents/ui-designer.md` and
`book/agents/ui-inspector.md`: `editor.ui_designer_preview` adds `interactive` and `input`;
`editor.ui_designer_style_sources`, `ui.inspect_events`, `ui.set_live_style`, and
`editor.ui_inspector_copy_to_document` expose the same actions. Every new panel control has a
keyboard path. No persisted designer field or frozen UI format changes.
