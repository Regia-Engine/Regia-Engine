# 82 — Terrain editing completion and wind groundwork

Status: proposed (ADR-1853). Plan item: **terrain editing** (sculpt, paint, holes, scatter brush), `master-plan.md` v0.3.x (b),
and `22-tier2.md` § T2.5 (sculpt raise/lower/smooth/flatten/**noise/ramp**, paint layers, **import/export 16-bit PNG/RAW**); plus
the groundwork for "foliage wind and grass at scale" (`versions.md` v0.4.0, N18). Crates: `regia_terrain`, `regia_agent`,
`regia_editor`.

No reviewed public spec set covers this item: the Regia docs are the source. Sources used: the Regia specs named here, the
code on `main`, and public papers (§ 11). No other engine's text, names or settings.

## 1. Built baseline (main, 2026-10-29)

Already on `main`, and not rebuilt here:

| Piece | Where | Notes |
|---|---|---|
| Brush core: Raise, Lower, Flatten, Assign, Smooth; circle and rectangle; hardness; threaded and synchronous strokes agree bit for bit | `regia_terrain::brush` (ported from Fyrox, MIT) | each sample written once per stroke from its original value |
| Stroke glue: dirty rect to the GPU, min/max pyramid, per-stroke undo, incremental re-cook of touched cook tiles | `regia_terrain::edit` (`apply_stroke`, `TerrainEdits`, `StrokeUndo`, `recook_tiles`) | |
| Painted layer weights | `regia_terrain::layer_paint`, `TerrainPaint` | |
| Editor tool: `T`, `[` `]`, `-` `=`, `1`..`5`, Shift inverts, one undo per stroke, brush outline | `regia_editor::terrain_tool` | |
| Agent: `terrain.stroke` | `regia_agent::terrain` | `book/agents/terrain-editing.md` |
| Holes: named holes, `terrain.set_hole`, `holes.ron` v2 | ADR-1595 | agent only: no editor mode, and nothing but `upgrade-project` wrote the file |
| Runtime height patches (`TerrainHeightPatches`, `terrain.patch_heights`) | ADR-1595 | `specs/25` § 7.4 / H4 |
| Grass density edits (`TerrainGrassHandle::scale_density`) | ADR-1400 | the grass "scatter brush" |
| Grass sway | `grass.wgsl` | one fixed wind direction `(1, 0.3)`, per-class amplitude and frequency, a global `sway_scale` |

What is missing, and what this spec adds:

1. **Noise and Ramp sculpt modes** (T2.5).
2. **Heightmap export and import**: a save writes only the cooked `.fckd`; the authoring source (the 16-bit PNG) never
   sees a sculpt, so a re-import from source loses every edit. T2.5 asks for 16-bit PNG and RAW both ways.
3. **Wind as data**: the grass's wind direction is a shader constant. Foliage wind (v0.4.0) needs one wind every swaying
   thing reads, on the GPU and on the CPU.
4. **Holes in the editor, and saved**: a hole cut in a session was lost with it (§ 6, § 5).

## 2. Noise and Ramp brush modes

`regia_terrain::brush::BrushMode` gains two variants:

```rust
/// Add seeded value noise in `-amount..=amount` (metres for heights, weight fraction for layers).
Noise { amount: f32, wavelength: f32, seed: u32 },
/// Blend toward the line from `start` to `end` (sample coordinates, column then row): the target at a
/// sample is `start_value`..`end_value` by the sample's projection onto the segment, clamped to it.
Ramp { start: [f32; 2], end: [f32; 2], start_value: f32, end_value: f32 },
```

- **Noise.** `result = original.raise(alpha · amount · (2 · value_noise2(col / wavelength, row / wavelength, seed) − 1))`.
  `wavelength` is in samples (≥ 0.5). `regia_terrain::noise::value_noise2` (quintic fade, integer hash) is the source, so the
  same seed gives the same field on every machine (integer lattice, `f32` interpolation in a fixed order).
- **Ramp.** With `d = end − start`: `t = clamp(dot(p − start, d) / dot(d, d), 0, 1)` (`t = 0` when `|d|² < 1e-6`),
  `target = start_value + (end_value − start_value) · t`, `result = original.lerp_to(target, alpha)`. A ramp stroke stamps at the
  start and smears to the end, so the brush width is the ramp width and the hardness its shoulder.
- `BrushMode::name` returns `"noise"` and `"ramp"`; `inverted()` negates the noise amount and swaps the ramp's ends.
- `validate_brush` refuses a non-finite `amount`, a `wavelength` below 0.5 or non-finite, and non-finite ramp values.
- Helper: `regia_terrain::ramp_mode(map, from_xz, to_xz, from_height, to_height) -> BrushMode` converts world XZ metres into
  sample coordinates; `None` heights take the map's bilinear height at that end.
- Both modes run through the existing stroke path, so they get one undo step, the dirty-rect upload and the incremental
  re-cook for free.

## 3. Heightmap export and import

Module `regia_terrain::height_io` (runtime crate, no new dependency):

```rust
pub enum RawEndian { Little, Big }
pub fn encode_r16(hf: &Heightfield, endian: RawEndian) -> Vec<u8>;
pub fn decode_r16(bytes: &[u8], width: u32, height: u32, endian: RawEndian) -> Result<Heightfield, HeightIoError>;
pub fn square_side(byte_len: usize) -> Option<u32>;               // RAW without a size: a square grid
pub struct HeightExport { pub heightfield: Heightfield, pub clamped_low: u64, pub clamped_high: u64 }
pub fn export_heights(map: &TerrainMap) -> HeightExport;          // TerrainMap::to_heightfield + clamp counts
pub fn import_heights(terrain: &mut Terrain, edits: &mut TerrainEdits, hf: &Heightfield) -> Result<HeightImport, HeightIoError>;
pub struct HeightImport { pub changed: u64, pub dirty: DirtyRect, pub undo: Option<StrokeUndo> }
pub const MAX_IMPORT_UNDO_SAMPLES: usize = 1 << 22;
```

- Encoding is the map's own 16-bit grid: `round((h − offset) · 65535 / scale)` clamped (`TerrainMap::to_heightfield`); the
  import is its inverse (`TerrainMap::new`'s formula), so export → import is the identity on every sample a save could hold.
- Row 0 of the file is the map's row 0 (the −Z edge), as the cooker's PNG import reads it (`import_png_height`).
- An import must match the map's size (`HeightIoError::SizeMismatch` names both). It writes only changed samples, notes
  the dirty rectangle and edit cells on `TerrainEdits` (GPU upload, unsaved tiles) and returns one undo record of the
  changed samples (16 bytes each). Above `MAX_IMPORT_UNDO_SAMPLES` changed samples (64 MiB of undo) the import still applies
  and `undo` is `None`; an import that changes nothing also returns `undo: None`. `dirty` bounds the changed samples (empty
  when none changed) and is the agent reply's `dirty` (ADR-5815).
- PNG: 16-bit grayscale, big-endian samples as the PNG standard requires, written and read by `regia_agent` (which already
  links `png`). RAW `.r16`: headerless, little-endian by default.
- Files are written atomically (temporary file in the same directory, then rename).

### 3.1 Formats

No new Regia format. `.png` (16-bit gray) and `.r16` are external interchange formats with no `format_version`; the frozen
cooked heightfield (`.fckd`) and `holes.ron` v2 are unchanged. `<map>.weights.bin` is the existing `LayerWeightMap` blob
(`LAYER_WEIGHTS_FORMAT_VERSION` 1, unchanged), now also a file: a later shape change bumps that version with a migrator, as
`specs/12` § 4a asks. Migration: none needed.

## 4. Wind (groundwork)

Module `regia_terrain::wind`:

```rust
#[derive(Resource, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TerrainWind {
    pub direction: [f32; 2],   // XZ, need not be unit; default [1.0, 0.3]
    pub strength: f32,         // multiplies every sway amplitude, 0..=4, default 1
    pub frequency_scale: f32,  // multiplies every sway frequency, 0.1..=4, default 1
    pub gust: f32,             // weight of the second harmonic, 0..=1, default 0.35
    pub style: WindStyle,      // Classic (default, the formula below) or Natural (ADR-2446)
}
impl TerrainWind {
    pub fn usable_direction(direction: [f32; 2]) -> bool;                  // finite, length in 1e-6..=1e6
    pub fn clamped(self) -> Self;
    pub fn unit_direction(&self) -> [f32; 2];
    pub fn gust_wave(&self, phase: f32, hz: f32, time_s: f32) -> f32;      // sin(w + phase) + gust · sin(2.3 w + 1.7 phase)
    pub fn sway_offset(&self, pos_xz: [f32; 2], jitter: f32, amplitude_hz: [f32; 2], sway_scale: f32, s: f32, time_s: f32) -> [f32; 2];
}
```

- Later additions (ADR-5815 records them here): `style` and `natural_offset` (the opt-in natural sway, ADR-2446),
  `gpu_words` (the `wind` vec4 the shader reads), `steered_by` / `couple_scene_wind` (a scene wind steers this one,
  ADR-2028 addendum and ADR-2032). `GrassParams` has grown since (288 bytes now); the size is pinned against the parsed WGSL
  (`gpu::grass::tests::gpu_structs_match_the_parsed_wgsl_layout`).
- The default reproduces the old shader constant exactly (direction `(1, 0.3)` normalized on the GPU, factors of 1, the
  0.35 second harmonic), so every existing grass image is unchanged.
- `TerrainGrassHandle::{set_wind, wind}` carry it to the bound `GrassLayer`, which packs it into a new `wind: vec4<f32>`
  of `GrassParams` (`direction.x, direction.z, frequency_scale, gust`) and multiplies `strength` into the sway scale.
- `TerrainPlugin` copies a changed `TerrainWind` resource into the world's `TerrainGrassHandle` each frame (no work when
  unchanged).
- `sway_offset` is the CPU mirror of the blade's sway: future foliage (scatter meshes) and gameplay (cloth, audio) read
  the same wind. The sun-cascade pass draws the rest pose while the sway clock is still; once the clock moves, the cascades named by the dynamic-caster class draw the swayed blade (spec 88 § 2.4).
- Wind is visual only and never feeds the simulation; it is a function of `(position, time)` with no state, so two renders
  with the same clock sway the same.

## 5. Agent-protocol surface

| Method | Params | Reply | Codes |
|---|---|---|---|
| `terrain.stroke` (extended) | `mode: "noise"` with `strength` (m), `wavelength` (m, default 8 samples), `seed`; `mode: "ramp"` with `center` → `to` (or the last `path` point), optional `from_height`, `to_height` | as before | `F0003` |
| `terrain.export_heights` | `path`, `format` (`"png"` / `"r16"`, default by extension), `endian` (`"little"` / `"big"`), `entity` | `{entity, path, format, width, height, bytes, clamped_low, clamped_high}` | `height_io_bad_format`, `height_io_write` |
| `terrain.import_heights` | `path`, `format`, `endian`, `width`/`height` (RAW; default a square from the byte count), `entity` | `{entity, path, format, width, height, changed, undoable, dirty}` | `height_io_bad_format`, `height_io_read`, `height_io_decode`, `height_io_size_mismatch` |
| `terrain.save_holes` | `path` (a map or level file, or a `.holes.ron` name, under the project root) | `{path, bytes, shapes, named}` | `holes_write` |
| `terrain.save_paint` | `path` (a map or level file, or a `.weights.bin` name), `entity` | `{entity, path, width, height, layers, bytes}` | `F0003`, `height_io_write` |
| `terrain.load_paint` | `path`, `entity` | `{entity, path, width, height, layers}` | `F0003`, `height_io_read`, `height_io_decode`, `height_io_size_mismatch` |
| `terrain.wind` | none | the `TerrainWind` in effect, `unit_direction`, `grass_linked` | |
| `terrain.set_wind` | any of `direction` `[x, z]`, `strength`, `frequency_scale`, `gust`, `style` (ADR-2446) | the clamped `TerrainWind` | `wind_bad_direction`, `wind_bad_style` |

An import is logged to `TerrainStrokeLog` like a stroke, so the editor turns it into one undo step.

Paths are relative to the project root, without `..`, and may not leave it through a symlink: the deepest part of the path
that exists is resolved and checked, for an import and for an export that would create directories. A file over
`2 · 16384² + 1 MiB` bytes (a `.weights.bin` for `terrain.load_paint`: `2^30 + 1 MiB`, the largest weight map
`terrain.save_paint` can write, ADR-5815) is refused before it is read (`height_io_decode`).

Guide pages: `book/agents/terrain-editing.md` (new sections: noise, ramp, export/import) and `book/agents/terrain-wind.md`.

## 6. Editor

- Hotkey `6` selects Noise (strength in metres, seed advances each stroke so repeated strokes do not stack the same field);
  `7` selects Ramp: press at the start, drag, release at the end; the stroke runs on release from the press point to the
  release point with the heights under both ends.
- The status line shows the new mode names.
- Hotkey `8` selects Hole. A click cuts a round hole of the brush's size; a drag (longer than a quarter of the radius) cuts a
  slot along it as wide as the brush (`HoleShape::segment`); Shift removes the named hole under the release point. Ids are
  the lowest free `editor_hole_<n>`. Each edit is one undo entry (`TerrainHoleCommand`), through the linked draw's
  `TerrainHoles` when there is one, else on the `Terrain`.
- The editor does not load `<map>.holes.ron` with a level, so the first hole edit of a session merges the file in (named
  holes by id, unnamed footprints when the session has none): a save cannot drop them. A file that exists and cannot be
  read refuses the edit, so it is never replaced.
- **One save.** `level_io::save_level_and_terrain` is the save behind Save, Save As, saving on close and `editor.save_level`:
  the level, the re-cooked tiles (`save_terrain_edits`), `<map>.holes.ron` after a hole edit (`save_terrain_holes`) and
  `<map>.weights.bin` after a paint edit (`save_terrain_paint`), atomically, next to the level. A level has one of each
  file, so holes or paint unsaved on two terrains refuse the save instead of keeping the last writer (ADR-1957). Before this the UI's saves wrote
  the level alone, so a sculpt survived only an agent's `editor.save_level`, and nothing wrote painted weights. The autosave
  stays `save_level_file`: a recovery copy must not touch the cooked heightfield or the level's files.
- Reply fields of `editor.save_level`: `terrain_tiles_recooked`, `terrain_holes_file`, `terrain_paint_file`.
- A level does not reference a terrain: a `Terrain` in the editor comes from the host, which also inserts
  `TerrainCookTarget`, `TerrainPaint` and loads the holes and weights files. See § 12.

## 7. Determinism

- Noise: integer lattice hash plus `f32` interpolation in a fixed order: the same seed, brush and path give bit-identical
  heights on one build (cross-platform bitwise equality of `f32` arithmetic is not promised; the exported 16-bit file is the
  shareable result).
- Ramp, import, export: pure functions of the map and the inputs.
- Wind: stateless function of position and clock.

## 8. Tests and acceptance

- `brush`: noise is zero-mean over a large stamp, bounded by `amount`, reproducible by seed and different across seeds;
  ramp writes the interpolated heights along its axis at full alpha and leaves samples outside the brush; inverted modes.
- `edit::validate_brush` refuses bad noise and ramp parameters.
- `height_io`: R16 round trip in both endians; export → import is the identity; size mismatch refused; clamp counts;
  import undo restores the map bit for bit; the undo cap keeps the record at the limit and returns `None` one sample past it,
  with the heights applied.
- `wind`: the default equals the legacy constant formula; direction normalization; `clamped` repairs non-finite input;
  `GrassParams` stays 16-byte aligned and matches the WGSL size (shader parse test).
- Agent: noise and ramp strokes, export → import round trip through PNG and R16 files in a unique temp dir, size mismatch
  code, wind get/set and its push into the grass handle, a symlink cannot leave the project root, an oversize file is
  refused before it is read.
- Editor: a threaded Noise stroke matches the synchronous path bit for bit. Hole mode: click, drag, undo, redo, Shift removal,
  a linked draw; a save keeps the holes on disk, follows undo, writes nothing without an edit, and never replaces a broken file.
- `terrain.save_holes`: the file loads back, closed holes keep their state, a bad path and a failed write get their codes.
- `terrain.save_paint` / `terrain.load_paint`: a round trip into a fresh terrain, the upload mark, the unsaved mark, a grid
  mismatch, a missing and a broken file. Editor: the weights file is written only after a paint edit.
- The Save path itself (`EditorShell::save_level`): a sculpt, a hole and a paint stroke reach their files.
- No golden changes: the default wind reproduces the old shader.

## 9. Platforms

Pure Rust and WGSL; no platform code. Linux, Windows and Android aarch64 build the same code (`cargo ndk -t arm64-v8a
check -p regia`). The editor parts are desktop-only, as the editor is. File export on Android goes through the agent's
paths like any other agent file write.

## 10. Performance budget

- Noise and Ramp cost what Flatten costs per sample plus one noise lookup: inside the existing stroke budget.
- Export/import: one pass over the map (4097² ≈ 16.8 M samples: one allocation of 32 MiB for the file); editor-time only,
  under a `tracing` span.
- Wind: one 16-byte uniform field, written with the grass params each frame; no new pass.

## 11. Provenance

- Value noise with a quintic fade: K. Perlin, "Improving Noise", SIGGRAPH 2002 (already the source of `noise.rs`).
- Ramp: linear interpolation along a segment projection (standard vector algebra).
- RAW 16-bit heightmaps: headerless row-major unsigned 16-bit samples (the common interchange layout);
  PNG 16-bit grayscale: W3C PNG Specification (Third Edition), § 7.1 (big-endian samples), § 11.2.2 (IHDR bit depth 16).
- Grass sway as a sum of two sines phased by position along the wind: the blade model already in `grass.wgsl`
  (ADR-1400); wind as one shared parameter block read by every swaying thing is in-house design.

## 12. Out of scope

- Foliage (scatter mesh) sway and the dynamic-caster shadow class (v0.4.0, N18).
- A wind field that varies in space (volumes, turbulence textures); weather.
- Hole painting as a raster brush (holes are shapes, ADR-1595).
- A level that names its terrain, so the stock editor can open one without a host. It needs a field in the frozen
  `.level.ron` and a loader; until then `TerrainCookTarget` and the holes and weights files stay the host's to wire.
- A grass density brush. Grass density edits stay on `terrain.set_grass` and `TerrainGrassHandle::scale_density`: the density
  mask is baked at load and has no saved form, so a brush would lose its work on reload. It needs a sidecar format (a
  `format_version`, a migrator row) and a replay at load. Mesh-instance painting (T2.5) is § 13.
- Erosion and other simulation brushes; the 2-point ramp gizmo beyond press/release.

## 13. Painted mesh instances (ADR-1936)

The T2.5 line "painted instances (density, scale/rotation jitter, slope/height filters) stored per tile; GPU instanced with
per-cell culling and distance fade". Module `regia_terrain::scatter::painted`.

**Storage.** `PaintedScatter`: per class (by name), a mesh path table and a map of 64 m tiles anchored at the world origin, each
an `Arc` of instances sorted by id. A clone shares the tiles, so a snapshot costs one pointer per tile and an edit copies the
tiles it touches. `PaintedInstance`: `id`, `mesh`, `flags`, `position`, `normal`, `yaw`, `scale`, `shade`, `tilt`. Classes and meshes
are names and paths, so reordering a `.scatter.ron` moves nothing; what the set no longer names is kept and not drawn. Tilt and
shade are stored as draws and resolved against the class at placement, so a class edit restyles what was painted. `flags` is
reserved for per-instance foliage wind (spec 88): v1 writes 0 and keeps what it reads. Wind itself is not in this section.

**Brush.** `ScatterBrush`: class, mode (paint or erase), shape, hardness, strength, density (per square meter), scale `[min, max]`
(a multiplier on the class's scale), slope and height filters, `min_distance`, seed. `ScatterBrush::for_class` takes the filters
from the class; the class's layer mask, exclusions and holes rule always apply.

**Candidates.** A grid of cell `1 / sqrt(density)` meters anchored at the world origin. Cell `(cx, cz)` draws ten values from PCG32
seeded by a SplitMix64 mix of the set seed, the class name's FNV-1a hash, the brush seed, the cell size and the coordinate. Its
position is `(cell index + draw) · cell`, and its id a second mix of the same inputs. Coverage at a candidate is the brush
falloff (`CircleRaster` / `RectRaster` strength through `apply_hardness`) at the nearest point of each path segment, maximized over
the path. A candidate is placed when its draw is below `coverage · strength`, it passes the filters and its id is not in its tile. So
density is a target, a repeated stroke adds nothing, and a path cut into segments gives the same set as the whole path (with
`min_distance` 0; with it, the order of arrival decides who is kept).

**Erase.** Removes instances under the brush that pass the filters, each by its own draw (seeded by its id and the brush seed)
against `coverage · strength`.

**Journal.** A stroke returns `StrokeUndo::Scatter(ScatterStroke)`: the instances added and removed, with the mesh table. It
goes on `TerrainStrokeLog` from the agent and on the editor's undo stack through `TerrainStrokeCommand`; `StrokeUndo::restore`
gains a `scatter` argument. An undo restores the bytes of the store. A drag in the editor is one entry (`ScatterStroke::merge`).

**Drawing.** `ScatterField::set_painted(&PaintedScatter)` compares the store's generation (a process-wide counter) and returns when
it has that store. Otherwise it diffs tile `Arc`s by pointer, drops the field tiles and pending placements under changed storage
tiles and keeps a snapshot; `update` places those tiles again. `place_tile` appends the painted instances of its box to the seeded
ones as `build_instance` sees them, so culling, LOD, fade, shadows, contact and impostors are the existing path. The GPU impostor
layer re-uploads on a painted edit, because a tile id does not show that a tile's content changed. `extract_runtime_scatter` syncs
the store of the terrain with the lowest entity id.

**Seating.** Instances keep height and normal. `TerrainEdits` tracks a re-seat rectangle (`note`, `note_scatter`) and
`reseat_painted_scatter` (`TerrainPlugin`, `Update`) re-seats what lies in it, two samples wider for the lookup reach. Tiles nothing
moved in are not copied, and re-seating after an undone sculpt gives the old bytes. A sculpt does not re-run filters.

**File.** `<map>.scatterpaint.bin`: `docs/formats/terrain.md` § 10. The same instances give the same bytes. It ships raw beside the map.

**Limits.** 4 Mi candidates per stroke, 16 Mi instances per store, density 64 per square meter, brush reach 4096 m, 2 GiB read.

**Agent.** `terrain.scatter_stroke`, `terrain.scatter_paint`, `terrain.save_scatter_paint`, `terrain.load_scatter_paint`
(`book/agents/terrain-scatter-paint.md`), codes `scatter_paint_*`, guide topic `terrain-scatter-paint`.

**Editor.** Hotkey `9` selects Scatter: `-` `=` density, `,` `.` class, Shift erases, instances appear as the drag goes, one undo entry.
It needs a scatter set (`RuntimeScatter` or `ScatterPaintSet`). `frame_effects` loads the level's file, once per level and terrain;
`save_terrain_scatter` writes it from every save, after an edit or when the level has no file yet. A file that cannot be read refuses
edits and is never replaced. `editor.save_level` replies `terrain_scatter_file`.

**Tests.** `regia_terrain::scatter::painted::tests`, `crates/regia_terrain/tests/scatter_paint.rs` (the field, tiles, the journal, the ECS
frame, sculpt re-seat, a lavapipe frame), `regia_agent::terrain_scatter_paint::tests`, `regia_editor::terrain_tool::tests` (mode 9, undo,
save and load, a broken file), `regia_editor::shell::tests::save_level_saves_what_the_terrain_tool_edited`.

**Not here.** Foliage wind on instances (spec 88, `flags` keeps the format open). Filters against painted layer weights (layer masks read
the procedural weights, as seeded placement does). A class picker panel in the editor. Stroke previews. Per-instance edits (move one tree).

## 14. Terrain editing remainder (ADR-2411)

Built after § 12 listed them as out of scope: the level that names its terrain and the grass density brush, plus editor UI
for § 3's heightmap export/import and for `.scatter.ron` rules. `<map>.weights.bin` was already registered (ADR-2316).

- **The level names its terrain.** Scene format v12: `LevelSettings::terrain` (`layers`, `map`, `world_width_m`,
  `height_scale_m`, `height_offset_m`; `docs/formats/scene-files.md`). `regia_terrain::level_terrain` loads it from the
  cooked heightfield (`terrain/heightfield` under the layer file's map id) and the side files under `map` (empty: the level's
  own name); `LevelTerrainPlugin` is the opt-in runtime system. The editor builds it when the level opens, with a
  `TerrainCookTarget`, and every side file follows `map`. A v11 level migrates naming none: same side files, the host's
  heightfield, the same terrain. § 6's last bullet and § 12's first out-of-scope item are superseded.
- **Grass density.** `regia_terrain::grass_density::GrassDensityPaint` and `<map>.grassdensity.bin` v1
  (`docs/formats/grass-density.md`): a deterministic brush with exact undo, read by the grass bake as per-class multipliers.
  Editor Terrain mode `0`, one undo entry per drag, written by every save after an edit. § 12's grass density item is
  superseded; a live GPU re-bake per stroke stays with the host.
- **Editor Terrain panel** (`regia_editor::terrain_panel`, `Panel::Terrain`): level terrain status, heightmap Export/Import
  (16-bit PNG and RAW; the height scaling is `docs/formats/terrain.md` § 11), the density target, the scatter rule editor
  (fields by name, add/remove class, canonical RON save).
- **Agent:** `terrain.level_terrain`, `set_level_terrain`, `load_level_terrain`, `grass_density_stroke`,
  `undo_grass_density`, `redo_grass_density`, `grass_density`, `save_grass_density`, `load_grass_density`,
  `scatter_rules`, `set_scatter_rule` (`book/agents/level-terrain.md`).
- **Tests:** `regia_scene::format::tests` (v11 → v12, validation), `crates/regia_terrain/tests/level_terrain.rs` (an old
  level migrates and loads the same terrain by digest; a named map; error codes; the plugin), `regia_terrain::grass_density::tests`,
  `regia_editor::terrain_panel::tests` (heightmap round trip through PNG and RAW both endians; density determinism and undo;
  the viewport drag; the scatter panel's headless edit and canonical save; the level terrain load), `regia_agent::terrain_level::tests`,
  `regia_cli` upgrade-project (v11 level to v12; the density version probe), the fuzz targets `scene.terrain` and
  `terrain.grass_density`.

## 15. As built: New terrain (ADR-7727)

The editor creates a terrain: Terrain panel ▸ **New terrain…**, Content ▸ New ▸ **Terrain…** and the editor method
`editor.terrain_create` (`regia_editor::terrain_create`). Sizes `2^n + 1` from 129 to 4097 samples a side, cell sizes
0.25–4 m, a height range, layers (grass, rock and dirt with starter content, else `ground`), or a 16-bit PNG/RAW to start
from (Import as new keeps its samples). The files follow `docs/formats/terrain.md` § 9 in `assets/<folder>/<name>/`; the
cook runs as the cancellable job `terrain_create:<n>` from a staging folder, so a cancel leaves nothing; the level's
`LevelSettings::terrain` then names it as one undo step (undo keeps the files) and the editor builds it. Tests:
`regia_editor::terrain_create::tests` (a 129² create loads through the runtime loader, sculpts and undoes; a cancel at each
check leaves no output; the agent and the dialog write the same bytes; the refusal codes; import as new keeps the
samples), `crates/regia_editor/tests/terrain_create.rs` (the dispatcher, `terrain.level_terrain`, an async job).

## 15. Non-destructive edit layers (ADR-7825, P-236)

- **Model.** A terrain may carry a stack of named edit layers over its base heights (`regia_terrain::edit_layers`). Blends:
  add (`opacity × delta`) and set (toward the layer's height by `coverage × opacity`). The terrain is the base folded through
  every enabled layer, bottom first; consecutive add layers are summed in `f64` in layer-id order, so their order is irrelevant;
  a set layer's position matters. Up to 32 layers; a layer can be hidden, locked, faded and reordered.
- **Strokes.** Every height stroke (editor tool, `terrain.stroke`, height edits drained from the stroke log) writes the active
  layer or the base: it lands on the map as before, then its change is moved into the layer and its samples are rewritten from
  the fold. Undo restores map and layer bit for bit. A locked, hidden or fully transparent target refuses the stroke.
- **Recompute.** Show/hide, opacity, reorder and delete recompute only the changed layer's bounds; the stack counts recomputed
  samples. Flatten into base bakes the enabled layers into the base without changing a bit of the map.
- **Files.** `<map>.terrain.editlayers` v2 beside the level (`docs/formats/terrain-editlayers.md`); v1 loads through the migrator
  with the same height bits, no weights and no procgen target. The cooked heightfield holds the flattened result and does not
  change format. A file whose height fold no longer matches the cooked map is refused as stale.
- **Weights.** Each sample's material weights are the base plus every enabled layer's opacity-scaled signed delta. Negative
  channels clamp to 0, then the channels are divided by their sum. A sum of 0 is channel 0 at weight 1 (ADR-8381).
- **Procgen.** A layer may name a procgen graph and output. Publish writes that layer only; cleanup removes values from that
  layer only. A brush on an owned layer is refused until the target is detached. Detach is undoable.
- **Agents.** `terrain.edit_layers`, `terrain.edit_layer_create`, `terrain.edit_layer_set`, `terrain.edit_layer_delete`,
  `terrain.edit_layer_reorder`, `terrain.edit_layer_flatten`, `terrain.edit_layer_weights`,
  `terrain.edit_layer_paint_weights`, `terrain.edit_layer_procgen`; `terrain.stroke { edit_layer }`
  (`book/agents/terrain-editing.md`).
- **Tests.** `regia_terrain::edit_layers::tests` (add layers add up, toggle exactness, reorder for add and set, bit-exact undo,
  regional recompute, cooked map equals a hand-flattened map, flatten, refusals, the sidecar round trip and its refusals,
  `weight_layers_renormalize_to_one`, `brush_on_owned_layer_is_refused_until_detached`,
  `a_v1_file_migrates_with_identical_heights`, `weights_and_procgen_target_save_reload_and_undo`),
  `regia_procgen` `a_procgen_edit_lands_in_its_layer`,
  `regia_editor::terrain_layers::tests` (tool strokes into the active layer with undo/redo, panel toggles as undo steps, the
  keyboard path, the sidecar beside the level), `regia_agent::terrain_edit_layers::tests`, the fuzz target
  `terrain.edit_layers`.

### Unlayered writer preservation (ADR-8202)

Runtime height bands and procedural publications update the base of an existing edit stack.
The stack retains the last composed heights in memory, detects direct map changes before a
regional recompute, and absorbs their displacement into the base. Stack operations reconcile
the map before taking undo snapshots; editor save reconciles before writing the v2 sidecar.
Undoing a prior layer operation preserves later unlayered base edits. Named-layer strokes
and their undo update the composed-height bookkeeping so they are not counted as base edits.
A Set layer can cover the base while enabled. ADR-8381 accepts the weight rule and the persisted
procgen target that ADR-8202 deferred, as one v1 → v2 sidecar bump.
