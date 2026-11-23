# Terrain sources and shaded overviews

Use `terrain.inspect_source` to validate an authoring heightfield and inspect its
meter heights before connecting it to a host's terrain. Use `terrain.bake_overview`
to create a north-up shaded PNG for a map picker or overview. Both run on the CPU.
Neither changes scene heights. Existing `terrain.import_heights` remains the
undoable PNG/RAW edit that uses the destination terrain's scale and offset.

## Inspect a source

```json
{"method":"terrain.inspect_source","params":{"path":"terrain/source.json"}}
```

Schema-2 JSON contains unsigned 16-bit counts in row-major order:

```json
{
  "schema": 2,
  "grid": { "cols": 3, "rows": 2, "cell_m": 2.0,
            "row0": "south", "col0": "west" },
  "units": { "kind": "counts", "meters_per_count": 0.125, "zero_counts": 1000 },
  "counts": [992, 1000, 1008, 1016, 1024, 1032]
}
```

The height formula is `(count - zero_counts) * meters_per_count`; this example's
six samples are −1, 0, 1, 2, 3 and 4 meters. All counts and dimensions are retained.
`row0` defaults to `south` (minimum Z); `col0` defaults to `west` (minimum X).
Other orientations are refused. Unknown application metadata is ignored within
the depth and work budgets below.
`grid.sample0_x_m` and `grid.sample0_z_m` optionally locate the first sample;
otherwise each is `-dimension * cell_m / 2`. The last sample is at
`sample0 + (dimension - 1) * cell_m`. These coordinates need not be symmetric.
No origin recentering or rectangular-to-square resampling happens during import.

For a PNG, supply its positive height scale and square-cell spacing:

```json
{"method":"terrain.inspect_source","params":{
  "path":"terrain/source.png",
  "png":{"meters_per_count":0.125,"cell_m":2.0}
}}
```

Only noninterlaced, nonanimated grayscale 8/16-bit PNG is accepted. Eight-bit
counts expand by 257. The uncompressed Latin-1 `tEXt` keyword
`terrain_zero_counts` holds an integer in 0..=65535. Text before or after the image
is read. Duplicate, malformed or compressed datum metadata is refused.
`png.datum_keyword` selects a legacy application's keyword without modifying its
image. If the keyword is absent, the default is an error. Explicit alternatives:

- `png.zero_counts`: fallback integer datum, used only when text is absent.
- `png.center_zero: true`: fallback to sample `(width / 2, height / 2)`.
- `png.sample0: [x, z]`: optional first-sample coordinates.

Do not combine `zero_counts` and `center_zero`. Embedded metadata takes precedence
over a fallback; malformed metadata never falls back.

The result contains `width`, `height`, `samples`, `meters_per_count`, `zero_counts`,
`cell_m`, `sample0`, `min_m`, `max_m`, `corners_m` (southwest, southeast, northwest,
northeast), `row0` and `col0`. Inspection requires `read` permission and works in
read-only sessions. A project with an `AssetServer` is required; no terrain entity
or renderer is needed.

## Bake an overview

```json
{"method":"terrain.bake_overview","params":{
  "path":"terrain/source.json","output":"previews/terrain.png",
  "overview":{"size":[512,512],"relief":1.0,"contour_step_m":8.0,
              "blur_passes":1,"light":[-1.0,1.0,1.0],"frame_pixels":2}
}}
```

PNG input accepts the same `png` object as inspection. The output must be a
separate `.png` under the project root. A successful bake atomically replaces an
existing preview. Source files are never overwritten. This method requires
`assets` permission and is refused in read-only sessions.

The result contains `source` (the inspection report), `output`, `width`, `height`,
`bytes` and `north_up: true`. The image is RGB8; its top edge is maximum Z, its left
edge minimum X. With the default `fit: "stretch"` a square output intentionally
stretches a rectangular grid, but slope shading uses each axis's actual scale;
`fit: "preserve_aspect"` keeps one scale and letterboxes (see "Registered overviews"
below). All filtering and exaggeration affect only the preview.

`overview` fields are optional:

| Field | Default | Meaning |
|---|---|---|
| `size` | `[512,512]` | Width/height, each at least 2 |
| `relief` | `1` | Finite, nonnegative vertical exaggeration |
| `contour_step_m` | absent | Positive finite contour interval; absent disables contours |
| `blur_passes` | `1` | 0..=8 passes of a separable five-tap Gaussian approximation |
| `light` | `[-1,1,1]` | Finite nonzero direction to light, `[X,up,Z]` |
| `ambient` | `0.5` | Flat-ground brightness in 0..=1 |
| `contrast` | `1` | Finite, nonnegative shading contrast |
| `frame_pixels` | `0` | Gray border width, at most half the smaller output side |
| `fit` | `"stretch"` | `"stretch"` (legacy, fills the image) or `"preserve_aspect"` (ADR-8596) |

