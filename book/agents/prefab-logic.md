# Prefab logic (agents)

A prefab can carry its own logic: the reflected `LogicGraphRef` component names a project `.logic.ron`, and
`regia_ai::AiPlugin` compiles it into the entity's running `LogicGraph` whenever the component is added or changed.
Every instance of the prefab therefore runs the graph in play, exactly like a graph attached by code or by
`logic.compile`. The component's fields and save format are in `docs/formats/logic-graph-ref.md`; the graphs
themselves are on [Logic graphs](logic-graph.md); prefab edit mode is on [Prefab overrides and Edit in place](prefabs.md).
Design: P-165, ADR-8105 (N2), ADR-8566.

## In the editor

In prefab edit mode the banner at the top of the viewport has two buttons beside Save, Save & Close and
Discard & Close. Both are ordinary focusable buttons: Tab reaches them and Enter or Space runs them.

- **New graph for this prefab** writes an empty `.logic.ron` next to the prefab file (`door.prefab.ron` gives
  `door.logic.ron`, then `door_2.logic.ron`, … while the name is taken), adds a `LogicGraphRef` naming it to the
  prefab's root entity as one undo step, and opens the new graph in the Logic Graph panel. Undo takes the
  component off; the file stays (delete it from the Content Browser). Save the prefab to write the reference into
  the `.prefab.ron`.
- **Open graph** opens the graph the prefab references (the root's `LogicGraphRef`, else the first member that
  has one) in the Logic Graph panel and brings the panel forward.

Neither button drops unsaved work in the Logic Graph panel: save it first.

## `editor.prefab_logic`

| Param | Type | Notes |
|---|---|---|
| `op` | string | `"open"` or `"new"`, as the two buttons. |
| `discard` | boolean | Replace unsaved work in the Logic Graph panel (default `false`: refused). |

Result: `{ op, path, file, entity, created, discarded_unsaved, undoable }`. `path` is the project-relative graph
(what the component holds), `file` the file on disk, `entity` the entity that carries the reference, `created` is
`true` for `new`, and `undoable` is the undo depth after the call (`new` adds exactly one entry).

```json
{"jsonrpc":"2.0","id":1,"method":"editor.prefab_logic","params":{"op":"new"}}
```

Errors (`F0002`, `data.reason`):

| Reason | When |
|---|---|
| `prefab_logic_bad_op` | `op` is missing or not `open` / `new`. |
| `prefab_logic_not_editing` | No prefab is open in prefab edit mode (`editor.open_prefab` first). |
| `prefab_logic_no_project` | No project is open. |
| `prefab_logic_no_root` | The prefab has no root entity. |
| `prefab_logic_outside_project` | `new`: the prefab file is not inside the project. |
| `prefab_logic_none` | `open`: no entity of the prefab has a `LogicGraphRef`. |
| `prefab_logic_exists` | `new`: the prefab's root already has a `LogicGraphRef` (use `open`). |
| `prefab_logic_unsaved` | The Logic Graph panel has unsaved changes and `discard` is not `true`. |
| `prefab_logic_io` | The graph file could not be written, or the component could not be added. |
| `prefab_logic_open_failed` | The referenced file cannot be read or parsed. |

## Verifying a prefab's graph in play

1. `editor.open_prefab { path }`, then `editor.prefab_logic { op: "new" }`.
2. Edit the graph with `editor.logic_edit` and `editor.logic_save`; `editor.close_prefab {}` saves the prefab.
3. Place an instance, `editor.play`, then `scene.query` for `LogicGraphStatus` (or `logic.status { entity }`): the
   instance's graph is running. A reference that failed is in the log the runtime keeps
   (`LogicGraphRefLoads`) with one of the reasons in `docs/formats/logic-graph-ref.md`.
