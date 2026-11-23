# Gameplay panels: state trees, smart objects, cameras

Three editor tabs debug gameplay systems while a game runs, and an agent reads and drives each one through `editor.*` methods
(ADR-2030). The panel and its methods share one model, so what an agent sets is what the panel shows.

| Tab | What it shows | Methods |
|---|---|---|
| **State Trees** | Every `StateTree`: status and active path. For the selected one: the state hierarchy, each state's transitions (holding, last taken), the blackboard, the trace. | `editor.state_tree_panel_*` |
| **Smart Objects** | Every `SmartObject` and `SmartObjectSlot`: claim state, holder, priority, off-reasons, definition, recent messages. Markers in the viewport. | `editor.smart_object_panel_*` |
| **Cameras** | The asset page of each `.camera.ron`, its rig graph, and the drivers of a running game. | `editor.camera_panel_*`, `editor.camera_rig_graph` |

## Which world they look at

The agent server runs on the edit world; a running game is in the play world, a separate id space. While nothing plays, these
methods read and act on the edit world at once. While a session plays:

- `_get` answers from a snapshot the editor loop copies from the play world each frame. `source` says `play`, and `snapshots`
  counts up, so you can tell a new snapshot from an old one.
- A call that changes something answers `{ "queued": true }` and runs on the play world on the next frame. Its outcome is
  `last_call` in the next `_get`: `{ method, source, ok, answer, reason, message }`.
- The trees, slots and drivers are the ones the game created. A play session with no game has none, and the Cameras panel says
  so: `play_framework` is false until the game adds `CameraPlugin`.
- Selections are dropped when play starts or stops, because an entity id of one world means nothing in the other.

```json
editor.play {}
editor.state_tree_panel_get {}
-> { "source": "play", "total": 1, "selected": 4294967299, "snapshots": 12,
     "trees": [{ "entity": 4294967299, "name": "Bot", "status": "running", "paused": false,
                 "active_path": ["Root", "Idle"], "ticks": 240 }],
     "detail": { "states": [ ... ], "blackboard": [{ "key": "fear", "type": "float", "value": 0.75 }],
                 "held": [], "trace": [{ "tick": 1, "kind": "enter", "text": "tick 1: enter Root" }] } }

editor.state_tree_panel_control { "op": "send_event", "event": "heard" }
-> { "queued": true, "hint": "..." }
editor.state_tree_panel_get {}
-> { ..., "last_call": { "method": "statetree.send_event", "source": "play", "ok": true, "answer": { "queued": true } } }
```

## State trees

| Method | Permission | What it does |
|---|---|---|
| `editor.state_tree_panel_get` | read | The list and the selected tree in full. The list holds 512 trees at most (`total` counts them all, `truncated` says so). |
| `editor.state_tree_panel_select` | play | `{ entity }`: the tree the detail shows. |
| `editor.state_tree_panel_control` | play | `{ entity?, op, ... }` on a tree, by default the selected one. |

`op` is one of:

| `op` | Parameters | What it does |
|---|---|---|
| `pause`, `resume`, `restart` | | As `statetree.control`. |
| `send_event` | `event` | Queues the event for the next tick. `answer.queued` is false when the queue is full. |
| `debug` | `disabled?`, `pause_on_enter?` | As `statetree.debug`: lists of state names to skip and to pause on. Development builds only. |
| `reload` | `path` | Reads a `.statetree.ron` relative to the project, expands its includes and hot-swaps it, as `statetree.reload`. |

Each state row has `name`, `depth`, `selection` (`first`, `utility`, `weighted`), `enter` (the condition as text, empty for always),
`tasks`, `active`, `disabled`, `pause_on_enter` and `transitions`. Each transition has `trigger` (`tick`, `event:<name>`, `succeeded`,
`failed`, `completed`), `target` (a state name, or `<next>`, `<next-enterable>`, `<parent>`, `<stay>`, `<succeed>`, `<fail>`),
`condition`, `priority`, `hold`, `holding` (its hold counter is running) and `last_taken` (the tick, as far back as the 64-line trace
reaches). A trace row's `kind` is `enter`, `exit`, `transition`, `rejected`, `event_ignored`, `event_dropped`, `start_failed` or `finished`.
A blackboard row's `type` is `bool`, `int`, `float`, `string`, `vec3` or `entity`.

