# Editor guide

One page per area of the editor: what it is for, its main actions with their keyboard shortcuts,
and the agent method that does each action, so a person and an agent can do the same work. The
shortcuts on these pages are the ones the editor's code handles (the command table in
`crates/regia_editor/src/command_palette.rs` and the key handling of each panel); an action
listed with no key has none. Hands-on walk-throughs are the tutorials
[Your first scene in the editor](../../tutorials/editor-first-scene.md),
[Your first material graph](../../tutorials/editor-first-material.md) and
[Package a build](../../tutorials/package-a-build.md).

The editor overview (launching, levels, hot reload) is [Editor](../editor.md); the editor's
whole agent surface is [Editor surface](../../agents/editor.md); keyboard reachability, focus and
contrast are reviewed in [Editor accessibility](accessibility.md).

## The window

The menu bar has **File**, **Create**, **Edit** and **View**. Below it, the play toolbar
([Play-in-editor](play-in-editor.md)), then the dock: every panel is a tab. The default layout
puts the **Outliner** left, the **Viewport** in the middle, the **Inspector** right, and
**Content**, **Output Log**, **Project Settings** and the other tool tabs in the group under the
viewport. Drag a tab to move it, out of the window to float it; **View ▸ Reset layout** restores
the default and brings back a closed tab.

An agent brings a tab forward (adding it when it is closed) with
`editor.layout_apply {"open": "<id>"}`; the ids are the tab names in snake case
(`viewport`, `outliner`, `inspector`, `content`, `material_graph`, `seq_curves`,
`ui_designer`, `project_settings`, `fracture`, `proc_graph`, ...). `editor.layout_get`,
`editor.layout_save` and `editor.layout_reset` read, save and reset the arrangement
([Editor preferences, keymap, layout](../../agents/editor-preferences.md)). Named layouts,
workspaces, maximize (Ctrl+Space), the full-screen viewport (F11), the `+` and Replace with
pane controls and feature profiles are on [Layouts, workspaces and feature profiles](layouts-workspaces.md).

## Keys that work everywhere

| Key | Action | Agent method |
|---|---|---|
| Ctrl+Z | Undo the last edit (not while a text field has focus: the field keeps its own undo; not during play) | `editor.undo` |
| Ctrl+Shift+Z, Ctrl+Y | Redo | `editor.redo` |
| Ctrl+Shift+P | Open the command palette: type to search every command, Enter runs the highlighted one | `editor.command` |
| F2 (in the palette) | Record a new shortcut for the highlighted command | `editor.keymap_bind` |
| Alt+P | Play in the editor (when no session runs) | `editor.play` |
| Escape | Stop play when a session runs; otherwise ask to close the editor. A panel that uses Escape (a text field, a menu) gets it first | `editor.stop`, `quit.request` |
| Ctrl+Alt+F11 | Compile the game crate and hot-reload it | `editor.code_status` reports the build |
| Ctrl+= or Ctrl+Plus, Ctrl+-, Ctrl+Alt+0 | Zoom the whole editor interface in, out, or back to 100 % (**View ▸ Reset zoom**); the zoom is the **UI scale** preference, kept between sessions; rebindable (`view.zoom_in`, `view.zoom_out`, `view.reset_zoom`, ADR-7733) | `editor.command` `run: "view.zoom_in"`, `editor.prefs_set` `appearance.ui_scale` |
| Ctrl+0 | In the viewport, stores camera bookmark 0; it no longer resets the interface zoom (Ctrl+Alt+0 does; [finding E-10, FU-4](accessibility.md)) | `editor.bookmark` |

Ctrl is Cmd on macOS. Every palette command can be given a shortcut in **Edit ▸ Preferences… ▸
Keymap** or with F2 in the palette; the bindings are saved per user in `editor_keymap.ron`
([Editor command palette](../../agents/editor-command.md)). Ctrl+Shift+P cannot be rebound.

Every panel of this guide can be brought forward from the keyboard: Ctrl+Shift+P, type `open`,
pick **Open the … tab** (commands `window.open_viewport`, `window.open_outliner`, ...,
[Editor command palette](../../agents/editor-command.md)); bind one in the Keymap tab to get a
direct key.

## Which panel gets a key

Keys go to the panel under the pointer or with keyboard focus, and only when no text field has
focus: typing a `W` into the Inspector's name field never switches the gizmo. The viewport's keys
work in the active pane (the last one clicked); the Outliner's Delete, Ctrl+D and Ctrl+G work
while the pointer is over the Outliner. Tab and Shift+Tab move keyboard focus between the
widgets of a panel, Space or Enter presses the focused button, and the arrow keys move in a
focused slider or list.

## Areas

