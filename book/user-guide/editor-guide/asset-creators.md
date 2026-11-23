# Asset creators

Several places in the editor create something new. None of them is a dock tab of its own; each is a
menu or a small form, and each calls the same code as its agent method, so a person and an agent make
the same files. This page lists them: the Content browser's **New** menu (assets), the **Create** menu
(entities in the level), **File ▸ New script…**, and the project browser's template picker.
Depth: [Editor creators](../../agents/editor-creators.md), [Create menu and viewport
helpers](../../agents/create-menu.md).

## New assets: the Content browser's New menu

The **Content** tab has a **New** button next to **Import…**. Each entry creates a file in the folder
the browser is showing, under the first free name `<kind>-<n>.<extension>`, with the kind in snake case (`material-1.material.ron`,
then `material-2.material.ron`; `material_graph-1.matgraph.ron`), selects it and shows `Created <path>` on the toolbar. With no
project open, the toast says `no project is open`.

| New entry | File | Notes |
|---|---|---|
| Material | `.material.ron` | |
| Prefab | `.prefab.ron` | |
| Level | `.level.ron` | |
| Anim graph | `.animgraph.ron` | |
| Sound cue | `.cue.ron` | |
| UI layout | `.ui.ron` | |
| Data asset ▸ *type* | `.data.ron` | A submenu of the registered data asset types; the new file opens in the Data Asset panel (the `data-asset.md` page of this guide). With no registered type the submenu says so. |
| Material graph | `.matgraph.ron` | |
| Material function | `.matfunc.ron` | |
| Sequence | `.seq.ron` | |
| Shot list | `.shots.ron` | |
| Attribute set | `.attrs.ron` | |
| Effect | `.effect.ron` | |
| Ability | `.ability.ron` | |
| Material instance ▸ *graph* | `.matinst.ron` | A submenu of the project's `.matgraph.ron` files: the instance has that graph as its parent and no overrides. With no material graph the submenu says so. |
| Folder | none | A folder under `assets/`. |

An agent makes the same files at a path it chooses with `editor.create_asset`:

```json
{"method": "editor.create_asset", "params": {"kind": "sound_cue", "path": "assets/audio/door.cue.ron"}}
```

`kind` is the entry's name in snake case (`anim_graph`, `ui_layout`, `data_asset`, `material_graph`,
`shot_list`, `material_instance`, `folder`, ...). `data_asset` also needs `type`, and `material_instance`
needs `parent`. The path must end with the kind's extension; a path that escapes `assets/`, names a file
that already exists, or omits the extension is refused and nothing is written. The agent page
[Editor creators](../../agents/editor-creators.md) lists what each new file holds and the error codes of
the data asset methods.

## Create menu: entities in the level

**Create ▸ Shape ▸** adds a graybox primitive. Below it are **Player start**, **Trigger volume**,
**Point light**, **Spot light**, **Directional light**, **Camera** and **Reflection capture**. Each adds
one entity at the viewport's focus point, selects it and records one undo step. The agent method is
`editor.create_entity` with the kind (`player_start`, `trigger_volume`, `point_light`, `cuboid`, ...) and an
optional `translation`.

## New script wizard

**File ▸ New script…** opens a small **New script** window: a **Name** field, a **Template** choice
(`empty` or `component`) and **Create**. The window says where the file goes
(`game/src/scripts/<name>.rs`, registered at the `REGIA:SCRIPTS` marker). **Create** is disabled until the
name is valid and a project is open. After a successful Create the name field clears and the window shows
`created <path>`; **Close** hides the window.

| Action | Control | Agent method |
|---|---|---|
| Open the wizard | **File ▸ New script…** | `editor.script_wizard` with `open: true` |
| Create the script | **Create** | `editor.create_script` with `name`, `template` |

## Project browser: template picker

Launching the editor without a project shows the project browser. Under **New project**, type a
**Name**, pick a **Folder** (**Browse…**), choose a **Template** (**Blank**, **First Person**, **Third
Person**, **Top Down**) and press **Create project**. **Open folder…** opens an existing project, and
**Recent** lists projects opened before. The agent method is `editor.new_project` with `name`, `path`
and `template` (`blank`, `first_person`, `third_person`, `top_down`).

## Walk-through: a data asset

1. Open the **Content** tab and the folder that should hold it.
2. Press **New ▸ Data asset** and pick a type from the submenu.
3. The file is created as `data_asset-1.data.ron` (or the next free number) and opens in the Data Asset
   panel; edit its fields there and press **Save**.

## Actions

| Action | Control / key | Agent method |
|---|---|---|
| New asset in the shown folder | **Content ▸ New ▸ *kind*** | `editor.create_asset` |
| List creatable data asset types | the **Data asset** submenu | `editor.data_asset_types` |
| Add an entity at the focus point | **Create ▸ ...** | `editor.create_entity` |
| New script | **File ▸ New script…** | `editor.script_wizard`, `editor.create_script` |
| New project from a template | the project browser | `editor.new_project` |

None of these has a keyboard shortcut of its own.

## Known limits

- The New menu and the wizards are desktop-editor features; they need an open project.
- A data asset type appears in the New menu only when it is registered as a data asset.
- The project browser offers four templates; the `regia new` command has more (`quick`, `net_game`), which
  need command-line options.

## See also

[Content browser](content-browser.md), [Project settings](project-settings.md),
[Editor creators](../../agents/editor-creators.md), [Create menu and viewport
helpers](../../agents/create-menu.md).
