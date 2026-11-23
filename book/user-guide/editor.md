# Editor

The editor (`regia-editor`) is a docking UI over the engine's own ECS: a **Viewport**, **Outliner**,
**Inspector**, **Content Browser** and **Output Log**, all editing the same world the game runs.

```console
$ regia-editor --project my_game
```

Without `--project` it shows the project browser (recent projects, Blank, Open folder). With
`--project`, it opens the startup level from `Regia.toml`.

## The edit world

The editor keeps an **edit world** (a minimal `App` with the agent protocol server and editor
resources). Every panel edit is a command on a shared **undo stack** (Ctrl+Z undoes, Ctrl+Shift+Z
or Ctrl+Y redoes; Cmd on macOS; one gizmo drag or slider scrub is one step); Play runs a **copy** of the
world (play-in-editor), so play never writes your edit. Autosave writes
`.regia/autosave/<stem>.level.ron` (`<stem>~<8 hex digits>.level.ron` when another level shares the stem) every five minutes while a level is dirty, and offers Restore
on the next launch.

## Panels

- **Viewport** — the scene view: a sky dome, lit proxy cubes, a grid, gizmos (translate/rotate/scale) and selection labels. Editing uses a cheap stable profile (native scale, no temporal jitter). Play shows the same camera over the play world; Stop restores the edit view.
- **Outliner** — the entity hierarchy with a name search; reparent by dragging rows (a selected row drags the
  whole selection; drop under the tree to move to the root; world poses are kept). Shift-click selects a range;
  Delete, Ctrl+D (duplicate) and Ctrl+G (group) act on the selection. See
  [Outliner hierarchy edits](../agents/hierarchy.md).
- **Inspector** — the reflected fields of the selection, with ranges/units/tooltips from editor
  attributes.
- **Content Browser** — import (glTF/PNG/audio), create materials/prefabs/levels, drag into the
  viewport.
- **Output Log** — the engine log, filtered. `path:line[:col]` opens in your code editor (Preferences ▸ Code ▸ Code
  editor command: `code`, `zed`, `$EDITOR` or a command with `{file}`, `{line}`, `{col}`), and an asset path opens the
  asset. Paths with spaces work.
- **World Settings** — **Edit ▸ World settings…**: the current level's gravity, environment, game rules, ambient
  audio, streaming, atmosphere, look and audio medium. Edits are undoable and save with the level. What reads the
  settings live changes at once; what a level load applies (game rules, baked GI, lightmap) takes effect when the level
  opens again. The viewport's `editor` quality keeps its own sky and exposure: pick another quality to see the atmosphere
  and look.
- **Kit** — a Tincal kit folder opened with **File ▸ Open kit…** (or from the project browser, no
  project needed): metadata, checksums, clips, sockets and the model with its cockpit. See
  [Open kit](open-kit.md).

## Viewport and outliner keys

Keys act on the pane the pointer is over. Every one is a command: search the palette (Ctrl+Shift+P) and press F2, or use
Preferences ▸ Keymap, to move it. A user shortcut replaces the built-in key.

| Key | Does | Command |
|---|---|---|
| W, E, R | translate, rotate, scale gizmo | `gizmo.translate`, `gizmo.rotate`, `gizmo.scale` |
| Ctrl (held) | invert the toolbar's Snap toggle while dragging (grid, angle or scale step) | |
| V (held) | vertex snap while translating: the pivot goes to the nearest vertex of the triangle under the pointer | `gizmo.vertex_snap` |
| End | drop the selection onto the first surface below it | `selection.drop_to_floor` |
| C | collider handles on the selection, instead of its gizmo | `collider.toggle_handles` |
| F | frame the selection (an eased 0.25 s move) | `view.frame_selection` |
| 1, 2, 3, 4 | perspective, top, front, side view preset (eased) | |
| Ctrl+0…9, Shift+0…9 | store, recall a camera bookmark | |
| Ctrl+D (viewport) | duplicate the selection | |
| Alt+drag on the gizmo | duplicate the selection and move the copies (one undo step) | |
| G | game view | |
| Right mouse + W, S, A, D, E, Q | fly forward, back, left, right, up, down | `camera.fly_forward`, `camera.fly_back`, `camera.fly_left`, `camera.fly_right`, `camera.fly_up`, `camera.fly_down` |
| Delete, Ctrl+D, Ctrl+G (pointer over the outliner) | delete, duplicate, group the selection | `outliner.delete`, `outliner.duplicate`, `outliner.group` |
| Ctrl+Z, Ctrl+Shift+Z | undo, redo | `edit.undo`, `edit.redo` |
| Ctrl+Shift+P | command palette | |

The viewport toolbar's **Pivot** choice sets where a multi-selection rotates and scales about: the median point, each
entity's own origin, or the active (last selected) entity. The Outliner row's eye hides an entity in the viewport (one
undo step) and its lock keeps it from being picked or box-selected; a link icon marks a prefab instance and a dot one with
overrides. **File** has Save all, Recent and Exit; **Edit** has Cut, Copy, Paste, Duplicate, Delete and Select all
(ADR-7204).

