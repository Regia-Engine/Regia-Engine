# Input rebinding (agents)

Guide topic: `agent.guide {"topic": "input_rebind"}`. ADR-1801, ADR-2129 (v0.4.0), spec 11 § 7.

The player rebinds actions at run time. `regia_input` captures the next input, merges it over the project config and
saves it to `user://input_overrides.ron`; `regia_ui::RebindPanel` is the widget that shows it; the methods below drive
the same code for an agent. The First Person template has a working controls screen (`game/src/controls.rs`, F10).

## Capture modes

| Mode | Fits | Captures | Result |
|---|---|---|---|
| `single` | any action | one key, mouse button, pad button, stick or pad axis (or mouse motion, see below) | one `Binding` on that input |
| `keys1d` | one-axis action | two keys: `negative`, `positive` | a `Keys2D` source with the vertical keys unset |
| `keys2d` | two- or three-axis action | four keys: `up`, `down`, `left`, `right` | a `Keys2D` source |
| `chord` | button | a `modifier`, then the main `key` | the main binding gets `Trigger::Chord("<action>.chord")`; the modifier is bound to the helper action `<action>.chord` (declared as a button) |

- A group or chord capture takes one press per step. An input that does not fit the step (a stick for a key group, a
  stick for a chord) or that was already captured in this capture is ignored and the capture keeps waiting.
- Keys and buttons count on their press edge only, and a stick held through the start does not count until it returns
  below the threshold, so the click on a "Rebind" button is never captured.
- The panel and the methods refuse a mode that does not fit the action's kind (`rebind_bad_mode`).
- **Conflicts** are other actions in the same context that read an overlapping key (or the same source) under the same
  chord. The rebind still applies; the outcome lists the conflicting actions.
- **Reset** restores the project config's bindings for the action (and removes its chord helper), removes the override
  and saves. Resetting all clears every override.
- At start-up an `input_overrides.ron` that does not parse (or has a newer `format_version`) is renamed to
  `input_overrides.ron.bad-<n>` (at most 5 kept), a warning names it, and the project bindings load; the next rebind
  saves normally (ADR-3102). A file that cannot be read or moved aside is never overwritten: the rebind applies for
  the session, `persist_blocked` is true and `persist_error` says why.

## Device slots

A capture has a `slot`. `all` (the default) listens to every device and replaces all of the action's bindings.
`keyboard_mouse` and `gamepad` listen to that device only and replace only that device's bindings, so rebinding a key
keeps the gamepad binding. `keys1d` and `keys2d` do not fit the gamepad slot. A chord keeps its modifier per slot
(`<action>.chord`, `<action>.keyboard_mouse.chord`, `<action>.gamepad.chord`).

What a capture makes of an axis:

- On a two- or three-axis action a stick binds the stick. On a one-axis or button action it binds the single axis that
  moved, and a stick's Y axis is moved to x, where a one-axis action reads it.
- On a two- or three-axis action in the `keyboard_mouse` slot, 200 units of mouse travel bind mouse motion. Other captures
  do not listen for motion.
- A stick or pad axis gets the dead zone of the binding it replaces, else 0.15. On a button action it gets 0.5, so the
  input presses at half travel and resting drift does not hold the button.
- The action's triggers carry over (a `Pressed` action stays `Pressed`; a chord belongs to the capture mode). Modifiers
  carry over only from a binding of the same shape, so a mouse sensitivity scale does not land on a stick.

## Axis tuning

`input.rebind_axis` sets the dead zone and invert of an action's axis bindings on a slot and saves them. Sticks, pad axes,
key groups and mouse motion can be inverted; only sticks and pad axes have a dead zone. Invert is the last modifier, so it
acts on the action's value; other modifiers (a curve, a scale) stay where they are. A reset restores the project's
bindings, tuning included.

## Profiles

The active set is `user://input_overrides.ron`, saved after every change. `input.rebind_save { profile }` keeps a named
copy in `user://input_profiles/<name>.ron`. `input.rebind_load { profile }` replaces the live set with it: the project
bindings come back, the profile merges over them, and it becomes the active set, so it survives a restart. A missing or
corrupt file changes nothing.

