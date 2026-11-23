# Shots panel (editor)

The editor's **Shots** tab is a small curve editor for the shot sequence that is loaded in the app
([camera paths and shot sequences](camera-paths.md)). It edits one scalar track of one shot at a time:
**fov**, **roll**, and, for a shot with depth of field, **focus_distance** and **aperture**. Every edit goes
straight into the loaded sequence (the clock and play state stay), so the camera preview follows.

## In the panel

- Pick the **Shot** and the **Track** from the two drop-downs.
- Drag a key to move it in time and value. Double-click empty space to add a key. Right-click a key for its
  menu: delete it, or set the ease of the segment that starts there (a constant key is `Step`, a linear key
  `Linear`, a cubic key `Smooth`). The grid pans with the secondary button and zooms with the wheel.
- The playhead shows the current time of the shot; scrubbing it moves the sequence clock.
- A refused edit (a time outside the shot, a value outside the track's range, two keys at one time) shows its
  message under the curve and changes nothing.

## The shot list as a document

The loaded list is a document (ADR-8730): the path row at the top of the panel opens a project `.shots.ron`
(**Open**) and saves the list (**Save**, or Ctrl+S / Cmd+S while the pointer is over the panel and no text field has
focus). A `● unsaved` mark shows while the list differs from what its file holds. Every edit counts, whether it
came from this panel, the Sequence Curves timeline's cut row (Add cut, move, trim, delete, fades: it edits this list
when the sequence player has no shot list of its own), undo or redo, or an agent call; undoing back to the saved list
is clean again. A list loaded without a file (`render.sequence`, the timeline's first Add cut) is unsaved and has no
path until a save gives it one. Saving an unchanged list again writes the same bytes. Opening is refused while the
loaded list has unsaved edits (pass `discard: true` to drop them). After an Open, undoing a cut edit made on the
previous list does nothing to the new one. Content Browser **New ▸ Shot list** makes a new `.shots.ron` (one still
shot, 5 s at 30 fps; see [Editor creators](editor-creators.md)).

A dirty list with a file autosaves with the editor's documents (kind `shots`,
[editor autosave](editor-autosave.md)): the autosave holds the text a save writes (the current format version,
whatever number the list carries in memory), and Restore loads it back at its path, unsaved until you save it.

**Closing the editor.** The close prompt lists a dirty shot list. **Save and exit** writes a list that has a file. A
list with no file (detached, or adopted and never saved) is listed as "an untitled shot list (not saved by Save and
exit: save it from the Shots panel first)": Save and exit goes ahead without it, as Discard would, so Cancel and
Save with a path if you want to keep it.

**Another list in its place (ADR-8940).** When something other than the panel loads a list into the shot player
(`render.sequence { action: "load" }`, `camera.set_path` on an empty player, a host calling `load` or `clear`), the
document is detached from its file: the path is cleared, the path row stops naming the old file, the new list shows
`● unsaved`, and undoing a cut edit made on the old list does nothing to the new one. **Save** then needs a path:
without one it answers `shot_panel_detached`, so the opened file is never overwritten with the other list. Saving to a
path gives the new list its own file.

**A sequence player's own shot list (ADR-8940).** A list the host set on a sequence player
(`SequencePlayer::set_shots`) is not loaded in the shot player. **Edit player's shots** (shown when the Sequence
Curves panel's player, or any player, has its own list) takes it into the panel as an unsaved document with no path:
the curve here and the timeline's cut row then both edit the player's list. Save with a path (Save As) writes it as
`.shots.ron` at the current format version, whatever version number the list carries in memory; after that it is a
document like an opened one (a second save writes the same bytes, edits mark it unsaved). If the player is despawned
the panel goes back to the shot player's list. A crash-recovered adopted list (one saved to a file, then edited) is
restored into the shot player at its file's path, not back into the sequence player: the recovery row keeps the file,
not the host.

