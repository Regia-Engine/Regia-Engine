# Editor accessibility (agents)

The editor measures its own theme against WCAG 2.x AA, and its widgets carry AccessKit names
that tests audit (ADR-9810). The person-facing page is [Editor
accessibility](../user-guide/editor-guide/accessibility.md); the review with every finding is
`docs/evidence/editor-a11y-2026-11.md`.

## `editor.a11y_contrast`

```json
{ "method": "editor.a11y_contrast", "params": {} }
{ "method": "editor.a11y_contrast", "params": { "theme": "regia-light" } }
```

Measures each text color of an editor theme against every surface it is drawn on: the ground,
panels, windows, fields and the resting, pressed and open control fills (needs 4.5:1, the AA ratio
for text), Signal as link and active text, the inverted selection, every status color; and the
control outline, the keyboard focus ring, the selection block and the play-session tints (needs
3:1, the AA ratio for a control's boundary and graphical objects). The pair list is the PiercingXX
UI kit's contrast check ported onto the editor tokens (ADR-8585, 83 rows; the first 18 are the
November 2026 audit's rows, same names). Without `theme` it measures the active editor theme;
`default` (Piercing Graphite), `amoled-night`, `paper`, `high-contrast`, `regia-dark` and
`regia-light` (also written `ui:regia-dark`, ...) measure that theme whatever is active. Every
built-in editor look answers `failing: []`. It reads nothing from disk and changes nothing (permission `read`).

Answer:

```json
{
  "theme": "active",
  "rows": [
    { "pair": "text on panel", "fg": "#e7e7e8ff", "bg": "#18181bff", "ratio": 14.6, "required": 4.5, "passes": true },
    { "pair": "edge on raised", "fg": "#7c7c84ff", "bg": "#1f1f23ff", "ratio": 3.97, "required": 3.0, "passes": true }
  ],
  "failing": []
}
```

(two of the 83 rows shown). `fg` is the stored color as `#rrggbbaa`, premultiplied as egui
keeps it; a translucent foreground is measured as drawn over `bg`. `pair` names the theme tokens
(`text`, `muted`, `widgets.button_text`, `accents.error_text`, ...) of a `.theme` file or
`editor.set_theme`'s `tokens` ([Editor theme](theme.md)), so a theme author can fix a failing
pair and call again.

| Code | When |
|---|---|
| `unknown_theme` | `theme` is not `default`, `amoled-night`, `paper`, `high-contrast`, `regia-dark` or `regia-light` |
| `bad_level` | `level` is not `aa` or `aaa` |

`level: "aaa"` raises every text pair to 7:1 (WCAG 1.4.6; large text needs 4.5:1, so passing the body bar covers
it) and keeps 3:1 for the control boundary; the answer carries `level`. The `high-contrast` theme passes every row at
`aaa` ([Editor theme](theme.md#high-contrast)). A control is measured by its outline at rest: `widgets.edge` when the
theme has one (the brand looks, ADR-8585); else, for a control filled with the panel color, the `line` color or the
strong border `widgets.hover_line` when `line` is a hairline under 3:1 (`theme::idle_outline`, ADR-8589); else its
fill. The focus ring is `signal` in a theme with `widgets.edge`, else the pressed outline `strong`.

## AccessKit names in tests

`regia_editor::a11y_audit` has the helpers the editor uses for widgets whose visible text is a
glyph or empty (`glyph_button`, `bare_checkbox`, `name`, `warning_glyph` for a status glyph whose
meaning is its hover text, `named_drag_value` for an unnamed drag value, `spoken_only` for a
focusable pane, which must not grow a pane-sized tooltip) and three readers of a frame's AccessKit
tree for tests: `unnamed_widgets` lists the operable nodes (buttons, checkboxes, text inputs,
sliders, ...) with no name, `widget_names` lists every operable node's role and name, and
`label_names` lists the spoken text of the `Label` nodes (static text, such as a `warning_glyph`,
which keeps the `Label` role and so never appears in `widget_names`: a screen reader must not
announce a button that does nothing). A test turns AccessKit on, draws a panel and checks the
tree:

```text
let ctx = egui::Context::default();
ctx.enable_accesskit();
let output = ctx.run_ui(egui::RawInput::default(), |ui| {
    ui.horizontal(|ui| {
        let _ = regia_editor::shell::play_buttons(ui, Some(regia_editor::pie::PlayMode::Paused));
    });
});
let update = output.platform_output.accesskit_update.expect("accesskit is on");
assert!(regia_editor::a11y_audit::unnamed_widgets(&update).is_empty());
```

## Tests

```sh
cargo test -p regia_editor --lib a11y_audit                 # contrast math, the themes' failing pairs, names, the method
cargo test -p regia_editor --lib a_paused_session_resumes   # the play toolbar: Resume, names, Tab + Enter
cargo test -p regia_editor --lib keyboard_focus_activates_a_pane_and_its_hotkey   # FU-9: Tab into a viewport pane
cargo test -p regia_editor --lib enter_or_space_on_a_focused_pane_keeps_the_selection  # Enter/Space on a pane never picks
cargo test -p regia_editor --lib a_focused_pane_takes_the  # 2D mode and terrain keys under pane focus; a text field keeps them
cargo test -p regia_editor --lib a_focused_pane_keeps_its_keys_while_another_pane_is_hovered
cargo test -p regia_editor --lib a_warning_glyph_is_a_label_that_speaks_its_message  # FU-7: Label role, not Button
cargo test -p regia_editor --test inspector_containers bool_fields_and_list_buttons_carry_spoken_names  # FU-6
cargo test -p regia_editor --lib a_header_warning_carries_its_text_as_a_spoken_name  # FU-7 (matgraph)
cargo test -p regia_editor --lib a_node_error_carries_its_text_as_a_spoken_name      # FU-7 (procgen)
cargo test -p regia_editor --lib half_extent_drag_values_are_named_per_axis          # FU-8
cargo test -p regia_editor --test extension_method_calls the_contrast_report
cargo test -p regia_editor --lib edits_by_keyboard_what_the_mouse_edits   # FU-5: one test per canvas
cargo test -p regia_editor --lib every_painted_canvas_has_a_keyboard_route_or_a_reason  # the canvas scan
cargo test -p regia_editor --lib ctrl_0_stores_bookmark_0_and_leaves_the_interface_zoom  # FU-4
cargo test -p regia_editor --lib zoom_keys_change_the_saved_ui_scale    # FU-4: commands, binding, prefs
```

## Canvas list views (ADR-7383)

The painted canvases have a focusable list beside them (`regia_editor::canvas_keys`) for people
on a keyboard or a screen reader. They add no method: each list action is the edit an existing
method makes, through the same code path, so an agent drives the same result directly.

| Panel | List | Agent method for the same action |
|---|---|---|
| Material graph | **Nodes**: Ctrl+Shift+A adds a node wired after the focused one; **Connect…**; Delete; Alt+arrow moves 16 units | `editor.matgraph_edit` `add` + `connect`, `connect` / `output`, `remove`, `move` |
| Procedural graph | **Nodes**: the same keys | `procgen.graph_edit` `add_node` + `connect`, `connect`, `remove_node`, `move` |
| Sequencer curve | **Keys of <track>**: Left / Right one frame, Delete | `editor.seq_curve_edit` `move`, `remove` |
| Sequencer section strip | **Sections of <track>**: Left / Right one frame, Delete | `editor.seq_section_edit` `move`, `remove` |
| Profiler flame view | **Spans**: name, parent, thread, inclusive and self ms, calls | `profiler.frame` |
| Trace timeline | **Items in view**: name, lane, frame, ms | `editor.trace_timeline` |

Each canvas is an AccessKit node with role `Canvas` and a name with its count ("Material graph
canvas, 12 nodes", "Sequence curve, 4 keys", "Flame view, 37 spans"). Tests read them with
`a11y_audit::widget_names` and check `a11y_audit::unnamed_widgets` is empty for each panel.

### More canvases (FU-5, w11)

The same lists now sit beside eleven more canvases. Every key is the mouse's edit through the
same code path (the same pending edit, op or widget action), so the agent method in the last
column gives the same document.

| Key | Does (in every list) |
|---|---|
| Tab / Shift+Tab | Reach the list (each row is a focusable widget named after its item) |
| Up / Down | Move the focus to the previous / next item |
| Enter | Select the item, as a click does (see the table for what a second Enter does) |
| Left / Right, Alt+arrow | Move the item; Shift: a coarser step |
| Delete | Remove the item, as the canvas's right-click or menu does |
| Ctrl+Shift+A | Add an item after the focused one, as the canvas's double-click or add menu does |
| Ctrl+Shift+C | Start a connection from the item (opens **Connect…**); Enter in its last field connects, Escape cancels |

| Canvas | List | Keys beyond the common ones | Agent method for the same action |
|---|---|---|---|
| Animation graph | **Nodes** ("Clip #1") | Alt+arrow moves 16 units (Shift: 64); Ctrl+Shift+A adds a node wired after the focused one; Ctrl+Shift+C wires it into a node ("Blend #2", "#2") or "Output", with an input pin by name or index; Enter on the selected state machine opens its states | `editor.anim_add_node` + `editor.anim_connect`, `editor.anim_set_node` `pos`, `editor.anim_remove_node` |
| State machine | **States**: the states, "Any state", the transitions | Alt+arrow moves a state; Ctrl+Shift+A adds a state with a transition from the focused one (or Any); Ctrl+Shift+C adds a transition to a state typed by name; Delete removes a state or a transition; Enter on the selected state opens its sub-graph | `editor.anim_state` `add` / `move` / `remove`, `editor.anim_transition` `add` / `remove` |
| State tree | **States** ("Idle, under Root, level 1") | Alt+Up / Alt+Down reorder among siblings, Alt+Left moves out after the parent, Alt+Right moves under the previous sibling; Delete is **Remove state**; Ctrl+Shift+A selects the state and focuses the palette's new-state field | `editor.statetree_edit` `reparent_state`, `remove_state`, `add_state` |
| VFX curves | one key list per curve ("size_over_life key 1 of 2: time 0.00, value 1.00") | Left / Right: time ±0.01 (Shift ±0.1); Alt+Up / Alt+Down: value ±5 % of the plot range (Shift ×4); presses on one key coalesce into one undo step, as a drag does | `editor.vfx_edit` `curve_move_key`, `curve_delete_key`, `curve_add_key` |
| Conversation | **Nodes** (label or id, first step) | Alt+arrow moves the node; Enter on the selected node previews from it; Ctrl+Shift+C retargets one of its jumps (`goto`, `then`, `else`, `option <n>`) to a labelled node | `editor.conversation_edit` `move`, the `retarget` op |
| UI animation | key list of the shown track ("opacity key at 0.50 s, value 0.50") | Left / Right: time ±0.01 s (Shift ±0.1), snapped to 10 ms; Alt+Up / Alt+Down: value ±0.01 (Shift ±0.1); Ctrl+Shift+A adds a copy 0.1 s later | `editor.ui_anim_set_key`, `editor.ui_anim_remove_key` |
| Camera rig graph | **Nodes** ("walk node 2: boom, length param len") | Enter selects the node (its fields show below); the graph lays itself out, so there is no move | `editor.camera_rig_graph` (read) |
| Blend space | sample list ("Sample 2: Run at 1.00, 1.00") | Enter puts the preview input on the sample; Alt+arrow moves it one snap step (a 1D space also takes Left / Right; Shift: four steps) | the graph inspector's blend-space edits (`editor.anim_set_node` `data`) |
| Animation timeline | key and marker list ("hips key 2 at 1.00 s") | Left / Right: one snap step (else 1/30 s; Shift ×10), one finished move per press | the timeline's events, applied by its panel |
| Curve editor (clips, shots) | key list ("Key 2 of 3: 1.00 s, value 0.50, linear") | Left / Right: 1/30 s (Shift ×10); Alt+Up / Alt+Down: 5 % of the value span (Shift ×4); the interpolation kind stays on the right-click menu | `editor.anim_clip` `set_curve`, `editor.shot_panel_edit` |
| Clip notifies | notify list ("Notify FootL at 0.250 s") | Left / Right: 1/30 s (Shift ×10) | `editor.anim_clip` `set_notify` / `add_notify` / `remove_notify` |
| Content grid | the tiles themselves (named after the file) | Arrows move the focus and the selection a tile or a row at a time | `editor.content_index` (read) |
| Logic graph (ADR-8628) | **Nodes** ("go: Start (#1)") | Enter selects and frames the node; Alt+arrow moves it 16 units (Shift: 64); Ctrl+Shift+A adds a node its output feeds, wired; Ctrl+Shift+C wires it into a node typed by row name, `#id` or a unique title (Escape cancels); Delete removes it | `editor.logic_edit` `select`, `move_node`, `add_*` + `connect`, `connect`, `remove_node` |

On each canvas the focused item draws a ring in the theme's focus color
(`canvas_keys::focus_color`, the `accents.select` token) and the canvas is an AccessKit `Canvas`
node with its count ("Animation graph canvas, 3 nodes", "State machine canvas, 2 states, 1
transitions", "Timeline canvas, 4 keys, 2 markers").

`canvas_keys::CANVAS_ALLOW` lists every painted canvas of the editor without a list, with its
class (`view_only`, `not_canvas`, `own_keys`, `pending`) and why; the test
`canvas_keys::scan_tests::every_painted_canvas_has_a_keyboard_route_or_a_reason` scans the
editor's source for painted regions (`SnarlWidget::new(`, `allocate_painter(`,
`Sense::click_and_drag()`) and fails for a new one with neither a `canvas_keys` route nor a row.

## Interface zoom (ADR-7733)

egui's own keyboard zoom is off: its Ctrl+0 reset clashed with camera bookmark 0. The commands
`view.zoom_in` (Ctrl+Plus, Ctrl+=), `view.zoom_out` (Ctrl+Minus) and `view.reset_zoom`
(Ctrl+Alt+0, also **View ▸ Reset zoom**) step the `appearance.ui_scale` preference by 0.1 within
its 0.5 to 3.0 range, so the zoom is saved. Agents run them with `editor.command`:

```json
{ "method": "editor.command", "params": { "run": "view.reset_zoom" } }
```

They answer the status line ("Interface zoom 100 %"); `editor.prefs_get` reads the scale back.
A user binding (`editor.command` `bind`) replaces a command's built-in chord.

## Screen reader (ADR-7746)

The editor window has an OS accessibility tree: egui's AccessKit tree goes to the same OS
adapter a game's window uses (AT-SPI on Linux for Orca, UI Automation on Windows for NVDA and
Narrator, NSAccessibility on macOS for VoiceOver; ADR-1998), and a reader's requests come back
as egui input. It is the editor's `accesskit` cargo feature, on by default (it turns on
`regia_platform/accesskit-adapters`, the adapters without the runner's hooks, so a workspace
build that unifies it in adds no system to a game app; ADR-7795);
`REGIA_ACCESSKIT=0` turns the OS adapter off at run time. egui builds the tree only while a
reader is connected or an agent asked for it, so an editor nobody reads pays nothing.

The play toolbar and the viewport toolbar are `Toolbar` nodes ("Play toolbar", "Viewport
toolbar"), every dock panel is a `Pane` node named after its tab ("Inspector", "Content",
"Outliner", ...), and the root window is "Regia Engine Editor". What a reader can do is what
egui's widgets do with an AccessKit request: `click` (the default action) presses a button,
ticks a checkbox, opens a collapsing section, selects a row; `focus` moves the keyboard focus;
`set_value`, `increment` and `decrement` change a drag value or a slider; `scroll_into_view`
and the scroll actions scroll. A text field takes no `set_value` in egui 0.36: focus it and type.

### `editor.a11y_keep_tree`

```json
{ "method": "editor.a11y_keep_tree", "params": { "on": true } }
```

`on: true` builds the tree every frame without a reader (the next frame has it), `on: false`
goes back to building it only for a reader. Building the tree costs UI time every frame, so
this is a mutating call with permission `edit_scene`: a read-only client cannot turn it on
(ADR-7795). Answer `{ "keep_on": true, "tree_on": false }` (`tree_on` turns true with the next
frame). Error `invalid_value` when `on` is not a boolean.

### `editor.a11y_tree`

```json
{ "method": "editor.a11y_tree", "params": {} }
{ "method": "editor.a11y_tree", "params": { "role": "Button", "name": "Remove Beacon" } }
```

The tree as the OS adapter has it (the bridge's checked copy), depth first; it answers no nodes
until a reader is connected or `editor.a11y_keep_tree { on: true }` ran a frame ago. `role` (an
AccessKit role: `Button`, `CheckBox`, `TextInput`, `SpinButton`, `Toolbar`, `Pane`, ...) and
`name` (exact) filter the nodes. Each node's `name` is the name the OS adapter exposes, which is what a reader
speaks (ADR-8587). A `Label` is named by its text (its value). Any other node is named by its label, else by the
nodes it is labelled by (a field named by the label beside it, such as the play toolbar's "Preview language"
picker). `editor.a11y_action`'s `name` uses the same names. Permission `read`; it changes nothing. A `keep_on` parameter is
refused with `keep_on_moved` (it is `editor.a11y_keep_tree` now).

```json
{
  "adapter": "atspi", "adapter_error": null, "active": false,
  "keep_on": true, "tree_on": true, "frames_sent": 12, "actions_injected": 0,
  "actions_queued": 0, "invalid_updates": 0,
  "nodes": [
    { "id": 4410, "parent": 4402, "role": "Button", "name": "Remove Beacon", "value": null,
      "numeric_value": null, "toggled": null, "disabled": false, "focused": false,
      "actions": ["click", "focus"] }
  ]
}
```

`adapter` is null and `adapter_error` says why when no OS adapter is attached (a headless
editor, `REGIA_ACCESSKIT=0`); the tree still works. `active` is true while a reader is
connected. `invalid_updates` counts frames whose tree the adapter's check refused (always 0
unless egui sends a broken tree).

### `editor.a11y_action`

```json
{ "method": "editor.a11y_action", "params": { "action": "click", "name": "Beacon", "role": "Button" } }
{ "method": "editor.a11y_action", "params": { "action": "set_value", "node": 4415, "value": 2.5 } }
```

Queues an action request for a node, exactly as the OS adapter does when a reader asks; the
next frame's widget acts on it (read the result with `editor.a11y_tree` or the method of the
thing it changed). Name the node by `node` (an id from `editor.a11y_tree`) or by `name` (plus
`role` when two nodes share a name). `action` is a snake_case AccessKit action (`click`,
`focus`, `blur`, `set_value`, `increment`, `decrement`, `scroll_into_view`, `expand`,
`collapse`, ...); `value` is `set_value`'s number (a drag value or slider) or string.

Permissions `review` and `build` (ADR-7795): a click can press any editor control, the Agent
Changes panel's **Accept** (what `review.accept` needs `review` for: an agent must not approve
its own proposal) and the package, launch and plugin controls (`build`) included. Neither is
in the default profile and a tool's scope never holds `review`, so an agent drives the editor's
UI this way only when the project grants both in `.regia/agent.toml`
(`permissions = ["read", "edit_scene", "assets", "play", "review", "build"]`); otherwise the
call answers `F0012` (permission denied) before any node is looked up. A person's screen reader
is not affected: its requests come through the OS adapter, not the agent protocol. Answer `{ "queued": true, "node": 4410, "action":
"click" }`.

