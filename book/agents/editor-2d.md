# Editor 2D mode

The editor's 2D mode (`docs/specs/86-2d-toolkit.md` § 6, ADR-1889) paints tiles and auto-tiled
terrains onto a `.tilemap.ron` in the front orthographic view. It edits the map **source file** (the
document), not the copy a running game has loaded: `editor.tilemap_save` writes it back and the asset
watcher re-cooks it. For edits inside a running game use the runtime `tilemap.*` writes
([2D](2d.md)).

Registered by `regia_editor::mode2d` (editor only).

| Method | Params | Result |
|---|---|---|
| `editor.mode2d_get` | — | `{ active, map, rules, layer, brush, grid, hover, origin, view, stamp, clock, undo_depth }` |
| `editor.mode2d_set` | `active?`, `map?`, `discard?`, `rules?` (path or `null`), `layer?`, `brush?`, `grid?`, `clock?`, `reload_images?`, `origin?` | the new state |
| `editor.tile_stroke` | `cells: [[x, y], …]` (≤ 65 536), `erase?` | `{ changed, undo_depth }` |
| `editor.tile_stamp` | `action` (`copy` / `transform` / `clear`), `rect?: [x0, y0, x1, y1]`, `op?` | `{ stamp, brush }` |
| `editor.tilemap_save` | `path?` | `{ path, bytes }` |
| `editor.tilemap_new` | `path`, `size?`, `tile_size?`, `layer?`, `tilesets?`, `discard?` | `{ path, bytes, version, tilesets, layers }` |
| `editor.tilemap_tilesets` | `op` (`add` / `remove` / `replace` / `move` / `layer_move`), `tileset?`, `source?`, `layer?`, `to?` | `{ done, tilesets, layers, layer, undo_depth }` |
| `editor.tileset_candidates` | — | `{ candidates }` |
| `editor.autotile_get` | — | `{ doc, terrain, rule, preview, undo_depth }` |
| `editor.autotile_open` | `path`, `discard?` | the rule editor's state |
| `editor.autotile_new` | `path`, `tileset?`, `discard?` | the rule editor's state |
| `editor.autotile_edit` | `op`, `terrain?`, `rule?`, `cell?`, `value?`, `pattern?`, `tile?`, `flip?`, `at?`, `to?`, `index?`, `name?`, `layout?`, `border?`, `seed?`, `tileset?` | the state and `{ done }` |
| `editor.autotile_save` | `path?` | `{ path, bytes }` |
| `editor.autotile_preview` | `terrain?`, `cells?`, `rect?`, `size?` | `{ size, tiles, gids }` |
| `editor.autotile_select` | `terrain?`, `rule?`, `size?`, `cells?`, `rect?` | the rule editor's state |

- **Paths** are relative to the editor project's root, or absolute under the project or the temp dir (the document
  rule of [Request paths](safety.md#request-paths), ADR-7388: `path_outside_project` otherwise; a level's
  `TileMap.map` that leads out of the project names no map). `map` must be a `.tilemap.ron`;
  Tiled sources (`.tmx`, `.tmj`) are edited in Tiled. Loading a map while the loaded one has unsaved
  edits fails with `mode2d_unsaved` unless `discard: true`.
- **`map`** in the state: `path`, `size` (tiles), `tile_size`, `dirty` (unsaved edits),
  `layers` (`index`, `name`, `kind` = `tiles` / `objects`) and `tilesets` (`name`, `first_gid`,
  `tile_count`, `columns`, `image`, `loaded`, `error`). A tile's gid is `first_gid` + its local id.
  `layer` is the tile layer strokes paint into (`{ index, name }`); loading a map picks its first
  tile layer.
- **`tilesets[].error`** says why an image is missing: not found, not an image, or larger than the
  GPU's texture limit. Its tiles then draw in flat colors; painting still works.
  `reload_images: true` decodes the images again after they change on disk.
