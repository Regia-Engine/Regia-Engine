# AI: behavior trees and the blackboard

Inspect running behavior trees through the generic scene calls (`regia_ai`, `docs/specs/22-tier2.md` § T2.2). Other decision
models have their own pages: [state trees](state-tree.md), [utility reasoners](utility-ai.md) and
[perception and GOAP](ai-perception.md). Groups (squads, attack tokens, formations) and the budget that lets far
agents tick less often are on [their own page](ai-squads.md).

## What an AI agent entity carries

| Component | What it is | Reflected |
|---|---|---|
| `BehaviorTreeDef` | The authored tree: a flat node arena plus a root index (`.bt.ron`). Only there when game code inserts it; the compiled `BehaviorTree` keeps its own copy. | yes |
| `BehaviorTreeStatus` | A snapshot of the last tick: the root status and one status per node. | yes |
| `Blackboard` | Sorted key/value store (bool, int, float, string, vec3, entity). | yes |
| `Perception` | Sight and hearing settings. | yes |

The compiled `BehaviorTree` itself is not reflected; `BehaviorTreeStatus` is its read-only view and is
rewritten every `FixedUpdate` tick. Statuses are `Success`, `Failure` and `Running`.

## Reading a running tree

```json
scene.query_entities { "component": "BehaviorTreeStatus" }
scene.get_entity { "entity": 4294967296, "components": ["BehaviorTreeStatus", "Blackboard"] }
→ { "components": { "BehaviorTreeStatus": { "value": { "last_status": "Running",
      "node_status": ["Running", "Failure", "Running"] } }, "Blackboard": { "value": { ... } } } }
```

`node_status[i]` belongs to `BehaviorTreeDef.nodes[i]`, so read the definition alongside it to name the node
(`ai.bt_live` returns each node's name, kind and status in one call, from the compiled tree's own copy).
Nodes the last tick did not reach keep their previous status.

## Steering an agent

Write blackboard keys with `scene.set_property` (`"component": "Blackboard"`, a path into `values`). A
`Condition` node succeeds while its key is the boolean `true`; a missing or non-boolean key counts as
`false`. Use `play.wait_until` / `expect` on `BehaviorTreeStatus.last_status` to wait for a behavior.

## Compile errors

A tree is compiled from its definition when you build it (`BehaviorTree::compile`). A bad definition is
rejected, never ticked:

- an unregistered leaf or service task type path: `UnknownTask`;
- an empty arena, a root or child index outside the arena, a decorator without exactly one child, a `Condition`
  with children, a node reachable twice (child links that form a cycle, or a node with two parents), or a `Cooldown`
  or `Timeout` whose `seconds` is negative or not finite: `Malformed`.

## Building and running a tree (game code)

`AiPlugin` ticks trees; game code builds them:

1. A leaf is a Rust type that implements `BtTask`: `tick(&mut self, ctx)` returns a `BtStatus`, and the optional
   `reset` clears the task's own state when a `Repeat` starts another run or a `Timeout` or a finished `Parallel`
   aborts the subtree. `#[derive(BtTask)]` (the type also needs `Default`) registers it under its full type path
   (`std::any::type_name`), which is the `task` string a leaf or service node names.
2. Register it: `world.resource_mut::<BtTaskRegistry>().register::<Patrol>()`. A leaf with settings goes in with
   `register_factory(path, || Box::new(...))`, which builds a fresh instance each time a tree compiles.
3. Author the definition as a `.bt.ron` file (format: [`docs/formats/bt-ron.md`](../../docs/formats/bt-ron.md)) or in
   code with `BehaviorTreeBuilder` (`selector`, `sequence`, `parallel`, `inverter`, `condition`, `cooldown`,
   `repeat`, `timeout`, `service`, `leaf`, each returning the node index, then `build(root)`).
4. `BehaviorTree::compile(&def, &registry)` and insert the result on the agent entity with a `Blackboard`.

