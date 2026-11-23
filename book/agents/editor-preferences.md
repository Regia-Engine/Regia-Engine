# Editor preferences, keymap, layout and project settings

ADR-2321. Code: `regia_editor::{editor_prefs, editor_keymap, editor_layout, project_settings_ext, prefs_panel,
baseline_agent}`.

The editor keeps four kinds of settings, each with a panel and an agent surface:

| What | Where it is stored | Panel | Methods |
|---|---|---|---|
| Preferences (typed per-user settings) | `<user config>/regia/editor_prefs.ron` | Edit ▸ Preferences… (Settings tab; Theme tab for the theme, accent, every token and the diagnostic colors) | `editor.prefs_get`, `editor.prefs_set`, `editor.prefs_reset`, `editor.prefs_panel` |
| Keymap (user shortcuts of editor commands) | `<user config>/regia/editor_keymap.ron` | Edit ▸ Preferences… (Keymap tab), F2 in the palette | `editor.keymap_get`, `editor.keymap_bind`, `editor.keymap_import`, `editor.keymap_export`, `editor.keymap_reset` |
| Dock layout | `<project>/.regia/editor_layout.ron`, and the user's default in `<user config>/regia/editor_layout.ron` | View ▸ Save layout to project / Save layout as my default / Reset layout | `editor.layout_get`, `editor.layout_save`, `editor.layout_apply`, `editor.layout_reset` |
| Project settings | `Regia.toml` (unknown keys kept), `config/input.ron` | Edit ▸ Project settings… | `editor.project_settings_get`, `editor.project_settings_set`, `editor.project_input_edit` |

`<user config>` is `$REGIA_EDITOR_CONFIG_DIR` when set (the files live directly in it). On Windows it is `%APPDATA%`, then `%USERPROFILE%`, then the temp
directory (`XDG_CONFIG_HOME`/`HOME` set by Git Bash/MSYS are ignored there). Elsewhere it is
`$XDG_CONFIG_HOME`, then `$HOME/.config`, then `$APPDATA`, then `%USERPROFILE%`, then the temp directory. The editor
appends `regia` to the base (so a temp fallback lands in `<temp>/regia`), and the table's `<user config>/regia/<file>`
composes with either reading. The editor binary reads the files on its first frame and writes them when they change
(atomic write: temp file, then rename). A world without
`editor_prefs::EditorConfigPaths` keeps everything for the session, which is how tests stay hermetic.

## Preferences