- **`view`**: `{ tiles, stride }` of the front pane's last frame. An orthogonal map draws at most
  200 000 tiles: a far zoom sets `stride` above 1 and draws every Nth cell. Isometric, staggered
  and hexagonal maps do not stride-sample; they stop at 200 000 quads and leave `stride` at 1.
- **`rules`** is an `.autotile.ron` ([format](../../docs/formats/autotile-ron.md)); the state shows its
  `path`, `tileset` and `terrains`. It must fit the map (`mode2d_bad_rules` otherwise).
- **`brush`**: `{ "tile": gid, "flip": { "x", "y", "diagonal" } }` (a raw tile), `{ "terrain": name }`
  (the painted cells and their 8 neighbours re-resolve to edge and corner tiles; on staggered and hexagonal maps the
  neighbors are the geometric ones of `docs/specs/86-2d-toolkit.md` § 13) or `{ "erase": true }`
  (with rules loaded, the neighbours re-resolve too) or `{ "stamp": true }` (the copied stamp).
- **Orientation**: the state's `map.orientation` is `orthogonal`, `isometric`, `staggered` or
  `hexagonal`. Picking (`hover`), the grid, the hover outline and the tiles' draw order follow it:
  a click lands in the diamond or hexagon under the pointer. An orthogonal grid is full-span axis
  lines, drawn only when the grid is on, each tile is at least 4 px on screen, and both sides are
  at most 1024. Isometric, staggered and hexagonal maps outline each cell, up to 16 384 cells,
  under the same grid and tile-size checks. A larger map still draws its bounds.
- **Stamps**: `editor.tile_stamp` with `action: copy` and `rect` takes a rectangle of the active
  layer into the stamp and selects the stamp brush (`brush: { "stamp": true }`); `transform` with
  `op` (`rotate_cw`, `rotate_ccw`, `rotate_180`, `flip_x`, `flip_y`) turns it, flip bits included;
  `clear` drops it. Painting with the stamp brush pastes it centered on every painted cell, as one
  undo entry per stroke (`editor.tile_stroke`). Empty stamp cells leave the map alone. In the pane,
  Ctrl-drag selects the rectangle and copies it on release; the 2D tab has Rotate, Flip and Drop.
  The state shows `stamp: { width, height }`.
- **`origin`**: world position of the map's bottom-left corner. Loading a map takes it from a
  `regia_2d::TileMap` entity whose asset path ends the file path, else `[0, 0, 0]`.
- Turning the mode on, or loading a map, switches the viewport to its front pane and frames the map
  on the next frame.
- **`editor.tile_stroke`** is one undo entry: `editor.undo` restores every cell exactly and
  `editor.redo` paints them again. Every cell is checked before anything is written.
- **`editor.tilemap_save`** writes atomically (a temporary file, then a rename). With `path` it writes
  a copy and the document stays dirty.
- **Saving the level saves the map too.** `editor.save_level` (and File → Save level, and the save
  on close) also writes the 2D mode's map to its own `.tilemap.ron` when it has unsaved edits; the
  answer's `tilemap_saved` is that path, or `null` when the map was clean or none is loaded.

