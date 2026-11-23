# Editor autosave and crash recovery

ADR-9770 (merged by ADR-7381). Code: `regia_editor::doc_autosave`, `regia_editor::autosave_agent`,
`regia_editor::tasks`. Spec: `docs/specs/19-editor.md` § 7. Crash reports themselves are on
[Editor crash reports](editor-crash-reports.md).

## What is autosaved

Every `autosave.interval_minutes` (default 5; 0 turns it off) the editor autosaves:

- the open level, when its undo stack is dirty, to `<project>/.regia/autosave/<stem>.level.ron`;
- every asset document with unsaved edits, to `<project>/.regia/autosave/docs/<id>.autosave`, with an
  editor-only index `docs/index.ron` (kind, original path, label, size, time).

The documents come from a registry: each asset editor registers its dirty documents, their bytes (what
its save would write) and a restore hook. Registered kinds: `matgraph` (every material graph, function
and instance tab), `ui_design`, `procgen`, `behavior_tree`, `state_tree`, `table` (curve tables,
choosers and data tables), `sound_cue`, `vfx`, `conversation`, `anim_graph`, `ui_anim`, `sequence` and
`prefab`. The `vfx` bytes are an editor-only envelope: the effect text a save writes plus the
disabled-module stash, so a recovered effect keeps its disabled modules (with values).
`editor.autosave_state` lists them under `kinds`.

**More kinds** (w11 forms-undo-autosave, ADR-7731): `cloth` (the cloth panel's open `.cloth.ron` /
`.soft.ron`: the bytes its save writes), `ragdoll` (the Ragdoll editor's profile, a new profile not saved yet
too), `vehicle` (Vehicle Tuning's `.vehicle.ron`, with the comments its save keeps), `abilities` (the
Abilities inspector's asset editor: the open `.attrs.ron`, `.effect.ron` or `.ability.ron` text) and `loc`.
Restore opens the original file in its editor and puts the recovered edit over it, unsaved until a save. The
Localization panel writes each translation when it is applied, so its `loc` document is the edit box: a selected
cell whose typed text is not applied yet (labelled `<key> (<locale>)`, no original file); Restore selects the cell
again with the typed text, and **Apply** writes it.

**Sequences** (`sequence`): every `.seq.ron` the Sequence Curves panel opened or saved
(`editor.seq_curve_open` / `editor.seq_curve_save`) is a document with its own dirty flag, independent of
the level's undo stack: it is dirty while the player's sequence differs from what its file holds, so any
edit (curve, section strip, binding rename, undo, redo) sets it and a save or undoing back to the saved
sequence clears it. `editor.seq_curve_state` reports the selected player's `path` and `dirty`. Restore
opens the recovered text as a new player at the original path, dirty until a save. The Shots panel's
shot sequence has no file of its own and is not autosaved.

**Prefab edit mode** (`prefab`): while a prefab is open, the mode's own undo history is its dirty flag
(`editor.prefab_mode` reports `dirty`); `editor.save_prefab` clears it. The autosave holds the text
`editor.save_prefab` would write (an edit that has two roots for a moment is still kept). Restore
re-enters prefab edit on the original path with the recovered contents (the level that is open is
stashed as usual), dirty until `editor.save_prefab` or `editor.close_prefab { save: true }`; closing
without a save drops the edit and its autosave. While a prefab is open, the level autosave keeps the
stashed level when it had unsaved edits, and closing the prefab gives the level its dirty flag back.

**Documents kept behind the open one** (P-322, ADR-8365): the Tables panel and Vehicle Tuning keep a
table or `.vehicle.ron` that was open before the one they show in memory, with its unsaved edits, so a
shared undo can bring it back. Those documents are autosaved too while dirty (the newest copy of each
file, under the same `table` / `vehicle` kind, so Restore opens them as usual), unless the panel has the
same file open again: then the open copy is the one that counts. Editors register them through
`DocAutosaveRegistry::register_retained`.

**Runtime spawns**: a level save (`editor.save_level`, File → Save) and the level autosave first adopt
the entities the runtime methods `scene.spawn` / `scene.spawn_primitive` made (no guid, no level) into
the open level, so they are saved whether or not an editor frame ran in between.

The snapshot is taken on the main thread; encoding and writing run on one autosave worker thread, in
order. At most one autosave tick is queued at a time: a tick that comes due while the last one is still
queued or running waits, then writes the latest snapshot (`editor.autosave_now` queues anyway). A write
that panics is reported in `last_error` and the worker keeps running. Every file is written to a temp
name and renamed. Index rows whose `id` is not 16 lowercase hex digits, or whose `file` is not a plain
file name, are ignored: an edited `index.ron` never reads or deletes a file outside `docs/`. `.regia/` is editor state: the cook and the
packager read `assets/` only, so autosaves are never cooked or packaged.

A manual save of a document (it turns clean) removes its autosave on the next frame. A clean exit
removes every autosave except the ones still offered for recovery. An agent's `quit.request` keeps the
dirty level and documents for the next launch.

## Recovery

When a session ends without a clean exit, the next launch shows the **Crash recovery** window: one row
per level autosave newer than its level and per document, with its kind, original path, time and size,
and **Restore** and **Discard** buttons, then **Restore all** and **Discard all**. Restore opens a level
as the open level and a document in its panel at its original path; both stay dirty until saved, and the
autosave file stays until then. A project without `.regia/autosave/docs/` opens unchanged.

## Play-in-editor panics

A panic in game code during play logs its backtrace through ADR-7205's `panic_trace` (the integration kept that
one implementation, ADR-7381); see [Play-in-editor modes](pie-modes.md).

