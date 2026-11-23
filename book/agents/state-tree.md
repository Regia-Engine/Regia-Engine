# AI: state trees

State trees are the second decision model in `regia_ai` next to behavior trees (`regia_ai::state_tree`, ADR-1565).
A tree is a hierarchy of states. While a state is active its tasks run; transitions leave a state on a tick, an
event or completion, and a parent picks which child to enter by order, utility score or weighted chance. They use
the same blackboard, the same `BtTask` types and the same `BtTaskRegistry` as behavior trees. The authored file is
`.statetree.ron` (format in `docs/formats/statetree-ron.md`, version 2; version 1 files still load).

## What an agent entity carries

| Component | What it is | Reflected |
|---|---|---|
| `StateTree` | The compiled tree (not reflected; build it with `StateTree::compile(&def, &registry)`). | no |
| `StateTreeStatus` | Read-only snapshot written each fixed step: `status`, `active_path`, `ticks`, `trace`, `paused`, `held`. | yes |
| `StateTreeInbox` | `events: [String]`; names appended here are delivered on the next fixed step. `dropped: u64` counts events dropped while the tree was paused or its agent parked (see below). | yes |
| `Blackboard` | Conditions and utility scores read it; `state_key` writes the active leaf name to it. | yes |
| `StateTreeDebug` | Development builds only: `disabled` and `pause_on_enter` state names (see `statetree.debug`). | yes |

`status` is `NotStarted`, `Running`, `Succeeded` or `Failed`. `active_path` lists the active states root first.
`trace` holds the last 64 lines, for example `tick 4: transition Idle -> Alert on event:heard (priority 0)`.

## Reading and steering a running tree

```json
scene.get_entity { "entity": 4294967296, "components": ["StateTreeStatus"] }
→ { "components": { "StateTreeStatus": { "value": { "status": "Running",
      "active_path": ["Guard", "Alert"], "ticks": 12,
      "trace": ["tick 1: enter Guard", "tick 1: enter Idle", "tick 7: transition Idle -> Alert on event:heard (priority 0)"] } } } }

scene.set_property { "entity": 4294967296, "component": "StateTreeInbox", "path": "events", "value": ["heard"] }
```

Use `play.wait_until` / `expect` on `StateTreeStatus.active_path` or `.status` to wait for a behavior. Set blackboard keys with `scene.set_property` (`"component": "Blackboard"`) to open or close conditions.

## Methods

| Method | Class | Params | Result |
|---|---|---|---|
| `statetree.compile` | read | `text` or `path`, optional `includes` (default true) | `{ ok, states, errors: [{ reason, state?, detail }] }` |
| `statetree.control` | play | `entity`, `op`: `pause`, `resume` or `restart` | `{ status, paused }` |
| `statetree.reload` | play | `entity`, `text` or `path` | `{ outcome: "kept" or "restarted" }` |
| `statetree.debug` | play (dev) | `entity`, `disabled?`, `pause_on_enter?` (state names) | `{ disabled, pause_on_enter }` |

`path` is project-relative (no `..`). `statetree.compile` is the way to validate a file an agent just wrote before
attaching it. Compilation stops at the first error, so `errors` holds at most one entry: fix it and compile again. `statetree.reload` compiles first: a definition that does not compile answers the compile reason and the
running tree keeps running. When it compiles, the active path, tick count and event queue are kept if every active
state still exists under the same parents (`kept`); otherwise every state is exited and the tree starts from the new
root on its next tick (`restarted`).

Errors are `F0002` with `data.reason`: `missing_param`, `entity_not_found`, `no_state_tree`, `bad_op`,
`state_not_found` (an unknown name given to `statetree.debug`), `dev_only` (`statetree.debug` in a shipping build),
plus the compile reasons below. A missing project file answers `F0003`.

## Format version 2

Everything in version 1 keeps its meaning; these fields are optional (`docs/formats/statetree-ron.md`):

```ron
StateTreeDef(
  format_version: 2,
  root: "Root",
  ambient: ["game::TrackThreat"],
  includes: [ (prefix: "Combat", path: "ai/combat.statetree.ron") ],
  states: [
    (name: "Idle",
     enter_set: { "mood": String("calm") },
     transitions: [
       (trigger: Tick, condition: Compare(key: "threat_dist", op: Lt, value: 12.0), hold: 0.25, target: State("Alert")),
       (trigger: Event("hit"), target: Parent),
       (trigger: Event("noise"), target: Stay),
     ]),
    (name: "Search", fallback: NextEnterable),
  ],
)
```

- `enter_set` writes literals (`Bool`, `Int`, `Float`, `String`) to the blackboard, in key order, when the state becomes
  active and before its tasks first tick. Tasks read their settings from those keys. Leaving does not remove them.
- `hold` (seconds) makes a `Tick`, `Succeeded`, `Failed` or `Completed` transition wait until its trigger and guard
  have held for that long in a row, counted in fixed steps (0.25 s at 1/60 s is 15 steps). The transition fires on
  the step the counter reaches the count, or on a later step while trigger and guard keep holding when it was
  rejected (target not enterable) or outranked on that step (ADR-1809). Losing the guard resets the counter, and so
  does leaving the state.
  `StateTreeStatus.held` lists the transitions whose counter is running, as `Owner -> Target`.
