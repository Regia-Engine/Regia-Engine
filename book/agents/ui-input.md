# UI input events and layers (agents)

Guide topic: `agent.guide {"topic": "ui_input"}`. ADR-273 lane 4, ADR-293.

## Events

Every `UiEvent` carries `button` (`primary`, `secondary`, `middle`), `modifiers` (shift, ctrl, alt, meta, tracked from
keyboard messages) and `click_count`. New kinds:

| Kind | When |
|---|---|
| `DoubleClick` | The second press-release on the same node within 0.4 s and 8 px. Sent after the second `Click`, which keeps its old meaning (`click_count` 2). |
| `ContextClick` | Secondary button released over the node it went down on. No `Click` is sent for it. |
| `MiddleClick` | The same for the middle button. |
| `Scroll([dx, dy])` | Wheel or trackpad over a node (physical px). Scroll views still consume the wheel as before. |
| `LongPress` | A node with `LongPressable { delay }` held for `delay` seconds; once per press. |
| `Repeat(n)` | A node with `RepeatOnHold { delay, interval }` still held; `n` counts from 1. |

When a long press or repeat fired, the release sends no `Click`. Click, long-press and key-repeat
timers step on `RealTime`. Pause and `time_scale` change `Time` only, so they do not freeze or stretch
these timers. A manual `play.step` still moves them: that step is applied to `RealTime` before `Time`
scales it. `UiInputConfig` holds the double-click window and slop, in seconds of `RealTime`.

## Bubbling and scripts (ADR-7070)

A `UiEvent` is about its `target` only. After the UI passes, `bubble_ui_events` writes a `UiBubbledEvent { current,
event, depth }` for each ancestor of the target, nearest first (`depth` 1 is the parent), for every kind except
`PointerEnter` and `PointerLeave`. The walk stops after a node that carries `UiStopBubble`. A panel reads the copies
whose `current` is itself to handle its children's clicks in one place.

Scripts (`regia_framework::script`) receive `ScriptEvent::Ui { entity, origin, event }`: on the target's scripts with
`entity == origin`, then on each ancestor's scripts with `entity` = that ancestor. `event` is a `ScriptUiEvent`
(`Click { click_count }`, `ValueChanged(ScriptUiValue)`, `FocusChanged(bool)`, ...); text payloads stay on the widget.
Delivery can trail the UI pass by one frame.

## Gameplay gating (ADR-7070)

While the UI holds the pointer (`UiPointer.consumed_pointer`: over an interactable node, or pressing one),
`regia_input::UiPointerGate.ui_consumed` is set and mouse-button bindings of every input context except the exempt
ones (`Menu` by default) read as released. A click on a button does not also fire the gameplay `Fire` action; the
same click on empty space does. The flag follows the interaction pass, so evaluation sees the previous frame's
hover. Keys, mouse motion and gamepads are not gated. While pointer claims are on (the default with `UiPlugin`,
ADR-8786), the flag is set only while a press that started on a node lasts, not on hover: a press that started on the
world stays the game's when it moves over UI, and the first press after the pointer leaves UI is the game's on its
own frame. With claims off (`input.pointer_claims {enabled: false}`) the hover reading above applies.

To check it: hover a button with `play.inject_input` `cursor`, press `mouse_button` `left`, and `play.expect` that the
gameplay effect did not happen; repeat over empty space.

## Navigation from the `Menu` input context (ADR-7070)

When the project's `InputConfig` declares the `Menu` context (`InputConfig::with_menu_context()` adds the default
bindings) and player 0 has it enabled (`InputContexts::push(0, "Menu")`), focus navigation reads its actions
`Up`, `Down`, `Left`, `Right`, `Next`, `Prev`, `Submit`, `Cancel` and the `Navigate` stick, and the raw arrow-key and
gamepad paths stand down. Rebinding a `Menu` action with `input.rebind {context: "Menu", action: "Up"}` therefore
changes how the UI moves. Held directions repeat with `GamepadNavConfig`; `Next` with Shift held is `Prev`; while a
text input has focus, left/right and `Submit` belong to the editor. Without the context enabled, nothing changes.

## Script-field bindings (ADR-7070)

`Bind::script_field("Hud.score", Some("Score: {:03}"))` (`BindSource::ScriptField`) shows the field `score` of the
script `Hud` on the bound node or its nearest ancestor that carries one. The script exposes its fields by
implementing `Script::reflect_fields` (`Some(self)` on a `Reflect` script). The binding is one way and is read every
frame; the label changes only when the text does. An unknown script or field reports `binding_unresolved` in
`ui.inspect`'s binding `error`, and `source_kind` is `script_field`. `.ui.ron` layouts cannot name a script field yet.

## Runtime settings

`Regia.toml` may carry a `[runtime]` table (`docs/formats/regia-toml-runtime.md`): `pause_on_focus_loss = true`
pauses gameplay `Time` while the window is out of focus, and `keep_screen_on = true` keeps an Android display awake.
`settings.get` (no `path`) reports the live values under `runtime`:

```json
{"method": "settings.get", "params": {}}
{"settings": {"...": "..."}, "runtime": {"pause_on_focus_loss": false, "keep_screen_on": false}}
```

## Input layers

A node with `InputLayer { mode }` is a layer root. Only the top layer's subtree is hit-tested and navigable; the rest
of the screen gets no hover, click or focus movement. An open `Modal` becomes a layer by itself (mode `ui_and_game`)
and stops being one when it closes.

| Mode | Meaning |
|---|---|
| `ui_only` | Gameplay should not act on input. Read `UiInputState::layer_mode`. |
| `ui_and_game` | The layer and the game both get input. Also the mode with no layer. |
| `game_only` | The UI ignores the pointer and navigation. |

Push focuses the layer's `DefaultFocus` (else its first focusable in reading order). Pop focuses what had focus before
the push and restores the previous input mode. `UiInputState::pointer_consumed` keeps its meaning.

## Methods

- `ui.layers { action?, entity?, mode? }`. `action` is `list` (default; the list result carries `applied: false`, so it is not written to the change log), `push` or `pop`. `push` needs `entity` (entity
  bits) and takes `mode` (default `ui_and_game`). `pop` takes the top layer, or `entity` to pop a specific layer (a
  modal is closed). Returns `{ layers: [{ entity, mode, previous_mode, restore_focus }], depth, mode, focused }`, bottom
  layer first, plus `popped` after a pop. Changes apply at once. Mutating; permission `edit_scene`.

```json
{"method": "ui.layers", "params": {}}
{"method": "ui.layers", "params": {"action": "push", "entity": 4294967301, "mode": "ui_only"}}
{"method": "ui.layers", "params": {"action": "pop"}}
```

## Errors

| Code | Meaning |
|---|---|
| `no_layers` | `pop` with an empty stack. |
| `not_a_layer` | `pop` of an entity that is not a layer root. |
| `already_layer` | `push` of a node that already is a layer. |
| `unknown_entity` | The entity does not exist. |

