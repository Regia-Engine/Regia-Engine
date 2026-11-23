# 86 — 2D toolkit remainder: auto-tiling rules and the 2D editor mode

Status: proposed (ADR-1889). Version line: v0.4.0 "content creation" (`docs/versions.md`), plan row
"2D toolkit remainder (auto-tiling, 2D editor mode)" (`docs/master-plan.md`). Parent:
`docs/specs/22-tier2.md` § T2.10, whose 2026-10-08 implementation left three items open: auto-tiling
rules, 2D lights (still parked, § 9) and the 2D editor mode.

## 1. Purpose

A level designer paints a terrain ("grass", "water", "wall") onto a tile layer and the right edge,
corner and inner tiles appear by themselves; erasing a cell repairs its neighbours. The same
operation is available to code, to the agent protocol and to the editor, and it always produces the
same tiles for the same input. The editor gets a 2D mode: an orthographic view onto the map's
plane that shows the map's tiles, a tile grid, a hover cell, a 2D tab with a tile palette, and a
tile/terrain brush whose strokes are undoable and can be saved back to the `.tilemap.ron`.

The output of auto-tiling is an ordinary tile layer. Nothing about drawing, collision or the frozen
`.tilemap.ron` shape changes (it was v1 then; v2 since ADR-2597 adds the orientations of § 12): the rules are an authoring tool, applied when a cell is painted (by
the editor, an agent, or game code that builds levels at run time).

## 2. Model

- A **rule set** (`.autotile.ron`) is bound to one tileset of a map, by tileset name.
- A rule set holds one or more **terrains**. Each terrain owns a set of **member tiles** (local ids
  in that tileset): every tile its rules can output, its variants, and an optional extra `members`
  list. A cell belongs to terrain *T* when its tile is from the bound tileset and its local id is a
  member of *T* (flip bits are ignored). Member sets of the terrains of one rule set are disjoint.
- **Resolving** a cell that belongs to *T*: look at its 8 neighbours (row 0 is the top row, so north
  is `y − 1`; on staggered and hexagonal maps these are the geometric neighbors of § 13), compute which of them also belong to *T*, and pick the output tile from the terrain's
  **layout**. Cells off the map count as *T* when `border: Same` (the default; a terrain runs off the
  edge without an edge tile) and as not-*T* with `border: Empty`.
- **Painting** terrain *T* (or erasing) over a list of cells writes a member of *T* (or `0`) into
  every painted cell, then resolves every cell of any terrain in the painted cells and their
  8-neighbourhood. Because resolution reads only membership and every output is a member of the same
  terrain, the result does not depend on the order of cells or of strokes that set the same
  membership: painting A then B gives the same layer as B then A, and as re-resolving the whole
  layer afterwards. Where two strokes with different brushes (two terrains, or a terrain and erase)
  cover the same cell, the later stroke's brush sets that cell's membership (ADR-5817).

### 2.1 Layouts

| Layout | Mask | Tiles | Use |
|---|---|---|---|
| `Edge16([t; 16])` | 4 bits, N = 1, E = 2, S = 4, W = 8 | index = mask | Walls, pipes, simple coasts ("marching squares"-style edge set) |
| `Blob47([t; 47])` | 8 bits clockwise from north: N = 1, NE = 2, E = 4, SE = 8, S = 16, SW = 32, W = 64, NW = 128; a corner bit counts only when both edges next to it are set | index = position of the reduced mask among the 47 reduced masks in ascending order (0 → 0, 1 → 1, 4 → 2, 5 → 3, 7 → 4, …, 255 → 46) | Ground, water, any terrain with inner corners |
| `Rules([rule, …])` | 3×3 pattern | first matching rule wins; `fallback` (or the first rule's tile) when none matches | Anything the presets do not cover |

A pattern rule's `pattern` is nine cells, row by row, top row first (whitespace and `/` are ignored so
it can be written `"?#? / ### / ?.?"`): `#` = the neighbour belongs to this terrain, `.` = it does not,
`?` = either. The centre cell must be `#` or `?`. An output can carry flips (`flip_x`, `flip_y`,
`diagonal`, the GID flag bits of `.tilemap.ron`).

### 2.2 Variants

