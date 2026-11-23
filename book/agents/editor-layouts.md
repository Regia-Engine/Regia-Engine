# Editor layouts, workspaces and feature profiles (agents)

ADR-7753. Code: `regia_editor::{workspaces, feature_profiles, window_state}`. The single dock layout of the project and the
user's default (`editor.layout_get`, `editor.layout_save`, `editor.layout_apply`, `editor.layout_reset`) are on
[Editor preferences, keymap, layout and project settings](editor-preferences.md); this page adds named layouts,
workspaces, maximize and the full-screen viewport, pane changes, feature profiles and the main window state.

Every method below queues the same request the matching control queues (the View menu, the toolbar's workspace
switcher, a tab group's `+` popup, a tab's context menu, the palette, Ctrl+Space and F11), so the agent route and the
person's route run one code path. Queued changes apply on the editor's next frame.

## Files

| What | Where | Shape |
|---|---|---|
| A named layout | `<user config>/regia/layouts/<name>.editor_layout.ron` | `editor_layout.ron` version 3 (panel instances, below; version 1 and 2 files read as version 3) |
| A named layout flagged as a workspace | `<user config>/regia/layouts/<name>.workspace.editor_layout.ron` | the same |
| A saved feature profile | `<user config>/regia/profiles/<name>.editor_profile.ron` | `.editor_profile.ron` version 1 |
| The project's active profile | `<project>/.regia/editor_profile.ron` (absent: Full) | `.editor_profile.ron` version 1 |
| The main window state | `<user config>/regia/editor_window_state.ron`, one record per project path | `editor_window_state.ron` version 1 |

Layout names are 1 to 64 ASCII letters, digits, spaces, `-` and `_`, compared without case (saving `Wide` over `wide`
replaces it). A newer editor's file is never overwritten.

## Named layouts

| Method | Params | Does |
|---|---|---|
| `editor.layout_list` | none | `{ layouts: [{ name, workspace, path }], dir }` |
| `editor.layout_save_named` | `name`, `workspace?`, `rename_from?` | saves the shown layout under `name` (as a workspace when `workspace`); with `rename_from`, renames that layout instead |
| `editor.layout_apply_named` | `name` | shows the layout (checked now, shown next frame) |
| `editor.layout_delete` | `name` | deletes the layout's file |

Set as my default is `editor.layout_save { scope: "user" }`; Reset to built-in is `editor.layout_reset`.

```json
{"method": "editor.layout_save_named", "params": {"name": "Wide", "workspace": false}}
{"method": "editor.layout_apply_named", "params": {"name": "wide"}}
```

## Workspaces

| Method | Params | Does |
|---|---|---|
| `editor.workspace_list` | none | `{ workspace, mode, maximized, fullscreen, parked, pending_requests, workspaces: [{ id, title, built_in, mode, extension, description, panels, opens_profiler }] }` |
| `editor.workspace_set` | `id`, `mode?` | switches workspace next frame; an optional mode overrides this workspace for the session |

Built-in ids: `level` (the built-in layout), `animation`, `materials`, `ui`, `world` (terrain, procedural graphs, the 2D
mode, layers, world settings), `cinematics`, `debug` (also opens the profiler window). A user workspace is
`user:<layout name>`. Switching parks the shown dock under the workspace it leaves for the rest of the session, so
switching back shows the same tree byte for byte; documents stay open (they live in their editors, not in tabs). The
palette has `workspace.<id>` for every built-in workspace.

ADR-8197 adds `terrain` and `tiles2d` workspaces. The mode ids are `world`, `terrain`,
`tiles2d`, `animation`, and `ui_design`. Level and World use World; Animation and UI use
Animation and UI design. Materials, Cinematics and Debug default to World. The mode
brings forward its existing tool palette and shows related tool-panel buttons in a toolbar
strip. Terrain enables terrain brushing; 2D tiles enables tile painting and disables terrain
brushing. Leaving either completes a current stroke and deactivates its viewport tool.
Documents and tool settings remain in their existing resources. A fresh workspace or explicit
mode change brings forward the palette; returning to a parked workspace preserves its tab
selections while activating its mode. The toolbar can bring its palette forward again.

The workspace list's top-level `mode` is the current mode id. Each workspace entry also
has `mode` and `extension` (the owning extension id, or null). The optional `mode` parameter
must be one of the ids above, never null. Overrides and parked docks last for this session;
user layout files retain their existing shape and do not store mode overrides. Built-in
and extension declarations provide startup defaults.

