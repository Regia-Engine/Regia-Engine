# Editor extensions: panels, commands, settings, viewport tools and hooks (`editor.extension_*`)

ADR-9850, ADR-7791. Code: `regia_editor::extension::{actions, context, settings, viewport_tools, hooks}`. Spec: `docs/specs/57-plugin-extension-api.md` § 3.3
item 3, § 5.1. Enabling, granting and reloading extensions is on the [Extensions](plugins.md) page.

An editor extension (a `[[code]]` entry with `audience = ["editor"]`) registers its editor UI through
`regia_editor::extension::ToolExtensionContext`, which wraps the `ExtensionContext` its `build` receives:

| Call | Adds | Registration kind |
|---|---|---|
| `register_editor_panel(PanelDesc { id, title_key, dock_region, draw })` | a dockable tab | `editor_panel` |
| `register_editor_action(ActionDesc { id, title, menu, toolbar, run })` | an entry in the menu bar's **Extensions** menu (in the submenu `menu`, or at the top level when empty) and, with `toolbar: true`, a button on the play toolbar | `editor_action` |
| `register_asset_tool(AssetToolDesc { id, title, suffixes, run })` | a tool for project assets whose file name ends in one of `suffixes` (case-insensitive), in the content browser's context menu of such a file | `asset_tool` |
| `register_inspector(InspectorCustomization { .. })` | a widget for one reflected type | `inspector` |
| `register_viewport_tool(ViewportToolDesc { id, title, icon, handles, drag })` | an entry in the viewport's **Tools** menu; while active, its handles in every viewport pane, each drag one undo step | `viewport_tool` |
| `register_overlay(OverlayDesc { id, title, corner, draw })` | a small panel over each viewport pane, in a corner; shown or hidden from the **Tools** menu | `overlay` |
| `register_setting(SettingDesc { key, title, description, scope, default, min, max, choices })` | a typed setting under the extension's name in **Preferences** (user scope) or **Project Settings** (project scope) | `editor_setting` |
| `register_build_hook(BuildHookDesc { id, title, stage, run })` | a before-cook, after-package or before-launch hook | `build_hook` |
| `register_workspace(WorkspaceDesc { id, title, layout, mode })` | a task workspace in the switcher with an extension mark (ADR-8197) | `editor_workspace` |

Every id is `<extension id>.<name>`. All of them need the `editor_panel` capability: request it in
`regia-plugin.toml` and grant it in the project's `Regia.toml` (`[extensions.grants] my_ext = ["editor_panel"]`). The
registrations are owned by the extension: `plugin.info` lists them by kind, and a reload or a disable removes them.
An action's `run` returns `Ok(status line)` or `Err(message)`; an asset tool's `run` gets the project-relative path and
the file and returns `Ok(json)` or `Err(message)`. A panic in either is caught and reported as the `*_failed` code; the
editor keeps running. `editor.command { run: "<action id>" }` (and the command palette) runs an action as well.

## Commands and shortcuts (ADR-7791)

An action is also a command. `ActionDesc::new(id, title, run)` fills the rest with defaults; set the others with struct
update syntax:

<!-- ignore-reason: a fragment of an extension's `build`; the compiled examples are extensions/hello_panel/src/hello.rs and crates/regia_editor/tests/extension_points.rs -->
```rust,ignore
editor.register_editor_action(ActionDesc {
    category: "Terrain".into(),        // the palette's group; empty: the extension's display name
    shortcut: "Ctrl+Alt+G".into(),     // a default; the user rebinds it in Preferences ▸ Keymap
    places: vec![ActionPlace::Outliner],
    enabled_when: Some(SelectionPredicate { components: vec!["Name".into()], min_count: 1, ..Default::default() }),
    ..ActionDesc::new("my_ext.mark", "Mark", Arc::new(mark))
})?;
```

