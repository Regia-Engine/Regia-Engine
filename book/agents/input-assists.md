# Input assists: hold or toggle, minimum hold, repeat, cool-down, rumble and phone vibration (agents)

ADR-2646, extended by ADR-3464. `regia_input::InputAssists` holds the player's per-action input assists. They are
player choices (accessibility settings), applied on top of the designer's bindings and triggers: the triggers stay
as authored and run on the assisted value. The engine-wide accessibility review is `docs/accessibility-review.md`.

## What an assist does

`ActionAssist { mode, min_hold_secs, repeat, cooldown_steps }`, by action name. The default set applies to every
player. A per-player map and a per-gamepad map overlay it (ADR-3464): the player's own entry wins, else the
assigned gamepad's entry, else the default set.

| Field | Effect |
|---|---|
| `mode: hold` | The action is active while its input is held (the authored behavior). |
| `mode: toggle` | Each counted press flips the action on or off. While on, the action reads the value it had on the press that turned it on (1 for a button), whether or not anything is held. Nothing needs to stay held. A press inside the cool-down does not flip it. |
| `min_hold_secs` | A press counts only while it is still down this long after the frame it started on (the start frame is time 0; each later frame adds its delta). Shorter presses never reach the action: a tremor or a bumped key does nothing. `0` to `5`. |
| `repeat: { delay_secs, interval_secs }` | While the action stays active it reports `Triggered` (so `just_pressed` is true) once after `delay_secs`, then every `interval_secs`. Holding replaces mashing. A frame longer than the interval fires once, never a burst. `delay_secs` `0` to `5`, `interval_secs` `0.05` to `5`. |
| `cooldown_steps` | After a press is accepted, a later press that *starts* inside the next `cooldown_steps` evaluations is ignored for that whole hold. A hold that was already accepted is not cut short. The gap is evaluation steps, not seconds (`0` to `600`; `600` is ten seconds at 60 Hz). `0` accepts every press. |

Order per frame: the bindings' combined device value (after modifiers), then the look scale when the action is
named in `InputConfig.look_actions` (see below), then injection (virtual touch controls and `play.inject_input` `actions`
replace the value outright, unscaled), then the action kind's clamp, then the minimum hold, the cool-down and
the mode, then the action's triggers, then the repeat. The default assist (hold, no minimum, no repeat, no
cool-down) changes nothing, and is not stored. Hold timing uses the evaluation delta only (one fixed step at a time, zero on a frame without a fixed step), and the cool-down counts
evaluations, so a recorded event stream gives the same phases every run.

A game that must stand the player up (a cutscene, a death) calls `Actions::release_toggle(player, name)` or
`Actions::release_all_toggles()`; the latch drops on the next evaluation. `ActionState::toggled_on()` tells
whether a toggle holds an action on.

## Persistence

`GameSettings.accessibility.input_assists` (action name to assist) is the default set. `input_assists_by_player`
and `input_assists_by_gamepad` are the overlays. All three are saved with the other settings and copied into
`InputAssists` when any of them changes (on the first frame only when a map is not empty, so a game's own
assists survive a player who never opened the menu). Older settings files load with empty maps and
`cooldown_steps` `0`.

## Method

- `input.assists { set?, clear?, player?, gamepad? }`. With no parameters it reports. `set` maps action names to an
  assist object (`{ mode?, min_hold_secs?, repeat?, cooldown_steps? }`, missing fields are the neutral value) or to
  `null` to remove that action's assist. `player` (0 to 255) writes that player's overlay; `gamepad` (a device id)
  writes that pad's overlay. Passing both is `input_assist_bad_params`. Omitting both writes the default set.
  `clear: true` removes the chosen set first (the default set when neither key is set; a player's or a pad's map
  when one is). Everything is checked before anything changes. A change writes `GameSettings` (persisted like
  `settings.set`) and the live resource.
  Returns `{ assists, players, gamepads, toggled_on, live, persisted }`. Each assist object is
  `{ mode, min_hold_secs, repeat, cooldown_steps }`. `toggled_on` lists the actions a toggle holds on for player 0.
  Mutating; permission `edit_scene`.

```json
{"method": "input.assists", "params": {}}
{"method": "input.assists", "params": {"set": {"Crouch": {"mode": "toggle", "cooldown_steps": 4}, "Fire": {"repeat": {"delay_secs": 0.4, "interval_secs": 0.1}}}}}
{"method": "input.assists", "params": {"player": 1, "set": {"Crouch": {"mode": "hold"}}}}
{"method": "input.assists", "params": {"set": {"Crouch": null}}}
```

## Rumble intensity

`Gamepads::set_rumble_scale(scale)` scales every later rumble request (`Gamepads::rumble` and `RumbleRequest`
messages) by `0` (off) to `1` (as the game asks, the default). Requests still queue at zero strength when it is off,
so a stop request still stops. `GameSettings.accessibility.rumble_scale` (missing in older files: `1`) persists it.

`PhoneVibration` is the same scale for the phone. A game calls `PhoneVibration::vibrate` (or reads
`scaled_amplitude` and calls `regia_platform::Haptics::vibrate` itself). `0` is off, `1` is the request as asked.
Reduced motion does not change rumble or phone vibration. `GameSettings.accessibility.vibration_scale` (missing in
older files: `1`) persists it. Desktop builds accept the call and do not move the device.

- `input.haptics { rumble_scale?, vibration_scale? }`. With no parameters it reports. A change writes
  `GameSettings` and the live `Gamepads` or `PhoneVibration`. Returns
  `{ rumble_scale, vibration_scale, live, persisted }`. Mutating; permission `edit_scene`.

```json
{"method": "input.haptics", "params": {"rumble_scale": 0.5, "vibration_scale": 0.25}}
```

## Look sensitivity

`GameSettings.input.sensitivity` and `invert_y` scale an action only when `config/input.ron` names it in
`look_actions` (builder: `InputConfig::look_action`). The scale runs on the combined device value, before
injection, so an injected value is not scaled. Mouse deltas and the right stick both go through it; stick axes
are clamped to `-1..=1` by the device update before the scale, so a stick at `1` with sensitivity `2` reads `2`.
An empty `look_actions` (the default, and older input files) leaves both settings for the game to read. A
non-finite sensitivity reads as `1`; values above `100` clamp to `100`.

## Errors

| Code | Meaning |
|---|---|
| `input_assist_bad_params` | The parameters are not `{ set?, clear?, player?, gamepad? }`, both `player` and `gamepad` are set, an assist has an unknown field, or a value has the wrong type |
| `input_assist_bad_value` | Unknown `mode`, or `min_hold_secs`, `delay_secs`, `interval_secs` or `cooldown_steps` out of range (`cooldown_steps` is 0 to 600) |
| `input_assist_unknown_action` | An input config is loaded and does not declare the action |
| `input_haptics_bad_params` | `input.haptics` parameters are not `{ rumble_scale?, vibration_scale? }`, or a scale is not a number |
| `input_haptics_bad_scale` | `rumble_scale` or `vibration_scale` is outside 0 to 1 |

## Limits

- A player overlay and a gamepad overlay are separate maps. One call writes one of them, or the default set.
- The cool-down counts evaluations of the input systems, not seconds and not rendered frames.
