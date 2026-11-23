# Logic graph

The **Logic Graph** tab edits a `.logic.ron` logic graph: a list of *when event E happens, if condition C holds,
run actions A1, A2, …* rules, drawn as nodes on a canvas. Events sit on the left, conditions in the middle and
actions on the right; a rule is one event node wired through zero or more condition nodes into one or more action
nodes, in the order they run. While the game plays the tab shows which rules fired, matched or were rejected on
the newest tick of a running graph, and that graph's variables. Depth: [Logic graphs (agents)](../../agents/logic-graph.md),
the file format [`docs/formats/logic-ron.md`](../../../docs/formats/logic-ron.md) and spec 102.

## Opening it

The tab sits in the group under the viewport in the default layout; **View ▸ Reset layout** brings it back if you
closed it. An agent brings it forward with `editor.layout_apply {"open": "logic_graph"}`. Double-clicking a
`.logic.ron` in the Content Browser, clicking a `.logic.ron` link in the Output Log or calling `editor.open_asset`
opens the file in the tab and brings the tab forward. The tab has no palette command and no key.

## Walk-through: a door that counts its visitors

1. In the Content Browser, open the folder the graph should live in (for example `assets/logic`) and choose
   **New ▸ Logic graph**. The browser writes an empty graph (`logic_graph-1.logic.ron`, or the next free number),
   selects it and says so in its toast. Rename it if you like (**Rename / Move…** in its context menu). Inside the tab,
   **New…** with a path does the same without the browser: type `assets/logic/door.logic.ron` and click **Create**
   (the file is written on the first **Save**).
2. Double-click the file. The tab opens it; its path shows in the tab's top bar.
3. In the **Palette** on the left, under **Events**, click **Entity event**. An event node appears in the events
   column. Select it: the inspector names the rule (**Rule name**, for example `visit`) and holds its event as RON
   (**Event (RON)**: type `EntityEvent("visit")` and press Enter). The **priority** and **cooldown ticks** fields
   below it set the rule's order among rules on the same event and how often it may run.
4. Under **Conditions**, click **Key** for a condition node, select it and set **Condition (RON)** to `Key("locked")`.
   Its mode buttons switch the node between `leaf`, `all`, `any` and `not`: pick **not** so the rule runs only while
   the door is not locked.
5. Under **Actions**, click **logic::SetBlackboard**. In the inspector add the parameters `key` with the value
   `Literal(String("local/visits"))` and `op` with `Literal(String("increment"))` (**Add parameter**, then type the
   RON binding and press Enter). The `local/visits` variable needs a declaration: an agent adds it with the
   `set_local` operation (see the agent page), as the canvas has no locals editor yet.
6. Wire the rule: drag from the event node's output to the condition's input, then from the condition's output to
   the action's input. Dropping a wire on empty canvas opens **Add and connect**, which adds the node you pick and
   wires it in one step. Right-click a pin to remove its wire; right-click empty canvas for **Add node**; a node's
   menu has **Delete**.
7. The issue list under the canvas names each problem with its code (`dangling_wire`, `unknown_task`,
   `unknown_scope`, `duplicate_name`, `bad_value`, …) and the node it is about; clicking a row selects that node. A
   node with errors shows a warning sign.
8. Click **Save**. The tab writes the canonical `.logic.ron` and, next to it, `door.logic.layout.ron` with the node
   positions (the graph file itself has no positions). A graph with errors is not saved: the top bar says why.
   Re-saving an unchanged graph writes the same bytes.

Every edit is one undo step on the editor's undo stack: Ctrl+Z and Ctrl+Y (and `editor.undo`, `editor.redo`) undo
and redo node adds, wires, inspector edits and node drags in order with your level edits. **New…** and **Open…**
replace the open graph; save it first.

## Wire events to actions

