# Importing assets

<!-- index: Import -->
<!-- index: glTF -->
<!-- index: FBX -->
<!-- index: OBJ -->
<!-- index: USDA -->
<!-- index: .meta | see: Asset id -->

Drop a source file under your project's `assets/` folder and `regia cook` turns it into the form the
game loads. The editor's Content Browser **Import...** button copies a file in first. A file the cook
has no importer for stops with `unsupported source extension: <ext>`. This chapter lists every importer
the engine has, what each one produces, how to change its settings and what it refuses.

## How an import works

- **The `.meta` sidecar.** The first cook writes `<file>.meta` next to each source: a RON file with the
  asset's stable `id`, the `importer` that handled it, a `settings` string and the ids of its sub-assets
  (see `docs/formats/meta.example.ron`). Commit it. The id is what other assets refer to, so deleting a
  `.meta` gives the asset a new identity and breaks references to it.
- **Settings** are a RON string in the `.meta` `settings` field, and every field is optional:

  ```ron
  (
      format_version: 1,
      id: "67e55044-10b1-426f-9247-bb680e5fe0c8",
      importer: "bvh",
      settings: "(unit: Centimeters, up_axis: Z, root_motion: XZ)",
  )
  ```

  A malformed settings string for most importers is a cook warning and the defaults apply; the FBX,
  OBJ and BVH importers fail the cook with their own `*_bad_settings` code instead.
- **Incremental.** The cook keys each result on the source bytes, the settings and the importer
  version, so an edit to either recooks only that file.
- **Sub-assets.** A model produces several outputs (meshes, materials, skeletons, clips). They get stable
  ids by path, so re-ordering inside the source does not break references.
- **Untrusted input.** Importers check sizes before parsing and use hard caps. A file that breaks a cap
  fails its own import with a code in the message and the rest of the cook goes on.

## The importers

Extensions are matched in lower case. "Agent page" links go to the method-level detail.

### Models, animation and scenes

| Extensions | Produces | Settings (`.meta`) | Limits and errors | Agent page |
|---|---|---|---|---|
| `.glb`, `.gltf` | Meshes (with LODs and a meshlet hierarchy on request), materials, textures, models, skeletons, clips, morph targets | `generate_lods`, `meshlets`, `drop_double_sided_on_closed`, `vertex_ao`, `skin_influences` (4 or 8), `fracture`, `collision` (`ConvexDecomposition`, tuned by `collision_hulls`), `voxel_far` | Node nesting 256 deep and 1,048,576 node visits; a `.glb` with a header length below 12 bytes is refused before parsing | [glTF conventions](../agents/gltf-conventions.md), [glTF hierarchy](../agents/gltf-hierarchy.md) |
| `.fbx` (binary or ASCII, FBX 6.1 to 7.7) | The same outputs as glTF: the file is converted to an in-memory glTF and cooked through the glTF importer | `scale`, `import_animation`, `sample_rate` (1 to 240), `uv_set`, `limits`, plus the glTF keys above | File 512 MiB, record depth 64; `fbx_bad_params`, `fbx_io`, `fbx_too_large`, `fbx_not_fbx`, `fbx_too_deep`, `fbx_malformed`, `fbx_parse`, `fbx_unsupported_version`, `fbx_limit`, `fbx_bad_settings`, `fbx_no_mesh` | [FBX and OBJ import](../agents/fbx-import.md) |
| `.obj` (and its `.mtl`) | One mesh per object, one primitive per material, converted through the glTF importer. Read as meters, +Y up | `scale`, plus the glTF keys | Caps on file size, vertices, indices, polygon corners and objects; `obj_bad_params`, `obj_io`, `obj_too_large`, `obj_parse` (with the line), `obj_limit`, `obj_bad_settings`, `obj_no_mesh` | [FBX and OBJ import](../agents/fbx-import.md) |
| `.bvh` | A skeleton and a clip `clip/<name>`; optionally a clip retargeted onto a project character | `unit`, `unit_scale`, `up_axis`, `keep_forward`, `root_motion`, `rotation_tolerance_degrees`, `translation_tolerance`, `end_sites`, `clip_name`, `retarget`, `limits` | File 256 MiB, 1,024 joints, 1,000,000 frames, 8,192 channels per frame, depth 256; `bvh_bad_params`, `bvh_io`, `bvh_too_large`, `bvh_syntax`, `bvh_channels`, `bvh_duplicate_joint`, `bvh_motion`, `bvh_limit`, `bvh_bad_settings`, `bvh_retarget` | [BVH import](../agents/bvh-import.md) |
| `.tmx`, `.tmj` (with `.tsx`, `.tsj` tilesets) | A tile map: finite maps in every Tiled orientation, tile and object layers, group layers flattened | none | XML files 32 MiB and 256 attributes per element | [2D](../agents/2d.md), [2D editor](../agents/editor-2d.md) |

