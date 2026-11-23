# Terrain formats (frozen v4 layers, v2 scatter, v1 water/deform)

All terrain asset formats listed below use the RON (Rusty Object Notation) encoding. The layers, scatter and holes files
begin with a `version` field (water planes and craters/scorch lists have none); loaders reject unknown versions with a clear error (spec 04 § 7).
The formats are frozen per spec 26 § 1 and spec 12 § 4a. The layers format is at **v4**. A v1
file migrates v1 → v2 (texture slots and blend knobs), v2 → v3 (`orm_texture: None`), then v3 → v4
(the weather response: `weather: None` per layer, `puddles: true`). The defaults render the
previous look. `regia upgrade-project --write` re-saves older files at v4.

## 1. Terrain layers (`.terrain.layers.ron`)

```ron
TerrainLayers(
    version: 5,
    wetness_default: 0.0,        // 0..=1 default wet/dry
    layers: [
        LayerDesc(
            name: "ground",
            albedo: (0.18, 0.28, 0.10),    // linear RGB, 0..=1 (tint when albedo_texture is set)
            roughness: 0.95,                 // 0..=1, used when orm_texture is None
            tiling: 8.0,                     // repeats per metre (world.xz * tiling)
            slope_min: 0.0,                  // radians from up, inclusive
            slope_max: 0.6,                  // radians from up, exclusive
            altitude_min: -1000.0,           // metres
            altitude_max: 1000.0,            // metres
            blend_sharpness: 0.15,           // 0.01..=0.5
            triplanar: false,
            // v2 additions (a migrated v1 file carries exactly these defaults):
            albedo_texture: None,            // asset path of a tiling RGBA8 sRGB albedo
            normal_texture: None,            // asset path of a tiling tangent-space normal map
            height_texture: None,            // asset path of a tiling height map (R channel)
            detail_normal_strength: 0.0,     // 0..=2 micro-normal detail (0 = off, distance-faded)
            height_blend: 0.0,               // 0..=1 height-blended transition sharpness
            // v3 addition (a migrated v2 file carries None):
            orm_texture: None,               // glTF ORM: R occlusion, G roughness, B metallic
            // v4 addition (a migrated v3 file carries None): how rain and snow change the layer
            // (spec 99 § 7.3). `Some(())` is the same as None.
            weather: Some((
                porosity: Some(0.9),         // 0..=1 how much the wet layer darkens; None: from roughness
                snow: 1.0,                   // 0..=1 scales the snow cover (0: snow never settles here)
                // v5 addition (a migrated v4 file carries false): with no `porosity`, read each
                // texel's porosity from the ORM map's blue channel (0 sheds a film, 1 soaks).
                porosity_from_orm: false,
            )),
        ),
    ],
    // v4 addition (a migrated v3 file carries true): flat ground may hold puddles in rain.
    puddles: true,
)
```

- Up to 8 layers (constant `MAX_LAYERS`).
- Layers are applied bottom (index 0) to top; weights are normalised.
- `triplanar: true` enables box-projected texture sampling on cliff faces; steep slopes switch
  to triplanar projection automatically even when the flag is off.
- `height_blend > 0` sharpens layer transitions using the per-layer height (texture or
  procedural), so layers interlock along height contours instead of fading uniformly.
- `weather` (v4) applies only while the weather is wet or snowy (`config/weather.ron`
  `effects.surface`, spec 99): a dry frame is unchanged by it. Each layer's porosity (its own,
  or the one its roughness gives, `clamp((roughness - 0.25) / 0.5, 0, 1)`) and snow scale blend
  by the layers' slope, altitude and splat weights; the height blend and the procedural mottle
  are not repeated for this. `puddles: false` keeps the whole terrain to a wet film (a desert,
  a scree field). Out-of-range values are refused and named (`weather.porosity`,
  `weather.snow`).
- `porosity_from_orm` (v5, ADR-3367) reads a layer's porosity per texel from the blue channel
  of its `orm_texture`, with the colour pass's own ORM tap, where the block names no `porosity`
  (a named one wins). It is off by default because an ORM map's blue channel is glTF metallic:
  maps authored that way are never reinterpreted. Terrain is not metallic, so the blue channel
  is otherwise unread. A layer without an ORM map keeps the porosity of its roughness. Only wet
  frames read it. A v4 file migrates by its version alone (`regia upgrade-project --write`).
- File name: any name that ends in `terrain.layers.ron`, in any letter case (`terrain.layers.ron`,
  `ground.terrain.layers.ron`, `Cliff.Terrain.Layers.ron`). The cook, the live loader and
  `regia upgrade-project` all use this one rule (ADR-7978), so a map folder's plain
  `textures/terrain.layers.ron` migrates like a named one.

## 2. Scatter (`.scatter.ron`, format v2)

One file per map. A set of classes (boulders, talus, gravel, debris); each class places its own
weighted list of glTF meshes with its own density rules and look. Example (the Switchbacks
stand-in, `samples/lookdev/scatter/mars.scatter.ron`, trimmed):

```ron
ScatterSet(
    version: 2,
    seed: 2747,                                   // u64; same seed + map + file ⇒ same rocks everywhere
    classes: [
        ScatterClass(
            name: "boulders",                     // unique; also seeds the class's random stream
            meshes: [                             // ≥ 1; picked by weight
                ScatterMeshRef(mesh: "props/rocks/mars_block_a.glb", weight: 3.0,
                               min_scale: 0.35, max_scale: 2.4, scale_bias: 3.0),
                ScatterMeshRef(mesh: "props/rocks/mars_slab_a.glb", weight: 1.0,
                               min_scale: 0.5, max_scale: 2.8, scale_bias: 3.5),
            ],
            cell_size: 7.0,                       // metres, 0.25..; determinism is per cell
            density: 0.9,                         // expected candidates per cell (fraction = probability), ≤ 64
            density_map: None,                    // Some("maps/switchbacks_rocks.png"): grayscale over the whole terrain
            noise_scale: 90.0,                    // metres per patch of the procedural density noise (0 = off)
            noise_amount: 0.75,                   // 0..1, how bare the sparse patches get
            slope_min: 0.0, slope_max: 0.75,      // radians from up
            height_min: -100000.0, height_max: 100000.0,  // metres
            layer_mask: 0,                        // bit i = terrain layer i; 0 = any layer
            layer_min_weight: 0.3,                // summed weight of the masked layers
            avoid_holes: true,                    // tunnel mouths
            exclusions: [                         // circles (1 point) or paths (≥ 2 points) with a clearance
                ScatterExclusion(points: [(10.0, -4.0), (220.0, 80.0)], radius: 6.0),
            ],
            yaw_jitter: 6.2832,                   // radians
            align_to_normal: 0.5,                 // 0 = upright, 1 = follow the ground normal
            tilt_max: 0.45, tilt_jitter: 0.18,    // radians: cap on the tilt, random extra tilt
            sink: 0.18,                           // fraction of the mesh height buried (plus the slope gap)
            tint: (1.0, 1.0, 1.0),                // linear multiplier on the mesh base colour
            tint_jitter: 0.18,                    // ± brightness between instances (0..0.5)
            base_tint: (0.50, 0.30, 0.17),        // linear colour the bottom of each rock blends toward
            base_blend: 0.3,                      // fraction of the height that blend covers (0 = off)
            contact_ao: 0.6,                      // darkening of the ground under each rock (0..1)
            contact_radius: 1.6,                  // × the rock's footprint radius
            lod_bias: 1.0,                        // > 1 keeps detailed LODs longer
            fade_start: 260.0, fade_end: 340.0,   // metres: rocks shrink into the ground over this band
            cast_shadows: true,                   // sun and local-light shadows
            shadow_max_lod: 3,                    // coarsest LOD that still casts (0..=3)
            shadow_distance: 0.0,                 // metres; 0 = cast up to fade_end
        ),
    ],
)
```

