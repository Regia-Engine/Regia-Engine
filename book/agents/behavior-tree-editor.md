# Behavior tree editor

Behavior trees are authored as `.bt.ron` files (a flat node arena plus a root index, see
`docs/formats/bt-ron.md`). The editor's **Behavior Tree** panel shows one as an indented outline with a palette
of node kinds and registered leaf tasks, an inspector for the selected node's parameters, the validation issues,
and, while the game plays, each node's last status with the running nodes highlighted. The `ai.bt_*` methods
read, edit, check and watch trees without the editor; the `editor.bt_*` methods drive the open panel. Edits are
the same operations in both places (ADR-2408). `.bt.ron` is version 2 (ADR-4543): a v1 file has no
`format_version` and still loads; `regia upgrade-project` stamps `format_version: 2` and keeps every other byte.

## Operations

`ai.bt_edit` and `editor.bt_edit` take a list of `ops`, applied in order, all or nothing: the first op that does
not fit the tree leaves the tree as it was and names its position and reason.

| `op` | Fields | Does |
|---|---|---|
| `add_node` | `kind`, `name?`, `parent?`, `index?`, `task?`, `key?`, `seconds?`, `times?`, `policy?`, `abort?`, `compare?`, `value_kind?`, `literal?`, `literal_hi?`, `rhs_key?` | Adds a node. With a `parent` it is connected there (at `index`, default last). With no parent and an empty tree it becomes the root; otherwise it stays unconnected. Answers the new node's index in `created`. |
| `remove_node` | `node`, `subtree?` | Removes the node and renumbers the arena. `subtree: true` takes its descendants too; otherwise its children stay as unconnected nodes. If the root goes, node 0 becomes the root. |
| `connect` | `parent`, `child`, `index?` | Puts `child` under `parent`, moving it when it already has a parent. |
| `disconnect` | `parent`, `child` | Cuts the edge; the child stays as an unconnected node. |
| `reorder` | `parent`, `from`, `to` | Moves a child within its parent's list. |
| `set_node` | `node`, `name?`, `kind?`, `key?`, `seconds?`, `times?`, `clear_times?`, `task?`, `policy?`, `abort?`, `compare?`, `value_kind?`, `literal?`, `literal_hi?`, `clear_literal?`, `clear_literal_hi?`, `rhs_key?` | Changes the given fields. `clear_times` makes a repeat run for ever. `clear_literal` and `clear_literal_hi` drop a compare constant. |
| `set_root` | `node` | Makes `node` the root. |

Node kinds are `selector`, `sequence`, `parallel`, `inverter`, `condition`, `compare`, `cooldown`, `repeat`,
`timeout`, `service`, `state_tree` and `leaf`. Decorators (`inverter`, `cooldown`, `repeat`, `timeout`,
`service`) take exactly one child. `leaf`, `condition`, `compare` and `state_tree` take none. `compare`
stores `op`, `value_kind`, `literal` or `rhs_key`, and `literal_hi` for `in_range`. On `add_node` and `set_node` that operator is the field `compare`, because `op` already names the edit. `state_tree` stores the
`.statetree.ron` path in `task`. `abort` is `none`, `self`, `lower_priority` or `both`.

## Files: `ai.bt_get`, `ai.bt_edit`, `ai.bt_validate`

Paths are project-relative, end in `.bt.ron` and may not contain `..`.

```json
{ "method": "ai.bt_edit", "params": { "path": "assets/ai/guard.bt.ron", "create": true, "ops": [
    { "op": "add_node", "kind": "selector", "name": "root" },
    { "op": "add_node", "kind": "leaf", "name": "patrol", "parent": 0, "task": "game::Patrol" },
    { "op": "add_node", "kind": "condition", "name": "sees", "parent": 0, "key": "enemy_visible", "index": 0 } ] } }
```