## Cancel

The toolbar's compiling label has **Cancel** while the game build runs: it sets the build's token (the same one
`editor.cancel_job { id: "build" }` sets), cargo is killed and nothing is copied, so the loaded game library stays.
The Content Browser's Import runs as ADR-7205's background job with its own Cancel button; a cancelled import removes
the files it had copied (`ImportReport::cancelled` is `true`).

## `editor.autosave_state`

Read-only. No params.

```json
{ "method": "editor.autosave_state", "params": {} }
```

Returns `{ interval_s, ticks, completed, in_flight, last_tick_us, last_level, last_error, kinds, dirty:
[{ kind, label, original }], tasks: [{ id, label }], level_snapshot }`. `last_tick_us` is the main-thread cost of the
last tick (snapshots and queueing).

`level_snapshot` is `{ reflected, reused, full }` for the last level autosave snapshot, `null` before the first one.
The level autosave is incremental (ADR-7748): an entity whose saved components did not change since the last tick is
not reflected again; its components come from the snapshot cache. `reflected` counts the entities serialized on the
main thread, `reused` the ones taken from the cache, and `full` is `true` when the cache started over (the first tick,
another level, a newly registered type). The file is byte-identical to a full snapshot's. After one edit on a big
level, `editor.autosave_now` followed by `editor.autosave_state` reports `reflected: 1`, also with the editor's
shell frames drawn in between (test `autosave_incremental_shell`). Only the reflection is incremental: the tick still
walks every entity of the level on the main thread (about 65 ms at 20 000 entities on a loaded host).

## `editor.autosave_now`

Autosaves the dirty level and documents now and waits for the write (up to 30 s).

```json
{ "method": "editor.autosave_now", "params": {} }
```

Returns `{ level, docs: [id], main_thread_us, errors: [string] }`.

## `editor.recovery_list`

Read-only. No params. Returns `{ items: [{ id, kind, label, original, autosave, size, saved_unix_ms }] }`.
`id` is `level:<autosave file name>` or `doc:<id>`.

## `editor.recovery_restore` / `editor.recovery_discard`

`ids` (array of strings, optional): rows from `editor.recovery_list`; omitted or empty means every row.

```json
{ "method": "editor.recovery_restore", "params": { "ids": ["doc:5f1c0b2e9a7d3c41"] } }
```

Return `{ restored: [id], failed: [{ id, error }] }` and `{ discarded: [id], failed: [{ id, error }] }`.

While a prefab is open (`editor.open_prefab`), a restore that includes a `level:` row (also an omitted or empty
`ids` when a level row is offered) is refused as a whole with `prefab_open` and changes nothing: the restored
level would replace the level the prefab session keeps stashed. Close the prefab (`editor.close_prefab`) first,
or restore only `doc:` rows. The recovery window and the command palette's `recovery.restore` refuse the same way
("Close the prefab editor first (editor.close_prefab) to restore an autosave").

## `editor.cancel_task`

