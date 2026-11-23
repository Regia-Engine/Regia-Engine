# Level terrain, grass density and scatter rules

The terrain editing remainder (ADR-2411, `docs/specs/82-terrain-editing.md` § 14): a level names its terrain (scene
format v12), a grass density brush with its own saved file, and `.scatter.ron` rule editing. Heightmap export and import
are `terrain.export_heights` / `terrain.import_heights` on the [terrain editing](terrain-editing.md) page; the editor's
Terrain panel has buttons for them, and the height scaling is in `docs/formats/terrain.md` § 11.

## The level names its terrain

A v12 `.level.ron` may carry `LevelSettings::terrain`:

```ron
terrain: Some((
    layers: "maps/arena/terrain.layers.ron", // its .meta id is the map id of the cooked heightfield
    map: "maps/arena",                       // side files: maps/arena.holes.ron, .weights.bin, ...
    world_width_m: Some(512.0),              // None: the cooked terrain index's size_m
    height_scale_m: 256.0,                   // metres from sample 0 to sample 65535
    height_offset_m: 0.0,                    // the height of sample 0
    sample_layout: VertexCentered,           // v14: or CellCentered (samples at cell centers)
)),
```

Since scene format v14 (ADR-10079) the block has `sample_layout`: `VertexCentered` (the default, and what a migrated v13
level gets) puts the first and last samples on the edges; `CellCentered` reads `world_width_m` as `n` equal cells and puts
each sample at its cell's center (see [terrain runtime](terrain-runtime.md#sample-layout)). A count-mapped terrain refuses
`CellCentered` with `level_terrain_mapping`.

