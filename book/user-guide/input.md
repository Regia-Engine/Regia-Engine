# Input

<!-- index: Input action -->
<!-- index: Input binding -->
<!-- index: Input rebinding -->
<!-- index: Input context -->
<!-- index: Cursor grab -->
<!-- index: Virtual stick -->
<!-- index: Touch controls -->

`regia_input` turns platform input into two things you use: **messages** (raw events) and
**actions** (named, mapped inputs). Actions are the portable form: the same gameplay code runs on
keyboard and mouse, gamepads and touch, and the bindings can be changed by the player at runtime.
What an agent can inject and read is mapped on [Input (agents)](../agents/input.md).

## Raw messages

Keyboard, mouse, touch, IME and back-button events arrive as messages from `PlatformPlugin`:

```rust
use regia::prelude::*;

fn on_key(mut keys: MessageReader<KeyboardInput>) {
    for e in keys.read() {
        if e.state == ButtonState::Pressed {
            // e.physical_key / e.logical_key
        }
    }
}
```

Raw events are platform-shaped; use them for text entry and debug keys, and actions for gameplay.

## Defining actions

An action has a name and a kind (`Button`, `Axis1D`, `Axis2D` or `Axis3D`). Bindings map a source
(`InputSource`: a key, a 2D key set, mouse motion or a button, a gamepad button, stick or axis) to
an action. Build the configuration in code and hand it to `InputPlugin`:

```rust
use regia::prelude::*;
use regia_input::prelude::*;

fn input_config() -> InputConfig {
    InputConfig::new()
        .action("Move", ActionKind::Axis2D)
        .action("Look", ActionKind::Axis2D)
        .action("Jump", ActionKind::Button)
        .action("Fire", ActionKind::Button)
        .binding(Binding::new("Move", InputSource::keys2d("KeyW", "KeyS", "KeyA", "KeyD")))
        .binding(Binding::new("Move", InputSource::gamepad_stick(Stick::Left)))
        .binding(Binding::new("Look", InputSource::MouseDelta))
        .binding(Binding::new("Look", InputSource::gamepad_stick(Stick::Right)))
        .binding(Binding::new("Jump", InputSource::key("Space")))
        .binding(Binding::new(
            "Jump",
            InputSource::gamepad_button(regia::platform::GamepadButton::South),
        ))
        .binding(Binding::new("Fire", InputSource::mouse_button(MouseButton::Left)))
}

pub struct Game;

impl Plugin for Game {
    fn build(&self, app: &mut App) {
        app.add_plugins(InputPlugin::new(input_config()));
    }
}
```

Keys are named by their physical position (`"KeyW"`, `"Space"`, `"ShiftLeft"`), so WASD stays
WASD on an AZERTY keyboard. The configuration also round-trips through RON
(`InputConfig::from_ron` / `to_ron`) if you prefer to keep it in a file under `config/`.

## Reading actions

```rust
use regia::prelude::*;
use regia_input::prelude::*;

fn drive_player(actions: Res<Actions>) {
    let move_axis = actions.axis2d("Move"); // Vec2, x right, y forward
    let jumping = actions.pressed("Jump"); // held this frame
    let jumped = actions.just_pressed("Jump"); // went down this frame
    let _ = (move_axis, jumping, jumped);
}
```

`value(name)` returns the raw `Vec3`, `axis1d` a scalar, `axis2d` a `Vec2`; `phase(name)` and
`triggered(name)` report the trigger state (below); `elapsed(name)` how long it has been held.
`actions.player(n)` reads local player `n` in split-screen: each player's devices are assigned
through `DeviceAssignment` in the `PlayerInputs` resource.

Actions are evaluated once per **fixed step** in `FixedUpdate` (and once per frame with a zero
delta when the frame runs no fixed step), so a `FixedUpdate` system reading actions orders itself
`after(InputSet::Evaluation)`. `just_pressed` is the per-step edge (use it in simulation); a
system that runs once per frame (`Update`, quicksave, agent polling) uses
`just_pressed_this_frame`, which stays true for the whole frame even when a multi-step frame
consumed the step-level edge on its first step. Mouse motion (`InputSource::MouseDelta`) is per
frame: a frame that runs several fixed steps applies it once, on its last step, so a `FixedUpdate`
reader sees it once and an `Update` mouse look sees the whole frame's motion.

## Modifiers and triggers

A binding can process its value before the action sees it. Triggers run per binding: each binding's
trigger list runs on that binding's own value, and an empty list is `Down`. The action takes the
strongest phase of its bindings, so a `Pressed` on one binding and an empty list on a sibling means
holding the sibling holds the action, and a press of the `Pressed` key still fires while the sibling is
down; an action that was already active reports `Ongoing`, not a second `Started`. A `Hold` or `Tap` on
one binding sees only that binding's key and times that key's own press. The action's value combines its
bindings: a button keeps the last non-zero value and axes add, and a zero value contributes nothing. An
injected value, or an assist that changes whether the action is down, is one value for the whole action:
every binding's triggers then see that value.