Extensions contribute `extension:<extension id>.<workspace name>` entries, marked with
`(extension)` in the switcher. See [Editor extensions](editor-extensions.md).

```json
{"method":"editor.workspace_set","params":{"id":"terrain"}}
{"method":"editor.workspace_set","params":{"id":"user:Review","mode":"animation"}}
```

## Maximize and the full-screen viewport

`editor.maximize_panel { panel?, fullscreen? }` toggles. Without `fullscreen`: a tab is maximized to the whole dock area
(`panel`, else the focused tab, else the viewport), or, when one is maximized, the exact previous dock comes back. With
`fullscreen: true`: the viewport alone with the menu bar, toolbars and status bar hidden, or back. The commands are
`view.maximize_panel` (Ctrl+Space) and `view.fullscreen_viewport` (F11), both rebindable with `editor.keymap_bind`. While
a tab is maximized, `editor.layout_get` and every layout save report the dock it replaced.

Escape order in the editor window: a panel that uses Escape keeps it; a running play session stops; then full screen
ends; only then does Escape ask to close the editor. One press does one of these.

## Pane changes

`editor.panel_replace { to, from? | near? }`: with `from`, `to` takes that tab's place (Replace with in the tab's
context menu); otherwise `to` is added to the tab group of `near`, or the focused group (the group's `+` popup). A panel
open elsewhere moves, so it is never in two places. The `+` popup also lists the group's own tabs (the list of a crowded
tab bar); picking one brings it forward. Dragging an asset or entity over a tab for 30 frames brings that tab forward.

Every tab bar ends in an overflow button (`▾`, right of the `+`) whose menu lists the group's hidden tabs: the ones
scrolled out of a crowded bar or clipped at its end. Picking one brings it forward and scrolls it into the bar.
`editor.tab_overflow` (no params, read-only) returns the same lists as of the last drawn frame:
`{ groups: [{ surface, node, tabs, hidden }], drawn }` (`tabs` and `hidden` are panel ids in tab order; `drawn` is false
before the dock has drawn once, when every `hidden` list is empty). Bring a hidden tab forward with
`editor.panel_replace { to, near }`, `to` being the hidden tab.

Area parameters accept a built-in panel id or `area:<surface>:<node>`, using the indices in
`editor.tab_overflow`. Addresses resolve against the last published dock, including extension
panels; use a fresh query after a layout edit. Queued edits retain the resolved panel anchors
and revalidate their areas at application time.

`editor.layout_join { from, into }` merges the entire area containing panel `from` into
the neighboring area containing panel `into`. Tabs append in order; the target keeps its
active tab. The empty source area collapses. Both areas must be on the same surface and
share an edge of positive length. Diagonal contact is not a neighbor.

`editor.layout_swap { from, with }` exchanges the two areas' entire tab stacks and their
active tabs. Their geometry stays in place; areas may be on different surfaces. Identifying
two tabs in the same area is refused for both operations. Both methods validate against the
last published layout and queue one atomic edit, then validate again when applied. They
return `{ queued: { from, into } }` or `{ queued: { from, with } }`. Missing or closed panels
and invalid area pairs cause no partial edit. There is no layout undo history at this base;
these view changes never enter the document undo stack.

The tab context menu and **Add panel** menu expose **Join with neighbor** and **Swap with**.
Use Tab and Enter to open Add panel and navigate its menus with the keyboard. Its spoken
name is Add panel; the overflow control is Hidden tabs. Menu activations run once per frame.

## Feature profiles

| Method | Params | Does |
|---|---|---|
| `editor.feature_profile_get` | none | `{ active, hidden_panels, hidden_kinds, built_in, saved, areas, format_version }` |
| `editor.feature_profile_set` | `name` or `profile`, `save_as?` | makes it the open project's active profile; `save_as` also saves it to share |

Areas: `3d`, `2d`, `animation`, `ui`, `networking`, `terrain`, `procgen`, `audio`, `scripting`. A profile hides whole
areas, single panels (`hidden_panels`, panel ids) and Create entity kinds (`hidden_kinds`). Hidden panels leave the
View ▸ Panels list, the palette's `window.open_*` commands, the `+` popup and Replace with; their open tabs close and
come back when a profile shows them again. Hidden kinds leave the Create menu and the palette's `create.*` commands.
Agents keep every method. Built-in profiles: `Full` (hides nothing) and `UI only` (every area but `ui`).

