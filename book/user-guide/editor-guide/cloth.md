# Cloth

Panel id: `cloth`.

The **Cloth** tab opens a `.cloth.ron` (or a `.soft.ron` soft body) from the project, edits its
simulation parameters, and paints per-vertex maps on the simulation mesh in the viewport. It edits
the file through its own types and saves it atomically; the level is only touched when you change an
instance's blend or LOD distances. Depth: [Cloth (agents)](../../agents/cloth.md); the solver itself
is described in [Cloth and ropes](../cloth.md).

## Open it

The tab is in the default layout's bottom group; **View ▸ Reset layout** brings back a closed one.
An agent brings it forward with `editor.layout_apply {"open": "cloth"}`. There is no palette command
for this tab.

## Edit and paint a cloth

1. Type a project-relative path such as `cloth/cape.cloth.ron` and press **Open**. The header shows
   the cloth's name, path and kind, and `(edited)` once it has unsaved changes.
2. Select an entity that has a cloth or soft-body simulation component: it becomes the target of
   the instance controls. Without one the tab says to select an entity with a `ClothSimSource`.
3. For a cloth, pick a **starting point** (a preset, or none). Edit any parameter in the grid (a soft
   body shows the parameters it has). For a cloth choose a **self-collision** mode (off, spheres or
   vertex_triangle) and its radius.
4. With a target selected, set its **blend** and its **LOD m** distances; these are saved with the level.
5. Turn on **Paint**, pick a **map**, a **value**, a brush radius **r**, **str**ength and a falloff
   (linear, smooth or hard). Press and drag over the sim mesh in the viewport and release: the
   stroke is painted as one undo step. The viewport colors each vertex on the **ramp m** range.
   Ctrl-click a vertex to select or deselect it for the gizmos.
6. **Simulate preview** steps a private solver with the edited parameters so you can see the result
   without touching the level. **Gizmos** shows the `cloth` gizmo group (body proxies, collision
   shapes, and `reach` and `standoff` for selected vertices).
7. Press **Save** (enabled while there are changes). A file you did not change is never rewritten.

## Actions and agent methods

| Action | Control / key | Agent method |
|---|---|---|
| Open a file | Path field, **Open** | `editor.cloth_panel_edit` with `path` |
| Read the panel | The tab | `editor.cloth_panel_state` |
| Parameters, preset, self-collision, wind model | The grid and drop-downs | `editor.cloth_panel_edit` |
| Instance blend and LOD distances | **blend**, **LOD m** | `editor.cloth_panel_edit` (`blend`, `lod_distances`) |
| Paint mode and brush | **Paint**, **map**, **value**, **r**, **str**, falloff | `editor.cloth_panel_edit` (`paint`) |
| Paint a stroke | Drag in the viewport | `editor.cloth_paint_stroke` |
| Select vertices | Ctrl-click (Cmd on macOS) in paint mode | `editor.cloth_panel_edit` (`selected`) |
| Simulate preview, gizmos | **Simulate preview**, **Gizmos** | `editor.cloth_panel_edit` (`simulate`, `gizmos`) |
| Save | **Save** | `editor.cloth_panel_edit` (`save: true`) |
| Undo, redo | Ctrl+Z, Ctrl+Y | `editor.cloth_panel_edit` (`undo`, `redo`) |

## Keyboard

Every change is one entry on the editor's shared undo stack, so Ctrl+Z and Ctrl+Y undo and redo
cloth edits and strokes in order with level edits. In paint mode Ctrl-click selects a vertex. The tab
has no other keys of its own.

## Known limits

- Paint needs a sim mesh: the cooked asset of a cloth simulation in the level with the file's name,
  else the file cooked from the project. Without one the tab shows `no sim mesh` and the reason.
- Paint works on cloth files; a soft body has the parameter grid but no paint controls.
- One stroke holds a limited number of points; past the limit the tab says so and the release paints
  the points kept.
- The preview never changes the level.

See also: [Physics](../physics.md), [Editor guide](index.md).
