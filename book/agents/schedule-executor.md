# Schedule executor

Every schedule of a Regia app (`First`, `PreUpdate`, `FixedUpdate`, `Update`, `PostUpdate`, `Last`,
`Extract`, `Render`, ...) runs on one of the two `bevy_ecs` executors (ADR-4933):

- `multi_threaded`: each system runs as a task on the compute pool, in parallel with the systems
  whose data access does not conflict. The main thread waits for the schedule to end.
- `single_threaded`: the systems run one after another on the thread that runs the schedule (the
  main thread), in the same order their `.before`/`.after`/`.chain()` edges give.

The default is `single_threaded` on Android and `multi_threaded` everywhere else. On a phone the pool
threads sleep on cores in deep idle states, many of them the small in-order cores. A frame of a
few hundred short systems then spends more time waking threads and handing work between them than
running the systems. Systems still fan out inside themselves (`regia_core::tasks::par_chunks`,
`scope`): only the hand-off between systems moves to the main thread.

| Method | Permission | What it does |
|---|---|---|
| `executor.get` | read | The executor the schedules run on, a request waiting for the next frame, and the platform default. |
| `executor.set` | play | Ask for `multi_threaded`, `single_threaded` or `default` (the platform's). |

## Read it

```json
{ "executor": "single_threaded", "requested": "single_threaded", "pending": false,
  "platform_default": "single_threaded", "switched_schedules": 14, "switches": 1 }
```

- `executor` is what the schedules run on now; `requested` is what the next frame installs.
  `pending` is `true` between an `executor.set` and the next frame.
- `switched_schedules` counts the schedules the setting installed its executor on. It is `0` while the
  app runs on the multi-threaded executor every schedule was created with: the default on a desktop
  never replaces an executor a test or a tool installed (`App::shuffle_schedules`, for example).
- `switches` counts how many changed requests were applied.

## A/B on a device

Switch, let a few frames pass, then measure the window with `play.hitch_report` or `pacing.trace`:

```sh
regia agent call executor.set '{ "executor": "multi_threaded" }'
regia agent call play.wait_until '{ "condition": { "resource": "Time", "path": "frame", "op": "gte", "value": <now + 120> }, "timeout_ms": 30000 }'
regia agent call play.hitch_report '{ "seconds": 8, "warmup_s": 1 }'
regia agent call executor.set '{ "executor": "default" }'
```

A switch takes effect between two frames. The first frame on a new executor builds its executor
state again (one allocation per schedule), so leave it out of the window you measure.

## Errors

`executor.set` answers `invalid_params` (JSON-RPC -32602, `F0002`) when `executor` is missing, is not a
string, or names something other than `multi_threaded`, `single_threaded` or `default`.

## From a host

```rust,wrap
use regia_app::{App, ScheduleExecutor};

let mut app = App::new();
app.set_schedule_executor(ScheduleExecutor::MultiThreaded); // keep the parallel executor on Android too
```

`ScheduleExecutor::platform_default()` is the default; `ScheduleExecutorSetting` is the resource
the app reads before each frame.
