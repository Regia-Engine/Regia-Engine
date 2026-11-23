# Review notes and session Activity

Use review markers for discussion tied to a level. They save with the level and are removed from
streamed cells and ordinary packaged levels/prefabs, including their descendants and prefab-added children.
A marker is a separate entity; attaching a note never strips the reviewed object. No scene format
version changes. Editor-only: runtime hosts do not register these methods.

Open **Window → Notes** or **Window → Activity**, or use `editor.command` with
`window.open_notes` / `window.open_activity`. In a viewport, **N** opens a note draft at the cursor's
surface; without a surface, select an entity to attach it. Rebind `review.add_note` with
`editor.keymap_bind`. Tab and Shift+Tab reach every filter, text field and action; Space or Enter
activates buttons. Notes can be framed from the list or their viewport badge. The badge counts the
initial note and replies. Controls use the appearance scale and theme tokens.

## Note contract

| Method | Parameters | Result |
|---|---|---|
| `editor.note_add` | `text` (required string), `entity` (optional attachment id), `position` (optional world `[x,y,z]`) | `{entity, note}` for the new marker |
| `editor.note_reply` | `entity` (required marker id), `text` (required string) | `{entity, note}` with the appended reply |
| `editor.note_resolve` | `entity` (required marker id), `resolved` (required boolean) | `{entity, note}`; false reopens |
| `editor.note_list` | `filter` (optional `all`, `open`, `resolved`, `mine`; default `all`) | `{notes: [{entity, note, position}]}` |

`entity` and `position` are mutually exclusive in `editor.note_add`. If neither is supplied, it uses
the selected entity. An attachment must belong to the open level. A position must contain three finite
numbers. Text must be nonblank and at most 16,384 UTF-8 bytes. Each thread holds at most 1,000 replies.
A note is `{text, author, created, resolved, thread: [{text, author, created}]}`; times are Unix
milliseconds. The author comes from the project's git `user.name`, read without a shell, falling back
to `editor.prefs_set {"values":{"review.author":"Reviewer name"}}`. `mine` compares that name.
The git name is cached until the project changes. All note mutations need `edit_scene` permission,
are undoable and mark the level dirty. Listing needs `read`. No file is written until a normal save.

```json
{"method":"editor.note_add","params":{"text":"Check the doorway clearance","position":[2,0,4]}}
{"method":"editor.note_reply","params":{"entity":4294967310,"text":"Adjusted the frame"}}
{"method":"editor.note_resolve","params":{"entity":4294967310,"resolved":true}}
{"method":"editor.note_list","params":{"filter":"open"}}
```

Use `editor.select {"entities":[id]}` and `editor.focus {"entities":[id]}` to select and frame a
listed marker. Use `editor.save_level` to save. IDs may change after reopening; query notes again.

Errors: `note_text` (missing, blank, wrong-type or oversized body), `note_entity` (invalid attachment
or marker id parameter), `note_placement` (ambiguous or invalid position, no placement, or attachment
to another note), `note_level` (no open level), `note_not_found` (no such review marker),
`note_thread_full` (reply limit), `note_resolved` (missing/wrong-type boolean), `note_filter` (unknown
filter), `note_edit` (reflection or spawn failed). Protocol permission errors and dispatcher `F0002` size-limit errors also apply.

## Activity contract

`editor.activity {"since":0,"source":"all","method":""}` needs `read` and returns
`{entries, next}`. Each entry is `{seq, unix_ms, source, who, method, entities, assets, outcome}`.
`since` is an exclusive unsigned sequence cursor, not a time. `source` is `all` (default), `agent`
or `user`; `method` is an optional case-sensitive substring. Use `next` as the next call's `since`.
`activity_filter` means a malformed cursor or filter.

Agent entries use the connection's client name, RPC method and audit outcome. User entries name the
edit/undo/redo command, including individual edits that coalesce in the undo stack. Both share an increasing sequence, so equal timestamps do not reorder them.
Agent edits are one call row rather than a duplicate user edit. The Activity panel's entity buttons
select live targets; removed entities are disabled. Existing `editor.select` provides that action to
agents. Asset paths are shown when the command or request identifies one. Request metadata is capped at 256 entity ids and paths of at most 4,096 bytes, including refused calls.

The browser retains at most 4,000 recent entries. A level switch folds prior edits into the
2,000-entry retained audit/edit journal; the current undo stack has a separate 2,000-entry edit journal.
On opening it can read the last 1 MiB of the existing audit JSONL; malformed/partial lines are skipped. Old audit rows have empty
target lists because the audit file did not store targets. Targets of commands without entity/asset
metadata are likewise empty. Hosts can supply those through `EditorCommand::activity_targets`;
standard scene edits and generic asset edits provide their targets. Sequence cursors belong to this process; start at zero after restarting.
The activity query itself appears in subsequent audit results, not in its own response.

## Verification

`cargo test -p regia_editor --lib review_notes` exercises dispatcher add/reply/resolve/list, save/load,
undo/redo, invalid inputs and the Notes keyboard path. `cargo test -p regia_editor --lib activity_panel`
checks call/edit order, equal-clock ordering and keyboard target selection.
`cargo test -p regia_asset_pipeline --lib cook_review_notes` checks stripped cooked payloads and streamed
partitions. The package review-note test checks ordinary staging without an editor dependency.