**Align to surface** in the viewport's **Tools** menu makes End also turn the selection's up onto the surface normal
(`placement.toggle_align_normal`). With **Collider handles** on (same menu), grips on the faces of a box, the surface of a sphere and
the ends and sides of a capsule change its size about its center, and the grip at its center moves its offset; each drag is
one undo step. See [Viewport tools](../agents/viewport-tools.md).

## Graybox shapes

**Create ▸ Shape** spawns a parametric shape at the viewport's orbit target and selects it: cuboid,
sphere, capsule, cylinder, cone, conical frustum, plane, torus, tetrahedron, ramp or prism. It is a
`Primitive` component (shape, parameters, color, roughness, metalness) on an entity with a
`Transform`, so the inspector edits its size and resolution, the viewport and the game draw the real
mesh, it saves in the level file, and Undo removes it. Agents use `scene.spawn_primitive`
([scene page](../agents/scene.md)). The meshers are ported from Bevy's `bevy_mesh` primitives
(MIT; see `docs/ported-sources.toml`).

## Placeable actors

Beside Shape, the **Create** menu places a **Player start**, **Trigger volume**, **Point light**,
**Spot light**, **Directional light**, **Camera**, **Reflection capture**, **Audio emitter**,
**Rect light**, **Tube light**, **Decal**, **Post-process volume**, **Spline**, **Nav link** and
**Smart object** at the viewport's orbit target. Each is one entity named after the entry, carrying
the runtime component with its default values (a rect light faces the floor; a decal names the
built-in scorch marking; a spline has two points 4 m apart), selected and saved with the level; one
Undo removes it. Agents use `editor.create_entity { kind }` ([Create menu](../agents/create-menu.md)).
Water planes and fog volumes are not Create entries yet.

## Editing and play

The editor edits through reflection, so every saved type is editable, and every property edit is undoable (`editor.set_render_override` is a live tint and is not).
Play-in-editor (Play / Pause / Step / Stop) runs the game crate if one is open (hot-reloaded), or
the engine's built-in systems otherwise. **⧉ Play in window** plays in a separate window with the game's
camera: a click in it captures the mouse, Shift+F1 frees it, and closing it stops play. The window opens at
the size (and, off Wayland, the place) the last one had. The **⛶** menu next to it picks the size the
game renders at in that window: the window's own, 720p, 1080p, a 9:16 or 9:19.5 phone, a 3:4 tablet, or a
custom size. The picture is scaled to fit with black bars, and clicks land where they would on that screen.
While the window is open, the viewport shows a note instead of a second copy of the game. See `docs/specs/19-editor.md` and the
[agent guide's editor page](../agents/editor.md) for the full `editor.*` method surface.

## Scripts

`editor.create_script <name>` writes `game/src/scripts/<name>.rs` and registers it at
`// REGIA:SCRIPTS`; the next play (or hot reload) picks it up. Scripts are the intended way to
attach behavior without leaving the editor.

## Bake, budgets, the content index and hierarchy drops

- **Bake runner** tab: runs `regia bake-gi` on a built-in scene or one of the project's `.bake.ron`
  descriptions. **Validate** is a dry run: it checks the description and lays out the probes, with
  no GPU and nothing written. **Bake** traces the volume in a separate process, so the editor keeps
  drawing; a progress bar shows the stage and the probes done. **Cancel** stops it. Then the panel
  shows the probe counts and the file written, or the error with the description key at fault. The
  editor runs the `regia` CLI next to its own executable (or `REGIA_CLI`). See
  [Editor Bake runner panel](../agents/editor-bake.md).
- **Budget meters**: the Kit panel shows the kit's triangles, materials, texture memory and the
  other model limits against its class (from `kit.json`; pick another class beside it), plus its
  LOD chain. Prefab mode shows the same line in its banner (hover for every meter). Green is under
  80 % of a limit, amber is near it, red is over. See [Editor budget meters](../agents/editor-budget.md).
- **Content Browser** speed: the asset list comes from a cached scan. It is not walked every frame,
  and only the rows in view are drawn. Changes made outside the editor show up within a second; ↻
  rescans at once. See [Editor content index](../agents/editor-content-index.md).
- **Content Browser grid and search**: **Grid** (beside **List**) shows thumbnails of textures and
  models, kept in `.regia/thumbs` (safe to delete). The search box ignores case; `-word` hides,
  `type:mesh` / `type:texture` / ... keeps one asset type, and `ext:png` keeps one file suffix. Best
  matches (file names that start with your first word) come first. Projects of tens of thousands of
  assets stay smooth: a folder costs its own size, typing narrows the last result, and an outside
  change re-lists only its folder. See [Editor asset browser](../agents/editor-asset-browser.md).
- **Outliner drops**: drag a row onto another row to parent it, or onto the strip under the tree to
  unparent it. A selected row brings the whole selection along. Entities keep their place in the
  world (hold Alt to keep the local transform instead). A drop into the dragged entity's own subtree
  is outlined in red and refused. One Ctrl+Z undoes the whole drop. See
  [Editor hierarchy drag and drop](../agents/editor-hierarchy.md).

## Several entities in the Inspector

Select more than one entity and the Inspector shows a field's value when they agree, and an em dash
when they do not. Changing a field writes it onto every selected entity that has the component.
A vector axis, a color channel and one Euler axis change on their own: the other axes stay as each
entity had them. A component that only some of the selection has is labeled "on k of n" and can
still be edited on those entities. Add component and Remove component apply to the whole selection.
One Ctrl+Z undoes the edit for every entity. On a prefab instance the edit is recorded as an
override of that instance, and one undo restores the previous value and override state.

Prefab overrides appear in bold with an entity id and field path. Revert and Apply target that
instance. Apply updates the loaded template in memory; one undo restores the template and marker.

## Conversation graph editor

The **Conversation** tab (in the bottom tab group of the default layout) shows a conversation (`assets/conversations/<id>.conv.ron`) as a
graph. Type the path and press **Open**. Blocks of lines sit between labels; `If`, `Choice` and `End` steps are nodes
of their own, and arrows show where each jump, branch and option goes (a red dangling arrow is a jump to a label
that does not exist). The layout is automatic and does not move when you edit a line; drag a node to place it by
hand (the position is kept in `.regia/conversation_layout/`, not in the conversation file). The wheel zooms, dragging
the background pans, **Fit** frames the whole graph, and **Search** finds nodes by text, key, label or speaker.

Click a node to edit its steps on the right: speaker, text key and note of a line, a choice's prompt and options
(text, condition, target), an `If`'s condition and branches, event names, and label names (renaming a label updates
every jump to it). Buttons below insert new lines and steps after the node. To retarget a jump, drag the small circle
at the bottom of a node onto a node that starts with a label. Every change is one Ctrl+Z step. **Save** writes the
file in its standard layout; opening and saving without changes leaves the file byte for byte as it was.

