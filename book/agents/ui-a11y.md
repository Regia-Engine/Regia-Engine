# UI accessibility tree and screen readers (agents)

UI series L14, ADR-865 (roadmap L03). `regia_ui::a11y` builds a platform-neutral accessibility tree from the
runtime UI and hands its changes to a screen-reader backend. Accessibility settings (text scale, reduced motion,
right-to-left layout) are a separate page: [UI accessibility](ui-accessibility.md).

## The tree

`build_a11y_tree(world)` walks every `UiRoot` (by `sort_order`) depth first. For each node:

- **Role** from the widget: `UiRoot` → `window`, `Button` → `button`, `Toggle` → `check_box`, `Slider` → `slider`,
  `ProgressBar` → `progress_bar`, `TextInput` → `text_input`, `Dropdown` → `combo_box`, `TabBar` → `tab_list`,
  `Modal` → `dialog`, `RadialMenu` → `menu`, `ListView` → `list`, `ScrollView` → `scroll_view`, `Text` → `label`,
  `UiImage` → `image`, anything else → `group`. `heading`, `region`, `status` and `alert` come only from
  `Accessible`.
- **Name**: `Accessible::label_key` (a localization key, used when the table defines it) or `Accessible::label`,
  else the node's own text, else the text of its label children (childless `Text`
  children, which then do not appear on their own), else a text field's placeholder.
- **Description and hint**: `Accessible::description` and `Accessible::hint`, each with a `_key` form that resolves
  through `Localization` (the literal is the fallback when the key is undefined; an undefined key with no literal
  shows the table's missing-key text). The tree is rebuilt every frame while a reader is live, so a locale change
  re-resolves them, and the diff reports a `name_changed`. The hint is a short usage line ("Double tap to open") read
  last in the spoken summary.
- **Value**: a slider's number, a progress bar's percentage, a text field's text (masked when it is secret), the
  selected option of a dropdown or the active tab.
- **States**: `focused`, `focusable`, `disabled`, `selected`, `invalid`, `checked` (check boxes), `expanded`
  (dropdowns, dialogs), `inert` (outside the top input layer, such as a menu under an open dialog) and
  `read_only` (a text field that takes no edits).
- **Bounds** from the layout, in physical px.

An unnamed container with no role, no `Accessible` and no focus is flattened: its children move up to its parent.
`Display::None` subtrees and nodes with `Accessible { hidden: true }` (`Accessible::decorative()`) are left out.

`spoken_summary()` is what a reader says on focus, for example "Volume, slider, 50" or "Subtitles, check box,
checked".

## The custom-widget hook

Add `Accessible` to a node to set what inference cannot know:

```rust,wrap
# use regia::prelude::*;
# use regia_core::math::{Vec3, Quat};
# use regia_scene::Transform;
# let mut commands: Commands = todo!();
# let world: &mut World = todo!();
# let app: &mut App = todo!();
# use regia_ui::a11y::*;
commands.spawn((UiNode, Text::new("Ready"), Accessible::default()
    .with_role(A11yRole::Status)
    .with_live(Politeness::Polite)));
```

A live region (`live` polite or assertive) announces its new name or value whenever it changes.

## Backends

`ScreenReaderBackend` is the platform seam: `name`, `is_active`, `tree_updated(tree, events)` and, for recorders,
`recent_events`. The `ScreenReader` resource owns the backend. Each frame (after text shaping) `sync_screen_reader`
rebuilds the tree only while the backend is active or exporting was turned on (`ScreenReader::set_exporting`), so a
game with no screen reader pays nothing. It diffs the tree against the last one and sends `added`, `removed`,
`name_changed`, `value_changed`, `states_changed`, `screen_changed`, `focus_changed` and `announcement` events.
`screen_changed` fires when the top input layer changes (a dialog opened or closed, `from` and `to` are the layer
roots), so a bridge knows when to move the reader's cursor. The first sync after
activation reports every node as added. `ScreenReader::announce(text, politeness)` queues speech.

- **Linux**: `atspi::AtSpiStub` maps roles onto AT-SPI 2 role names ("push button", "check box", "entry", ...) and
  events onto the signals an AT-SPI bridge emits (`object:children-changed:add`, `object:state-changed:focused`,
  `object:property-change:accessible-name`, `object:announcement`, ...), with object paths
  `/org/a11y/atspi/accessible/<id>`. It records them and does not connect to D-Bus yet. It is active when
  `REGIA_SCREEN_READER=1`.
- **Windows, macOS, Android**: no backend yet (`none`). UI Automation, NSAccessibility and Android
  `AccessibilityNodeInfo` bridges plug into the same trait.

## Requests: what a reader can do

Reading is half of it. A reader also presses, steps, types, focuses, dismisses and scrolls. An `A11yRequest` is a
node id plus an `A11yAction`; a backend queues them (`ScreenReaderBackend::drain_requests`, empty by default) and
`route_a11y_requests` (`PreUpdate`, only while the reader is live) runs each through `apply_request`, which checks it
against a fresh tree and sends it down the path a key press, click or wheel takes. Then it tells the backend how it
ended (`request_result`).

| Action | Does | Refused when |
|---|---|---|
| `press` | clicks a button, flips a check box, opens a combo box, focuses a text field | slider, or a node that is not a control |
| `raise`, `lower` | steps a slider by its own `step` (a tenth of its range when it has none), clamped | not a slider |
| `set_text` | replaces a text field's text the way typing it would: control characters are stripped, `max_chars` cuts whole grapheme clusters | not a text field, it is read-only, the field's filter refuses the text, or the text is longer than 4096 characters |
| `focus` | moves keyboard focus (the focus ring shows, as after a Tab) | node is not focusable |
| `close` | dismisses the dialog the node is, or is inside, the way Cancel does | no dialog, or `close_on_cancel` is off |
| `scroll_into_view` | scrolls the enclosing scroll view until the node shows | no enclosing scroll view |
| `page_up`, `page_down`, `page_left`, `page_right` | scrolls the node's scroll view (or the enclosing one) by 90% of its size | no scroll view |

Every request is refused, with no effect, when the node is unknown or not in the tree (`unknown_node`), `Disabled`
(`node_disabled`, as a click on a disabled control is) or outside the top input layer (`node_inert`, a menu under
an open dialog). Widgets react in the same update, so the result shows after the next frame.

`RecordingReader` is the headless backend for tests: always active, records events and request results, and takes
requests through its `RecordingHandle`. The OS bridge is the `accesskit` feature (ADR-1998): see
[Screen-reader OS bridge](ui-a11y-platform.md) for the platform tree and simulated platform actions. Without it,
`atspi::request_from_call(path, action, text)` maps AT-SPI calls to requests and `AtSpiStub::request_queue()` takes
them.

## Methods

- `ui.a11y_tree { node? }` (read-only, permission `read`). Builds the tree now, whatever the backend. `node`
  (alias `entity`) limits it to one node and its descendants. Returns `{ backend, live, builds, focus, count,
  top_layer, nodes: [{ id, parent, children, role, name, description, hint, value, states, bounds, live,
  summary }], announcements, backend_events, requests: [{ node, action, accepted, reason }], sync_cost }`. `backend_events` are the backend's last 64 signals (the AT-SPI stub's on
  Linux). `sync_cost` (ADR-5379) is `{ samples, total, mean_ms, p99_ms, max_ms, stages_mean_ms, ui_entities }`: what the per-frame screen-reader
  sync (`regia_ui::a11y::sync_screen_reader`) cost on its last 512 live frames (a reader connected, or exporting
  on). `stages_mean_ms` splits the mean into `build`, `diff` and `backend`; `ui_entities` is the UI node count. Frames with no reader cost one resource lookup and are not counted. `null` without a `ScreenReader`.
