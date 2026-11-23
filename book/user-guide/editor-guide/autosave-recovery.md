# Autosave and recovery

The editor saves a copy of your unsaved work in the background, and offers it back after a crash. It
covers the open level and every document panel that has unsaved edits: material graphs, procedural
graphs, UI designs and UI animations, behavior trees, state trees, tables, sound cues, VFX effects,
conversations, animation graphs, sequences, shot lists and prefabs. There is nothing to open: autosave
runs by itself, and the **Crash recovery** window appears at the next launch when something was left
behind. Depth: [Editor autosave and crash recovery](../../agents/editor-autosave.md), [Editor crash
reports](../../agents/editor-crash-reports.md).

## What is saved, and where

Every few minutes the editor writes:

- the open level, when it has unsaved edits, to `.regia/autosave/<name>.level.ron` in the project;
- each document with unsaved edits, to `.regia/autosave/docs/<id>.autosave`, with an index file.

The interval is the **Autosave interval (minutes)** preference (**Edit ▸ Preferences…**, category
**Autosave**): 5 by default, from 0 to 120, and 0 turns autosave off. The snapshot is taken on the main
thread; encoding and writing run on a worker thread, so editing does not wait for the disk.

Autosaves are editor state under `.regia/`: the cook and the packager read `assets/` only, so they are
never cooked or packaged. Saving a document removes its autosave. A normal exit removes every autosave
except the ones still waiting in the recovery window.

## Walk-through: recover after a crash

1. Reopen the project. If the last session did not exit cleanly, a **Crash recovery** window appears over
   the editor. It says `The editor closed unexpectedly.`
2. Under `A previous editor session left unsaved work behind:` each row shows the document's label, its
   kind (`level`, `matgraph`, `sound_cue`, ...), its original path, when it was saved and its size.
3. Press **Restore** on a row to bring it back: a level becomes the open level, and a document opens in
   its own panel at its original path. A restored item stays unsaved (dirty) until you save it, and its
   autosave stays until then.
4. Press **Discard** to throw a row away, or **Restore all** and **Discard all** for every row.
5. If the editor also wrote a crash report, the window shows its first line with **View report**, **Copy to
   clipboard**, **Open folder** and **Dismiss report**.

While a prefab is open, **Restore** of a level row is refused (close the prefab first); document rows
restore normally.

## Closing with unsaved work

Closing the editor with unsaved documents shows an **Unsaved changes** window listing them, with **Save
and exit**, **Discard and exit** and **Cancel**. A close requested by an agent (`quit.request`) exits without a prompt and writes
the dirty level's autosave first, so the next launch offers it.

## Actions

| Action | Control / key | Agent method |
|---|---|---|
| Change or turn off the interval | **Preferences ▸ Autosave** | `editor.prefs_set` `autosave.interval_minutes` |
| Read autosave state (interval, ticks, dirty documents, last error) | none | `editor.autosave_state` |
| Autosave now | none | `editor.autosave_now` |
| List what is offered for recovery | the **Crash recovery** window | `editor.recovery_list` |
| Restore rows | **Restore**, **Restore all** | `editor.recovery_restore` (`ids`, or none for all) |
| Discard rows | **Discard**, **Discard all** | `editor.recovery_discard` (`ids`, or none for all) |
| Cancel a running game build or import | **Cancel** beside the compiling label (build), **Cancel import** (Content) | `editor.cancel_task` with `game_build` or `import` |

## Version history

Every autosave and every save of a level or an asset document also keeps a version of it in the
project's `.regia/history/` folder. **File ▸ Version history…** lists the versions of the open level (or
pick another level or document), with the time, whether an autosave or a save made it, and its size.
**Preview** shows a version's text read-only (for a level, its entity count too). **Restore** puts the
version back as a new change: nothing is written until you save, and **Edit ▸ Undo** takes it back. A
document's unsaved edits are kept as a "Before restore" version first. Preferences ▸ Autosave sets how many
autosave and save versions stay per file (10 each) and the history's size cap (512 MB); the oldest go first.

**File ▸ Duplicate project…** copies the project's source files to a new folder (no build output, caches or
editor state). On the command line, `regia upgrade-project --write --backup <folder>` makes the same copy to
a dated folder before it migrates anything; it also keeps the settings files directly inside `.regia/` (the
editor layouts it upgrades, `agent.toml`), but not the caches and autosaves in its subfolders.

| Action | Editor | Agent |
|---|---|---|
| List versions | File ▸ Version history… | `editor.versions` |
| Preview a version | **Preview** | `editor.version_preview` |
| Restore a version | **Restore** | `editor.version_restore` |
| Duplicate the project | File ▸ Duplicate project… | `editor.duplicate_project` |

## Keyboard

Autosave and the recovery window have no shortcuts of their own. The buttons are focusable: Tab reaches
them and Space or Enter presses the focused button.

## Known limits

- Only documents that are registered with the autosave registry are covered; a panel that holds
  unsaved state outside it is not.
- A document autosave is dropped when you save it by hand, so an autosave never outlives a save.
- The recovery list reads `.regia/autosave/docs/index.ron`; an index row with a malformed id or a file
  name that is not a plain name is ignored.

## See also

[Editor autosave and crash recovery](../../agents/editor-autosave.md), [Editor crash
reports](../../agents/editor-crash-reports.md), [Editor](../editor.md), [Editor guide](index.md).