| Code | When |
|---|---|
| `F0012` | the profile lacks `review` or `build` (permission denied) |
| `no_tree` | the editor sent no tree yet: call `editor.a11y_keep_tree { on: true }` and wait a frame |
| `node_not_found` | no node with that id, or no node with that name (and role) |
| `ambiguous_name` | several nodes have that name: add `role` or give `node` |
| `missing_target` | neither `node` nor `name` |
| `unknown_action` | `action` is missing or not an AccessKit action name |
| `invalid_value` | `value` is neither a number nor a string |
| `keep_on_moved` | (`editor.a11y_tree`) a `keep_on` parameter: call `editor.a11y_keep_tree` |

### Tests (screen reader)

```sh
cargo test -p regia_editor --lib a11y_bridge     # the shell's frame through the bridge, reader actions, names, the methods
cargo test -p regia_platform --lib accesskit_waker   # the bridge wakes the host for a request
```

`a11y_bridge_reader_operates_the_inspector_and_content_browser` draws the whole shell over a
project, takes the tree the bridge would send, checks the named roles in the Inspector and
Content panes, then pushes reader clicks into the bridge (open the `Beacon` section, tick its
checkbox, press **Remove Beacon**) and sees each op run. `a11y_bridge_toolbars_and_outliner_rows_have_spoken_names`
walks the play toolbar, the Outliner and the viewport toolbar and fails on an operable node
whose name has no letter or digit (its allow list, `UNSPOKEN_ALLOWED`, is empty).
`a11y_bridge_default_profile_agent_cannot_press_accept` sends `editor.a11y_action` through the
real dispatcher: refused on the default profile and on `review` alone, accepted with `review`
plus `build`. `a11y_bridge_read_only_client_cannot_turn_the_tree_on` checks that
`editor.a11y_tree` changes nothing and that `editor.a11y_keep_tree` needs `edit_scene`.
`a11y_bridge_feature_adds_no_system_to_platform_apps` fails if the editor's feature ever turns
on the runtime's `accesskit` (whose adapter system would join every `PlatformPlugin` app).

