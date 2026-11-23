# The UI designer

The editor's **UI Designer** tab builds a runtime screen the way it will look in the game. A node
tree sits on the left, a preview laid out by the runtime's own layout in the middle, and an inspector
with bindings and lint on the right. What you save is a `.ui.ron` file that the game spawns. This
chapter builds one screen from an empty document. For how the runtime UI itself works read
[Runtime UI](ui.md) first; for translations read [Localization](localization.md). Method-level detail
is on the agent page [UI designer and view-model binding](../agents/ui-designer.md) and the format is
`docs/formats/ui-ron.md` (version 6).

## 1. Start a document

Open the **UI Designer** tab and either open an existing file (older versions are migrated in
memory; the file on disk is untouched until you save) or start a new document with a path to save
to. Nodes are addressed by a path from the root (`/` is the root, `/0/2` is child 2 of child 0) or by
`#name`.

Plan the screen as a tree. A root column holds a title, a settings panel and a button row:

```ron,ui
(
    version: 6,
    root: (
        style: (direction: column, padding: 16, gap: (12, 12)),
        children: [
            (name: "title", text: (spans: [Loc("settings-title")], style: (font_px: 28))),
            (name: "panel", classes: ["surface"], style: (direction: column, padding: 12, gap: (8, 8)),
                children: [
                    (name: "volume", widget: slider(0.5, 0.0, 1.0, 0.05),
                        bindings: [(source: "SettingsVm.volume", target: value, mode: two_way)]),
                    (name: "mute", widget: toggle(false),
                        bindings: [(source: "SettingsVm.muted", target: checked, mode: two_way)]),
                ]),
            (name: "buttons", style: (direction: row, gap: (8, 8)), children: [
                (name: "apply", widget: button, classes: ["primary"], text: (spans: [Loc("settings-apply")]),
                    command: "settings.apply",
                    bindings: [(source: "SettingsVm.dirty", target: enabled)]),
                (name: "back", widget: button, classes: ["ghost"], text: (spans: [Loc("settings-back")]),
                    command: "settings.back"),
            ]),
        ],
    ),
)
```

## 2. Build the hierarchy

The **palette** has one chip per preset: `panel`, `text`, `button`, `toggle`, `slider`, `progress`,
`text_input`, `dropdown`, `image`, `list` and `scroll`. Drag a chip onto a row in the hierarchy or a
node in the preview. The upper and lower quarter of the target place the new node before or after it;
the middle makes it a child. In the hierarchy you can also duplicate, remove, move up and down, or
drag a row to reparent it. Each is one undo step, and Ctrl+Z, Ctrl+Shift+Z and Ctrl+Y undo and redo
(200 steps) while the pointer is over the panel and no text field has focus.

`spawn_ui` uses only the root node's `style` and `children`. Put `safe_area`, `background` and the
like on a child; the lint reports a root that sets them (`root_field_ignored`).

## 3. Layout

Layout is a subset of flexbox. In the inspector, typed rows cover display, position, direction,
justify, align, size, min and max size, gap, margin, padding, inset, grow, shrink, opacity, wrap,
clip, interactable, disabled and background color. One drag on a number is one undo step. Any other
field has a JSON editor (for example `style.size` = `[200, "Auto"]`).

- Use `direction: column` or `row` on containers and let children grow and shrink, rather than
  fixed sizes. A fixed size is for things that really are fixed, such as an icon.
- Sizes accept `Auto`, pixels or `Percent(n)`.
- A node only receives clicks when it is `interactable` (a `button` is); the lint flags a command on
  a node that gets none (`command_not_interactable`).
- Give nodes a unique `name`. The lint warns on duplicates (`duplicate_name`) because `#name` and
  `ui.query` find the first.

## 4. Theme classes

A node's `classes` pick its look from the active theme; you do not set colors per node. A class names
a widget type or a variation of it: `button` is `Button`, `primary` is `Button.primary`, `surface` is
`Panel.surface`, and a class may name one directly (`"Button.ghost"`). The built-in `regia-dark` theme
has these variations:

| Widget | Variations |
|---|---|
| `Button` | `primary`, `secondary`, `danger`, `ghost`, `nav` |
| `Panel` | `surface`, `overlay` |

For each property the first hit wins: the node's own override, then the variation, then the widget
type, then the theme defaults. A disabled widget draws at 0.45 opacity. Three more themes ship:
`regia-light`, `regia-high-contrast` and `regia-light-high-contrast`. To re-skin the whole game, write
a `.theme.ron` that `extends` one and overrides tokens such as `color.accent`; see
[UI themes](../agents/ui-theme.md). Tick **Themed** in the preview to see the document drawn by the
runtime renderer with your project's theme (`assets/ui/themes/*.theme.ron`) instead of the
schematic fills; it needs a Vulkan device and lavapipe counts.

## 5. Bindings

Layouts bind to **view models**, plain reflected resources your game registers, so the game never
touches widget entities:

<!-- ignore-reason: a fragment of a game setup; the app and Reflect derive come from the game -->
```rust,ignore
#[derive(Resource, Reflect, Default)]
#[reflect(Resource, Default)]
struct SettingsVm { volume: f32, muted: bool, dirty: bool }

app.register_view_model::<SettingsVm>(); // regia_ui::ViewModelAppExt
```

A binding path is `"Model.field"`. Once view models are registered, the inspector offers a model and
field picker, and the lint knows which names exist. A node can carry any number of `bindings`:

| Field | Meaning |
|---|---|
| `source` | `"Model.field"`, nested fields with more dots. |
| `target` | `text` (default), `progress`, `opacity`, `checked`, `value` (slider), `input` (text input), `selected` (dropdown or tab bar), `visible`, `enabled`. |
| `mode` | `one_way` (default), `two_way` (the widget writes back), `one_time`. |
| `format` | `"{:03}"`, `"{:.1}"` and so on. |
| `invert` | Negate a boolean for `checked`, `visible`, `enabled`. |
| `converter` | Transform the value first (below). |

A `command` on a node is sent as a `UiCommand { name, node, value }` message on a click, or on a text
submit with the text. Handle it in a system; do not read widget state.

Only registered view models are written to, and a widget edit is applied before the game's systems run
that frame. If your game writes the same field later in the frame, the game's value wins and is pushed
back. A binding problem is kept per binding rather than logged every frame, with one of these codes:
`binding_unresolved`, `binding_not_view_model`, `binding_type_mismatch`, `binding_item_outside_repeat`,
`binding_converter_one_way`. `ui.view_models {}` in a running game lists every model, its fields and
every binding with its last value and error.

### Converters

A converter changes the value before `format` and `invert`. The set is closed, with no scripting:
`number(precision: 2, group: ",")`, `clamp(min: 0, max: 100)`,
`map_range(from: (0, 100), to: (0, 1), clamp: true)` (these three work both ways),
`bool_to_visibility`, `enum_text(table: [("Idle", "Waiting")], default: "?")` and
`loc_key(prefix: "state-")` (these three are one-way; a two-way binding through one is the lint error
`converter_one_way`).

### Lists

A node with `repeat` spawns its children once per item of a list field, with a keyed diff so a row
keeps its state when items are reordered or removed. Always set `key` to something stable:

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

Inside a template, a source starting with a dot reads the item (`".name"`; `"."` is the item). A
row is real entities, so for long collections put the `repeat` on a `list` widget: it materializes
only the visible rows plus two of overscan each side, so a 10,000-item inventory keeps about 15 rows
alive. Make the template root `Percent(100)` tall so it fits the row height, or the lint warns
`repeat_on_list`.

## 6. Preview sizes and safe areas

The preview lays the document out at a window size. Pick a preset or type a custom size (1 to 8192
on a side):

| Preset | Size | Safe-area insets (left, top, right, bottom) |
|---|---|---|
| `1920x1080` | desktop | none |
| `1280x720` | small desktop | none |
| `1280x800` | handheld | none |
| `2400x1080` | phone, landscape | 110, 0, 60, 0 |
| `1080x2400` | phone, portrait | 0, 110, 0, 60 |

