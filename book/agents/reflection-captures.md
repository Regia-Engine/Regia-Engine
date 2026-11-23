# Reflection captures (agents)

Local reflection captures (ADR-2000, ADR-2860) show what stands around a point in metal and other glossy surfaces:
six HDR faces rendered from the capture's centre, GGX-filtered and read with box projection by the forward pass on
every tier that draws image light. Since ADR-2860 they are level content: a level saves its captures, names a
persisted bake (`.fcap`), and opens with their reflections installed and no capture face rendered.

## In a level

A capture is a `regia_render::LocalCapture` component on an entity; the capture point is the entity's world
position.

```ron
"regia_render::local_capture::LocalCapture": (id: 1, half_extent: (4.0, 2.5, 4.0), weight: 1.0, fade: 1.0),
```

- `id`: stable, shared with the agent methods and the bake file.
- `half_extent`: the parallax box in metres: the walls of the room the reflection stands in.
- `weight`: blend against the environment, 0 to 1.
- `fade`: how far outside the box the capture fades out, metres.

The level names its bake in `LevelLook::captures` (scene format v13):

```ron
look: (captures: Some((file: "maps/hall.fcap"))),
```

When the level opens, `regia_framework::local_capture_level::LocalCaptureLevelPlugin` (in `regia`'s default
plugins) reads the file and every capture baked within 1 mm of its position shows its map from the first frames,
with no face rendered. A capture moved since the bake, or one the file lacks, is baked by rendering as before. A
missing file is `captures_not_found`; a damaged one is refused by name (below) and the captures bake by rendering.

## Bake and save

1. Place captures (`LocalCapture` components, or `render.place_local_capture`).
2. Bake them: `render.bake_local_capture {}` queues every capture for the next frame; poll
   `render.list_local_captures` until no capture is `pending`.
3. `render.save_local_captures { "file": "maps/hall.fcap" }` writes every baked capture to `assets/maps/hall.fcap`
   and names it in the open level's `look.captures`.
4. Save the level (`level.save`).

`regia bake-captures --project DIR --file maps/hall.fcap` runs steps 2 and 3 against the running app;
`regia bake-captures --check assets/maps/hall.fcap` reads a file offline and prints its summary or its error code.

## Methods

### `render.save_local_captures { file, name_in_level? }`

Writes every baked capture (placement, coverage, filtered HDR chain) to `assets/<file>`; `file` must end in `.fcap`.
Unless `name_in_level` is `false` it sets the open level's `look.captures`. Returns
`{ file, captures, skipped_unbaked, bytes, named_in_level }`.

### `render.load_local_captures { file }`

Reads `assets/<file>` and installs its maps for every placed capture baked at its position (the same path the level
plugin takes). Returns `{ file, captures, ids }`.

### `render.get_capture_bake`

```json
{ "level": { "file": "maps/hall.fcap", "installed": true, "captures": 3, "error": null, "error_code": null },
  "installed_bakes": 3, "budget": 4, "layers": 4, "max": 32, "fade_frames": 8, "hysteresis_m": 0.75,
  "selected": 3,
  "captures": [ { "id": 1, "center": [0, 1.5, 0], "baked": true, "pending": false, "bakes": 1,
                  "layer": 0, "blend": 1.0, "uploads": 1 } ] }
```

`budget` is the number of captures a frame of the runtime's profile reads; `layer` is the array layer a selected
capture is read from (`null` while not selected); `blend` its weight in frames now (it ramps during a swap);
`uploads` how many times its map was written to a layer (a frame where nothing moved writes none).

## Many captures: the selection

A renderer holds up to 32 captures and keeps every bake on the CPU. A frame reads at most the tier's budget of them
(`regia_render::local_capture_budget`): 2 on Mobile Low and the lab and editor qualities, 3 on Mobile High, Desktop
Low and the Deck, 4 on Medium, Desktop High and Ultra. Each frame the runtime selects the boxes nearest to the
eye; a selected capture stays until another is 0.75 m nearer (`LOCAL_CAPTURE_HYSTERESIS_M`), so an eye standing on a
boundary does not swap them back and forth; a swap fades the leaving capture out and the entering one in over 8
frames (`LOCAL_CAPTURE_FADE_FRAMES`), so a slot change does not pop. Where boxes overlap, captures blend by weight.