### Real-window checks

An agent cannot listen to speech, so the OS trees are checked instead. A walker queries the OS accessibility
API from outside the editor process, reads what a reader would read, and compares it with the editor's own tree
(`editor.a11y_tree`). It then presses one button and sets one value through the OS API and reads the result over
the agent protocol. The scripts and every table are in `docs/evidence/editor-a11y-os-trees-2026-11.md` (lane
v06-a11y-os-tree-pass, ADR-8587).

| OS | Where | Walker | Press | Value set | Result |
|---|---|---|---|---|---|
| Windows 11 (UI Automation) | the min-spec Windows laptop, console session, scheduled task | `uia-walk.ps1` (`System.Windows.Automation`, no install) | **▶ Play** through `InvokePattern`: `editor.play_state` `stopped` → `playing` | the Inspector's first spin button through `RangeValuePattern.SetValue(2.5)`: translation x 0 → 2.5 | pass: the window, the play toolbar's buttons, the Inspector pane and its fields have the editor's roles and names (162 of 162 named nodes after the fix) |
| Linux (AT-SPI) | the build server under Xvfb, private session bus (`dbus-run-session`, the system's `at-spi-bus-launcher`), no desktop and no Orca | `atspi-walk.py` (raw D-Bus through Gio, no pyatspi) | **▶ Play** through `Action.DoAction(0)` ("click"): `stopped` → `playing` | the same spin button through `Value.CurrentValue = 2.5`: translation x 0 → 2.5 | pass: the same nodes with the editor's roles and names (160 of 160 named nodes after the fix) |
| macOS (NSAccessibility) | `the test Mac` | `ax-walk.swift` (AXUIElement) | not run | not run | not run: the Mac did not answer ssh during the lane; the terminal also needs Accessibility (System Settings ▸ Privacy & Security ▸ Accessibility) |

The walks found that a `Label` node speaks its value and never a label set on it. All three adapters follow
that rule. `editor.a11y_tree` and `name_node` now follow it too (see `editor.a11y_tree` above): a `Label`
answers its text as `name`, and a field named by a label beside it answers that label's text.

Still open, from the walks: egui_dock's tab title and close areas are unnamed `Unknown` nodes with click and
focus (about 146 in the default layout); FU-11 (the Transform's unnamed drag values) shows on the OS as unnamed
`Spinner` elements. Speech itself (a reader announcing the names) still needs a person and a reader. Run the steps
below on a desktop session with the editor built with default features, a project open, and an entity
selected. Record pass or fail per step in the evidence file.

- **Linux, Orca** (a GNOME or KDE session on the build server's or the test machine's desktop): enable the
  screen reader (Settings ▸ Accessibility ▸ Screen Reader, or `orca --replace &`), confirm the
  bus is on (`gsettings get org.gnome.desktop.a11y.applications screen-reader-enabled` is
  `true`), launch `regia-editor <project>`; the log shows `screen-reader bridge: atspi adapter
  attached to the editor window`. Orca reads "Regia Engine Editor". Press Tab through the play
  toolbar: Orca speaks "Play toolbar", then "▶ Play, button", "Simulate, button", .... Tab into
  the Inspector: it speaks "Inspector" and each field's name and value. Focus the Transform's
  first drag value and press Up arrow or type a number and Enter: Orca speaks the new value and
  the viewport moves. In the Outliner, Tab to the eye: Orca speaks "Hide <entity> in the
  viewport, button"; Enter hides it.
- **Windows, NVDA** (the min-spec Windows laptop, `<build-dir>`): start NVDA, launch `regia-editor.exe
  <project>`; the log shows the `uia` adapter attached and the window opens normally (it is
  created hidden and shown after the adapter subclasses it). NVDA reads the window title, then
  the same Tab walk, Inspector read and value change as above. Also check Narrator reads the
  toolbar buttons.
- **macOS, VoiceOver** (`the test Mac`, a logged-in console session): Cmd+F5 turns VoiceOver on;
  launch the editor; the log shows `ns_accessibility`; VO+Right walks the toolbar, VO+Space
  presses a button, the Inspector read and the value change as above.

## Clipped text and text scale (ADR-7838)

`a11y_audit::clipped_text(&output.shapes, tolerance)` lists the text runs of a frame that extend past their clip
rectangle (cut off on screen), with their bounds and clip. Text scrolled fully out of view is not counted. With
`appearance.text_scale` at 2.0 the editor's default layout at 1920×1080 must show none (the test also skips a tab
title cut only at the side edge of a dock tab strip, which scrolls to it):

```sh
cargo test -p regia_editor --test theme main_panels_at_text_scale_two_clip_no_text
cargo test -p regia_editor --test theme the_high_contrast_theme_meets_aaa_in_the_audit
cargo test -p regia_editor --lib theme_tab_tests      # the Theme page: Tab to the accent's hex field, every widget named
cargo test -p regia_editor --lib chrome_color_vision  # the chrome filter and editor.color_vision
```

The rules a panel author follows are in [Accessibility rules for editor panels](../user-guide/editor-guide/accessibility-rules.md).

## Widget states, the light theme, glyphs and tool windows (ADR-8589)

The fixes for bugs 5, 7, 8 and 15 of the 2026-11-16 all-host editor smoke run add no method; an agent
sees them through methods that exist:

- **UI scale.** `editor.prefs_set { "values": { "appearance.ui_scale": 2.0 } }` now doubles the
  interface on every host: egui's pixels per point is the window's scale factor times the preference
  (`editor_prefs::frame_screen`), and `editor.layout_get` reports it as `pixels_per_point`. The smoke
  harness's `scale200:*` steps fail a crop that is pixel-identical to its 100 % crop.
- **Glyphs.** The editor installs its own egui font table at the first theme apply: egui's defaults,
  then egui's bundled monospace face (arrows, triangles, `●`) and the runtime UI's embedded Inter and
  script subsets as fallbacks in every family (`theme::font_definitions`). A test walks every string
  literal of the editor's non-test sources and fails on a character without a glyph.
- **Light theme.** `regia-dark` and `regia-light` map widget states and accents from their roles
  (`ThemeTokens::from_ui_roles`). `a11y_audit::widget_contrast(&tokens)` measures every egui widget
  state (text on the panel and on its own fills, the boundary of an interactive state), weak text,
  warning, error and link colors, every text accent and the meter bars over their track; both themes
  pass every row. `editor.a11y_contrast { "theme": "regia-light" }` answers `failing: []`.
- **Tool windows.** The Frame Debugger, Rendering Debug, Diagnostics, Profiler, Asset Jobs, Revision
  Control, Screenshot and About windows open in a cascade inside the dock area, in the order they
  first open (`tool_windows::default_pos`); a moved window keeps its place (egui's window memory).
  The commands that open them (`view.toggle_profiler`, `file.screenshot`, `editor.about`, ...) are
  unchanged.

```sh
cargo test -p regia_editor --lib ui_scale_200_doubles_a_label_on_screen_at_every_native_scale
cargo test -p regia_editor --lib every_character_the_editor_writes_has_a_glyph
cargo test -p regia_editor --lib every_widget_state_and_slot_of_the_ui_themes_meets_aa
cargo test -p regia_editor --lib widget_states_and_accents_map_from_the_roles
cargo test -p regia_editor --lib the_eight_tool_windows_open_apart_and_below_the_toolbars
cargo xtask editor-smoke --only 'scale200:*'
cargo xtask editor-smoke --only 'light:*'
```