- The command palette lists it as `category: title` with its shortcut; `editor.command { run }` runs it by id.
- The shortcut uses the Keymap's spelling (`Ctrl` is Cmd on macOS). A default that a built-in command (its built-in or
  user shortcut), a fixed editor key (Ctrl+Y, Ctrl+Shift+P) or an earlier extension action (by id) already has is a
  **conflict**: it never runs, and `editor.extension_ui` lists it under `shortcut_conflicts`. A shortcut that does not
  parse refuses the registration (`manifest_invalid`).
- **Preferences ▸ Keymap ▸ Extension commands** lists every extension command with a chord field (type `Ctrl+Alt+H`),
  **Set**, **Clear** and **Default**, and its conflict. Binding a taken chord is refused. User chords are saved in
  `editor_keymap.ron` with the built-in ones (a cleared one as `""`); the file already keeps ids it does not know, so
  this is no format change.

## Selection predicates and context menus (ADR-7791)

`places` adds the action to the **Outliner** row menu, the viewport's **Tools** menu (`Viewport`) or a **Content
Browser** asset's menu. `visible_when` hides the entry and `enabled_when` greys it out unless a `SelectionPredicate`
holds; every part must hold and an empty part holds:

| Field | Holds when |
|---|---|
| `kinds` | the selection holds only these kinds (`Entity`, `Asset`) |
| `components` | every selected entity has these components (short type name, case-insensitive, as the Outliner's `type:` search) |
| `asset_suffixes` | every selected asset ends with one of these (case-insensitive) |
| `min_count`, `max_count` | the number of selected entities plus assets is in range |

The Extensions menu, the toolbar, the palette, shortcuts, the Outliner and the viewport use the level selection; a
Content Browser entry uses the asset it was opened on. Before it runs an action the editor inserts that selection as the
`MenuSelection` resource, so the callback reads what it acts on. An action that does not apply answers
`extension_action_disabled`.

## Settings (ADR-7791)

A `SettingDesc`'s `default` (`PrefValue::Bool`, `Int`, `Float` or `Text`) is its type; `min`/`max` bound numbers and
`choices` limits text. The setting shows under the extension's display name with a widget for its type and a **Reset**
button: user scope in **Preferences** (searchable with the built-in preferences), project scope in **Project Settings**.
Read it with `regia_editor::extension::settings::get(world, key)`. A user value is kept in `editor_prefs.ron` as
`extension.<key>`; a project value is written to `Regia.toml` as `[extensions.settings.<ext>] <name> = value`, the table
`ExtensionContext::settings::<T>()` reads at build time.

## Viewport tools and overlays (ADR-7791)

A tool's `handles(&World)` returns the handles to show (`regia_editor::handles::Handle`: points, arrows, dials), drawn and
dragged by the same handle code as the collider grips. Its `drag(&mut World, &ToolDrag)` gets `Begin`, `Drag` per move
and `End` with the handle index, position, step and total movement; it previews on the world as the drag goes and, on
`End`, answers the `EditorCommand` of the whole drag (already applied). The editor pushes that command on the undo
stack without applying it again: one drag is one undo step. The viewport offers a press to the tool only after the
gizmo and the built-in grips (one owner per press, ADR-7674). An overlay's `draw` is a panel callback drawn in a frame
in its `corner` of each viewport pane (not in the game view).

## Build and launch hooks (ADR-7791)

A hook's `run(&HookInput, &mut HookOutput)` gets the stage, project, target, profile and (after a package) the output
folder. It may push log lines, push files (`(relative path, bytes)`: project-relative before the cook and the launch,
output-relative after a package; a path leaving the folder aborts) and return `Err(message)` to abort. Hooks of a stage
run in id order; the first abort stops the stage; a panic aborts with a message.

| Stage | Runs | An abort |
|---|---|---|
| `before_cook` | when File ▸ Package for platform (or `editor.package_start`) starts, before the CLI cooks | refuses the run: `package_hook_aborted`; the log lines open the job's log |
| `after_package` | once, when the editor sees a successful package run finished (not a dry run) | turns its result into `ok: false, code: package_hook_aborted` (Package dialog, `editor.package_status`) |
| `before_launch` | when `editor.launch` (or the Launch bar) starts | refuses: `launch_hook_aborted`, the hook's lines in `log_tail` |

## Where extensions are found

`<project>/extensions/` (and the project's workspace members and `extension_paths`) first, then the engine's
`<engine>/extensions/`. A project extension with the id of an engine extension **overrides** it: `plugin.list` shows
only the project copy, and `plugin.info` of that id lists the info diagnostic `extension_overridden` naming both paths.
The editor finds the engine directory in `REGIA_ENGINE_DIR`, else uses the checkout around its executable (a
`target/<profile>/` build inside a checkout; debug builds also fall back to the checkout they were compiled in, ADR-10544).

The engine ships one example, `extensions/hello_panel` (a panel, an action with a toolbar button, and a line-count tool
for `.ron` files). It is not enabled by default:

```toml
[extensions]
enabled = ["hello_panel"]
[extensions.grants]
hello_panel = ["editor_panel"]
```

## `editor.extension_ui`

Read-only, no params. What active extensions add to the editor:

```json
{ "panels": [{ "id": "hello_panel.greeting", "title": "Hello", "dock": "bottom", "owner": "hello_panel" }],
  "actions": [{ "id": "hello_panel.say_hello", "title": "Say hello", "menu": "Hello", "toolbar": true, "owner": "hello_panel" }],
  "asset_tools": [{ "id": "hello_panel.line_count", "title": "Count lines", "suffixes": [".ron"], "owner": "hello_panel" }] }
```

`dock` is `left`, `right`, `bottom` or `center`. Each action also has `category`, `shortcut` (now, user or default),
`default_shortcut`, `places` (`outliner`, `viewport`, `content_browser`), `visible` and `enabled` for the level
selection, and its `visible_when`/`enabled_when` predicates (`{ kinds, components, asset_suffixes, min_count,
max_count }` or `null`). The answer also has `shortcut_conflicts: [{ id, keys, with }]` and
`hooks: [{ id, title, stage, owner }]` (`stage`: `before_cook`, `after_package`, `before_launch`).

## `editor.extension_action`

Mutating (`edit_scene`). `{ id }` runs the action exactly as the menu entry or toolbar button does, on the level
selection, and returns `{ id, owner, message }`. `{ id, assets: ["assets/a.ron"] }` runs it as its Content Browser entry
does, on those assets.

## `editor.extension_shortcut`

Mutating (`edit_scene`). `{ id, keys: "Ctrl+Alt+H" }` binds, `{ id, clear: true }` removes, `{ id, reset: true }` goes
back to the default, as the Keymap tab's rows do. Returns `{ id, shortcut, saved, conflicts }`; `saved` says whether the
keymap file was written (the editor binary has one; a headless editor keeps the binding for the session).

## `editor.extension_settings`

Mutating (`edit_scene`). No params lists `{ settings: [{ key, title, description, scope, type, value, default,
is_default, min, max, choices, owner, extension }] }` (`type`: `bool`, `int`, `float`, `text`; `extension` is the display
name). `{ key }` answers that row; `{ key, value }` sets it (a whole number is taken for a number setting); `{ key,
reset: true }` returns it to its default.

## `editor.extension_tools`

Mutating (`edit_scene`). Answers `{ tools: [{ id, title, icon, owner, active }], overlays: [{ id, title, corner, owner,
shown }], active, handles }` (`handles`: the active tool's handle positions, or `null`). Params, applied in this order:

- `activate`: a tool id makes it active (as its Tools menu entry does); `""` or `null` for none.
- `overlay` with `shown`: show or hide an overlay.
- `drag: { handle, to: [x, y, z], steps? }`: drags a handle of the active tool from where it is to `to` in `steps` moves
  (default 4), through the same `Begin`/`Drag`/`End` calls the pointer makes; adds `undo_entry` (whether the drag pushed
  one undo step).
- `overlay_text`: draws that overlay off screen and adds `text` (its text runs in paint order).

## `editor.extension_asset_tool`

Mutating (`edit_scene`). `{ id, path }`, `path` project-relative (`assets/data/table.ron`). Returns
`{ id, path, owner, result }`, `result` being the tool's JSON.

## `editor.extension_panel`

Read-only. `{ id }` draws the panel once off screen, with the callback its dock tab runs, and returns
`{ id, title, dock, owner, text }`: `text` is every text run the panel painted, in paint order. Use it to check what a
panel shows without a window; take a screenshot of the docked tab for the look.

## Task flow

1. `plugin.list`, then `plugin.info { id }`: is the extension `active`, and which registrations does it have?
2. `editor.extension_ui`: the panel, action and tool ids.
3. `editor.extension_panel { id }` before and after `editor.extension_action { id }` to see the action's effect.
4. `editor.extension_asset_tool { id, path }` on an asset of a type the tool lists.
5. `editor.extension_tools { activate }`, then `{ drag }`, then `editor.undo` to check a tool's edit is one step.
6. `editor.extension_settings {}` for the settings; `{ key, value }` to change one, then read what the extension does
   with it.

## Error codes

| Code | When |
|---|---|
| `extension_action_not_found` | no active extension registered that action id |
| `extension_action_failed` | the action returned an error or panicked (the message says which) |
| `asset_tool_not_found` | no active extension registered that tool id |
| `asset_tool_wrong_type` | the file's name ends in none of the tool's suffixes |
| `asset_tool_failed` | the tool returned an error or panicked |
| `bad_path` | `path` is absolute, contains `..`, or is not a file of the project |
| `no_project` | the editor has no project open |
| `extension_panel_not_found` | no active extension registered that panel id |
| `extension_action_disabled` | the action's predicates do not hold for the selection it would run on |
| `extension_shortcut_bad_chord` | `keys` is not a chord (`Ctrl+Alt+G`) |
| `extension_shortcut_reserved` | `keys` is Ctrl+Shift+P (the command palette) |
| `extension_shortcut_conflict` | a built-in command, a fixed key or another extension action has the chord |
| `extension_setting_not_found` | no active extension declared that setting key |
| `extension_setting_invalid` | the value is of the wrong type, out of range, or not one of the choices |
| `extension_setting_write_failed` | `Regia.toml` could not be read, parsed or written |
| `extension_tool_not_found` | no active extension registered that viewport tool |
| `extension_tool_inactive` | `drag` with no active extension tool |
| `extension_tool_handle_unknown` | `drag.handle` is past the active tool's handles |
| `extension_overlay_not_found` | no active extension registered that overlay |
| `F0002` | a required string parameter is missing |


## Workspaces (ADR-8197)

`regia_editor::extension::context::WorkspaceDesc` contains an owned `id`, display `title`,
an existing `editor_layout::LayoutDoc`, and optional `workspaces::EditorMode`. Registration
validates the layout and nonempty title before claiming the id. It needs `editor_panel`,
uses the extension's id prefix, and participates in disable and failed-reload restoration.
A workspace without a mode selects World.

`editor.workspace_list` lists it as `extension:<id>` with `extension` naming its owner and
`mode` naming its effective mode. `editor.workspace_set { id, mode? }` applies the registered
layout and mode; a parked layout returns on later visits. Disabled registrations disappear
from the switcher and cannot be applied, even if a dock is parked. A currently shown layout
remains usable after disable; its extension panels follow the existing removal behavior.
Errors use the existing extension codes: `capability_not_granted`, `registration_conflict`,
`manifest_invalid`, and `not_supported`. Agent errors and session persistence are documented
on [Editor layouts](editor-layouts.md).

Workspace registration rejects an empty title or invalid/newer layout with `manifest_invalid`;
missing `editor_panel` grants use `capability_not_granted`, and foreign or taken registration
ids use `registration_conflict`. No layout file is written during registration.