- `ui.a11y_announce { text, priority? }` (permission `play`). Queues `text` for the next frame. `priority` is
  `polite` (default) or `assertive`. Returns `{ queued, backend, live, priority }`.
- `ui.a11y_act { node, action, value? }` (permission `play`). Does what a reader's request would (table above).
  `node` (alias `entity`) is a tree node id; `action` is one of `press`, `raise`, `lower`, `set_text`, `focus`,
  `close`, `scroll_into_view`, `page_up`, `page_down`, `page_left`, `page_right`; `value` is the text for `set_text`.
  Returns `{ node, action, accepted, summary, note }` where `summary` is the node's spoken summary as of the call;
  the widgets react in the next frame, so wait a frame (`wait_until`) and read `ui.a11y_tree` for the result.
  `ui.a11y_tree` lists the last 32 requests with `accepted` and the refusal `reason`.

```json
{"method": "ui.a11y_tree", "params": {}}
{"method": "ui.a11y_announce", "params": {"text": "Checkpoint reached", "priority": "assertive"}}
{"method": "ui.a11y_act", "params": {"node": 4294967300, "action": "raise"}}
{"method": "ui.a11y_act", "params": {"node": 4294967310, "action": "set_text", "value": "Ada"}}
```

Checks worth automating: every focusable node has a non-empty `name`; nothing outside an open dialog is focusable
without `inert`; a status text is a live region.

## Errors

| Code | Meaning |
|---|---|
| `bad_entity` | `node` is not a node id |
| `unknown_entity` | the entity does not exist |
| `not_in_tree` | the node is hidden, decorative or a flattened container |
| `a11y_text_missing` | `text` is missing |
| `bad_text` | `text` is not a non-empty string |
| `text_too_long` | `text` has more than 1024 characters, or `set_text`'s `value` more than 4096 |
| `bad_priority` | `priority` is not `polite` or `assertive` |
| `a11y_node_missing` | `ui.a11y_act` without `node` |
| `a11y_action_missing` | `ui.a11y_act` without `action` |
| `a11y_value_missing` | `set_text` without `value` |
| `bad_action` | `action` is not a string, or not one of the eleven names |
| `bad_value` | `value` is not a string |
| `unknown_node` | the node is gone, hidden, decorative or a flattened container |
| `node_disabled` | the node is `Disabled` |
| `node_inert` | the node is outside the top input layer |
| `unsupported_action` | the node cannot do that action (see the table) |
| `not_dismissable` | the dialog has `close_on_cancel` off |
| `no_scroll_view` | the node is in no scroll view |
| `ui_not_installed` | the app has no `ScreenReader` (no `UiPlugin`) |

## Known limits

- Without the `accesskit` feature no backend talks to a real screen reader: the AT-SPI stub records signals only.
  With it, [the OS bridge](ui-a11y-platform.md) does (ADR-1998; TalkBack on Android is ADR-3454). Steam Deck
  game mode has no screen-reader path; desktop mode on the Deck is AT-SPI.
- Hints and labels take a key without arguments; a label that needs arguments is a `Text` child with a
  `TextContent::Loc`.
- Text inside a text field is reported as one value; caret and selection are not exported.
- List rows are exported as they exist: a virtualised list exposes only its live rows.
- The tree walk is bounded: at most 512 hierarchy levels (`MAX_A11Y_DEPTH`) and 65,536 nodes (`MAX_A11Y_NODES`),
  depth first, so the first nodes in reading order are kept; past either limit the rest is left out and a warning
  is logged.