The bake area-averages samples, flips north-up, blurs, then shades using surface
slopes. The 1st/99th percentile heights modulate brightness; contour-band changes
are darkened. It is a presentation bake, with no lockstep sampling contract and
no promise to reproduce an older tool's preview pixels.

## Limits and errors

Both methods accept an optional `limits` object: `max_bytes`, `max_samples`,
`max_json_depth`, and `max_unknown_values`. Omitted fields use engine defaults.
Effective ceilings are the minimum of the request, the world's
`regia_agent::TerrainSourcePolicy.limits` (defaults when absent), and fixed engine
maxima: 134,217,728 encoded bytes, 16,777,216 samples, depth 32 and 65,536 ignored
JSON values/keys. Hosts insert the policy after loading their project settings;
requests cannot raise it. Unknown or non-integer limit fields are invalid params.

Inspection reports `effective_limits`; baking reports `source.effective_limits`.
Source decode/limit errors also include `data.effective_limits`. Zero budgets are
allowed; a zero metadata budget permits recognized fields only. Depth counts the
root object as one, grid/units/counts as two and their scalar values as three.
Unknown field names, values and nested object keys share the metadata budget.
PNG ignores the JSON-specific ceilings but reports the same policy object.

The JSON reader charges a sample before decoding or growing its bounded vector;
`counts` may appear before or after dimensions. Dimensions use checked
multiplication. Ignored data has bounded work/depth and no retained JSON tree.
Duplicate recognized fields, trailing data and incomplete documents are refused.
Source readers validate integer ranges before narrowing, finite height ranges,
positive pitch and representable extents. JSON float fields still deserialize
directly into f32, including midpoint decimal tokens. PNG checks dimensions before
allocating the pixel buffer. Overview outputs remain capped at 4,194,304 pixels.

| Code | Meaning |
|---|---|
| `height_io_bad_format` | Input extension is not JSON/PNG, or output is not PNG |
| `height_io_read` | Input cannot be opened or read |
| `height_io_decode` | Source, metadata, mapping or overview setting is invalid |
| `height_source_limit_bytes` | Encoded source exceeds effective `max_bytes` |
| `height_source_limit_samples` | Counts or checked dimensions exceed effective `max_samples` |
| `height_source_limit_depth` | JSON nesting exceeds effective `max_json_depth` |
| `height_source_limit_metadata` | Ignored JSON values/keys exhaust `max_unknown_values` |
| `height_source_cancelled` | Native observer cancelled parsing; no source returned |
| `height_source_allocation` | Bounded sample allocation could not be satisfied |
| `height_io_write` | PNG encoding or atomic output write failed |
| `F0002` | Missing/wrong parameter, conflicting fallback, unsafe path or source overwrite |
| `F0003` | No project asset server |
| `F0012` | Required permission missing or bake requested read-only |

Paths cannot contain `..` or resolve outside the project root through a symlink.
Malformed sources and invalid options produce no output. Read errors never mutate
a scene. The dispatcher tests exercise both methods, permission refusal, exact
meter values and PNG decode/orientation without golden images.

## Native API

`regia_terrain::source::TerrainSource::{from_json,from_png}` returns validated
counts, mapping accessors, `height_m`, `heights_m` and a raw cooked `heightfield`.
The host retains the mapping alongside that raw grid. In particular, do not feed
a rectangular source into a centered square `TerrainMap` and assume placement
will survive. `CountMapping::terrain_map` builds the exact runtime map or refuses
(see "Cook a count source" below, ADR-8471).

`regia_terrain::overview::bake_overview` returns an `OverviewImage`; `rgb` exposes
pixels and `encode_png` returns bytes. These functions perform no filesystem IO.


## Budget workflow and fuzz coverage (ADR-8362)

```json
{"method":"terrain.inspect_source","params":{"path":"terrain/source.json","limits":{"max_samples":4}}}
{"method":"terrain.bake_overview","params":{"path":"terrain/source.json","output":"preview.png","limits":{"max_samples":4}}}
{"method":"fuzz.run","params":{"target":"terrain.source_json","seed":8237,"iterations":256}}
{"method":"fuzz.run","params":{"target":"terrain.source_png","seed":8237,"iterations":256}}
```