Every `ScatterClass` field except `name` and `meshes` has a default (`ScatterClass::default`), and
Shadows: a casting instance is drawn only into the sun cascade that holds it and the next one,
and only into the shadow pages its light-space footprint covers (its shadow is assumed to end
within `r / tan(sun elevation)` of the caster, clamped to 1..=128 m; elevation floored at 0.03 rad). Turn casting off for classes or LODs whose
shadows are below a shadow texel (gravel beyond ~40 m) with `shadow_max_lod` / `shadow_distance`.

`ScatterMeshRef` defaults `weight`, `min_scale`, `max_scale`, `scale_bias` to 1, so a generator may
emit only what it changes. Scale is `min + (max − min) · uᵇ` with `b = scale_bias` (b > 1: mostly
small rocks, a few big ones).

**Meshes** (what DO27 drops in): glTF 2.0 per `docs/formats/gltf-conventions.md` — metres, +Y up;
pivot at the centre of the rock's base, at the height where it meets flat ground (the engine buries
`sink` of the height below that); any facing (yaw is random); nominal size about 1 m (the scale
range sets the real size). LODs: 1–4, authored as `<stem>_LOD0` … `_LOD3` nodes (LOD0 most
detailed) or one mesh with cook-generated LODs (the engine's procedural stand-ins follow the same
convention: 1 m across at scale 1, so `min_scale`/`max_scale` are sizes in metres); a 1–2 k / 300 / 80 / 20 triangle chain matches
the stand-ins. Switch sizes come from the cooked LOD chain's `screen_relative_size` (defaults
0.06 / 0.018 / 0.003 of the screen half-height). One closed, outward-facing primitive with one
metallic-roughness material (base colour, normal, ORM). `tint` multiplies the base colour and
`base_tint` is applied as a vertex-colour ratio, so near-neutral albedo textures blend best.

**Density maps**: an 8- or 16-bit grayscale PNG covering the whole terrain with the heightfield's
mapping (column → +X, row → +Z); white keeps every candidate, black removes them. A map only removes
candidates, so editing it never moves the rocks it keeps.

**Determinism**: a cell's stream is PCG32 seeded by a SplitMix64 mix of `seed`, the FNV-1a hash of
the class name and the cell coordinate; every candidate draws the same number of values whether
or not a filter rejects it. Positions are sampled from the bilinear source heightfield (the
surface the simulation reads), never from the render LOD. Filters use square roots and a fixed
Taylor series, not libm trigonometry.

**Versions**: v1 files (`ScatterRule(version: 1, …)`, one mesh) still load: they become a set with
one class `"rocks"` carrying that mesh and filters; `regia upgrade-project --write` rewrites
them as v2. A newer version is rejected before the body is parsed.

**Cook**: `regia cook` validates the file and stores it as `regia_terrain::ScatterSet`
(little-endian `u32` cooked version 1, then postcard). Placement is not baked; it streams at
runtime in 64 m tiles around the camera.

## 3. Water plane (`.water.ron`)

```ron
WaterPlane(
    level: 4.0,                     // metres world Y
    shallow_color: (0.10, 0.35, 0.38),   // linear RGB
    deep_color: (0.02, 0.10, 0.16),
    foam_color: (0.42, 0.46, 0.48),   // the default
    foam_width: 1.6,                // meters from the shoreline, measured horizontally
    max_depth: 12.0,                // meters of view path to the deep color (95 % of alpha at half)
    alpha: 0.75,                    // opacity of deep water
    wave_amplitude: 0.05,           // metres
    wave_frequency: 0.9,            // radians per metre
)
```

No simulation; purely visual. Depth computed from the terrain height texture at each fragment.
Shading (ADR-1237): Fresnel reflection of the sky, Beer–Lambert opacity along the refracted view
ray, an opacity fade over the last 0.2 m of depth at the waterline, and a foam band whose width is the horizontal
distance to the shoreline (`foam_width`, not `depth / bed slope`). The fields and their shape are unchanged from v1.

## 4. Crater / scorch (`.deform.ron`)

```ron
// Crater (applied as a deformation hook):
Crater(
    center: (0.0, 0.0),    // (x, z) metres
    radius: 10.0,           // metres
    depth: 2.0,             // metres positive = dug
    rim_height: 1.0,        // metres
)

// Scorch mark list:
ScorchList(
    marks: [
        ScorchMark(
            center: (5.0, -2.0),
            radius: 3.0,
            strength: 0.8,    // 0..=1
        ),
    ],
)
```

Crater profile: `(1 - t²)²` bowl + `sin²(πt)` rim (in-house, cited in `provenance.md`).
Maximum 16 scorch marks per terrain (uniform buffer bound).

## 5. Hole mask (`<map>.holes.ron`, format v2)

A map's terrain holes (tunnel mouths, ramp trenches, ADR-088, ADR-247) live in
`<map>.holes.ron` next to the map: the map file's name up to its first dot plus `.holes.ron`
(`maps/switchbacks.level.ron` → `maps/switchbacks.holes.ron`, `regia_terrain::holes_ron_path`).
The document is a serialized `HolesMask` with a version (`HOLES_FORMAT_VERSION` = 2):

```ron
(
    version: 2,            // optional: a file without it is version 1
    holes: [],             // legacy ellipses: HoleEllipse (center, radius, rotation_radians)
    shapes: [              // HoleShape, externally tagged, snake case
        polygon(points: [(-15.0, -9.0), (14.0, -10.0), (16.0, 8.0), (0.0, 12.0), (-14.0, 9.0)]),
        rect(center: (40.0, 2.0), half_extents: (10.0, 12.5), rotation_radians: 0.7),
        ellipse((center: (-40.0, 0.0), radius: (6.0, 4.0), rotation_radians: 0.0)),
        // raster((center: (x, z), half_extents: (hx, hz), rotation_radians: r,
        //         width: w, height: h, coverage: [0..255; w * h])), row = local z;
        // inside where the bilinear coverage reaches 50 %.
    ],
    named: [               // v2 (ADR-1595): NamedHole, opened and closed at runtime by id
        (id: "tunnel_3", open: false, shape: rect(center: (60.0, -8.0), half_extents: (10.0, 12.5), rotation_radians: 0.0)),
    ],
)
```

Version 2 adds `named` (optional; omitted when empty): holes with an `id` (1..=64 ASCII letters,
digits, `_`, `-`, `.`, unique) and `open` (absent = true). A closed hole cuts nothing but counts
toward the 64 shapes, so opening it at runtime (`terrain.set_hole`, `HolesMask::set_open`) never
fails. Named holes are sorted by id on load and on edit. A version 1 file loads unchanged (no
named holes); `named` in a version 1 file is refused. The v1 → v2 migration
(`regia upgrade-project --write`) re-saves the file at version 2.

All coordinates are world XZ metres. Rules (`HolesMask::from_holes_ron` refuses the file
otherwise): at most 64 shapes in all, ellipses and named holes (open or closed) included (the GPU draws one 256² coverage window
per shape); polygons need three finite points (even-odd fill); radii and half extents finite and
positive; a raster's `coverage` holds `width × height` bytes, at most 2048 texels a side. A
`version` newer than the engine's is refused before the body is parsed. The `HolesMask(...)`
struct-named form parses as well as the bare `(...)`, so `ron::to_string(&mask)` output loads
as version 1. `regia upgrade-project` lists the files (`terrain-holes`), and
`cargo xtask check-formats` guards the shape (the document, `HoleEllipse`, `HoleRaster`, the
variant names and `NamedHole`; a change to the `rect` or `polygon` variant fields is not caught by the
fingerprint, so bump the version by hand then).