| Area | Tab | Page |
|---|---|---|
| Viewport | Viewport | [Viewport](viewport.md) |
| Outliner | Outliner | [Outliner](outliner.md) |
| Inspector | Inspector | [Inspector](inspector.md) |
| Content browser | Content | [Content browser](content-browser.md) |
| Material graph | Material Graph, Material | [Material graph](material-graph.md) |
| Sequencer | Sequence Curves, Shots | [Sequencer](sequencer.md) |
| UI designer | UI Designer, UI Animation | [UI designer](ui-designer.md) |
| Profiler | View ▸ Profiler, View ▸ Diagnostics (windows) | [Profiler](profiler.md) |
| Play-in-editor | the play toolbar, Net | [Play-in-editor](play-in-editor.md) |
| Project settings | Project Settings, Preferences | [Project settings](project-settings.md) |
| Fracture | Fracture | [Fracture](fracture.md) |
| Procedural graph | Procedural Graph | [Procedural graph](procgen.md) |
| Logic graph | Logic Graph | [Logic graph](logic-graph.md) |
| Terrain | Terrain | [Terrain](terrain.md) |
| Vehicle tuning | Vehicle Tuning | [Vehicle tuning](vehicle-tuning.md) |

## Every panel

Every tab of the dock has a row here, and `cargo xtask book-check` fails when a panel has none (the `Panel` enum in `crates/regia_editor/src/shell.rs` is the list). A panel that shares a page with others is documented in its own section of that page.