## Screen reflections

SSR and stochastic SSR trace what is on screen. Where a ray misses inside a capture's box, the frame keeps the
capture's HDR reflection instead of blending the older coarse reflection probe there. Outside every capture, and
in a level with none, the screen reflections behave as before.

## In the editor: Create, the box helper and Tools ▸ Bake (ADR-7592)

**Create ▸ Reflection capture** (`editor.create_entity { kind: "reflection_capture" }`) adds an entity named
"Reflection capture" at the viewport's focus point with a `LocalCapture`: an id one past the largest capture id in the
level (ids start at 1), a 4 m box (`half_extent` 2), full weight and the default blend margin (a quarter of the box,
0.5 m). It is selected and is one undo step; it saves with the level like any component. Edit the box, weight and
margin in the inspector or with `editor.set_property { component: "LocalCapture", path: "half_extent", ... }`.

With **Show ▸ Bounds** on (`editor.set_viewport { bounds: true }`, the default), a selected capture draws its
influence box in amber (`capture_helper::CAPTURE_BOX`) and its blend margin, the box grown by `fade`, dashed in
orange (`capture_helper::CAPTURE_MARGIN`). Both boxes are axis aligned and unscaled, as the runtime places them: only
the entity's world position moves a capture. `editor.capture_viewport` shows the helper in captures.

A copied capture gets its own id (ADR-7663): Edit ▸ Duplicate (`editor.menu_invoke { path: "Edit/Duplicate" }`),
Copy then Paste (`Edit/Copy`, `Edit/Paste`), the gizmo's Alt-drag duplicate (`editor.gizmo_drag { duplicate: true }`)
and the Outliner's Duplicate (`editor.duplicate`, `editor.command { run: "outliner.duplicate" }`) give each copied
`LocalCapture` the next free id, one past the largest in the level, so the bake never refuses the copy with
`capture_duplicate_id`. A copy keeps its id only when no other capture uses it: a Cut then Paste moves a capture and
keeps its id (and the bake made for it, at the same place); pasting that clipboard again takes a new one. Undo
removes the copy; redo brings it back with the same new id, and the original keeps its own.
An instanced prefab's captures get their own id too (ADR-7716). A prefab whose template holds a `LocalCapture` starts
every instance with the template's id; spawning an instance (`editor.spawn_prefab`, or dragging the prefab from the Content
browser into the viewport) keeps the id when no other capture uses it and otherwise gives the
capture the next free id. The new id is stored as an `.id` override on the instance, so it saves with the level, a reload
gives the same id, and undo and redo bring the instance back with it. Duplicate, Copy/Paste and the Outliner's Duplicate
of an instance renumber the copy's captures the same way. A prefab that nests the same prefab twice has two captures with
one local id that one override cannot tell apart; both keep the shared id and the bake still names it with
`capture_duplicate_id`.

**Box and margin handles** (ADR-7663): with the viewport's handle tool on (Tools ▸ Collider handles (C), the
`collider.toggle_handles` command, or `editor.capture_handles { enabled: true }`), the primary selection's capture
shows grips instead of its transform gizmo, through the handle widget the collider grips use: an arrow on each face
of the influence box (`extent_x+` … `extent_z-`) sets `half_extent` on that axis about the center (at least 0.01 m),
and an arrow outside each face of the blend margin, pointing in (`margin_x+` … `margin_z-`), sets `fade` to how far
outside the box it is dragged (at least 0). Each drag is one undo step (`Reflection capture handle`). An entity with a
collider shows its collider grips instead. Agents drive the same drag:

```json
{ "method": "editor.capture_handles", "params": { "entity": 4294967301 } }
→ { "enabled": true, "entity": 4294967301, "shown": true, "center": [0, 1, 0],
    "capture": { "id": 1, "half_extent": [2, 2, 2], "fade": 0.5, "weight": 1 },
    "handles": [{ "name": "extent_x+", "position": [2, 1, 0] }, ...] }
{ "method": "editor.capture_handle_drag", "params": { "entity": 4294967301, "handle": "margin_x+", "to": [3.5, 1, 0] } }
→ { "entity": 4294967301, "changed": true, "capture": { "id": 1, "half_extent": [2, 2, 2], "fade": 1.5, "weight": 1 } }
```

`enabled` sets the viewport's handle tool, the same toggle as `editor.collider_handles { enabled }`: it is view state
(no undo step, the level is not marked changed), and like that method `editor.capture_handles` is declared read-only,
so a read-only agent may switch the tool (ADR-7699). Omit `enabled` to only read.

`steps` (default 4, at most 240) sets the drag's intermediate positions. Refusals (`data.reason`):
`capture_missing` (the entity has no `LocalCapture`), `capture_handle_unknown` (not one of the twelve names),
`capture_feed_owned` (the DCC feed owns the entity).

**Tools ▸ Bake reflection captures** (`editor.bake_captures { file? }`) bakes every capture of the level with the
editor's renderer, one capture per editor frame, and writes `assets/<file>`: by default the file the level already
names in `look.captures`, else `maps/captures.fcap`. The bytes are those `render.save_local_captures` writes after
`regia bake-captures` on the same level (same ids, boxes, order and maps). The file is written to a temporary name
and renamed over the old one, and it is named in the level's `look.captures`; save the level to keep that (a new
naming marks the level unsaved, so the close prompt and Save & Exit keep it). The centers are the captures' positions
when the bake starts, parent chain included, even for a move made earlier in the same agent batch. While it
runs a progress window shows the count with a Cancel button; an agent polls `editor.bake_captures_status` and cancels
with `editor.cancel_task { id: "bake_captures" }`. A cancel, or a capture that does not bake, writes nothing: the old
file stays byte for byte.

```json
{ "method": "editor.bake_captures", "params": { "file": "maps/hall.fcap" } }
→ { "task": "bake_captures", "file": "maps/hall.fcap", "ids": [1, 2], "captures": 2, "state": "running" }
{ "method": "editor.bake_captures_status" }
→ { "task": "bake_captures", "state": "done", "file": "maps/hall.fcap", "ids": [1, 2], "baked": 2, "total": 2,
    "reason": null, "message": "baked 2 reflection captures into maps/hall.fcap (save the level to keep it)",
    "bytes": 1234 }
```

(`bytes` is the size of the written file; the number above is a placeholder.) `state` is `idle` (no bake yet this session), `running`, `done`, `failed` or `cancelled`. `editor.bake_captures`
refuses with `data.reason`:

| Reason | Meaning |
|---|---|
| `bake_busy` | a bake is already running |
| `bake_no_project` | no project is open (the file goes under its `assets/`) |
| `bake_no_renderer` | the edit world has no `RuntimeRenderer` |
| `bad_file` | `file` is not a string or does not end in `.fcap` |
| `path_outside_project` | `file` leaves the project |
| `captures_none` | the level has no reflection capture |
| `capture_duplicate_id` | two captures share an id (each needs its own) |
| `capture_bad_value` | a capture's box, weight or margin is out of range |
| `captures_full` | more than 32 captures |
| `capture_missing` | `editor.capture_handles` / `editor.capture_handle_drag`: the entity has no `LocalCapture` |
| `capture_handle_unknown` | `editor.capture_handle_drag`: `handle` is not `extent_*` / `margin_*` with an axis and sign |
| `capture_feed_owned` | `editor.capture_handle_drag`: the DCC feed owns the entity |

A run that ends `failed` names `bake_failed` (a capture did not bake, for example no GPU device) or `fcap_io` (the
file could not be written) in `reason`; a cancelled one names `bake_cancelled`, `bake_capture_changed` (below), or `bake_level_changed` when the
open level or the project changed while it ran (File ▸ Open, New level, Revert, a project switch): the run stops
before the next capture, writes nothing and does not touch the new level's `look.captures`. However a run ends, its
captures are taken off the editor's renderer again, so the viewport never draws a stale reflection box; a game, and
`regia bake-captures`, load the saved file.

