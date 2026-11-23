# Editor tour

`regia-editor` is a docking editor built on the engine's own ECS: the panels edit the same
world data your game runs, and every edit goes through one undo stack.
[Editor](../user-guide/editor.md) is the full chapter. The
[agent guide's editor page](../agents/editor.md) lists the `editor.*` methods that drive every
panel action from a script or an AI agent.

## Opening a project

```console
$ regia-editor --project my_game
```

Without `--project` the editor starts on the **project browser**: recent projects, a Blank
quick-create, and Open folder. With `--project` it opens the folder that holds `Regia.toml` and
loads its `startup_level`. `--game <crate>` names the game crate to host for code hot reload
([Hot reload](../user-guide/hot-reload.md)).

## The window

The default layout docks these panels; drag a tab to rearrange them.

| Panel | What it shows |
|---|---|
| **Viewport** | The 3D scene: procedural sky, your entities, a reference grid, the transform gizmo and selection labels. |
| **Outliner** | The entity hierarchy of the open level. Drag an entity onto another to reparent it. |
| **Inspector** | The reflected components of the selection. Every registered `Reflect` field is editable, with ranges, units and tooltips from its editor attributes ([Reflection](../fundamentals/reflection.md)). |
| **Content Browser** | The project's `assets/` folder: search, Import…, New (material, prefab, level, folder), and per-asset Copy path, Show in file manager, Reimport, Delete (which warns about files that still reference the asset). Double-click a level to open it, a material to edit it, a prefab to enter prefab mode; drag a mesh or a prefab into the viewport to place it. |
| **Output Log** | The engine log, filterable. |
| **Material** | The `.material.ron` editor: base color, metallic, roughness and the alpha mode, saved to the file. |
| **Project Settings** | `Regia.toml`'s general fields: name, id, version, build number, startup level. |

The menu bar has **File** (New level, Open level…, Open kit…, Save level, Save level as…, Create
entity), **Create** (Shape), **Edit** (Undo and Redo, labeled with the action they revert;
Preferences…, Project settings…, World settings…) and **View** (grid, selection labels, Frame
selection, and saving or resetting the layout).

## Moving around the viewport

| Input | Action |
|---|---|
| Right mouse button + `W` `A` `S` `D` | Fly forward, left, back, right |
| Right mouse button + `E` / `Q` | Fly up / down |
| `Alt` + left drag | Orbit |
| Middle drag | Pan |
| Left drag on empty space | Box select |
| `Alt` + right drag, or the mouse wheel | Zoom |
| Left click | Select (picking) |
| `W` / `E` / `R` | Gizmo: translate / rotate / scale |
| `F` | Frame the selection |
| `G` | Toggle game view (hides every editor helper) |

The editor viewport renders with a stable editing profile: native resolution, no temporal jitter,
so a still frame stays still. Your game's render profile applies in Play.

## Editing

- Select an entity in the Outliner or the viewport; its components appear in the Inspector.
- Drag the gizmo, or type values into the Inspector. Each drag or edit is one undo entry.
- File → Create entity adds an empty entity; add components from the Inspector.
- Prefabs: double-click a prefab in the Content Browser (or `editor.open_prefab`) to edit it in
  prefab mode; Save & Close re-bakes the template and updates every live instance.
- File → Save level writes canonical `.level.ron`: re-saving an unchanged level is byte-identical,
  so diffs show only what you changed.

While a level has unsaved changes the editor autosaves it every five minutes to
`.regia/autosave/<stem>.level.ron`. If the editor closes without saving, the next launch offers
to restore it.

## Play in editor

The toolbar's **▶ Play** copies the edit world into a separate play world and runs it; Pause and
Step (one frame) let you inspect a moment; Stop drops the play world. Play never writes to your
edit world, so nothing you do while playing changes the level. When a game crate is open, the play
world runs your hot-reloaded game plugin; otherwise it runs the engine's built-in systems. If the
game panics during play, the status bar shows the panic message and the editor keeps running.

## Scripts from the editor

`editor.create_script` (and the matching editor action) writes `game/src/scripts/<name>.rs` and
registers it at the `// REGIA:SCRIPTS` marker in `game/src/lib.rs`. The next play or hot reload
picks it up. Scripts are covered in [Gameplay framework](../user-guide/gameplay.md).

## The editor is agent-controllable

The editor runs the same agent-protocol server as a game, extended with `editor.*` methods.
Selection, gizmo mode, transforms, undo and redo, saving, play-in-editor, imports, asset creation,
prefab editing and project creation are all calls. Agent edits and your edits share the undo
stack, so you can undo what an agent did. See [Agent protocol and MCP](../user-guide/agent-protocol.md).
