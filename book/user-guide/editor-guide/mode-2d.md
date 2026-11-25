# 2D

<!-- index: 2D mode -->
<!-- index: Tilemap -->

Panel id: `mode2d`. The **2D** tab paints a `.tilemap.ron` and, when you open one, an `.autotile.ron` rule set. It edits those source files. A running game keeps its own loaded copy until the asset watcher cooks the file you saved. Depth: [2D](../2d.md), [2D editing (agents)](../../agents/editor-2d.md). Spec: `docs/specs/86-2d-toolkit.md`.

The tab is in the default layout's bottom group. **View ▸ Reset layout** restores a closed one. An agent opens it with `editor.layout_apply {"open": "mode2d"}`. The World and 2D tiles workspaces open it with the viewport ([Layouts, workspaces and feature profiles](layouts-workspaces.md)).

## Paint a map

1. Tick **2D mode**. The viewport switches to the front orthographic pane and frames the map.
2. Press **Open map…** and choose a `.tilemap.ron` under the project. If the map already open has unsaved edits, choose **Save and open**, **Discard and open** or **Cancel**.
3. Press **Open rules…** for an `.autotile.ron` that fits this map, or leave rules clear and paint raw tiles.
4. Under **Layers**, pick the tile layer strokes write into. **Add object** and **Delete object** edit object layers.
5. Under **Tool**, pick paint, line, rectangle, bucket or pick. **Filled** makes the rectangle a fill or an outline. **Contiguous** makes the bucket fill the connected region or every matching cell.
6. Under **Brush**, pick a tile from the palette (arrow keys move the palette focus; Enter or Space paints the map cursor). **Flip X**, **Flip Y** and **Diagonal** flip that tile. With rules open, pick a terrain instead.
7. Paint in the viewport: left-drag paints, Shift erases. One stroke is one undo step (Ctrl+Z).
8. **Save** writes the map. Saving the level also writes the map when it is dirty.

**Grid** draws the cell grid when cells are large enough on screen. **Play animations** advances animated tiles. **Rulers** and **Snap to guides** use the guide list; **Add vertical guide** and **Add horizontal guide** add one. **Paint random tiles** scatters the weighted tiles you list, including a chance of leaving a cell alone.

This tab does not edit `.tmx` or `.tmj` files. Author those elsewhere and import, or work in `.tilemap.ron`.

## Keys while the viewport is painting

These apply when 2D mode is on and no text field has focus. Rebind them from the [keymap](keymap.md).

| Key | Action |
|---|---|
| B | Toggle 2D mode |
| Left-drag | Paint |
| Shift | Erase |
| `,` / `.` | Previous / next layer |
| `[` / `]` | Previous / next terrain |
| Ctrl-drag | Copy a rectangle into the stamp |

The tab's **Rotate**, **Flip** and **Drop** buttons transform the stamp. Arrow keys move the map cursor when it has focus; Enter or Space paints that cell and Delete erases it.

## Actions

| Action | Control | Agent method |
|---|---|---|
| Turn the mode on, open a map or rules, pick a layer | **2D mode**, **Open map…**, **Open rules…**, the layer list | `editor.mode2d_set`, `editor.mode2d_get` |
| Paint or erase | The viewport, or the map cursor | `editor.tile_stroke` |
| Stamp | Ctrl-drag; Rotate, Flip, Drop | `editor.tile_stamp` |
| Save the map | **Save**, or save the level | `editor.tilemap_save` |
| New map, tilesets, layers | The layer and tileset controls | `editor.tilemap_new`, `editor.tilemap_tilesets` |
| Rule set | **Open rules…** and the terrain brush | `editor.autotile_open`, `editor.autotile_edit`, `editor.autotile_save` |

## Known limits

- Opening another map while this one is dirty is refused until you save or discard (`mode2d_unsaved`).
- A rule set that does not fit the map is refused (`mode2d_bad_rules`).
- A missing tileset image draws flat colors. Painting still works. **Reload images** (or `reload_images`) reads the files again.
- An orthogonal map draws at most 200 000 tiles; farther zoom draws every Nth cell. Isometric, staggered and hexagonal maps stop at 200 000 quads instead of skipping cells.
- Paths must stay inside the project.

## See also

[Viewport](viewport.md), [Content browser](content-browser.md), [2D](../2d.md), [Editor guide](index.md).