In the viewport: `B` toggles the mode, left-drag paints (a tile brush fills every cell on the pointer's
path; a stamp lands once on each new cell the pointer enters, not on the cells between samples; a stroke
stays on the layer it started on), Shift erases (including a stamp erase, which fills the line), `,` / `.`
step the layer, `[` / `]` the terrain. The
front pane draws the document's tiles, then the map bounds, the tile grid (when a tile is at least
4 px on screen), the hover cell (`hover` in the state) and a preview of the tile brush. Placement,
flips, layer offset and opacity match Play. Animated tiles show the frame of the **pane clock**
(`clock` in the state: `{ playing, time_ms }`, starts paused at 0 ms, so a fresh pane shows each
tile's first frame). Turn it on with the 2D tab's *Play animations* box (Rewind goes back to 0) or
`editor.mode2d_set {"clock": {"play": true}}`; `{"clock": {"step_ms": 250}}` advances it by exactly
250 ms whether it plays or not (at most 3 600 000), `{"reset": true}` rewinds; they apply in the order
reset, step, play. The pane picks the frame with the same function and millisecond conversion the
game uses, so after the same steps both show the same frame (an agent can step, capture, compare).
A bad `clock` is `mode2d_bad_params`. Sprites other than the map are not drawn: Play shows those.

The **2D** tab shows the same state to a person: the mode and grid switches, Open map…, Save map,
Reload images, Open rules…, Clear rules, the layers, Erase, the terrains, the flips, the **New tile
map**, **Tilesets and layers** and **Auto-tile rules** sections, the map cursor and the tile
palette. `B` focuses it when no map is loaded.

## New tile maps and the tileset binding

(ADR-7742.) **`editor.tilemap_new`** writes a new `.tilemap.ron` at the current format version
through the format's own writer and opens it in the 2D mode: `size` is `[width, height]` in cells
(1..=4096, default `[16, 16]`), `tile_size` the cell in pixels (default `[16, 16]`), `layer` the name
of its one empty tile layer (default `ground`), `tilesets` a list of sources. A source is
`{ "image": path, "tile_size"?: [w, h], "name"? }` (an image cut into whole cells; columns and count
follow from its pixel size, the name defaults to the file stem) or `{ "map": path, "tileset": name }`
(a tileset of another `.tilemap.ron`). Image paths in the file are relative to the new map's folder.
An existing `path` is refused (`mode2d_bad_params`) and nothing is written; so is a path that is not a
`.tilemap.ron` (`mode2d_not_tilemap_ron`), or unsaved edits of the loaded map without `discard: true`
(`mode2d_unsaved`). Content ▸ New ▸ Tile map (`editor.create_asset` with kind `"tilemap"`) writes
the default map with no tileset; the 2D tab's **New tile map** section takes every field and picks
the tileset from a list.

**`editor.tilemap_tilesets`** edits the loaded map's binding as one undo entry: `add` (`source`),
`remove` (`tileset`, by name or index), `replace` (`tileset`, `source`), `move` (`tileset`, `to`) and
`layer_move` (`layer` by name or index, `to`; layers are bottom first). Tilesets are renumbered from
GID 1 in list order and every tile of every layer (and every tile object) is remapped with its flip
bits; tiles of a removed tileset, and tiles whose local id the replacement does not have, become
empty. Tileset names stay unique (rule sets bind by name). The active layer follows a move.
`editor.undo` restores the map bit for bit. The **Tilesets and layers** section of the 2D tab has the
same buttons (Up, Down, Replace, Remove, Add tileset, Lower, Raise).

**`editor.tileset_candidates`** lists what the picker offers: `{ kind: "image", path }` for each
`.png` and `{ kind: "map", map, tileset, tile_size, tile_count }` for each tileset of a
`.tilemap.ron` under the project's `assets/` folder (at most 512, sorted by path).

## Auto-tile rule editor

(ADR-7742.) The 2D tab's **Auto-tile rules** section edits one `.autotile.ron`
([format](../../docs/formats/autotile-ron.md)) with its own document: `editor.autotile_open` reads a
file, `editor.autotile_new` starts a set (current format version, one rules terrain named `terrain`
whose rule `???/?#?/???` writes tile 0; `tileset` defaults to the 2D map's first tileset) that is
written on `editor.autotile_save`. `editor.autotile_get` shows `doc` (`path`, `dirty`, `version`,
`tileset`, `seed`, `border`, `terrains` with `name`, `layout`, `rules` (`pattern`, `tile`, `flip`),
`tiles` of a preset layout, `fallback`), the selected `terrain` and `rule`, the `preview` grid and
`undo_depth`.

**`editor.autotile_edit`** applies one `op` as one undo entry. The result is validated by the
format's own rules; an invalid edit (a tile another terrain owns, a `.` center) is
`autotile_invalid` and changes nothing. `terrain` is a name or index (default: the selected one).

| `op` | Params | Does |
|---|---|---|
| `set_cell` | `rule`, `cell` (0..9, row by row, top row first), `value` (`#`, `.`, `?`) | sets one neighbor of a rule's 3x3 pattern |
| `set_pattern` | `rule`, `pattern` | replaces the pattern |
| `set_tile` | `rule`, `tile`, `flip?` `{x?, y?, diagonal?}` | sets the rule's output |
| `add_rule` | `tile`, `pattern?` (default `???/?#?/???`), `at?` | inserts a rule |
| `remove_rule` | `rule` | removes it (a terrain keeps one) |
| `move_rule` | `rule`, `to` | reorders (the first match wins) |
| `set_fallback` | `tile` (or `null`) | a rules terrain's fallback |
| `set_entry` | `index`, `tile` | an `edge16` / `blob47` / `wang_corner` entry |
| `add_terrain` | `name`, `layout?` (`rules`, `edge16`, `blob47`, `wang_corner`) | appends a terrain on unused tiles |
| `remove_terrain`, `rename_terrain` | `name` (rename) | |
| `set_border`, `set_seed`, `set_tileset` | `border` (`same` / `empty`), `seed`, `tileset` | |

Patterns the editor writes are canonical: three rows joined by ` / `. When the 2D mode's brush uses
the same file (`editor.mode2d_set rules`), each edit reaches the brush at once.

**`editor.autotile_preview`** paints a terrain over `cells` (or `rect`, or the tab's stored preview
cells) of a scratch map of `size` (1..=64 per side) with the runtime's auto-tiler, the function the
2D brush and `tilemap.paint` call, and returns `tiles` (rows of local ids, `null` = empty) and the raw
`gids` (flip bits included). Nothing changes. The scratch map uses the 2D map's tileset of the rule
set's name, or a placeholder. **`editor.autotile_select`** sets the tab's view state (not undone):
the selected `terrain` and `rule`, the preview's `size` and its painted `cells` / `rect`. In the tab
a click on a pattern cell steps it `?` → `#` → `.` → `?` (the center only `?` ↔ `#`), and a click on
a preview cell paints or clears it.

## Keyboard

The 2D tab works without a pointer (a11y FU-5). Each palette tile is a focusable button named
`Tile <local id>`: with one focused, the arrow keys move the focus one tile (Up and Down by a palette
row, scrolling it into view) and Enter or Space picks it as the brush. The **Map cursor** row
(named "Map cursor") moves a cell with the arrows while it has the focus, shows it as the pane's
hover cell, paints the brush there with Enter or Space and erases it with Delete, one undo entry per
press (as `editor.tile_stroke` with one cell). Pattern-grid cells are named `Neighbor <slot> is <value>`
and preview cells `Preview cell <x>, <y>`.

## Error codes

Errors are `F0002` with `data.mode2d_error`:

| Code | Meaning |
|---|---|
| `mode2d_bad_params` | A parameter is missing or has the wrong shape. |
| `mode2d_no_map` | No map is loaded (load one with `map`). |
| `mode2d_no_rules` | The brush is a terrain but no rule set is loaded. |
| `mode2d_unknown_terrain` | The rule set has no terrain of that name. |
| `mode2d_bad_layer` | The layer does not exist or is an object layer. |
| `mode2d_outside_map` | A cell is outside the map; nothing was written. |
| `mode2d_bad_gid` | No tileset holds the tile, or it carries flag bits. |
| `mode2d_not_tilemap_ron` | The path is not a `.tilemap.ron`. |
| `mode2d_load_failed` | The map file could not be read or parsed. |
| `mode2d_bad_rules` | The rule set could not be read, parsed or bound to the map. |
| `mode2d_save_failed` | The map could not be written. |
| `mode2d_unsaved` | The loaded map has unsaved edits; save it or pass `discard: true`. |
| `mode2d_bad_stamp` | The stamp rectangle is outside the map, empty or over the size limit, or the stamp does not fit (a tile no tileset holds). |
| `mode2d_no_stamp` | The stamp brush is selected, or a transform asked for, but nothing was copied yet. |
| `autotile_bad_params` | A rule-editor parameter is missing or malformed, an index is out of range, the op does not fit the terrain's layout, or a path is not an `.autotile.ron` (or already exists, for `editor.autotile_new`). |
| `autotile_no_document` | No rule set is open in the rule editor. |
| `autotile_invalid` | The edit would make a rule set the format refuses; nothing changed. |
| `autotile_load_failed` | The `.autotile.ron` could not be read or parsed. |
| `autotile_save_failed` | The rule set could not be written. |
| `autotile_unsaved` | The open rule set has unsaved edits; save it or pass `discard: true`. |
| `autotile_preview_failed` | The rules do not bind to the preview's tileset. |

## Recipe: paint a walled room and save it

```text
editor.mode2d_set {"active": true, "map": "assets/maps/room.tilemap.ron", "rules": "assets/maps/room.autotile.ron", "layer": "walls", "brush": {"terrain": "wall"}}
editor.tile_stroke {"cells": [[2, 2], [3, 2], [4, 2], [5, 2], [5, 3], [5, 4], [4, 4], [3, 4], [2, 4], [2, 3]]}
editor.mode2d_get {}                 # map.dirty = true, undo_depth + 1
editor.undo {}                       # the whole stroke goes away
editor.redo {}
editor.tilemap_save {}
```

A whole-window capture shows the tiles in the front pane, without Play. After the save, Play the
level and `tilemap.get_tile` the corners: their `local_id`s are the terrain's corner tiles.

## Recipe: a new map, a rule, a preview

```text
editor.tilemap_new {"path": "assets/maps/cave.tilemap.ron", "size": [32, 18], "tile_size": [16, 16], "tilesets": [{"image": "assets/tiles/cave.png"}]}
editor.autotile_new {"path": "assets/maps/cave.autotile.ron"}
editor.autotile_edit {"op": "add_rule", "at": 0, "pattern": "?#? / ?#? / ?#?", "tile": 4}
editor.autotile_preview {"size": [5, 5], "cells": [[2, 1], [2, 2], [2, 3]]}   # tiles[2][2] = 4
editor.autotile_save {}
editor.mode2d_set {"rules": "assets/maps/cave.autotile.ron", "brush": {"terrain": "terrain"}}
editor.tile_stroke {"cells": [[4, 4], [4, 5], [4, 6]]}
editor.tilemap_save {}
```

## Paint tools, random brush, terrain path mode (ADR-7853)

| Method | Params | Result |
|---|---|---|
| `editor.tile_stroke` | adds `tool?` (`paint` / `line` / `rect` / `bucket` / `pick`), `filled?`, `contiguous?`, `random?`, `mode?` (`connect` / `path`) | `{ changed, undo_depth, tool, brush }` |
| `editor.mode2d_set` | adds `tool?`, `rect_filled?`, `bucket_contiguous?`, `random?` (object or `null`), `autotile_mode?` | the new state |
| `editor.mode2d_get` | — | adds `tool`, `rect_filled`, `bucket_contiguous`, `random`, `autotile_mode` |

- **Tools.** `paint` paints `cells` as given (the pane fills the line between pointer samples).
  `line` takes two cells and paints the line between them; `rect` takes two corners and fills the
  rectangle (`filled: false` draws its border); `bucket` takes one cell and fills the 4-connected
  cells that hold the same tile (`contiguous: false`: every such cell of the layer); `pick` with one
  cell makes that cell's tile the brush (an empty cell picks erase, and the random brush turns off),
  with two corners it copies the rectangle into the stamp. With the stamp brush, `line`, `rect` and
  `bucket` tile the stamp over their cells, aligned to the map. Every call is one undo entry. The
  call's `tool`, `filled`, `contiguous`, `random` and `mode` apply to that call only; `editor.mode2d_set`
  changes the tab's own settings.
- **Random brush.** `random: {tiles: [{tile, weight?, flip?}], nothing?, seed?}` (in a stroke call it
  applies whatever the brush is; as the tab's setting it applies to the tile brush):
  each painted cell takes one of `tiles` by weight (default 1), or is left alone with chance `nothing`
  (0 to 1). The pick is a hash of `seed` and the cell, so a call repeats exactly; the pane moves the
  seed after every stroke. 1 to 256 tiles, weights at least 1.
- **Terrain modes.** `connect` (the default) re-resolves the painted cells and every neighbor of the
  same terrain. `path` joins only the cells painted in the same stroke (or call): existing neighbors of
  the terrain keep their tiles and the stroke does not join them.
- **In the pane.** A line or rectangle shows its cells while dragging and lands on release. Ctrl+click
  (any tool) or a click with the pick tool picks the tile; Ctrl-drag or a pick-tool drag picks a
  stamp. A right-click erases the cell under the pointer in every tool (the bucket erases its region);
  a right drag still pans the view. Shift-drag erases with the current tool.

## Layer tree and object layers (ADR-7853)

| Method | Params | Result |
|---|---|---|
| `editor.tile_layers` | `action?` (`list` / `select` / `move` / `object_add` / `object_set` / `object_delete`), `layer?`, `depth?`, `id?`, `object?` | `{ map, layers, active, object_layer, selected_object, id? }` |

- **Typed layers with depth.** `layers` lists every layer with `kind` (`tiles` or `objects`) and
  `depth` (its index: the list is bottom first, so a higher depth draws later). `move` sets a layer's
  depth (`depth` is the new index); the active paint layer and object layer follow their layers. Depth
  is the layer order of the `.tilemap.ron`, so the format does not change.
- **Active target.** `select` with a tile layer makes it the paint layer; with an object layer it
  makes it the edited object layer (and `id` selects an object on it).
- **Objects.** `object_add` adds an object (a fresh map-unique id) with
  `object: {name?, class?, position?: [x, y], size?: [w, h], rotation?, shape?: rect|ellipse|point,
  properties?: {key: bool|number|string}}`, in map pixels with +Y down as the file stores them;
  `object_set` changes the given fields (`properties` entries with `null` are removed);
  `object_delete` removes it. Each change is one undo entry.
- **In the 2D tab.** The Layers tree lists the map's layers top first, each with its kind and depth,
  Raise and Lower buttons, and an object layer's objects under it; the selected object shows its name,
  class, position and size fields, with Add object and Delete object.

## Rulers and guides (ADR-7853)

| Method | Params | Result |
|---|---|---|
| `editor.mode2d_guides` | `action?` (`list` / `add` / `move` / `remove` / `clear` / `set` / `snap`), `axis?`, `at?`, `index?`, `rulers?`, `snap?`, `moving?`, `others?`, `tolerance?`, `alt?` | `{ rulers, snap, guides, snap_result? }` |

- Rulers run along the front pane's top and left edges in level units (from the map's bottom-left
  corner, +Y up), drawn with theme colors. Drag from the top ruler for a horizontal guide or from the
  left ruler for a vertical one; drag a guide to move it and drop it on a ruler to remove it. Guides
  are editor view state: they are saved per level in `.regia/editor_view.ron` (`guides_2d`) and never
  written into the map or the level.
- With an object layer active, a click in the pane selects the object under the pointer (empty space
  clears the selection) and a drag moves it, snapping to the guides, then to the edges and centers of
  the layer's other objects or to equal gaps between its neighbors; the alignment lines and spacing
  hints are drawn while dragging. Alt held drags without snapping. A drag is one undo entry. Object
  boxes ignore rotation; dragging works on orthogonal maps (other orientations: the tab's fields or
  `editor.tile_layers`). Tile tools do not paint while an object layer is active.
- `snap` reports what a moved box would snap to: a guide first (its edges or center within
  `tolerance`), else edge and center alignment with `others`, or equal gaps to its nearest neighbors
  on each side. `snap_result` has the `offset` to add, `to_guide` per axis, the alignment `lines` and
  the equal-spacing hints (`spacing`), each `[x0, y0, x1, y1]`. `alt: true` (Alt held) turns snapping
  off. The pane's object drag uses the same rules (tolerance: 6 pixels at the pane's scale).
- In the 2D tab, the **Rulers and guides** section's **Rulers**, **Snap to guides**, **Add vertical
  guide**, **Add horizontal guide** and each guide's position field and Remove button do the same from
  the keyboard. The **Random brush** and **Patterns** sections open the same way (Tab, then Space).

## Saved patterns (ADR-7853)

| Method | Params | Result |
|---|---|---|
| `editor.tile_pattern_list` | `tileset?` | `{ tileset, path, patterns: [{ name, width, height }], brush }` |
| `editor.tile_pattern_save` | `name`, `tileset?` | the same |
| `editor.tile_pattern_use` | `name`, `tileset?` | the same (`brush` is the stamp) |
| `editor.tile_pattern_delete` | `name`, `tileset?` | the same |

- A pattern is the stamp saved under a name. It is stored with its tileset in a
  `.tilepatterns.ron` next to the tileset's image (`tiles/grass.png` → `tiles/grass.tilepatterns.ron`,
  [format](../../docs/formats/tilepatterns-ron.md)), so every map using the tileset sees it. Cells are
  stored as the tileset's local ids, so a map that gives the tileset another `first_gid` reads it right.
