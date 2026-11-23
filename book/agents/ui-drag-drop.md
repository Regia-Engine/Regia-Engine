# UI drag and drop (agents)

Guide topic: `agent.guide {"topic": "ui_drag_drop"}`. ADR-517.

## Model

A node with `DragSource { payload: { kind, data } }` starts an operation when it is dragged past the interaction
system's threshold (the same `DragStart` the sliders use). The payload `kind` is a type tag (`"item"`, `"tab"`); `data`
is a `UiValue`. A node with `DropTarget { accepts }` receives drops: an empty `accepts` takes every kind, otherwise only
the listed kinds. A target also receives drops that land on its `Interactable` descendants (the nearest target above
the node under the pointer wins). Nodes inside the dragged source's own subtree are never drop sites.

While an operation runs the source carries `DragActive` and the hovered target carries `DropHover { accepted }`
(`accepted: false` means "show a no-drop cue"). Game code reacts to `DragDropEvent` messages: `Started`, `HoverEnter`,
`HoverLeave`, `Dropped { source, target, payload }` and `Cancelled { source, reason }`. The engine moves nothing, so a
cancel needs no undo. `DragOperation::ghost_origin()` gives where a ghost belongs (pointer minus the grab offset).

An operation ends with a drop only when the button is released over a target that accepts the kind. Everything else
cancels:

| Cancel `reason` | When |
|---|---|
| `escape` | Escape was pressed. |
| `no_target` | Released over no drop target. |
| `rejected` | Released over a target that does not accept the kind. |
| `source_gone` | The source node was despawned or lost its layout. |
| `layer_changed` | An input layer opened that excludes the source. |
| `requested` | `cancel_drag` (or the agent's `cancel`). |

While an input layer is active only its subtree can be hovered.

## Methods

- `ui.drag_drop { action? }` with `action` `state` (default), `drag`, `drop` or `cancel`. Mutating (permission
  `edit_scene`), like the other `ui.*` writes.
  - `state` returns `{ active, source, kind, data, position, grab_offset, hover: { node, accepted } | null, sources:
    [{ node, kind }], targets: [{ node, accepts }] }` (the middle fields only while `active`).
  - `drag { from, to | at, release? }` queues five frames of real input: move onto `from`, press, move 6 px (starts the
    operation), move onto `to` (a node, as `ui.query` filters) or `at` (`[x, y]` physical px), release. With
    `release: false` the button stays down and the operation stays open for `state`, `drop` or `cancel`. `from` needs a
    `DragSource`; `to` needs a `DropTarget`. Returns `{ from, start, end, released, queued_frames }`.
  - `drop { to? }` moves onto `to` when given, then releases. Returns `{ queued_frames }`.
  - `cancel` cancels the running operation and releases the button. Returns `{ cancelled, queued_frames }`.
- `ui.query` nodes carry `drag_kind` (the payload kind or `null`) and `drop_accepts` (the list or `null`).

Effects show after the frames run: use `play.step` or `play.wait_until`.

```json
{"method": "ui.drag_drop", "params": {"action": "drag", "from": {"name": "sword"}, "to": {"name": "slot_3"}}}
{"method": "ui.drag_drop", "params": {"action": "drag", "from": {"name": "sword"}, "to": {"name": "slot_3"}, "release": false}}
{"method": "ui.drag_drop", "params": {}}
{"method": "ui.drag_drop", "params": {"action": "cancel"}}
```

## Errors

| `reason` | `data.code` | When |
|---|---|---|
| `ui_drag_active` | `F0002` | `drag` while an operation runs. |
| `ui_no_drag` | `F0002` | `drop` or `cancel` with no operation. |
| `ui_not_draggable` | `F0002` | `from` has no `DragSource`. |
| `ui_not_drop_target` | `F0002` | `to` has no `DropTarget`. |
| `ui_drag_target_required` | `F0002` | `drag` without `to` or `at`. |
| `ui_node_off_window` | `F0002` | `from` or `to` draws into a camera texture or a world panel; the window pointer cannot press it (use `ui.world_click`). |
| `ui_node_required` | `F0002` | `drag` without `from`. |
| `ui_bad_param` | `F0002` | Wrong type for `action`, `from`, `to` or `at`. |

Node lookup, disabled and invisible nodes answer with the codes in the [UI](ui.md) page (`ui_node_not_found`,
`ui_node_ambiguous`, `ui_node_disabled`, `ui_node_not_visible`).

Not covered yet: hover-to-switch (a tab bar activating a tab while a payload hovers it) and a built-in ghost node.
