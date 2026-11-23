# UI touch gestures (agents)

ADR-273 I08, ADR-1842. Code: `regia_ui::gesture`.

`GestureRecognizer` is a pure state machine over touch samples `{ id, stage, pos, time }` (UI px, seconds). It never
reads a clock, so a recorded sequence always yields the same gestures.

| Gesture | When |
|---|---|
| `pinch` | two fingers whose distance has changed by at least `pinch_threshold` (default 12 px) and by at least as much as their midpoint has moved (a tie is a pinch); reports the cumulative `scale` since the pinch began and the `center` |
| `two_finger_scroll` | two fingers whose midpoint has moved at least `scroll_threshold` (10 px) and strictly more than their distance has changed; reports the `delta` of the midpoint per update |
| `swipe` | one finger that travels at least `swipe_distance` (80 px) within `swipe_time` (0.4 s) before it lifts, and never had a second finger; `direction` is the dominant axis (`left`, `right`, `up`, `down`) |

A third finger ends the two-finger gesture until only two remain. NaN positions are ignored.

## Methods

- `ui.touch { samples, config? }` replays up to 4096 samples through a fresh recognizer and returns
  `{ gestures: [{ kind, ... }], count, active_touches }`. `config` may override `pinch_threshold`, `scroll_threshold`,
  `swipe_distance` and `swipe_time`. Not mutating; permission `read`.

```json
{"method": "ui.touch", "params": {"samples": [
  {"id": 1, "stage": "down", "pos": [0, 0], "time": 0.0},
  {"id": 1, "stage": "up", "pos": [0, -150], "time": 0.1}]}}
```

## Errors

| Code | Meaning |
|---|---|
| `bad_samples` | `samples` is missing, too long or a sample is malformed |
| `bad_config` | a `config` threshold is not a non-negative number |

## Live gestures (ADR-2045)

`regia_ui::gesture::UiGestureInput` feeds live platform touches through a recognizer when `enabled` (off by
default; the single-touch pointer path is unaffected). Positions are physical px over the OS scale factor and time is
the real clock, so gestures work in a paused game's menus. Each gesture is also sent as a `UiGestureEvent` message.

- `ui.touch_live { enabled? }`: with no parameters it reports; `enabled` switches the live input. Returns
  `{ enabled, count, active_touches, recent }` where `recent` holds the last 32 gestures, oldest first, in the
  `ui.touch` gesture form. Mutating; permission `edit_scene`. Error: `bad_flag` (`enabled` is not a boolean).

```json
{"method": "ui.touch_live", "params": {"enabled": true}}
```

`ui.touch` requires a `samples` array with at most 4,096 entries. Each sample has an unsigned integer `id`, a `stage`
(`down`, `move`, `up` or `cancel`), a two-number `pos` in UI pixels and numeric `time` in seconds. Optional `config`
may set `pinch_threshold` (default 12 px), `scroll_threshold` (10 px), `swipe_distance` (80 px) and `swipe_time`
(0.4 s); each must be a non-negative number. Malformed or oversized samples return `bad_samples`; invalid thresholds
return `bad_config`.

## Failure example

Captured CLI output from `regia agent call ui.touch '{"samples":[],"config":{"swipe_time":-1}}'`:

```text
error: method call failed: `config.swipe_time` must be a non-negative number (-32602): {"code":"bad_config","hint":"omit it for the default"}
```

## Workflow

Replay a recorded sequence through `ui.touch`, inspect the returned `gestures`, then adjust only the relevant threshold
in `config` and replay the same samples. Because the input includes timestamps and no clock is read, the comparison is
repeatable. Use `ui.touch_live` only when checking platform-delivered gestures.