- `tileset` defaults to the tileset of the stamp's first tile, else the map's first tileset.
  `save` replaces a pattern of the same name. Save and delete write the file at once; they are not on
  the map's undo stack. `use` loads the pattern into the stamp and selects the stamp brush.
- In the 2D tab, the **Patterns** section lists the patterns (a button per pattern uses it, its
  Delete button removes it) and saves the stamp under the typed **Name** with **Save stamp as
  pattern**.

### Error codes added

| Code | Meaning |
|---|---|
| `mode2d_no_object` | The object layer has no object with that id. |
| `mode2d_no_pattern` | The tileset has no saved pattern of that name. |
| `mode2d_bad_patterns` | The tileset's `.tilepatterns.ron` cannot be read, parsed or written, the name is blank or too long, or a stamp tile is not in the tileset. A broken file is reported and never overwritten by a list. |

`mode2d_bad_params` also covers a bad `tool`, `mode`, `random`, `filled`, `contiguous`, guide or
object field; `mode2d_bad_layer` a layer that is not an object layer for an object action, or a
depth out of range.

## Recipe: scatter grass in a fenced field

```text
editor.mode2d_set {"active": true, "map": "assets/maps/farm.tilemap.ron", "layer": "ground", "brush": {"tile": 9}}
editor.tile_stroke {"tool": "rect", "filled": false, "cells": [[2, 2], [12, 8]]}           # the fence
editor.tile_stroke {"tool": "bucket", "cells": [[5, 5]], "random": {"tiles": [{"tile": 21}, {"tile": 22, "weight": 3}], "nothing": 0.4, "seed": 1}}
editor.tile_layers {"action": "object_add", "layer": "spawns", "object": {"name": "gate", "class": "door", "position": [112, 32], "size": [16, 16]}}
editor.undo {}                       # the object goes; the grass and fence stay
editor.tilemap_save {}
```