**Camera position and rotation keys (ADR-7726).** The Track drop-down also lists **position** (the shot camera's
path keys, `camera.path.keys`) and **rotation** (its orientation keys, `camera.rotation`). With one of them picked
the panel draws the path from above (X to the right, Z down) with its keys, a ring at the playhead's eye and, for
rotation, each rotation key's view direction; below it a key list with each key's time and value (x, y, z metres for
a position; yaw and pitch degrees for a rotation, which is the orientation before the roll track). Dragging a field
moves the key (one undo entry for the whole drag; a typed value is one entry). **Add key at playhead** keys the
preview's own pose at the playhead (so the preview does not jump), and **Delete key** removes the selected key; each
is one undo entry. The path keeps at least one key (`shot_panel_last_key`). Rotation keys are stored as unit
quaternions; a moved rotation key keeps its ease. With no rotation keys the look-at rule orients the camera; the
look-at rule itself (`Heading`, `Point`, `Track`) is edited with `camera.set_path` or in the file. The curve draws
`Smooth` segments as straight lines; the playback still eases them.

## Agent methods

- `editor.shot_panel_state { shot?, track? }` (read-only) selects the shot (by name) and track if given and
  returns `{ loaded, shots, shot, shot_name, duration, track, tracks, range, has_track, selected, keys:
  [{ time, value, ease }], error, path, dirty, host }`. With nothing loaded it returns `loaded: false` (with `path`,
  `host` and `dirty: false`). `path` is the document's project-relative file (`null` before a save, and after another
  list replaced the opened one); `dirty` is true while the list differs from that file. `host` is `"shot_player"`, or
  `{ "player": <entity> }` for an adopted sequence player's own list. It is a read: it reports a pending detach
  (`path: null`, `dirty: true`) without committing it; the next mutating Shots method or panel draw commits it.
- `editor.shot_panel_open { path, discard? }` (needs `edit_scene`) opens a project `.shots.ron` into the shot
  player, stopped at zero, and returns `{ path, shots, dirty }`.
- `editor.shot_panel_save { path? }` (needs `assets`) writes the list atomically to `path`, else to its document's
  file, makes that the document's path and returns `{ path, bytes, dirty }`. The file always carries the current
  format version. A failed save changes nothing and the list stays dirty. A read-only destination file is replaced
  (as level saves do); a folder that cannot be written answers `shot_panel_io`. With no `path` after another list
  replaced the opened one, it answers `shot_panel_detached`.
- `editor.shot_panel_adopt { player?, discard? }` (needs `edit_scene`) takes the sequence player's own shot list as
  the document (unsaved, no path; the panel's **Edit player's shots**) and returns `{ player, shots, path, dirty }`.
  `player` is an entity id (default: the Sequence Curves panel's player when it has a shot list, else the first player
  that has one). Refused while the loaded list has unsaved edits unless `discard: true`.
- `editor.shot_panel_edit { op, shot?, track?, index?, time?, value?, ease? }` (needs `edit_scene`) does one
  edit and returns `{ applied, selected, keys }`:
  - `op: "add"` with `time` and `value` inserts a `Linear` key.
  - `op: "move"` with `index`, `time`, `value` moves a key; its ease travels with it and the keys re-sort.
  - `op: "remove"` with `index`.
  - `op: "ease"` with `index` and `ease` (`Linear`, `Step` or `Smooth`).
  The whole sequence is validated before it is swapped in.
  With `track: "position"` or `"rotation"` the same ops `add`, `move` and `remove` edit the shot camera's path or
  rotation keys, with `value` `[x, y, z]` (metres) or a quaternion `[x, y, z, w]` (stored normalized); each call is
  one undo entry, and the keys come back as `[{ time, value }]` (rotation keys also carry `ease`). The panel's
  buttons and fields run the same code, so the same calls write the same file bytes.