`input_overrides.ron` is at `format_version` 2. Version 1 (no version field) is a subset and reads as it stands; the next
save writes version 2. A file from a newer build is moved aside at start-up like a damaged one (above).

## Methods

- `input.rebind_state { context? }`. Read-only. Returns `{ pending, outcome, persist_error, persist_blocked, contexts }`.
  `pending` is `{ context, action, mode, slot, step, total, prompt, captured }` or null. `outcome` is the last completed
  rebind `{ context, action, input, conflicts, slot }` or null. `contexts` maps each context to its actions
  `[{ action, kind, binding, overridden, modes, slots, conflicts }]`; chord helpers are hidden. `slots` holds, for
  `keyboard_mouse` and `gamepad`, `{ binding, tuning: { dead_zone, invert_x, invert_y } | null, has_dead_zone }`.
  `conflicts` are the actions sharing an input with this one right now, whenever the binding was made.
- `input.rebind { context, action, mode?, slot? }`. Starts a capture (default `single`, `all`). Returns `{ context,
  action, mode, slot, steps }`. Feed the presses with `play.inject_input` (key events), one frame apart, with a release
  between presses. Mutating; permission `play`.
- `input.rebind_axis { context, action, slot?, dead_zone?, invert_x?, invert_y? }`. A field left out keeps its value;
  `dead_zone: null` removes the dead zone. Returns `{ context, action, slot, tuning }`. Mutating; permission `play`.
- `input.rebind_save { profile? }`. Writes a named profile, or the active file without one. Returns `{ profile, path }`.
  Mutating; permission `play`.
- `input.rebind_load { profile? }`. Loads a named profile, or reloads the active file without one. Returns `{ profile,
  overrides }`. Mutating; permission `play`.
- `input.rebind_profiles {}`. Read-only. Returns `{ profiles }`.
- `input.rebind_cancel {}`. Drops the capture. Returns `{ cancelled: true }`. Mutating; permission `play`.
- `input.rebind_reset { context?, action? }`. With `action` (and `context`) resets that action and returns `{ context,
  action, reset }` (`reset` is whether an override was removed); without it resets everything and returns `{ reset:
  <count of overridden actions> }`. Mutating; permission `play`.

```json
{"method": "input.rebind", "params": {"context": "Gameplay", "action": "Jump", "mode": "chord"}}
{"method": "play.inject_input", "params": {"events": [{"type": "key", "key": "ControlLeft", "text": "", "state": "pressed"}]}}
{"method": "input.rebind_state", "params": {"context": "Gameplay"}}
{"method": "input.rebind_axis", "params": {"context": "Gameplay", "action": "Look", "slot": "gamepad", "dead_zone": 0.2, "invert_y": true}}
{"method": "input.rebind_save", "params": {"profile": "left_hand"}}
```

## The widget

`regia_ui::RebindPanel { context, actions }` on a node builds one row per action (all bound actions of the context,
or the listed ones in order): a name, a binding button, an alt button (`Chord` for buttons, `Keys` for axes), a
`Reset` button and a status line, plus a `Reset all` button. Clicking the binding or alt button starts the capture
(the button shows `Press an input...` or `Press up (1/4) ...`); clicking it again cancels. The status line shows
`Also used by: ...` after a conflicting rebind and `Not saved: ...` when saving failed. Rows are named
`rebind.<context>.<action>.row`, `.bind`, `.alt`, `.reset`, `.status` and `rebind.<context>.reset_all`, so `ui.query`
and `ui.click` reach them. The labels are literal US English text today; a game that localizes them replaces the
`Text` of those nodes.

`RebindPanelOptions` on the same node, all off by default, adds:

- `per_device`: `.keyboard_mouse` and `.gamepad` capture buttons in place of `.bind`, in columns under a header row
  (`rebind.<context>.header`), the `.alt` button on the keyboard and mouse slot, and `.gamepad_alt` (a pad chord) for
  buttons.