Each `FixedUpdate`, in the `AiStep` set (ordered after `PhysicsStep`), `AiPlugin` ticks every `Abilities` cooldown,
then every `BehaviorTree`, then every state tree. Trees tick in the order their `BehaviorTree` arrived in the world,
and an entity with `LayerParked` is skipped. The tick is exclusive: `BtContext` hands a leaf `world`, `entity`,
`dt` (the fixed step) and `blackboard()` / `blackboard_mut()`. A `Service` runs its task every tick, ignores the
task's status and returns its child's. Order `AiStep` against your own steps, for example `(AiStep, NavStep).chain()`
when trees plan nav paths. A worked example is in the
[navigation and AI guide](../user-guide/navigation-ai.md#behavior-trees).

## Tuning notes

- `Cooldown` waits `seconds` of simulated time (counted in fixed-step `dt`) after its child returns `Success`; while
  it is cooling it returns `Failure`. A child that returns `Failure` or `Running` does not start the wait, so it is
  ticked every step.
- `Timeout` fails (and resets its child) once the child has been running `seconds` without finishing.
- `Parallel` ticks every child that has not finished each tick; a child that returned `Success` or `Failure` is
  latched and not ticked again until the parallel itself finishes. `RequireAll` fails on any failure, `RequireOne` succeeds on any
  success.

## Abilities: cooldowns and costs

An AI agent may carry an `Abilities` component (`regia_ai::abilities`, ADR-1563): named actions gated by a cooldown
and a resource cost. `AiPlugin` ticks every cooldown once per fixed step, before the behavior trees. Read and edit
it with the same scene calls:

```json
scene.get_entity { "entity": 4294967296, "components": ["Abilities"] }
→ { "components": { "Abilities": { "value": { "slots": {
      "dash": { "def": { "id": "dash", "cooldown_secs": 2.0, "cost": 10.0 }, "remaining": 1.25, "activations": 3 }
    } } } } }
```

- `remaining` is the cooldown left in seconds (`0.0` = ready); `activations` counts successful activations.
- `scene.set_property` on `slots.<id>.remaining` with `0.0` makes an ability ready again (the same as
  `Abilities::cancel`); on `slots.<id>.def.cooldown_secs` or `def.cost` retunes it. Negative or non-finite values are
  read as `0.0` when the ability is used, so an activation never spends time on a NaN. The stored `cooldown_secs` and
  `cost` are not rewritten: `scene.get_entity` can still show a NaN a reflected edit wrote. A negative `remaining` is
  rewritten to `0.0` on the next ability tick; the authored cooldown is not.
- Wait for an ability with `play.wait_until` on `Abilities.slots.<id>.remaining` equal to `0.0`.

Activation is game code (a leaf task or script calls `Abilities::try_activate(id, &mut pool)`); it checks the cooldown
first, then the pool, and a rejected activation spends nothing. Gating by tags, blackboard keys or line of sight
stays with the caller.

## Editing trees and watching them run

The [behavior tree editor](behavior-tree-editor.md) page covers the editor panel and the `ai.bt_get`,
`ai.bt_edit`, `ai.bt_validate` and `ai.bt_live` methods: edit a `.bt.ron` without opening the editor, check it
with stable issue codes, and read each node's live status of a ticking tree.

## Value conditions and observer aborts

A `compare` node reads one blackboard key and relates it to a constant or to a second key. The operators are
`eq`, `ne`, `lt`, `le`, `gt`, `ge`, `is_set` and `in_range`. The value kind is `bool`, `int`, `float`, `tag`
or `entity`. Integers and floats are not coerced: a float stored under an `int` compare is false at runtime,
and a constant of the wrong kind is the validator error `type_mismatch` (the tree does not compile, and it
does not panic). `is_set` is true when the key is present, whatever it holds. `in_range` is inclusive and
only for `int` and `float`. A missing key, a non-finite float or a value of another type is false.

`condition`, `compare` and the decorators (`inverter`, `cooldown`, `repeat`, `timeout`, `service`) may set
`abort` to `none` (the default), `self`, `lower_priority` or `both`. `none` keeps the tree's memory: a
selector or sequence resumes the running child and does not re-check the branches it already passed.
`lower_priority` re-checks when a watched key changes and, if the check passes, aborts the lower-priority
sibling the selector is running. `self` aborts this node's own running branch when the check fails. `both`
does both. The abort calls the task's `reset`. The tree re-checks an observer only when one of its keys
changed (`Blackboard::insert` and `remove` record a real change; a direct write of `values` is caught by
the snapshot on the next tick). A tick where nothing watched changed does not evaluate the observers. A
tree with no `abort` set does not read that change list. The dispatch span is `bt_observer_dispatch`.

An aborted decorator resets its own service task and timeout or repeat state as well as its child.
Overlapping observers call each task's `reset` once per dispatch.

A decorator with `abort` set is a gate on its key (ADR-7559). Each time it is entered from rest (its parent
starts it, or starts it again after it finished or was aborted) the gate is checked first, in every mode; a shut
gate fails at once and does not tick its child or its service task, or run its cooldown or timeout clock. While
the decorator runs, the gate is re-checked only when its key changes: `self` (and `both`) then abort the running
branch if the gate is shut, `lower_priority` alone lets it run on and checks again on the next entry. So after a
`self` abort the branch stays out until the key passes again. A gate whose child already succeeded or failed is
not running and has nothing to abort; a sequence's later child keeps running, and the gate is checked when the
sequence starts over. `lower_priority` (and `both`) on a gate in an earlier selector branch aborts the running lower
branch when the gate opens, whether the gate's own child ran before or not; a selector that is not running has
nothing to abort, and the change takes effect when it starts again. A `condition` or `compare` is evaluated on
every visit anyway, and with `self` in a sequence it aborts the running later child.

A tree is at most 256 nodes deep on any path from the root (`MAX_BT_DEPTH`): a deeper tree does not compile and is
the validator error `too_deep`. The count spans bridges: a bridge task's tick runs the inner tree on top of the outer
tree's recursion frames, so the heights of every behavior tree in a BT → ST → BT chain must sum to at most 256.

## Bridges

A `state_tree` node runs a `.statetree.ron` until that tree succeeds or fails, and returns success or
failure for those two results. A state-tree task whose path is `bt:<file>.bt.ron` runs that behavior tree
the same way. Nesting stops at four bridges, and a path may not appear twice (`bridge_cycle`,
`bridge_depth`). A missing asset is a compile error. The path shape is checked by `ai.bt_validate`
(`empty_state_tree`, `bad_state_tree`, `state_tree_has_children`). On a state tree the same failure is
`bad_behavior_tree`. See the [state tree](state-tree.md) page.

## Seeing AI in play

ADR-8463. While a game is running, the `ai_debug` gizmo group draws what each agent is doing in the
world. The group stays off until `debug.set_gizmos` turns it on:

```json
{ "method": "debug.set_gizmos", "params": { "group": "ai_debug", "enabled": true } }
```

`ai.debug_layers` reads the overlay. `ai.debug_layers_set` replaces only the fields the call names.
Neither method enables the gizmo group. Layers start off. The filter starts as `all`, the radius as
20 metres, and the label cap as 64. A call with `{}` reads the current settings, or writes nothing
and still succeeds.

`layers` is the whole set. Names left out are off. An empty array turns every layer off. The names
are `bt` (the running behavior-tree leaf, as text above the agent), `state_tree` (the active state),
`goap` (the current plan step, the goal facts, and a cross), `squad` (a line from the leader to each
other member, and a role letter), `formation` (a cross on each slot and a line to its member),
`move_to` (the MoveTo target, the nav target when it differs, and the waypoints still ahead), and
`blackboard` (one line per watched key). `watch_keys` is that list, at most 32 keys of 64 bytes.
An empty array clears it.

`filter` is `all`, `selected`, or `radius`. `selected` keeps the entity id in `selected` (null clears
it). `radius` keeps agents within `radius` metres of that entity, or of the stored focus when the
entity has no pose. `label_cap` is how many text labels one frame may draw. Further labels increment
`dropped` and are not drawn. Shapes do not use the cap. `dropped` is the count from the last frame.

```json
{ "method": "ai.debug_layers_set", "params": { "layers": ["bt", "squad"], "filter": "radius", "radius": 12.5 } }
```

The result is `{ layers, filter, radius, label_cap, dropped, watch_keys, selected }`. `layers` lists
the enabled names in the order above. `selected` is the entity id, or null.

Errors, both `invalid_params`: `unknown_layer` (a layer name is not one of the seven; nothing is
written) and `bad_filter` (filter is empty or not `all`, `selected`, or `radius`; nothing is
written). A present field of the wrong JSON type is `F0002` and writes nothing. A negative or
non-finite radius is `F0002`.

The viewport Show menu has one row per layer: `ai_bt`, `ai_state_tree`, `ai_goap`, `ai_squad`,
`ai_formation`, `ai_move_to`, and `ai_blackboard` on `editor.set_viewport`. Turning any of them on
also enables the `ai_debug` group. Clearing the last one turns that group off. Game view leaves
these seven as they are. See [Viewport Show flags](viewport-flags.md).
