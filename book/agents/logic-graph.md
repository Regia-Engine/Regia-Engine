# Logic graphs (agents)

A **logic graph** is a designer-facing list of *when event E happens, if condition C holds, run
actions A1, A2, …* rules. It is authored as a `.logic.ron` file (format:
[`docs/formats/logic-ron.md`](../../docs/formats/logic-ron.md)), compiled into a small rule
interpreter (`regia_ai::logic::LogicGraph`) and ticked once per fixed step on the entity that holds
it. "Logic Graph" is the working name (spec 102, owner decision 1 is open). This page is the phase-1
reference: the runtime and the six `logic.*` methods (`logic.debug` and `step` are phase 4); [Editor canvas](#editor-canvas-phase-2) covers
the phase-2 Logic Graph panel and its five `editor.logic_*` methods; [Integrations](#integrations-phase-3)
covers phase 3: world blackboards (`logic.world`, `logic.world_set`), the sequence, input and net event
arms and the sequencer, cue and Lua actions. [Performance](#performance-phase-4) covers the
allocation-free steady state of phase 4.

## Runtime in one paragraph

One entity holds at most one `LogicGraph`, usually with a `Blackboard` (the *entity* scope) and a
`LogicInbox` (named events waiting for the next tick). Each fixed step, after behavior trees and
state trees, the graph runs: a paused graph does nothing; the first tick after a start queues
`Start`; due `Timer` arms are queued (arm order, then period); every watched key written since the
last tick queues one `BlackboardChanged` event (key order); then the inbox, oldest first (at most 64
a tick: the oldest overflow is dropped with an `inbox_overflow` trace line). Suspended action chains
resume first, then each event runs the rules in priority order (higher first, ties in declaration
order). Events emitted during a tick (by `logic::EmitEvent`, `logic.emit` or gameplay code) arrive on
the **next** tick, so two rules that emit each other's events advance one hop per tick.

## Scopes

| Key | Store |
|---|---|
| `local/x` | the graph's locals (declared with defaults in `locals:`) |
| `entity/x` or `x` | the entity's `Blackboard` |
| `world/<name>/x` | the world blackboard `<name>` (`LogicWorldBlackboards`), shared by every graph that declares `<name>` in its `world:` list |

A key with any other `/` prefix, or a `world/` key whose board the graph does not declare, is
`unknown_scope`. There is no shadowing: `local/x` and `x` are two stores.

## Built-in actions

| Task | `set` parameters |
|---|---|
| `logic::SetBlackboard` | `key` (a scoped key string, literal or `BbRef`), `op` = `set` (default) / `increment` / `append`, `value` (required for `set` and `append`; `increment` adds 1 without it) |
| `logic::EmitEvent` | `event` (string), `target` (optional `BbRef` to an entity value; default: this graph) |
| `logic::EnableEntity` / `logic::DisableEntity` | `target` (optional `BbRef` to an entity value; default: this entity). Removes or inserts the ECS `Disabled` marker. |

Any task registered in the `BtTaskRegistry` is also an action: its `set` bindings are written into the
entity blackboard before each tick, it ticks once per fixed step, `Running` suspends the chain until
the next tick, `Failure` stops the chain (`action_failed`). A rule fires again only when its chain has
finished (`chain_busy` otherwise). On a peer where `LogicAuthority` says no, the graph still ticks but
skips its sim actions: every built-in and every task not marked presentation
(`BtTaskRegistry::mark_presentation`; `logic::PlaySequence` and `logic::PlayCue` are).

## Methods

All errors are `F0002` with `data.reason` unless noted. Unknown or missing parameters: `missing_param`.
The guide topic `logic-graphs` (`agent.guide { "topic": "logic-graphs" }`) summarizes them.

### `logic.compile { text | path }`

Parses and compiles without building an entity. Never mutates the world.
Result: `{ ok, rules: [name in evaluation order], errors: [{ reason, rule?, detail }] }`. A compile
problem is *not* a protocol error: `ok` is false and `errors[0].reason` says why.

### `logic.control { entity, op }`

`op` is `pause`, `resume`, `restart` or `step`. Result: `{ status: "running" | "paused" | "not_started", paused }`.
`restart` resets cooldowns, timers, locals and suspended chains; the next tick runs the `Start` rules.
`step` runs exactly one more tick (inbox events that waited during the pause included) and pauses
again; on a running graph it pauses at the end of the next tick.

### `logic.emit { entity, event }`

Appends `event` to the entity's `LogicInbox` (created if absent). Result: `{ queued }` (events now
waiting). The graph sees it on its next tick; an `EntityEvent(event)` rule matches it.

### `logic.reload { entity, text | path }`

Compiles the new definition and hot-swaps it. Result: `{ outcome: "kept" | "restarted" }`. Kept: the
tick count, locals by name (new ones take their default, removed ones vanish), fire counts, cooldowns,
timer schedules and suspended chains of rules that kept their name (and task list), and the inbox.
Restarted: a `Start` rule that did not exist before must run, or a suspended chain's rule was removed
or now runs other tasks; locals, fire counts and the inbox still carry. A definition that does not
compile is refused with its compile reason and the running graph is untouched. Each call adds a
record to the `LogicReloads` log (Rust hosts can also queue a compiled graph there; it swaps in at
the start of the entity's next tick).

### `logic.status { entity }`

The `LogicGraphStatus`: `{ running, paused, ticks, evaluations, rules: [{ name, fires, matches,
last_fired_tick, cooldown_remaining, chain_busy }], locals: { name: value }, world: { "<board>/<key>":
value }, trace: [line] }` (`world` holds the graph's declared boards, at most 64 entries). The
same component is reflected, so `play.wait_until` can wait on it:

```json
{ "method": "play.wait_until", "params": { "condition": {
  "entity": 4294967301, "component": "LogicGraphStatus", "path": "rules[0].fires", "op": "gte", "value": 1 } } }
```

Trace lines read `tick N: <kind> <detail>` with kinds `event_received`, `rule_matched`,
`rule_rejected`, `action_started`, `action_failed`, `chain_busy`, `reload_kept`, `reload_restarted`,
`inbox_overflow` and `breakpoint` (`breakpoint rule <name>`, `breakpoint event <name>`; at most 64
lines, oldest first). An event no rule listens for is traced as
`event_received <name> (dropped: no rule)`.

### `logic.debug { entity, pause_on_rule?, pause_on_event?, disabled? }` (development builds)

Breakpoints and disabled rules (spec 102 § 9 and § 13 P4), held in the entity's `LogicGraphDebug`
component (reflected, never saved in the asset, compiled out of shipping builds): a fire of a rule
named in `pause_on_rule`, or an inbox event named in `pause_on_event` (only `EntityEvent` inbox
events, never a timer, watch or start), pauses the graph **at the end of that tick** (the rest of the
tick still runs) and traces `breakpoint rule <name>` / `breakpoint event <name>`. Rules named in
`disabled` never match (no evaluation, no trace line). Omitted lists keep their value; an empty list
clears one. Result: `{ pause_on_rule, pause_on_event, disabled }`. Advance with `logic.control step`,
continue with `resume`. A shipping build answers `dev_only`.

## Error reasons

| Reason | Methods | Meaning |
|---|---|---|
| `missing_param` | all | a required parameter is absent (`text`/`path`, `entity`, `op`, `event`); also a built-in action missing a required `set` key (compile) |
| `entity_not_found` | control, emit, reload, status | the entity id is not a live entity |
| `no_logic_graph` | control, emit, reload, status | the entity has no `LogicGraph` |
| `bad_op` | control; compile | `op` is not pause/resume/restart/step; a `logic::SetBlackboard` op is not set/increment/append |
| `bad_value` | emit, world_set; compile | an empty event; an empty rule, local or key name, a non-finite number, a zero timer period, more than 64 locals, 16 world boards or 32 `set` entries, a world board name that is empty or holds `/`, an empty sequence/marker/message/action name in an event arm, a built-in parameter of the wrong type; `logic.world_set`: a bad board or key, or a value that is not a bool, number, string, `[x, y, z]` or `{ "entity": bits }` |
| `parse` | compile, reload | the RON text does not parse |
| `unsupported_version` | compile, reload | `format_version` is not 1 |
| `too_many_rules` | compile, reload | more than 256 rules |
| `too_many_actions` | compile, reload | a rule has more than 16 actions |
| `condition_too_deep` | compile, reload | a condition nests deeper than 16 |
| `duplicate_name` | compile, reload | two rules, two locals or two world boards share a name |
| `unknown_task` | compile, reload | an action names neither a built-in nor a registered task |
| `unknown_scope` | compile, reload | a key's prefix is not `local/`, `entity/`, `world/<declared board>/` or none |
| `unsupported_feature` | compile, reload | not raised since phase 3 (it named the then-unbuilt world list and sequence, net and input arms); kept so clients that match on it still parse |
| `board_not_found` | world | no world blackboard has that name (a graph creates its declared boards on its first tick) |
| `rule_not_found` | debug | a name in `pause_on_rule` or `disabled` is not a rule of the graph |
| `dev_only` | debug | a shipping build (`logic.debug` is compiled for development builds only) |

`dangling_wire` is an editor issue code (below). `missing_param`, `entity_not_found`, `no_logic_graph`
and `bad_value` (a list that is not an array of names, an empty event name) apply to `logic.debug` too.

## Integrations (phase 3)

Spec 102 § 13 P3, ADR-7268. World blackboards and the event arms are in `regia_ai`; the actions and
routes that need the sequencer, audio, Lua or the net layer are in the umbrella crate:
`regia::logic_bridge` (Cargo feature `logic`), installed with `LogicBridgePlugin` next to `AiPlugin`.

### World blackboards

A graph lists the boards it uses in `world: ["gatehouse"]` and reads or writes `world/gatehouse/<key>`
in conditions, `BbRef`s, `logic::SetBlackboard` keys and `BlackboardChanged` watches. Boards live in
the `LogicWorldBlackboards` resource (one `Blackboard` per name) for as long as the world; a graph
creates its declared boards on its first tick. Writes are counted like entity writes, so one graph's
`SetBlackboard` wakes another graph's `BlackboardChanged(key: "world/gatehouse/alarm")` on its next
tick. At most 16 boards per graph; names are non-empty, without `/`, unique.

### `logic.world { board? }`

Reads the boards: `{ boards: { "<board>": { "<key>": value } } }`, every board or only `board`
(`board_not_found` when it does not exist). Read-only.

### `logic.world_set { board, key, value }`

Writes one value (a bool, number, string, `[x, y, z]` or `{ "entity": bits }`) into a board, creating
the board when missing. Result: `{ board, key, value, writes }` (`writes` is the key's write counter).
Graphs watching the key see the write on their next tick. Reasons: `missing_param`, `bad_value`.

### Event arms

The three arms arrive through the inbox under fixed spellings, so `logic.emit` can raise any of them
by hand when testing a graph:

| Arm | Inbox event | Raised by |
|---|---|---|
| `OnSequenceMarker(sequence, marker)` | `sequence_marker:<sequence>#<marker>` | a sequence player with a `LogicSequenceSource` (every `logic::PlaySequence` player has one) crossing the marker; the graph that started it and every bound target with a logic graph hear it |
| `InputAction(action)` | `input_action:<action>` | the bound action's press edge (one fixed step) on the entity a `PlayerController` possesses, for that controller's player |
| `OnNetMessage(message)` | `net_message:<message id>` | a net message applied at `NetSet::Apply` whose target has a logic graph, for message types routed with `app.add_logic_net_message::<M>()`; the sender's tick is written to the target's `net_message_tick` first; order = application order |

### Actions

| Task | `set` parameters | Kind |
|---|---|---|
| `logic::PlaySequence` | `sequence` (a `SequenceLibrary` name, usually the `.seq.ron` path), `target` (optional entity; default this entity), `bind` (optional binding name; default `target`) | presentation (`client`) |
| `logic::PlayCue` | `cue` (a registered cue name, or a `.cue.ron` path whose file stem is the name) | presentation (`client`) |
| `logic::CallLua` | `mod`, `function`; every other entry reaches the function as a table | sim (`lua`) |

`PlaySequence` spawns one player and succeeds at once; an unknown sequence fails the action.
`PlayCue` plays through `Audio::play_cue` (a play refused by the cue's cooldown still succeeds; an
unknown cue fails the action). `CallLua` runs inside the mod's budget and memory cap: `false` fails the
action, any other return succeeds, and a Lua error fails only the action — the mod stays enabled and
the graph keeps running. From Lua, `regia.emit(name, value, target)` (spec 84 as amended) raises
`name` in the target entity's graph inbox; without a target it goes to every entity marked
`LogicModHost { mod_id }` for that mod. Events no graph takes stay in `LogicLuaUnrouted`.

**Payload drop (intended, v1; ADR-7268 addendum).** Only the event *name* crosses into the inbox:
the inbox stays plain strings (spec 102 § 4.1), the route writes no blackboard key (contrast the net
arm's `net_message_tick`), and the emitted `value` does not reach the graph. To hand a value to a
graph, keep it in the mod's Lua state (the source of truth) and read it from the game side with
`LuaMods::call_function` (it returns the value) or, for events no graph took, from
`LogicLuaUnrouted` (unrouted events keep their full value), then write it into the entity or world
blackboard that graph conditions read. A graph can also call back into the mod with `logic::CallLua`,
whose `set` map reaches the function as a table. A blackboard payload convention for Lua-routed
events (the route writing the value, as the net arm does with the tick) is a v2 candidate needing its
own ADR, not implemented here.

### Networking

With the `net` feature `LogicBridgePlugin` installs a `LogicAuthority` built on `NetRole`: an entity
without a role, or with `Authority` / `Local`, runs everything; on `Proxy` and `Predicted` copies the
graph still ticks and runs only presentation actions.

## Determinism

Timers count fixed steps (a graph the AI tick budget defers is woken on the step its timer is due),
jitter comes from the graph `seed` through SplitMix64, rules run in a fixed order and every map is a
`BTreeMap`. The same definition, seed, writes and events give a byte-identical `LogicGraphStatus`.

## Performance (phase 4)

A steady graph allocates nothing per tick, including ticks that fire rules, write the `local/`,
entity and `world/` scopes, take inbox events and trace (spec 102 § 13 P4.3, ADR-7282): the trace is
a ring of typed records formatted only when read, and `LogicGraphStatus` is refreshed in place (only
new trace lines are formatted, into the buffers of the lines that dropped out). `logic.status` and the
editor's live view print the same trace text as before. `logic::EmitEvent` reuses the buffers of
inbox events the graph consumed. What still allocates: an emit from a graph that never receives
events, a `BbRef` key or event name, an action failure, the first write of a key, a reload.
`LogicGraph::write_trace_line` formats a line into a caller's buffer. The test is
`crates/regia_ai/tests/logic_graph_alloc.rs` (100 graphs × 25 compare rules, 1,000 steps); the
wall-clock microbenchmark in the same file is `#[ignore]`d and run by hand.

### Dogfood script (phase 4)

`crates/regia_editor/tests/logic_graph_dogfood.rs` (`cargo test -p regia_editor --test logic_graph_dogfood`, run
by the merge gate with the other editor tests) is the agent's whole session over the real dispatcher, in order:
`editor.asset_kinds` → `editor.create_asset { kind: "logic_graph" }` → `logic.compile { path }` →
`editor.open_asset` → `editor.logic_state { palette }` → `editor.logic_edit` (two rules; a refused `pin_type` wire;
an extra rule taken back by `editor.undo`) → `editor.logic_validate` → `editor.logic_save` → the saved file runs:
`logic.emit`, `play.wait_until` on `LogicGraphStatus`, `logic.status` → `logic.debug` breakpoint, `logic.control
step` / `resume`, a disabled rule (development builds) → an edit saved and applied with `logic.reload { path }`
(`kept`, locals carried over) → `editor.open_asset` reopens the file clean. Copy it as a template for scripts of
your own.

## Editor canvas (phase 2)

The **Logic Graph** panel (`Panel::LogicGraph`, `regia_editor::logic_graph_editor`, ADR-7224) edits
a `.logic.ron` as nodes on an egui-snarl canvas: a rule is one **event** node (rule name, event arm,
priority, cooldown) wired through zero or more **condition** nodes into zero or more **action**
nodes, in execution order. Every node takes one wire in and gives one wire out, so a rule is a
chain. Pin types: an event has only an output; a condition feeds a condition or an action; an action
feeds the next action. Lowering to the file: no condition node is `Always`, one node is its
condition as written, two or more are `All` of them in wire order. A condition node's *mode*
(`leaf`, `all`, `any`, `not`) is the top of its condition tree; switching it re-wraps the same terms.
Opening a file raises it back into nodes (`All` of two or more terms becomes one node per term), so
re-saving an unchanged file writes the same bytes.

Positions are editor-only: they go to the `x.logic.layout.ron` sidecar next to `x.logic.ron`
(`format_version: 1, positions: { "<rule>#event" | "<rule>#condition<i>" | "<rule>#action<i>": (x, y) }`),
never into the frozen `.logic.ron`. A missing or broken sidecar gives the default three-column layout
(events left, conditions middle, actions right).

The panel's gestures (palette add, dropped-wire add-and-connect, wire drag, right-click a pin to
unwire, node menu delete, node drags, inspector fields) are the same `LogicOp`s the agent sends, each
gesture one step on the editor's undo stack (`editor.undo` reverts it; a node drag is one step).
While the game plays the panel lists the play world's graph entities, colors the watched graph's
rules by what they did on its newest traced tick (fired, matched, rejected, failed; an action node
lights up when its chain reached it) and shows its variables (`local/`, entity and `world/`),
with pause / resume / step buttons for the watched graph (breakpoints are set with `logic.debug`).
Saving during play reloads every graph that ran the file as last opened or saved through
`LogicReloads` (kept or restarted by rule name, spec 102 § 7); a graph running another file is never
touched.

### Creating and opening a graph file

Content ▸ **New ▸ Logic graph** and `editor.create_asset { "kind": "logic_graph", "path": "assets/…/x.logic.ron" }`
write the same file: an empty graph at format version 1 (no rules, locals or world boards), the bytes the panel
saves for an empty graph, which `logic.compile` and `LogicGraph::compile` accept. `editor.asset_kinds` lists the
kind in both tables: `create` (`logic_graph`, label `Logic graph`, extension `logic.ron`) and `open` (suffix
`.logic.ron`, panel `Logic Graph`, state `editor.logic_state` → `path`). A double click in the Content Browser, an
Output Log link and `editor.open_asset { path }` all open a `.logic.ron` through the one asset-open dispatcher
(`book/agents/content-browser.md`), which calls `editor.logic_open { path }` and brings the panel forward; the
answer's `reply` is `editor.logic_open`'s result, and `editor.logic_open`'s reasons come back unchanged. The
`.logic.layout.ron` sidecar is not an openable kind (`asset_kind_unknown`).

### `editor.logic_open { path? , create? }`

Opens a `.logic.ron` (and its sidecar) or, with `create: true`, starts an empty graph (saved to
`path` later). Returns `{ path, rules, nodes, valid, revision, discarded_unsaved }`.

### `editor.logic_state { palette?, ron? }`

The open graph: `nodes` (`id`, `kind`: `event | condition | action` with their fields, `pos`),
`wires` (`from`, `to`), `rules` (name, `event` node, `conditions`, `actions` in wire order), `locals`,
`next_id`, `valid`, `issues`, `selected`, the play-world `agents`, `watch`, `live` (`entity`,
`running`, `ticks`, `activity_tick`, `events`, `activity` per rule, `actions_started`, `fires`,
`locals`, `entity_vars`, `world_vars`, `matches_document`) and `reload` (`save`, `queued`, `kept`,
`restarted`, `rejected`, `reason`). `palette: true` adds the event arms, condition kinds, modes and
action tasks; `ron: true` the text a save would write.

### `editor.logic_edit { ops?, select?, watch?, control?, label? }`

One undoable edit: `ops` applied all or nothing (at most 1024). Returns `{ revision, created, valid,
issues, selected, watch }`; `created` lists the ids of added nodes. Ops (`"op"` names; enum fields use
the file's spelling):

```json
[
  { "op": "add_event", "rule": "open", "event": { "EntityEvent": "use" }, "priority": 0, "cooldown_ticks": 0, "id": 1, "at": [0, 0] },
  { "op": "add_condition", "condition": { "Key": "unlocked" }, "id": 2 },
  { "op": "add_action", "task": "logic::EmitEvent", "set": { "event": { "Literal": { "String": "opened" } } }, "id": 3 },
  { "op": "connect", "from": 1, "to": 2 },
  { "op": "connect", "from": 2, "to": 3 },
  { "op": "disconnect", "from": 2, "to": 3 },
  { "op": "move_node", "node": 3, "at": [480, 0] },
  { "op": "remove_node", "node": 3 },
  { "op": "set_event", "node": 1, "rule": "open_door", "event": "Start", "priority": 5, "cooldown_ticks": 60 },
  { "op": "set_condition", "node": 2, "condition": { "All": [{ "Has": "a" }, { "Key": "b" }] } },
  { "op": "set_condition_mode", "node": 2, "mode": "any" },
  { "op": "set_task", "node": 3, "task": "logic::SetBlackboard" },
  { "op": "set_binding", "node": 3, "key": "target", "value": { "BbRef": "door" } },
  { "op": "set_local", "name": "n", "default": { "Int": 0 } },
  { "op": "set_entity_var", "name": "hp", "default": { "Int": 100 } },
  { "op": "set_world_var", "board": "gatehouse", "name": "raised", "default": { "Int": 0 } },
  { "op": "set_graph", "description": "Gatehouse", "seed": 7, "world": ["gatehouse"] },
  { "op": "move_rule", "node": 1, "index": 0 }
]
```

`id` (add ops) picks the new node's id so later ops in the same batch can wire it; without it the
document's `next_id` is used. `set_binding`, `set_local`, `set_entity_var` and `set_world_var` without
`value` / `default` remove the entry. `set_entity_var` and `set_world_var` declare `.logic.ron` v2 typed
variables (ADR-8900): the default's kind is the type, and the runtime writes the default into the entity's
`Blackboard` or the world board when the graph starts and the key is missing. `select` (node id, 0 clears) and `watch` (play-world entity bits, 0 picks automatically) can be
sent with or without ops. `control` (`pause`, `resume` or `step`) acts on the watched play-world graph as the
live view's Pause, Resume and Step buttons do (applied at the next pump; not an undo step; ADR-7774).

### `editor.logic_save { path? }`

Writes the canonical `.logic.ron` and the layout sidecar atomically (`{ path, layout, valid, saves,
playing }`). A graph with errors is refused with `invalid_graph` (its `data.issues` lists them) and
nothing is written.

### `editor.logic_validate { text? | path? }`

Validates text, a file, or (with neither) the open graph without changing anything:
`{ valid, issues, rules }`.

### Issues

Each issue is `{ code, severity, node, rule, message }`; `node` points at the node to fix (the action
with the unknown task, the condition or action that names the bad key, else the rule's event node).
Each rule is checked on its own, so one bad rule does not hide another's problems.

| Code | Meaning |
|---|---|
| `dangling_wire` | a condition or action node is not wired to any event (the canvas only; a file has none) |
| `duplicate_name` | two rules (or two locals) share a name |
| `too_many_rules`, `too_many_actions`, `condition_too_deep` | past the § 3 limits (256 rules, 16 actions, depth 16) |
| `unknown_task` | an action's task is neither built-in nor registered (a **warning** when the editor has no task registry loaded) |
| `unknown_scope` | a key's prefix is not `local/`, `entity/` or none |
| `bad_value`, `bad_op`, `missing_param` | as for `logic.compile` (an empty rule name, a zero timer period, a built-in's parameters) |
| `unsupported_feature` | not raised since phase 3 (see the method reasons above) |

### Editor error reasons

| Reason | Methods | Meaning |
|---|---|---|
| `no_graph_open` | state, edit, save, validate (no args) | nothing is open: call `editor.logic_open` first |
| `missing_param` | open, edit | `path` without `create`; `edit` with no `ops`, `select`, `watch` or `control` |
| `bad_control` | edit | `control` is not `pause`, `resume`, `step` or `step_to_fire` |
| `bad_path` | open, save, validate | the path does not end in `.logic.ron` (or no path to save to) |
| `path_outside_project` | open, save, validate | the path leaves the project, temp and working directories |
| `open_failed` | open | the file is not a readable version-1 `.logic.ron` |
| `bad_ops`, `too_many_ops` | edit | `ops` is not an array of known ops; more than 1024 |
| `unknown_node` | edit | an op (or `select`) names a node that does not exist |
| `duplicate_id` | edit | an add op's `id` is taken |
| `wrong_kind` | edit | e.g. `set_task` on a condition node |
| `pin_type` | edit | the wire breaks the pin types (into an event; action → condition; event → event) |
| `pin_busy` | edit | the output or input already has a wire (disconnect it first; the panel does that for a drag) |
| `cycle` | edit | the wire would close a loop |
| `no_wire` | edit | `disconnect` of a wire that does not exist |
| `bad_value` | edit | a non-finite position, node id 0, an empty parameter name |
| `invalid_graph` | save | the graph has errors; nothing was written |
| `save_failed` | save | the write failed |
| `bad_params` | validate | both `text` and `path` |
| `parse`, `unsupported_version` | validate | the text does not parse, or is not version 1 |

## Debugger and authoring helpers (ADR-8021)

The Logic Graph panel debugs a running graph in place: rule and event breakpoints with a
Breakpoints list, Step to the next fired rule, hover values on condition nodes, a Watch list with a
Live toggle, an evaluate box, wires that pulse when their rule fires, a Variables list, a fuzzy
palette search, and the open graph's issues in the editor-wide Problems panel. Every control calls
the same function as its method below. The runtime methods (`logic.debug`, `logic.control`) stay the
play-world API; these are the editor's.

**Step granularity.** A graph's tick evaluates every queued event against every rule and runs the
fired chains in one pass (`LogicGraph::tick`); it can stop only between ticks. So the finest step
past `step` (one tick) is **Step to the next fired rule**: `editor.logic_edit { control:
"step_to_fire" }` resumes the watched graph with a temporary breakpoint on every rule and pauses at
the end of the first tick in which any rule fires (development builds; the temporary entries leave
the debug lists once it paused).

**Breakpoints are per user.** They live in `<project>/.regia/logic_breakpoints.ron` (keyed by the
graph's project-relative path, `format_version: 1`), never in the `.logic.ron`, and apply only in
development builds (they ride on `LogicGraphDebug`, which a shipping build compiles out). While the
game plays, the editor adds the open file's enabled breakpoints to the `LogicGraphDebug` of every
graph running that file and removes only what it added, so entries set with `logic.debug` stay.
**Debug-instance filter.** With two or more instances of the file running, the live view's instance
pick (`editor.logic_edit { watch }`) filters the debugger to that instance: the open file's
breakpoints go to its `LogicGraphDebug` only, and move with the pick (w13, ADR-8565). With no pick
(or a pick that left the world), they apply to every instance running the file, as above.

**Watch values come from a snapshot.** The editor copies the watched graph's scopes (its locals, its
entity's `Blackboard`, its declared world boards) when the graph pauses, or every editor frame with
Live on; a running graph with Live off costs nothing (`refreshes` counts the copies). Keys and
conditions resolve by the runtime's own scope rule (`regia_ai::logic::LogicScopeView`).

### `editor.logic_palette_search { query, from?, side?, limit? }`

Fuzzy search over the palette (events, conditions, actions), best first (ties in palette order).
With `from` (a node id) and `side` (`output`, the default, or `input`) only what a wire dragged from
that pin can connect to: an event's or a condition's output takes conditions and actions, an
action's output only actions; a condition's input is fed by events and conditions, an action's by
anything. Result `{ hits: [{ group, label, ops }], total }`: `ops` is the `editor.logic_edit` batch
that adds the entry (wired to `from` when given), at the origin.

### `editor.logic_variables {}`

`{ variables: [{ scope, name, type, default, uses }] }`: the locals (`local`, with `type` and
`default`), the declared world boards (`world`, `type` and `default` null), the typed world variables
(`world`, named `<board>/<key>`, with `type` and `default`; `.logic.ron` v2) and the entity keys: every
declared entity variable (with `type` and `default`) and every key the graph reads or writes without a
declaration (`type` and `default` null).

### `editor.logic_variable_set { scope, name, type?, default?, rename_to? }`

One undo step. `scope: "local"` declares the local or changes its `type` (`bool`, `int`, `float`,
`string`; the default converts) or `default` (a JSON bool, number or string); `scope: "entity"`
does the same for an entity variable (`.logic.ron` v2, ADR-8900: the runtime seeds the default when the
graph starts and the entity lacks the key); `scope: "world"` with a plain name declares a board, and
with `<board>/<key>` declares or retypes a world variable (declaring its board when missing).
`rename_to` renames a local, a board, a world variable's key (`<board>/<new key>`, same board) or an
entity key (and its declaration) and rewrites every reference (`BlackboardChanged` keys, condition
keys, `BbRef` bindings, `logic::SetBlackboard`'s literal key); a renamed local or entity variable keeps
its place, and a renamed board takes its world variables along. Result `{ revision, variables }`.

### `editor.logic_variable_remove { scope, name }`

Removes a local's, a board's, an entity variable's or a world variable's (`<board>/<key>`) declaration
(one undo step); references stay (a removed entity variable's key stays listed, untyped) and show as
issues where they no longer compile.

### `editor.logic_breakpoint_set { kind, name, enabled?, remove?, toggle? }`

`kind`: `rule` (pauses at the end of a tick the rule fires in) or `event` (pauses at the end of the
tick its inbox event arrives in; an arm that comes through the inbox: `EntityEvent`, sequence
markers, net messages, input actions). Without flags it adds the breakpoint enabled; `enabled:
false` disables it, `remove: true` removes it, `toggle: true` adds or removes it (F9). The graph
must be saved (breakpoints belong to its file). Result `{ exists, breakpoints }` (the list below).

### `editor.logic_breakpoint_list {}`

`{ graph, file, applies, rules: [{ name, enabled, node }], events: [{ name, enabled, node }], hits,
step_to_fire, error }`: `node` is the event node Go to selects, `hits` the breakpoints that paused the
watched graph on its newest tick (`rule <name>`, `event <name>`), `applies` false in a shipping
build, `error` why the file could not be read.

### `editor.logic_watch { add?, remove?, add_condition?, remove_condition?, live? }`

Pins and unpins variables (`local/x`, `world/<board>/x`, `entity/x` or a bare entity key) and
condition nodes, and sets Live. Result `{ live, items: [{ label, key?, node?, value?, error? }],
snapshot: { entity, tick, paused } | null, refreshes }`; a value is the `Debug` text of the value
(`unset` when the scope holds none), a condition's is `true` or `false`.

### `editor.logic_evaluate { expression }`

While the watched graph is paused: evaluates a condition in the graph's own syntax (a `Condition` in
RON, as the inspector's Condition field takes it, e.g. `Compare(key: "local/n", op: Gt, value:
0.0)`) over the paused instance. `{ value, entity, tick }`.

### `editor.logic_goto_node { node }`

Brings the Logic Graph panel forward, selects the node and frames it: what a Problems row's link and
the Breakpoints list's Go to do. `editor.problems_panel` answers the open graph's issues as
`logic_graph: [{ path, node, severity, code, message }]`.

### Keyboard

| Key or control | Does |
|---|---|
| F9 | toggles the breakpoint of the selected node's rule |
| The rule head's dot (a named button) | sets or removes the rule breakpoint |
| Search nodes (palette), Enter | adds the best match |
| A dropped wire's menu: type, Enter | adds the best match, wired |
| Breakpoints tab rows | enable box, Go to, Remove (Tab reaches each) |
| Variables tab fields, Enter | rename, set a default, declare a local or a board |
| Watch a variable, Enter | pins the key |
| Evaluate expression, Enter (while paused) | evaluates |
| A Problems row, Enter | opens the graph at the node |
| Inspector "Find usages of …", Enter | runs the search, shows the Usages tab |
| A Usages row, Enter | opens the graph at the node |
| **Nodes** list beside the canvas (Tab reaches it; a row per node, "go: Start (#1)") | Up / Down move between nodes; Enter selects the node and frames it on the canvas |
| Nodes row, Ctrl+Shift+C | starts a wire from the node's output (opens **Connect…**): type the target's row name, `#id` or a title only one node has, Enter completes it (the `connect` op, after freeing both pins), Escape cancels |
| Nodes row, Ctrl+Shift+A | opens the palette filtered to what the node's output feeds; Enter adds the first match wired after it |
| Nodes row, Alt+arrow / Delete | moves the node 16 units (Shift: 64; one undo step per press: `move_node`) / removes it (`remove_node`) |

The Nodes list is the canvas's keyboard route (FU-5, ADR-8628; `canvas_keys::graph_keys_ui`, the
material graph's pattern): each key makes the op `editor.logic_edit` takes, through the same
`edit` path, and `editor.logic_edit { select }` is Enter's selection. A wire target that names no
node (or a title two nodes share) is refused with a toast and changes nothing. The canvas is an
AccessKit `Canvas` node named "Logic graph canvas, N nodes". Test:
`cargo test -p regia_editor --lib logic_graph_editor::keys_tests`.

### Debugger error reasons

| Reason | Methods | Meaning |
|---|---|---|
| `no_graph_open` | every method here | nothing is open |
| `missing_param` | palette_search, variable_*, breakpoint_set, evaluate, goto_node | a required parameter is missing |
| `unknown_node` | palette_search, goto_node | no such node |
| `bad_value` | palette_search, variable_set, breakpoint_set, watch, evaluate | `side`, `type`, `default`, `kind` or a name is not valid; an empty event name |
| `bad_scope` | variable_set, variable_remove | `scope` is not `local`, `world` or `entity` |
| `bad_name` | variable_set, variable_remove | a name is empty or has a `/` (a world variable is `<board>/<key>`, both non-empty; a rename keeps the board) |
| `duplicate_name` | variable_set | `rename_to` names an existing local, board, world variable or entity variable |
| `unknown_variable` | variable_set, variable_remove | no such local, board, world variable, used entity key or (remove) declared entity variable |
| `no_graph_path` | breakpoint_set | the graph was never saved |
| `rule_not_found` | breakpoint_set | the graph has no rule of that name |
| `not_found` | breakpoint_set | disable or remove a breakpoint that does not exist |
| `save_failed` | breakpoint_set | `.regia/logic_breakpoints.ron` could not be written, or it exists but could not be read (damaged, or a newer format version): it is kept as it is, never overwritten |
| `not_a_condition` | watch | `add_condition` names a node that is not a condition |
| `too_many_watches` | watch | the Watch list holds 64 entries |
| `not_paused` | evaluate | the watched graph is not paused (nothing to evaluate against) |
| `parse` | evaluate | the expression is not a `Condition` in RON |
| `unknown_scope` | evaluate | a key names a scope the graph does not declare (`world/<board>/` of an undeclared board) |
| `condition_too_deep` | evaluate | deeper than 16 levels |

## Find usages (w13, ADR-8565)

Which graphs use an event, a blackboard key or an animation parameter, across the project. A logic
node's context menu (right-click) has a **Find usages of event "x"** / **key "x"** / **param "x"**
item for each name the node uses, and the inspector shows the same buttons for the selected node
(the keyboard path: Tab to one, Enter). The answer fills the panel's **Usages** tab, one focusable row
per use; Enter or a click on a logic row opens that graph with the node selected and framed (a graph
with unsaved work is never replaced: save it first), on an animation-graph row opens it in its
editor. Code rows show the file and line.

The project index is built on demand (never per frame): every `.logic.ron`, `.animgraph.ron` and
`.rs` under the project root (hidden folders and `target` skipped, 50 000 entries at most). A file
whose content hash is unchanged since the last search keeps its extracted uses, so a second search
parses nothing.

| Kind | `how` | Where |
|---|---|---|
| event | `receives` | a rule's event arm, by its inbox name (`EntityEvent("x")` is `x`; `input_action:<a>`, `net_message:<m>`, `sequence_marker:<s>#<m>`) |
| event | `emits` | `logic::EmitEvent` with a literal `event` |
| key | `reads` | conditions (`Key`, `Has`, `Compare`, `Text`), any `BbRef` binding |
| key | `writes` | `logic::SetBlackboard` with a literal `key`; a registered task's parameters (written into the entity blackboard before its ticks) |
| key | `watches` | `BlackboardChanged` event arms |
| key | `declares` | a graph local (`local/x`) |
| param | `declares`, `reads` | an animation graph's parameter list; blend spaces, transitions, blend, additive, layer, IK and look-at weights |
| param | `sets` | a registered task's parameter of that name, or a literal string parameter value naming it; code lines `set_float("x", …)` (and `set_int`, `set_bool`, `set_vec2`, `set_vec3`, `set_trigger`) |

`x` and `entity/x` are one key; `local/x` is per graph (each graph's row is its own local).
Events emitted from code or with a key-bound name, and keys built at run time, are not found.

### `editor.logic_find_usages { event | key | param, open? }`

Exactly one of `event`, `key`, `param`. Result `{ kind, name, usages: [{ index, path, doc, how, node,
rule, line, detail }], files, parsed, opened? }`: `doc` is `logic`, `anim` or `code`; `node` the logic
node id as the panel opens the file; `files` the files indexed, `parsed` how many this search had
to parse. `open: <index>` follows that row as a click does and answers `opened: { path, doc, node }`.
Needs no graph open.

| Reason | Meaning |
|---|---|
| `missing_param` | none of `event`, `key`, `param` |
| `bad_value` | more than one of them, an empty or non-string name, `open` not a row index |
| `no_search` | `open` with no answer (cannot happen through this method; the panel's rows) |
| `usage_missing` | `open` names no row of the answer |
| `unsaved_changes` | `open` would replace another graph with unsaved work |
| `open_failed` | the file could not be opened |
| `unknown_node` | the row's node is gone from the open graph (edited since) |
| `no_editor` | `open` names a code row |

## Graphs on prefab and level entities

An entity can run a graph from data: the reflected `LogicGraphRef { graph, enabled, overrides }` component names a
project `.logic.ron` (and optional starting values for its locals), and `AiPlugin` compiles it into the entity's
`LogicGraph` when the component is added or changed. Saved in prefabs and levels (`logic-graph-ref` v1,
`docs/formats/logic-graph-ref.md`). In prefab edit mode, `editor.prefab_logic { op: "open" | "new" }` (the banner's
Open graph and New graph for this prefab) opens the prefab's graph or creates one next to the prefab and attaches it
in one undo step: see [Prefab logic](prefab-logic.md).

## Match rules nodes (GR-7.6)

`LogicBridgePlugin` registers five match-rules tasks in every build and routes the match session's events into
graphs (spec 102 § 14). No new method and no new error reason: the tasks show in the palette
(`editor.logic_state { palette: true }`, `editor.logic_palette_search { query: "rules" }`), a failure is an
`action_failed` trace line in `logic.status`, and `logic.emit { entity, event: "match.flag_captured" }` raises a
match event by hand when testing a graph (write the `match_<kind>_<field>` keys it reads first).

### Actions

| Task | `set` parameters | Kind |
|---|---|---|
| `rules::AddScore` | `points` (integer, negative takes away) and `team`, or `team` + `seat` (slot), or `entity` (a pawn with `TeamId` + `LedgerSlot`) | sim, host only |
| `rules::EndMatch` | `team` (default `0`), `reason` (default `ended`; `forfeit`, `draw`, `time_limit`, `rounds_won`, `capture_limit_reached`, `hill_score_limit_reached`) | sim, host only |
| `rules::SetTeam` | `team`, `entity` (default this entity) | sim, host only |
| `rules::SetSpawnEnabled` | `entity` (a `PlayerStart`; default this entity), `on` (default `true`) | sim, host only |
| `rules::LockObjective` | `id` (objective entity id), `on` (default `true`) | sim, host only |

"Host only": a peer without authority skips the action like every sim action, and a process in `NetMode::Client`
refuses it (a warning on `regia::logic`, then `action_failed ... refused on a client`). A team score from `AddScore`
sends no event and checks no limit; a seat score sends `match.player_scored` with `match_player_scored_reason` `scripted`.
`EndMatch` with the phase machine on lands on the next session step. `SetTeam` on a seated pawn follows the
session's `teams.switch` policy (the default `never` refuses: `match_team_switch_refused` in the trace line).
`LockObjective` stops pickups, touch returns and captures of a flag and the points of a hill until it is unlocked;
`rules.match_get` shows the locked ids as `board.locked` (absent while nothing is locked).

### Events

Every `MatchEvent` of the fixed step arrives as `EntityEvent("match.<kind>")` on each `MatchEventListener` with a graph
and on the entities the event involves (the unit, the flag's or hill's `ObjectiveEntity`, the seat's pawn):

| Event | Involves | Fields written (`match_<kind>_<field>`) |
|---|---|---|
| `match.flag_taken` | unit, flag | team, objective, unit, tick |
| `match.flag_dropped` | unit, flag | team (the flag's), objective, unit, tick |
| `match.flag_returned` | unit, flag | team (the flag's), objective, unit, tick |
| `match.flag_captured` | unit, flag | team, score, objective, unit, tick |
| `match.flag_auto_returned` | flag | team (the flag's), objective, reason (`timeout`, `drop_check`), tick |
| `match.hill_point` | hill | team, score, objective, tick |
| `match.player_scored` | seat pawn | team, seat, score, reason (`kill`, `death`, `assist`, `objective`, `scripted`), tick |
| `match.seat_left` | seat pawn | team, seat, tick |
| `match.match_over` | none | team, reason (the end reason), tick |
| `match.round_over` | none | team, round, reason, tick |
| `match.phase_changed` | none | reason (the phase), round, tick |
| `match.paused` | none | reason (who paused), tick |
| `match.resumed`, `match.launch_scheduled`, `match.launch_cancelled` | none | tick |
| `match.launched` | none | reason (the mode), tick |
| `match.respawn_at`, `match.eliminated` | seat pawn | team, seat, tick |
| `match.zone_captured`, `match.zone_activated` | zone | team (the new owner; none for an activation), objective, tick |
| `match.zone_point` | zone | team, score, objective, tick |
| `match.wave_started`, `match.wave_cleared` | none | round (the wave index), tick |
| `match.wave_spawn` | none | round (the wave index), score (the count), reason (the archetype), tick |
| `match.coop_ended` | none | reason (`all_waves_cleared`, `objective_reached`, `team_wipe`, `objective_destroyed`), tick |
| `match.racer_checkpoint` | seat pawn, checkpoint | team, seat, objective (the checkpoint), round (the lap), tick |
| `match.racer_lap` | seat pawn | team, seat, round (the lap), score (the lap ticks), reason (`best` for a best lap), tick |
| `match.racer_missed_checkpoint` | seat pawn, checkpoint | team, seat, objective (the checkpoint passed), tick |
| `match.racer_wrong_way`, `match.racer_dnf` | seat pawn | team, seat, tick |
| `match.racer_finished` | seat pawn | team, seat, score (the place), tick |
| `match.zone_phase` | none | round (the stage index), reason (`idle`, `waiting`, `shrinking`, `closed`), tick |
| `match.zone_damage` | unit | unit, score (the damage), tick |
| `match.team_eliminated` | none | team, score (the place), reason (the out cause), tick |
| `match.victory` | none | team (the first winner), reason (the end reason), tick |
| `match.surrender_vote` | seat pawn | team, seat, reason (`yes`, `no`), tick |
| `match.surrender_failed` | none | team, tick |
| `match.row_skipped` | none | round (the row's playlist index), reason (`too_few_players`, `too_many_players`, `missing_content`, `recent`), tick (the launch step) |

The respawn, capture-zone, co-op, race, shrinking-zone and victory events joined the route at the v0.7 roll-up
(ADR-8731), and `row_skipped` with ADR-8881; `MATCH_EVENT_KINDS` lists all 38 kinds and the logic graph editor's
palette shows them. A checkpoint
entity tagged `ObjectiveEntity(<checkpoint id>)` hears its racer events.

The unit is the entity that was that contestant on the fixed step the event happened (the session adapter records it
in `MatchEventUnits`). When that entity has despawned by the time the route runs, nobody hears the event as its unit
and `match_<kind>_unit` is removed, even if a new entity has reused its index. An event the adapter did not send (a
client's replicated copy) has no unit entity either.

Keys are bare entity-scope keys, one set per kind: `match_<kind>_<field>`, for example `match_flag_captured_team`,
`match_flag_captured_unit` or `match_player_scored_reason` (`regia::logic_bridge::rules::match_key`). Per-kind keys keep
a capture's fields when the same step also sends the carrier's `match.player_scored`. A field the event does not carry
is removed. Several events of one kind to one entity in
frame leave the last one's keys. The route runs in `PostUpdate` (once per frame, after every fixed step of the frame), so graphs take the events on their next
tick.

```ron
// On the level's game-mode entity (MatchEventListener), with `door` in its blackboard:
(name: "open_gate", event: EntityEvent("match.flag_captured"),
 condition: Compare(key: "match_flag_captured_team", op: Eq, value: 1.0),
 actions: [(task: "logic::DisableEntity", set: { "target": BbRef("door") })])
```
