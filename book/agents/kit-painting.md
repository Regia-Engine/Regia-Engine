# Kit painting

The viewport's **Kit painting** switch opens the modular-piece palette. Enter a project-relative
kit folder and activate **Refresh pieces**. Select a `.glb`, `.gltf`, `.prefab.ron` or
`.prefab.variant.ron` piece. Set Grid size (meters), Floor, and Quarter turns. The existing kit
manifest has no grid-spacing field; Grid size supplies an explicit value, initially 1 meter.

The grid lies in XZ. A cell `[column,row]` places a piece's origin at
`[column*grid, floor*grid, row*grid]`. Click to place, or drag to fill a straight line of
face-adjacent cells. Right-click or right-drag erases this kit palette's pieces on that floor.
A stroke commits on release and adds one undo entry. Occupied cells are skipped, and repeated
cell coordinates never duplicate a placement. Other assets and parented entities are left alone.

With the viewport focused: arrows move the cell cursor, Enter places, Delete erases, R rotates
90 degrees, PageUp/PageDown changes the floor, and Escape leaves the mode. The palette, floor,
rotation, grid size and coordinate controls are also reachable with Tab. **Place at cell** and
**Erase at cell** provide focusable alternatives to all mouse actions. The cursor survives an
unmoved mouse while using the keyboard. Grid and selection colors come from editor theme tokens;
controls and drawing scale with the editor's appearance scale.

## editor.kit_paint

```json
{"method":"editor.kit_paint","params":{"folder":"assets/kit","piece":"assets/kit/wall.prefab.ron","grid":2,"floor":1,"rotation":90,"cells":[[0,0],[1,0],[2,0]]}}
{"method":"editor.kit_paint","params":{"op":"erase","cells":[[1,0]],"floor":1}}
{"method":"editor.kit_paint","params":{"enabled":true}}
```

Parameters are optional and retain the current tool value when omitted:

| Parameter | Meaning |
|---|---|
| `piece` | Project-relative piece path from the palette folder. |
| `cells` | 1..=4096 integer `[column,row]` pairs, each coordinate within ±1,000,000. Omit to configure/read session state without editing the level. |
| `floor` | Integer floor within ±1,000,000. |
| `rotation` | Clockwise degrees, exactly 0, 90, 180 or 270. |
| `grid` | Cell width/depth and floor spacing in meters, 0.01..=10000; initially 1. |
| `folder` | Project-relative kit folder; refreshes its palette. Only immediate mesh/prefab files are listed. |
| `op` | `paint` (default) or `erase`. Erase matches all pieces from this folder. |
| `enabled` | Arm/disarm the viewport tool; session state only. |

An edit returns `{changed,entities,undoable:true}`. `entities` lists newly placed root ids (empty
for erase). A configuration/query call returns `{enabled,folder,piece,grid,floor,rotation,cells,pieces}`.
`editor.undo` and `editor.redo` use the shared history. Painting into only occupied cells or erasing
empty cells adds no history entry.

Mesh pieces become ordinary level entities with `MeshSource` and `Transform`. Prefab pieces remain
normal prefab instances, with the usual override/save behavior. The palette and grid settings are
session memory; no new saved content format or runtime editor dependency is introduced. If spawning
fails, the partial stroke is rolled back and the preexisting undo/redo branch is preserved.

Errors: `kit_paint_invalid` covers invalid coordinates/settings, unsupported or missing pieces,
unsafe paths, folder/metadata I/O errors, malformed prefabs and missing scene type registration.
Dispatcher limits and scene-edit permission checks apply before the handler. Calls that violate
those limits return the protocol's standard parameter/permission errors. An invalid request does
not change the scene or commit its new settings.

Verification: `kit_paint` unit tests cover face-connected lines in all directions, bounds, large
strokes beyond the undo stack's entry cap, floor/rotation, prefab identity, erase, exact grouped
undo/redo and failure preservation. `terrain_kit_tools` checks the registered schema and real
agent dispatcher; keyboard and viewport press tests require no GPU or golden image.