| Modifier | Effect |
|---|---|
| `DeadZone(DeadZone::radial(lo, hi))` / `axial(lo, hi)` | Ignore stick noise, rescale the rest |
| `Scale(v)`, `Negate { .. }`, `Swizzle(..)`, `InvertY(bool)` | Shape and remap axes |
| `Smooth(t)` | Exponential smoothing |
| `ResponseCurve(..)` | `Linear`, `Square`, `Cubic`, `SquareRoot`, `SmoothStep` |
| `ScaleByDelta` | Multiply by the frame delta |

| Trigger | Fires |
|---|---|
| `Down` (default) | while actuated |
| `Pressed` / `Released` | on the frame it goes down / up |
| `Hold(secs)` | once held this long |
| `Tap(secs)` | when released within this time |
| `Pulse(secs)` | immediately, then repeatedly while held |
| `Chord(action)` | while the named action's previous-frame `actuated` flag is set (`Started`, `Ongoing` or `Triggered`, and non-zero). A `Pressed` partner only chords on the following frame. Losing the partner ends as `Idle`, not `Completed`. Bind the partner with the default `Down` |

Use them with `Binding::new(..).with_modifier(..).with_trigger(..)`.

## Contexts

Group bindings into **contexts** (`InputConfig::context(name, InputContext::consuming(priority))` and
`binding_in(context, binding)`), then enable and disable them at runtime through the
`InputContexts` resource: a vehicle context while driving, a menu context while paused. Higher
priority contexts evaluate first; a context built with `InputContext::consuming` also hides its
actuated inputs from lower ones (so it wins a shared input), while `InputContext::new` lets both fire.

## Rebinding

`InputRebinder` listens for the next raw input and turns it into a new binding for an action
(the classic "press a key for Jump" screen), detecting conflicts. The result is stored in
`InputOverrides`, merged over the project configuration at startup and saved to
`user://input_overrides.ron` immediately after every change, so a rebind survives restarts.

A capture can be limited to one device (`RebindSlot::KeyboardMouse`, `RebindSlot::Gamepad`), which
keeps the bindings on the other devices. `set_axis_tuning` sets a dead zone and invert on an action's
axes, and `save_profile` and `load_profile` keep named copies in `user://input_profiles/`.
`regia_ui::RebindPanel` with `RebindPanelOptions` is the screen for all of this; the First Person
template has one working in `game/src/controls.rs` (F10). See
[the agent guide page](../agents/input-rebind.md).

## Touch and Android

Touch arrives as `TouchInput` messages, and the `Touches` state tracks every active touch point
(`centroid`, per-touch displacement) for gestures and virtual sticks; `samples/sim_bridge` has
on-screen touch controls. Without UI widgets, the `TouchControls` resource maps screen zones (a
button or a floating stick) and recognized gestures to actions through `InputInjection`, so they
run the same triggers as a key; agents read it with `input.touch_controls` (see
[Android](android.md)). On Android, gamepads come through the activity's input events (there
is no `gilrs` on Android, so `regia_input` has a separate Android backend); the action layer
is identical. The Android back button arrives as `BackPressed`.

The `regia_ui` widgets `VirtualStick { action: "Move", .. }` and `VirtualButton { action: "Jump" }`
write to their actions with no glue code: while a finger is on the stick, `Move` holds the stick's
deflection (x right, y up), and while the button is pressed, `Jump` holds `1.0`. Both go through
`InputInjection::hold`, so triggers and phases work as they do for a key, and lifting the finger
hands the action back to its device bindings. While a finger rests on the stick, the stick wins
over the keyboard for that action. `TouchControls` (screen zones and gestures without widgets) uses
the same path.

## Cursor grab

A look-driven game writes `cursor_grab` on the `Window` component: `CursorGrab::Locked` while
playing, `CursorGrab::None` while a menu is open, `CursorGrab::Confined` when the cursor should
stay inside the window and remain visible. Apply the change on the main thread. A system that
calls the OS setter from a worker pool deadlocks the frame on Windows. Take `NonSendMarker` so
the system stays on the main thread. The same rule covers `set_title` and `request_inner_size`.

winit 0.30 does not implement every grab on every backend. Regia rewrites the request before it
is sent, and logs the rewrite once per process:

| Request | Windows | X11 and Xwayland | Wayland | macOS |
|---|---|---|---|---|
| `Locked` | requested as `Confined` | requested as `Confined` | `Locked` | `Locked` |
| `Confined` | `Confined` | `Confined` | `Confined` | requested as `Locked` |
| `None` | `None` | `None` | `None` | `None` |

A grab the OS still rejects is retried on a later frame. The component keeps the mode you asked
for. The fallback is only the mode sent to the window. Tell players who read a settings label
that "locked" on Windows and X11 confines the cursor to the window. On macOS, "confined" hides
and locks it, because that platform has no confine.

The behavior is [spec 11](../../docs/specs/11-platform-and-input.md) §§ 5–7, implemented in
`regia_platform::window_os`.

## Testing input

The agent protocol can inject raw events and scheduled action values (`play.inject_input`), and
`InputInjection` does the same from a test, so input-driven behavior runs deterministically in
CI. See the [agent guide's testing page](../agents/testing.md). Input specification:
[spec 11](../../docs/specs/11-platform-and-input.md) §§ 5–7.
