# Shared undo history and recovery

Open **Window → Undo History**, or open the `undo_history` panel with
`editor.layout_apply`. Tab moves through history entries; Enter or Space moves to
that boundary. Focus also selects the entry's details. The panel uses the active
theme and UI scale. `editor.capture_panel {"panel":"undo_history"}` captures the
panel through the agent; `panel: "safe_mode"` captures the recovery controls.

`editor.history {}` retains `undo`, `redo`, depths, `dirty`, `cap`, and `remapped`.
It adds `entries` in chronological order, `index` (the number applied), and
`transaction` (the active label or null). Each entry has `index`, `description`,
`applied`, `documents`, and `touched`. Each touched item names `document` and
optional `entity`, `guid`, `asset`, and `property`. An empty details list means an
older command has not supplied details. `level` means the current edit level.

`editor.history_jump {"index": 0}` undoes every retained entry. An index of N
leaves N entries applied. Each command's revert or apply runs separately; jumps
do not replace the world with a snapshot. Indices refer to the current retained
history, so read history again after an edit, branch change, or eviction.

## Script transactions

```json
{"method":"editor.transaction_begin","params":{"label":"Arrange scene"}}
{"method":"editor.spawn","params":{"name":"Marker A"}}
{"method":"scene.spawn","params":{"name":"Marker B"}}
{"method":"editor.transaction_commit","params":{}}
```

The group is one undo entry. `editor.transaction_abort {}` reverts its applied
commands in reverse order. An empty commit adds no entry. Abort and empty commit
preserve the previous redo branch. Nesting is refused. Labels are nonempty and
limited to 1024 bytes by the agent contract.

A transport connection owns the group. Other connections may read history but
cannot mutate until it finishes. Disconnecting commits the group after that connection's queued notifications have
been dispatched and writes a warning to the log. Pending requests keep their
connection identity across close, including a queued begin; history remains
one step when the connection closes before the editor drains the queue. Undo, redo,
history jumps and replacement of the edit world are refused while a transaction
is active. Save and non-command operations retain their normal behavior; the
transaction groups undoable edits and does not roll back external side effects.
Wait for asynchronous edit jobs to finish before committing their transaction.

Extensions can use `UndoStack::transaction(world, label, |world| { ... })`.
The closure must use editing APIs that push `EditorCommand`s. Direct ECS writes
cannot be inferred. A panicking closure commits commands already applied before
resuming unwinding. Commands may override `touched()`; its default is empty.

## Hosted scene and document edits

The editor installs an optional `SceneEditHook` only in its edit world. Native
`scene.spawn`, `scene.spawn_primitive`, `scene.spawn_prefab`, `scene.duplicate`,
`scene.despawn`, `scene.reparent`, `scene.set_property`, `scene.add_component`,
`scene.remove_component`, `scene.apply_edits`, `scene.season` and `scene.possess`
then enter the same history. Season queries add no entry; its nonpersistent driver
uses the `session` document and does not mark the level file dirty. Possession
restores both the possessed entity and the camera view target.
A batch is one step (or part of the explicit transaction); dry runs add none.
Play worlds and runtime applications without the hook remain undo-free.

Vehicle and table edits in the hosted editor use the shared order. Their old
Undo/Redo controls follow that order, including edits in other documents. Undo
reactivates the affected document when needed. Asset edits retain their own dirty
flags and do not dirty the level. Standalone model instances without an editor
stack retain their existing local history API.

The Biome, Ragdoll and Material editors work the same way in the hosted editor.
Each edit is one entry that names its file in `documents` (the Biome entry also
names the field path in `touched[].property`, the Ragdoll entry names
`bones/<joint>`). A Biome `merge` chain (a drag) is one entry. The panels' own
Undo and Redo buttons, `op: "undo"` / `"redo"` of `editor.biome_panel_edit` and
`editor.ragdoll_panel_edit`, and the palette's `material.undo` / `material.redo`
step the shared history, and their `undo_depth` / `redo_depth` report its
depths. Undoing an edit of a file that is no longer open opens that file again
with its saved baseline, so `dirty` stays right. The Project Settings page's
input-map, UI-scale and physics-layer writes were already shared entries. They
now name `config/input.ron`, `assets/ui/project.uisettings.ron` or
`config/physics.ron` in `documents`.

## Recovery startup

Start `regia-editor --safe-mode` to skip extension and game-library loading,
Lua mods, autosave restore prompts, reopening the last level or tabs, and agent
server autostart. Saved layouts and recovery files remain available for a later
normal start. Two consecutive editor sessions that end without orderly teardown automatically
show the safe shell with a Startup recovery prompt on the next start.
The banner's **Restart normally** action uses the normal unsaved-document close
flow, then relaunches with the original arguments and `--normal-mode`.

`editor.safe_mode_state {}` returns `enabled`, `startup_prompt`,
`restart_requested`, and `skipped`. `editor.safe_mode_restart {}` requests the
same restart as the banner. Safe mode does not start a listener automatically;
these methods are also available to an explicitly hosted in-process dispatcher.

## Error reasons and permissions

New error reasons are lowercase `data.reason` values inside the existing RPC
error envelope:

| Reason | Meaning |
|---|---|
| `history_index` | Index is missing, malformed, or outside retained history. |
| `transaction_nested` | Begin was called while a transaction is open. |
| `transaction_missing` | Commit or abort has no open transaction. |
| `transaction_active` | History navigation or edit-world replacement was requested during a transaction. |
| `transaction_owner` | Another transport owns the open transaction. |
| `safe_mode_inactive` | Normal restart was requested outside safe mode. |

History and recovery state require Read. History jumps and transaction operations
require EditScene. Normal restart requires Tools. Existing validation and
permission errors still apply.