| Panel variant | Tab | Page |
|---|---|---|
| `Viewport` | Viewport | [Viewport](viewport.md) |
| `Outliner` | Outliner | [Outliner](outliner.md) |
| `Inspector` | Inspector | [Inspector](inspector.md) |
| `Content` | Content | [Content browser](content-browser.md) |
| `OutputLog` | Output Log | [Output log](output-log.md) |
| `Material` | Material | [Material graph](material-graph.md) |
| `MaterialGraph` | Material Graph | [Material graph](material-graph.md) |
| `ProjectSettings` | Project Settings | [Project settings](project-settings.md) |
| `Preferences` | Preferences | [Project settings](project-settings.md) |
| `WorldSettings` | World Settings | [World settings](world-settings.md) |
| `AssetViewer` | Asset Viewer | [Asset viewer](asset-viewer.md) |
| `AnimGraph` | Anim Graph | [Animation panels](anim-graph.md) |
| `AnimClip` | Clip | [Animation panels](anim-graph.md) |
| `AnimSkeleton` | Skeleton | [Animation panels](anim-graph.md) |
| `Montage` | Montage | [Montage](montage.md) |
| `Ragdoll` | Ragdoll | [Ragdoll](ragdoll.md) |
| `Cloth` | Cloth | [Cloth](cloth.md) |
| `Kit` | Kit | [Kit](kit.md) |
| `Bake` | Bake | [Baked lighting panels](bake.md) |
| `BakeRunner` | Bake runner | [Baked lighting panels](bake.md) |
| `Lightmaps` | Lightmaps | [Baked lighting panels](bake.md) |
| `LookDev` | Look-dev | [Look-dev](lookdev.md) |
| `Theme` | Theme | [Theme](theme.md) |
| `Shots` | Shots | [Sequencer](sequencer.md) |
| `SeqCurves` | Sequence Curves | [Sequencer](sequencer.md) |
| `Net` | Net | [Play-in-editor](play-in-editor.md) |
| `Tool` | Tools | [Tools](tools.md) |
| `DccFeed` | DCC Feed | [DCC Feed](dcc-feed.md) |
| `ProcGraph` | Procedural Graph | [Procedural graph](procgen.md) |
| `ContentChunks` | Content Chunks | [Content chunks](content-chunks.md) |
| `Extensions` | Extensions | [Extensions](extensions.md) |
| `ExtensionPanel` | the title an extension gives it | [Extensions](extensions.md) |
| `UiDesigner` | UI Designer | [UI designer](ui-designer.md) |
| `UiAnim` | UI Animation | [UI designer](ui-designer.md) |
| `Fracture` | Fracture | [Fracture](fracture.md) |
| `Tables` | Tables | [Tables](tables.md) |
| `Abilities` | Abilities | [Abilities](abilities.md) |
| `Problems` | Problems | [Problems](problems.md) |
| `ReferenceGraph` | Reference Graph | [Reference Graph](reference-graph.md) |
| `SizeMap` | Size Map | [Size Map](size-map.md) |
| `Layers` | Layers | [Layers](layers.md) |
| `Tests` | Tests | [Tests](tests.md) |
| `StateTrees` | State Trees | [State trees](state-trees.md) |
| `StateTreeEditor` | State Tree Editor | [State trees](state-trees.md) |
| `AudioMixer` | Audio Mixer | [Audio Mixer](audio-mixer.md) |
| `CueEditor` | Cue Editor | [Cue Editor](cue-editor.md) |
| `AgentChanges` | Agent Changes | [Agent Changes](agent-changes.md) |
| `BehaviorTree` | Behavior Tree | [Behavior tree](behavior-tree.md) |
| `UiInspector` | UI Inspector | [UI Inspector](ui-inspector.md) |
| `VfxEditor` | VFX Editor | [VFX Editor](vfx-editor.md) |
| `Video` | Video | [Video](video.md) |
| `Conversation` | Conversation | [Conversation](conversation.md) |
| `Localization` | Localization | [Localization](localization.md) |
| `DataAsset` | Data Asset | [Data Asset](data-asset.md) |
| `VehicleTuning` | Vehicle Tuning | [Vehicle tuning](vehicle-tuning.md) |
| `Terrain` | Terrain | [Terrain](terrain.md) |
| `LogicGraph` | Logic Graph | [Logic graph](logic-graph.md) |
| `BulkEdit` | Bulk Edit | [Bulk Edit](bulk-edit.md) |
| `GameView` | Game: server, Game: client k (one tab per separate PIE process) | [Play-in-editor](play-in-editor.md#game-tabs) |
| `UndoHistory` | Undo History | [Undo History](undo-history.md) |
| `Notes` | Notes | [Notes and Activity](notes-activity.md#notes) |
| `Activity` | Activity | [Notes and Activity](notes-activity.md#activity) |
| `SelectedDebug` | Selected Object Debug | [Selected Object Debug and Visual Log](debug-visual-log.md#selected-object-debug) |
| `VisualLog` | Visual Log | [Selected Object Debug and Visual Log](debug-visual-log.md#visual-log) |
| `ProjectHealth` | Project Health | [Project Health](project-health.md) |
| `WorldMap` | World Map | [World Map](world-map.md) |
| `Agent` | Agent | [Agent](agent-panel.md) |
| `Retarget` | Retarget | [Retarget](retarget.md) |
| `Biome` | Biome | [Biome](biome.md) |

### Panels whose page is still to come

These panels have no page of their own yet. Each row says why; a page replaces the row when it lands.

| Panel variant | Why it has no page of its own |
|---|---|
| `Cameras` | The camera rig editor is in, but its guide page is not written yet; [Gameplay panels (agents)](../../agents/editor-gameplay-panels.md) covers it |
| `SmartObjects` | The guide page is not written yet; [Smart objects (agents)](../../agents/smart-objects.md) covers the runtime this tab inspects |
| `SmartObjectDef` | The definition editor is in, but its guide page is not written yet; [Smart object editor (agents)](../../agents/smart-object-editor.md) covers it |
| `MotionMatching` | The motion database editor window is in (ADR-7740), but the guide page is not written yet; [Motion database editor (agents)](../../agents/motion-database-editor.md) covers it |
| `Mode2d` | The 2D tab and its rule editor are in, but the guide page is not written yet; [2D editing (agents)](../../agents/editor-2d.md) covers them |
| `Instance` | A second or later copy of the Outliner, Inspector or Content Browser (Window ▸ New Inspector and the palette commands); the guide page is not written yet; [Editor layouts (agents)](../../agents/editor-layouts.md) covers `editor.panel_new_instance` |
| `UtilityPanel` | A panel a project builds from a `.ui.ron` document and a driver (Window ▸ Utility Panels); the guide page is not written yet; [Utility panels (agents)](../../agents/utility-panels.md) covers it |
| `MatchRules` | The Match Rules panel (rule sets, playlists and the World Settings Game Rules picker, ADR-8738) is in, but its guide page is not written yet; [Editor match rules (agents)](../../agents/editor-match-rules.md) covers it |

### Features with no tab of their own

Four pages cover editor features that are not a tab: [Play window](play-window.md) (the separate play
window), [Asset creators](asset-creators.md) (the Create menu's asset creators),
[Autosave and recovery](autosave-recovery.md) and [Panel timings](panel-timings.md) (the dock's
draw-time rings and the long lists of large projects).

## Coming in v0.6.0

Everything below is **planned** unless it says **built**: a built item has landed on the v0.6 line and links its
page. The plan is ADR-7752 and the
"As planned" sections of the specs for the editor, the UI designer, the runtime UI and localization. Each feature will
have an agent method, so an agent can do whatever a person can, and each is reachable from the keyboard.

Shell, search and settings:

- Named layouts and workspaces (Level, Animation, Materials, UI, World, Cinematics, Debug): **built**
  ([Layouts, workspaces and feature profiles](layouts-workspaces.md)).
- Maximize a panel or the viewport; add or replace a panel in any pane: planned.
- Panels as their own operating-system windows, and several Outliners, Content browsers or Inspectors: planned.
- A clickable status bar, content and log drawers, and a console line for commands and agent methods: planned.
- One search window for assets, entities, commands, menus, settings and agent methods: planned.
- Keymap presets, shortcuts that depend on the focused panel, and F1 help for the focused panel or field: planned.
- One Settings window with search, changed-value marks and reset per category: planned.

Viewport:

- More camera options: inverted axes, six orthographic views, zoom to cursor, navigation presets: planned.
- Pilot a camera, light or entity, a picture-in-picture camera preview and high-resolution screenshots: planned.
- World, local, parent and view reference spaces, more pivots and snapping to surfaces: planned.
- Lasso selection, select similar, pick from a stack under the cursor and a measure tool: planned.
- A view-mode menu (wireframe, overdraw and more), icon filters and a Rendering Debug panel: planned.

Outliner and Inspector:

- Rename in place, a query language for filtering, and folders: planned.
- Configuration warnings, badges, columns and row colors: planned.
- Property search, categories, pinned fields and an Inspector that stays on one entity: planned.
- Copy and paste values, reset to default, conditional fields, expressions with units: planned.
- Asset and entity pickers, inline curves and gradients, and a full color picker: planned.
- A Bulk Edit grid for many entities or assets at once: **built** ([Bulk Edit](bulk-edit.md)).

Content, import and problems:

- Folder tree, breadcrumbs, favorites, filter chips and collections in the Content browser: planned.
- Bulk actions, a Save all dialog, delete with "replace references with": planned.
- Drop files from the operating system, asset packages and an import dialog with presets: planned.
- Problems grouped by source and a Project Health panel: **built** ([Problems](problems.md),
  [Project Health](project-health.md)).

Prefabs, levels and graphs:

- A prefab Overrides window and in-context prefab editing: planned.
- Level variants, layer locks and several levels open at once: planned.
- Material parameters panel, shader statistics and a WGSL code node: planned.
- Logic graph search, a Variables panel, breakpoints and stepping: planned.
- A structural diff of assets and graphs against a git revision: planned.

Animation, effects and world:

- A retargeting panel: **built** ([Retarget](retarget.md)). Animation debugger and clip preview options: planned.
- Sequence event tracks, track lock, mute and solo, and a dope sheet for UI animations: planned.
- Effects overview, mixer effect chains and navigation path view: planned.
- A world streaming map: **built** ([World Map](world-map.md)). Terrain brush options, terrain edit layers, tile tools
  and CSG blockout: planned.

UI designer, theme and localization:

- Anchor presets, aspect and effect rows, and a preview that uses the project's scale rule: planned.
- Split, foldable and tab containers, markup text, focus-only styles and layout variants: planned.
- An interactive preview, a view of which style wins, and live style edits in a running game: planned.
- A theme editor with a live gallery of every widget state: planned.
- A localization compile step, translator comments, a preview-language picker and table import and export: planned.

Play, devices and debugging:

- Play start options, time scale, device frames, touch and gamepad simulation: planned.
- Separate-process play in a dock tab, remote Outliner and Inspector for a running game: **built**
  ([Play-in-editor § Game tabs](play-in-editor.md#game-tabs), [Live edit (agents)](../../agents/live-edit.md)).
- Asset push to a launched game and device mirroring: planned.
- Profiler modules and monitors, memory snapshots, a GPU resource browser and a frame debugger: planned.

Undo, revision control and collaboration:

- An Undo History panel, transactions and safe mode: **built** ([Undo History](undo-history.md); transactions and
  safe mode: [Shared undo history and recovery (agents)](../../agents/undo.md)).
- Kept versions, project backup and project duplicate: planned.
- A Revision Control panel for git, with lock on edit for projects that use git LFS locks: planned.
- Review notes pinned in the world and an Activity timeline: **built** ([Notes and Activity](notes-activity.md)).
- LAN presence between editors, off by default: planned.

Build, extensions, accessibility and agents:

- Launch profiles, a Platforms panel and a package size report: planned.
- Extension commands, settings, hooks and utility panels; a samples browser: planned.
- Editor themes with a high-contrast theme, a text scale and a color-vision filter: planned.
- A project browser with thumbnails and level templates: planned.
- An Agent panel that runs an agent command you configure, in Ask-only, Propose or Act mode: **built**
  ([Agent](agent-panel.md)).

Not planned for v0.6.0: code editing inside the editor (use your own IDE), an asset marketplace, and editing on touch
devices or in a browser. In-editor polygon modeling, posing a rig in the viewport and a node-based audio graph each need
their own spec first.