- Targets `Parent` (re-enter the parent and descend by its selection), `Stay` (match and consume the trigger,
  change nothing, so a lower-priority transition does not take the event) and `NextEnterable` (the first later
  sibling whose `enter` and descent succeed; `Next` still means exactly the next sibling).
- `fallback: NextEnterable` on a state: when a transition into it finds nothing to enter below it, the following
  siblings are tried instead of rejecting the transition.
- `ambient` task paths tick first on every tick while the tree runs, before the first selection, so a value an ambient
  task writes is visible to `enter` conditions. Their results are ignored.
- `includes` inlines other trees at compile time: every state of the included file is renamed `prefix/<name>` and its
  root is reachable as `prefix/<its root>`. Nest at most 4 deep; a cycle is an error.

A paused tree (`statetree.control` `pause`, `StateTree::pause`) advances nothing: no tasks, timers or events, and the
tick count stops. Queued events wait, and so do events sent to its `StateTreeInbox`: they stay in the inbox, in order,
until the tree resumes, so a burst sent during a pause writes no `dropped` trace lines. Meanwhile the inbox keeps
the newest 64 events (the tree's queue size): older ones are dropped, counted in `StateTreeInbox.dropped`, and the
first drop on an inbox logs a warning. The same cap holds while a layer parks the agent. A nonzero `dropped` after a
pause means the host sent more events than the tree can take; send state through the blackboard instead. A `pause_on_enter` state pauses
the tree at the end of the tick that enters it. The pause flag and the debug lists survive a `statetree.reload`.

## Finding out why a transition did not fire

Read `StateTreeStatus.trace` (the last 64 lines). A line `rejected Idle -> Alert (cannot enter)` means the guard
held but the target could not be entered: check the target's `enter` condition (and its children's). No line at all
means the guard never held: read the blackboard, and for a transition with `hold` look at `held`; a counter that
keeps resetting means the guard flickers. `event heard ignored` means no active state had a matching `Event`
transition when the event arrived. Use `statetree.debug` to disable a competing state or to pause as a state is
entered, then read the blackboard before resuming.

## A worked guard example

```json
statetree.compile { "path": "ai/guard.statetree.ron" }
→ { "ok": true, "states": 6, "errors": [] }
scene.set_property { "entity": 4294967296, "component": "StateTreeInbox", "path": "events", "value": ["heard"] }
play.wait_until { "condition": { "entity": 4294967296, "component": "StateTreeStatus", "path": "active_path[1]", "op": "eq", "value": "Alert" }, "timeout_ms": 5000 }
statetree.control { "entity": 4294967296, "op": "pause" }
→ { "status": "Running", "paused": true }
```

## How a tick runs

1. Ambient tasks run first, so a blackboard value they write is visible to the event guards of the same tick.
2. Queued events are handled in order. Each event takes at most one transition (matching `Event` transitions of any
   active state, best first). An event that matches nothing is dropped, never kept for later.
3. The tasks of every active state run, root first. A state with several tasks is complete when all succeed, or at
   once when one fails (later tasks of that state then stop).
4. The `hold` counters advance for every active transition whose trigger and condition hold; the others reset.
5. Tick and completion transitions are collected from every active state and the best one that can be entered is
   taken. At most one per tick, so a loop of always-true transitions advances one state per tick. A held transition
   matches from the tick its counter is reached for as long as it keeps holding, so one that was outranked or rejected
   on that tick fires later.
6. `pause_on_enter` requests are applied, `state_key` is written, and the status is updated.

Choosing among matching transitions: higher `priority` first, then the deeper state, then declaration order. A
transition whose target (or something it would descend into) cannot be entered is skipped with a `rejected` trace
line and the next candidate is tried.

Completion: a state with tasks completes `Succeeded` or `Failed` when its tasks finish; a state without tasks takes
the completion of its active child; a leaf without tasks never completes. `Succeeded`, `Failed` and `Completed`
transitions match while the state is complete. A `hold` on a completion trigger counts from the tick the state
completes; because a state without tasks takes its child's completion, a root without tasks of its own finishes the
tree on that same tick, before the hold ends: give an ancestor a long-running task when a held completion transition
must fire. If nothing handles a completed root, the tree finishes with that result
(`Succeeded` or `Failed`); `Succeed` and `Fail` targets finish it directly. A finished tree ignores ticks and
events until `StateTree::restart`.

Entering a state re-enters the target itself even if it is active (a self transition restarts it) and enters its
inactive ancestors; shared ancestors stay active and keep their tasks running. Leaving a state calls `reset` on the
tasks that ran, leaf first.

## Choosing a child

- `First`: the first child whose `enter` condition holds.
- `Utility`: the enterable child with the highest `bias + scale * value`; ties go to the earlier child; a missing key
  reads as 0 and a non-finite score is skipped.