Problems the conversation checker finds (unreachable lines, missing labels, choices without options, missing voice
clips per language, lines too long for their subtitle time) are drawn on the nodes and listed under **Findings**;
click one to jump to its node. **Preview** plays the conversation from the start or from the selected node: tick the
conditions you want true, click through lines and pick choices, and turn on **Play voice** to hear recorded clips. A
file that does not parse opens read-only with the error and its line. Agents use the same panel through
[the conversation editor methods](../agents/conversation-editor.md).

## When the editor crashes

If the editor closes unexpectedly, the next launch says so. A level that was left unsaved still
offers **Restore** and **Discard autosave**. When a crash report was written, the same dialog
offers **View report**, **Copy to clipboard** and **Open folder**. The report holds the panic
message, the backtrace, the editor build, the operating system, the GPU adapter and the last
200 log lines. Home directories in the report are shown as `~`.

A panic inside game code while playing in the editor writes a report marked `pie`, stops play,
and leaves the editor running. A native crash (the process died without a Rust panic) is saved
as a feedback bundle next to the text reports. On Windows that capture stays off unless you
turn it on (`REGIA_NATIVE_CRASH=1`); `REGIA_NO_NATIVE_CRASH=1` turns it off on every desktop.

Nothing in a report is sent anywhere. Share the file yourself if you want someone to look at it.
Agents can list and read the same files with `editor.crash_reports` and
`editor.crash_report_read` ([Editor crash reports](../agents/editor-crash-reports.md)).

**Dismiss report** hides the offer for the current session and keeps the report on disk.

## Status bar, drawers and the console

The status bar's indicators are buttons: **Content** and **Log** open the drawers, **Unsaved: N** saves
the level (File ▸ Save all; other documents save from their own panels for now), **Jobs: N** lists the running background jobs with their progress and a
Cancel each, **Trace** starts and stops a trace recording, and **Problems: N** opens the Problems tab.
A drawer slides the Content Browser or the Output Log up over the dock without changing your layout;
Esc, a click elsewhere or moving the focus away closes it (Esc then does nothing else), and **Dock**
keeps it as a normal tab. Ctrl+Shift+Space toggles the content drawer outside play and text fields
(rebind it in the command palette with F2).

The Output Log ends with a console line: type a command id (`edit.undo`), an agent method with JSON
parameters (`editor.focus {"entity": 12}`) or a console variable the project registered. Tab completes
names and shows a method's parameters, Up and Down walk the history (kept per project), and the result
is printed in the log. The main toolbar's **Quick settings** gathers snapping and its steps, the realtime
viewport, the main viewport's show flags and preview sun, and autosave. Agents use the same features
through [the status bar, drawer and console methods](../agents/editor-console.md).