`variants: { 12: [(tile: 12, weight: 6), (tile: 40, weight: 1), (tile: 41, weight: 1)] }` replaces a
resolved tile with a weighted pick. The pick is a pure function of `(seed, x, y, base tile)`
(`mix64(mix64(seed << 32 | tile) ^ (x << 32 | y))` with `mix64` SplitMix64's output step, golden
gamma added and then the finalizer; then `hash % total_weight`; `variant_hash`, ADR-5817), so the same cell always shows
the same variant whatever the paint order, on every platform.

## 3. Rust API (`regia_2d::autotile`)

```rust
pub const AUTOTILE_FORMAT_VERSION: u32 = 2;   // v2: WangCorner (ADR-2597)

pub struct AutoTileSet {            // the `.autotile.ron` document
    pub version: u32,
    pub tileset: String,            // tileset name in the map
    pub seed: u32,                  // default 0
    pub border: Border,             // Same (default) | Empty
    pub terrains: Vec<Terrain>,
}
pub struct Terrain {
    pub name: String,
    pub layout: Layout,             // Edge16(Vec<u32>) | Blob47(Vec<u32>) | Rules(Vec<PatternRule>) | WangCorner(Vec<u32>) (§ 12)
    pub fallback: Option<u32>,      // Rules only
    pub members: Vec<u32>,          // extra member tiles
    pub variants: BTreeMap<u32, Vec<WeightedTile>>,
}
pub struct PatternRule { pub pattern: String, pub tile: u32, pub flip_x: bool, pub flip_y: bool, pub diagonal: bool }
pub struct WeightedTile { pub tile: u32, pub weight: u32, pub flip_x: bool, pub flip_y: bool, pub diagonal: bool }

impl AutoTileSet {
    pub fn from_ron(text: &str) -> Result<Self, AutoTileError>;   // version gate, migrate, validate
    pub fn to_ron(&self) -> Result<String, AutoTileError>;
    pub fn validate(&self) -> Result<(), AutoTileError>;
    pub fn terrain_index(&self, name: &str) -> Option<usize>;
    pub fn bind(&self, map: &TileMapAsset) -> Result<BoundAutoTiles<'_>, AutoTileError>;
}
impl BoundAutoTiles<'_> {
    pub fn terrain_at(&self, map: &TileMapAsset, layer: usize, x: u32, y: u32) -> Option<usize>;
    pub fn resolve(&self, map: &TileMapAsset, layer: usize, x: u32, y: u32) -> Option<u32>; // raw GID
    pub fn paint(&self, map: &mut TileMapAsset, layer: usize, brush: TerrainBrush, cells: &[(u32, u32)])
        -> Result<Vec<TileChange>, AutoTileError>;                 // TerrainBrush::Terrain(i) | Erase
    pub fn apply_layer(&self, map: &mut TileMapAsset, layer: usize) -> Result<Vec<TileChange>, AutoTileError>;
}
pub struct TileChange { pub x: u32, pub y: u32, pub before: u32, pub after: u32 }
pub fn blob47_index(mask: u8) -> usize;  pub const fn edge16_mask(north: bool, east: bool, south: bool, west: bool) -> usize;  // exposed for tools and tests
pub fn autotile_schema_fingerprint() -> String;  pub fn autotile_migration_sources() -> Vec<u32>;
```