Loading: `regia_terrain::load_holes_ron(path)` (`Ok(None)` when the file does not exist), then
`TerrainSharedDraw::set_holes` for the drawn terrain, the probes and the path tracer, and
`HolesMask::cut_heights` for the physics heightfield. The agent protocol's `terrain.load_holes`
does the draw part.

## 6. Tunnel provenance

Tunnel render meshes carry a `CollisionProvenance` record linking to the collision source
asset and triangle count:

```ron
CollisionProvenance(
    source_asset: "tunnels/collision_north.gltf",
    triangle_count: 482,
    note: "generator v3, exported 2026-10-04",
)
```

## 7. Tiled texture (`.terrain.texture` / virtual-texture tiles)

Photoreal macro maps (8K albedo/normal/AO), the 2 × RGBA 8K splat masks and the 6 material
layers (2K–4K albedo/normal/ORM/height) cook into a *virtual-texture* layout so the runtime can
stream only the tiles a camera needs (see `crates/regia_terrain/src/stream.rs`).

- Each source texture is square and a power of two; the tile size (`DEFAULT_TILE_SIZE` = 256 px)
  divides it. Fine mips (size ≥ tile size) are split into `tile_size` tiles; every tile is its own
  KTX2 with a full sub-mip chain (tile mip 0 down to 1×1). Mips smaller than the tile size are
  packed into one resident "tail" KTX2, down to 4×4 (the smallest block-compressible level).
- Block formats (chosen per slot, see `crates/regia_asset_pipeline/src/terrain_data.rs`
  `TileFormat`):
  - **albedo** → BC7 sRGB; **normal** → BC5; **AO / height** → BC4; **masks / ORM** → BC7 linear.
  - The two RGBA splat masks use **BC7 linear**, not a per-channel BC1/BC4 split: four channels at
    BC4 is 16 bpp, while one BC7 texture carries all four channels at 8 bpp — half the bytes, with
    three-subset partitions and 8-bit endpoints that preserve smooth weight gradients. Quality is
    measured by the deterministic bit rate and the tile-coverage test (which verifies each tile's
    VkFormat and that its texels map to the correct source region); a decoder-based PSNR is
    deferred (`intel_tex_2` is encode-only).
- A tile's cooked `AssetId` is derived deterministically from the map id and its path:

  ```text
  tile_asset_id = AssetId::sub_asset(map_id, "terrain/<slot>/mip<mip>/tile_<x>_<y>")
  tail_asset_id = AssetId::sub_asset(map_id, "terrain/<slot>/tail")
  ```

  so the cooker and the runtime streamer agree on every tile and tail id (no random UUIDs; the cook
  is byte-reproducible).
- Every tile and the resident tail are written as `regia_asset::CookedTexture` `.fckd` files
  (`regia_asset_pipeline::write_tiled_texture_cooked`) under their deterministic ids; the streamer
  reads them back with `regia_asset::load_texture_bytes` and caches the KTX2 payloads.
- The runtime description is [`regia_terrain::stream::TiledTexture`] (name, size, tile size,
  format, mip count, tail mips); the cooker produces [`TiledTextureCook`] (tail KTX2 + per-tile
  KTX2).
- **Android** (`platform = android`) cooks those same tiles as **ASTC 4×4 LDR**. 4×4 is the block
  this section names: it is the smallest block-compressible level, and it has the same footprint as
  the BCn blocks above. The bytes are the Khronos ASTC LDR block (Khronos Data Format Specification
  1.3, chapter 23: 128-bit blocks, including the void-extent form a constant-color tile uses), packed
  in a KTX 2.0 container (header `vkFormat` at byte 12). Albedo is `ASTC_4x4_SRGB_BLOCK` (158).
  Normal, AO, height, ORM and the splat masks are `ASTC_4x4_UNORM_BLOCK` (157). The encoder and the
  channel layouts are the Android terrain material arrays': astcenc (medium preset; thorough for
  normals), AO and height in one channel (`R = G = B`), normals in `.rg` like BC5 (`R G 0 1`). The size rule is the
  desktop one: a slot is a power of two, at least 256 a side, because `regia cook` tiles every slot
  at `DEFAULT_TILE_SIZE` (§ 9); a smaller slot fails the cook with `BadTileSize`. A tile edge that is
  not a multiple of 4 is refused with `terrain_astc_alignment` before any encode: that is an
  API-level guard of `cook_tiled_texture` for a caller's own tile size, which `regia cook` never
  raises. Sub-mips of 2×2
  and 1×1 pad to one block, the same way the BCn chain does. The low-quality 6×6 / 8×8 ladder of the
  generic texture cook is not used here. The encoding lives in the tile payload, not in an index
  field (the index is version 2 since ADR-8024, § 12, for other reasons). The heightfield stays 16-bit samples (§ 8).

## 8. Tiled heightfield (`.terrain.heightfield`)

The 4096² R16 heightfield cooks into `tile_size` (256) square tiles; samples are kept as 16-bit
verbatim (no resampling — spec 26 § 1 requires the render surface to match the simulation). Each
tile carries its own `[min, max]` sample so LOD bounds are available per tile without scanning the
map:

```text
TiledHeightfield {
    version: 1,
    width, height, tile_size: u32,
    tiles: [ { x, y, min: u16, max: u16, samples: [u16; tile_size²] } ]
}
```

Postcard-serialised into the cooked cache / pak (`regia_asset_pipeline::cook_tiled_heightfield`).
Edge tiles are border-padded when the map size is not a multiple of the tile size.

Neither format changes the frozen `.terrain.layers.ron` / `.scatter.ron` formats: the data path
adds new cooked assets alongside them rather than new fields on them, so no version bump or
migrator is required.

## 9. Cooking a terrain (cook convention)

`regia cook` cooks every `*.terrain.layers.ron` source as one *terrain map* through the tiled
path (§ 7, § 8). The inputs are discovered from the paths the layer file names plus this documented
cook convention (a convention, **not** a frozen-format change — the `.terrain.layers.ron` v1 format
is unchanged). For a layer file at `<dir>/terrain.layers.ron`:

```
<dir>/heightfield.png           16-bit gray R16 heightfield (first match of this, then
                                <dir>/../visual/heightfield.png)
<dir>/heightfield.json          optional world extents: `size_m` (f32) + `origin` ([f32; 3]);
                                unknown fields ignored; absent → extents come from the runtime
                                defaults. Read beside whichever heightfield matched: for the visual
                                sibling it is <dir>/../visual/heightfield.json. With `"schema": 2`
                                it is a count mapping instead (§ 9a).
<dir>/macro_albedo.png          sRGB albedo → BC7 sRGB
<dir>/macro/normal.png          tangent normal, linear → BC5
<dir>/macro/ao.png              8-bit L (read R; L or RGB accepted) → BC4
<dir>/macro/pack0.png           RGBA8 linear splat mask → BC7 linear
<dir>/macro/pack1.png           RGBA8 linear splat mask → BC7 linear
<dir>/detail/<name>/albedo.png  sRGB → BC7 sRGB
<dir>/detail/<name>/normal.png  linear → BC5
<dir>/detail/<name>/orm.png     linear RGBA (R AO, G roughness, B metal) → BC7 linear
<dir>/detail/<name>/height.png  linear L → BC4
```

Layer names and order come from the file's `layers` array (never a folder scan — unused folders in
`detail/` are ignored). Slot names are deterministic and stable: the macro slots are the five names
above; a layer's four slots are `layer{index}_{kind}` where `index` is the 0-based bottom-first
position and `kind` ∈ `albedo | normal | orm | height` (e.g. `layer2_orm`).

