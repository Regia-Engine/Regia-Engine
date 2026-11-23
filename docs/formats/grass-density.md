# `<map>.grassdensity.bin` (painted grass density, version 1)

Owner: `regia_terrain::grass_density` (`GrassDensityPaint`, ADR-2411). The saved form of grass density edits: one 8-bit
multiplier per texel over the whole map, `255` = the density the grass mask bakes (×1), `0` = no grass. Next to the map and
named like its holes file (`maps/switchbacks.level.ron` → `maps/switchbacks.grassdensity.bin`, `grass_density_path`); a
level that names its terrain (scene format v12, `LevelTerrain::map`) puts it under that map's name. Painted by the editor's
Terrain mode `0` and `terrain.grass_density_stroke`; written by every level save after an edit and by
`terrain.save_grass_density`; read by `regia_terrain::load_level_terrain` and `terrain.load_grass_density`
(`book/agents/level-terrain.md`).

| Field (postcard order) | Meaning |
|---|---|
| `version` | u32 varint, 1. Read before the body: any other version is refused (`BadVersion`) |
| `side` | u32 varint, texels per side, `2..=4096`; refused before the texels are read |
| `texels` | `side × side` bytes, row-major, row 0 at −Z, column 0 at −X; texel `(c, r)` covers world XZ `((c, r) / side − ½) · world_width` to the next texel |

A blob whose texel count is not `side²` is refused. The same image gives the same bytes.

**The brush.** A stroke (`GrassDensityStroke`: `path` in world XZ metres, `radius_m`, `hardness`, `strength`, `target`, all
checked) moves every texel whose centre lies within `radius_m` of the path toward `target · 255` by
`weight · strength`, where `weight` is 1 up to `hardness · radius_m` from the nearest point of the path, then a smoothstep down
to 0 at the radius. Each texel is written once per stroke from its value before the stroke, rounded to the nearest byte, in a
fixed order, so a stroke is a pure function of the image and the stroke. The undo record holds the touched rectangle's bytes
before and after.

**How the grass reads it.** The host passes `GrassDensityPaint::multipliers_for(set, base)` as
`GrassMaskInputs::multipliers`: the image for every class, multiplied into a class's biome multiplier when one exists. The
mask bake multiplies it into each class's density before quantization (`docs/formats/grass-ron.md`).

**Versioning.** Registered in `cargo xtask check-formats` (`grass-density`) and `regia upgrade-project` (reads the leading
postcard version). A layout change bumps `version` and ships a migrator.
