# Game states: GameFlow, sub-states and computed states

Games built on Regia Engine describe their flow (title screen, loading, play, pause) as **game states**
(`regia_ecs::state`, adopted from `bevy_state`). Every state is reflected, so you can read and
drive any game's flow without knowing its code: `state.list` shows what exists, `state.get` reads
one, `state.set` requests a change, and the `{ "state": … }` predicate waits for it.

## The model in one paragraph

A state type has one current value (`State<S>`). A change is a *request* (`NextState<S>`), applied
once per frame by the `StateTransition` schedule, which runs right after `PreUpdate` (and once
before `Startup`). A transition runs `OnExit(old)`, then `OnTransition { old → new }`, then
`OnEnter(new)`, and despawns entities tagged `DespawnOnExit(old)`. **Sub-states** exist only while
their source state has a given value (a pause-menu page exists only while paused) and start at
their default each time. **Computed states** are derived from other states (`InGame` = playing or
paused) and cannot be set; set their source instead.

## GameFlow

`regia_framework::flow::GameFlowPlugin` gives a game the standard flow:

```
Boot → MainMenu → Loading → Playing ⇄ Paused
```

| State | Kind | Values |
|---|---|---|
| `GameFlow` | settable | `Boot` (one frame), `MainMenu`, `Loading` (until `LoadingTasks` is empty), `Playing`, `Paused` (freezes `Time`) |
| `InGame` | computed from `GameFlow` | exists while `Playing` or `Paused`; level content uses `DespawnOnExit(InGame)` |
| `PausePage` | sub-state of `GameFlow::Paused` | `Main`, `Options` |

`GameFlowSettings { skip_main_menu, pause_time }` controls the automatic steps. The
`third_person` sample uses it with `skip_main_menu` (Boot → Loading → Playing in three frames) and
keeps it in step with its Esc pause menu.

## state.list

```json
state.list {}
→ { "count": 3, "states": [
    { "state": "regia_framework::flow::GameFlow", "name": "GameFlow", "exists": true,
      "current": "Playing", "settable": true, "pending": "Unchanged",
      "variants": ["Boot", "MainMenu", "Loading", "Playing", "Paused"] },
    { "state": "regia_framework::flow::InGame", "name": "InGame", "exists": true,
      "current": null, "settable": false, "pending": null, "variants": [] },
    { "state": "regia_framework::flow::PausePage", "name": "PausePage", "exists": false,
      "current": null, "settable": true, "pending": "Unchanged", "variants": ["Main", "Options"] } ] }
```

`exists` tells absent apart from a unit value (`InGame` is a unit struct, so its `current` is
`null` even while it exists). `pending` is the queued request: `"Unchanged"`,
`{ "Pending": v }` (forced) or `{ "PendingIfDifferent": v }`.

## state.get

```json
state.get { "state": "GameFlow" }
→ { "state": "regia_framework::flow::GameFlow", "name": "GameFlow", "exists": true,
    "current": "Playing", "settable": true, "pending": "Unchanged", "variants": [ … ] }
```

`state` accepts the short name or the full type path; an ambiguous short name answers with the
candidates (`F0002`). An unknown state type is `F0003`.

## state.set

```json
state.set { "state": "GameFlow", "value": "Paused" }
→ { "state": "regia_framework::flow::GameFlow", "before": "Playing", "requested": "Paused",
    "force": false, "applies": "on the next StateTransition (after the next PreUpdate); …" }
```

Needs the `play` permission. The change is applied on the **next frame**, not during the call:
follow it with `play.wait_until { "condition": { "state": "GameFlow", "value": "Paused" } }` (or,
with the app held by `play.pause`, advance one frame with `play.step`). Setting the current value does nothing unless
`"force": true`, which re-runs `OnExit`/`OnEnter`. A value that is not a variant fails with the
variant list; a computed state fails (set its source); both are `F0002`; a sub-state that does not exist right now
answers with a `warning`; the pending value is consumed by the next `StateTransition` and is
applied only if the source state at that transition makes the sub-state exist, otherwise it is dropped.

## Waiting on a state

```json
play.wait_until { "condition": { "state": "GameFlow", "op": "eq", "value": "Playing" }, "timeout_ms": 5000 }
play.expect { "assertions": [ { "name": "pause page", "condition": { "state": "PausePage", "value": "Main" } } ] }
```

The state atom compares the current value (`null` while the state does not exist).

## Recipes

- **Skip the menu in a test:** `state.set { "state": "GameFlow", "value": "Loading" }`, then wait
  for `Playing`.
- **Check pause really freezes the game:** set `Paused`, wait for it, read a moving entity's
  `Transform` twice; it must not change, and `{ "resource": "Time", "path": "paused", "value": true }`
  holds.