`ai.bt_edit` saves the canonical text (one line per node, the layout in the format page) unless `save: false`. A
tree with validation errors is saved too, so a tree can be built up through invalid states; the answer carries
`valid` and the `issues`. `create: true` starts an empty tree when the file does not exist. `ai.bt_get { path }`
answers `{ path, root, nodes, valid, tasks_checked, issues, ron }`; each node carries its `index`, `name`, `kind`,
`children`, `key`, `seconds`, `times`, `task`, `policy`, and, when set, `abort`, `op`, `value_kind`,
`literal`, `literal_hi` and `rhs_key`. `ai.bt_validate { path | text }` answers
`{ ok, errors, warnings, tasks_checked, issues }` and takes RON text too.

Leaf and service task paths are checked against the registered tasks when any task is registered
(`tasks_checked: true`); with none registered (an editor with no game loaded) the check is skipped.

### Issue codes

Each issue is `{ code, severity, node, message }`. Errors stop the tree from compiling; warnings do not.

| Code | Severity | Meaning |
|---|---|---|
| `empty_tree` | error | The tree has no nodes. |
| `root_out_of_range` | error | The root index is outside the arena. |
| `child_out_of_range` | error | A child index is outside the arena. |
| `condition_has_children` | error | A condition has children. |
| `leaf_has_children` | error | A leaf has children. |
| `decorator_arity` | error | A decorator does not have exactly one child. |
| `bad_seconds` | error | A cooldown or timeout has a negative or non-finite `seconds`. |
| `empty_key` | error | A condition has no blackboard key. |
| `empty_task` | error | A leaf or service has no task path. |
| `unknown_task` | error | The task path is not registered. |
| `shared_node` | error | A node is reachable twice (two parents, or a cycle). |
| `cycle` | error | A cycle in the arena. |
| `empty_composite` | warning | A selector, sequence or parallel has no children. |
| `unreachable_node` | warning | A node cannot be reached from the root. |
| `duplicate_name` | warning | Two nodes share a name. |
| `type_mismatch` | error | A compare constant or operator does not match `value_kind`. |
| `bad_compare` | error | A compare is missing a side, has both a constant and a second key, or has a non-finite or unordered range. |
| `compare_has_children` | error | A compare has children. |
| `abort_not_allowed` | error | `abort` is set on a selector, sequence, parallel, leaf or state_tree. |
| `abort_without_key` | error | `abort` is set and the node has no blackboard key. |
| `empty_state_tree` | error | A state_tree node has no path. |
| `bad_state_tree` | error | A state_tree path does not end in `.statetree.ron`. |
| `state_tree_has_children` | error | A state_tree node has children. |
| `too_deep` | error | A path from the root holds more than 256 nodes (`MAX_BT_DEPTH`); reported once, at the first node past the cap. The compile-time count also spans `state_tree` bridge chains (the validator checks one file). |

### Edit reasons

Failures are `F0002` with a lowercase `data.reason`: `missing_param`, `bad_path`, `not_found`, `parse_failed`,
`bad_ops`, `too_many_ops` (more than 512), `write_failed`, `bad_value` and the failing op's own reason:
`bad_node`, `bad_root`, `no_children_allowed`, `decorator_full`, `would_cycle`, `not_a_child`, `bad_index`,
`bad_field`, `too_many_nodes` (a tree holds at most 4096 nodes).

## Live state: `ai.bt_live`

```json
{ "method": "ai.bt_live", "params": { "entity": 4294967296 } }
```

The first call attaches a read-only debugger to the entity; from then on every tick is recorded. The answer is
`{ entity, attached, newly_attached, ticks, root, last_status, running, nodes, move_to }`, with each node's `status`
(`success`, `failure`, `running`), `running` and `changed_at` (the recorded tick on which its status last
changed). `move_to` lists the agent's `MoveTo` runs (see "The `MoveTo` leaf" below), one object per progress key:
`{ key, provider, state, crowd, target, remaining, waypoint, waypoints, replans, ticks, error }`; `[]` when the tree has
no `MoveTo` leaf or none has ticked. Before the first recorded tick it shows what the tree last reported on its own. `detach: true` stops
recording (`{ attached: false, was_attached }`). Reasons: `missing_param`, `entity_not_found`, `no_behavior_tree`.
Attaching changes what the tick records, so the method needs the `play` permission and counts as a mutating call
(ADR-3285).