The green frame is the safe area. Nodes that cross it are drawn red and listed as
`outside_safe_area`, so you find a button under a camera cutout before a tester does. You can set
your own insets as `[left, top, right, bottom]`. The layout rects are the runtime's, so the same file
at the same size lays out the same in the game. Always check the phone landscape and portrait
presets; those are where layouts break.

A node with a binding has no model value in the editor. Give it one with **samples**, which feed only
the preview: `{"SettingsVm.volume": 0.8}`. Under a repeat the preview shows sample items (3 by
default, set with the `items` box or `repeat_count`, up to 256): `"Vm.items[].name"` fills every row,
`"Vm.items[1].name"` one item, and `"Vm.items[]#count"` sets the count.

## 7. Preview in another language

Text uses localized spans (`Loc("settings-title")`). The preview shows them in a locale you choose,
resolved from the project's `assets/localization/<locale>/*.ftl`. A key the locale lacks shows as
`⟦key⟧`, so missing translations are visible at a glance. Tick **pseudo-localization** to expand
and accent all resolved text: that finds clipped labels and hard-coded English in one look, before
any translator touches the file. Check the longest language you ship (German often wins) at the
smallest size. The workflow for writing and validating translations is in
[Localization](localization.md).

## 8. Lint

The lint refreshes on every change. Errors mean the screen is wrong; warnings mean it is probably
wrong:

- Errors: `binding_empty_source`, `binding_target_mismatch` (for example `value` on a node that is not
  a slider), `binding_unknown_field`, `command_not_interactable`, `converter_one_way` and the
  `repeat_*` source errors (`repeat_bad_source`, `repeat_outside_template`, `repeat_not_a_list`,
  `repeat_unknown_field`).
- Warnings: `duplicate_name`, `binding_two_way_read_only`, `binding_unknown_model`,
  `converter_bad_settings`, `converter_target_mismatch`, `repeat_empty_template`, `repeat_no_key` and
  `repeat_on_list`, plus `root_field_ignored`.

The model checks run only when your game has registered view models; the answer's `schema` field says
whether they were known.

## 9. Save

**Save** rewrites only what changed in the file it was opened from. Comments, key order and every
other line stay byte for byte; a removed node takes its comments with it. The text is parsed back and
compared before a temp file and a rename put it in place. If the result would differ, the canonical
form (only fields that differ from their defaults) is written instead and the panel says the
comments were not kept. A new document is always written in the canonical form.

## 10. Check it in the game

Run the game and use the same layout as an agent would:

1. `ui.view_models {}`: every binding should have a `last` value and no error.
2. `ui.query` to find a node by name, then `ui.set_value` on the slider. Read the field back with
   `ui.view_models` on the next frame to prove the two-way path.
3. For lists, `ui.collection_state` reports each repeat's instances and last pass.
4. A screenshot at the phone sizes, with the safe area applied.

## Through an agent

Every panel action has an `editor.ui_designer_*` method: `open`, `state`, `edit` (`add`, `remove`,
`move`, `duplicate`, `set`, `repeat`, `converter` and undo/redo ops), `lint`, `preview`, `render` and
`save`. Mutations need the `edit_scene` permission; opening and saving need `assets`. Errors carry a
code such as `ui_designer_no_document`, `ui_designer_bad_node`, `ui_designer_bad_value`,
`ui_designer_parse`, `ui_designer_bad_size` or `ui_designer_no_gpu` (only `render` needs a GPU;
`preview` never does). The list is on the [agent page](../agents/ui-designer.md).

```json
{ "method": "editor.ui_designer_open", "params": { "path": "assets/ui/settings.ui.ron" } }
{ "method": "editor.ui_designer_lint", "params": {} }
{ "method": "editor.ui_designer_preview", "params": { "size": "2400x1080", "locale": "de", "samples": { "SettingsVm.volume": 0.8 } } }
{ "method": "editor.ui_designer_save", "params": {} }
```