Missing optional slots are **warnings, not errors**: a missing macro or layer albedo/normal slot is
simply omitted from the index. A missing layer `orm`/`height` cooks a **neutral default** instead
(ORM = AO 1, roughness 0.8, metal 0; height = flat 0) at the layer's reference size (its first
present texture). Every slot cooks at tile size 256; the map's deterministic ids are
`terrain/<slot>/mip<mip>/tile_<x>_<y>`, `terrain/<slot>/tail`, `terrain/heightfield` and
`terrain/index`, all derived from the `.meta` id of the layer file (the *map id*).

The primary cooked asset of a terrain is its **terrain index** (`terrain/index`), a postcard payload
in a standard `.fckd` wrapper (type path `regia_terrain::stream::TerrainIndex`; version 1 below, still read).
Version 2 is one unreleased layout (ADR-8619) that appends, in this order, the optional `heights`, `holes` and
`material` entries of § 12 (ADR-8024; the standard cook writes them as `None`) and then `source_mapping` (§ 9a,
ADR-8471):

```text
TerrainIndex {
    version: 1,
    map_id: AssetId,
    slots: [ { name, width, height, tile_size, format, mip_count, tail_mips, tile_count, bytes } ],
    heightfield: Option<{ width, height, tile_size, tiles_x, tiles_y, bytes }>,
    layers: [String],
    world_size_m: Option<f32>,
    world_origin: Option<[f32; 3]>,
}
```

The runtime streamer (`regia_terrain::stream`) reads the index to register each slot as a
[`TiledTexture`] and address its tiles; `regia cook --stats` reads the same index to report the
tiled sizes per slot and per map.

### 9a. Count mapping (`heightfield.json` schema 2, index version 2, ADR-8471)

A `heightfield.json` that has a `schema` key carries a **count mapping** instead of the legacy
extents. The fields are the schema-2 source fields of `terrain.inspect_source` (ADR-8063), without
`counts` (the counts are the PNG's):

```json
{
  "schema": 2,
  "grid": { "cols": 33, "rows": 33, "cell_m": 0.5,
            "sample0_x_m": 37.25, "sample0_z_m": 37.25, "row0": "south", "col0": "west" },
  "units": { "kind": "counts", "meters_per_count": 0.03125, "zero_counts": 20000 }
}
```

- `cols` and `rows` are required and must be the PNG's size; the cook fails otherwise.
- `sample0_x_m` / `sample0_z_m` default to `-dimension * cell_m / 2` (the source default). The
  registration is the first sample's world X/Z; sample `(col, row)` sits at
  `sample0 + (col, row) * cell_m`, computed in f64 and rounded to f32 once.
- A height is `(f32(count) - f32(zero_counts)) * meters_per_count`, in that order.
- `size_m` or `origin` beside `schema` is refused (the registration would be given twice). Without
  `schema` the file is the legacy extents file, read and hashed exactly as before, whatever other
  keys it holds. Unknown keys are ignored. The file is read up to 1 MiB.

The tiled heightfield (§ 8) is unchanged: the counts are stored verbatim. The **terrain index is
version 2** and appends the resolved mapping:

```text
TerrainIndex { ...version 1 fields..., source_mapping: Option<{ cols, rows, meters_per_count,
               zero_counts, cell_m, sample0_m: [f32; 2] }> }
```

A version 1 index still loads (no mapping). `regia upgrade-project` lists cooked indexes as
`terrain-index` and rewrites a v1 index as v2 with no mapping, the cooked header kept and a `.bak`
of the original; a run that fails on any file writes nothing.

The cook input hash keeps its domain and the legacy 18-byte extents layout; a mapping appends
`b'M'` and 26 bytes (`cols u32 | rows u32 | meters_per_count f32 | zero_counts u16 | cell_m f32 |
sample0 X f32 | sample0 Z f32`, little-endian, the resolved values). So a change of only the
datum, scale, cell or registration recooks the terrain, and a legacy file's hash (and its net
ground hash) is unchanged.

A level that names the terrain (`regia_terrain::load_level_terrain`) builds its map from the
mapping: the exact heights above, world width `(n - 1) * cell_m` and the grid origin at the grid's
center (`TerrainMap::with_grid_origin`). The level's `height_scale_m` and `height_offset_m` do not
apply; a `world_width_m` must equal the mapping's. The runtime map is square with one origin on X
and Z, so a rectangular grid or a center that differs on X and Z is refused with
`level_terrain_mapping` (never resampled), as is a source whose sample positions or exported counts
would move. `terrain.inspect_source { mapping }` reports `runtime_map.loadable` before a cook.

A layer file with **no resolvable heightfield or layer texture** is not a terrain: it is left
uncooked (no terrain index is emitted) with a warning. On **Android**, `regia cook --platform android`
cooks the same tiled path: the heightfield stays R16 tiles, and every texture slot above is ASTC 4×4
(§ 7) instead of BCn. Texture slots follow the desktop size rule on Android too: a power of two, at
least 256 a side (the tile size); a smaller slot fails the cook with `BadTileSize`, not
`terrain_astc_alignment` (an API-level guard that this cook never raises). The inputs are not sent through the generic single-texture cooker, so a level
that names the terrain (`regia_terrain::load_level_terrain`) loads the same heights on Android as on
desktop from the tiled heightfield alone. The index has the same version on both platforms;
`assets.terrain_tiles` reads the encoding from each slot's tail. (ADR-5294 replaced the interim
heightfield-only Android index of ADR-4914, whose `slots: []` index and generic texture copies a
later Android cook removes: the Android cook key changed, so the first Android cook after the update
recooks every terrain.)

### 9b. Source grid (index version 3, ADR-8871)

Version 3 of the cooked terrain index appends two optional entries after `source_mapping`, in this
order. A version 1 or 2 index loads as version 3 with both `None`; `regia upgrade-project --write`
rewrites the file (cooked header kept), and a second run leaves the same bytes.

| Entry | Type | Meaning |
|---|---|---|
| `source_spacing_z_m` | `Option<f32>` | The source grid's sample spacing on Z, meters, when it differs from X (`source_mapping.cell_m`). Equal spacing is always `None` (one grid, one encoding). Needs `source_mapping`; positive, finite, and the last row `sample0_m[1] + (rows - 1) * spacing` (in `f64`, rounded once) finite and past the first. |
| `source_valid_mask` | `Option<TerrainValidMaskIndex { cols: u32, rows: u32, missing: u32, bits: Vec<u8> }>` | The samples that hold no data. `cols`/`rows` equal the mapping's; `bits` holds `cols * rows` flags in canonical order (row 0 south, column 0 west), row-major, eight to a byte, least significant bit first, `1` = has data, `ceil(cols * rows / 8)` bytes, the unused bits of the last byte 0; `missing` is the number of 0 flags, at least 1 and fewer than `cols * rows` (an all-valid mask is `None`). A missing sample keeps its count verbatim in `terrain/heightfield` and is never a height. |

The loader refuses an entry that breaks these rules (`TerrainIndexError::BadSourceGrid`). These are
what a raster descriptor (§ 14) can describe and the version-2 index could not. The cook still
refuses both (`terrain_mapping_unrepresentable`, `terrain_raster_missing_data`): no run-time consumer
reads them yet (the run-time map has one cell size, and there is no run-time hole policy for the
mask). A level whose cooked index holds either fails to load with `level_terrain_mapping` instead of
reading X's spacing on Z or a missing sample's count as a height
(`TerrainIndex::source_grid_unsupported`). `assets.terrain_tiles` reports both as `source_grid`.

## 10. Painted scatter (`<map>.scatterpaint.bin`, format v1)

Mesh instances placed with the scatter brush (ADR-1936, `docs/specs/82-terrain-editing.md` § 13): one file next to the map, named
as `<map>.holes.ron` is, written by the editor's Save and `terrain.save_scatter_paint`. It is a raw asset, not cooked: a game reads
it through `load_scatter_paint` or `PaintedScatter::from_bytes`.

