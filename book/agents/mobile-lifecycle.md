# Mobile lifecycle, thermal and touch controls

Android takes the window away on every home press or screen-off. The renderer must drop every
surface before the callback returns and create one per window on resume, keeping the GPU device.
The OS also throttles a hot phone, and touch screens have no keys. These methods let an agent test
all three on a desktop, without a phone (ADR-527).

| Method | Permission | What it does |
|---|---|---|
| `lifecycle.get` | read | Lifecycle state, the drill queue and the surface audit. |
| `lifecycle.drill` | play | Queue synthetic suspend → resume cycles. |
| `thermal.get` | read | The thermal reading in effect, the override and the quality governor. |
| `thermal.set_override` | play | Force a thermal reading, or clear the override. |
| `input.touch_controls` | read | Touch zones, gesture bindings, which finger holds which zone. |

## Suspend/resume drills

`lifecycle.drill { "cycles": N }` (1..=1000) queues N cycles. The windowed runner runs one cycle
per frame. Each cycle sends `Suspended` then `Resumed` through the runner's own lifecycle tracker,
the same path a Steam Deck wake takes. On a desktop the real surfaces are dropped and recreated,
audio output stops and starts again, and gameplay time does not advance. `{ "cancel": true }`
drops the queue.

```json
{ "cycles": 20 }
→ { "queued": 20, "pending": 20, "completed": 0 }
```

When the app is not running (before the first resume, suspended by the OS, or exiting), nothing
is queued:

```json
{ "cycles": 20 }
→ { "queued": 0, "refused": "lifecycle_not_running", "state": "suspended" }
```

| `refused` code | Meaning |
|---|---|
| `lifecycle_not_running` | The app is not in the `running` state; wait until `lifecycle.get` reports it. |

Bad parameters (`cycles` missing, 0, or over 1000) are `INVALID_PARAMS` (`F0002`).

## The surface audit

The renderer reports to the surface audit after every lifecycle transition. The audit checks three
rules:

| Violation `code` | Rule broken |
|---|---|
| `surface_kept_while_suspended` | A surface was still alive after `Suspended` or `WillTerminate`. |
| `surface_missing_after_resume` | After `Resumed`, fewer surfaces were alive than windows exist. |
| `surface_count_mismatch` | Surfaces created − dropped ≠ surfaces alive (a leak or a double drop). |

`lifecycle.get`:

```json
{}
→ { "state": "running", "resume_count": 21,
    "drill": { "pending": 0, "completed": 20 },
    "audit": { "clean": true, "suspends": 20, "resumes": 21, "live": 1, "windows": 1,
               "created_total": 21, "dropped_total": 20, "violation_count": 0, "violations": [] },
    "surfaces": { "live": 1, "created_total": 21, "dropped_total": 20, "lost_streak": 0 } }
```

`surfaces` is `null` without the renderer. The audit keeps the first 32 violations; each has
`code`, `event`, `resumes` (resumes seen before it) and the counts it saw. `violation_count` keeps
counting after 32.

A full check: `lifecycle.drill { "cycles": 50 }`, then `play.wait_until` on `lifecycle.get`'s
`drill.pending == 0`. Then expect `audit.clean` to be true and `audit.resumes` to have grown by 50.
Take a `seeing.screenshot` to confirm the frame came back.

On a phone, the platform layer logs one `regia surface audit: suspends=… resumes=… created=…
dropped=… live=… windows=… violations=…` line per resume. `cargo xtask android-lifecycle` (and
`android-pass`) reads the last one and fails the run when `violations` is above 0.

## Thermal override

`thermal.get` returns `reading` (the `ThermalState` in effect: Android's `PowerManager` status and
10-second headroom, idle on desktop) and `override` (the forced reading, or `null`). It also
returns `governor`: `enabled`, `level` 0..=3, `factors`, `min_scale`, `relax_seconds`, `max_fps`
(the frame cap per level), `base_render_scale`, `holding_max_fps` and `base_max_fps`.

`thermal.set_override { "status": "severe", "headroom": 0.95 }` forces a reading from the next
frame. Statuses are `none`, `light`, `moderate`, `severe`, `critical`, `emergency` and
`shutdown`. Headroom is optional, `>= 0` (1.0 = the severe threshold). `{ "clear": true }`
removes the override, and the OS reading returns. The answer is `{ before, after }`.

The governor then steps the render scale down at once, and back up one level per `relax_seconds`
of cooler readings. When the host set `ThermalGovernor::max_fps` for that level, it also caps the
frame rate. Each level change writes a `ThermalLevelChanged { from, to, reading }` message. Games
read it to shed their own load: simulation detail, particle budgets, AI tick rate.

A check: `thermal.set_override { "status": "critical" }`, step a frame, then expect
`render.get_profile`'s `thermal.governor_level` to be 3 and the render scale to have dropped. Then
`thermal.set_override { "clear": true }`. The scale returns only after the relax time, so turn the
governor off from game code when a test needs the scale back at once.

## Touch controls

A host maps touches to actions with the `regia_input::TouchControls` resource (code, not a RON
file):

- **Zones**: rectangles in normalized window coordinates (0..1, top-left origin, y down). The
  first finger that lands in a zone claims it until the finger lifts. A `Button` zone holds its
  action at 1.0. A `Stick` zone is a floating stick: the finger's offset from where it landed, y
  up, divided by `radius_px`, clamped to unit length, with a `dead_zone`.
- **Gesture bindings**: `tap`, `double_tap`, `long_press`, `swipe` (any direction; the value is
  the direction; the recogniser emits it only for a drag of at least 48 px that lifts within
  1 second), `swipe_up` / `swipe_down` / `swipe_left` / `swipe_right`, `pinch` (x = scale − 1)
  and `two_finger_pan` (movement in screen heights, y up). Each pulses its action for `frames`
  frames, optionally only inside a rectangle. A zone claims the finger that starts inside it.
  Gesture suppression uses the gesture's point: a tap, double tap or long press uses its position
  (the lift or the hold), a swipe uses its start, and a pinch or two-finger pan uses its center.
  A tap that lifts outside a zone still pulses when the finger went down inside one.

The values go through `InputInjection`, like the UI's virtual sticks, so triggers and phases work
as they do for a key.

`input.touch_controls`:

```json
{}
→ { "installed": true,
    "zones": [{ "name": "move", "action": "move", "rect": { "min": [0, 0], "max": [0.5, 1] },
                "kind": { "type": "stick", "radius_px": 120, "dead_zone": 0.1 } }],
    "gestures": [{ "gesture": "swipe_up", "action": "jump", "rect": null, "frames": 2 }],
    "claims": [{ "finger": 1, "zone": "move" }], "held_actions": ["move"],
    "fingers": [{ "id": 1, "phase": "Moved", "start": [200, 600], "current": [260, 540] }] }
```

Without the resource the answer is `{ "installed": false, "fingers": [...] }`. Drive the zones
with `play.inject_input` touch events (`{ "type": "touch", "id", "phase", "x", "y" }`, see
[testing](testing.md)), then read the action with the input methods.

## Not covered

- The drill runs on a desktop. It does not reproduce Android's own teardown order (the activity's
  `onPause` before the window goes) or a native window that comes back at a new size. The device
  pass still covers those.
- The thermal override does not change what the OS throttles, only what the engine sees.