`id` (string, required): `game_build` or `import`. Returns `{ cancelled: true, id }`. `game_build` sets the
running build's token (the same one `editor.cancel_job { id: "build" }` sets: cargo is killed and the loaded
game library stays). `import` stops the Content Browser's background import job (`editor.import { async }` and
`editor.cancel_job` drive the same job, ADR-7205); a cancelled import removes the files it had copied. Only the
game build is listed in `editor.autosave_state`'s tasks (ADR-7381).

```json
{ "method": "editor.cancel_task", "params": { "id": "game_build" } }
```

## Version history

ADR-7928. Code: `regia_editor::version_history`. Every autosave of the open level or an asset document, and
every manual save of one, also keeps a **version**: a copy of the bytes it wrote, in
`<project>/.regia/history/<target id>/<unix ms>-<seq>-<source>.version` with a `target.ron` naming the
level or document (kind, project-relative original path, label). A level version is the single-file
`.level.ron` text, also for a `.level.d` level. The autosave worker writes every version, after the
autosave itself, so versions never slow a frame or race each other; a version that cannot be written is
reported in `editor.autosave_state`'s `last_error` and never fails a save.

- **Sources**: `autosave` (the periodic tick and `editor.autosave_now`), `save` (a level save through
  File ▸ Save or `editor.save_level`; a document that turns clean, because its editor saved it), and
  `before_restore` (a document's unsaved edits, kept just before a version is restored over them).
- **Retention**: the newest `history.keep_autosaves` autosave versions and the newest `history.keep_saves`
  save and before-restore versions stay per level and document (default 10 each; 0 keeps none); then, while
  the whole store is over `history.max_mb` (default 512), the oldest version anywhere goes. The version just
  written is never pruned. A copy equal to the newest version of the same source is not written again (a
  revert, or an autosave with nothing new).
- **Preview** (`editor.version_preview`) reads a version and changes nothing: its text (up to 64 KB) and,
  for a level, its entity names.
- **Restore** (`editor.version_restore`) never writes a file. A level version replaces the open level's
  contents as **one undo entry** (Edit ▸ Undo or `editor.undo` brings the replaced contents back; older undo
  entries keep finding entities whose guid survives); the level is dirty until saved, and must be the open
  level. A document version opens in its editor as unsaved edits through the autosave registry's restore hook,
  also as one undo entry; when the document had unsaved edits, they are kept first as a `before_restore`
  version.

**File ▸ Version history…** opens the window: the level or document (a picker when there are several), its
versions (time, source, size), **Preview** and **Restore**, and the read-only preview. Every control is a
focusable widget: Tab moves between them (also into the read-only preview text and out of it again), Enter or
Space presses (test `version_history::tests::the_window_works_from_the_keyboard`). The open window reads the
history folder only when it changed: after a version is written or pruned in this session, when another level or
document is picked, after Preview or Restore, and when the window is opened again (ADR-8435; a version another
process writes shows on reopening).

`.regia/` is editor state: versions are never cooked, packaged, or copied by Duplicate project.

## Project duplicate and backup

**File ▸ Duplicate project…** (`editor.duplicate_project`) copies the open project's source files to a new
folder: everything except `target/`, `build/`, `dist/`, `.git/` and `.regia/` at the project root, any
`.regia/` below it, any folder holding a `CACHEDIR.TAG` (a cargo target), and symbolic links (listed under
`skipped`). The copy goes to a staging folder beside the destination and is renamed into place, so a failed
copy leaves no half project; the open project stays open. `regia upgrade-project --write --backup <dir>` makes
the same copy into `<dir>/<project>-backup-<UTC YYYYMMDD-HHMMSS>/` before anything is migrated
([Command-line reference](../reference/cli.md)), with one difference: the backup keeps the files directly inside
`.regia/` (or a legacy `.ferrite/`), such as the editor layouts and views `--write` migrates and `agent.toml`, and
leaves out only that folder's subfolders (cooked output, caches, the asset database, autosaves, version history,
the agent endpoint and token) (ADR-8435).

## `editor.versions`

Read-only. `path` (string, optional): the project-relative path of the level or document; omitted = the open
level.

```json
{ "method": "editor.versions", "params": { "path": "assets/maps/main.level.ron" } }
```

Returns `{ target: { id, kind, original, label }, versions: [{ id, source, saved_unix_ms, size }], targets:
[{ id, kind, original, label }], store_bytes }`, newest first. `targets` lists every level and document that
has versions.

