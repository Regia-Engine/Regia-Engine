# Input (agents)

Spec 11 §§ 6–7. This page is the map of everything input-related an agent can drive or read. The details are on
the linked pages; the game-side guide is [Input](../user-guide/input.md).

## How input flows

The platform layer produces raw device messages (keyboard, mouse, touch, back button, and gamepad messages from gilrs
on desktop or `android-activity` + `InputManager` on Android). `regia_input::InputPlugin` folds them into the
`Keyboard`, `Mouse`, `Touches` and `Gamepads` resources in `PreUpdate`, then evaluates the project's `InputConfig`
(contexts, bindings, modifiers, triggers) into per-player `Actions` once per fixed step. Gameplay reads only
`Actions` (`pressed`, `just_pressed`, `value`, `axis2d`). Axis convention: x = right, y = forward/up.

An agent can enter at two points of that pipeline, and both are deterministic and replayable:

| Entry | Method | What it feeds |
|---|---|---|
| Raw events | `play.inject_input` `events` | the same messages a real window sends, so bindings, modifiers, rebind capture and UI focus all see them |
| Action values | `play.inject_input` `actions` | `InputInjection`: the named action holds the value from `at_frame` for `duration_frames` frames, bypassing bindings |

## `play.inject_input`

Full reference: [Testing](testing.md#playinject_input). At least one of `events` and `actions` is required; the whole
batch is validated first, so a refused call injects nothing.

Raw event types and their fields (absent numbers read as 0):

| `type` | Fields |
|---|---|
| `key` | `key` (physical key name, e.g. `KeyW`), `text` (logical key, defaults to `key`), `state` (`pressed`, or `released`/`up`) |
| `mouse_button` | `button` (`left`, `right`, `middle`, `back`, `forward`), `state` |
| `cursor` | `x`, `y` (window pixels, y down) |
| `wheel` | `dx`, `dy` (lines) |
| `touch` | `id`, `phase` (`started`, `moved`, `ended`, `cancelled`), `x`, `y` |
| `back` | none (sends `BackPressed`, the Android back button or gesture; unless `ClosePolicy::back_closes_primary` is off it closes the primary window) |

There is no raw gamepad event: drive gamepad-bound gameplay through `actions`. Action values are a number (x), an
array `[x, y]` / `[x, y, z]` or an object `{x, y, z}`. `at_frame` defaults to the current frame and is capped at
10,000,000 frames ahead; `duration_frames` defaults to 1 and is capped at 1,000,000.

```json
play.inject_input {"events": [{"type": "touch", "id": 0, "phase": "started", "x": 120, "y": 640}], "actions": [{"action": "Move", "value": [0, 1], "duration_frames": 30}]}
```

Errors: `F0003` when raw events are sent before the primary window exists (wait for lifecycle `Started`), `F0013`
when `actions` are sent to an app without `InputPlugin`, `F0002` for an unknown event `type`, a wrong-typed field or
a malformed action value.

## Reading and tuning input

| Need | Method | Page |
|---|---|---|
| Rebind an action, save and load profiles | `input.rebind`, `input.rebind_axis`, `input.rebind_state`, `input.rebind_save`, `input.rebind_load`, `input.rebind_profiles`, `input.rebind_cancel`, `input.rebind_reset` | [Input rebinding](input-rebind.md) |
| Hold-or-toggle, repeat and cool-down assists; look sensitivity | `input.assists` | [Input assists](input-assists.md) |
| Rumble and phone vibration intensity | `input.haptics` | [Input assists](input-assists.md#rumble-intensity) |
| On-screen touch zones, sticks and gestures | `input.touch_controls` | [Mobile lifecycle](mobile-lifecycle.md), [Android](../user-guide/android.md) |
| SpaceMouse (desktop 6-DoF) | `input.spacemouse_status`, `input.spacemouse_inject`, `input.spacemouse_nav_defaults` | [SpaceMouse](spacemouse.md) |
| UI focus, layers and keyboard navigation | `ui.*` | [UI input and layers](ui-input.md) |
| The prompt glyph for an action on a device | `input.glyph` | [Button-prompt glyphs](#button-prompt-glyphs) |
| What malformed input never does | — | [Hostile input](hostile-input.md) |

To check what gameplay did with an input, assert on its effect with `play.wait_until` / `play.expect` (a component
moved, a state changed), not on the action itself.

## Button-prompt glyphs

`input.glyph {action, device?}` answers the glyph a device family shows for an action (spec 11 § 7, ADR-7070).
`device` is `keyboard_mouse`, `xbox`, `playstation` or `steam_deck`; without it the current `HintDevice` answers.
The lookup tries the project's glyph table (`assets/input/project.glyphs.ron`, `docs/formats/input-glyphs-ron.md`), then the
built-in families of the UI hint actions (`activate`, `back`, ...), then the action's first binding the device can
press, named per family (`RT` on Xbox, `R2` on PlayStation). Read-only; permission `read`.

```json
{"method": "input.glyph", "params": {"action": "Fire", "device": "playstation"}}
→ {"action": "Fire", "device": "playstation", "glyph": "R2", "source": "binding", "table_rows": 0}
```

| Code | JSON-RPC | Meaning |
|---|---|---|
| `input_glyph_bad_params` | -32602 | Not `{ action, device? }`, `action` empty, or an unknown `device`. |
| `input_glyph_unknown` | -32001 | No table row, not a hint action, and no binding the device can press. |

Typed actions: `#[derive(InputAction)]` on a fieldless enum names one action per variant
(`#[action(name = "fire_primary", kind = Axis2D)]` renames it or changes its kind); `InputConfig::declare::<A>()`
declares them and `actions.pressed_action(A::Jump)` reads them. Agents still address actions by name.

UI routing (the gameplay pointer gate and the `Menu` context) is on [UI input and layers](ui-input.md).

## Game-side resources an agent cannot read directly

These are Rust API only; no method reports them:

- `LastUsedDevice` (`Keyboard`, `Mouse`, `Gamepad(id)`, `Touch`): which device the player used last, for choosing
  button prompts. It changes only when the class or pad changes.
- `Gamepads`: connected pads (`pads`, `connected`, `first`, `name`), per-pad buttons and axes, and rumble:
  `rumble(device, strength, duration)` queues a request (strength clamped to 0..=1, then scaled by the player's
  haptics setting from `set_rumble_scale`); at most `MAX_QUEUED_RUMBLE` (64) requests wait and the oldest are
  dropped first. `PhoneVibration` is the matching scale for phone vibration.
- On Android the D-pad of a gamepad arrives as arrow keys, not as pad buttons, so bind both when an
  action should follow the D-pad on every platform.

## Independent Hold bindings (v0.5.1)

Each physical binding has its own Hold timer and fired latch. Hold one key
past its threshold, then press and hold the second key bound to the same
action: the second binding fires when its own threshold is reached.
Releasing it does not rearm the first. For agent checks, send raw key events
through `play.inject_input` and advance fixed steps; action-value injection
represents one shared action value and does not distinguish physical bindings.
The latch advances only on fixed steps, like the binding's timer and Tap window.