For a six-sample fixture both source methods return `height_source_limit_samples`;
an existing preview is preserved. The `terrain_source` dispatcher integration
workflow queues `play.wait_until` on a scene transform, attempts a refused bake,
advances one frame, then uses `play.expect` to verify the transform is unchanged.
It also verifies the preview's bytes and effective request/project ceilings.
The two fuzz targets bound inputs to 16 KiB, 64 samples, depth eight and 64 ignored
values/keys, and assert deterministic JSON work/storage counters. They read external
source formats with no writer/round-trip claim.

Native `TerrainSource::from_json_with_observer` calls an observer with
`JsonImportProgress` before parsing, before each sample decode and when charging ignored values/keys,
after each stored sample and before returning the validated source. Counters show
charged/stored samples, sample vector capacity, ignored values and maximum depth.
Returning `ControlFlow::Break(())` cancels and drops all partial storage. Token
length and work between callbacks remain bounded by `max_bytes`. The ordinary
`from_json` API uses the same reader with an always-continue observer.
`SourceError::code()` provides the stable code; its existing string payload remains.

Each of the six `height_source_*` codes also has an `errors.explain` entry with
its meaning and recovery guidance, for example
`{"method":"errors.explain","params":{"code":"height_source_limit_samples"}}`.

## Cook a count source and preview its placement (ADR-8471)

`regia cook` keeps a count source's datum, scale, cell size and registration when the
terrain's `heightfield.json` (beside `heightfield.png`) carries a schema-2 mapping:

```json
{"schema": 2,
 "grid": {"cols": 33, "rows": 33, "cell_m": 0.5, "sample0_x_m": 37.25, "sample0_z_m": 37.25},
 "units": {"kind": "counts", "meters_per_count": 0.03125, "zero_counts": 20000}}
```

The PNG's counts are cooked verbatim; the cooked terrain index (version 2) stores the
resolved mapping, and a level that names the terrain loads every sample at
`sample0 + (col, row) * cell_m` with height `(count - zero_counts) * meters_per_count`.
The level's `height_scale_m` and `height_offset_m` do not apply. The full rules are in
`docs/formats/terrain.md` § 9a. Changing only the datum, scale, cell or registration
recooks the terrain.

Preview what the cook will persist, and whether a level will load it, before cooking:

```json
{"method":"terrain.inspect_source","params":{
  "path":"maps/ridge/heightfield.png",
  "png":{"meters_per_count":1.0,"cell_m":1.0,"zero_counts":0},
  "mapping":"maps/ridge/heightfield.json"
}}
```

`mapping` names a project-relative `heightfield.json` (at most 1 MiB) whose schema-2
mapping replaces the request's own scale, datum, cell and first sample, exactly as the
cook applies them (`png` still has to parse, but its numbers are replaced). A file
without `"schema": 2` is refused with `height_io_decode`.

Every inspection now also reports:

| Field | Meaning |
|---|---|
| `world_min`, `world_max` | World X/Z of the first and the last sample (the registration and the far corner) |
| `mapping` | `{ cols, rows, meters_per_count, zero_counts, cell_m, sample0_m, world_min, world_max }`, the values a cook persists |
| `runtime_map` | `{ loadable: true, grid_origin, world_width }` when a level loads the source exactly, else `{ loadable: false, reason }` |

The runtime map is square in world space with one grid origin on X and Z, so a
rectangular grid, or one whose center differs on X and Z, is not loadable. It still cooks
and round-trips exactly; `terrain.load_level_terrain` refuses it with
`level_terrain_mapping` and never resamples it. `terrain.load_level_terrain` reports the
loaded `source_mapping` (null for a legacy terrain).

Native: `CountMapping::from_heightfield_json`, `TerrainSource::{mapping, from_counts,
sample_world}`, `CountMapping::terrain_map` and `TerrainIndex::source_mapping`.

The `terrain_source` dispatcher test `inspect_with_a_cook_mapping_previews_world_bounds_before_apply`
queues `play.wait_until` on a scene transform, previews a loadable and an unloadable
mapping, advances one frame and checks with `play.expect` that the scene did not change.
`cargo test -p regia_asset_pipeline --test terrain_source_cook_mapping` covers the cook,
the index, the level load, recooks, cache invalidation and legacy files.

## Import or reimport a source onto a terrain (ADR-8541)

The editor imports a count source onto the heights of a terrain in the open level in
four steps: start, preview, apply or cancel. The Terrain panel's **Import source**
section, these methods and `regia terrain source-inspect` all read the source through
one service, so their previews agree on bounds, datum and units.

```json
{"method":"editor.terrain_source_start","params":{"path":"assets/maps/ridge.json"}}
```