```json
{"method": "editor.feature_profile_set", "params": {"profile": {"name": "No audio", "hidden_areas": ["audio"]}, "save_as": "No audio"}}
```

## Main window state

When the editor exits, it records for the open project the main window's monitor, size, position, full-screen and
maximized state, the open level and the documents open in the Material and animation graph editors. On the next start of
the same project the window opens there (a monitor that is gone falls back to the primary display, centered; a window
off its monitor is centered on it; Wayland places windows itself), the level reopens when the editor still shows a new
unsaved level, and the documents reopen one per frame through the Content Browser's open route. No agent method: the
record follows the window.

## Panel instances

ADR-8145 (row (a) of ADR-8105, parity row P-003). Code: `regia_editor::panel_instances`. The Outliner, the Content
Browser and the Inspector may each be open up to eight times. The first is the plain tab (`outliner`, `content`,
`inspector`, instance 0); Window ▸ New Outliner, New Content Browser and New Inspector (palette `window.new_outliner`,
`window.new_content`, `window.new_inspector`) open the lowest free instance 1 to 7 as the tab `<panel>#<n>`, titled
"Outliner 2" to "Outliner 8", beside its panel. Each instance has its own state: an Outliner its search text, row cache
and columns (the view state's tree `outliner#<n>`; the first keeps `main`), a Content Browser its folder and search, an
Inspector its lock. Closing an instance drops its state; a new instance starts fresh.

| Method | Params | Does |
|---|---|---|
| `editor.panel_new_instance` | `panel` (`outliner`, `content` or `inspector`) | queues the lowest free instance for the next frame. Reply `{ panel, instance, title, queued }` (`panel` is the tab id, `outliner#1`) |

The tab ids work wherever a panel id does (`editor.layout_apply { open }`, `editor.panel_float`, `editor.maximize_panel`);
`editor.panel_instances` reports each tab's `instance`. `editor.layout_get` lists the tabs and, under `layout.instances`,
what each open instance shows, as the layout file saves it (version 3): `filter`, `folder`, `columns` and `locked`
(`guid:<32 hex>`), each left out when it is the default. A layout given to `editor.layout_apply` brings its
`instances` back with the tabs; a locked guid the open level lacks restores unlocked.

```json
{"method": "editor.panel_new_instance", "params": {"panel": "outliner"}}
{"method": "editor.outliner_columns", "params": {"tree": "outliner#1", "set": {"tags": true}}}
{"method": "editor.layout_get", "params": {}}
```

## Error codes

| Code | When |
|---|---|
| `bad_layout_name` | the name is empty, longer than 64 characters or uses other characters |
| `layout_not_found` | no layout has that name |
| `layout_exists` | a rename's new name is taken |
| `too_many_layouts` | 256 named layouts are saved already |
| `layout_io`, `layout_parse`, `layout_newer`, `layout_invalid` | the file cannot be written or read, or does not build |
| `no_user_config` | the editor keeps no per-user files (a bare test world) |
| `no_layout` | the editor has not drawn a layout yet |
| `unknown_workspace` | no workspace has that id |
| `unknown_panel` | no panel has that id |
| `panel_not_instanced` | `editor.panel_new_instance` named a panel without instances, or an instance id instead of the plain id |
| `panel_instances_full` | eight instances of the panel are open or queued |
| `panel_not_open` | the tab to maximize, replace or add next to is not open |
| `bad_pane_request` | a required pane parameter is missing, both `from` and `near` are given, both panels share an area, or join areas are not neighbors |
| `workspace_mode` | `mode` is not a supported mode id |
| `unknown_profile` | no built-in or saved profile has that name |
| `bad_profile` | no `name` or `profile`, an empty or long name, or a list over 512 entries |
| `profile_parse`, `profile_newer`, `profile_io` | the profile does not parse, is from a newer editor, or cannot be written |
| `no_project` | a profile needs an open project |

## Tests

`cargo test -p regia_editor --test editor_workspaces` (two named layouts survive a restart and each restores its tree;
switching workspace and back is byte-equal; every built-in workspace opens its panels; Ctrl+Space and F11 from the
keyboard; add and replace from the tab controls equal the agent op; UI only hides from View, palette and Create and Full
brings them back; the window state, level and asset documents come back for the same project).
`cargo test -p regia_editor --lib workspaces feature_profiles window_state`.