| Key | Kind | Default | Does |
|---|---|---|---|
| `appearance.theme` | choice `default`, `regia-dark`, `regia-light`, `high-contrast` | `default` | the editor's colors (same as `editor.set_theme { ui_theme }`; `high-contrast` is the editor's own AAA theme, [Editor theme](theme.md#high-contrast)) |
| `appearance.ui_scale` | float 0.5 to 3.0 | 1.0 | zoom of the whole interface, on top of the display scale (pixels per point = display scale x this, ADR-8589) |
| `appearance.text_scale` | float 0.8 to 2.0 | 1.0 | size of the editor's text, separate from the UI scale (egui text styles; spacing and icons keep their size) |
| `appearance.custom_theme` | text (a saved theme name) | empty | the saved custom theme in use (`themes/<name>.editor_theme.ron` in the user config directory); empty uses `appearance.theme`. Set by Save theme as… and `editor.theme_save` / `editor.theme_import` ([Editor theme](theme.md#saved-themes)) |
| `appearance.color_error` | text `#rrggbb` or empty | empty | error color of the Output Log, the Problems panel, status lines and badges; empty uses the theme's |
| `appearance.color_warning` | text `#rrggbb` or empty | empty | warning color (also the content browser's warning badges and the unsaved-changes label) |
| `appearance.color_info` | text `#rrggbb` or empty | empty | info color of the Output Log and the Problems panel |
| `appearance.color_success` | text `#rrggbb` or empty | empty | color of passing checks and valid items |
| `camera.fly_speed` | float 0.1 to 500 | 6.0 | the viewport camera's fly speed when a session starts (m/s) |
| `camera.field_of_view` | float 20 to 120 | 60.0 | the viewport camera's vertical field of view (degrees) |
| `viewport.grid` | bool | true | grid shown at start |
| `viewport.realtime` | bool | true | viewport redraws every frame at start |
| `viewport.camera_preview` | choice `off`, `small`, `medium`, `large` | `medium` | size of the selected scene camera's picture-in-picture preview ([Viewport tools](viewport-tools.md#pilot-look-through-camera-preview-screenshots-and-guides)) |
| `autosave.interval_minutes` | int 0 to 120 | 5 | crash-recovery autosave interval; 0 turns it off |
| `history.keep_autosaves` | int 0 to 100 | 10 | autosave versions kept per level and document ([Version history](editor-autosave.md#version-history)); 0 keeps none |
| `history.keep_saves` | int 0 to 100 | 10 | manual-save versions kept per level and document; 0 keeps none |
| `history.max_mb` | int 1 to 100000 | 512 | size cap of the project's version history; the oldest versions go first |
| `layout.restore` | bool | true | open a project with its saved layout (else the user's default, else the built-in one) |
| `layout.save_on_exit` | bool | true | save the dock layout into the project when the editor closes |
| `code.editor_command` | text | empty | what a click on `file:line` in the Output Log runs: `code`, `zed`, `$EDITOR`, or a command with `{file}`, `{line}` and `{col}`; empty uses `[editor] code_editor` of `.regia/agent.toml`, then `$VISUAL` and `$EDITOR`. The owner's only: `editor.prefs_set` refuses it with `pref_owner_only` ([Viewport tools](viewport-tools.md)) |
| `procgen.hit.<tool>.terrain` | bool | true | the procedural `<tool>` (`path`, `area`, `paint`, `box`) may land on terrain ([Procedural graph editor](procgen-editor.md#placement-tools)) |
| `procgen.hit.<tool>.meshes` | bool | true | the procedural `<tool>` may land on meshes |
| `procgen.hit.<tool>.skip_generated` | bool | true | the procedural `<tool>` ignores entities a generator made |
| `procgen.hit.<tool>.physics_mask` | int 0 to 4294967295 | 4294967295 | render layers (a bit each) a mesh must be on for the procedural `<tool>` to land on it |
| `play.window_width` | int 160 to 16384 | 1280 | width of the separate play window, physical pixels; written when the window closes ([The play window](pie-modes.md#the-play-window)) |
| `play.window_height` | int 120 to 16384 | 720 | height of the separate play window, physical pixels |
| `play.window_placed` | bool | false | open the play window where the last one was; off lets the window system place it (Wayland always does) |
| `play.window_x` | int -65536 to 65536 | 0 | the play window's left edge on the desktop, used when `play.window_placed` is on |
| `play.window_y` | int -65536 to 65536 | 0 | the play window's top edge on the desktop, used when `play.window_placed` is on |
| `play.resolution` | choice `window`, `hd_720`, `hd_1080`, `phone_9_16_portrait`, `phone_9_19_5_portrait`, `phone_9_19_5_landscape`, `tablet_3_4_portrait`, `tablet_3_4_landscape`, `custom` | `window` | the size the play world renders at in the play window, scaled to fit with bars ([Play resolution](pie-modes.md#play-resolution)); the toolbar's ⛶ menu sets it |
| `play.resolution_width` | int 16 to 16384 | 1280 | width of the custom play resolution, used when `play.resolution` is `custom` |
| `play.resolution_height` | int 16 to 16384 | 720 | height of the custom play resolution, used when `play.resolution` is `custom` |
| `camera.invert_orbit_x` | bool | false | horizontal orbit drags turn the camera the other way ([Viewport tools](viewport-tools.md), ADR-7755) |
| `camera.invert_orbit_y` | bool | false | vertical orbit drags tilt the camera the other way |
| `camera.invert_pan_x` | bool | false | horizontal pan drags move the view the other way |
| `camera.invert_pan_y` | bool | false | vertical pan drags move the view the other way |
| `camera.invert_fly_y` | bool | false | vertical look drags while flying (secondary button) tilt the other way |
| `camera.invert_wheel` | bool | false | the wheel zooms (and changes the fly speed) the other way |
| `camera.speed_with_distance` | bool | false | the fly speed grows with the distance to the orbit target (the set speed at 8 m) |
| `camera.orbit_selection` | bool | false | orbit drags turn around the selection's center without framing it first (the target when nothing is selected) |
| `camera.zoom_to_cursor` | bool | true | the wheel zooms toward the point under the pointer instead of the view center |
| `camera.navigation_preset` | choice `regia_default`, `three_button`, `numpad` | `regia_default` | mouse chords and view keys: the default, three-button mouse emulation (Alt+Shift+left pans), or numpad emulation (the number row as a keypad for views) |
| `camera.near_clip` | float 0.001 to 100 | 0.1 | the perspective viewport's near clip plane when a session starts (m); far stays at least ten times near, and a change of near and far together is checked as a pair (ADR-7797) |
| `camera.far_clip` | float 1 to 100000 | 4000.0 | the perspective viewport's far clip plane when a session starts (m) |
| `inspector.linked_vectors` | text up to 1024 bytes | empty | vector fields edited with linked components (`Transform.scale,Decal.size`); the inspector's chain toggle beside a Vec2/Vec3 field adds or removes its entry |
| `color.swatches` | text, at most 320 bytes | empty | the color picker's saved swatches, `#rrggbbaa` separated by commas ([Color picker](inspector.md#color-picker-adr-7786)); `editor.color_swatches` edits it |
| `color.recent` | text, at most 120 bytes | empty | the color picker's recent colors, newest first |
| `vcs.git_command` | text | empty | the git program the Revision Control window runs (a path, or a name on `PATH`); empty uses `git` from `PATH`. The owner's only: `editor.prefs_set` refuses it with `pref_owner_only` ([Revision control](revision-control.md)) |
| `ui.floating_windows` | choice `os`, `in_window` | `os` on Linux and Windows, `in_window` elsewhere | where a floated panel goes: its own OS window, or a window inside the main editor window (single-window mode; [Floated panels and OS windows](editor-os-windows.md), ADR-7821) |
| `vcs.lock_on_edit` | choice `ask`, `always`, `never` | `ask` | what the first edit of a lockable file (git LFS) does: ask whether to lock it, lock it, or nothing ([Revision control](revision-control.md#file-locks-git-lfs)) |
| `vcs.lockable_read_only` | bool | false | show lockable files you do not lock with a read-only badge in the Content Browser |

The defaults are the editor's behavior before preferences existed: without a file nothing changes. The first frame
applies only the settings that are not at their default; later frames apply the ones whose value changed. The
Preferences window searches labels, keys and descriptions, shows a Reset button beside every changed row and a "Reset all
to defaults" button.

`editor.prefs_get { query? }` returns `{ query, count, prefs: [{ key, category, label, description, kind, value, default,
is_default, changed, min, max, choices }], path, format_version, generation, saved, read_only, diagnostics, unknown_keys }`.
Read-only.

`editor.prefs_set { values }` sets several keys at once, all or nothing: if one value is refused, none changes. An integer
is accepted for a float setting. Returns the `prefs_get` result plus `changed` (the keys whose value changed). Mutating;
permission `edit_scene`.

`editor.prefs_reset { keys? }` returns the named keys (or every key) to the default and adds `reset`. Mutating;
permission `edit_scene`.

`editor.prefs_panel { tab?, query? }` opens the Preferences tab on the next frame and sets which tab it shows
(`"settings"`, `"keymap"` or `"theme"`) and its search text, as a person would; returns `{ tab, query, matches }` (the preference
keys the search matches). Mutating; permission `edit_scene`.

```json
{"method": "editor.prefs_set", "params": {"values": {"appearance.ui_scale": 1.25, "autosave.interval_minutes": 0}}}
```

### The file

```ron
(
    format_version: 1,
    values: { "appearance.ui_scale": Float(1.25), "camera.fly_speed": Float(12.0) },
)
```

Only values that differ from the defaults are stored. Reading: a file without `format_version` reads as version 1; a
newer version is refused and the file is left untouched (the editor then does not write it this session, `read_only`
says why); keys this build does not know are kept and written back, so a newer editor's settings survive a save by an
older one; a value of the wrong kind or out of range is dropped with a diagnostic and the default applies. Version 1 is
the first; there are no migration steps yet (`editor_prefs::prefs_migration_sources`).

## Keymap

Every editor command in the palette registry (`command_palette::COMMANDS`) can have a user shortcut: in the palette (F2
on a row), in Preferences ▸ Keymap (Set…, Default), or through `editor.command { bind }` and `editor.keymap_bind`. The
keymap is saved when it changes, whichever way it changed. Commands added for this page: `gizmo.translate` (W), `gizmo.rotate`
(E), `gizmo.scale` (R), `window.preferences`, `window.project_settings`, `layout.save`, `layout.save_default`,
`layout.reset`. The viewport reads W, E, R and F (`view.frame_selection`) itself while it has the pointer; when one of
those commands has a user shortcut, the viewport leaves its built-in key alone, so the binding replaces it. The viewport's
fly keys, its placement keys and the outliner's keys are commands too (ADR-2696; the table is in
[Viewport tools](viewport-tools.md)).

**Conflicts.** A user chord on one command that equals another command's built-in shortcut (Ctrl+Z is Undo's, A is
`camera.fly_left`'s, Delete is `outliner.delete`'s) or a fixed editor key (Ctrl+Y redo, Ctrl+Shift+P the palette) is listed
in `conflicts`: the user shortcut wins while it is bound, and the list says what it hides. A built-in shortcut stops counting once its own command has a user chord. Ctrl+Shift+P cannot
be bound at all. Binding a chord that another command's user shortcut holds moves it (reported as `moved` /
`moved_from`).

`editor.keymap_get` returns `{ commands: [{ id, category, title, shortcut, default_shortcut, user }], conflicts: [{ keys,
command, shadows }], path, format_version, saved, read_only, problems, unknown_commands }`. Read-only.

`editor.keymap_bind { bindings: { id: "Ctrl+Shift+G" | null } }` sets or clears user shortcuts, all or nothing, and adds
`moved: [{ keys, from, to }]`. Mutating; permission `edit_scene`.

`editor.keymap_import { path | text, mode?, dry_run? }` reads a keymap file (relative to the project, or an absolute
path under the project or the temp directory; anything else, `..` or a symlink leading out is `path_outside_project`;
at most 1 MiB) and merges it into the current keymap (`mode: "merge"`, default) or replaces it (`"replace"`). It adds `import: {
applied, moved_from, problems: [{ kind, message }], dry_run }`; with `dry_run` nothing changes and `conflicts` describes
the would-be keymap. Mutating; permission `edit_scene`.

`editor.keymap_export { path }` writes the user shortcuts (plus bindings of commands this build does not know) to a file
another editor can import; returns `{ path, bindings }`. The path follows the same rule as the import, and the file
name must end in `keymap.ron` (`my.keymap.ron`), else `bad_path`, so an export never writes over another document. Mutating; permission `edit_scene`.

`editor.keymap_reset` removes every user shortcut and adds `cleared`. Mutating; permission `edit_scene`.

```json
{"method": "editor.keymap_bind", "params": {"bindings": {"window.preferences": "Ctrl+Comma"}}}
```

### The file

```ron
(
    format_version: 1,
    bindings: { "view.toggle_grid": "Ctrl+G", "window.preferences": "Ctrl+Comma" },
)
```

Chords are written `Ctrl+Alt+Shift+Key` (`Ctrl` is Cmd on macOS; key names are egui's: `K`, `F5`, `Space`, `Comma`).
Reading keeps bindings of unknown command ids (`unknown_command` problem, written back), drops chords that do not parse
(`bad_chord`) and Ctrl+Shift+P (`reserved`), and when two commands claim one chord keeps the first in id order
(`duplicate`). Versioning is as for preferences.

## Layout

The layout is the dock's main surface as a tree of tab groups and splits naming panels by id (`viewport`, `outliner`,
`inspector`, `content`, `output_log`, `material`, `project_settings`, `preferences`, ...; `editor.layout_get` lists the
ones shown). A project opens with its `.regia/editor_layout.ron`, else the user's default layout, else the built-in one
(Preferences ▸ Layout ▸ Restore saved layout turns this off). The editor saves the layout into the project on exit
(Preferences ▸ Layout ▸ Save layout on exit).

Restoring normalizes: unknown ids, extension panels (they come back when their extension registers them) and repeated
panels are dropped; an empty tab group collapses its split; fractions are clamped to 0.05..0.95 (NaN becomes 0.5); a
panel of the built-in layout that the saving editor did not have (`known_panels`) joins the Content tab group, so a panel
a newer build adds is never hidden by an old file, while a panel the user closed stays closed. Nesting deeper than 10
splits or more than 512 tabs is refused (`layout_invalid`).

**Floating windows** (ADR-2771). Windows torn off the dock are saved and restored: each is saved as one flat tab group
(a window holding a split keeps all its panels as tabs) with its rectangle once it has been drawn (`rect`: x, y, width,
height in points). Restoring applies the same normalization as the main surface (a panel is in one place only; a window
left with no panel is dropped; at most 32 windows). A window that no longer fits the screen (a smaller monitor) is moved
back on it and shrunk to fit, never below 160 by 100 points. `editor.layout_get` lists them under `layout.floating`.

`editor.layout_get` returns `{ layout: { format_version, root, known_panels } | null, panels, source, dropped, added,
pending_requests, format_version, panel_regions, pixels_per_point, focused_widget }`; `source` is `default`, `project`, `user` or `session`; `layout` is `null` before the
editor window drew its first frame. Read-only.

Since ADR-7955, `panel_regions` maps the panels drawn in the latest UI pass to `[x, y, width, height]` in
logical points. These are clipped body rectangles, excluding dock tab bars and surrounding chrome; closed or
hidden panels have no entry. Multiply by `pixels_per_point` to crop `seeing.screenshot` pixels (round inward).
`focused_widget` is the actual egui keyboard-focus ID as an opaque string, or `null` for no focused widget.
Compare IDs only within the same editor session. Before a window has drawn, regions are empty and scale/focus
are null. The observations are reset every pass and are never saved into `.regia/editor_layout.ron`.
Layout save/apply/reset replies include the same observations; a queued change is visible only after drawing.


`editor.layout_save { scope? }` writes the shown layout into the project (`scope: "project"`, default) or as the user's
default (`"user"`) and adds `saved_to`. A file written by a newer editor is not overwritten (`layout_newer`). Mutating;
permission `edit_scene`.

`editor.layout_apply { layout?, open?, reload? }` queues layout changes for the next frame: `layout` shows a layout in the
shape `layout_get` returns (validated now: a layout naming no panel is refused), `open` brings a panel forward by id
(adding its tab when closed), `reload` re-reads the saved layout. Adds `queued`. Mutating; permission `edit_scene`.

`editor.layout_reset` queues the built-in layout (the saved files are not touched). Mutating; permission `edit_scene`.

```json
{"method": "editor.layout_apply", "params": {"open": "preferences"}}
```

### The file

```ron
(
    format_version: 2,
    root: Split(axis: Horizontal, fraction: 0.17,
        first: Tabs(tabs: ["outliner"], active: 0),
        second: Tabs(tabs: ["viewport", "content"], active: 0)),
    known_panels: ["viewport", "outliner", "content"],
    floating: [(tabs: ["problems"], active: 0, rect: Some((40.0, 60.0, 480.0, 320.0)))],
)
```

`axis: Horizontal` puts `first` on the left, `Vertical` on top; `fraction` is `first`'s share. Versioning is as for
preferences. Version 2 added `floating`; migration step 1 to 2 (`editor_layout::layout_migration_sources`) is "read the
file as it is": a version 1 file has no floating windows and is written back as version 2 the next time the layout is
saved. An older editor refuses a version 2 file as newer and leaves it untouched.

## Project settings

Edit ▸ Project settings… opens the `Regia.toml` page: name, id, version, build number, organization, the default level
(`startup_level`, picked from the project's `.level.ron` files and level folders), the platform targets (one
`[platforms.<os>]` table each for `linux`, `windows`, `android`, `macos`; macOS is testing-only) with each target's quality
preset (`render_profile`: `MobileLow`, `MobileHigh`, `DesktopLow`, `DesktopHigh`, `DesktopUltra`, `Deck`, `Cinematic`, or
the engine default), the content chunk rules, the XR section, and the input maps in
`config/input.ron` (actions, contexts, bindings) with its problems (a binding writing an undeclared action, an action
nothing binds). The input maps can be edited there too: add or
remove an action (with its kind), add a binding to a context (the input in the file's RON form, `Key("Space")`,
`GamepadButton(South)`; a missing context is created) or remove one; `config/input.ron` is written at once (created when
missing) in canonical RON, so comments and hand formatting in it are not kept. Every key the page does not model is
kept. Save refuses an unknown render profile, a default level that
is not a level or leaves `assets/`, and an empty version; an unknown `[platforms.<os>]` table is a warning and is kept.
Each context also shows its priority and "Consumes lower" switch, and each binding its modifiers and triggers as RON
lists (`[Scale((2.0, 2.0, 1.0)), InvertY(true)]`, `[Pressed, Hold(0.5)]`; `[]` clears), changed with the **Set modifiers
and triggers** button. A write is refused when a trigger time is not a positive number, a `Chord` names an action that is
not declared (or the binding's own), or a modifier has a number that is not finite. **Conflicts** are listed under the
maps: two actions on one input in one context with a trigger in common, the same input bound to the same action twice,
and a consuming context that evaluates first (higher priority, or equal priority and a name that sorts first, the
runtime's order) and hides the other context's binding of the same input. Every write is one
undo step (Ctrl+Z restores `config/input.ron` byte for byte, or removes it when the write created it).

What the save keeps: every action, context (with priority and `consume_lower`), binding, modifier, trigger, and the
`spacemouse` section. What it loses: comments, the order of keys and hand formatting (the file is rewritten in canonical
RON; this was already so for the action and binding edits).

`editor.project_settings_get` returns `{ project: { name, id, version, build_number, organization, startup_level },
platforms: { linux|windows|android|macos: { enabled, render_profile } }, render_profiles, levels, input: { path, actions:
[{ name, kind }], contexts: [{ name, priority, consume_lower, bindings: [{ action, input, modifiers, triggers }] }], problems, conflicts } | { error } |
null, config_files, problems: [{ key, message, blocks_save }] }`. Read-only.

`editor.project_settings_set { project?, platforms?, content?, save? }` edits the same fields: `project: { name?, id?, version?,
build_number?, organization?, startup_level? }` (`startup_level` relative to `assets/`, `""` clears it), `platforms: {
<os>: { enabled?, render_profile? } }` (`render_profile` accepts `mobile_high`-style spellings and stores the canonical
name; `null` returns to the engine default; disabling a target removes its table), `content: { base_url? }` (the
`[content]` chunk host of the page's **Chunk host** field, trimmed; `""` removes the key; ADR-7747). The `get`
result reports it as `content: { base_url }`. Nothing is written when a value is
refused. `save: false` validates without writing. Returns the `get` result plus `saved`; a save reloads the Project
Settings panel unless it has unsaved edits. Mutating; permission `edit_scene`.

```json
{"method": "editor.project_settings_set", "params": {"platforms": {"android": {"render_profile": "MobileHigh"}}}}
```

`editor.project_input_edit { edits }` applies input-map edits in order, all or nothing: `{ op: "add_action", name, kind }`
(`Button`, `Axis1D`, `Axis2D`, `Axis3D`), `{ op: "remove_action", name }` (its bindings in every context go too),
`{ op: "add_binding", context?, action, input }` (`context` defaults to `Gameplay`; the action must be declared) and
`{ op: "remove_binding", context, index }`, and (ADR-2771) `{ op: "set_context", context, priority?, consume_lower? }` (a
missing context is created), `{ op: "set_modifiers", context, index, modifiers }` and `{ op: "set_triggers", context,
index, triggers }` where `modifiers` and `triggers` are RON list text as in the file. Returns `{ input }` in the
`project_settings_get` shape, with each binding's `modifiers` and `triggers` and the input's `conflicts`. One call is one
undo step (`editor.undo`). Mutating; permission `edit_scene`.

```json
{"method": "editor.project_input_edit", "params": {"edits": [{"op": "add_action", "name": "Jump", "kind": "Button"}, {"op": "add_binding", "action": "Jump", "input": "Key(\"Space\")"}]}}
```

`editor.project_settings_set` also accepts `package_hygiene` (ADR-8244). The object may set
`max_memory_bytes`, `max_file_bytes`, `max_total_bytes`, `max_work`, `scan_window_bytes` and
`max_matcher_bytes`. Each value is a positive integer no larger than `i64::MAX`, and
`scan_window_bytes` must not exceed `max_memory_bytes`. Absent keys keep the effective
defaults (32 MiB memory, 1 GiB per file, 64 GiB total, 1 TiB work, 1 MiB scan window, 8 MiB
matchers). The call writes only those integers into `[package_hygiene]` and leaves
`forbidden_strings`, `deny_terms` and `allow` in place. A zero, a non-integer, or a window
larger than memory is `bad_settings` and writes nothing. `editor.project_settings_get`
returns the effective ceilings at `pages.package_hygiene`. The packaging page edits the same
six numbers; an unchanged drag does not write, and a hygiene edit does not stamp Android
packaging keys.

```json
{"method": "editor.project_settings_set", "params": {"package_hygiene": {"max_memory_bytes": 4096, "scan_window_bytes": 1024}}}
```

## Fuzz targets

`fuzz.targets` lists `editor.prefs`, `editor.keymap` and `editor.layout` (`source: "extension"`, round trip on). Each
parses its file, consumes it (validates every value; builds the dock for a layout) and re-encodes it. The editor's other
persisted files have targets too (ADR-7902): `editor.window_state`, `editor.inspector_pins`, `editor.profile`,
`editor.view_state`, `editor.snap_steps`, `editor.saved_searches` and `editor.theme` (saved themes). All of these files are frozen formats (`editor-prefs`, `editor-keymap`,
`editor-layout` and the others in `docs/formats/`), checked by `cargo xtask check-formats` and read by
`regia upgrade-project`.

## Errors

| Code | Meaning |
|---|---|
| `bad_query` | `query` is not a string |
| `bad_tab` | `tab` is not `settings` or `keymap` |
| `unknown_pref` | no preference has the key |
| `bad_pref_value` | `values` is missing or a value does not fit its preference (kind, range, choice) |
| `pref_owner_only` | `editor.prefs_set` named a preference only the owner sets (`code.editor_command`) |
| `prefs_parse`, `prefs_newer`, `prefs_io` | the preferences file does not parse, is from a newer editor, or cannot be read |
| `unknown_command` | no editor command has the id |
| `bad_binding` | `bindings` is malformed, a chord does not parse, or it is Ctrl+Shift+P |
| `bad_mode` | `mode` is not `merge` or `replace` |
| `bad_path` | `path` is missing or empty, or (`editor.keymap_export`) its file name does not end in `keymap.ron` |
| `path_outside_project` | `path` is outside the project and the temp directory, has `..`, or a symlink in it leads out (`data.path_reason` says which) |
| `keymap_parse`, `keymap_newer`, `keymap_io` | the keymap file does not parse, is from a newer editor, or cannot be read or written |
| `no_project` | the method needs an open project |
| `no_user_config` | `scope: "user"` in a world without per-user files |
| `bad_scope` | `scope` is not `project` or `user` |
| `no_layout` | the editor window has not drawn a layout yet |
| `unknown_panel` | `open` is not a panel id |
| `bad_layout_request` | `editor.layout_apply` was given nothing to do |
| `layout_parse`, `layout_newer`, `layout_invalid`, `layout_io` | the layout does not parse, is from a newer editor, is too deep or large or names no panel, or cannot be written |
| `project_settings_read` | `Regia.toml` cannot be read or parsed |
| `bad_settings` | a `project` field is unknown or of the wrong type, or `platforms`/`enabled` is malformed |
| `unknown_platform` | not one of `linux`, `windows`, `android`, `macos` |
| `bad_render_profile` | `render_profile` is not a render profile name |
| `project_settings_invalid` | the result would not save (bad default level, empty version, ...); nothing was written |
| `bad_input_edit` | `edits` is malformed, or an edit names an unknown or duplicate action, an unknown kind, context or index, or would leave an invalid trigger or modifier (see Project settings) |
| `bad_input_source` | `input` is not an input source in RON |
| `bad_input_modifiers`, `bad_input_triggers` | `modifiers` or `triggers` is not a RON list of modifiers or triggers |
| `input_config_read`, `input_config_write` | `config/input.ron` does not parse or cannot be read, or the write failed |

## Scoped shortcuts, lookup and presets

ADR-7767. Code: `regia_editor::editor_keymap` (scopes, `lookup`, `key_states`, presets), `command_palette::show` (the
dispatcher), `prefs_panel` (the Keymap tab).

**Scopes.** A user binding has a scope: `global` (the default, and every binding written before scopes), `viewport`,
`outliner`, `inspector`, `content`, `graph` (material, animation, procedural, behavior, state tree, conversation and VFX
canvases), `timeline` (sequence curves, shots, clips, montages, UI animation) or `text` (a text field has the keyboard).
The dispatcher tries the bindings of the focused panel's scope first, then the global ones; while a text field has the
keyboard only `text` bindings run (and F1). The same chord may be bound in two different non-global scopes: X in the
viewport and X in the timeline run different commands, each only with its panel focused. A chord moves only inside one
scope. `conflicts` reports a clash only inside one scope or against a global binding (each entry has `scope`). Two
built-in shortcuts never conflict (their panel arbitrates: W moves the gizmo, held with the right mouse button it
flies). The built-in shortcuts of the viewport's commands (camera, gizmo, framing, placement, collider handles) are in
the `viewport` scope and the outliner's in `outliner`; rebinding such a command with Set… or F2 keeps it in that scope.
Chords with more modifiers are tried first, so a global Shift+X is not taken by a scoped X.

**Safety keys.** Escape (stop play), Ctrl+S (save), Ctrl+Z (undo), Ctrl+Shift+Z and Ctrl+Y (redo) are global: binding
one with a non-global scope is refused with `shortcut_safety_key`, so no panel can take them away.

`editor.keymap_bind { bindings, scope? }` binds every chord of the call in `scope` (absent: `global`, as before);
`bad_scope` for an unknown scope. `editor.keymap_get` adds `scope` to each command (the user binding's, or the built-in
shortcut's), `scopes` (every name) and `active_preset`.

`editor.keymap_lookup { chord }` returns `{ keys, commands: [{ id, title, scope, user }], fixed, conflict }`: every
command the chord runs in any scope (user bindings, and built-in shortcuts the user did not move), the fixed and safety
keys on it, and whether two uses collide. Read-only. The Keymap tab's **Press a key…** button does the same from the
keyboard (the next press is captured before any shortcut runs; Escape cancels; the capture ends when the Keymap tab is
no longer shown), and **Keyboard map** draws a keyboard
whose keys are colored bound, unbound or conflict for the chosen Ctrl, Shift and Alt layer; activating a key lists its
commands. Every key and button is a focusable widget (Tab, then Enter or Space).

**Presets.** Named keymaps are files `<user config>/regia/keymaps/<name>.editor_keymap.ron` in the keymap file's version 1
shape, plus two built-ins: `Default` (no user shortcuts) and `Laptop` (Drop to floor moves from End to Alt+Down in the
viewport, and the Outliner's Delete also answers Backspace). There are no numpad view keys to move yet: no command binds
a numpad key and egui reports numpad digits as the main-row digit keys. Applying a preset replaces the keymap at once (no restart) and the editor then saves
it as the user's keymap.

- `editor.keymap_preset_list` returns `{ presets: [{ name, builtin, path }], dir, active }`. Read-only.
- `editor.keymap_preset_apply { name }` adds `applied` and `problems`; `preset_unknown` for no such preset. Mutating.
- `editor.keymap_preset_save { name }` adds `saved_to`; `preset_name` for a name that is not 1 to 64 letters, digits,
  spaces, `-` or `_`, `preset_builtin` for `Default` or `Laptop`, `no_user_config` without per-user files. Mutating.

A scoped binding's key in the file is `"<scope>:<command id>"`; the version stays 1, and an older editor keeps such a key
as an unknown command and writes it back unchanged:

```ron
(
    format_version: 1,
    bindings: { "viewport:view.toggle_grid": "X", "timeline:view.toggle_labels": "X", "window.preferences": "Ctrl+Comma" },
)
```

Reading drops a safety key bound in a scope (`safety_key` problem) and keeps a key with an unknown scope as an unknown
command. Extension commands (when extensions register commands) bind through the same scope and safety rules.

```json
{"method": "editor.keymap_bind", "params": {"bindings": {"view.toggle_grid": "X"}, "scope": "viewport"}}
```

## Settings window

Edit ▸ Preferences… opens one Settings window with three tabs, **Preferences**, **Keymap** and **Project Settings**, and
one search box: the Preferences tab says how many project settings match and the Project Settings tab how many
preferences match, each with a button to switch. Project Settings lists every `Regia.toml` key (tables flattened to dotted
keys) that matches, with its value; **Open Project Settings** opens the editing panel. A preference that differs from
its default shows a dot and a bold label, with **Reset** beside it; a category with a changed value has **Reset
category**. **Export preferences…** and **Import preferences…** write and read a file in the `editor_prefs.ron` shape;
**Import replaces** picks replace over merge (for the keymap's Import too).

- `editor.prefs_reset { category }` resets one category (`Appearance`, `Camera`, ...; case-insensitive) and adds
  `category`; `unknown_category` otherwise. `keys` and no argument work as before.
- `editor.prefs_export { path }` writes the preferences; the file name must end in `prefs.ron` (`my.prefs.ron`), else
  `bad_path`; the path follows the keymap export's rule. Returns `{ path, values }`. Mutating.
- `editor.prefs_import { path | text, mode?, dry_run? }` merges (default) or replaces; adds `import: { applied, changed,
  skipped_owner_only, diagnostics, dry_run }`. An agent import never changes `code.editor_command` (it stays as it was and
  is listed in `skipped_owner_only`). Mutating.
- `editor.prefs_panel { tab: "project" }` shows the Project Settings tab; the result adds `project_matches` (the
  `Regia.toml` keys the search matches).

## Tooltips and contextual help

ADR-7767. Code: `regia_editor::tooltips` (`RichTooltip`), `about` (contextual help).

Registered menu items (including File, Create, Edit and View), palette entries, Keymap rows and preferences show one tooltip shape: the
name, what it does, the default value, the shortcut (with its scope when not global) and the agent route (an
`editor.command { run }` id, an agent method, `editor.prefs_set`, or for an Inspector field the `editor.set_property`
path). `tooltips::field_doc` and `tooltips::field_default` read a component field's doc comment and default for the
Inspector's field rows.

**F1** (`help.context`, rebindable) opens the book page of the focused panel; over an Inspector field it opens the agent
page section whose heading names the field's component, when one exists. Every panel has a page (an extension's panel
documents itself). Keyboard focus on a field label also supplies its context. Moving away clears
the hover context; the rendered HTML book opens with the section anchor, while a Markdown-only
installation opens the source page.

`editor.help_context { panel?, field?, open? }` returns `{ panel, page, anchor, path, agent_page, queued }`: `panel` is a
panel id (default the focused one), `field` is `{ component, path }`, `path` is the local file (the rendered page, else
its Markdown; null without a local book) and `open: true` opens it on the next frame. `unknown_panel`, `help_no_page`
(an extension's panel), `bad_field` (no `component`). Mutating (it can open a viewer).

```json
{"method": "editor.help_context", "params": {"panel": "content"}}
```

### Errors of scopes, presets, settings and help

| Code | Meaning |
|---|---|
| `shortcut_safety_key` | a safety key (Escape, Ctrl+S, Ctrl+Z, Ctrl+Shift+Z, Ctrl+Y) was bound with a non-global scope |
| `bad_scope` | (`editor.keymap_bind`) `scope` is not a shortcut scope name |
| `preset_name`, `preset_builtin`, `preset_unknown` | a bad preset name, a built-in preset's name for a save, no such preset |
| `unknown_category` | `editor.prefs_reset { category }` names no preference category |
| `bad_tab` | (`editor.prefs_panel`) `tab` is not `settings`, `keymap` or `project` |
| `help_no_page`, `bad_field` | the panel has no book page (an extension's panel); `field` has no `component` |

## Project defaults and revert (ADR-8196)

The Project Settings tab in Settings marks a differing value with a dot and a bold label.
**Revert** resets that key; **Reset project**, **Reset content**, or **Reset platforms.android**
resets all documented keys in that category, including keys hidden by the search. Tab reaches
these buttons and Enter activates them. Each operation saves immediately and creates one undo
step. Undo restores the exact previous manifest bytes, including comments; Redo reapplies the edit.
These operations do not dirty the open level.

Both `editor.project_settings_get` and `editor.project_settings_set` add `settings`, an array of
`{ key, value, default, changed, resettable }`. Values and concrete defaults are TOML text.
`default: "(unset)"` means remove the override and use engine behavior. Unknown defaults have
`default: null`, `changed: null`, `resettable: false` and are preserved by category reset.
`editor.prefs_get` adds `changed` (the inverse of `is_default`) to each existing preference row.

| Key | Default source |
|---|---|
| `project.version` | Fresh project: `"0.1.0"` |
| `project.build_number` | Fresh project: `1` |
| `project.organization`, `project.startup_level` | Documented empty/unset value: `""` |
| `content.base_url`, `platforms.<os>.render_profile` | Remove the optional override |
| Project name/id and other manifest keys | No generic default; never reset automatically |

`editor.project_settings_set` accepts `reset_keys: ["project.version"]` or
`reset_category: "project"`, exclusively. Existing `save: false` validates and previews without
writing or recording undo. Explicit edits, if supplied too, apply after the reset. An unknown
reset key or a malformed reset selector returns the existing `bad_settings` error without
writing anything. All saved changes through this method now use the same single undo step.

```json
{"method":"editor.project_settings_set","params":{"reset_keys":["project.version"]}}
{"method":"editor.project_settings_set","params":{"reset_category":"platforms.android"}}
```