## `editor.version_preview`

Read-only. `id` (string, required): a version id from `editor.versions`. Returns `{ id, target, source,
saved_unix_ms, size, text, truncated, entities }` (`text` is `null` for bytes that are not UTF-8; `entities`
is `null` for a document).

## `editor.version_restore`

`id` (string, required). Returns `{ restored, target, undo, kept }`: `undo` is the undo entry's description,
`kept` whether a `before_restore` copy was written.

```json
{ "method": "editor.version_restore", "params": { "id": "4f0c2b1a9e8d7c65/1763246400000-000003-save" } }
```

## `editor.duplicate_project`

`dest` (string, required): the new folder, absolute or relative to the folder that holds the project. It must
not exist (an empty folder is fine) and must be outside the project. Returns `{ root, files, bytes, skipped }`.

```json
{ "method": "editor.duplicate_project", "params": { "dest": "My Game copy" } }
```

## Errors

| Code | When | Fix |
|---|---|---|
| `no_project` | No project is open. | Open a project first. |
| `invalid_ids` | `ids` is not an array of strings. | Pass ids from `editor.recovery_list`. |
| `prefab_open` | `editor.recovery_restore` of a `level:` row while a prefab is open. | Close it with `editor.close_prefab`, or restore only `doc:` rows. |
| `invalid_task_id` | `id` is missing. | Pass `game_build` or `import`. |
| `no_such_task` | That task is not running. | `editor.autosave_state` lists running tasks. |
| `invalid_version_id` | `editor.version_preview` / `editor.version_restore` without `id`. | Pass an id from `editor.versions`. |
| `invalid_path` | `editor.versions` `path` is not a string. | Pass a project-relative path, or omit it. |
| `no_such_version` | No version has that id (or the id is not a version id). | `editor.versions` lists them. |
| `version_unreadable` | The version file cannot be read or does not parse. | `editor.version_preview` shows its text. |
| `version_level_not_open` | `editor.version_restore` of a level version while another level is open. | Open the level with `editor.open_level`, then restore. |
| `version_no_editor` | No editor restores the document's kind. | `editor.autosave_state` lists the kinds. |
| `prefab_open` | `editor.version_restore` of a level version while a prefab is open. | Close it with `editor.close_prefab`. |
| `invalid_dest` | `editor.duplicate_project` without `dest`. | Pass the new folder. |
| `not_a_project` | The open project has no `Regia.toml`. | Open a project. |
| `duplicate_dest_exists` | `dest` exists and is not an empty folder. | Choose a new folder. |
| `duplicate_dest_inside` | `dest` is inside the project. | Choose a folder outside the project. |
| `duplicate_failed` | A folder could not be created or a file could not be copied. | Check permissions and free space. |

A row id that matches nothing is reported in `failed` (`no such recovery row`), not as an error.

## Tests

`cargo test -p regia_editor --test autosave_crash` runs a child editor process that edits a level and a
material graph, waits for one autosave tick and aborts; the relaunch offers both and restores them
byte-identical, and the agent methods run through the dispatcher. Unit tests: `cargo test -p regia_editor
--lib autosave` and `cancel_`. PIE panic backtraces are ADR-7205's (`panic_trace`, `book/agents/pie-modes.md`).
Sequences and prefab mode: `autosave_sequence_restores_byte_identical_text`,
`autosave_prefab_restores_the_edit_and_close_removes_it`, `autosave_level_includes_runtime_spawns`,
`prefab_edit::tests::a_closed_prefab_leaves_nothing_in_the_next_prefab_session`;
`cargo test -p regia_editor --test undo_agent_smoke runtime_spawns_are_saved_with_no_frame_between`.
Retained documents: `cargo test -p regia_editor --test undo_global_panels an_inactive_dirty_` and
`--lib retained_documents_are_listed_and_written_once_beside_the_open_ones`.
Version history and duplicate (ADR-7928): `cargo test -p regia_editor --lib version_history project`
(retention, size cap, hooks, preview, undoable restore, the window's keyboard path, the copy rules) and
`cargo test -p regia_editor --test version_history_agent` (the methods over the dispatcher, with undo);
`cargo test -p regia_cli --bin regia backup_tests` for `upgrade-project --backup`.
