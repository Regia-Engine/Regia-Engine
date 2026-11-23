# Quick apps: the model, the loop, and driving a one-file game

A quick app (`regia::quick::app(model).update(update).view(view)`, `examples/quick/*.rs`, projects
from `regia new --template quick`) keeps its whole game state in one reflected resource,
`regia_quick::app::Model<crate::YourModel>`. Its drawing is not made of entities. So for a quick
app, `scene.query_entities` shows little: read the **model** and the **draw list** instead, and
look with `seeing.screenshot`.

## Read the model

The `QuickModel` predicate resource is the model as JSON: struct fields by name, `Vec2`/`Vec3` and
tuples as arrays, so `head.1` is the y of a `(i16, i16)` and `vel.1` the y of a `Vec2`.

```json
play.expect { "assertions": [
  { "condition": { "resource": "QuickModel", "path": "game_over", "op": "eq", "value": false } }
] }
→ { "pass": true, "results": [ { "detail": "QuickModel.game_over = false, eq false", ... } ] }
```

`resources.list` shows the model's full type (`regia_quick::app::Model<snake::Snake>`) and its
docs. Fields marked `#[reflect(ignore)]` (random generators, caches) are not in the JSON.

## The loop

`QuickLoop` is the run mode and the loop counters:

```json
{ "run_mode": { "mode": "LoopNTimes", "n": 1 }, "runs": 1, "ran_this_frame": false, "frozen": true }
```

- `Loop`: `update`/`view` every frame. `runs` counts them.
- `Wait`: they run on the first frame, then on input, a resize, or a scale-factor change. `ran_this_frame` is false while idle.
- `LoopNTimes(n)`: after `n` runs, `frozen` is true. The last drawing stays on screen
  (`quick.draw_list` reports `"frozen": true`) and the window still closes normally.

## Drive and check a quick game

Inject input, wait for the model to react, then look:

```json
play.inject_input { "events": [ { "type": "key", "key": "ArrowDown", "text": "", "state": "pressed" } ] }
play.wait_until { "condition": { "resource": "QuickModel", "path": "head.1", "op": "gte", "value": 2 },
                  "timeout_ms": 8000 }
seeing.screenshot { "timeout_ms": 30000 }
```

Keys are physical names (`ArrowLeft`, `Space`, `Enter`, `KeyW`). Touches are
`{ "type": "touch", "id": 0, "phase": "started", "x": 400, "y": 300 }` in window pixels; the game
sees them through `q.touches()` in drawing coordinates (center origin, +Y up).

The HUD text a quick app draws is in `quick.draw_list` → `texts` ([quick draw](quick-draw.md)).
That is the cheapest way to read a score or a prompt without a screenshot.

## Running the examples headless

The examples are windowed apps. On a machine without a display, run one under Xvfb with lavapipe
and point the agent at a project directory of your choice:

```console
$ VK_LOADER_DRIVERS_SELECT='*lvp*' REGIA_PROJECT_DIR=/tmp/snake \
    xvfb-run -a -s "-screen 0 1280x800x24" target/debug/examples/snake &
$ regia agent call seeing.screenshot '{"timeout_ms":30000}' --project /tmp/snake
```

## Hot reload

The model survives a game-library reload: it is snapshotted, the builder's `model` runs again in
the new library, and the snapshot is applied over it. Fields added since the last load start at
their `Default`. Check it after a reload with the same `QuickModel` predicate you used before.

## Limits

- A `draw_3d` frame needs at least one mesh (a cube, plane, …) for the 3D view to render. Lines
  alone (`grid`, `line`, `cube_wires`) draw nothing on their own.
- `QuickModel` reads one model. An app with two quick apps reports the first one found.