Parameter details: `shot` is an optional string (default: the panel's selected shot); `track` is an
optional string, `fov` by default, with `roll`, `focus_distance`, `aperture`, `position` and `rotation` also
accepted (a state call with `position` or `rotation` returns that track's keys and `range: null`). `op`
is a required string. `index` is a zero-based non-negative integer key index for `move`, `remove` and `ease`;
`time` and `value` are finite numbers required for `add` and `move`; `ease` is `Linear`, `Step` or
`Smooth` for `ease`. Time is limited to `0..=duration`; values are `fov` 1..=170 degrees, `roll`
−3600..=3600 degrees, `focus_distance` 0.001..=1,000,000 meters, or `aperture` 0.1..=128 f-stops.
An absent state `shot` or `track` keeps the current selection.

## Errors

| Code | When |
| --- | --- |
| `shot_panel_no_sequence` | no sequence is loaded (`render.sequence { action: "load" }` or `camera.set_path` first) |
| `shot_panel_bad_shot` | `shot` names no shot |
| `shot_panel_bad_track` | `track` is not `fov`, `roll`, `focus_distance`, `aperture`, `position` or `rotation` |
| `shot_panel_no_dof` | the shot has no depth of field and the track is `focus_distance` or `aperture` |
| `shot_panel_bad_key` | `index` is past the last key |
| `shot_panel_out_of_range` | `time` outside `0..=duration`, or `value` outside the track's `range`, or not finite |
| `shot_panel_key_time_taken` | another key already sits at that time |
| `shot_panel_bad_value` | a rotation value on the position track or a position value on the rotation track |
| `shot_panel_last_key` | removing the position path's only key |
| `shot_panel_invalid` | the edited sequence failed validation (also refuses a save) |
| `shot_panel_unsaved` | `editor.shot_panel_open` or `editor.shot_panel_adopt` while the loaded list has unsaved edits and `discard` is not true |
| `shot_panel_detached` | `editor.shot_panel_save` with no `path` after another list replaced the opened one (give a path) |
| `shot_panel_no_player` | `editor.shot_panel_adopt`: `player` is not a sequence player, or no player has a shot list |
| `shot_panel_bad_path` | the path does not end in `.shots.ron`, is not relative, leads out of the project, or a save has no path |
| `shot_panel_no_project` | no project is open |
| `shot_panel_io` | the file cannot be read (open) or its folder cannot be written (save) |
| `shot_panel_bad_file` | the file is not a valid `.shots.ron` (parse, version or validation error) |
| `F0002` | invalid or missing parameters, including a missing/unknown `op`, a missing field for the chosen op, or a wrong parameter type |

```json
{"method": "editor.shot_panel_state", "params": {"shot": "wide", "track": "fov"}}
{"method": "editor.shot_panel_edit", "params": {"op": "add", "time": 1.5, "value": 35}}
{"method": "editor.shot_panel_edit", "params": {"op": "ease", "index": 1, "ease": "Smooth"}}
{"method": "editor.shot_panel_edit", "params": {"track": "position", "op": "add", "time": 1.0, "value": [1.25, 1.75, 4.5]}}
{"method": "editor.shot_panel_edit", "params": {"track": "rotation", "op": "add", "time": 3.0, "value": [0.0, 0.3, 0.0, 0.954]}}
{"method": "editor.shot_panel_open", "params": {"path": "assets/cine/intro.shots.ron"}}
{"method": "editor.shot_panel_save", "params": {}}
{"method": "editor.shot_panel_adopt", "params": {"player": 4294967301}}
{"method": "editor.shot_panel_save", "params": {"path": "assets/cine/own.shots.ron"}}
```

## Workflow

1. Open a project `.shots.ron` with `editor.shot_panel_open` (or load one with
   `render.sequence {"action":"load", "ron":"..."}`).
2. Read `editor.shot_panel_state` for the selected shot, track, range and existing keys.
3. Call `editor.shot_panel_edit` with one add, move, remove or ease operation.
4. Read state again and capture the updated camera with `seeing.screenshot`.
5. `editor.shot_panel_save` writes it; `editor.shot_panel_state` then reports `dirty: false`.

## Failing call

With no sequence loaded, this captured call returns the handler error and changes nothing:

```text
$ regia agent call editor.shot_panel_edit '{"op":"add","time":0,"value":50}' --project samples/boot_probe
error: method call failed: no shot sequence is loaded (-32001): {"code":"shot_panel_no_sequence","hint":"render.sequence { action: \"load\", ... } or camera.set_path first"}
```