A capture moved, resized (box, margin or weight), deleted or added while a run bakes the same level ends the run
`cancelled` with `bake_capture_changed` at its next step (ADR-7663): `changed_capture` in the status names the
capture, and nothing is written, so a file never holds a map baked where a capture no longer is. A move of a parent
counts; a move of any other entity does not. Bake again once the edit is done.

**The viewport draws the baked captures** (ADR-7663), as a game that loads the level does: after a run ends `done`
its maps are published in the edit world's `LocalCaptureBakes` (the file the run wrote is not read back), and when a
level opens that names a bake in `look.captures` the editor reads `assets/<file>` (again whenever the file changes).
Every editor frame places each capture whose bake was made at its position (within 1 mm) on the viewport's renderer
with that map, and runs the capture selection from the editor camera. A capture moved since its bake, or one without a
bake, is not drawn until it is baked again (the editor renders no capture faces outside a run). `editor.bake_captures_status`
reports it in `viewport`:

```json
→ { ..., "changed_capture": null, "viewport": { "file": "maps/hall.fcap", "drawn": [1, 2], "error": null } }
```

`viewport.error` starts with `captures_not_found` when the level names a file that is not under `assets/`, or with
the reader's code (`fcap_crc`, ...) when it does not read; nothing is drawn then.

Tests: `cargo test -p regia_editor --lib shapes capture` (`reflection_capture_creates_a_unique_capture_in_one_undo_step`,
`reflection_capture_saves_the_bytes_set_property_writes`, `capture_bake_bytes_equal_the_cli_path`,
`capture_bake_cancel_mid_bake_leaves_the_old_file_untouched`, `capture_helper_overlay_follows_the_bounds_flag`,
`capture_bake_done_marks_a_saved_level_dirty_when_the_naming_changes`, `capture_bake_cancels_when_the_level_changes`,
`capture_bake_cancels_when_the_edit_session_restarts`, `capture_bake_cancels_when_the_project_changes`,
`capture_bake_centers_include_unpropagated_moves_and_parents`, `capture_bake_leaves_no_placement_on_the_renderer`,
`copied_reflection_captures_get_fresh_ids_on_every_copy_path`, `cut_then_paste_keeps_the_capture_id`,
`capture_bake_cancels_when_a_capture_changes_mid_run`, `capture_bake_publishes_the_baked_set_to_the_viewport`,
`viewport_draws_the_bake_the_level_names`, `capture_handle_drag_matches_the_viewport_drag_as_one_undo_step`,
`capture_handle_drag_survives_a_second_egui_pass`, `capture_handles_show_under_the_tool_and_refuse_with_reasons`) and,
on a Vulkan adapter (lavapipe), `cargo test -p regia_editor --test reflection_capture_helper`.

## Error reasons

Refusals are `invalid_params` (`F0002`) with `data.reason`:

| Reason | Meaning |
|---|---|
| `bad_file` | `file` missing, empty or not ending in `.fcap` |
| `captures_not_baked` | `render.save_local_captures` found no baked capture |
| `path_outside_project` | `file` leaves the project |
| `fcap_io` | the file could not be read or written |
| `fcap_bad_magic` | not a capture bake |
| `fcap_version` | a format version this build does not read |
| `fcap_truncated` | the file ends before its header or payload |
| `fcap_crc` | the payload's checksum does not match: the file is damaged |
| `fcap_layout` | sizes that do not fit together (width, mips, mip lengths, count) |
| `fcap_value` | a non-finite or out-of-range number, or a repeated id |

The level plugin reports the same codes, plus `captures_not_found`, in `render.get_capture_bake` `level.error_code`.
No renderer is `F0003`.

See also: [Local captures](local-capture.md) for placing and baking single captures, the format page
`docs/formats/fcap.md`, and ADR-2860.