- `axis_tuning`: under each axis action a row per slot, `.tuning.<slot>.row`, with the toggles `.invert_x` and `.invert_y`
  (a one-axis action has only `.invert_x`) and the slider `.dead_zone` where a stick or pad axis is bound. The controls
  follow a reset or a loaded profile.
- `profiles`: `rebind.<context>.profile.<name>.save` and `.load` per name, and the result in
  `rebind.<context>.profile_status`.

Two things a capture started from the panel adds. `Escape` and the pad's `Start` leave it instead of being captured, so a
player with one device can always back out (they cannot be bound from the panel). And the press a capture consumes does not
count as a click on the panel: its release would otherwise start the next capture (`InputRebinder::click_guard`).

While a capture runs, and until the press it took comes up, the UI's keyboard and gamepad navigation does nothing: the
`Escape` or pad `B` it takes does not also close a pause dialog around the panel, and a captured `Tab` or D-pad press does
not move focus off the row. Screen-reader actions (`ui.a11y_platform_action`, the OS reader) still work, so a reader user
can press the capturing button again to cancel. Each row is a `FocusGroup`: left and right stay on the row while it has a
control in that direction (the D-pad reaches an axis row's `.dead_zone` slider from its invert toggles), up and down leave it.

The panel is touch-sized: each row is at least `UiDensity::touch_min()` tall (44 UI px in the default `comfortable`
density, 28 in `compact`) and every button, toggle and slider carries a `TouchTarget` of that size, so a fingertip
hits a 44x24 toggle without the rows' hit boxes overlapping (ADR-4903).

Every control also carries an accessible description naming its action and device (`Move, gamepad`; `Reset` reads `Move`,
the profile buttons `Profile <name>`), so a screen reader tells the rows' `Invert Y`, `Dead zone` and `Reset` apart.

## Errors

The code is in `data.code`.

| Code | Meaning |
|---|---|
| `rebind_unknown_context` | The input config has no context of that name. |
| `rebind_unknown_action` | The context does not bind that action (chord helpers are not addressable). |
| `rebind_bad_mode` | The mode does not fit the action's kind. |
| `rebind_not_pending` | `input.rebind_cancel` with no capture pending. |
| `rebind_no_default` | `input.rebind_reset` for an action the project config does not bind. |
| `rebind_bad_tuning` | `input.rebind_axis`: the dead zone is outside 0 to 0.95 or not finite. |
| `rebind_not_tunable` | `input.rebind_axis`: the action has no stick, pad axis, key group or mouse motion on that slot. |
| `rebind_bad_profile` | The profile name is not 1 to 32 of `a-z`, `0-9`, `_`, `-`. |
| `rebind_profile_missing` | `input.rebind_load`: no such file, or no file system is mounted. |
| `rebind_profile_corrupt` | `input.rebind_load`: the file does not load; nothing changed. |
| `rebind_save_failed` | `input.rebind_save`: the write failed, or the active file failed to load and is being left untouched. |
| `input_not_installed` | The app has no `InputPlugin`. |

## The template's controls screen on a touch screen (ADR-4932)

The First Person template's `controls.rs` has `ControlsLayout`. In `Desktop` (the default on desktop), F10 opens and
closes the screen, scaled by the project's reference resolution. `Touch` is the Android default: a touch-sized
`controls.open` button stays top right, and the screen opens with a `controls.close` button in its header. The
bindings scroll inside `controls.scroll`. The screen's root is `ScaleMode::ConstantPhysicalSize` with
`SafeAreaPadding`, so the rows' 44 UI px touch boxes are 44 dp on a phone. While the screen is open, the system Back
closes it, not the game (`ClosePolicy::back_closes_primary` is off until one frame after it closes). An agent opens it
with `ui.click` on `controls.open` or with F10 through `play.inject_input`, and closes it with `ui.click` on
`controls.close`, F10, Escape, or `play.inject_input` `{"type": "back"}`.