```text
magic   b"RSPT"
version u32 little-endian (1)
body    postcard:
PaintFile {
    classes: [ {
        name: string,                 // the .scatter.ron class; sorted, unique, empty classes not written
        meshes: [string],             // mesh paths; an instance's `mesh` indexes this table
        tiles: [ {
            tile: [i32; 2],           // floor(x / 64), floor(z / 64): anchored at the world origin
            instances: [ {            // sorted by id, unique in the tile
                id: u64,              // stable: the renderer's object key and the candidate's identity
                mesh: u16,
                flags: u16,           // reserved for foliage wind; 0 in v1, kept through load and save
                position: [f32; 3],   // ground contact point, meters
                normal: [f32; 3],     // unit ground normal
                yaw: f32,             // radians
                scale: f32,           // > 0
                shade: f32,           // 0..=1, a draw; the class's tint_jitter makes it a brightness
                tilt: [f32; 2],       // 0..=1 each, draws; the class's tilt_jitter scales them
            } ],
        } ],
    } ],
}
```

The same instances give the same bytes. The version is read before the body, so a newer file is refused with a clear error. A
reader rejects a bad magic, a truncated body, a repeated class, tile or id, an empty tile, an instance outside its tile, a mesh index
past the table, a non-finite value and a store past 16 Mi instances. `regia upgrade-project` reads the header; v1 has no migrator yet,
and a shape change needs a version bump and one, as for every frozen format.

## 11. Heightmap interchange (16-bit PNG and RAW, ADR-1853, ADR-2411)

`terrain.export_heights` / `terrain.import_heights` and the editor's Terrain panel (Heightmap: Export, Import) write and read
the map's own 16-bit grid. These are external interchange files with no `format_version`; nothing here is a frozen Regia
format.

- **Height scaling.** A sample `s` (0..=65535) is the height `h = offset + s · scale / 65535` metres, where `scale` and
  `offset` are the map's (`TerrainMap::new`; for a level that names its terrain, `LevelTerrain::height_scale_m` and
  `height_offset_m`, scene format v12). Export writes `s = round((h − offset) · 65535 / scale)` clamped to 0..=65535 and
  reports how many samples it clamped low and high (an edit can push heights outside the range). Import is the inverse, so
  export then import gives back every sample bit for bit.
- **Layout.** Row-major, row 0 is the map's −Z edge and column 0 its −X edge (as the cooker's PNG import reads it). The
  world width is the map's; a file carries none.
- **PNG.** 16-bit grayscale (color type 0, bit depth 16), big-endian samples as the PNG standard stores them. Other color
  types and depths are refused.
- **RAW (`.r16`, `.raw`).** Headerless 16-bit samples, little-endian by default (the panel's RAW big-endian box or
  `endian: "big"` flips it). Without a size, a RAW file is a square grid of `sqrt(bytes / 2)` samples per side.
- An import must match the map's size. It writes only changed samples. An import of at most 2^22
  changed samples is one undo step; a larger import applies with no undo entry. Files over
  `2 · 16384² + 1 MiB` bytes are refused before they are read.

## 12. Mesh layers and world geometry source (`<map>.meshlayers.ron`, `<map>.passages.ron`, format v1)

Registry name `terrain-mesh-layers` (spec 121 § 9.1, spec 26 § 2a.10, ADR-8112; crate `regia_world_geom::meshlayers`
and `::passages`). RON, root `version` first. A newer file is refused (`meshlayers_newer_version`) before its other
fields are read, so a v2 file with fields this engine does not know still names the right error. There is no older
version. `regia upgrade-project` lists both files under this name.

`<map>.meshlayers.ron` sits next to the map, like the holes file:

```ron
(
    version: 1,
    chunk_m: 64.0,                       // render chunk side (default 64)
    geom: (max_floors: 5),               // GeomSettings; every field optional, defaults are DO27's values
    ground: Full,                        // or Nested(max_level: 2); authored, hashed, never switched by the cook
    ground_tile_cells: 64,               // power of two, 16..=256
    sections: (section_chord_m: 4.0, plug_length_m: 6.0),
    layers: [
        (
            id: "tubes",                 // 1..=64 of A-Z a-z 0-9 _ - .; unique
            source: Passages(path: "switchbacks.passages.ron"),
            fills: ["north_mouth"],      // named holes; each filled by one layer at most
            // optional: map_layer (0), cast_shadows (true), collider (false), material (""), max_lods (4),
            // throat_m (2), skirt_m (0.25), max_stitch_gap_m (1), blockers ([]), look ((detail: None, uv_tile_m: 4))
        ),
    ],
)
```

- `source` is one of `Passages(path)`, `Gltf(path, node, collision_node)`, `Collision(path, detail)` and
  `Volume(sdf, iso, cell_m)`. Unknown fields are refused (`meshlayers_parse`); out-of-range values name the field
  (`meshlayers_invalid`).
- `geom` (`GeomSettings`): `index_cell_m` 16, `wall_pad_m` 8, `same_height_m` 0.001, `lid_gap_m` 0.05, `hull_fit_m`
  2.5, `step_bound_mm` 50, `max_floors` 8 (1..=16), `min_cover_mm` 2000, `pitch_error_m` 0.05, `fine_band_m` 16. Every
  float must be finite; `-0.0` is cooked as `+0.0`.
- `blockers: [(passage, from_s_m, to_s_m, initially_active)]` names a collapsible run of a passage.

`<map>.passages.ron` is a passage description as data (the file a `Passages` source names): `version` (the same format
version), `layout` (the Stage A `TunnelLayout`: `map_id`, its own layout `version`, `sample_step_m`, `passages`,
`openings`, `junctions`, `crawlways`) and, for an imported file, `sampled`, `holes` and `triangle_count`. A file authored
in Regia omits the last three. The cook samples the layout on the map's terrain and refuses an imported `sampled` block
that disagrees (`passages_drift`).

`regia terrain passages import <tunnels.json> [-o OUT] [--json]` converts DO27's `do27-tunnels/1` export to this file
(default `<map_id>.passages.ron` next to the input); `regia terrain passages export <file> [-o OUT]` writes the JSON back,
byte for byte for an imported file (`passages_unsampled` for a Regia-authored one). `do27-tunnels/1` is a foreign host
format: Regia imports it and never freezes it.

## 13. Cooked mesh layers (`<map>.meshlayers`, format v2)

Registry name `terrain-mesh-layers-cooked` (spec 121 § 9.2, ADR-8112; crate `regia_world_geom::cooked`). A cache: `regia
cook` writes it again when its inputs or versions change. Version 2 since v0.7 (format batch 2) has two changes from
version 1 (v0.6.0): the canonical section stores its counts at a fixed width behind a hashed head (ADR-8816), and the
chunk section is version 3 (each layer's `collider`, ADR-8872). A version 1 file still loads (the loader migrates the
canonical section in memory and the terrain crate the chunk section, with `collider` false), and `regia upgrade-project
--write` rewrites it at version 2 (`regia_terrain::upgrade_cooked_mesh_layers`; the canonical value, so `content_hash`,
and the presentation section are unchanged).

```text
"RGML"                                   4-byte magic
postcard(MeshLayersHeader)               version (u32 varint, 2), input_hash [u8; 32], settings copy, ground copy
postcard(Vec<u8>) canonical section      canonical_version (2) first; laid out below
postcard(Vec<u8>) presentation section   the `Passages` render meshes (regia_world_geom::render, ADR-8175)
postcard(Vec<u8>) chunk section          only when not empty; opaque to the world-geometry crate (the terrain
                                         crate's mesh-layer chunks, ADR-8398, ADR-8576)
```

Nothing may follow the chunk section (or the presentation section when the file has no chunk section). The loader reads the header version first and refuses a newer file
(`geom_newer_version`) before anything else; it refuses a newer `canonical_version` the same way.