An empty `map` names the side files after the level itself. A level with no `terrain` (every migrated v11 level) names
none: the host supplies the terrain and the side files keep the level's name, as before v12, so an old level loads the
same terrain. The editor builds the named terrain when the level opens and gives it a cook target, so a sculpt saves into
the cooked heightfield it came from; a game adds `regia_terrain::LevelTerrainPlugin` and sets `LevelTerrainRoots`.
Every platform's cook writes that heightfield: Android cooks the same tiled terrain as desktop, with ASTC 4×4 texture
tiles instead of BCn (ADR-5294, which replaced ADR-4914's heightfield-only Android index), so the same level loads the
same heights there.

A terrain whose `heightfield.json` carries a schema-2 count mapping (ADR-8471, `docs/formats/terrain.md` § 9a) loads from
that mapping instead: heights are `(count - zero_counts) * meters_per_count` and samples sit at the source's registration
(`sample0 + (col, row) * cell_m`, the map's grid origin at the grid's center). `height_scale_m` and `height_offset_m` do
not apply; `world_width_m` must be absent or equal `(cols - 1) * cell_m`. A mapping the runtime map cannot hold exactly is
refused with `level_terrain_mapping`, never resampled.

| Method | Params | Reply |
|---|---|---|
| `terrain.level_terrain` | `level` (optional, relative to `assets/`) | `{ terrain, files, loaded, status }`: the block, the files it resolves to (each `{ path, exists }`), the level terrains in the world (`entity`, `level`, `digest`, `map_id`) and the plugin's last status |
| `terrain.set_level_terrain` | `terrain`: the block (missing fields take their defaults) or `null` | `{ terrain, changed }`; the next level save writes it |
| `terrain.load_level_terrain` | `level` | `{ entity, replaced, digest, map_id, cooked_heightfield, files, source_mapping }`; replaces an earlier level terrain; `entity` is `null` when the level names no heightfield; `source_mapping` is the cooked count mapping the terrain was built from (`null` for a legacy terrain, ADR-8471) |

The `digest` is FNV-1a 64 over the heightfield samples and mapping, the holes, the painted weights and the grass density:
two loads of the same terrain give the same digest.

```json
{ "method": "terrain.set_level_terrain", "params": { "terrain": { "layers": "maps/arena/terrain.layers.ron", "height_scale_m": 120.0 } } }
{ "method": "editor.save_level", "params": {} }
{ "method": "terrain.load_level_terrain", "params": { "level": "maps/arena.level.ron" } }
```

## Grass density

`<map>.grassdensity.bin` (`docs/formats/grass-density.md`) holds one multiplier per texel over the map: 1 is the baked
grass density, 0 no grass. A stroke moves the texels under the brush toward `target` by the brush weight times `strength`.
It is a pure function of the image and the stroke, so the same strokes give the same bytes; undo restores the bytes
exactly. Hosts bake grass with `GrassDensityPaint::multipliers_for(set, base)` as the mask's `multipliers`.

In the editor, Terrain mode `0` paints it: `-` `=` move the target in steps of 0.1, `[` `]` the size, Shift paints toward 1
(restores grass). A drag is one undo entry, and every level save writes the file after an edit.

| Method | Params | Reply |
|---|---|---|
| `terrain.grass_density_stroke` | `path` `[[x, z], …]` or `center` `[x, z]`, `radius` (8), `hardness` (0.5), `strength` (1), `target` (0), `texel_m` (1, for a new image), `entity` | `{ entity, side, painted, digest, changed, undo_depth }` |
| `terrain.undo_grass_density` | none | `{ entity, stepped, side, painted, digest, undo_depth, redo_depth }` |
| `terrain.redo_grass_density` | none | as undo |
| `terrain.grass_density` | `entity` | `{ entity, side, painted, digest }` |
| `terrain.save_grass_density` | `path` (a map or level file, or a `.grassdensity.bin` name), `entity` | `{ entity, path, side, bytes }` |
| `terrain.load_grass_density` | `path`, `entity` | `{ entity, path, side, painted, digest }` |

The agent's undo stack holds up to 256 strokes (and 256 MiB of records) and is separate from the editor's undo stack, so
in the editor undo agent strokes with `terrain.undo_grass_density`, not Edit ▸ Undo; loading a file drops the agent's
records for that terrain. Every stroke, from the editor or the agent, marks the image unsaved, so the next level save
writes it. A stroke may cost at most 2²⁸ texel checks: the axis-aligned texel box of the path expanded by the radius, times the path segments (one for a single point). Split a long path or use a smaller
radius (`grass_density_bad_stroke`). When the open level's density file exists and cannot be read, the editor refuses
density edits and never replaces the file. The same refusal blocks `terrain.grass_density_stroke`: the call answers
`grass_density_unreadable` and paints nothing.

## Scatter rules

| Method | Params | Reply |
|---|---|---|
| `terrain.scatter_rules` | `path` (a `.scatter.ron`) | `{ path, version, seed, classes: [{ name, fields }] }` |
| `terrain.set_scatter_rule` | `path`, `class`, `field`, `value` | `{ path, class, field, value, bytes }`; the file is saved as canonical RON, atomically |

The value must have the field's kind (a number for a number), and the set must still validate. `name` and `meshes` are not
rule fields. The editor's Terrain panel (Scatter rules) edits the same fields with a list of classes, add and remove, and a
Save button.

## Error codes

| Code | When |
|---|---|
| `level_terrain_invalid` | A path in the block or the level path leaves `assets/` (absolute, `..`, a drive), `layers` is not a `.terrain.layers.ron`, `world_width_m` or `height_scale_m` is not finite and above 0, or `height_offset_m` is not finite (`0` is valid) |
| `level_terrain_read` | The layer file, its `.meta` or the cooked heightfield is missing or cannot be read |
| `level_terrain_decode` | A named file does not parse: the layer file, the `.meta`, the cooked heightfield, a holes, weights or density file, or weights of another grid size |
| `level_terrain_mapping` | The terrain's cooked count mapping (a schema-2 `heightfield.json`, ADR-8471) cannot load exactly into the runtime map: a rectangular grid, a grid center that differs on X and Z, sample positions or exported counts that would move. Or the level's `world_width_m` is not the mapping's `(cols - 1) * cell_m`. Nothing is resampled; `terrain.inspect_source { mapping }` reports `runtime_map.loadable` before a cook |
| `grass_density_bad_stroke` | A stroke field is out of range (an empty or non-finite path, a radius outside (0, 4096], hardness, strength or target outside 0..1), or the axis-aligned texel box of the path expanded by the radius, times the path segments, would cost more than 2²⁸ texel checks |
| `grass_density_unreadable` | The editor has marked the open level's density file unreadable, so the stroke paints nothing |
| `grass_density_none` | The terrain has no painted grass density yet |
| `grass_density_write` | The density file cannot be encoded or written |
| `grass_density_read` | The density file is missing or cannot be read |
| `grass_density_decode` | The density file is not version 1, its side is outside 2..4096, or its texel count is wrong |
| `scatter_rules_read` | The path is not a `.scatter.ron`, or it cannot be read |
| `scatter_rules_invalid` | The file does not parse, or the edited set does not validate |
| `scatter_rules_field` | No such class or field, a value of another kind, or `name` / `meshes` |
| `scatter_rules_write` | The file cannot be written |
| `F0003` | No terrain, or (save, load, scatter) no project root |

## Scatter classes from an agent (ADR-7307)

`editor.scatter_class { path, add?, remove? }` does the Terrain panel's Add class and Remove on a
project-relative `.scatter.ron`: `remove` drops the named class, then `add` appends a copy of the first
class under the new name, and the file is saved as canonical RON. Returns `{ path, classes }`. Field
edits stay `terrain.set_scatter_rule`. Errors: `scatter_class_bad_params` (no `path`, or neither `add`
nor `remove`), `scatter_class_bad_path` (absolute or `..`), `scatter_class_unreadable`,
`scatter_class_unknown` (no such class), `scatter_class_refused` (the name is taken or empty, or the set
no longer validates).