The debugger is a resource that exists only once something attaches. With none attached the tick does one
resource lookup per pass and records nothing; it never feeds back into the simulation. Without calling the
method, `BehaviorTreeStatus` (see [AI](ai.md)) still holds the last status of every node.

## The editor: `editor.bt_*`

| Method | Does |
|---|---|
| `editor.bt_open { path?, create? }` | Opens a `.bt.ron`, or starts a new tree. Answers `{ path, nodes, valid, revision, discarded_unsaved }`. |
| `editor.bt_state { palette?, ron? }` | The open tree: nodes with their `parent`, `issues`, `selected`, `agents`, the `live` view while playing, the palette, the canonical text. |
| `editor.bt_edit { ops?, select?, label? }` | One undoable edit (the whole `ops` list is one undo step); `select` picks a node (-1 clears). A new node becomes the selection. |
| `editor.bt_save { path? }` | Saves the canonical text, atomically. |

Paths follow the other editor panels: project-relative or absolute under the project, the temp directory or the
working directory (`path_outside_project` otherwise), and the name must end in `.bt.ron` (`bad_path`). Reasons:
`no_tree_open`, `open_failed`, `save_failed`, plus those of `ai.bt_edit`. Every edit is on the editor's undo
stack, so `editor.undo` reverts it and Ctrl+Z in the panel does too. While the game plays the panel lists the
agents that have a tree, watches one (`live.entity`) and colors the outline: running nodes in the active color,
successes and failures in theirs. If the running tree differs from the open document the statuses are not drawn
on the outline (`live.matches_document` is false).

## Panel

The toolbar has New, Open, Save, Save as, Undo and Redo. The palette adds a node under the selected node (or as
the root of an empty tree, or unconnected); each registered leaf task has its own button. The inspector edits the
name, the blackboard key, the seconds, the repeat count, the parallel policy and the task, moves the node up or
down among its siblings, disconnects, re-roots, removes it (with or without its children) and connects it under
another node. Issues are listed at the bottom; clicking one selects its node.

<!-- depth-2:method-reference -->

## Call reference

Parameters, results and error codes for every method on this page. Types and defaults come from the schema and handlers; limits include the contract. Error codes are in `data.code`; names explicitly labeled `data.reason` are additional handler reasons. Every method can also return `F0001`, `F0002`, `F0004`, `F0007` and `F0012`. The server can return `F0005`, `F0006`, `request_too_large`, or `F0008` when the method is not registered; see [protocol](protocol.md).

Unless a lower or explicit method limit is listed, the dispatcher accepts at most 65,536 items in a top-level array parameter and 1,048,576 UTF-8 bytes in a string parameter. A limit violation returns `F0002`.

The request in each fence is the call that was sent with `regia agent call`.
The line under it is the host's answer. Absolute project paths and messages containing them are elided with `... /* comment */`.
A second fence tagged `json,ignore` is an error answer. Large fields are elided with `...`. A fence tagged `json,ignore` is a real call on a host or plugin method that the runtime schema does not list.


### `ai.bt_edit`

Applies structural edits to a .bt.ron all or nothing and saves the canonical text. Ops: add_node { kind, name?, parent?, index?, task?, key?, seconds?, times?, policy? }, remove_node { node, subtree? }, connect { parent, child, index? }, disconnect { parent, child }, reorder { parent, from, to }, set_node { node, name?, kind?, key?, seconds?, times?, clear_times?, task?, policy? }, set_root { node }. A tree with validation errors is still saved; the answer lists the issues.

*EditScene, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `path` | string | yes | Project-relative path of a .bt.ron. |
| `ops` | array | yes | The edit operations, applied in order (at most 512). |
| `create` | boolean | no | Start an empty tree when the file does not exist. |
| `save` | boolean | no | Write the file (default true). |

`ops` has at most 512 items.

Result: `{ path, root, nodes, valid, tasks_checked, issues, saved, created, ron }`.