answers `{ "job": "terrain_source:3", "state": "running" }`. The parameters are those of
`terrain.inspect_source` (`path`, `png`, `mapping`, `limits`) plus `entity` (the
destination terrain; default the selected terrain, else the lowest id) and `conversion`
(see below). A worker job (`agent.job_status` lists it; `editor.cancel_job` and
`editor.terrain_source_cancel` stop it) reads and decodes the source, encodes it in the
terrain's 16 bits and compares it with a snapshot of the terrain. Nothing in the world
or on disk changes. Wait for the preview without polling:

```json
{"method":"play.wait_until","params":{"condition":{"events_contain":
  {"kind":"terrain_source","contains":"terrain_source:3 staged"}},"timeout_ms":60000}}
```

Every transition is an agent event of kind `terrain_source` with
`{ "event": "<job> <state>", "job", "state" }` (`running`, `staged`, `applied`,
`cancelled`, `failed`).

`editor.terrain_source_status { job? }` then answers `state: "staged"` and `preview`:

| Field | Meaning |
|---|---|
| `source` | The `terrain.inspect_source` value: size, `meters_per_count`, `zero_counts` (datum), `cell_m`, `sample0`, `world_min`/`world_max`, `min_m`/`max_m`, `effective_limits`, `mapping`, `runtime_map` |
| `digest` | BLAKE3 of the source (and mapping) bytes the preview read |
| `destination` | The terrain: `entity`, `cols`, `rows`, `cell_m`, `world_min`/`world_max`, `height_range_m`, `generation` |
| `mapping` | `registration_match`, `registration_shift_m` (source first sample minus the terrain's), `cell_ratio`, `heights_in_range`, `requantized` (the source's count encoding is not the terrain's 16-bit encoding), `needs` |
| `changes` | `changed` samples of `total`, `dirty` rectangle, `max_delta_m`, `clamped_low`, `clamped_high` |
| `undo` | `limit`, `undoable`, `acknowledge_required` |

While the worker runs, `status` answers `running` with `progress { fraction, message }`;
after the end it answers the last import's `applied`, `cancelled` or `failed` report
(`error { code, message }` for a failure).

### Apply

```json
{"method":"editor.terrain_source_apply","params":{"job":"terrain_source:3"}}
```

answers `{ job, state: "applied", changed, undoable, backup }`. The guards, in order:

- **Destination mapping.** A source of another grid size fails the job with
  `terrain_source_grid_mismatch` (an import never resamples). A source whose cell size
  or first sample differ from the terrain's by more than 1 mm needs the conversion
  `"registration"` (sample `(c, r)` goes onto the terrain's sample `(c, r)`); heights
  outside the terrain's height range need `"clamp_heights"`. Without them apply answers
  `terrain_source_mapping_mismatch` with `data.needs` and the import stays staged.
  Give them in `conversion` on start or apply: `"conversion": ["registration", "clamp_heights"]`.
- **Stale source.** The source (and mapping) bytes are read again; if they changed since
  the preview, `terrain_source_stale` ends the import. Start again for a fresh preview.
- **Changed terrain.** If the terrain's heights changed since the start (a sculpt, another
  import), `terrain_source_destination_changed` ends the import.
- **Undo limit.** At most `undo.limit` changed samples (default 4,194,304; a host may
  lower it with `TerrainSourceImportPolicy`) is one undo step, as the heightmap import.
  A larger import answers `terrain_source_undo_limit` (`data.changed`, `data.undo_limit`)
  unless `acknowledge_undo_limit: true`. Then the original heights are first written as
  a 16-bit PNG to `<state>/terrain_source_backups/<job>-<entity>.png` (the `backup` field,
  project-relative; `terrain_source_backup_failed` stops the apply if it cannot be
  written), the heights are applied without an undo step and the undo history is
  cleared, because its terrain entries would write pre-import heights back. The Terrain
  panel's heightmap **Import** of the backup restores the original heights bit for bit.

The changed-sample count of the preview is the count the apply makes.

### Cancel and reimport

`editor.terrain_source_cancel { job? }` stops a running worker at its next check (the
job then ends `cancelled`) or drops a staged import at once; it answers
`changed_assets: false`. No file, height or undo entry changes.

An applied import is recorded by terrain (the level terrain's layers path, else the
entity) in `<state>/terrain_sources.json` (editor state, not a content format):
the source parameters and the conversions. `editor.terrain_source_reimport { entity? }`
starts the recorded import again through the same preview and guards and answers
`{ job, state, source, conversion }`; the recorded conversions apply without being
chosen again. A terrain with no record answers `terrain_source_no_record`. Saving the
level afterwards re-cooks only the heightfield tiles the import changed.

### Permissions and errors

`start`, `status` and `reimport` need `read` (they write nothing); `cancel` needs
`assets`, as `editor.cancel_job`; `apply` needs `edit_scene`. One import runs at a time (`terrain_source_busy`).

| Code | When |
|---|---|
| `terrain_source_bad_params` | A malformed `path`, `entity`, `conversion` or PNG settings field |
| `terrain_source_no_project` | No project is open |
| `terrain_source_no_terrain` | The destination entity has no terrain, or the level has none |
| `terrain_source_busy` | Another import is running or staged |
| `terrain_source_failed` | The worker could not start or stopped |
| `terrain_source_not_found` | No running or staged import with that job id |
| `terrain_source_not_ready` | Apply before the worker staged the import |
| `terrain_source_grid_mismatch` | The source is not the terrain's grid size |
| `terrain_source_mapping_mismatch` | Registration or height range differ and the conversion was not chosen |
| `terrain_source_stale` | The source changed after the preview |
| `terrain_source_destination_changed` | The terrain's heights changed after the preview |
| `terrain_source_undo_limit` | Over the undo limit without `acknowledge_undo_limit` |
| `terrain_source_backup_failed` | The large-import backup could not be written |
| `terrain_source_no_record` | Reimport of a terrain with no applied import |

A source refusal keeps its own code (`height_io_read`, `height_io_decode`,
`height_io_bad_format`, `height_source_limit_*`).

### CLI

`regia terrain source-inspect <path> [--project DIR] [--mapping M] [--png JSON] [--limits JSON]`
prints the same preview as `terrain.inspect_source`. `regia terrain source-import <path>
--dest <dir>/heightfield.png` publishes a source as a terrain's cook input: the counts go
verbatim into the 16-bit `heightfield.png` and the source's schema-2 mapping into the
`heightfield.json` beside it, each written atomically (the PNG first). A destination of
another grid size is refused (`terrain_source_grid_mismatch`); a destination
`heightfield.json` that maps counts differently, or a legacy extents file, is refused
(`terrain_source_mapping_mismatch`) unless `--adopt-source`. `--dry-run` prints the
preview and writes nothing. Native: `regia_agent::{read_project_source, source_summary,
publish_source_asset}`.

`cargo test -p regia_editor --test terrain_source_workflow` drives the workflow through
the dispatcher with `play.wait_until` and `play.expect`, a channel-gated worker and no
wall-clock waits.

## Raster metadata: spacing, orientation, registration and missing data (ADR-8542)

A raster that is north-up, has different X and Z spacing, is registered at the outer
cell edge or has missing samples is described with an engine-owned **raster
descriptor** (`docs/formats/terrain.md` § 14, JSON schema
`docs/formats/terrain-raster-descriptor.schema.json`). Pass it as `raster`, a
project-relative path or an inline object, to either method:

```json
{"method":"terrain.inspect_source","params":{
  "path":"terrain/north.json",
  "raster":{"raster_descriptor":1,"spacing_m":[2.0,0.5],"row0":"north",
            "registration":"edge","origin_m":[10.0,-4.0],"nodata_counts":65535}
}}
```

The descriptor replaces the source's cell size and first sample; its counts, datum
and scale stay. `registration` is required (`center`: `origin_m` is the corner
sample; `edge`: it is the corner cell's outer edge, so samples sit half a spacing
inside). A north-first or east-first raster is reordered by an exact index
permutation into the canonical order (row 0 south, column 0 west); nothing is
interpolated. Samples whose `valid_mask` entry is 0 or whose count equals
`nodata_counts` are **missing**: their counts are kept, but they have no height.

The report adds:

| Field | Meaning |
|---|---|
| `spacing_m` | `[X, Z]` spacing |
| `missing_samples` | Samples without data |
| `raster.declared` | The stored `row0` / `col0` |
| `raster.canonical` | Always `south` / `west`; `x_axis` is +X by column, `z_axis` +Z by row |
| `raster.registration` | `center` or `edge` |
| `raster.sample0_m` | Resolved first sample (after the edge offset) |
| `raster.missing_cells` | Up to 64 canonical `[col, row]` missing samples; `missing_cells_truncated` when there are more |
| `runtime_map.with_holes` | With missing samples: whether the runtime map loads under the explicit hole policy |

`min_m`, `max_m` and `corners_m` skip missing samples (`null` for a missing corner);
a nodata sentinel is never reported as a height. `mapping` is `null` when no cooked
count mapping can describe the source (unequal spacing or missing samples), and
`runtime_map` refuses missing samples with `terrain_raster_missing_data` (the runtime
map has no implicit fill). A bake paints pixels with no valid sample magenta and
reports `missing_pixels` and `missing_rgb` (`[255, 0, 255]`); slopes use each axis's
own spacing.

| Code | Meaning |
|---|---|
| `terrain_raster_version` | `raster_descriptor` is not 1 |
| `terrain_raster_transform` | `skew_m` is not `[0, 0]`: rotated or sheared rasters are refused, never resampled |
| `terrain_raster_mask` | `valid_mask` is not the raster's size, or an entry is not 0/1 |
| `terrain_raster_missing_data` | No sample holds data (or, natively, the map refused missing samples) |

Other descriptor mistakes (nonpositive spacing, an unknown field or enum value, a
missing `registration`) are `height_io_decode`. Each code has an `errors.explain`
entry.

Native: `RasterDescriptor::{from_json, legacy, geometry}`, `TerrainSource::{from_raster,
with_raster, spacing_m, layout, valid_mask, missing_samples, sample_height_m,
count_mapping, terrain_map}`, `MissingDataPolicy::{Refuse, Holes}` (the hole policy
returns a `HoleShape::Raster` over the missing samples beside the map), `ValidMask`,
`overview::MISSING_RGB` and `OverviewImage::missing_pixels`.

The `terrain_source` dispatcher test
`raster_descriptor_previews_axes_registration_and_missing_cells` queues
`play.wait_until` on a scene transform, previews a north-first edge-registered source
with a nodata sample, bakes it, checks the mask and transform refusals, advances one
frame and checks with `play.expect` that the scene did not change.
`regia cook` reads the same descriptor as `heightfield.raster.json` beside
`heightfield.png` (with the schema-2 `heightfield.json` for datum and scale): it
reorders the counts exactly and stores the registered mapping, and refuses unequal
spacing and missing samples (`cargo test -p regia_asset_pipeline --test
terrain_raster_cook`).
`cargo test -p regia_terrain --test terrain_raster_metadata` covers orientation,
spacing, registration, nodata, masks, the x86-64/aarch64 coordinate bits, the legacy
schema-2 bits and the property loops.

## Source tiles: a heightfield in several files (ADR-8595)

A heightfield too large for one source file (the import limits cap a source at
128 MiB and 16,777,216 samples) is split into tiles named by a tile-set descriptor,
`heightfield.tiles.json` (`"terrain_source_tiles": 1`; schema
`docs/formats/terrain-source-tiles.schema.json`, example
`docs/formats/terrain-source-tiles.example.json`, format `docs/formats/terrain.md` § 15).
The descriptor holds the global grid (`cols`, `rows`, `cell_m`, `sample0_m`), the units
(`meters_per_count`, `zero_counts`), `max_overlap` (default 1) and the tiles
(`path` relative to the descriptor, `origin` `[col, row]` in the global grid, `size`
`[cols, rows]`). A tile is a schema-2 JSON source or a 16-bit grayscale PNG; each tile
stays within the import limits on its own.

Nothing is resampled, shifted or blended. Neighboring tiles may share a border up to
`max_overlap` samples wide, and every shared sample must hold the same count in each
tile. A global sample's count is the tiles' count, bit for bit, and its world position is
`sample0_m + (col, row) * cell_m` (f64, rounded to f32 once), exactly as for a monolithic
source with the same mapping. The order the descriptor lists tiles in does not matter:
tiles are sorted by origin (row, then column), and the tile id is `r<row>_c<col>`.

**Inspect.** `terrain.inspect_source { "path": "maps/basin/heightfield.tiles.json" }`
(any path ending `.tiles.json`) validates the structure from the descriptor, then reads
every tile in 256-sample bands, holding only the tiles a band needs, and checks each
tile's size and mapping and every shared sample. The answer is the usual source summary
plus `tile_set`:

| Field | Meaning |
|---|---|
| `tiles`, `tiles_listed` (at most 64: `id`, `path`, `origin`, `size`), `tiles_truncated` | The tiles, sorted |
| `grid_samples`, `tile_samples` | Global samples; samples counted once per tile (the excess is shared borders) |
| `overlapping_pairs`, `max_overlap`, `max_tile_samples` | Shared borders and the largest tile |
| `covered` | Always `true` in an answer (a gap is an error) |
| `assembled` | `true` when the grid fits `max_samples` and the full summary was built from it; `false`: only the streamed fields (size, mapping, bounds, height range) |
| `tiles_decoded`, `seam_samples_checked`, `peak_charged_samples` | Counters of the validation read |

`png`, `mapping` and `raster` do not apply to a tile set (`invalid_params`). A
monolithic source over `max_bytes` or `max_samples` answers its limit code with
`data.hint` pointing here.

| Code | Meaning |
|---|---|
| `terrain_tiles_version` | `terrain_source_tiles` is not 1 |
| `terrain_tiles_limit` | No tile, or more than 4096 |
| `terrain_tiles_path` | A tile path is absolute, has `.`/`..`/empty segments, `\` or `:`, is not `.json`/`.png`, or leaves the folder |
| `terrain_tiles_bounds` | A tile is under 2x2 samples or leaves the grid |
| `terrain_tiles_overlap` | Two tiles share a region wider than `max_overlap` on both axes |
| `terrain_tiles_gap` | A global sample is in no tile (the message names it) |
| `terrain_tiles_dimensions` | A tile file's grid is not its entry's `size` |
| `terrain_tiles_mapping` | A tile's datum, scale or cell differs from the descriptor's |
| `terrain_tiles_misregistered` | A JSON tile's `sample0` is more than 1 mm from its origin's position |
| `terrain_tiles_seam` | Two tiles disagree on a shared sample (the message names the sample and both tiles) |

A tile over the per-tile budget is `height_source_limit_samples` or
`height_source_limit_bytes` with the tile id. Each code has an `errors.explain` entry.

**Import into the editor.** `editor.terrain_source_start` with a `.tiles.json` path
assembles the tiles (the grid must fit the import limits) and stages the usual preview;
its progress reads `reading source tile N of M`, and a cancel between tiles ends the job
`cancelled` with nothing changed. The native seam is
`regia_agent::read_project_source_observed(root, policy, params, observer)`.

**Cook.** `regia cook` cooks a `heightfield.tiles.json` beside a `*.terrain.layers.ron`
in place of `heightfield.png` (with both, or with a `heightfield.json` or a
`heightfield.raster.json` beside it, the cook refuses with `terrain_tiles_ambiguous` and
writes no index; ADR-8639). The tiles are fed one cooked-tile region at
a time into the tiled heightfield cook (`cook_tiled_heightfield_regions`, the same tiles,
padding and bytes as `cook_tiled_heightfield` of the assembled grid), so the decoded
source buffers depend on the tile size, not the world area. Nothing is published until
every region passed: a refusal leaves the previous cooked map byte for byte. The terrain
index stores the descriptor's mapping. Evidence of the last cook (each tile's BLAKE3 and
the cooked file's) sits in `<cache>/terrain-source-tiles/`: a recook decodes only the
changed tiles' regions and copies the other cooked tiles from the previous file.
`cook_tile_set_heightfield(map_id, descriptor, cache_dir, tile_size, progress)` reports
`SourceTileProgress::{Validated, Decoding, Region}`, cancels on `ControlFlow::Break`
(`height_source_cancelled`, nothing written) and returns `SourceTileCookReport`
(`source_tiles`, `source_tiles_changed`, `source_tiles_decoded`, `cooked_tiles`,
`cooked_tiles_recooked`, `peak_charged_samples`, `seam_samples_checked`), also on
`TerrainCookOutcome::source_tiles`.

Native: `TileSetDescriptor::{from_json, mapping, tiles, coverage, decode_tile,
tiles_intersecting}`, `TileSetReader::{new, read_rect, for_each_rect, assemble, stats}`,
`SourceTileRef`, `TileSetCoverage`, `TileReadStats`.

Tests: `cargo test -p regia_asset_pipeline --test terrain_tile_import` (two and three
tiles equal the monolithic cook byte for byte; seam, gap, misregistration, overlap and
size refusals leave the published map; shuffled order; peak buffers; cancel and partial
recook), `cargo test -p regia_terrain --test terrain_source_tiles` (structure, seams,
order, bounded reads, pinned placement bits, the schema) and the dispatcher test
`source_tile_set_previews_coverage_and_refuses_seams_and_gaps` in
`cargo test -p regia_agent --test terrain_source`, which queues `play.wait_until`,
previews a two-tile set beside its monolithic equivalent, checks the seam, gap and
split-hint refusals, and confirms with `play.expect` that the scene did not change.

## Registered overviews: world mapping, legend, markers, cache and cancel (ADR-8596)

An overview can be published as a **registered pair**: the PNG and an engine
descriptor `<name>.overview.json` beside it (`docs/formats/terrain.md` § 16, JSON
schema `docs/formats/terrain-overview-descriptor.schema.json`) recording the source
identity, every setting, the image-to-world mapping, orientation, legend, markers and
the comparison tolerance. Overview images are presentation-only.

```json
{"method":"terrain.bake_overview","params":{
  "path":"terrain/source.json","output":"previews/terrain.png","register":true,
  "overview":{"size":[512,512],"fit":"preserve_aspect","contour_step_m":8.0},
  "markers":[{"label":"origin","xz":[0.0,0.0]},{"entity":{"name":"Spawn"}}]
}}
```

Every bake now also answers `fit`, `registered`, `mapping`, `legend` and `markers`:

- `mapping`: `image_size`, `content_px` (`[x, y, width, height]` of the source
  rectangle), `world_min`/`world_max` (X/Z of the outer cell edges, half a spacing
  beyond the first and last sample centers) and `meters_per_pixel`. **Coordinate
  convention:** `(u, v)` are continuous pixels, `(0, 0)` the top-left corner of pixel
  `(0, 0)`, `u` east (+X), `v` south (-Z), row 0 north:
  `x = world_min.x + (u - content_px.x) * meters_per_pixel.x` and
  `z = world_max.z - (v - content_px.y) * meters_per_pixel.z`. A pixel's center is
  `(px + 0.5, py + 0.5)`.
- `fit`: `stretch` fills the image (one scale per axis; the legacy image, byte for
  byte); `preserve_aspect` uses one scale, centers the source and paints the rest
  `letterbox_rgb` `[0, 0, 128]`. Missing data stays `missing_rgb` `[255, 0, 255]`,
  never a gray: a zero height is relief, not missing.
- `legend`: `elevation_range_m` (1st/99th percentile heights; brightness runs 70 % to
  100 % across it), `scale_bar_m` (1, 2 or 5 times a power of ten meters, at most a
  quarter of the source width) and `scale_bar_px`, `missing_rgb`, `letterbox_rgb`.
- `markers`: up to 256, each `{label?, xz: [x, z], rgb?}` or `{label?, entity, rgb?}`
  (an entity reference as in `play.wait_until`; its world X/Z, its name as the
  default label). Drawn as red plus signs (3 pixels per arm) clipped to the source
  rectangle; each reports `pixel` (`null` outside it).

With `register: true` the reply adds `descriptor` (the project-relative descriptor
path), `cache_key` and `cached`. The cache key covers the algorithm version, the
source, mapping and raster descriptor files, the decoded source, the settings and
the markers: when the pair on disk is current (its descriptor names this key and the
PNG still hashes to the recorded BLAKE3) nothing is baked or written and `cached` is
`true`. Any change bakes again; an edited PNG or a missing descriptor is never a hit.

**Editor job.** `editor.terrain_overview_start` takes the same parameters (always
registered) and starts the cancellable job `terrain_overview:<n>` (assets permission;
entity markers resolve when it starts; one job at a time, else
`terrain_overview_busy`). Wait for it with

```json
{"method":"play.wait_until","params":{"condition":{"events_contain":
  {"kind":"terrain_overview","contains":"terrain_overview:1 done"}},"timeout_ms":60000}}
```

then read `editor.terrain_overview_status { job }` (`state` running with `progress`,
done with `result`: the registered reply above, cancelled with `published: false`,
or failed with `error`) or `agent.job_status { id }`. `editor.cancel_job { id }`
cancels: the worker checks at every bake step and once more before publication, and a
cancelled job writes no PNG, descriptor or temporary file and leaves an earlier pair
byte for byte. The Terrain panel's **Overview** section (output, Keep aspect ratio,
size, Bake overview / Cancel overview) builds the same request from the Import source
fields and shows the job's state, mapping scale and scale bar.

**CLI.** `regia terrain source-overview <source> --output <png> [--fit
preserve_aspect] [--size 512x256] [--settings '{"contour_step_m":8}'] [--marker
spawn=4,-2] [--mapping ...] [--png ...] [--raster ...]` runs the same service
(`regia_agent::publish_project_overview`) and prints the registered reply.

Errors: the reader's codes, `height_io_bad_format`, `height_io_write`,
`terrain_overview_cancelled` (job only), `terrain_overview_busy`,
`terrain_overview_no_project`, `terrain_overview_not_found`,
`terrain_overview_failed`, `invalid_params` (an unknown `fit`, a bad marker).

**Tolerance.** Two bakes of one source and settings may differ across architectures
by at most one 8-bit step on at most 0.1 % of the pixels (`OVERVIEW_TOLERANCE`,
recorded in the descriptor). Tests compare numbers and that tolerance; no overview
golden image is blessed. Slope and passability layers will reach overviews only
through the C-19 adapter.

`cargo test -p regia_terrain --test overview_registered` covers the four corners and
center under both fits, the image agreeing with its mapping, no data versus zero
height, cache invalidation by source, settings, input digest and markers, cancel at
every step with and without an earlier pair, north-up and hillshade invariants, marker
pixels and the schema. `cargo test -p regia_editor --test terrain_overview_job`
drives the editor job through the dispatcher with `play.wait_until` and `play.expect`,
a channel-gated worker and no wall-clock waits.
