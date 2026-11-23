# Frame loop and quitting

Control how the running app steps frames, and ask it to quit, without touching game code. The loop
settings are one view over the frame cap (`FramePacing`), the fixed-step rate (`FixedTime`) and the
runner's throttling (`regia_app::LoopSettings`).

| Mode | What runs | Input |
|---|---|---|
| `continuous` | One frame per redraw, capped by `max_fps` (the default). | Delivered. |
| `bench` | Frames back to back: no frame cap, no sleeping, no redraw round trip. Use it to measure throughput. | **Ignored** by the window runner (agent `play.inject_input` still works). |
| `lazy` | A frame when input or a window event arrives (or the game calls `LoopControl::request_redraw`), otherwise one every `1 / throttled_fps` s so the protocol stays responsive. | Delivered. |
| `run_frames` | `frames` frames from the moment it is set, then **frozen**: game time pauses and frames run at `throttled_fps`. Any other mode thaws it (a pause the game set itself is kept). | Delivered. |

The headless runner (tests, servers) has no OS events and treats `lazy` like `continuous`.

## Read the loop

`loop.get`:

```json
{}
→ { "mode": "continuous", "frames": null, "frames_in_mode": 0, "frozen": false, "accepts_input": true,
    "max_fps": null, "fixed_hz": 60.0, "max_substeps": 5, "throttled_fps": 10.0 }
```

## Change it

`loop.set` takes any of `mode` (+ `frames` for `run_frames`), `max_fps` (`1..=10000`, `null` =
no agent cap), `fixed_hz` (`1..=1000000000`), `max_substeps` (the catch-up limit, `1..=64`) and
`throttled_fps` (`1..=1000`), and returns `{ before, after }`. Those are the rates the engine
runs (`MIN_PACING_FPS`/`MAX_PACING_FPS`, `MAX_FIXED_HZ`, `MAX_SUBSTEPS_LIMIT`,
`MIN_THROTTLED_FPS`/`MAX_THROTTLED_FPS`). A value outside them is `INVALID_PARAMS` and changes
nothing: `loop.set` does not clamp. A direct `LoopSettings::apply` from Rust still clamps into
the same ranges, so `loop.get` after a protocol call matches the request.

`max_fps: null` clears the cap you set, and the loop then runs uncapped (or at vsync) **unless the render profile
has a frame cap of its own**: the `Deck` preset's 40 fps cap is copied into pacing again on the next frame whenever no
cap is set, so a later `loop.get` reports `max_fps` 40 there (found on the Steam Deck pass, ADR-5565). To measure headroom on
such a profile, set a large cap instead (`{"max_fps": 10000}`); the panel's vsync still limits a window.

```json
{ "mode": "run_frames", "frames": 120 }
→ { "before": { "mode": "continuous", ... }, "after": { "mode": "run_frames", "frames": 120, "frozen": false, ... } }
```

A reproducible still: `loop.set { "mode": "run_frames", "frames": 120 }`, then
`play.wait_until` on `loop.get`'s `frozen`, or wait, then `seeing.screenshot`. A throughput
number: `loop.set { "mode": "bench" }`, `play.get_diagnostics` after a few seconds, then
`loop.set { "mode": "continuous" }`.

## Quit

`quit.request` asks the app to quit the way closing its primary window does, so the game's
`ClosePolicy` decides:

```json
{}
→ { "window": 4294967297, "will_exit": true, "handled_by": "engine" }
```

With `ClosePolicy.exit_on_primary_close` off (a game with a "quit?" dialog) `will_exit` is `false`:
the game receives `WindowCloseRequested` and may cancel. Headless apps without a window exit directly
when the policy allows. The connection closes when the app exits.

`quit.request { "mode": "fast" }` skips the teardown: the windows are hidden at once, and a frame
later a bounded flush runs and the process ends (see [fast quit and single
instance](process-guards.md)).

Every quit (this request, a closed window, `AppExit`, `regia::request_clean_exit`) runs the engine's
clean exit: stop the pipeline warm, leave fullscreen, wait for the GPU (at most 1 s), drop the
surfaces, the world and the device, then write the pipeline cache. The connection closes when the
world drops, in the middle of it. To check a quit, read the app's log: one `regia exit: <step> took
<ms> ms` line per step, a warning for any step over its budget, `regia exit: GPU device destroyed;
no wgpu objects left`, and `regia exit: done in <ms> ms` (ADR-188).

## Input timestamps

Every raw input event (`KeyboardInput`, `MouseButtonInput`, `CursorMoved`, `MouseMotion`,
`MouseWheel`, `TouchInput`) carries `timestamp`, when it reached the engine (winit exposes no OS
event time, so this is the receipt time in the event loop). `InputTiming.max_latency()` gives the
oldest input of the frame to frame start, and input recordings store each step's stamps for
analysis; replays are indexed by step and never use them. Game code reads
`regia_platform::InputTiming`; there is no protocol method for it.