## Smart objects

| Method | Permission | What it does |
|---|---|---|
| `editor.smart_object_panel_get` | read | Objects with their slots (slots with no object come last, under `entity: null`), `counts` by state, recent `events`. |
| `editor.smart_object_panel_set` | play | `{ selected?, show_gizmo? }`: the object or slot to mark, and the viewport markers. |
| `editor.smart_object_panel_control` | play | `{ entity?, op, ... }` on an object or slot, by default the selected one. |

`op` is `set_enabled` (`enabled`, `reason?`, default reason `editor_panel`; two different reasons need two enables, as in
`smartobject.set_enabled`) or `release`, which takes a slot's claim back from its holder. A slot's `state` is `free`, `claimed`, `in_use`
or `disabled`. The rows read the components directly, so the edit world, which runs no smart object systems, lists authored objects
too; `events` needs `SmartObjectPlugin` in that world. In the viewport a slot is a cross colored by state, larger when it or
its object is selected, with a line to its holder.

## Cameras

| Method | Permission | What it does |
|---|---|---|
| `editor.camera_panel_get` | read | `assets` with status, reads and file errors, `files`, the `page` of `selected_asset` (issues, parameters, variables, input slots, director), `drivers`, and `live`, the `camera.inspect` of `selected_driver`. |
| `editor.camera_panel_select` | play | `{ asset?, driver? }`. |
| `editor.camera_panel_load` | edit_scene | `{ path }` reads one file, relative to `assets/`. With no path, scans the project for files it has not read: `{ read, failures }`. |
| `editor.camera_rig_graph` | read | `{ asset? }`: the graph below. |
| `editor.camera_panel_edit` | edit_scene | `{ asset?, op, ... }`: one edit of the working copy, one undo step (see [Rig editor](#rig-editor)). The older `{ asset?, param, value }` (a parameter's default) and `{ asset?, rig, node, enabled }` (a `Group`'s switch) still work. The status becomes `stale`. |
| `editor.camera_panel_apply` | edit_scene | `{ asset? }`: validates the working copy; without errors it becomes the active version. |
| `editor.camera_panel_save` | edit_scene | `{ asset? }`: applies, then writes the file in canonical form and marks the asset's history saved. Comments in the old file do not survive. |
| `editor.camera_panel_driver` | play | `{ op, entity?, ... }`: `set_var`, `set_param`, `set_input`, `reset_input`, `push_overlay`, `add_additive`, `release_layer` or `hold_overlay`, with that `camera.*` method's parameters. |

The page holds the **edit world's** assets; no driver runs there, so editing cannot move a camera. The first time the tab is drawn it
scans the project. Saving is what reaches a running game: the file watcher reloads the file in both worlds, and `play_assets` shows the
revision the game runs. See [Camera framework](camera-framework.md) for what a reload does. A file that changes on disk replaces the working
copy, so edits you have not saved are lost; an edit that is only applied never reaches the file. A `Hook` director always validates in the edit
world, which has no game code to ask; the play world checks the name.

`editor.camera_rig_graph` answers:

```json
{ "asset": "player", "director": "rules: aim if aiming == true, else walk",
  "rigs": [{ "name": "walk", "tags": [], "director_choice": true, "blend_out": null, "live_weight": 1.0,
             "label_rect": [26.0, 0.0, 132.0, 22.0],
             "nodes": [{ "index": 0, "path": [0], "depth": 0, "kind": "boom", "summary": "length param len",
                         "switched_off": false, "rect": [26.0, 22.0, 132.0, 44.0], "live_skipped": false }] }],
  "edges": [{ "from": "walk", "to": "aim", "from_rigs": [0], "to_rigs": [1], "owner": "walk", "blend": "smooth",
              "seconds": 0.25, "start_facing": "own_seed", "hold_outgoing": false, "share_state": false,
              "interruptible": true, "conditions": [] }],
  "width": 420.0, "height": 300.0 }
```

A node's `index` is its place in evaluation order, nested nodes included, which is what `camera.inspect`'s trace calls `index`. Its
`path` addresses it in `editor.camera_panel_edit`. `live_weight` and `live_skipped` are null unless the selected driver runs this asset;
`live_skipped` is also null for a node that did not run this frame, such as one inside a `When` whose condition failed. `edges` are in the
order a driver searches them: the rigs' own lists, then the shared list (`owner: null`). In the panel, green boxes ran, red ones were
skipped, gray ones are switched off, and the arcs on the left are transitions (red for a cut, blue for a blend).

### Rig editor

The page edits the asset (ADR-7734): **Parameters**, **Rigs** (move, duplicate, remove, rename, add a node, add an own
transition, the rig's `tags` and `blend_out`) and **Transitions** (the shared list, then each rig's: move, duplicate, remove,
and every field), and on the **Rig graph** tab the clicked node's typed form with move, duplicate, remove, add after and add
inside (a `Group` or `When`). There is no node canvas: boxes are laid out from the asset, not dragged. Each change is one step of
the asset's own history (**Undo**, **Redo**); a drag or typing in one field is one step until you let go. **Save** (Ctrl+S
while the pointer is over the panel) writes the canonical text and marks the history saved; **Save\*** means unsaved edits.
A file that reloads from disk starts a new history. Unsaved edits are kept by document autosave (kind `camera`) and offered
for recovery after a crash. **Content ▸ New ▸ Camera rig** (`editor.create_asset { kind: "camera" }`) makes a valid asset named
after the file: a `look` slot and one rig `follow` that a fixed director runs.

`editor.camera_rig_graph` gives each node its `form`: `{ kind, fields, variants }`, the node's fields as values (an enum
variant with data is a one-key record: `{ "Literal": 4.0 }`, `{ "Param": "len" }`) and, per field path, the variants of
an enum there. A `Group`/`When` shows its `nodes` as a count. `editor.camera_panel_get`'s `page` adds `rigs: [{ name,
index, fields, nodes, transitions, references }]` (`references`: what still names the rig) and `transitions: [{ owner,
index, form }]`; the top level adds `history: { dirty, undo_depth, redo_depth, next_undo, next_redo, comments_warning }`.

| `op` | Parameters | What it does |
|---|---|---|
| `set_param` | `param`, `value` | A parameter's default, of the same type. |
| `set_group` | `rig`, `node`, `enabled` | A `Group`'s switch. |
| `node_field` | `rig`, `node`, `field`, `value` | Sets a field of the node at `node` (a path) through its form: `length.Literal`, `offset[1]`, `layers`. The value must fit the field. A list's Add entry sets the list with one more entry. |
| `node_variant` | `rig`, `node`, `field`, `variant` | Switches the enum at `field` (`length`: `Literal`, `Param`, `Var`; `target`; `cond`) with its fields default-filled. |
| `add_node` | `rig`, `kind`, `parent?`, `index?` | A default-filled node of `kind` (`Pivot`, `LookInput`, `Boom`, `Collision`, `Fov`, `Offset`, `SmoothPosition`, `SmoothRotation`, `Lean`, `Frame`, `Shake`, `Fly`, `Rail`, `Crane`, `Group`, `When`) in the root or in the `Group`/`When` at `parent`, at `index` (default the end). It is valid in the asset: a look or fly node takes the first input slot. Answers `node`, its path. |
| `remove_node`, `duplicate_node` | `rig`, `node` | Removes the node (and what it holds), or puts a copy right after it (answers `node`). |
| `move_node` | `rig`, `node`, `to` | Moves the node to index `to` of its own list. |
| `add_rig`, `duplicate_rig` | `name?`; `rig` | A new rig with one `Fov` node, or a copy right after `rig` (default names `rig`, `rig_2`, `<rig>_copy`). Answers `rig`. A copy's own transitions and `RigIs` conditions still name the original. |
| `remove_rig` | `rig` | Removes the rig, refused while the director, a transition outside it or another rig's `RigIs` names it (`camera_panel_rig_in_use`; the page's `references` lists them). |
| `move_rig`, `rename_rig` | `rig`, `to`; `rig`, `name` | Reorders the rig list; renames the rig and rewrites every reference to it. |
| `rig_field` | `rig`, `field`, `value` | `tags` or `blend_out`. |
| `add_transition` | `owner?`, `from_rig?`, `to_rig?` | A 0.25 s smooth transition at the end of the shared list (no `owner`) or a rig's own; ends are `any` (default), `tag:<tag>` or a rig name. Answers `transition: { owner, index }`. |
| `remove_transition`, `duplicate_transition`, `move_transition` | `owner?`, `index`, `to` (move) | By list and index; the list order is the order a driver searches. |
| `transition_field`, `transition_variant` | `owner?`, `index`, `field`, `value` / `variant` | Through the transition's form: `blend.Smooth`, `from.Rig`, `conditions`, `share_state`. |
| `undo`, `redo` | | One step of the asset's history. |

Every answer is `{ status, comments_dropped, history, node?, rig?, transition? }`. A refused edit changes nothing and is no
step; an edit that changes nothing is no step either. The ops never touch the file: Save writes it.

```json
{ "method": "editor.camera_panel_edit", "params": { "op": "node_variant", "rig": "walk", "node": [0], "field": "length", "variant": "Var" } }
{ "method": "editor.camera_panel_edit", "params": { "op": "node_field", "rig": "walk", "node": [0], "field": "length.Var", "value": "boom" } }
{ "method": "editor.camera_panel_edit", "params": { "op": "undo" } }
```

## Errors

Errors are `F0002` (`F0003` for `camera_file_not_found` through `camera.load`) with `data.reason`. While playing, the same codes come back in
`last_call.reason`; a failure with no code of its own says `call_failed`.

| Reason | Where | Meaning |
|---|---|---|
| `no_state_tree` | state trees | The entity has no `StateTree`, or no tree is selected and no `entity` was given. |
| `entity_not_found` | state trees, smart objects | No such entity, it is neither a smart object nor a slot, or nothing is selected. |
| `missing_param` | state trees, smart objects | `event`, `path` or `enabled` is required by this `op`. |
| `bad_op` | all | Unknown `op`. |
| `dev_only` | state trees | `debug` in a build without debug assertions. |
| `state_tree_panel_no_project` | state trees | `reload` with no project open. |
| `state_tree_panel_bad_path` | state trees | `reload`: not a plain path inside the project. |
| `state_tree_panel_io` | state trees | `reload`: the file cannot be read. |
| `slot_not_claimed` | smart objects | `release`: the slot has no holder. |
| `camera_unknown_asset` | cameras | No asset of that name on the page, or none selected. |
| `camera_bad_path`, `camera_file_not_found`, `camera_asset_invalid` | cameras | As in `camera.load`. |
| `camera_unknown_name`, `camera_bad_value` | cameras | `edit`: no such parameter, or a value of the wrong type. |
| `camera_no_driver` | cameras | No listed driver has that entity, or none is selected. |
| `camera_panel_unknown_node` | cameras | `edit`: no `Group` at that rig and path. |
| `camera_panel_has_errors` | cameras | `save`: validation errors; nothing was written. |
| `camera_panel_no_path` | cameras | `save`: the asset came from no file. |
| `camera_panel_no_project` | cameras | `save`: no project is open. |
| `camera_unknown_rig` | cameras | `edit`: no rig of that name (or a transition end names one). |
| `camera_panel_no_field` | cameras | `edit`: the node, rig or transition has no such field (a rig's form edits `tags` and `blend_out` only). |
| `camera_panel_structure_field` | cameras | `edit`: a `Group`/`When`'s `nodes` change through the node ops. |
| `camera_panel_bad_kind` | cameras | `add_node`: not a node kind. |
| `camera_panel_not_a_group` | cameras | `edit`: the path goes into a node that holds no nodes. |
| `camera_panel_bad_index` | cameras | `edit`: `index` or `to` past the end of the list. |
| `camera_panel_rig_in_use` | cameras | `remove_rig`: something else still names the rig. |
| `camera_panel_name_taken` | cameras | `add_rig`, `duplicate_rig`, `rename_rig`: the asset has a rig of that name. |
| `camera_panel_unknown_transition` | cameras | `edit`: no transition at that `owner` and `index`. |
| `camera_panel_nothing_to_undo`, `camera_panel_nothing_to_redo` | cameras | `undo`, `redo`: the history has no step. |
| `camera_panel_io` | cameras | `save`: the file cannot be written. |

A `reload` of a definition that does not compile answers the reason of `statetree.compile` (`book/agents/state-tree.md`), and the driver and
state tree calls forward the codes of the methods they wrap.

The State Trees panel debugs running trees; to author one, use the State Tree Editor panel
([state tree editor](state-tree-editor.md), `editor.statetree_*`).