The converted `.glb` of an FBX or OBJ is what the glTF importer sees. `regia import model.fbx --out
model.glb` writes it so you can open it in any glTF viewer; `editor.fbx_inspect` and `editor.obj_inspect`
return the same report without writing anything.

### Images

| Extensions | Produces | Settings (`.meta`) | Limits and errors | Agent page |
|---|---|---|---|---|
| `.png`, `.jpg`, `.jpeg`, `.tga`, `.bmp` | A texture with a full mip chain, block-compressed for the platform (BC on Linux, Windows, and macOS; ASTC on Android). TGA and BMP are decoded and then cooked like a PNG | `usage` (`color`, `linear`, `normal`, `mask`, `ui`), `max_size`, `compress`, `quality` (`high`, `low`), `stream`, `stream_min_px` | Default usage comes from the material slot that uses the image, else from its file name; a malformed settings string is logged and the defaults apply | [Texture compression](../agents/texture-compression.md), [Texture sets](../agents/texture-sets.md) |
| `.dds` | BC1, BC3, BC4, BC5 and BC7 with two or more mips keep their blocks (ASTC on Android); a single level, BC2, BC1 with punch-through alpha and 32-bit uncompressed formats decode to RGBA8 and cook like a PNG | `max_size` drops leading mip levels | Edge up to 32,768. Refused: cube maps, volumes, arrays, signed BC4 and BC5, BC6H and unlisted formats. `dds_bad_header`, `dds_unsupported`, `dds_truncated` | [Loudness and the other importers](../agents/loudness-import.md#importers) |
| `.exr`, `.hdr` | A linear float texture (KTX2) with a full mip chain. NaN and infinite samples become 0 (one warning per file), negatives clamp to 0, values above 65,504 clamp | `hdr_encoding` (`auto`, `bc6h`, `rgba16f`, `astc_hdr`), `compress`, `cube_face_px` (4 to 8192: cook an equirect source into a six-face cube map) | At most 67,108,864 texels. `hdr_decode`, `hdr_size`, `hdr_settings`, `hdr_ktx2` | [Texture compression](../agents/texture-compression.md) |

### Sound, font and video

| Extensions | Produces | Settings (`.meta`) | Limits and errors | Agent page |
|---|---|---|---|---|
| `.wav`, `.ogg`, `.flac`, `.mp3` | The file itself is not converted. The cook decodes it once and writes its EBU R128 loudness record into `loudness.ron` | `normalize: Some(false)` excludes a pre-mastered stem; `target_lufs: Some(-18.0)` changes its target. A project `loudness.toml` sets `target_lufs` (-70 to 0) and `true_peak_ceiling_db` (-30 to 0) | A sound that does not decode fails with `audio import error`; at run time `loudness_decode_failed` | [Loudness at import](../agents/loudness-import.md) |
| `.ttf`, `.otf`, `.ttc`, `.otc` | The font, subset to the characters your shipped locales use (cache only; `regia package` stages it) | `subset` (`auto`, `off`, `locales([...])`), `extra_chars`, `keep_ascii`, `keep_punctuation`, `keep_pseudo` | Errors start `font cook error:` with the file path | [Font subsetting](../agents/font-subset.md) |
| `.webm`, `.mkv` | The file copied byte for byte beside a clip report: validated, never transcoded | none | `container_invalid` (truncated or damaged), `codec_patented`, `codec_unsupported`. These containers are refused with `container_unsupported`: `.mp4`, `.m4v`, `.mov`, `.avi`, `.ts`, `.m2ts`, `.flv`, `.wmv`, `.3gp`, `.mpg`, `.mpeg` | [Video codecs](../agents/video-codecs.md), [Video playback](../agents/video-playback.md) |
| `.video.ron`, `.videolist.ron` | A video source or playlist, validated and stored as written | the file itself | A bad or future version fails the cook with the video error code | [Video playback](../agents/video-playback.md) |

### Materials, lighting and effects

| Extensions | Produces | Settings (`.meta`) | Limits and errors | Agent page |
|---|---|---|---|---|
| `.material.ron` | A cooked material (v5 output) | the file itself | Fails the cook with `material error:` and the cause | [Materials](materials.md) |
| `.matgraph.ron`, `.matinst.ron` (and `.matfunc.ron` they call) | A compiled material graph: the generated code is validated in every host shader; each texture it samples is cooked again as an RGBA8 sub-asset | the file itself | `material graph error:` with the node | [Material graph](../agents/material-graph.md) |
| `.ies` | A photometric profile resampled onto the renderer's grid | none | `ies error:` with the cause (format: `docs/formats/ies-profile-v1.md`) | [Light profiles](../agents/light-profiles.md) |
| `.fgiv` | A baked GI volume | none | The file's own checks | [Baked GI](../agents/baked-gi.md) |
| `.flmap` | A baked lightmap | `RGB9E5` (default), `BC6H`, ASTC 4x4 HDR or `auto` (BC6H on desktop, ASTC HDR on Android) | The file's own checks | [Lightmaps](../agents/lightmaps.md) |
| `.fluidcache` | A particle-liquid bake in canonical form (a v1 file re-encodes to itself) | none | A damaged file fails with a `fluidcache_*` code | [Fluid](../agents/fluid.md) |

### 2D, gameplay data and simulation

| Extensions | Produces | Settings (`.meta`) | Limits and errors | Agent page |
|---|---|---|---|---|
| `.atlas.ron` | Packed sprite pages (at most `max_size` pixels each), with a fixed packing order so the same sources give the same pages | the file itself | `2D asset error:` with the cause | [Atlas](../agents/atlas.md) |
| `.tilemap.ron`, `.autotile.ron` | A tile map, or auto-tile rules | the file itself | `2D asset error:` | [2D](../agents/2d.md) |
| `.table.ron`, `.table.csv`, `.curves.ron`, `.curves.csv`, `.chooser.ron`, `.smartobject.ron`, `.camera.ron` | Cooked gameplay data, structurally validated. A table that fails its checks is left out of the pak with a warning; one with errors fails the cook | the file itself | `data table error:` with the row and column | [Data tables](../agents/data-tables.md), [Curve tables](../agents/curve-tables.md), [Choosers](../agents/choosers.md) |
| `.scatter.ron` | Scatter rules | the file itself | `scatter error:` | [Scatter](../agents/terrain-scatter.md) |
| `.cloth.ron`, `.soft.ron` | A cooked cloth or soft body (`.cloth.bin`, `.soft.bin`), resolved against the glTF nodes it names | the file itself | `cloth error:` | [Cloth](../agents/cloth.md), [Soft bodies](../agents/soft-bodies.md) |
| `.motiondb.ron` | A motion-matching database with its feature set and search index built at cook time | the file itself | `animation error:`; clips with non-finite values are refused | [Motion matching](../agents/motion-matching.md) |

Anything under an extension's own `ext://` folder is cooked by that extension's loader, and extensions can
register importers for file types the built-ins do not take.

## More source formats the cook accepts

These cook in the current `regia` binary. The pipeline crate's default features include
`fbx_import`, `usd_import`, and `svg_import`. A build of the pipeline with one of those
features off refuses that extension with `unsupported source extension`. PLY, STL, PSD,
Aseprite, LDtk, and MaterialX are not behind those features.

| Extensions | Cooks as | Detail |
|---|---|---|
| `.ply`, `.stl` | An in-memory glTF, then the glTF importer. One mesh, no material. | [PLY, STL and PSD import](../agents/mesh-image-import.md) |
| `.psd` | An RGBA8 image, then the PNG texture path. | [PLY, STL and PSD import](../agents/mesh-image-import.md) |
| `.usda`, `.usdc`, `.usd`, `.usdz` | A composed stage, then an in-memory glTF. | [FBX and OBJ import](../agents/fbx-import.md#usd) |
| `.svg` | Pixels at cook time. The pak holds the raster, not the SVG. | [SVG import](../agents/svg-import.md) |
| `.mtlx` | A material graph, then the material-graph cook. | [MaterialX import](../agents/materialx-import.md) |
| `.aseprite`, `.ase` | Sprite-atlas strips. | [Import reports: Aseprite and LDtk](../agents/import-report.md) |
| `.ldtk` | A cooked tilemap. | [Import reports: Aseprite and LDtk](../agents/import-report.md) |

`regia import` also reads `.usda`, `.usdc`, `.usd`, and `.usdz` (plus `.fbx` and `.obj`) and
can write the converted `.glb` with `--out`. USD refusals use the `usd_*` codes on the FBX
page (`usd_not_usd`, `usd_parse`, `usd_no_mesh`, and the rest of that table).

## Export a glTF binary to USDA

`regia export usda` reads a `.glb` and writes one USDA stage for a DCC tool. It does not
import USDA. Cooking a `.usda` is the section above. The stage is not a Regia content
format: no `format_version`, and `regia upgrade-project` does not migrate it.

```console
$ regia export usda --glb assets/models/crate.glb --out build/exchange/crate.usda
```

Success prints `wrote <path> (<bytes> bytes, <nodes> nodes, <meshes> meshes)` and exits 0.
The file is `#usda 1.0`, default prim `World`, `metersPerUnit` 1, `upAxis` Y. Each glTF
node becomes an `Xform`. Each triangle primitive becomes a `Mesh` (points, face counts,
indices, normals when the primitive has them, and the first UV set). Materials, textures,
skins, animation, cameras, lights, and morph targets are not written. The UV `t` coordinate
is copied as stored. A primitive that is not a triangle is skipped.

| Code | Meaning |
|---|---|
| `usda_glb_missing` | `--glb` does not exist. |
| `usda_glb_invalid` | Not a glTF 2 binary, or a value cannot be written (non-finite number, bad index, a node cycle). |
| `usda_empty` | No triangle mesh. The output path is left as it was. |
| `usda_write` | The stage could not be written or renamed. |

The stage text is written beside the destination and renamed into place. The field list is
on [Exporting a glTF binary to USDA](../agents/export-usda.md). The task steps, including
FBX and OBJ, are in [Import a model](../how-to/create/import-a-model.md).

## Practical advice

- **Prefer glTF.** FBX and OBJ are converted to it, so glTF has no conversion step to get wrong and gives
  you every setting. Export from your tool at real-world scale and +Y up when you can, and use `scale` only
  to repair a unit mistake.
- **Keep textures beside the model.** FBX and OBJ look for textures in the model's own directory only
  (the relative path, then the bare file name). A path with `..` or an absolute path is never followed. A
  texture that is not found drops its slot with a warning and the import succeeds.
- **Check before you cook.** `regia import model.fbx --json`, `regia import model.obj --json`,
  `regia import model.usda --json`, and `editor.bvh_inspect` read a file and report what they
  found without cooking the project. `--out` is what writes a `.glb`.
- **Pixel art stays sharp.** Set `compress: Some(false)` so the image stays RGBA8, and keep `stream: Some(false)`
  for a texture that must never show a low mip.
- **Fix a unit or axis mistake in settings, not in the file.** BVH libraries in centimeters or Z-up need
  `unit: Centimeters` and `up_axis: Z`; the BVH importer's `retarget` plays the clip on a project character.
- **Read the cook report.** Every refusal above carries its code at the start of the message, and the cook
  report names the file. Search the code on the agent page to see the cause.