`ui.click { button?, count?, modifiers? }` synthesizes the same pointer fields (`button` is `left`, `right` or `middle`, `count` is 1 to 8 and
raises `click_count`, `modifiers` is a list of `shift`, `ctrl`, `alt` and `meta` sent as real key messages around the click,
so the click's `UiEvent::modifiers` reports them; ADR-5665).

`ui.layers` places its lower-case reason in `error.data.code`: `unknown_entity`, `not_a_layer` and `no_layers` use
JSON-RPC `-32001`, while `already_layer` uses `-32602`. `ui.click` takes the selectors in [Runtime UI](ui.md), plus
`button`, `count` and `modifiers` above. Its selectors are `node`, `name`, `text`, `text_contains`, `class`, `type` and `state`. It
can return `F0002` for an
invalid or refused node, `F0003` for no matching node, and `F0013` (`ui_not_installed`) without the runtime UI hook.

Workflow: query the modal or layer root, push it as `ui_only`, check `ui.layers` reports it at the top, then pop it
and check the prior mode and focused node were restored.

Captured failure on lavapipe:

Request:

```json
{"jsonrpc":"2.0","id":3,"method":"ui.layers","params":{"action":"pop","entity":0}}
```

Response:

```json
{"jsonrpc":"2.0","id":3,"error":{"code":-32602,"message":"`entity` must be entity bits from ui.layers list","data":{"code":"F0002","hint":"check the method's parameters in the agent guide"}}}
```


## Recursive disabled input

`Disabled` keeps its local-only meaning. Insert `Disabled::recursive()` (a `DisabledSubtree`
component) to disable pointer and keyboard/gamepad focus for a node and its current or future
descendants. Removing that marker restores inherited input without removing any descendant's
local `Disabled` marker. Reparenting takes effect immediately; no copied flags need repair.
`regia_ui::node::is_disabled` and `ui.inspect` / `ui.query` report the effective state.
This is a runtime API; its `.ui.ron` representation belongs to the format lane.

`ui.set_disabled {entity, disabled, recursive?}` requires `edit_scene`; `entity` is the numeric
entity bits from `ui.query`, `disabled` is required, and `recursive` defaults to false.
The call changes only the selected local or recursive flag and releases newly disabled focus.
The result is `{entity, disabled, local, recursive}`: `disabled` is effective, the other two
booleans are the node's own markers. Clearing a local flag cannot override a disabled ancestor.

```json
{"method":"ui.set_disabled","params":{"entity":4294967310,"disabled":true,"recursive":true}}
{"method":"ui.set_disabled","params":{"entity":4294967310,"disabled":false,"recursive":true}}
```

Errors: `bad_entity` (missing/invalid entity bits), `unknown_entity` (absent entity or not a UI
node), `bad_flag` (missing disabled boolean). Wrong parameter types may be refused by the
shared schema gate with `F0002`. The runtime function `set_disabled(world, entity, disabled,
recursive)` returns false for an absent entity.

## Pointer claims

Guide topic: `agent.guide {"topic": "ui-pointer-claims"}`. ADR-7976.

A mouse press whose press edge lands on a hit-testable UI node (the node `ui_interaction` would press: the top layer's
subtree only, nothing under a `game_only` layer) belongs to the UI until that button is released. While a button is
claimed, action evaluation reads it as up for every binding that names it, whatever the trigger (down, pressed,
released, hold, tap, pulse, chord). So a click on a loadout tile never fires the gun bound to the left button: not on
the press frame, not while held, not on the release.

| Case | Who gets the button |
|---|---|
| Press starts on a widget | The UI, until the release, even if the pointer leaves the widget. |
| Press starts on empty screen | Gameplay, until the release, even if the pointer moves over a widget. |
| Press and release in one frame's batch on a widget | The UI, for that frame. |
| Another button pressed while a widget is pressed | The UI. |
| A `game_only` layer is on top | Gameplay (the UI takes no pointer input). |

The claim is written in `PreUpdate` (`regia_ui::interaction::claim_pointer_buttons`, after `update_pointer` and
`regia_input::InputSet::Devices`, before `InputSet::Evaluation`), so it applies in the press frame. It is on whenever
`UiPlugin` is installed. `regia_input::PointerClaims` holds the claimed buttons; `set_enabled(false)` turns claims off
and gameplay reads the mouse as it is. Claims are derived state: input recordings and replays do not store them.
Keyboard and gamepad are not claimed (focus navigation and input layers route them). Touch, the wheel, mouse motion and
gameplay systems that read `regia_input::Mouse` directly are not gated.

### `input.pointer_claims`

`input.pointer_claims { enabled?, watch?, expect_untriggered? }`. With no parameters it only reports. Order: `enabled`
changes, then `watch` arms, then `expect_untriggered` checks. Mutating; permission `edit_scene`.

- `enabled` (boolean): turn claims on or off.
- `watch` (list of declared action names): arm a fresh watch. From then on, each frame (in `Last`) counts, per action,
  the frames gameplay saw it: any phase but idle, a press edge, or held. Replaces any earlier watch.
- `expect_untriggered` (list of watched action names): add `check { passed, triggered: { <action>: frames } }`;
  `passed` is true when every named action stayed at 0 frames.

Returns `{ enabled, ui_claimed_buttons, claimed_presses, watch: { frames, actions: { <action>: { triggered_frames } } }
| null, check? }`. Button names are `left`, `right`, `middle`, `back`, `forward` and `other_<code>`, in that order.
`claimed_presses` counts presses claimed since start-up.

Workflow (a click on a node must not fire `Fire`):

```json
{"method": "input.pointer_claims", "params": {"watch": ["Fire"]}}
{"method": "ui.click", "params": {"name": "loadout_tile"}}
{"method": "play.step", "params": {"frames": 4}}
{"method": "input.pointer_claims", "params": {"expect_untriggered": ["Fire"]}}
```

The last answer has `check.passed: true` and `claimed_presses` one higher. A press on empty screen in the watch window
makes `check.passed` false with `triggered.Fire` above 0.

| Code | Meaning |
|---|---|
| `pointer_claims_bad_params` | Parameters are not `{ enabled?, watch?, expect_untriggered? }`, or a list is empty. JSON-RPC `-32602`. |
| `pointer_claims_not_installed` | No `PointerClaims` resource: the app has no `InputPlugin`. JSON-RPC `-32001`. |
| `pointer_claims_not_watched` | `expect_untriggered` names an action with no armed watch. JSON-RPC `-32602`. |
| `pointer_claims_unknown_action` | `watch` names an action the input config does not declare. JSON-RPC `-32602`. |

## Host navigation keys

`UiNavActions` (`regia_ui::focus`) is the one input the focus pass reads. The built-in keyboard path maps the arrows,
Tab, Shift-Tab, Enter, Space and Escape, and `ui_gamepad_navigation` maps the gamepad. A host adds its own keys, such as
h/j/k/l, with a system ordered before `regia_ui::focus::ui_focus_navigation` that sets the flags. The flags are
cleared after each focus pass. Skip the map while a text field has focus so typing stays text:

<!-- ignore-reason: a host system fragment; its imports and the app that registers it are the host's -->
```rust,ignore
fn vi_nav_keys(
    mut keys: MessageReader<regia_platform::KeyboardInput>,
    mut nav: ResMut<UiNavActions>,
    focus: Res<FocusState>,
    text_inputs: Query<(), With<TextInput>>,
) {
    let typing = focus.focused.is_some_and(|f| text_inputs.contains(f));
    for e in keys.read() {
        if e.state != ButtonState::Pressed || typing {
            continue;
        }
        match e.logical_key.as_str() {
            "h" => nav.left = true,
            "j" => nav.down = true,
            "k" => nav.up = true,
            "l" => nav.right = true,
            _ => {}
        }
    }
}

app.add_systems(Update, vi_nav_keys.before(regia_ui::focus::ui_focus_navigation));
```

The arrows and Enter keep working beside it. Reading the `Menu` input context for navigation is a later item. Tested by
`crates/regia_ui/tests/nav_keymap.rs`.
