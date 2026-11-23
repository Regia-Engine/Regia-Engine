# Coroutines

Gameplay coroutines (`regia_framework::coroutine`) are `async` sequences — cutscenes, tutorials,
spawn waves — polled once per frame. Use `coroutines.list` to see which are running and what each is
waiting for, for example to wait until an intro sequence finishes before taking a screenshot.

```json
{}
→ { "count": 2, "running": 2, "coroutines": [
      { "id": 0, "generation": 0, "label": "intro", "owner": 4294967301, "started_frame": 12,
        "waiting": "seconds", "seconds_left": 1.25, "frames_left": null, "finished": false },
      { "id": 1, "generation": 3, "label": "", "owner": null, "started_frame": 40,
        "waiting": "condition", "seconds_left": null, "frames_left": null, "finished": false } ] }
```

- `waiting` is `seconds` (game time; frozen while paused), `frames`, `condition` (a `wait_until`
  predicate), `world` (a `with_world` closure) or `nothing`.
- `owner`: the coroutine is canceled on the first frame that entity no longer exists. Despawning it
  with `scene.despawn` is how to stop an owned sequence from the protocol.
- `finished`: done, holding a result the game has not taken yet.
- Coroutines are polled in start order, after timers and before scripts. `F0003` (numeric `-32001`) means
  the app has no `CoroutinePlugin`.

Coroutines are **not saved** by save games and do not survive a game-library hot reload: after
`save.load` the game restarts any sequence from saved state.

Recipe — wait for a sequence: `play.wait_until` with a condition the sequence sets (a resource field
or component), or poll `coroutines.list` until the labeled coroutine is gone.

`coroutines.list` takes no parameters. It returns `F0003` (numeric `-32001`) when the app has no
`Coroutines` resource; add `CoroutinePlugin` to make the method available. Workflow: start the named
coroutine in gameplay code, call `coroutines.list`, then wait on its observable completion condition
with `play.wait_until` (or poll the list) before capturing the result.

Captured failing call (lavapipe; this editor has no coroutine plugin):

```console
$ regia agent call coroutines.list '{}' --project samples/arena
error: method call failed: no Coroutines resource in this app (-32001): {"code":"F0003","hint":"add regia_framework::coroutine::CoroutinePlugin and start coroutines with start_coroutine"}
```