A rule's event is one of the arms in the palette: **Start** (the graph's first tick), **Timer** (every N fixed
steps, with optional jitter), **Blackboard changed** (a watched key was written since the last tick) and **Entity
event** (a named event arrived in the entity's inbox). Sequence markers, input actions and network messages arrive
as entity events named `sequence_marker:<sequence>#<marker>`, `input_action:<action>` and `net_message:<id>`
(see [Integrations](../../agents/logic-graph.md#integrations-phase-3)).

Conditions read three scopes: `local/x` (the graph's own variables), `x` or `entity/x` (the entity's blackboard)
and `world/<board>/x` (a world blackboard the graph declares). A rule with no condition node always runs; two
condition nodes in a row must both hold.

Actions are the built-in `logic::SetBlackboard`, `logic::EmitEvent`, `logic::EnableEntity` and
`logic::DisableEntity`, plus every task the game registers (with the umbrella crate's `logic` feature:
`logic::PlaySequence`, `logic::PlayCue` and, with `lua`, `logic::CallLua`). A rule's actions run in wire order; a
task that is still running holds the chain until a later tick, and a failure stops it.

To run a graph, put it on an entity: the game compiles the file with `LogicGraph::compile` and spawns it with a
`Blackboard` and a `LogicInbox` (the agent page shows the code). Gameplay code, Lua (`regia.emit`) and agents
(`logic.emit`) send it events.

## Debug a running graph

Press Alt+P to play. The **Live** area lists the running graphs (`entity <id>`); pick one to watch. Each rule shows
what it did on the newest tick: **fired** (its actions started), **matched** (its condition held but cooldown or a
busy chain kept it from running), **rejected** (its condition was false) or **failed** (an action failed). The
variable table shows its `local/`, entity and `world/` values. If the running graph is not the open document the
area says **differs from the document**.

**Pause**, **Resume** and **Step** (run one tick, then pause) control the watched graph. A graph runs a whole tick at
a time, so the finest step past one tick is **Step to next fired rule**: it runs until a tick in which a rule fires,
then pauses.

Breakpoints: click the dot in front of a rule's event node (or select any node of the rule and press F9) to pause the
game's graphs that run this file at the end of a tick in which the rule fires; an event node's menu has **Break when
… arrives** for events that come through the inbox. The **Breakpoints** tab lists them: tick a box to enable or
disable one, **Go to** selects its node, ✕ removes it. Breakpoints are yours, not the file's: they are kept in
`.regia/logic_breakpoints.ron` and work in development builds. **Paused at rule …** says which one stopped the graph.

The **Watch** tab pins variables (type `local/n`, `world/gatehouse/alarm` or an entity key and press Enter) and
condition results (a condition node's menu, **Watch this condition**). Values update when the graph pauses; turn on
**Live** to refresh them every frame while it runs. Hovering a condition node's title shows whether it holds now.
While paused, the box under the Live controls evaluates a condition written like the inspector's Condition field,
for example `Compare(key: "local/n", op: Gt, value: 0.0)`. The wires of rules that just fired pulse for half a
second.

The open graph's issues also appear in the editor's **Problems** panel; a row opens the graph at its node.

Saving while the game plays reloads every running graph that uses the file: locals, the inbox and fire counts are
kept by rule name (**kept**), or the graph restarts when its running state cannot carry over (**restarted**). The
line under the picker counts what the last save did; a file that does not compile in the game is **rejected** and
the old graph keeps running.

## Actions

| Action | Control / key | Agent method |
|---|---|---|
| Create a graph file | Content Browser **New ▸ Logic graph**; the tab's **New…** | `editor.create_asset {"kind": "logic_graph"}`, `editor.logic_open {"create": true}` |
| Open a file | Double click in the Content Browser, an Output Log link, **Open…** | `editor.open_asset`, `editor.logic_open` |
| Add, wire, change, move or remove nodes; select a node | Palette, canvas menus, wire drags, inspector, node drags | `editor.logic_edit` |
| Read the graph, its issues and the live view | The canvas, the issue list, **Live** | `editor.logic_state` |
| Check a graph | The issue list | `editor.logic_validate`, `logic.compile` |
| Save | **Save** | `editor.logic_save` |
| Watch a running graph; pause, resume, step it | **Live** picker, **Pause**, **Resume**, **Step** | `editor.logic_edit {"watch": …, "control": …}` |
| Find a node to add | Palette **Search nodes**, a dropped wire's menu | `editor.logic_palette_search` |
| Declare, rename, retype locals and boards | **Variables** tab | `editor.logic_variables`, `editor.logic_variable_set`, `editor.logic_variable_remove` |
| Breakpoints | Rule dot, F9, event node menu, **Breakpoints** tab | `editor.logic_breakpoint_set`, `editor.logic_breakpoint_list` |
| Step to the next fired rule | **Step to next fired rule** | `editor.logic_edit {"control": "step_to_fire"}` |
| Watch values; evaluate a condition while paused | **Watch** tab, hover, the evaluate box | `editor.logic_watch`, `editor.logic_evaluate` |
| Open the graph at an issue's node | A **Problems** row | `editor.logic_goto_node` |
| Disabled rules | none | `logic.debug` |
| Send an event, read the runtime status, reload | none | `logic.emit`, `logic.status`, `logic.reload`, `logic.control` |
| Undo, redo | Ctrl+Z, Ctrl+Y | `editor.undo`, `editor.redo` |

## Keyboard

F9 sets or removes the breakpoint of the selected node's rule. Enter in **Search nodes** adds the best match; Enter in
the Variables, Watch and evaluate fields applies them. Tab and Shift+Tab move between controls, and Space or Enter
presses the focused button ([Editor guide](index.md)).

## Known limits

- Events, conditions and parameter values are edited as RON text in the inspector (no per-field widgets yet).
- Entity variables have no type or default in the Variables tab (the entity's blackboard holds them).
- Disabled rules are set through `logic.debug` only; breakpoints apply in development builds only.
- A graph runs from a prefab through its `LogicGraphRef` component: in prefab edit mode the banner's **New graph for
  this prefab** creates one next to the prefab and attaches it, **Open graph** opens it ([Prefab logic](../../agents/prefab-logic.md)).
  Saving a graph during play reloads a prefab's running copies only when the reference has no `overrides`.
- Without the game's task registry loaded in the editor, an unknown action task is a warning, not an error; the
  game checks it again when it compiles the graph.