**The canonical section's bytes (version 2):**

```text
canonical_version        postcard u32 (2: one byte)
head_len                 u32, little-endian
postcard(CanonicalHead)  head_len bytes: the value below without canonical_version and ground_counts, then
                         count_lens (u32 per ground tile: how many counts its block stores) and
                         count_hashes ([u8; 32] per count-body block: its BLAKE3)
count body               every ground tile's counts, ascending tile index, row major, u16 little-endian each;
                         2 · sum(count_lens) bytes; hashed in blocks of 65,536 bytes (the last one shorter)
```

A version 1 section (v0.6.0) is `postcard(WorldGeomCooked)` in the field order below with `canonical_version` 1, every
count a varint. A version 1 file holds a version 1 section and a version 2 file a version 2 section; a mix is refused.

**The canonical section's value** (`WorldGeomCooked`), in field order: `canonical_version`; `settings`; `lattice`
(`lattice_n` u32, `width_mm` i32, `ground_tile_cells` u16, `offset_m_bits` and `step_m_bits`, the `f32` bits); `ground`
(`Full` or `Nested(max_level)`); `tile_levels` (one u8 per ground tile, ascending tile index `b · tiles_per_side + a`);
`hanging_overrides` and `ring_pins` (`(vertex, height_mm)`, strictly ascending vertex `j · n + i`); `tiles` (1024 m cooked
tiles in ascending `(z, x)` origin, each `origin_mm: [x, z]` and its triangles sorted by `(layer, index)`: `v` three
`[x, y, z]` mm with x and z local to the tile and y world, `kind` Floor/Roof/Wall/Blocker, `flags` (1 portal, 2 pilot
only, 4 no support), `section`, `material`, `layer`, `index`); `ground_counts` (u16 counts per ground tile at its level,
row major, border vertices included); `cut_bitmaps` (`(tile, bits)`: two bits per lattice cell, a set bit is cut);
`sections`, `portals` (polygons in i32 mm), `zones` and `plugs` with their initial states.

**`content_hash`** is BLAKE3 of the canonical section's hashed prefix exactly as stored: `canonical_version`, `head_len`
and the head. The head holds the BLAKE3 of every count-body block and the loader refuses a block that does not match
(`geom_corrupt`, naming the block), so the hash covers every byte the run time reads
(spec 121 § 9.2, ADR-8084, ADR-8816), and a peer with another setting, lattice constant, ground mode, tile level, pin,
override, cut bit, triangle or count fails the match handshake. A version 1 file's `content_hash` is that of its section
migrated to version 2, so it equals the hash of the same map cooked at version 2. A host may load with the body check
deferred (`WorldGeom::from_cooked_with(bytes, CountBodyCheck::Deferred)`) and run `cooked::check_count_body` on the
canonical section off the load path; it must not let the map into a match before that check passes. The header's settings and ground copies are informational; a file whose
copies differ from the canonical section is refused (`geom_corrupt`). A headless server reads the header and the
canonical section only.

**Canonical floats in cooked shapes** (ADR-8579). A zone's `OrientedBox` `yaw` is the lockstep `atan2` of the
section's chord (`atan2(dz, dx)` with signed-zero inputs taken as `+0`, a NaN result canonical): it lies in
`(-π, π]`, `+0` along `+x` and `+π` (never `-π`) along `-x`. Its `half_mm` is `m_to_mm` of the half extent plus the
0.25 m pad, like every other cooked length, and `reverb_size_m` is never `-0`. The same section gives the same bits
on every host.

The loader also refuses (`geom_corrupt`): bad magic, trailing bytes, a head length past the end of the section, bytes
left over inside the head, a count body whose length is not `2 · sum(count_lens)`, a number of count hashes other than
`ceil(body bytes / 65536)`, a count of tile levels or count blocks that is not
`tiles_per_side²`, a level other than 0 in `Full` mode or over `max_level` in `Nested`, a count block of the wrong size,
unsorted or out-of-range overrides, pins and cut bitmaps, tiles off the 1024 m grid or out of order, a triangle stored
twice, an unknown flag bit, and section, portal, zone or plug ids that are not dense or out of range. A coordinate at or
beyond 2^24 mm from the origin is `geom_extent_too_large`.

The cook (`regia_world_geom::cook`) builds the canonical section from the document, the heightfield and the `Passages`
files: the pinned Stage A mesher, then the weld step (a floor vertex up to 0.15 m over the ground goes onto it) and the
trim step (a floor strip under 2 m wide within 0.5 m of another floor gets the no-support flag), then `m_to_mm`. It
refuses what later lanes build (`cook_not_built`: `Nested` ground, blocker plugs, `Gltf`, `Collision` and `Volume`
sources) and a missing input (`input_missing`); ring pins, cut bitmaps, sections, portals, zones and plugs are
written empty until those lanes land.

