# Viewport tools: vertex snap, drop to floor, collider handles, World Settings, log links

ADR-2696. Code: `regia_editor::surface_tools`, `collider_handles`, `world_settings`, `log_links`. The user's view is
[Editor](../user-guide/editor.md). Every edit here goes through the undo stack, one step each (`editor.history` shows it).

## Keys

Each key is a palette command, so `editor.keymap_bind` moves it. A user shortcut replaces the built-in key.

| Command | Key | Does |
|---|---|---|
| `gizmo.vertex_snap` | V (held) | While translating, the pivot goes to the vertex under the pointer. Run from the palette, it stays on until run again |
| `selection.drop_to_floor` | End | Drops the selection |
| `placement.toggle_align_normal` | | Flips "align to surface normal" |
| `collider.toggle_handles` | C | Flips the collider tool |
| `window.world_settings` | | Opens the World Settings tab |
| `camera.fly_forward`, `_back`, `_left`, `_right`, `_up`, `_down` | W, S, A, D, E, Q (held, with the right mouse button) | Flies the camera |
| `outliner.delete`, `outliner.duplicate`, `outliner.group` | Delete, Ctrl+D, Ctrl+G (pointer over the outliner) | Acts on the selection |

Hold commands (`gizmo.vertex_snap` and the fly keys) are read while their key is down; a user chord only moves the key.

## `editor.vertex_snap`

Casts a ray through a pane pixel with the entity's own subtree left out, takes the vertex of the hit triangle nearest the
hit point and moves the entity's pivot onto it. Params: `x`, `y` (required, pixels of a `width`×`height` pane, default
640×480), `entity` (default the primary selection), `apply` (default true; false only reports), `view` (`perspective`,
`top`, `front`, `side`).

```json
{ "method": "editor.vertex_snap", "params": { "entity": 4294967297, "x": 320, "y": 240 } }
```

Returns `{ snapped, entity, position, vertex, triangle, target, target_owner, applied, translation }`; `snapped: false`
means the ray met no drawn mesh, or terrain hid it. Check it worked: the entity's world position equals `position`.

## `editor.drop_to_floor`

Moves each top-level entity straight down onto the first drawn mesh or terrain under its footprint, as one undo step. An
entity sunk into the floor comes back up. Listed entities are not surfaces for each other. A collider with no mesh is not
a surface. An entity with no mesh drops by its collider's outline, or else by its pivot. Params: `entities` (default the
selection), `align` (default the placement option): also turn the entity's up onto the normal of the surface under its
center, then slide straight down until its lowest point along the normal lies in that surface's plane.

```json
{ "method": "editor.drop_to_floor", "params": { "align": true } }
```

Returns `{ placed: [{ entity, from, to, surface, normal, translation, rotation }], missed, align, undoable }`. `missed`
lists what had no surface below it or cannot move. Check it worked: the lowest vertex is within 1 mm of the surface.

## `editor.placement_options`

Reads or sets `align_to_normal` and `vertex_snap`. Returns both. Read permission, like `editor.set_gizmo`.

## `editor.collider_handles`

Lists a collider's grips with their world positions, and reads or sets the viewport's collider tool. Params: `entity`
(omitted: only the tool state), `enabled` (sets the tool: view state, no undo step; the method is declared read-only,
so a read-only agent may switch the tool, ADR-7699). While the tool is on, the primary selection's collider shows its outline and
grips instead of the transform gizmo. They show where the gizmo would: not in game view, while the terrain tool sculpts, or in
a 2D mode pane. If the tool or the selection changes during a drag, what the drag did so far stays one undo step.

Returns `{ enabled, entity, collider: { shape, offset, half_extents, radius, half_height }, handles: [{ name, position }] }`.

