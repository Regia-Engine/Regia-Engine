# SpaceMouse (agents)

Desktop apps read 3Dconnexion SpaceMouse devices (any number, merged into one state): on Linux
through the `spacenavd` socket when the daemon runs, otherwise (and on Windows) through raw HID. Every Regia Engine UI app (the editor, Tincal, Red-Rouge) moves its viewport camera with the same
shared function, `regia_input::spacemouse::nav::step`, so the device behaves the same in all of
them. See the [user guide](../user-guide/spacemouse.md) for the defaults table and the Linux udev
rule. Guide topic: `agent.guide {"topic": "spacemouse"}`. Specification: `docs/specs/29-spacemouse.md`.

No hardware is needed to test: inject a state and check the camera.

With no device attached, `input.spacemouse_status` succeeds and reports `"connected": 0`
(`connected` is the number of connected devices) and a `devices` array that is empty when the
backend lists none (the other status fields describe the default merged state and configuration). Missing hardware is not an error. The handler returns `F0013`
(`unsupported`) when the SpaceMouse plugin is absent; Android also returns `F0013` because this
backend is desktop-only. Invalid injection parameters use `F0002`.

## Methods

- `input.spacemouse_status` returns `{ backend, connected, devices, state, injected_frames_left,
  config, toggles, linux_udev_rule }`. `devices` lists each device with `status` `connected`, or
  `unavailable` with a `reason` (no permission, in use by another driver), and its `source`
  (`spacenavd`, `hid` or `fake`). `state` is the merged
  sample `{ translation, rotation, buttons }`, normalised to -1..1 and before the dead zone.
  `config` is the user's `{ enabled, sensitivity, invert }`; `toggles` is `{ horizon_lock,
  dominant, object_tool }`. `linux_udev_rule` is the rule text when a Linux device is unavailable. Read-only.
- `input.spacemouse_inject` replaces the device state for `frames` frames (default 1):
  `translation [x, y, z]`, `rotation [rx, ry, rz]` (each -1..1, clamped), `buttons` (an array of
  button numbers or a bitmask). Pressing a button fires its action: 1 `view.fit_selection`,
  2 `view.home`, 3 toggles dominant-axis mode, 4 toggles horizon lock, 5 and up
  `spacemouse.button.N`. Returns `{ injected, frames, from_frame }`. Mutating; permission `play`.
- `input.spacemouse_nav_defaults` returns the fixed constants every app shares: dead zone 0.04,
  quadratic curve, orbit pan 1 orbit distance per second, zoom `exp(1.5 · v.z · dt)`, rotation
  1.5 rad/s, fly speed 1 scene scale per second, pitch limit 89°, horizon lock on, dominant axis
  off, the 2D modes (View2D pan 1 extent per second, Timeline scrub 1 span per second on `ry`,
  Step threshold 0.5 on the shaped `rz` with a 0.35 s repeat), the object tool (off, toggle
  action, 0.3 s gesture rest), the button table and the override ranges. Read-only.

Modes (`NavMode`): Orbit (default), Fly, Turntable (the Orbit delta applied to the object), View2D,
Timeline and Step. A golden table of `nav::step` inputs and outputs is checked in at
`crates/regia_input/tests/data/spacemouse_nav_golden.json`.

Frame of the values: +x right, +y up (lift the cap), +z toward the viewer (pull the cap). `rx` is
tilting the cap away from you, `ry` twisting it counter-clockwise seen from above, `rz` tilting it
left. Pushing the cap away (`translation [0, 0, -1]`) zooms in.

```json
{"method": "input.spacemouse_inject", "params": {"rotation": [0, 1, 0], "frames": 30}}
```

## Events

`events.read` records kind `spacemouse`: `{ "event": "action", "action", "button" }` for every
button press, and `{ "event": "connected" | "disconnected", "id" }` for hot-plug.

## Typical loop

1. `input.spacemouse_status` to see whether a device is present (an agent run usually has none).
2. `input.spacemouse_inject {"rotation": [0, 1, 0], "frames": 30}`, then `play.step` 30 frames (or
   wait), then `seeing.screenshot`: in an orbit viewport the camera has turned
   `30 · dt · 1.5` radians counter-clockwise around its target.
3. `input.spacemouse_inject {"buttons": [2]}` sends the camera home; `{"buttons": [1]}` frames the
   selection.

On a headless Linux run with no device, read status, inject a short rotation, advance the same
number of simulation frames, and inspect an offscreen screenshot. This checks navigation without
requiring hardware.

For example, the dispatcher's limit check (`frames` in 1..=4294967295) rejects a zero frame count
with `F0002` before the handler runs; the answer carries the `limit` it broke:

```json
{"jsonrpc":"2.0","id":8,"method":"input.spacemouse_inject","params":{"frames":0}}
→ {"jsonrpc":"2.0","id":8,"error":{"code":-32602,"message":"`frames` must be in 1..=4294967295 (got 0)","data":{"code":"F0002","hint":"agent.schema lists each method's params and limits","limit":{"kind":"range","max":4294967295.0,"min":1.0,"param":"frames"}}}}
```

## Example calls

Example calls (`regia agent call <method> '<params json>'`; entity ids come from `scene.query_entities`):

```text
input.spacemouse_status {}        # no device in an agent run: connected is 0
input.spacemouse_nav_defaults {}   # the shared navigation constants
```