Errors: `F0002`. `data.reason`: `missing_param`, `bad_path`, `not_found`, `parse_failed`, `too_many_ops`, `bad_ops`, `bad_value`, `write_failed`, `bad_node`, `no_children_allowed`, `decorator_full`, `bad_index`, `too_many_nodes`, `would_cycle`, `not_a_child`, `bad_field`, `bad_root`.

```json
ai.bt_edit {"path": "assets/ai/guard.bt.ron", "create": true, "ops": [{"op": "add_node", "kind": "selector", "name": "root"}, {"op": "add_node", "kind": "leaf", "parent": 0, "task": "game::Patrol"}]}
→ {
  "created": [
    0,
    1
  ],
  "issues": [],
  "nodes": [
    {
      "children": [
        1
      ],
      "index": 0,
      "key": "",
      "kind": "selector",
      "name": "root",
      "policy": "require_all",
      "seconds": 0.0,
      "task": "",
      "times": null
    },
    {
      "children": [],
      "index": 1,
      "key": "",
      "kind": "leaf",
      "name": "leaf_1",
      "policy": "require_all",
      "seconds": 0.0,
      "task": "game::Patrol",
      "times": null
    }
  ],
  "path": "assets/ai/guard.bt.ron",
  "ron": ...,
  "root": 0,
  "saved": true,
  "tasks_checked": false,
  "valid": true,
  ...
}
```

Elided: `ron` (string 290 chars).

<!-- capture:ai.bt_edit -->

### `ai.bt_get`

Reads a .bt.ron: its nodes (index, name, kind, children, parameters), validation issues with stable codes and the canonical text.

*Read, does not mutate.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `path` | string | yes | Project-relative path of a .bt.ron (no `..`). |

Result: `{ path, root, nodes, valid, tasks_checked, issues, ron }`.

Errors: `F0002`. `data.reason`: `missing_param`, `bad_path`, `not_found`, `parse_failed`.

```json
ai.bt_get {"path": "assets/ai/guard.bt.ron"}
→ {
  "issues": [],
  "nodes": [
    {
      "children": [
        1
      ],
      "index": 0,
      "key": "",
      "kind": "selector",
      "name": "root",
      "policy": "require_all",
      "seconds": 0.0,
      "task": "",
      "times": null
    },
    {
      "children": [],
      "index": 1,
      "key": "",
      "kind": "leaf",
      "name": "leaf_1",
      "policy": "require_all",
      "seconds": 0.0,
      "task": "game::Patrol",
      "times": null
    }
  ],
  "path": "assets/ai/guard.bt.ron",
  "ron": ...,
  "root": 0,
  "tasks_checked": false,
  "valid": true,
  ...
}
```

Elided: `ron` (string 290 chars).

<!-- capture:ai.bt_get -->

### `ai.bt_live`

The per-node state of a ticking behavior tree: each node's last status, whether it is running, and the recorded tick on which it last changed. The first call attaches a read-only debugger to the entity; detach: true removes it.

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `entity` | number | yes | The agent entity carrying a BehaviorTree. |
| `detach` | boolean | no | Stop recording this entity. |

`detach` defaults to false. A detached reply has no node snapshot; it reports whether a watch existed in `was_attached`.

Result: `{ entity, attached, newly_attached, ticks, root, last_status, running, nodes, move_to: [{ key, provider, state, crowd, target, remaining, waypoint, waypoints, replans, ticks, error }] }`; with `detach: true`, `{ entity, attached: false, was_attached }`.

Errors: `F0002`. `data.reason`: `missing_param`, `entity_not_found`, `no_behavior_tree`.

```json
ai.bt_live {"entity": 4294967296}
```