| Shape | Handles |
|---|---|
| Box | `extent_x+`, `extent_x-`, `extent_y+`, `extent_y-`, `extent_z+`, `extent_z-`, `offset` |
| Sphere | `radius_x`, `radius_y`, `radius_z`, `offset` |
| Capsule | `half_height+`, `half_height-`, `radius_x`, `radius_z`, `offset` (axes of the capsule's frame, Y along its length) |
| Other | `offset` |

## `editor.collider_handle_drag`

Drags a grip to a world point through the viewport's drag code. Params: `entity`, `handle`, `to` (`[x, y, z]`), `steps`
(default 4). A size is measured from the shape's center along the grip's axis, so the opposite side moves with it, and stays
at least 1 mm. The whole drag is one undo step; a drag that ends where it began is none. The body pose ignores scale, as the
physics step does.

```json
{ "method": "editor.collider_handle_drag", "params": { "entity": 4294967297, "handle": "extent_x+", "to": [3, 0, 0] } }
```

Returns `{ entity, changed, collider }`.

## `editor.world_settings_get`

The current level's `LevelSettings`, as `{ settings, level, dirty }`. `level` is the saved path or null.

## `editor.world_settings_set`

Changes them as one undo step. `settings` merges field by field over the current values, and `null` clears an optional
field; or `reset: true` returns all to the defaults. The next level save writes them. What reads the settings live
changes at once; what a level load applies (game rules, baked GI, lightmap) takes effect when the level opens again. The
viewport's `editor` quality preset keeps its own sky and exposure.

```json
{ "method": "editor.world_settings_set", "params": { "settings": { "gravity": [0, -1.62, 0], "atmosphere": { "preset": "Mars" } } } }
```

Returns the get result plus `changed`.

## `editor.world_settings_panel`

Brings the World Settings tab forward on the next frame. It is not in the built-in layout.

## `editor.log_links`

The links the Output Log finds in `text`, or without it in the recent records (`limit`, default 200). Returns
`{ links: [{ kind, start, end, path, readings, line, col, seq }] }`; `start` and `end` are byte offsets, `kind` is `file` or
`asset`.

- A **file link** is a path followed by `:line` or `:line:col` whose last segment has an extension. With no directory the
  extension must be a source one (`rs`, `wgsl`, `toml`, `ron`, ...), so `example.com:443` is not a link.
- An **asset link** is a path with an asset suffix (`.material.ron`, `.prefab.ron`, `.level.ron`, `.glb`, `.png`, ...) and no
  line number.
- A path with spaces is found when quoted, when it starts from a root (`/`, `./`, `../`, `~/`, `C:\`, `C:/`, `\\`), or when
  each word before the last names a directory (`assets/my models/a b.glb`). Otherwise an unquoted relative path is its last
  word.
- Where the text is ambiguous (`copied /tmp/a to src/b.rs:3`), `readings` lists each, longest first. Opening takes the first
  that exists under the project, its `game/` crate or its `assets/` directory.

## `editor.log_open_link`

Opens a link as a click does. Params: `text` (the log line, or just `path:line`), `index` (default 0), `dry_run`. A request
stays inside the project; a click in the panel may open any existing file.

A file opens in the code editor, an asset the way the Content Browser opens it (`action`: `editor` for a material or anim
graph, `viewer` for a mesh, texture or sound, `prefab`, `level`; other text assets open in the code editor). A level waits
while the current one has unsaved edits, a prefab is being edited or play-in-editor runs; the Output Log says so.

The command is the preference `code.editor_command`: `code`, `zed`, `$EDITOR`, or a template whose `{file}`, `{line}` and
`{col}` substitute per argument. With the preference empty, `[editor] code_editor` of `.regia/agent.toml`, then for this
method `$VISUAL` and `$EDITOR`. It never goes through a shell, and a program with no `{file}` gets `+line file` (so a terminal
editor needs a terminal: `kitty -e nvim +{line} {file}`). On Windows name the script: `code.cmd`.

`editor.prefs_set` refuses `code.editor_command`, so a request can never pick the program.

Returns `{ opened: "file", path, line, col, command }` or `{ opened: "asset", action, path }`.

## View presets, eased moves, pivot, snap, duplicate, menus (ADR-7204)

Code: `regia_editor::viewport_ops` (agent surface `viewport_ops_agent`). Keys in the active viewport pane:

| Key | Does |
|---|---|
| 1, 2, 3, 4 | Perspective, top, front, side view preset of the pane's camera (target and distance kept), eased over 0.25 s |
| F | Frames the selection, eased over 0.25 s |
| Ctrl+0…9 / Shift+0…9 | Stores / recalls (eased) a camera bookmark (bare digits were the recall before ADR-7204) |
| Ctrl+D | Duplicates the selection one metre along +X, one undo step |
| Alt+drag on the gizmo | Duplicates the selection in place, then the copies follow the drag; duplicate and move are one undo step |
| Ctrl (held) | Inverts the Snap toggle while dragging |

The viewport toolbar has the **Pivot** choice (median point, individual origins, active element) and the **Snap** toggle.
A gizmo drag on several selected entities moves the top-level ones: rotation and scale act about the pivot (the mean of
their translations, each one's own origin, or the active entity's origin). Without a GPU the click pick casts a ray against
each drawn mesh (its cached BVH when it has 64 or more triangles) and the terrain, not spheres around entity origins.

The Outliner row has an eye (show or hide in the viewport, one undo step), a lock (a locked entity, or a part of one, is
not picked or box-selected; not saved with the level), a link icon on prefab instance roots and a dot when the instance has
overrides; its context menu has Focus (eased). File has Save all, Recent and Exit; Edit has Cut, Copy, Paste, Duplicate,
Delete and Select all. Every item is also `editor.menu_invoke`.

## `editor.view_preset`

Turns the perspective camera (the world's `EditorCamera`) to a preset with the panel's eased move. Params: `preset`
(required: `perspective`, `top`, `front`, `side`, `bottom`, `back`, `opposite_side`; the last three since ADR-7755), `instant` (default false: jump). Advance the move with
`editor.camera_ease` (a headless editor has no frame clock for it).

```json
{ "method": "editor.view_preset", "params": { "preset": "top" } }
```

Returns `{ preset, from, to, duration }` (cameras as `{ target, distance, yaw, pitch, eye }`). Check it worked: after
`editor.camera_ease { advance: duration }` the camera equals `to` exactly.

## `editor.camera_ease`

Advances the perspective camera's running eased move (a view preset, Focus from the Outliner) by `advance` seconds
(default 0: read only; must be >= 0) and returns `{ camera, active, elapsed, duration }`. The move is a smoothstep in time;
yaw takes the shorter way round; once `elapsed` reaches `duration` the camera is the end pose and `active` is false.

## `editor.set_pivot_mode`

Reads or sets the gizmo pivot: `mode` = `median`, `individual` or `active`. Returns `{ mode }`.

## `editor.set_snap`

Reads or sets the Snap toggle and its steps: `enabled`, `distance` (metres), `angle_deg`, `scale` (each > 0). Returns
`{ enabled, distance, angle_deg, scale }`. `editor.set_gizmo`'s `snap` is the same toggle.

## `editor.gizmo_drag`

One gizmo drag, as one undo step, over `entities` (default the selection; only the top-level ones move) about the pivot
mode's point. Params: `translate` `[x, y, z]`, `rotate_deg` `[x, y, z]` (applied yaw Y, then X, then Z), `scale`
`[x, y, z]`, `active` (the active entity for pivot `active`; default the selection's primary), `duplicate` (the Alt+drag:
duplicate in place, move the copies, leave the originals).

```json
{ "method": "editor.gizmo_drag", "params": { "entities": [4294967297, 4294967298], "rotate_deg": [0, 180, 0] } }
```

Returns `{ moved: [{ entity, translation, rotation, scale }], copies, pivot, pivot_mode }`. Check it worked: one
`editor.undo` restores every transform (and removes the copies).

## `editor.menu_invoke`

Runs a File or Edit menu item by path. Params: `path` (required).

| Path | Does | Returns |
|---|---|---|
| `File/Save all` | Saves the open level when it has a path, the 2D mode's dirty tile map, and every other document the Save all dialog can save (prefab in edit mode, material graphs, UI document, shot list). The File ▸ Save all menu item itself opens the dialog: `editor.save_all { dialog: true }` (`book/agents/content-browser.md`) | `{ level, saved, failed: [{ id, error }], skipped }` (`level`: the `editor.save_level` result or null) |
| `File/Recent` | Lists recently opened or saved levels, newest first (kept per project in `.regia/editor_view.ron` since ADR-7275, so the last session's levels are listed) | `{ recent }` |
| `File/Recent/<n>` | Opens entry `n` | `{ opened }` |
| `File/Exit` | The window's close request | `{ exiting }` |
| `Edit/Undo`, `Edit/Redo` | As `editor.undo` / `editor.redo` | `{ undone }` / `{ redone }` |
| `Edit/Copy` | Copies the selected subtrees to the clipboard | `{ copied }` |
| `Edit/Cut` | Copies, then deletes them (one undo step) | `{ copied, deleted }` |
| `Edit/Paste` | Spawns the clipboard with fresh guids at the level root, children under their pasted parents (one undo step); the pasted roots become the selection | `{ pasted }` |
| `Edit/Duplicate` | Ctrl+D | `{ copies }` |
| `Edit/Delete` | Deletes the selection | `{ deleted }` |
| `Edit/Select all` | Selects every unlocked entity the Outliner lists | `{ selected }` |

Every result also echoes `path`. Refusals carry `data.menu_error`.

## `editor.outliner_set`

The Outliner toggles of one entity. Params: `entity` (required), `visible` (an undo step), `locked`. Returns
`{ entity, visible, locked, prefab, prefab_overrides }`. `editor.pick` never returns a locked entity, and a hidden one
is not drawn, so it is not picked either.

It also takes `color` (a row color name; `""` or `null` clears it) and `unique` (the saved unique-name marker) and
reports `color`, `unique`, `badges` and `warnings`: see [Outliner rows](outliner.md) (ADR-7772).

## View state per project (ADR-7275)

The editor keeps, per project in `.regia/editor_view.ron` (editor-only; the level file does not change):

- File ▸ Recent (project-relative paths, newest first, ten at most);
- per level: the perspective camera, the camera bookmarks (Ctrl+digit) and the Outliner locks (by `EntityGuid`;
  an entity without one is not saved).

When a level opens, its saved camera, bookmarks and locks come back; a level with no saved state keeps the session's
camera and bookmarks. The file is written when the state changed and 60 frames passed since the last write, when
another level opens, and when the editor exits (`agent.shutdown`, a signal, closing the window). A file written by a
newer editor, or one that does not parse, is read as empty and never overwritten.

## `editor.view_state`

Reads what the editor keeps (read only, no params). Returns
`{ file, exists, level, recent, levels: { <path>: { camera, bookmarks: [{ slot, camera }], locked: [guid], hidden:
[guid], isolated: [guid] } }, dirty, read_only, writes, error }` (`hidden` and `isolated`: the editor-only hide,
ADR-7770). `level` is the open level's project-relative path once its state was restored; `dirty`
means the editor holds changes the file does not have yet; `error` is `{ code, message }` of the last read or write
failure (`view_state_parse`, `view_state_newer`, `view_state_io`).

```
editor.view_state {}
→ { "file": ".regia/editor_view.ron", "exists": true, "level": "assets/maps/room.level.ron",
    "recent": ["assets/maps/room.level.ron"],
    "levels": { "assets/maps/room.level.ron": { "camera": { "target": [0, 1, 0], "distance": 8, "yaw": 0,
      "pitch": 1.57, "fov_deg": 60 }, "bookmarks": [{ "slot": 3, "camera": { … } }], "locked": ["67e55044-…"] } },
    "dirty": false, "read_only": false, "writes": 2, "error": null }
```

Check it worked: restart the editor on the project, `editor.open_level` the level, then `editor.camera_ease {}` and
`editor.bookmark { "action": "list" }` show the saved poses and `editor.outliner_set { entity }` reports `locked: true`.

## Camera navigation (ADR-7755)

The viewport's pointer chords come from the navigation preset's chord table, then the invert options, then the camera
move, in one place (`camera_nav::apply_drag`). The toolbar's right end has the fly speed slider (steps 1-8: 0.75, 1.5,
3, 6, 12, 24, 48 and 96 m/s; the value is shown beside it), the **Orbit selection** toggle and the **Camera** menu
(field of view, near and far for the active perspective pane; zoom toward cursor; speed scales with distance; Invert;
Navigation preset; Move camera to selection; Move selection to camera). The `camera.*` preferences set the session
defaults (`camera.invert_orbit_x`, `camera.invert_orbit_y`, `camera.invert_pan_x`, `camera.invert_pan_y`,
`camera.invert_fly_y`, `camera.invert_wheel`, `camera.speed_with_distance`, `camera.orbit_selection`,
`camera.zoom_to_cursor`, `camera.navigation_preset`, `camera.near_clip`, `camera.far_clip`). None of it is a document
edit except Move selection to camera, which is one undo step.
The Camera menu's Move camera to selection and Move selection to camera act on the perspective camera only: with an
orthographic pane active they are disabled (that pane's stored eye pose is never shown; ADR-7797). They run the same
function as the palette commands and the agent methods, which also stops the camera's running eased move.

| Preset | Chords | View keys |
|---|---|---|
| `regia_default` | Alt+left orbit, middle pan, Alt+right dolly, right look and fly | 1 perspective, 2 top, 3 front, 4 side; Alt+2 bottom, Alt+3 back, Alt+4 opposite side |
| `three_button` | as default, plus Alt+Shift+left pans (the middle button's pan) | as default |
| `numpad` | as default | the number row by keypad position: 5 front, 8 top, 2 bottom, 6 side, 4 opposite side, 0 perspective; Alt turns each to the opposite view |

Ctrl+digit stores and Shift+digit recalls a bookmark in every preset. In an orthographic pane a view key turns the pane to
that view (it keeps its own camera). The wheel zooms toward the point under the pointer (`zoom_to_cursor`, default on)
in perspective and orthographic panes; while flying it changes the speed, and the slider shows the new step.

Keyboard: the palette commands `view.camera_to_selection`, `view.selection_to_camera`, `view.toggle_orbit_selection`,
`view.toggle_zoom_to_cursor`, `camera.speed_up`, `camera.speed_down` and `view.cycle_layout` (all rebindable; the first two
are also in the View menu).

`editor.set_viewport` takes (all optional, applied only when every one is valid) `invert` (an object of booleans:
`orbit_x`, `orbit_y`, `pan_x`, `pan_y`, `fly_y`, `wheel`), `speed` (m/s, 0.5 to 500), `speed_step` (1 to 8),
`speed_with_distance`, `orbit_selection`, `zoom_to_cursor`, `navigation_preset`, `fov` (degrees, 20 to 120), `near`
(m, 0.001 to 100) and `far` (m, 1 to 100000, at least ten times near), and returns them with `speed_step` beside the
display fields. `layout` takes `single`, `quad`, `side_by_side`, `stacked` or `one_large_two`.

```json
{ "method": "editor.set_viewport", "params": { "invert": { "orbit_y": true }, "orbit_selection": true, "speed_step": 6 } }
```

Check it worked: the result has `invert.orbit_y: true`, `orbit_selection: true`, `speed: 24.0`, `speed_step: 6`.

## `editor.camera_to_selection`

Move camera to selection: the perspective camera's eye goes to the primary selected entity's world position, looking
along its forward (-Z), so a selected camera or light is looked through. Distance, speed, field of view and clip planes
stay; any eased move stops. Returns `{ from, camera }` (cameras with `speed`, `fov`, `near`, `far`). Check it worked:
`camera.eye` equals the entity's world translation.

## `editor.selection_to_camera`

Move selection to camera: the primary selected entity goes to the perspective camera's eye, facing where it looks (-Z
forward), keeping its scale; under a parent the local transform is solved so the world pose lands there. One undo step.
Returns `{ entity, translation, rotation, scale, undoable }` (the local transform). Check it worked: `editor.undo` puts it
back.

## `editor.viewport_layout`

The main viewport's panes: `preset` (`single`, `side_by_side`, `stacked`, `one_large_two`, `quad`) and `kinds` (per pane
index, `null` keeps one: pane 0 is `perspective`, panes 1-3 one of `top`, `front`, `side`, `bottom`, `back`,
`opposite_side`). Each pane keeps its own camera. Without params it reads. Returns `{ layout, active, panes: [{ index,
kind }] }`.

```json
{ "method": "editor.viewport_layout", "params": { "preset": "one_large_two", "kinds": [null, "top", "back"] } }
```

Check it worked: `panes` lists three panes with those kinds.

## Pane display: view modes, icons, exposure, screen percentage, badges (ADR-7770)

Each viewport pane has its own display settings, set from the toolbar above the panes (they apply to the
active pane) and through `editor.set_viewport` with `pane` (0-3, default the active pane):

- **View mode** (`view_mode`; keys Alt+1 to Alt+8 on the focused or hovered pane, except the digits the navigation
  preset turns views with: Alt+2, Alt+3 and Alt+4 under `regia_default`; ADR-8039): `lit`, `unlit`,
  `wireframe_only` (edges over flat dark surfaces, hidden edges stay hidden), `lit_wireframe` (edges over the
  lit picture; the renderer's `wireframe`), `lighting_only` (the light on a plain mid-gray surface),
  `light_complexity`, `overdraw`, and the buffer views under View ▸ Buffer views (`base_color` on Alt+8,
  `normals`, `roughness`, `metallic`, `ambient_occlusion`, `shadow_cascades`, `lod_color`, `motion_vectors`,
  `mip_level`, `material_id`, `uv_checker`, `texel_density`, `lightmap_density`, `lightmap_only`,
  `lightmap_invalid`, `image_light`, `image_light_occlusion`, `cloud_shadow_map`). The pane draws through the
  same pane request as `editor.capture_viewport { debug_view }`: a capture with `debug_view` draws what a
  pane in that mode shows, and a capture without it draws the pane's own mode.
- **Show ▸ Icons by kind** (`icon_filters: { lights, cameras, audio, decals, player_starts, match_rules,
  colliders, captures, volumes, nav }`; `match_rules` is the team-tinted spawn points and objective
  gizmos of [Match rules in the editor](editor-match-rules.md)), **icon size** (`icon_size`, 50-200 %) and **opacity** (`icon_opacity`,
  10-100 %). A kind that is off hides its icons and its always-on helpers (light radii, audio ranges,
  collider and trigger outlines, the navigation mesh, post volumes, capture boxes).
- **Exposure** (`exposure`: `auto`, `fixed` with `exposure_ev` -4 to 20, `level_band`): Auto is the editor's
  deterministic studio EV (14.5) at editor quality and the level band's centre at the other presets; Fixed
  pins the EV you set; Level band pins the band's centre at every preset. `exposure_ev` alone selects fixed.
- **Scale** (`screen_percentage`, 25-200 %): the pane renders at that share of its pixels and the
  renderer's upscaler brings the picture to the pane size. The stats overlay and `editor.capture_viewport`'s
  `info.render_size` show the internal size.

`editor.set_viewport` returns `pane`, `view_mode`, `icon_filters`, `icon_size`, `icon_opacity`, `exposure`,
`exposure_ev` (null unless fixed) and `screen_percentage` with the earlier fields. `editor.capture_viewport`'s
`info` also carries `view_mode`, `render_size`, `screen_percentage` and `exposure_ev` (the EV the frame is
pinned to, null when its own auto exposure runs).

```
editor.set_viewport { "pane": 0, "view_mode": "lighting_only", "exposure_ev": 11, "screen_percentage": 50 }
→ { …, "pane": 0, "view_mode": "lighting_only", "exposure": "fixed", "exposure_ev": 11.0, "screen_percentage": 50 }
```

### Badges

A pane shows a **Realtime off** badge while the viewport renders only on change (click it to render every
frame again), and a warning badge (`⚠ N changed`) when any of its settings differs from the default: view mode,
quality, exposure, screen percentage, show flags, icons. The badge's menu resets one setting or all of them.
Quality and show flags belong to the whole viewport, so their reset applies to every pane.

## `editor.viewport_badges`

Params: `pane` (default the active pane), `reset` (`all` or a setting name), `restore_realtime` (bool).
Returns `{ pane, realtime_off, changed: [{ setting, label }] }` after the resets.

```
editor.viewport_badges { "reset": "view_mode" }
→ { "pane": 0, "realtime_off": false, "changed": [{ "setting": "screen_percentage", "label": "Screen percentage" }] }
```

Check it worked: the same call with no params lists what is still changed; `editor.set_viewport {}` shows the
reset values.

## Hide in the editor and isolate (ADR-7770)

H hides the selection in the editor only, Alt+H shows everything hidden that way, Shift+H isolates the selection
(hides every other entity that is not under or above a selected one) and pressing it again shows them. The
editor-only hide is per user: it is kept in `.regia/editor_view.ron` per level (`hidden`, and the isolate's
share in `isolated`), it never changes the level file, never adds an undo step and never marks the level
unsaved. Hidden entities (and their children) do not draw and cannot be picked or box-selected. The Outliner eye
stays the saved visibility; an entity hidden in the editor only shows a dimmed eye. The keys are the commands
`view.hide_selection`, `view.unhide_all` and `view.isolate`, so a user shortcut can replace them.

## `editor.hide`

Params: `entities` (entity ids, default the selection), `hidden` (default true), `editor_only` (default true;
false sets the saved visibility instead, the Outliner eye, one undo step each), `all` (with `hidden: false`:
show every editor-only hidden entity and end an isolate). Returns `{ changed, editor_hidden: [entity],
isolating }`.

```
editor.hide { "entities": [4294967301] }
→ { "changed": 1, "editor_hidden": [4294967301], "isolating": false }
```

## `editor.isolate`

Params: `on` (true starts, false ends; absent toggles). Returns `{ changed, isolating, hidden }` (`hidden`:
how many entities the isolate hid).

Check it worked: `editor.pick` at the hidden entity's pixel returns another entity or none, `editor.view_state`
lists its guid under `hidden`, and `editor.history` shows no new entry.

## Error codes

| Code | Method | Meaning |
|---|---|---|
| `vertex_snap_no_entity` | `editor.vertex_snap` | No `entity` and no selection |
| `vertex_snap_no_transform` | `editor.vertex_snap` | The entity has no `Transform` |
| `vertex_snap_bad_parent` | `editor.vertex_snap` | The entity's parent has a zero scale |
| `drop_no_selection` | `editor.drop_to_floor` | No `entities` and no selection |
| `drop_no_surface` | `editor.drop_to_floor` | Nothing below any of them, or none can move |
| `collider_missing` | `editor.collider_handles`, `editor.collider_handle_drag` | The entity has no `Collider` |
| `collider_handle_unknown` | `editor.collider_handle_drag` | This shape has no such handle |
| `collider_feed_owned` | `editor.collider_handle_drag` | The DCC feed owns the entity |
| `world_settings_no_level` | `editor.world_settings_set` | No level is open |
| `world_settings_bad_value` | `editor.world_settings_set` | A value does not read as its type or is out of range |
| `world_settings_unknown_field` | `editor.world_settings_set` | Not a field of `LevelSettings`, or a number its type cannot hold |
| `log_link_missing` | `editor.log_open_link` | The text has no link at `index` |
| `log_link_outside_project` | `editor.log_open_link` | The path leaves the project |
| `open_in_editor_no_code_editor` | `editor.log_open_link` | No code editor is configured |
| `pref_owner_only` | `editor.prefs_set` | The preference names a program the editor runs |
| `menu_unknown_path` | `editor.menu_invoke` | No such menu item (`F0002`) |
| `menu_nothing_selected` | `editor.menu_invoke` | The item needs a selection (`F0002`) |
| `menu_clipboard_empty` | `editor.menu_invoke` | Paste with nothing copied (`F0003`) |
| `menu_no_recent` | `editor.menu_invoke` | `File/Recent/<n>` names no entry (`F0003`) |
| `menu_failed` | `editor.menu_invoke` | The item ran and failed (`F0001`) |
| `view_state_no_project` | `editor.view_state` | The editor has no project |
| `view_state_parse` | `editor.view_state` (`error.code`) | `.regia/editor_view.ron` does not parse; it is not overwritten |
| `view_state_newer` | `editor.view_state` (`error.code`) | The file is from a newer editor; it is not overwritten |
| `view_state_io` | `editor.view_state` (`error.code`) | The file could not be read or written |

## Reference space, select tool, pivots, snap steps, surface snap (ADR-7756)

Code: `regia_editor::viewport` (the gizmo block and toolbar), `viewport_ops` (spaces, pivots, steps, the fixed-step rotate)
and `surface_tools` (surface snap). Keys in the active viewport pane, each a palette command (`editor.keymap_bind` moves
it):

| Command | Key | Does |
|---|---|---|
| `gizmo.select` | Q | The select tool: no gizmo; a click picks and a drag box-selects, so a drag on the selection never moves it |
| `gizmo.translate`, `gizmo.rotate`, `gizmo.scale` | W, E, R | The gizmo tools, unchanged |
| `gizmo.toggle_space` | Ctrl+Backtick | The next reference space: world, local, parent, view |
| `gizmo.pivot_offset` | P (held) | While the rotate or scale gizmo shows, it becomes a handle on the pivot: a drag moves the pivot (no edit, no undo step). The offset belongs to the selection and lapses when it changes. Run from the palette, it clears the offset |
| `gizmo.rotate_step`, `gizmo.rotate_step_back` | U, Shift+U | Turns the selection by the angle step about the world up axis through the pivot, one undo step a press |
| `gizmo.toggle_snap` | | Flips the Snap toggle |
| `placement.toggle_surface_snap`, `placement.toggle_surface_align` | | Flip surface snap and its normal alignment |

The toolbar has **Space** (World, Local, Parent, View), **Pivot** (now with Bounds center), the **Snap** toggle and the
**Grid**, **Angle** and **Scale** step menus beside it: presets (grid 0.01 to 100 m in powers of two and tens; angle 5, 10,
15, 30, 45, 90 degrees; scale 0.01 to 1) and a typed custom value. The steps are saved to the project's
`.regia/viewport_snap.ron` (per-user view state, not a content format) and read back when the project opens again.
**Tools** has *Surface snap while dragging*, *Align to surface while snapping* and the offset along the normal.

The spaces: **World** is the world axes; **Local** the active entity's own axes; **Parent** its parent's axes (the world's
for a root); **View** the camera's right, up and back. A scale always runs along the entity's own axes in Parent and View.
The gizmo is drawn at the selection's world pivot, so a child of a moved parent shows it where the child is.

**Bounds center** turns and scales about the center of the world box around the selection's meshes (the box of their
origins when none draws a mesh). **Surface snap**: while translating, the pivot goes to the mesh or terrain under the pointer
(the moving entities left out), lifted by the offset along the surface normal; with align on, the active entity's up turns
onto the normal by the shortest arc. A drag is one undo step. Vertex snap (V) wins while it finds a vertex.

The rotate gizmo's outermost ring turns about the camera's view direction.

### `editor.set_gizmo`

Also takes `space` (`world`, `local`, `parent`, `view`) and `mode` `select`; returns `{ mode, snap, space }`.

### `editor.set_tool`

Reads or sets the viewport tool: `tool` = `select`, `translate`, `rotate` or `scale`. Returns `{ tool, gizmo }` (`gizmo`
false for `select`).

```json
{ "method": "editor.set_tool", "params": { "tool": "select" } }
```

### `editor.set_pivot_mode` (ADR-7756 additions)

`mode` also takes `bounds`. `offset` `[x, y, z]` sets the current selection's temporary pivot offset (world meters; null
or `[0, 0, 0]` clears it). Returns `{ mode, offset }` (the offset that applies to the current selection).

### `editor.set_snap` (ADR-7756 additions)

The same call the toolbar's step menus make. A step change is saved to `.regia/viewport_snap.ron` of the open project;
the result gains `saved` (that path, or null with no project open).

### `editor.gizmo_drag` (ADR-7756 additions)

`space` gives the axes of `translate` and `rotate_deg`: `world`, `local` (the active entity's), `parent` (the default,
the behavior before the parameter) or `view` (the perspective camera's). `scale` stays along each entity's own axes.

```json
{ "method": "editor.gizmo_drag", "params": { "translate": [2, 0, 0], "space": "local" } }
```

Check it worked: an entity turned 90 degrees about Z moves 2 m along world +Y.

### `editor.rotate_step`

Turns the selection by the snap angle step about the world up axis through the pivot, one undo step. `back: true` turns
the other way. Returns `{ moved: [{ entity, translation, rotation }], degrees, pivot }`.

### `editor.placement_options` (ADR-7756 additions)

`surface_snap`, `surface_align` (booleans) and `surface_offset` (meters along the normal); all three are in the result.

### Error codes (ADR-7756)

| Code | Method | Meaning |
|---|---|---|
| `gizmo_space_unknown` | `editor.set_gizmo`, `editor.gizmo_drag` | `space` is not world, local, parent or view |
| `tool_unknown` | `editor.set_tool` | `tool` is not select, translate, rotate or scale |
| `pivot_offset_no_selection` | `editor.set_pivot_mode` | An `offset` with nothing selected |
| `snap_step_invalid` | (the toolbar's log) | A step that is not a number above zero |
| `snap_steps_io` | `editor.set_snap` | `.regia/viewport_snap.ron` could not be written (the steps are set anyway) |
| `nothing_selected` | `editor.camera_to_selection`, `editor.selection_to_camera` | Nothing is selected (ADR-7755) |
| `no_transform` | `editor.camera_to_selection`, `editor.selection_to_camera` | The primary selection has no `Transform` |
| `singular_parent` | `editor.selection_to_camera` | The entity's parent has a zero scale axis |
| `bad_clip` | `editor.set_viewport`, `editor.viewport_instance_set` | `near`/`far` out of range, or far under ten times near; nothing changes |
| `bad_speed` | `editor.set_viewport` | `speed` outside 0.5..500 or `speed_step` outside 1..8 |
| `bad_fov` | `editor.set_viewport` | `fov` outside 20..120 degrees |
| `bad_preset` | `editor.set_viewport` | Unknown `navigation_preset` |
| `bad_layout` | `editor.viewport_layout` | Unknown `preset` |
| `bad_view` | `editor.viewport_layout` | Unknown view name, or pane 0 not perspective / panes 1-3 not orthographic |

## Pilot, look through, camera preview, screenshots and guides

ADR-7768 (spec addendum § 3.6-3.8). Code: `regia_editor::viewport_pilot`, `camera_preview`, `screenshot`, and
`overlays::ViewportGuides`. The main viewport only; view state (never saved, never on the undo stack) except a pilot's
moves, which are one undo step per session.

**Pilot.** With an entity selected (a camera, a light or anything with a `Transform`), **Pilot selection**
(Ctrl+Shift+L, the palette command `view.pilot`, the toolbar's **Camera** menu) locks the editor camera to it: the
camera jumps to the entity's position and facing (its −Z), and from then on flying, panning, dollying and the SpaceMouse
move the entity by exactly the camera's motion; the look drags turn it in place. The pane shows "Piloting <name>" with
an **Eject** button (also Ctrl+Shift+L again, `view.eject`). Eject leaves the camera where it is. For a scene camera the
view uses its vertical field of view ("exact framing"). Ctrl+Shift+P stays the palette's key, so pilot uses
Ctrl+Shift+L; `editor.keymap_bind { id: "view.pilot" }` moves it.

**Camera menu.** The viewport toolbar's **Camera: <name>** menu lists every scene camera with a search field. Each row
looks through that camera (the editor camera takes its pose and field of view every frame and nothing is edited) or
pilots it. **Editor camera** returns to the pose saved when looking through began. The palette's
`view.look_through_next` and `view.editor_camera` walk the same list from the keyboard.

**Camera preview.** While the primary selection is a scene camera, the perspective pane shows an inset of its view
(game look, its pose and field of view) in the bottom-right corner. Sizes: off, small, medium (default), large
(the preference `viewport.camera_preview`). It is
one more small render through its own renderer, at most once per frame, and none while no camera is selected
(`requests` counts them).

**Screenshot.** File ▸ Screenshot… (`file.screenshot`) opens a dialog: a multiplier of the perspective pane's pixel
size (1x to 8x) or an explicit size (16 to 16384 a side), editor helpers on or off, a file name. The PNG goes to the
project's `Saved/Screenshots` folder, written atomically, and a toast offers **Open folder**. The window's close
button (or Esc) and the toast's **Dismiss** are `editor.screenshot { close: true }` (alone): it closes the window and
the toast without taking a shot and answers `{ closed }` (false when neither showed). One render covers any
size within the device's texture limit (16384 on lavapipe and the desktop cards); a larger size answers
`screenshot_needs_tiles` (tiled capture is deferred, ADR-7768). `editor.capture_viewport` takes up to 16384 a side
under the same rule (it was clamped to 2048).
File > Screenshot currently renders **Lit**, regardless of the pane's diagnostic view mode.
It is synchronous; the off-axis still-capture renderer primitive (ADR-8200) is not yet wired
to this dialog or `editor.screenshot`. Background capture, progress and cancellation remain
pending a snapshot contract for project-installed renderer hooks.

**Guides.** The Camera menu's **Rule of thirds**, **Center cross**, **Safe areas** (action 93 %, title 90 %) and a
**Letterbox** aspect (2.39:1, 1.85:1, 16:9, 4:3, 1:1, 9:16) draw over the perspective pane; the thirds, cross and safe
boxes sit inside the letterboxed picture.

| Method | Params | Result |
|---|---|---|
| `editor.pilot` | `entity` (id, or null to eject), `fly` `{ forward, right, up, seconds, steps }` | `{ mode, entity, name, eye, fov_deg }` |
| `editor.cameras` | `query` | `{ cameras: [{ entity, name, fov_deg, viewing }], session }` |
| `editor.look_through` | `entity` (camera id, or null for the editor camera) | `{ mode, entity, name, eye, fov_deg }` |
| `editor.camera_preview` | `size` (`off`, `small`, `medium`, `large`) | `{ size, camera, requests }` |
| `editor.screenshot` | `multiplier`, or `width` + `height` (or `size: [w, h]`), `pane`, `helpers`, `name`; or `close: true` alone | `{ path, width, height, tiles, helpers }`; `{ closed }` with `close` |
| `editor.set_viewport` | `guides` `{ thirds, center, safe_areas, letterbox }` | adds `guides` |

Example: pilot a light 10 steps forward, eject, check the history.

```json
{"method": "editor.pilot", "params": {"entity": 4294967310}}
{"method": "editor.pilot", "params": {"fly": {"forward": 1, "seconds": 0.05, "steps": 10}}}
{"method": "editor.pilot", "params": {"entity": null}}
{"method": "editor.history", "params": {}}
```

| Code | Method | Meaning |
|---|---|---|
| `pilot_not_found` | `editor.pilot`, `editor.look_through` | No such entity |
| `pilot_no_transform` | `editor.pilot`, `editor.look_through` | The entity has no `Transform` |
| `pilot_in_play` | `editor.pilot` | A play world: pilot edits the level |
| `pilot_not_active` | `editor.pilot` | `fly` with no pilot session running |
| `pilot_no_selection` | `view.pilot` (status line) | Nothing selected to pilot |
| `look_through_not_camera` | `editor.look_through` | The entity is not a scene camera |
| `camera_preview_bad_size` | `editor.camera_preview` | Not off, small, medium or large |
| `screenshot_bad_multiplier` | `editor.screenshot` | Multiplier outside 1..=8 |
| `screenshot_bad_size` | `editor.screenshot` | A side outside 16..=16384 |
| `screenshot_bad_name` | `editor.screenshot` | Not a plain file name |
| `screenshot_no_project` | `editor.screenshot` | No project is open |
| `screenshot_needs_tiles` | `editor.screenshot`, `editor.capture_viewport` | Past the device's texture limit |
| `screenshot_render_failed` | `editor.screenshot` | No picture (no adapter) |
| `screenshot_write_failed` | `editor.screenshot` | The file could not be written |

## Selection and placement tools

ADR-7769. Code: `regia_editor::selection` (regions, select similar, the selection root), `pick_stack`, `measure_tool`,
`content` (the drop ghost), `overlays` (hover and ghost drawing), `viewport_agent` (`editor.lasso_select`). The user's
view is [Viewport](../user-guide/editor-guide/viewport.md).

### Keys and clicks

| Command | Key | Does |
|---|---|---|
| `selection.lasso` | L (held at the drag start) | The left drag draws a freehand lasso instead of a box. Run from the palette, every drag is a lasso until run again |
| (modifiers) | Shift / Ctrl+Shift / Ctrl | At the release of a box or lasso, and on a click: add, subtract, toggle. No modifier replaces |
| `selection.pick_stack` | Alt+click (no chord of its own) | Lists every entity under the cursor, nearest first, with its kind icon. Up and Down move, Enter picks, Escape closes. From the palette it opens under the pointer of the active pane |
| `selection.similar_kind`, `_components`, `_asset`, `_layer` | none | **Edit ▸ Select ▸ …**: replace the selection with every level entity similar to a selected one |
| `selection.toggle_root` | none | Marks the selection as selection roots (or unmarks them). Also the Outliner row menu's **Selection root** |
| `viewport.measure` | M | Measure mode: drag for a distance, click for the angle at the second point, hold V (`gizmo.vertex_snap`) to snap each point to a vertex, Escape leaves |

A selection root is the saved component `regia_editor::selection::EditorSelectionRoot`: a viewport click on a
descendant selects the nearest marked ancestor; Ctrl+click picks the descendant itself. The hover pre-highlight is a
thin outline at half the selection token's strength around what a click would select; it reads the id buffer the pane
already has (no extra read) and shows only while no drag, button or content drag is active. A content drag over a pane
shows a wire box of the asset's bounds where a release would place it; nothing is spawned until the release.

### Methods

| Method | Does |
|---|---|
| `editor.box_select { ..., mode }` | `mode` now also takes `subtract` |
| `editor.lasso_select { points, width?, height?, view?, mode? }` | Selects every entity with an id-buffer pixel center inside the polygon (even-odd rule). `points` is `[[x, y], ...]`, 3 or more. Answers `{ entities, mode, selection }`. Needs a GPU adapter (the id buffer) |
| `editor.select_similar { by? }` | `by` `kind` (default; the declared kind, else the role, every light type one kind, greybox shapes and models are meshes), `components` (the same component set, editor markers ignored), `asset` (a shared mesh, material, sound or prefab), `layer` (a shared layer; entities in no layer match each other). Locked, hidden and model part entities are skipped. Answers `{ by, entities }` |
| `editor.pick_stack { x, y, width?, height?, view?, select?, mode?, cpu? }` | The Alt+click list: `{ entities: [{ entity, name, distance, kind, glyph }], method, selected }`. `select` picks a row like Enter does |
| `editor.selection_root { entity, set?, probe? }` | Reads or sets the marker (one undo step). `probe`: `click_selects` is what a plain click on that entity selects |
| `editor.drop_preview { path, x?, y?, width?, height?, view?, place? }` | Where a drag of `path` over the pixel would place it: `{ places, ghost: { translation, rotation, scale, bounds_min, bounds_max } }`. `place: true` drops it there (one undo step) |
| `editor.measure { a?, b?, c?, active? }` | Distance `a` to `b` in level units (metres) and, with `c`, the angle at `b` in degrees. Shows it in the viewport. No points: reads the tool. `active: false` leaves measure mode |

### Example

```json
{ "method": "editor.lasso_select", "params": { "points": [[40, 100], [150, 60], [190, 120], [150, 180]], "mode": "add" } }
{ "method": "editor.pick_stack", "params": { "x": 160, "y": 120, "select": 1 } }
{ "method": "editor.measure", "params": { "a": [0, 0, 0], "b": [3, 0, 0], "c": [3, 0, 4] } }
```

The last answers `distance` 3, `angle_deg` 90.

### Error codes

| Code | Method | Meaning |
|---|---|---|
| `nothing_selected` | `editor.select_similar` | Nothing is selected to compare with |
| `no_entity` | `editor.selection_root` | No such entity |
| `drop_no_project` | `editor.drop_preview` | `place` with no project open |
| `drop_no_level` | `editor.drop_preview` | `place` with no level open |

Tests: `selection::tests` (lasso fill, region modes, select similar, selection root), `overlays::hover_tests`,
`pick_stack::tests`, `measure_tool::tests`, `content::tests::a_drag_shows_a_ghost_at_the_final_placement_and_spawns_only_on_release`,
the viewport rig tests `alt_click_lists_the_stack_and_the_keyboard_picks_a_row`,
`a_click_on_a_child_selects_its_selection_root_and_ctrl_picks_the_child`,
`m_enters_measure_mode_a_drag_and_a_click_measure_and_escape_leaves` and `the_hover_highlight_adds_no_id_buffer_request`,
`tests/viewport_render.rs` `a_lasso_over_two_of_three_cubes_selects_those_and_subtract_removes_one` (lavapipe) and
`tests/headless_editor` `select_similar_pick_stack_selection_root_and_measure_answer_over_the_protocol`.
| `viewport_badge_unknown_setting` | `editor.viewport_badges` | `reset` names no setting |
| `hide_no_entity` | `editor.hide` | An id in `entities` names no entity |
| `isolate_empty_selection` | `editor.isolate`, Shift+H | Nothing is selected to isolate |

## Viewport overlays (ADR-8199)

The viewport has six independently placed groups: **Transform**, **Snap**,
**Camera**, **View / Show**, **Tools**, and **Select**. Drag a group's grip within
the active pane, or use **Overlays → group → Layout** from the keyboard. Tab
reaches the menus, Enter opens or activates them, and arrow keys edit sliders.
Layout offers collapse to an icon menu, vertical orientation, docking to any edge,
normalized horizontal/vertical position, and reset. A collapsed icon has an
accessible name and opens all the group's controls. The persistent **Overlays**
menu also exposes every group, even if groups overlap or the pane is small.
Select provides **Box** and **Lasso**, using the existing selection shape.

`editor.viewport_overlays` requires `edit_scene` permission. A read method cannot change state, and this call saves per-user overlay layout and presets. It does not edit scene entities. It is available in the
editor; runtime-only hosts return `editor_not_running`. Parameters:

| Parameter | Meaning |
|---|---|
| `action` | `get` (default), `set`, `reset`, `preset_save`, `preset_apply`, `preset_delete` |
| `pane` | View kind: `perspective` (default), `top`, `front`, `side`, `bottom`, `back`, `opposite_side` |
| `group` | For layout `set`, or a single-group `reset`: `transform`, `snap`, `camera`, `view_show`, `tools`, `select` |
| `dock` | For `set`: `top`, `bottom`, `left`, `right`, `floating` |
| `position` | For `set`: finite normalized `[x,y]` in `0..1`; along a dock edge or inside the pane when floating |
| `collapsed` | For `set`: boolean; leaves a named icon menu |
| `vertical` | For `set`: boolean; arranges the group's controls vertically |
| `name` | Required by preset operations: trimmed, 1–64 UTF-8 bytes, no control characters; up to 32 per view kind |
| `selection` | For `set`: `box` or `lasso`, session-only; not part of presets |

Default top-edge groups at `[0,0]` pack into rows; a nonzero position places a
group independently along its edge.

A reply contains `pane`, `groups` (each group's `dock`, `position`, `collapsed`,
`vertical`), sorted `presets` names, and `selection`. Unknown parameters and invalid
values are refused before any mutation. Preset save replaces an existing name;
apply restores the entire layout for that view kind. Reset restores a group or the
current kind's layout; it keeps valid presets. Explicit reset recovers malformed
overlay preferences by replacing that invalid overlay preference with defaults.

```json
{"action":"set","pane":"perspective","group":"snap","dock":"left","vertical":true,"collapsed":true}
{"action":"preset_save","name":"Left snap"}
{"action":"preset_apply","name":"Left snap"}
```

Layouts and presets are per user, shared by main and instance viewports of the
same view kind. They use the extensible `viewport.overlays` text preference in
`editor_prefs.ron`; no project or frozen view-state fields change. The existing
preferences writer saves atomically; its diagnostics report write failures.
Changing overlays never resizes the rendered pane, dirties a scene or adds undo.
Colors use the current egui theme; sizes scale with `appearance.ui_scale`.

| Error code | Meaning / recovery |
|---|---|
| `editor_not_running` | Connect to an editor host. |
| `viewport_overlay_invalid` | Invalid action, fields, value, name, or preset capacity; use the bounds above. |
| `viewport_overlay_preset_missing` | No preset with that name for the requested view kind. |
| `viewport_overlay_read_only` | Preferences are from a newer editor; that file is preserved. |
| `viewport_overlay_saved_invalid` | Saved overlay preferences are invalid or exceed 256 KiB; explicit reset recovers them. |

Acceptance: `cargo test -p regia_editor --test viewport_overlays`; dispatcher
coverage: `viewport_overlays_dispatch_presets_and_reject_invalid_edits_atomically`
in `agent_parity_shell`.