- `Weighted`: a seeded random pick weighted by `weight`; deterministic for a given `seed` and history; weight 0 or less
  is never picked.

If no child can be entered the parent cannot be entered either (at start the tree then fails).

## Errors

`StateTree::compile` / `compile_with` return a `StateTreeError` (`UnsupportedVersion`, `Empty`, `TooManyStates`,
`EmptyName`, `DuplicateName`, `UnknownState`, `MultipleParents`, `Unreachable`, `UnknownTask`, `NoNextSibling`,
`ConditionTooDeep`, `BadValue`, `IncludeMissing`, `IncludeCycle`, `IncludeDepth`, `HoldOnEvent`, `ParentOfRoot`) and
`StateTreeDef::from_ron` adds `Parse`. A bad definition is never ticked. On the protocol the reasons are
`parse`, `unsupported_version`, `empty`, `too_many_states`, `empty_name`, `duplicate_name`, `unknown_state`,
`multiple_parents`, `unreachable`, `unknown_task`, `no_next_sibling`, `condition_too_deep`, `bad_value`,
`include_missing`, `include_cycle`, `include_depth`, `hold_on_event`, `parent_of_root` and
`bad_behavior_tree`.

A task path `bt:<file>.bt.ron` runs that behavior tree until it succeeds or fails, and the state completes
with the same result (ADR-4543). The file is loaded by the same include loader. A missing file, a cycle or
a nest deeper than four is `bad_behavior_tree`. A behavior-tree `state_tree` node runs a `.statetree.ron`
the other way; see [AI](ai.md).

## Limits

At most 4096 states, 64 queued events and 64 trace lines per tree, conditions nested at most 16 deep. A running tree
takes no heap allocation per tick while nothing happens: its status is updated in place, and trace lines are only
built when one was added. Your own tasks are not counted in that.

## In the editor

The State Trees panel shows every tree of a running game live: the active path, each state's transitions, the blackboard and
the trace, with pause, send event and reload buttons. `statetree.*` reach the world that serves the agent, which in the editor is
the edit world; `editor.state_tree_panel_*` reach the play world. See [gameplay panels](editor-gameplay-panels.md).

## Reload from the file on disk

An agent entity that carries a `StateTreeSource { path }` next to its `StateTree` follows its `.statetree.ron`:
when the asset watcher reports the file changed (a `SourceAssetChanged` message), `AiPlugin` reads and compiles
it in `Update` and queues the new tree. The next fixed step swaps it in before that agent's tree ticks, so a
reload never lands in the middle of a tick, and the swap rules are those of `statetree.reload` (the active path,
tick count and event queue carry over when every active state still exists in the new definition, otherwise the
tree restarts). A file that does not parse or compile is recorded and the running tree keeps ticking untouched.
`path` is project-relative (or absolute); `includes` resolve from the project root. A file the tree embeds reloads
it too (ADR-8593): an `includes` entry, a `bt:<path>.bt.ron` task's behavior tree and a state tree that behavior
tree runs. Each compile remembers the files it read; when a changed file is not an agent's own file, every agent file
whose embedded files are not known yet is compiled once to learn them (`embed_scans`).

The `StateTreeReloads` resource holds what is queued (`pending()`), the counters `applied` and `rejected`, and the
last 32 records (`log()`: the entity, the file and either `Kept` / `Restarted` or the reason the file was
rejected). A tree whose agent is parked waits in the queue until the agent ticks again. Nothing new runs in
`FixedUpdate`: with an empty queue the tick does one resource lookup per pass.

A changed file is read and compiled once however many agents use it; the other agents get trees built from the
compiled tree's merged definition with the same file reads (`bt:` tasks included; `file_compiles`, `instances`), and
a report after which the file and every file it embeds read as at the last compile is skipped (`unchanged`). Authoring trees (the State Tree Editor panel,
`ai.statetree_edit`, `ai.statetree_validate`, `editor.statetree_*`) is on the
[state tree editor](state-tree-editor.md) page (ADR-3342).

## Moving an agent: the `MoveTo` task

State trees run the same task types as behavior trees, so `regia_ai::MoveTo` (registered by `AiPlugin`, listed in the
state-tree editor's palette) is a state task too:

```ron
(name: "Go", tasks: ["regia_ai::MoveTo"], transitions: [
    (trigger: Succeeded, target: State("Guard")),
    (trigger: Failed, target: State("Search")),
]),
```

It reads `move_target`, `move_acceptance_radius` and `move_provider` from the blackboard and writes `move_to_state`,
`move_to_remaining`, `move_to_replans` and `move_to_error`, exactly as the leaf does (see "The `MoveTo` leaf" in
[Behavior tree editor](behavior-tree-editor.md)). The task succeeds inside the radius, so a `Succeeded` transition fires
on the tick the agent arrives; it fails with a lowercase code (`unreachable`, `partial_path`, `path_ended`, ...), which
a `Failed` transition can act on. Leaving the state resets the task: a queued plan is cancelled and a crowd target is
cleared. Which path provider to name: [Choosing a navigation stack](../user-guide/choosing-navigation.md).