```json,ignore
→ {
  "message": "no such entity",
  "code": -32602,
  "data": {
    "code": "F0002",
    "hint": "use scene.query_entities with BehaviorTreeStatus to find the agent",
    "reason": "entity_not_found"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:ai.bt_live -->

### `ai.bt_validate`

Lists every problem of a .bt.ron (or of RON `text`) as { code, severity, node, message }. Leaf and service task paths are checked against the registered tasks when any are registered.

*Read, does not mutate.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `path` | string | no | Project-relative path of a .bt.ron (or give `text`). |
| `text` | string | no | The tree as RON text (or give `path`). |

Give `text` or `path`; when text is present it takes precedence and the result’s `path` is null. An omitted text requires a path.

Result: `{ path, ok, errors, warnings, tasks_checked, issues }`.

Errors: `F0002`. `data.reason`: `missing_param`, `bad_path`, `not_found`, `parse_failed`.

```json
ai.bt_validate {"path": "assets/ai/guard.bt.ron"}
→ { "errors": 0, "issues": [], "ok": true, "path": "assets/ai/guard.bt.ron", "tasks_checked": false, "warnings": 0 }
```

<!-- capture:ai.bt_validate -->

### `editor.bt_edit`

One undoable edit of the open tree: `ops` as for ai.bt_edit (add_node, remove_node, connect, disconnect, reorder, set_node, set_root), applied all or nothing. `select` picks a node.

*EditScene, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `ops` | array | no | The operations, applied in order (at most 512). |
| `select` | number | no | Select this node (-1 clears the selection). |
| `label` | string | no | The undo entry's label. |

Omitted `ops` means no operations; omitted `select` keeps the selection (new nodes may become selected). At least one operation or a selection is required. A negative selection clears it. The default undo label is `edit`.

Result: `{ created, revision, valid, issues, selected }`.

Errors: `F0002`. `data.reason`: `no_tree_open`, `bad_ops`, `too_many_ops`, `missing_param`, `bad_node`, `no_children_allowed`, `decorator_full`, `bad_index`, `too_many_nodes`, `would_cycle`, `not_a_child`, `bad_field`, `bad_root`.

Ops match `ai.bt_edit` and apply all or nothing. At most 512 ops (`too_many_ops`). `select: -1` clears the selection.

```json
editor.bt_edit {"label": "guide", "ops": [{"op": "add_node", "kind": "selector", "name": "root"}, {"op": "add_node", "kind": "leaf", "name": "idle", "parent": 0, "task": "game::Idle"}]}
→ { "created": [ 0, 1 ], "issues": [], "revision": 2, "selected": 1, "valid": true }
```

<!-- capture:editor.bt_edit -->

### `editor.bt_open`

Open a .bt.ron in the behavior tree editor, or start a new tree (create: true).

*EditScene, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `path` | string | no | Project-relative or absolute .bt.ron path. |
| `create` | boolean | no | Start an empty tree (saved to `path` later). |

`create` defaults to false, which requires `path`. With `create: true`, an omitted path starts an untitled tree that needs a path when saved.

Result: `{ path, nodes, valid, revision, discarded_unsaved }`.

Errors besides the universal codes: `F0002`, `F0003`. `data.reason`: `bad_path`, `missing_param`, `open_failed`, `path_outside_project`.

`create: true` starts an empty tree. Without it, `path` is required (`missing_param`) and a missing file is `F0003`.

```json
editor.bt_open {"create": true, "path": "assets/ai/guide-depth.bt.ron"}
→ { "discarded_unsaved": false, "nodes": 0, "path": ... /* capture project path or message containing it */, "revision": 1, "valid": false }
```

<!-- capture:editor.bt_open -->

### `editor.bt_save`

Save the open tree as canonical .bt.ron (to `path`, or where it was opened). A tree with errors is saved too.

*EditScene, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `path` | string | no | Save as this path. |

An omitted path saves to the open tree’s path; an untitled tree requires a path.

Result: `{ path, valid }`.

Errors besides the universal codes: `F0002`. `data.reason`: `no_tree_open`, `bad_path`, `save_failed`, `path_outside_project`.

```json
editor.bt_save {}
→ { "path": ... /* capture project path or message containing it */, "valid": true }
```

<!-- capture:editor.bt_save -->

### `editor.bt_state`

The open tree: nodes (index, name, kind, parameters, children, parent), validation issues with codes, the selection, the live state while the game plays, and optionally the palette and the canonical text.

*Read, does not mutate.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `palette` | boolean | no | Include the node kinds and the registered leaf tasks. |
| `ron` | boolean | no | Include the .bt.ron text it would save. |

`palette` and `ron` default to false, so their corresponding result fields are omitted.

Result: `{ path, dirty, revision, root, nodes, valid, issues, selected, agents, live, palette?, ron? }`.

Errors besides the universal codes: `F0002`. `data.reason`: `no_tree_open`.

```json
editor.bt_state {"palette": true}
→ {
  "agents": [],
  "dirty": true,
  "issues": [],
  "live": null,
  "nodes": [
    {
      "children": [
        1
      ],
      "index": 0,
      "key": "",
      "kind": "selector",
      "name": "root",
      "parent": null,
      "policy": "require_all",
      "seconds": 0.0,
      "task": "",
      "times": null
    },
    {
      "children": [],
      "index": 1,
      "key": "",
      "kind": "leaf",
      "name": "idle",
      "parent": 0,
      "policy": "require_all",
      "seconds": 0.0,
      "task": "game::Idle",
      "times": null
    }
  ],
  "palette": {
    "kinds": [
      "selector",
      "sequence",
      ...
    ],
    "tasks": []
  },
  "path": ... /* capture project path or message containing it */,
  "revision": 2,
  "root": 0,
  "selected": 1,
  "valid": true,
  ...
}
```

Elided: `palette.kinds` (array 10).

<!-- capture:editor.bt_state -->

## Workflow: build a small tree and save it

1. `editor.bt_open` with `create` true and a `path` ending in `.bt.ron`. `bad_path` refuses any other suffix. `path_outside_project` refuses a path outside the allowed roots.
2. `editor.bt_state` with palette true to discover node kinds and registered task paths. If the task list is empty, a built-in wait node works without a game task. `no_tree_open` means step 1 did not happen.
3. `editor.bt_edit` with ops: add_node a selector, add_node a wait node with seconds 0.25, and connect it under the selector (or use parent 0 when adding it). For a custom leaf, choose its task from the palette. Check the returned valid flag and issues. More than 512 ops is too_many_ops; a cycle is would_cycle and the operation batch changes nothing.
4. `editor.bt_save`. The file is written even when `valid` is false.
5. `ai.bt_get` on that path, then `ai.bt_validate`. `ai.bt_edit` is the file-level edit (it does not need the panel open). `ai.bt_live` reads a running agent.

<!-- /depth-2:method-reference -->

## The `MoveTo` leaf

`AiPlugin` registers `regia_ai::MoveTo` in the `BtTaskRegistry`, so the editor's palette lists it with the other leaf
tasks (`editor.bt_state { palette: true }` returns it under `tasks`). Add it like any leaf:

```json
{ "op": "add_node", "kind": "leaf", "name": "go", "parent": 0, "task": "regia_ai::MoveTo" }
```

| Blackboard key | Read or written | Meaning |
|---|---|---|
| `move_target` | read | A vec3, or an entity whose position is the target |
| `move_acceptance_radius` | read | Success distance (m, float; 0.5 without it) |
| `move_provider` | read | The path provider name (`navmesh` without it; `det_grid`, `det_navmesh` or a game's own) |
| `move_to_state` | written | `idle`, `pending`, `following`, `crowd`, `arrived`, `failed` |
| `move_to_remaining` | written | Meters left along the path |
| `move_to_replans` | written | Re-plans since the run started (the target moved more than 1 m) |
| `move_to_error` | written | While failed: `no_target`, `no_position`, `unknown_provider`, `no_nav_data`, `start_off_nav`, `goal_off_nav`, `unreachable`, `invalid_input`, `unknown_ticket`, `partial_path`, `path_ended`, `crowd_refused`, `crowd_agent_missing` |

The leaf succeeds within the radius and fails with one of those codes. A game registers configured copies with
`MoveTo::new(target, radius).with_provider(name).register(registry, "game::Path")`; their keys use the leaf's progress
key (`move_to` by default). Which provider to pick: [Choosing a navigation stack](../user-guide/choosing-navigation.md).
To watch one, call `ai.bt_live` on the agent and read `move_to`.
