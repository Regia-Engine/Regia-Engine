# Smart-object definition editor (agents)

A smart-object definition (`.smartobject.ron`, format version 1, spec 39 § 3, `docs/specs/39-smart-objects-v2.md`)
describes what every object built from it offers: activity tags, requester rules and queries, conditions, behavior
payloads, and the slots (each with its offset, own tags, requester query, conditions, behaviors and entrances). The
editor's **Smart Object Definition** panel edits one definition as forms and places its slots and entrances in the
viewport (ADR-7736). The runtime side (claims, search, the live Smart Objects panel) is on
[Smart objects](smart-objects.md).

## In the editor

- **Content ▸ New ▸ Smart object** writes a definition with one slot, `slot_0`, at the object's origin
  (`editor.create_asset { kind: "smart_object", path: "assets/props/bench.smartobject.ron" }`).
- Open it in the panel (type the path, **Open**). The forms edit the definition's activity tags, activity merge
  (`union` or `slot_first`), requester rule (`definition_and_slot`, `slot_only`, `slot_else_definition`) and
  requester query (tag query RON, for example `AnyOf([Has("Role.Guard"), Has("Role.Civilian")])`), its conditions
  (type path plus `key=value; key=value` parameters) and behaviors (family plus RON payload); then each slot's name,
  start-enabled flag, position and yaw/pitch/roll, initial tags, the same activity, query, condition and behavior
  rows, and its entrances (kind, rank, position, rotation).
- Every change is one step on the editor's undo stack (Ctrl+Z); dragging a number is one step.
- Under the header the panel shows the runtime validator's verdict (`SmartObjectDef::validate`): "valid", or its
  message (a duplicate or bad slot name, too many slots or entrances, a non-finite offset, an over-deep query). The
  forms never refuse such an edit; **Save** (or Ctrl+S with the pointer over the panel) refuses an invalid definition
  with that message.
- A definition nobody edited is never rewritten. Saving writes `SmartObjectDef::to_ron`, the canonical text: a file
  with comments loses them, and the panel warns while the file still has them.
- An edited definition is kept by document autosave (kind `smart_object`) and offered back after a crash.

### Slot handles in the viewport

While a definition is open and **Slot handles** is on, selecting its **preview entity**, or a level object whose
`SmartObject::definition` names the open file, shows the definition's slots on that object: a grab point and a yaw
dial per slot (with a short ray along its facing, local −Z) and per entrance (with a line to its slot). The handles
take the place of the transform gizmo on that object. Dragging a point moves it in the plane facing the camera; its
offset changes by the drag taken into the parent's space (the object's for a slot, the slot's for an entrance), so
a turned or scaled object still gets the right local offset. Dragging a dial turns the slot or entrance about its own
up axis. Each drag is one undo step. A press beside the handles box-selects as usual.

## Methods

| Method | Class | Params | Result |
|---|---|---|---|
| `editor.smart_object_def_state` | read | — | `{ open, path, dirty, valid, validation, comments_warning, activity, activity_merge, user_rule, user_query, conditions, behaviors, slots, handles: { enabled, preview }, undo_depth }` |
| `editor.smart_object_def_edit` | assets | `path?`, `op`, op parameters (below) | the state plus `changed?`, `written?` |
| `editor.smart_object_slot_handles` | edit_scene | `entity?`, `enabled?`, `preview?` (`null` clears) | `{ enabled, preview, open, entity?, handles?: [{ name, position }] }` |
| `editor.smart_object_slot_handle_drag` | assets | `entity`, `handle`, `to?` (position handles), `angle?` (dials, radians), `steps?` | the state plus `entity`, `changed` |

`editor.smart_object_def_edit` ops (`path` on any op opens that file first; `slot` is an index or a slot name):

| `op` | Parameters | What it does |
|---|---|---|
| `get` (default) | — | Reads the open definition. |
| `open` | `path` | Opens a definition. |
| `set_activity` | `tags`, `slot?` | The definition's (no `slot`) or a slot's activity tags. |
| `set_rules` | `activity_merge?`, `user_rule?` | The merge and requester rules. |
| `set_user_query` | `query` (RON text or `null`), `slot?` | A requester query. |
| `set_conditions` | `conditions: [{ path, params? }]`, `slot?` | The conditions. |
| `set_behaviors` | `behaviors: [{ family, data? }]`, `slot?` | The behavior payloads. |
| `add_slot` | `name?` (default `slot_N`), `translation?`, `rotation?` | Appends a slot. |
| `remove_slot` | `slot` | Removes a slot. |
| `set_slot` | `slot`, `name?`, `enabled?`, `initial_tags?`, `translation?`, `rotation?` | Edits a slot. |
| `add_entrance` | `slot`, `translation?`, `rotation?`, `kind?`, `rank?` | Appends an entrance. |
| `remove_entrance` | `slot`, `entrance` | Removes an entrance. |
| `set_entrance` | `slot`, `entrance`, `translation?`, `rotation?`, `kind?`, `rank?` | Edits an entrance. |
| `save` | — | Saves (an unedited definition keeps its bytes; `written` is 0). |

A call is all or nothing: every op it implies is checked on a copy first. Each op is one undo step (`editor.undo`).
Handle names are `slot_<i>`, `slot_<i>_yaw`, `entrance_<i>_<j>` and `entrance_<i>_<j>_yaw`.

```json
{"method": "editor.smart_object_def_edit", "params": {"path": "assets/props/bench.smartobject.ron", "op": "add_slot", "name": "right", "translation": [0.4, 0.0, 0.0]}}
{"method": "editor.smart_object_slot_handles", "params": {"preview": 4294967300, "entity": 4294967300}}
{"method": "editor.smart_object_slot_handle_drag", "params": {"entity": 4294967300, "handle": "slot_1", "to": [5.4, 0.0, 1.0]}}
{"method": "editor.smart_object_def_edit", "params": {"op": "save"}}
```

## Errors

Errors are `F0002` (invalid params) with `data.reason`:

| Reason | When |
|---|---|
| `smart_object_def_no_project` | No project is open. |
| `smart_object_def_bad_path` | The path is not a relative `.smartobject.ron` inside the project. |
| `smart_object_def_io_failed` | The file could not be read or written. |
| `smart_object_def_parse_failed` | The loader refused the file; the message names `bad_definition` or `unsupported_version`. |
| `smart_object_def_no_definition` | No definition is open (pass `path`). |
| `smart_object_def_bad_index` | The slot or entrance does not exist. |
| `smart_object_def_bad_value` | A parameter has the wrong shape (a query that is not a tag query, a rank out of range, a non-finite number). |
| `smart_object_def_invalid` | Save refused: the runtime validator's message. |
| `smart_object_def_serialize_failed` | The writer failed. |
| `smart_object_handles_no_definition` | A handle call with no definition open. |
| `smart_object_handles_not_shown` | The entity is neither the preview entity nor an object naming the open file, or the handles are off. |
| `smart_object_handle_unknown` | The open definition has no handle of that name. |

A missing `handle`, `to` or `angle` is a plain `F0002` with the parameter named.