**The presentation section** (ADR-8175, `regia_world_geom::render`; v1 while the format is unshipped) is
`postcard(PresentationCooked)`, in field order: `version` (u32, 1, read first; a newer one is `geom_newer_version`);
`layers`, one `LayerRender` per `Passages` layer in document order: `layer` (u16, the document index), `id`,
`material`, `uv_tile_m_bits` (`f32` bits), `subdivision` (u8, 2..=16), `vertices` (distinct `[x, y, z]` world mm in
first-use order), `triangles` (`v` three u32 vertex indices wound like the canonical face, `part` Floor/Wall/Roof,
`source` `(passage u16, face u32, piece u16)`: the canonical face index in the layer and the piece of its
`subdivision²` grid), `passages` (`(id, first, count)` ranges of `triangles`, in layout order, covering them exactly)
and `mouths` (`(opening, corners)`: four `[x, y, z]` mm corners of each opening's cut quad). Empty bytes are an empty
section (a file cooked before the render half). A vertex index or passage range out of bounds, or trailing bytes, is
`geom_corrupt`. The render vertices are each canonical face's barycentric grid points
`(a·(d − i − j) + b·i + c·j) / d` in integer millimetres, rounded half away from zero: displacement 0, the same bytes
on every host, and the canonical triangles themselves are never in the draw list. Not hashed into `content_hash`.

**The chunk section** (ADR-8398, ADR-8451, ADR-8576; `regia_terrain::mesh_layer::TerrainMeshLayersCooked`, postcard, its
own `version` u32 first, 3), stored after the presentation section only when it is not empty (a file without one ends
after the presentation section). `grid`: `span_m` f32, `root_level` u32, `chunk_level` u32, `grid_size` u32, `range_factor` f32,
`origin` `[f32; 2]`. `layers` in document order: `id`, `map_layer` u8, `cast_shadows`, `collider` (the source layer's flag, spec 26 § 2a.8;
version 3), `material` (asset path as written),
`material_id` (`Option<AssetId>`, the cooked `CookedStandardMaterial` it resolved to; `None` for an empty `material`, the
default rock; version 2), `chunks`
sorted by `(z, x)`, each `x`, `z` (node at `chunk_level`), `bounds` `[min, max]` world metres, and `lods` finest first
(1 to 4): `mesh` (the `CookedMesh` v3 sub-asset id of `meshlayers/<layer>/<x>_<z>/lod<i>` under the map id), `triangles`,
`bytes` (the payload length the stream budgets) and `error_m` (meshopt's absolute error). A file with no chunk section has no chunks. A newer section is `meshlayers_presentation_newer`, version 1 (no `material_id`)
`meshlayers_presentation_stale`, anything malformed `meshlayers_presentation_corrupt`; all recook. Version 2 (no
`collider`) loads with every layer's `collider` false. `regia cook` runs the
cook for every level whose `terrain` block names a heightfield and whose map has a `.meshlayers.ron` (ADR-8451). `regia_asset_pipeline::terrain_mesh_layers::cook_mesh_layers` writes the whole file: W-06's cook for the
header, the canonical section and the presentation section (unchanged bytes), then the chunks and their LOD meshes. It skips an input whose file version, canonical
input hash, chunk grid, map id and layers' material and `collider` are unchanged, and never rewrites a file whose bytes would not change.

The input hash includes the semantic cook revision (ADR-10346): revision 2 rebuilds legacy
skylight cuts, including caches already rewritten into the current layout by an older
migrator. `upgrade_cooked_mesh_layers` also invalidates an old file's header input hash
while preserving its canonical geometry, chunk data and presentation. Migration alone
does not certify a current source cook. The next ordinary cook rebuilds; subsequent
unchanged cooks skip. This changes no serialized layout or format version.

## 14. Cooked photoreal terrain (`terrain-index` v2, `terrain-final-heights` v1, ADR-8024)

A host's package step (DO27) may cook a terrain whose final heights, holes and photoreal material it built itself,
through `regia_asset_pipeline::write_photoreal_terrain` (spec 26 § 7). Every file is a standard `.fckd` under a
deterministic id derived from the map id, like § 9's.

**Terrain index v2** (`terrain/index`, postcard, type path `regia_terrain::stream::TerrainIndex`). v1's fields in
v1's order, then three optional entries:

```text
TerrainIndex {
    version: 2,
    ... v1 fields (map_id, slots, heightfield, layers, world_size_m, world_origin) ...,
    heights: Option<{ width, height: u32, encoding: Residual | Raw, height_scale, height_offset: f32, bytes: u64 }>,
    holes: Option<{ bytes: u64 }>,
    material: Option<{
        params: { macro_world_size, macro_strength, macro_normal_strength, anti_tiling_cell,
                  detail_fade_start, detail_fade_end, triplanar_auto_slope: f32 },
        macro_tiles: u32,            // macro grid side (MacroBake::tiles), 0 = none
        layer_albedo, layer_normal, layer_height, layer_orm: u32,   // blob counts
        macro_albedo_tiles, macro_normal_tiles: u32,                 // macro tile blob counts
        macro_coarse_albedo, macro_coarse_normal: bool,
        bytes: u64,
    }>,
}
```

A v1 index (the standard cook's, § 9) loads as v2 with the three entries `None`: the version varint is read first
and the v1 shape decoded; any other version than 1 or 2 is refused before the body is read. The standard cook writes
v2 with the three `None`; its cook key is unchanged, so no terrain re-cooks. `regia upgrade-project` (`terrain-index`)
rewrites a v1 index `.fckd` in a dev cache to v2, header kept. A reader refuses heights of another size
than the R16 heightfield, residual heights without one, a non-finite scale or offset, more than 8 blobs of a layer
kind, a macro grid over 16 a side, and more macro tiles than the grid holds.

**Final heights** (`terrain/heights`, type path `regia_terrain::stream::TerrainFinalHeights`, `terrain-final-heights`
v1). Fixed little-endian layout:

```text
magic "RTFH" | version u32 = 1 | width u32 | height u32 | encoding u8 (0 residual, 1 raw) | 0u8 × 3 |
height_scale f32 | height_offset f32 | width × height f32 values, row-major, row 0 = −Z edge
```

Raw values are the heights in metres. Residual values are `r` with `h = base + r` in f32, where
`base = f32(s) / 65535 · height_scale + height_offset` and `s` is the R16 sample of `terrain/heightfield` at the
same position (§ 8). The writer uses residual only when that sum gives back the bits of every `h` (a map whose
heights are R16-exact stores zeros, which the pak block compression shrinks to almost nothing); otherwise raw. A
reader refuses a bad magic, a newer version, an unknown encoding, non-zero padding, a size that does not match the
index or the R16 grid, a short or long body and any non-finite value.

**Holes** (`terrain/holes`): the `.holes.ron` document of § 5 as raw UTF-8 text in the `.fckd` payload (type path
`regia_terrain::stream::TerrainCookedHoles`); its own version and migrator apply.

**Material blobs**: one KTX2 `CookedTexture` (the same `.fckd` the tiles use, § 7) per blob of
`CookedMaterialKtx2`, at `terrain/material/<kind>/<i>` with `kind` ∈ `layer_albedo | layer_normal | layer_height |
layer_orm | macro_albedo | macro_normal`, and `terrain/material/macro_coarse_albedo`,
`terrain/material/macro_coarse_normal`. Counts come from the index; the encoding (BCn, ASTC, ETC2) is the blob's own.

## 14. Count-raster descriptor (`raster_descriptor` 1, ADR-8542)

An engine-owned JSON document that describes how a count raster's samples sit in the world when schema 2's
assumptions (one cell size, row 0 south, column 0 west, sample-center registration, every count is data) do not hold.
Schema: [`terrain-raster-descriptor.schema.json`](terrain-raster-descriptor.schema.json). Unknown fields are refused.

```json
{"raster_descriptor": 1, "spacing_m": [2.0, 0.5], "row0": "north", "col0": "west",
 "registration": "edge", "origin_m": [100.0, -40.0], "nodata_counts": 65535,
 "valid_mask": {"cols": 3, "rows": 2, "valid": [1, 1, 1, 1, 0, 1]}}
```

| Field | Meaning |
|---|---|
| `raster_descriptor` | Version, `1`. Another value is `terrain_raster_version`, checked before any other field. |
| `spacing_m` | `[X, Z]` sample spacing, positive and finite. |
| `skew_m` | Rotation terms (X per row, Z per column); only `[0, 0]`, else `terrain_raster_transform`. Default `[0, 0]`. |
| `row0` / `col0` | Stored order: `south`/`north`, `west`/`east`. Default `south`, `west`. |
| `registration` | Required. `center`: `origin_m` is the minimum-corner sample. `edge`: it is that cell's outer corner. |
| `origin_m` | Minimum-X/minimum-Z registration point. Default `-dimension * spacing / 2` per axis. |
| `nodata_counts` | A count meaning "no data". |
| `valid_mask` | `{cols, rows, valid}`; `valid` holds `cols * rows` entries of 0/1 in stored order. Wrong size or values: `terrain_raster_mask`. |

Canonicalization (what every consumer sees): the stored order is permuted exactly into row 0 = minimum Z, column 0 =
minimum X (stored row `rows - 1 - r` for `north`, stored column `cols - 1 - c` for `east`). Canonical sample `(c, r)` is
at world X/Z `sample0 + (c, r) * spacing` per axis, computed in f64 and rounded to f32 once, where `sample0 = origin`
for `center` and `origin + spacing / 2` (f64, one rounding) for `edge`. A sample is missing when its mask entry is 0 or
its count equals `nodata_counts`; its count is kept verbatim but it has no height. A raster with no valid sample is
`terrain_raster_missing_data`.

Schema 2 maps to `{"raster_descriptor": 1, "spacing_m": [cell_m, cell_m], "registration": "center", "origin_m":
[sample0_x_m, sample0_z_m]}` (origin absent when the document has no `sample0_*`) and produces the same bits.

Consumers: `terrain.inspect_source` / `terrain.bake_overview` (`raster` parameter) and `TerrainSource::from_raster`.
The runtime map (`TerrainSource::terrain_map`) refuses missing samples unless the caller selects the hole policy, and
refuses unequal spacing (`terrain_mapping_unrepresentable`). The terrain cook reads `heightfield.raster.json` beside
`heightfield.png` (with the schema-2 `heightfield.json` for the datum and scale; a descriptor without it is refused):
it permutes the counts into canonical order before tiling and stores the resolved mapping (§ 9a). It refuses unequal
spacing and missing samples (no index is written); cooking those needs terrain index v3, scheduled with the v0.6
format batch. The descriptor is hashed only when present, so existing terrains keep their hashes. Version 1 is the first
version: there is nothing to migrate; a future version ships a migrator and `regia upgrade-project` support
(`docs/specs/12-assets.md` § 4a).

## 15. Source tile sets (`heightfield.tiles.json`, `terrain_source_tiles` 1, ADR-8595)

A heightfield source may be split over several files described by a tile-set descriptor
(schema `docs/formats/terrain-source-tiles.schema.json`, example
`docs/formats/terrain-source-tiles.example.json`). Unknown fields at any level are an error.

| Field | Meaning |
|---|---|
| `terrain_source_tiles` | Version, `1`. Another value is `terrain_tiles_version`, checked first. |
| `grid` | `cols`, `rows` (global samples, at least 2 each), `cell_m` (spacing on X and Z) and `sample0_m` (world X/Z of global sample (0, 0)); the `CountMapping` rules apply. |
| `units` | `meters_per_count` and `zero_counts`, as schema 2. |
| `max_overlap` | Widest border two tiles may share, samples; default 1. |
| `tiles` | 1 to 4096 entries `{path, origin: [col, row], size: [cols, rows]}`; `path` is relative to the descriptor, `/`-separated, no `.`/`..`, ending `.json` (schema 2) or `.png` (16-bit grayscale). |

Rules: every tile is at least 2x2 and inside the grid (`terrain_tiles_bounds`); two tiles share at most a
`max_overlap`-wide border (`terrain_tiles_overlap`); every global sample is in a tile (`terrain_tiles_gap`); a tile file
holds exactly `size` samples (`terrain_tiles_dimensions`) with the descriptor's datum, scale and cell, bit for bit
(`terrain_tiles_mapping`); a JSON tile's own `sample0` is within 1 mm of the position of its origin
(`terrain_tiles_misregistered`; placement always comes from the descriptor); a PNG tile's datum tEXt, if any, equals
`zero_counts`; every shared sample is the same count in each tile (`terrain_tiles_seam`). Global sample `(c, r)` holds the
tiles' count and sits at `sample0_m + (c, r) * cell_m` (f64, rounded to f32 once). Tiles are sorted by origin (row, then
column); the tile id is `r<row>_c<col>`.

Cook: a `heightfield.tiles.json` beside the layer file replaces `heightfield.png` (both, or a `heightfield.json` or a
`heightfield.raster.json` beside it, is `terrain_tiles_ambiguous` and no index is written; a tile set never applies a raster
descriptor's order or spacing, ADR-8639). The tiled heightfield (§ 8) is built region by region from the tiles with the same
tiles, padding and bytes as from the assembled grid; the index (v2) stores the descriptor's mapping (§ 9a). The tile
files are cook inputs, each hashed by its BLAKE3 framed as `regia-source-tile-blake3\0` + 32 bytes. Evidence for partial
recooks is a cache file (`<cache>/terrain-source-tiles/`), not a content format. Version 1 is the first version: nothing
to migrate; a future version ships a migrator and `regia upgrade-project` support (`docs/specs/12-assets.md` § 4a).

Both source descriptors are frozen content formats (ADR-8639): `terrain-raster-descriptor` (§ 14) and
`terrain-source-tiles` (this section) in `docs/formats/format-hashes.ron`, with v1 fixtures in the old-bytes corpus and
the fuzz targets `terrain.raster_descriptor` and `terrain.source_tiles`. `regia upgrade-project` lists `*.raster.json`
and `*.tiles.json` at v1 and leaves them in place. The overview descriptor (§ 16) is a cache and is not registered.

## 16. Overview descriptor (`overview_descriptor` 2, ADR-8596, ADR-8871)

A registered terrain overview is a pair: an RGB8 PNG (`<name>.png`, north-up, no ancillary chunks) and its
engine-owned JSON descriptor beside it (`<name>.overview.json`, JSON schema
`docs/formats/terrain-overview-descriptor.schema.json`). `terrain.bake_overview { register: true }`,
`regia terrain source-overview` and the editor job `terrain_overview:<n>` write it. Overviews are
presentation-only: no simulation, cook or runtime system reads them.

- `overview_descriptor`: 2. `algorithm`: the pixel version (`OVERVIEW_ALGORITHM_VERSION`, 1).
- `cache_key`: BLAKE3 (hex) of the algorithm version, the source's input digest (the source, mapping and raster
  descriptor files), a digest of the decoded source (grid, datum, scale, spacing, first sample, counts, valid mask),
  the settings and the markers. `max_pixels` is a budget that moves no pixel and is not part of it.
- `image`: `file` (the PNG's name in the same folder), `width`, `height`, `bytes`, `blake3` of the PNG file,
  `missing_pixels`.
- `source`: `path`, `input_digest`, `content_digest`, `cols`, `rows`, `spacing_m`, `sample0_m`, `missing_samples`.
- `settings`: `size`, `fit` (`stretch` | `preserve_aspect`), `relief`, `contour_step_m`, `blur_passes`, `light`,
  `ambient`, `contrast`, `frame_pixels`.
- `mapping`: `image_size`, `content_px` (`[x, y, width, height]` of the source rectangle), `world_min` and `world_max`
  (X/Z of the source rectangle's west/south and east/north edges: the outer cell edges, half a spacing beyond the first
  and last sample centers), `meters_per_pixel`. Coordinates are continuous pixels: `(0, 0)` is the top-left corner of
  pixel `(0, 0)`, `u` grows east (+X), `v` grows south (-Z):
  `x = world_min.x + (u - content_px.x) * meters_per_pixel.x`, `z = world_max.z - (v - content_px.y) * meters_per_pixel.z`.
  `stretch` (the legacy mode, byte-identical to earlier bakes) fills the image with one scale per axis;
  `preserve_aspect` uses one scale, centers the rectangle and paints the rest `letterbox_rgb` `[0, 0, 128]`.
- `orientation`: `north_up: true`, `row0: "north"`, `col0: "west"`.
- `legend`: `elevation_range_m` (the 1st and 99th percentile pixel heights; brightness runs from 70 % to 100 % across
  it, times the hillshade), `brightness`, `contour_step_m`, `missing_rgb` `[255, 0, 255]` (no source data under the
  pixel; never a zero height), `letterbox_rgb`, `scale_bar_m` (1, 2 or 5 times a power of ten meters, at most a quarter
  of the source rectangle's width) and `scale_bar_px`.
- `markers`: `[{label, world_xz, rgb, pixel}]` in drawing order; `pixel` is the pixel holding the marker's center, or
  `null` outside the source rectangle (not drawn). Markers are plus signs three pixels per arm, clipped to the source
  rectangle.
- `tolerance`: `max_channel_delta` 1, `max_differing_fraction` 0.001: how far two bakes of one source and settings may
  differ across architectures (x86-64, aarch64). Comparisons use it; no overview golden image is blessed.
- `presentation_only`: `true`.
- `descriptor_digest` (version 2, ADR-8871, plan W-19): BLAKE3 (hex) of the compact JSON of every other field,
  every object's keys sorted. The reader recomputes it: a hand edit of any field (the mapping, say) or an added field
  leaves the pair uncached, so it bakes again instead of reporting a georeference the bake did not make.

**Publication.** Both files are written to hidden temporary names in the destination folder; the commit point is
after the bake. A cancel at or before it removes the temporaries and leaves an earlier pair untouched. Then the PNG is
renamed into place, then the descriptor. A reader trusts a PNG only when the descriptor beside it records its BLAKE3,
so an interrupted rename never passes for a published pair; a publish whose key matches a current pair writes nothing.

Version 2 adds `descriptor_digest`. Nothing migrates: a reader that meets another version (a version 1 pair
included) treats the pair as stale and bakes again; a future shape change bumps `overview_descriptor` (`docs/specs/12-assets.md` § 4a). Slope and passability
layers will reach overviews only through the C-19 adapter, not through this descriptor.
