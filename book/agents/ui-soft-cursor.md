# UI software cursor (agents)

ADR-273 I10, ADR-1842. Code: `regia_ui::soft_cursor`.

With the cursor enabled, a stick (the right one by default) moves a drawn cross over the window and the right trigger acts
as the primary mouse button. The movement goes through `UiPointer`, so hover, press, drag, click and tooltips behave as
with a mouse. Movement rule (`step_cursor`): a radial dead zone (default 0.15) rescaled so motion starts smoothly at its
edge, a squared response for fine control near the center, full-deflection speed 900 px/s by default, y up on the stick
meaning up on screen, and clamping to the window. The cursor appears on the first stick movement, starts where the real
pointer was, and a real mouse move hides it again. It is drawn above the layout debug overlay. The face-button navigation
(`South` submits the focused node) is unchanged.

## Methods

- `ui.soft_cursor { enabled?, speed?, dead_zone?, stick?, position?, nudge? }`: with no parameters it reports. `position`
  places the cursor (physical px) and shows it when the cursor is enabled; `nudge: { stick: [x, y], dt }` moves it as a stick held for `dt` seconds
  would, so a test needs no gamepad. Returns `{ enabled, active, position, speed, dead_zone, stick }`. Mutating;
  permission `edit_scene`.

```json
{"method": "ui.soft_cursor", "params": {"enabled": true, "position": [400, 300]}}
```

## Errors

| Code | Meaning |
|---|---|
| `bad_flag` | `enabled` is not a boolean |
| `bad_cursor` | `speed`, `dead_zone`, `stick`, `position` or `nudge` is out of range or malformed |

`enabled` is a boolean; `speed` is 1–20,000 physical pixels/second (default 900); `dead_zone` is 0–0.95 (default
0.15); `stick` is `left` or `right` (default `right`). `position` is `[x, y]` in physical pixels. `nudge` is
`{ "stick": [x, y], "dt": seconds }`, with stick axes clamped to −1..1 and `dt` from 0 through 1. `bad_flag` covers
a non-boolean `enabled`.

## Failure example

Captured CLI output from `regia agent call ui.soft_cursor '{"enabled":true,"speed":0}'`:

```text
error: method call failed: `speed` must be a number from 1 to 20000 (-32602): {"code":"bad_cursor","hint":"see the parameter list of ui.soft_cursor"}
```

## Workflow

Read the initial position, enable the cursor, then set `position` or send a `nudge` to move it deterministically.
Use `ui.pick_at` with the returned physical position to identify the node under it, and disable the cursor when the
test is complete.