`TileChange` lists only cells whose raw GID changed, sorted by `(y, x)`; applying `before` values in
reverse restores the layer bit for bit (the editor's undo uses it).

Run time: `regia_2d::AutoTiles` (a resource: rule sets by name, `insert`, `get`; `Sprite2dPlugin`
adds it) and the existing `SpriteAssets::tilemap_mut(path)` (copy-on-write: tileset images stay
shared, `revision` bumps, the sprite pass draws the new tiles the next frame). In-memory edits are
lost on a hot reload of the map's source, as any runtime edit of a loaded asset.

## 4. Format: `.autotile.ron` (v1, now v2)

Frozen from this ADR on (`specs/12` § 4a): `format_version` is the `version` field (1 at first; 2
since ADR-2597, which added `WangCorner`). A newer version is refused before the body is parsed; the
migration chain has one step, `VersionStep { from: 1, .. }` (a v1 file is a valid v2 file), and
`regia upgrade-project` rewrites the version number. `cargo xtask
check-formats` hashes the shape (`autotile_schema_fingerprint`); `regia upgrade-project` lists the
format. Reference: `docs/formats/autotile-ron.md`, example `docs/formats/autotile.example.ron`.

Limits (validated): ≤ 256 terrains, ≤ 1024 rules per terrain, ≤ 64 variants per tile, weights 1 ..=
65 535, names ≤ 256 bytes, ≤ 65 536 extra `members` or variant keys per terrain, unique terrain names, disjoint member sets, layouts of exactly 16 / 47
entries. `bind` checks that the tileset exists in the map and that every tile id is `< tile_count`.

**Cook.** `.autotile.ron` is an authoring file: the editor reads it from the project, games that
paint at run time embed it (`include_str!` + `AutoTileSet::from_ron`) or ship it as a plain file.
A cooked form is out of scope for v1 (§ 9).

## 5. Agent-protocol surface

Runtime methods (`regia_agent`, `book/agents/2d.md`, Permission `EditScene`, mutating; the two that only read,
`tilemap.autotile_list` and `tilemap.stamp_copy` (§ 12), are `Read` and non-mutating, ADR-5817):

| Method | Params | Result |
|---|---|---|
| `tilemap.set_tile` | `entity?`, `layer`, `x`, `y`, `gid` (0 = erase), `flip? {x,y,diagonal}` | `{ cell, layer, before, after }` |
| `tilemap.autotile_list` | — | `{ rule_sets: [{ name, tileset, border, seed, terrains: [{ name, layout, rules, members, variants }] }] }` |
| `tilemap.autotile_register` | `name`, `ron` (`.autotile.ron` text, ≤ 1 MiB; longer is refused by the method contract with `F0002`) **or** `path` (cooked, § 12) | `{ name, terrains, replaced, path? }` |
| `tilemap.paint` | `entity?`, `layer`, `rules`, `terrain` **or** `erase: true`, `cells: [[x, y], …]` **or** `rect: [x0, y0, x1, y1]` | `{ count, truncated, changed: [{ cell, before, after }] }` (at most 1 000 listed) |
| `tilemap.autotile` | `entity?`, `layer`, `rules` | `{ changed: count }` (re-resolves every terrain cell of the layer) |

Errors carry `data.code` (with the JSON-RPC code `-32602`, or `-32001` for `tile_unknown_rules`), one of `tile_bad_params`,
`tile_not_tile_layer`, `tile_outside_map`, `tile_bad_gid`, `tile_unknown_rules`,
`tile_unknown_terrain`, `tile_bind_failed` (also a layer whose data is not `width × height` long), `tile_bad_rules`, `tile_too_many_cells` (≤ 65 536 cells a call). All are
listed with their meaning in the book page.

Editor methods (`regia_editor`, `book/agents/editor-2d.md`):

| Method | Params | Result |
|---|---|---|
| `editor.mode2d_get` | — | `{ active, map, rules, layer, brush, grid, hover, origin, view, clock, undo_depth }`; `clock` is `{ playing, time_ms }` (§ 13); `map.tilesets` lists `name`, `first_gid`, `tile_count`, `columns`, `image`, `loaded`, `error`; `view` is `{ tiles, stride }` of the front pane's last frame |
| `editor.mode2d_set` | `active?`, `map?` (path of a `.tilemap.ron`), `discard?`, `layer?`, `rules?` (path of an `.autotile.ron`, `null` clears), `brush?` (`{tile: gid, flip?}` / `{terrain: name}` / `{erase: true}`), `grid?`, `clock?` (`{ play?, reset?, step_ms? }`, § 13), `reload_images?`, `origin?` | the new state |
| `editor.tile_stroke` | `cells: [[x, y], …]` | `{ changed, undo_depth }`: one undo entry for the whole stroke |
| `editor.tilemap_save` | `path?` | `{ path, bytes }`: atomic write of the edited `.tilemap.ron` |

The editor's undo stack (`editor.undo` / `editor.redo`) undoes and redoes strokes exactly. Editor
errors are `F0002` with `data.mode2d_error` (the editor-extension convention): `mode2d_bad_params`,
`mode2d_no_map`, `mode2d_no_rules`, `mode2d_unknown_terrain`, `mode2d_bad_layer`,
`mode2d_outside_map`, `mode2d_bad_gid`, `mode2d_not_tilemap_ron`, `mode2d_load_failed`,
`mode2d_bad_rules`, `mode2d_save_failed`, `mode2d_unsaved`.

## 6. Editor 2D mode

`regia_editor::mode2d`. Turning the mode on loads the map document from its `.tilemap.ron` (and the
rule set, when named), switches the focused viewport to the front orthographic view (looking down
−Z: +X right, +Y up, the 2D world plane), and frames the map. With the mode on, left-drag paints with
the brush (tile, terrain, erase), right-drag pans and the wheel zooms as in any ortho pane; a stroke
(press → release) is **one** undo entry, on the layer it started on; a fast drag paints every cell on
the line between two pointer samples (Bresenham). The viewport draws the document's tiles, then the
map bounds, the tile grid (when a tile is at least 4 px on screen), the hover cell and a
half-transparent preview of the tile brush. Hotkeys with the viewport focused: `B` toggles the
mode; with it on, Shift held while painting erases (`E` stays the rotate gizmo), `,` / `.`
previous / next layer, `[` / `]` previous / next terrain. Saving is `editor.tilemap_save`; saving
the level (File → Save level, Save level as…, the save on close, `editor.save_level`) also writes
the map when it has unsaved edits (`tilemap_saved` in the `editor.save_level` answer). Only
`.tilemap.ron` sources load: Tiled sources (`.tmx`, `.tmj`) are edited in Tiled. Loading another map over unsaved edits needs `discard: true`.

**The 2D tab** (`mode2d::panel`, `Panel::Mode2d`, titled "2D"; `B` focuses it when no map is loaded)
draws the state the agent methods change: the mode and grid switches, Open map…, Save map, Reload
images, Open rules…, Clear rules, the tile layers, Erase, the terrains, the flip switches and a palette
of the tileset's tiles. Opening a map over unsaved edits asks: Save and open, Discard and open, or
Cancel.

**Tile images** (`mode2d::tiles`). The sprite pass renders into the game's frame, not into the editor
pane, so the pane draws the document itself with textured egui meshes, one per layer and tileset,
over the 3D frame. Each tileset's image is decoded once on load (`image`, relative to the map file)
and uploaded with nearest filtering. Placement and texture coordinates are those of the sprite pass
(`sprite2d.wgsl`): flip bits, layer offset, layer opacity, tiles larger than the grid anchored at the
cell's bottom-left. Animated tiles show the frame of the pane's clock (§ 13): a paused clock at 0 shows the first
animation frame, which can differ from the cell's base tile. An image that is missing, broken or larger
than the GPU's texture limit is reported in `map.tilesets[].error`, and its tiles draw in flat colors
per tile id, so painting still works. An image changed on disk comes back with Reload images
(`reload_images`).

## 7. Determinism

Resolution and variant picks are integer-only functions of membership, positions and the seed; no
floating point, no hash-map iteration order, no clock. `paint` and `apply_layer` produce identical
layers on Linux, Windows, Android aarch64 and macOS. Tests pin exact output layers.

## 8. Tests and acceptance

1. `blob47_index`: exactly 47 reduced masks, 0 → 0, 255 → 46, reduction drops lone corners.
2. Edge16 and Blob47 over hand-checked maps (a 3×3 block, a ring, a single cell, a line) give the
   pinned tile ids; `border: Empty` adds edges at the map boundary.
3. Pattern rules: first match wins, `?` / `.` / `#` semantics, fallback, flips land in the GID bits.
4. Order independence: random stroke orders (fixed seeds) give byte-identical layers, equal to
   `apply_layer` on the final membership.
5. Erase repairs the neighbours; `TileChange` before-values restore the layer exactly.
6. Variants: the pick depends on `(seed, x, y)` only, the distribution follows the weights over a
   large region within a fixed tolerance (counted, not timed).
7. Format: RON round trip; newer version refused; validation errors (layout size, overlapping members,
   duplicate names, bad pattern, zero weight, limits); `bind` errors (unknown tileset, id out of range);
   the published example parses.
8. Agent: each method's success path and each `tile_*` error code (unit tests on a `World`); contract
   rows and coverage calls.
9. Editor: stroke → one undo entry, undo/redo restore the layer; save writes a file that parses back
   equal; cursor → cell through the front view's matrix lands on the right cell; a layer change
   during a drag leaves the stroke on its layer.
10. Tile images: corner UVs for every flip against pinned values; a quad's corners equal the
    `project` of its cell, and the pick under its centre is that cell; culling, hidden and transparent
    layers, opacity, layer offset, oversized tiles; a 1024 × 1024 layer stays inside the quad budget;
    a missing or broken image is reported, not fatal. End to end (`tests/mode2d_view.rs`, headless
    adapter, no golden): tiles painted through the agent methods show their tileset's colors in the
    front pane of the real editor window, on the correct sides.
11. Input (`mode2d/input_tests.rs`, scripted events through an `egui::Context`, no window): a click
    paints one cell; a drag fills the line as one undo entry and paints the cell the button went down
    on even when the first sample is cells away; Shift erases and Alt paints nothing; leaving the pane
    mid-drag joins the line on return, and a release outside it ends the stroke; leaving the mode or
    losing the pane mid-drag keeps the undo entry; the status line sits on an opaque backing with at
    least 4.5:1 contrast. The 2D tab's
    palette, flips, Erase, Grid, 2D mode, layers, Save map and the unsaved-edits question respond to
    clicks, and a long palette scrolls to its last row with no drift.

## 9. Out of scope

- 2D lights (parked by `specs/22` § T2.10).
- Sprites and entities of a 2D scene other than the edited map inside the editor pane (the pane
  draws the map's tiles, animated by its clock, § 13; Play shows the rest).

Built since the first pass (§ 12, ADR-2597): Wang corner sets, isometric, staggered and hexagonal
maps, cooked `.autotile.ron`, Tiled's Wang sets, and multi-cell stamps; since ADR-2714 (§ 13): the pane
clock for animated tiles, geometric auto-tile neighbors, Tiled objects in world space and XML
`<terraintypes>`.

## 10. Platforms

Pure Rust, no new dependency, no platform code: Linux, Windows and Android aarch64 build the runtime
part (`regia_2d`, `regia_agent`); the editor part is desktop only (Linux, Windows; macOS for testing).

## 11. Performance budget

Painting is authoring work, not per-frame work: `paint` touches the painted cells plus their
8-neighbourhood (≤ 9× the cells); a 64 × 64 stroke resolves ≤ 4 356 cells, each 8 lookups. A whole
1024 × 1024 layer `apply_layer` is ~1 M resolutions (one pass, no allocation per cell); `tracing`
spans `autotile.paint` and `autotile.apply_layer` cover both. The runtime frame cost is unchanged.

The editor pane visits only visible cells and draws at most 200 000 quads a frame
(`MAX_VIEW_QUADS`); past that, on orthogonal maps, it draws every Nth cell N times larger, with
sampled cells on multiples of N so they do not swim while panning. Isometric, staggered and hexagonal
maps are not stride-sampled: they stop at the budget (ADR-2597 item 9). The span `mode2d.paint_overlay` covers it.

## 12. Orientations, Wang corner sets, cooked rules and stamps (ADR-2597)

**Orientations.** `.tilemap.ron` v2: `orientation` is `Orthogonal`, `Isometric`,
`Staggered(axis, index)` or `Hexagonal(axis, index, side_length)`. `regia_2d::grid` converts between
cells and map pixels (+Y down) for each; `TileMapAsset` wraps it for the world (+Y up, origin at the
bottom-left of the map's box): `pixel_size`, `tile_world_rect` (the cell's bounding box),
`tile_world_outline` (4 or 6 points), `tile_world_center`, `cell_at_world` (the cell whose diamond or
hexagon holds the point). The map box is `w·tw` by `h·th` (orthogonal) and `(w + h)·tw/2` by
`(w + h)·th/2` (isometric). A stagger or hexagon with axis Y is `w·tw` by `(h − 1)·(th + s)/2 + th`,
plus `tw/2` on the width only when a shifted row exists: index Odd shifts a row when `h > 1`, index
Even when `h > 0`. Axis X swaps the roles (the extra half tile is `th/2` on the height, and the shift
test uses `w`). `s` is the hexagon side length (0 for staggered diamonds). A one-row Odd stagger
therefore has no half-tile overhang. Tile images
draw from the bottom-left of the cell's box. Draw order is back to front: row by row, left to
right, except column-staggered maps, which go by the top edge of the cell. The Tiled importer reads
`orientation`, `staggeraxis`, `staggerindex` and `hexsidelength` (`.tmx` and `.tmj`).

`TileMapAsset::pixel_size` rounds fractional box extents upward to whole pixels (saturating at
`u32::MAX`). `tile_world_rect` returns `f64` corners; tile batching keeps double precision through
the entity transform and culling, then converts to the GPU's `f32` coordinates (ADR-4634). A map
with far-away cells must be translated/rebased near the world origin to retain pixel detail when
rendered; absolute GPU world coordinates and camera positions still have `f32` precision.

**Wang corner sets.** `Layout::WangCorner([u32; 16])` (`.autotile.ron` v2). `regia_asset_pipeline::
tiled_wang` imports Tiled's corner, edge and mixed Wang sets (and JSON terrains) as `WangCorner`,
`Edge16` and `Blob47` terrains; see ADR-2597 for the slot mapping and the nearest-mask fill.

**Cooked rules.** `regia cook` stores each `.autotile.ron` as a `CookedAutoTileSet`;
`AutoTiles::load_from_server` / `tilemap.autotile_register { path }` read it from the pak.

**Stamps.** `regia_2d::stamp::Stamp` (copy, rotate and mirror with flip bits, clipped paste). Runtime
agent methods `tilemap.stamp_copy`, `tilemap.stamp_paste`; editor method `editor.tile_stamp`, the
`Stamp` brush, Ctrl-drag copy and the 2D tab buttons.

**Tests.** `grid/tests.rs`: center round trip for every orientation and stagger setting over a grid of
cells; sample points against the drawn outlines (no overlaps, every pick inside its outline); map box
against cell boxes; draw order; culling ranges cover every intersecting cell. `autotile/tests.rs`:
Wang corner masks on a block, order independence, validation, the v1 file through the migration.
`stamp/tests.rs`: flip bits against a pixel picture for all eight flag sets and all ops, composition
laws, clipped paste, errors. `tests/tilemap_orientations.rs` (asset pipeline): hand-written `.tmj` /
`.tmx` / `.tsx` / `.tsj` fixtures import to the expected cells, orientations and Wang layouts; the
cooked rules load equal to the raw file.

## 13. Pane animation clock, geometric neighbors, object space and old terrains (ADR-2714)

**Pane clock.** `regia_editor::mode2d::AnimClock { playing, elapsed }` is the pane's own clock
(`Mode2d::clock`). It moves only by an explicit `Duration`: `tick(dt)` while playing (the pane feeds
it the frame time), `step(dt)` unconditionally (agents, tests), `reset()`. The pane shows an animated
tile with `regia_2d::batch::animated_tile(set, local, clock.time_ms())`, the function the sprite
pass calls with `Time::elapsed().as_millis()`, and `time_ms()` is the same whole-millisecond
conversion; after the same fixed steps the pane and a running game show the same frame. Play/pause
and Rewind are in the 2D tab; agents use `editor.mode2d_set { clock: { play?, reset?, step_ms? } }`
and read `clock: { playing, time_ms }` from `editor.mode2d_get`. Applied order: reset, step, play.

**Geometric neighbors.** `Orientation::neighbor_cells(x, y)` returns the nine pattern slots
(`NW N NE / W C E / SW S SE`) of the auto-tile rules as signed grid coordinates, per geometry:

| Geometry | Neighbors |
|---|---|
| Orthogonal, isometric | the 8 grid neighbors (isometric: the grid's compass, rotated 45° on screen) |
| Staggered, axis Y | `W` `E` = `(x∓1, y)`; `N` `S` = `(x, y∓2)`; diagonals `(x+d, y∓1)` with `d ∈ {−1, 0}` on `west` and `{0, +1}` on `east`: shifted rows `NW`,`SW` = `x`, `NE`,`SE` = `x+1`; unshifted rows `NW`,`SW` = `x−1`, `NE`,`SE` = `x` |
| Staggered, axis X | the same with x and y swapped: `N` `S` = `(x, y∓1)`; `W` `E` = `(x∓2, y)`; shifted columns `NW`,`NE` = `y`, `SW`,`SE` = `y+1`; unshifted `NW`,`NE` = `y−1`, `SW`,`SE` = `y` |
| Hexagonal, axis Y (pointy) | the six edge neighbors: the staggered diagonals plus `W` `E`; no `N` `S` |
| Hexagonal, axis X (flat) | the six edge neighbors: the staggered diagonals plus `N` `S`; no `W` `E` |

A shifted row (axis Y) or column (axis X) is an odd one for `StaggerIndex::Odd` and an even one for
`Even` (rows and columns count from 0). Hexagons with side length 0 are staggered diamonds. A slot with
no neighbor never matches: `#` fails and `.` holds. `Edge16`, `WangCorner` and `Blob47` read the same
slots (N E S W / the corners / all eight), so on hexagons they are meant for `Rules` terrains; the slot
map keeps them well defined. `paint` re-resolves the painted cell's neighbors in the same set (the
relation is symmetric). A rule set is the same on every orientation; only the neighbor lookup differs.

**Object space.** Tiled stores object positions in map pixels on orthogonal, staggered and
hexagonal maps, and in grid units scaled by the tile height on isometric ones.
`Orientation::object_to_map_pixels(grid, p)` (and `map_pixels_to_object`) converts; polygon points,
absolute, convert one by one (the map is affine). `TileMapAsset::object_world_position(&MapObject)`
returns the map-local world position (+Y up, origin at the map box's bottom-left). The `.tilemap.ron`
object stays as Tiled wrote it (no format change); tile objects keep the bottom-left anchor.

**Old terrains.** Tiled 1.0 XML `<terraintypes>` (plus each tile's `terrain="tl,tr,bl,br"`) in a
`.tsx`, an external tileset of a `.tmx`, or an embedded one, import as the equivalent `corner` Wang
set: terrain `i` is color `i + 1`, the four corner indices fill the corner slots. The result equals the
`WangCorner` terrain of the hand-written Wang set (`tiled_objects_terraintypes.rs`).

**Tests.** `grid/tests.rs`: the neighbor table for odd and even rows and columns, both indices, both
axes; symmetry and on-screen closeness for every orientation. `autotile/tests.rs`: an auto-tile result
on small pointy and flat hex maps (differs from the grid result), paint equals apply, undo.
`mode2d/tests.rs`: after N fixed steps the pane's tile frame equals the runtime's (`regia_app::Time`),
pause, step, loop, the `clock` parameter. `tiled_objects_terraintypes.rs`: isometric, staggered and
hexagonal object positions to world coordinates from tmx built in the test; terraintypes equal the
Wang set (terrain, layout, painted block), in a tsx, an external and an embedded tileset.

## As built (ADR-7742): new maps, tileset binding, the rule editor, keyboard

`mode2d::tilemap_new` creates maps (Content ▸ New ▸ Tile map, the 2D tab's **New tile map**, `editor.tilemap_new`)
at the current `.tilemap.ron` version through `TileMapAsset::to_ron`, with tilesets bound from an image or from
another map, and edits the binding and layer order (`editor.tilemap_tilesets`: add, remove, replace, move,
layer_move; tilesets renumber from GID 1 and every tile is remapped; one undo entry each). `mode2d::autotile_editor`
edits one `.autotile.ron` (open, New, save, dirty, undo; ops validated by `AutoTileSet::validate`): rules with a 3x3
neighbor grid, output tiles and flips, add, remove, reorder, preset layout entries, terrains; the preview paints a
scratch map with `BoundAutoTiles::paint`. The palette and a map cursor work from the keyboard. Tests:
`mode2d/tilemap_new.rs` (canonical bytes, runtime load, paint, remap, undo), `mode2d/autotile_editor.rs` (edits then
preview equal the runtime auto-tiler, the 2D brush follows), `mode2d/keys_tests.rs` (synthetic keys paint a cell).
Agent page `book/agents/editor-2d.md`.
