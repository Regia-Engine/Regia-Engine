# Editor command palette (`editor.command`)

ADR-273 E02, ADR-1842. Code: `regia_editor::command_palette`.

Ctrl+Shift+P (Cmd+Shift+P on macOS) opens a searchable list of editor commands. Typing narrows it with the fuzzy matcher
of `regia_ui::fuzzy` over `Category: Title`; Up and Down move the highlight, Enter runs it, Escape closes. Commands you
ran recently rank higher (the last five, newest first). The shortcut is ignored during play-in-editor. The palette and the
agent run the same code (`command_palette::run`), so they cannot disagree.

| Id | Does |
|---|---|
| `edit.undo`, `edit.redo` | step the undo stack |
| `view.frame_selection` | frame the selection in the editor camera |
| `view.toggle_grid`, `view.toggle_labels`, `view.toggle_realtime`, `view.toggle_audio_gizmos` | flip the viewport setting |
| `gizmo.translate`, `gizmo.rotate`, `gizmo.scale` | set the viewport gizmo mode (built-in keys W, E, R in the viewport) |
| `window.preferences`, `window.project_settings` | open the Preferences or Project Settings tab (next frame) |
| `layout.save`, `layout.save_default`, `layout.reset` | save the dock layout into the project or as your default, or reset it (next frame) |
| `window.world_settings` | open the World Settings tab (next frame) |
| `selection.drop_to_floor` | drop the selection onto the surface below (built-in key End in the viewport) |
| `placement.toggle_align_normal` | flip "align to surface normal" for drops |
| `collider.toggle_handles` | flip the viewport's collider tool (built-in key C) |
| `gizmo.vertex_snap` | hold command (V while translating); run from the palette it toggles vertex snap on |
| `camera.fly_forward`, `camera.fly_back`, `camera.fly_left`, `camera.fly_right`, `camera.fly_up`, `camera.fly_down` | hold commands: W, S, A, D, E, Q with the right mouse button; binding one moves the key |
| `outliner.delete`, `outliner.duplicate`, `outliner.group` | the outliner's Delete, Ctrl+D and Ctrl+G on the selection |
| `file.new_level` | File → New level; refused (the status line says so) while the level has unsaved changes (ADR-7307) or while a prefab edit session is open |
| `file.new_level_discard` | File → New level, discarding unsaved changes (the menu's second click) |
| `create.cuboid`, `create.sphere`, `create.capsule`, `create.cylinder`, `create.cone`, `create.conical_frustum`, `create.plane`, `create.torus`, `create.tetrahedron`, `create.ramp`, `create.prism` | Create → Shape: one undoable shape at the camera target, selected |
| `view.toggle_profiler`, `view.toggle_diagnostics` | View → Profiler, View → Diagnostics: show or hide the window |
| `view.toggle_revision_control` | View → Revision Control (and the status bar's revision slot): show or hide the git window ([Revision control](revision-control.md)) |
| `view.toggle_file_locks` | show or hide the File Locks window (git LFS locks; [Revision control](revision-control.md#file-locks-git-lfs)) |
| `vcs.unlock_all_mine` | release every git LFS lock you hold (`editor.vcs_unlock { all_mine: true }`) |
| `recovery.restore`, `recovery.discard` | the crash-recovery window's Restore and Discard autosave, on the first offered autosave (next frame); Restore is refused while a prefab edit session is open (Discard never touches the scene) |
| `viewport.toggle_spacemouse_object` | the viewport toolbar's "SpaceMouse moves selection" (the SpaceMouse Object tool) |
| `view.zoom_in`, `view.zoom_out`, `view.reset_zoom` | step the interface zoom (`appearance.ui_scale`) by 10 % or back to 100 %; built-in keys Ctrl+Plus (or Ctrl+=), Ctrl+Minus, Ctrl+Alt+0, also View → Reset zoom (ADR-7733) |
| `material.undo`, `material.redo`, `material.reload` | the Material panel's Undo, Redo and Reload from disk (its own history; next frame) |
| `terrain.retry_level_terrain` | the Terrain panel's Retry after a failed level-terrain load |
| `lightmap.discard_checkpoint` | the Lightmaps panel's checkpoint Discard: delete the checkpoint of the bake it would start |
| `window.open_<panel id>` for every built-in panel (`viewport`, `outliner`, `inspector`, `content`, `output_log`, `material_graph`, `seq_curves`, `ui_designer`, `fracture`, ...; the palette generates one per built-in panel, so a new panel has its command at once, ADR-7744) | bring that tab forward, adding it when closed (next frame); the ids after `window.open_` are `editor.layout_apply`'s `open` ids. With Ctrl+Shift+P they are the keyboard route to the [editor guide](../user-guide/editor-guide/index.md)'s panels (ADR-9810) |
| `gizmo.select`, `gizmo.toggle_space`, `gizmo.toggle_snap`, `gizmo.rotate_step`, `gizmo.rotate_step_back`, `gizmo.pivot_offset`, `placement.toggle_surface_snap`, `placement.toggle_surface_align` | the select tool (Q), the next reference space (Ctrl+Backtick), the Snap toggle, turning the selection by the angle step (U, Shift+U), clearing the pivot offset (P is its hold key), and surface snap with its alignment ([viewport tools](viewport-tools.md), ADR-7756) |
| `view.toggle_motion_database` | show or hide the Motion database editor window ([Motion database editor](motion-database-editor.md), ADR-7740) |
| `view.content_drawer`, `view.log_drawer`, `view.dock_drawer` | toggle the content drawer (built-in Ctrl+Shift+Space) or the log drawer, dock the open drawer as a tab ([drawers](editor-console.md), ADR-7754) |
| `help.context` | F1: open the book page of the focused panel, or over an Inspector field its component's agent page section (next frame; ADR-7767, [Editor preferences](editor-preferences.md#tooltips-and-contextual-help)) |
| `prefab.overrides`, `prefab.edit_in_place` | the Overrides window and Edit in place for the selected prefab instance ([Prefab overrides and Edit in place](prefabs.md), ADR-7790) |

## Rebinding (ADR-2045)

Any command can get a user shortcut. In the palette, highlight a command and press F2: the row shows
"press a key (Esc clears)" and the next key chord (with Ctrl/Cmd, Alt and Shift as held) becomes its shortcut; Escape
removes the user shortcut instead. A chord already used by another command moves to the new one. Ctrl+Shift+P stays the
palette's own and cannot be rebound. User shortcuts run the command as the palette does (not during play, not while a
text field has focus); the built-in undo and redo keys keep working beside them. The keymap
(`command_palette::CommandKeymap`) is saved per user, with conflict detection, import and export: see
[Editor preferences, keymap, layout and project settings](editor-preferences.md) (ADR-2321).

## `editor.command`

`editor.command { query?, limit?, run?, bind? }` lists the commands matching `query` (empty lists all), best first, up to
`limit` (1 to 200, default 20); with `run` it first runs that command. `bind: { id, keys }` first gives a command a user
shortcut (`keys` like `"Ctrl+Shift+G"`, `"Alt+F5"`, `"F"`; `null` removes it). Returns
`{ query, ran: { id, message } | null, bound: { id, keys, taken_from } | null, count, commands: [{ id, category, title,
shortcut, default_shortcut, score, positions }], recent, open }`, where `shortcut` is the user's when bound.
Mutating; permission `edit_scene`.

```json
{"method": "editor.command", "params": {"query": "grid", "run": "view.toggle_grid"}}
```

## Errors

| Code | Meaning |
|---|---|
| `bad_query` | `query` is not a string |
| `bad_limit` | `limit` is not a whole number from 1 to 200 |
| `unknown_command` | `run` or `bind.id` is not a command id |
| `bad_binding` | `bind` is malformed, `bind.keys` is not a chord, or it is Ctrl+Shift+P |

## UI parity: `editor.actions` (ADR-7307)

Every button, menu item, toggle and checkbox of the editor has an agent path. The action registry
(`regia_editor::agent_parity::ACTIONS`) holds the shell's menus and toolbars: File, Create, Edit and View,
the Play toolbar, the prefab banner, the unsaved-changes prompt, the crash-recovery window, the
viewport toolbar's SpaceMouse toggle, the Material panel's history buttons and the Terrain panel's Retry. Each entry names the method or
`editor.command` id that does what the control does, and the UI draws it through the registry's helpers
(`action_button`, `action_menu_item`, `action_enabled`, `action_toggle`), so the label and the agent path
cannot drift apart. Every other control site is listed in `crates/regia_editor/agent-parity-allow.ron`
with a class:

| Class | Meaning |
|---|---|
| `method` | an agent method does it (`method`) |
| `command` | an `editor.command` id does it (`command`) |
| `os_dialog` | the control opens a file dialog; the agent calls `method` with the path |
| `confirm` | a confirmation prompt; the agent states the choice as `method`'s params (a `confirm` row must name its `method`; ADR-7747) |
| `view_only` | scroll, collapse, hover, tab, filter or layout only |
| `gap` | changes state and has no agent path yet (a follow-up item) |

The test `agent_parity_scan` scans the editor's non-test sources and fails on a control that is
neither drawn through a helper nor listed, and on a listed site that no longer exists.
`REGIA_AGENT_PARITY_UPDATE=1 cargo test -p regia_editor --test agent_parity_scan` refreshes the list:
known sites keep their class, new ones are added as `gap` for review. A `gap` row fails the scan too,
unless the test's `KNOWN_GAPS` list names it with its follow-up (ADR-7452): classify a refreshed row
(`method` or `command` with the real `via`, `view_only`, `confirm`, `os_dialog`), add its agent path, or
list it. The scan also counts the call sites of the accessible-name helpers `glyph_button(` and
`bare_checkbox(`. `editor.actions { class: "gap" }` lists what is left.

### Value editors (ADR-7747)

The scan also finds every value editor of non-test editor code: `DragValue::new(`, `ComboBox::`,
`selectable_value(` (each option of a combo or a row of choices), `Slider::new(`, `TextEdit::singleline(`,
`TextEdit::multiline(`, `text_edit_singleline(`, `text_edit_multiline(`, `drag_angle(` and
`color_edit_button`. Their rows sit in the allowlist's last section, after the buttons, with a `call` naming
the widget, and are classified the same way:

- `method` (or `command`): the agent route that sets the same value; the row's `note` names the param or op.
  A field that only stages an argument for a button (a name typed before **Create**) is `method` via the
  button's route, with the param in the note.
- `view_only`: a search or filter box, a zoom, which tab, row or page is shown. The row must carry a `note`
  saying why the value needs no agent path.
- `gap`: the value is state (saved, written to the world, used by a later action) and no route sets it. The
  row must carry a `note`: what the value is, what is missing and the follow-up job that adds it. Value-editor
  gaps are listed by these notes, not in the test's `KNOWN_GAPS` (which stays for buttons).

A value-editor `view_only` or `gap` row without a `note` fails the scan, and so does a new value editor that is
not listed. The reflect-driven Inspector draws every reflected field with a few shared widgets (numbers with a
drag value or slider, strings with a text edit, `@AssetFilter` strings with the asset picker, enums with a combo
box, rotations with angle drags, color-named arrays with a color button and channel drags); those sites are
`method` rows via `editor.set_property`, the generic route that writes the same field, and the
`agent_parity_routes` test proves that route for each widget kind. `editor.actions` returns a row's `note`
when it has one, so `editor.actions { class: "gap" }` lists the open value-editor gaps with their reasons.

## Level changed on disk: `editor.level_reload_resolve` (ADR-7747)

When the open level's file changes on disk while the level has unsaved edits, the editor does not reload it
behind the user's back: the status bar asks (**Reload from disk** / **Keep mine**). An agent answers the
same prompt with `editor.level_reload_resolve { choice? }` (needs `edit_scene`):

- no `choice`: reports `{ pending: false }` or `{ pending: true, path }` and changes nothing;
- `choice: "reload"`: discards the unsaved edits and reloads the level from disk (the undo history and the
  selection are cleared; the editor camera stays). Returns `{ choice, path, reloaded: true, status }`, where
  `status` is the reload's status line (`reloaded <path> in <n> ms`, or the open error);
- `choice: "keep_mine"`: keeps the edits; the next save overwrites the file. Returns
  `{ choice, path, reloaded: false, status: null }`.

A level with no unsaved edits reloads without a prompt, and the editor's own saves never prompt.

```json
{"method": "editor.level_reload_resolve", "params": {}}
{"method": "editor.level_reload_resolve", "params": {"choice": "keep_mine"}}
```

| Code | Meaning |
|---|---|
| `bad_choice` | `choice` is not `"reload"` or `"keep_mine"` |
| `no_pending_reload` | a `choice` was given but no changed-on-disk prompt is waiting |

`editor.actions { panel?, class? }` returns the registry and the list. `panel` keeps one registry panel
(`shell`, `shell.file`, `shell.play`, ...) or one source file stem (`ui_designer_panel`,
`procgen/panel`); `class` keeps one class. Returns `{ actions: [{ id, label, panel, class, method?,
command? }], sites: [{ panel, file, line, call, label, class, method?, command?, note? }], counts: { class: n } }`.
Read-only; permission `read`.

```json
{"method": "editor.actions", "params": {"panel": "shell.file"}}
```

| Code | Meaning |
|---|---|
| `bad_panel` | `panel` is not a string |
| `bad_class` | `class` is not one of the six classes |
| `allowlist_unreadable` | the compiled-in allowlist does not parse (a build problem) |

`cargo xtask editor-agent-coverage` holds every editor method to the runtime rules: a
`book/COVERAGE.md` row whose page names the method and each parameter, an `agent.guide` topic
(`editor-methods`) that names it, and a test that calls it. It reads
`docs/api-snapshots/editor-agent-methods.txt`, which the `editor_method_snapshot` test keeps equal to
what the editor registers (`--update` refreshes it). `book-check` runs it too.

`prefab.create_variant` opens the Create prefab variant dialog for the one selected prefab instance (with a different
selection it answers a hint and opens nothing). The shell draws the dialog, so it shows with the Content Browser and the
Outliner both closed (ADR-7744).

## Menu paths, menu items and favorites (ADR-7766)

Every palette row shows its menu path next to its shortcut (`Edit: Undo    Edit ▸ Undo    Ctrl+Z`). The palette holds
one registry: the commands above, then every menu item that is not one of them, with the id `menu:<Menu/Label>` (for
example `menu:File/Open level…`, `menu:Edit/Select all`, `menu:Play/▶ Play`), so every menu action can be found and run
from the keyboard. A menu-item entry runs through the agent call `editor.menu_list` names for it; items that open a
file dialog or ask for a confirmation (Open level…, Save level as…, Export…, Package for platform…, Clear caches…) are
listed but the palette only says to choose them from the menu, and an item the menu shows grayed out answers "is not
available now". A menu item that reads like an entry already listed is not added twice.

With an empty query the first row is **Search everything…** (`search.everything`, Ctrl+T: the
[universal search](universal-search.md)), unless recent commands come first.

**Favorites:** F4 on the highlighted row pins it to the favorites, or unpins it (a pinned row shows ★). Ctrl+Shift+F
(`view.favorites`) opens the favorites popup: Up and Down move, Enter runs, Delete unpins, Escape closes. Favorites are
saved per user in `saved_searches.ron` (see [universal search](universal-search.md#saved-searches)). Ctrl+Shift+F was
checked against every built-in shortcut and fixed editor key (`new_default_chords_have_no_conflict`); a user binding on
it is reported as a conflict like any other.

| Id | Does |
|---|---|
| `search.everything` | open the universal search window (Ctrl+T) |
| `view.favorites` | show or hide the favorites popup (Ctrl+Shift+F) |
| `selection.back`, `selection.forward` | the previous or next selection of the selection history (Alt+Left, Alt+Right) |
| `selection.history` | show or hide the selection history list |

`editor.command` takes `pin: { id, pinned? }` (default `true`; `false` unpins), applied before `run`, and its result adds
`menu_path` to each command row, `pinned: { id, pinned } | null` and `favorites` (the pinned ids in pin order).

```json
{"method": "editor.command", "params": {"pin": {"id": "view.toggle_grid"}}}
```

| Code | Meaning |
|---|---|
| `bad_pin` | `pin` has no string `id`, or `pinned` is not a boolean |
| `unknown_command` | `pin.id` is not a palette entry id |
