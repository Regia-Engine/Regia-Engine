# FBX and OBJ import (`editor.fbx_inspect`, `editor.obj_inspect`, `regia import`)

Regia Engine cooks `.fbx` files (binary and ASCII, FBX 6.1 to 7.7) the way it cooks `.glb` files. The FBX
importer parses the file with the `ufbx` C library, converts the scene to an in-memory glTF 2.0 binary, and
hands that to the glTF importer. The cooked meshes, materials, textures, models, skeletons, clips and meshlets
are therefore the glTF importer's formats: nothing in the runtime knows the source was FBX.

`ufbx` is a pipeline-only exception to the Rust-only rule (ADR-1602). It is a dependency of
`regia_asset_pipeline` (cook, CLI, editor) and never of a runtime crate; the layer check fails if a runtime
crate reaches it. Shipped games never contain it.

## What is imported

| FBX | Becomes | Notes |
|---|---|---|
| Meshes (polygons) | glTF meshes, one primitive per material | Triangulated by `ufbx`. Instances share one mesh. |
| Normals | `NORMAL` | Generated when the file has none. |
| UV sets | `TEXCOORD_0..7` | `TEXCOORD_0` is the `uv_set` setting (default: the first set): the cooked mesh keeps one UV set. V is flipped to glTF's top-left origin. |
| First vertex color set | `COLOR_0` | |
| Skin clusters | glTF skin, `JOINTS_n`/`WEIGHTS_n` | The eight strongest influences per vertex, normalized. Inverse binds from the clusters. |
| Blend shapes | Morph targets | Named after the channel; default weights from `DeformPercent`. |
| Materials | Metallic-roughness materials | Through `ufbx`'s PBR mapping of Lambert, Phong and PBR shaders: base color, opacity (blend), metallic, roughness, emissive, double-sided. A PBR shader's separate roughness and metalness maps are packed into one metallic-roughness map (roughness in G, metalness in B; glossiness inverted), which the cook turns into the ORM map. |
| Textures | Base color, normal, emissive maps | Embedded (`Video` content) or a file inside the FBX's directory. PNG and JPEG are referenced; TGA, BMP and other formats the `image` crate reads are re-encoded as PNG. |
| Node hierarchy | glTF nodes | Names and local transforms; pivots, pre/post rotation and geometric transforms are resolved by `ufbx`. |
| Animation stacks | Clips | Translation, rotation and scale per node; blend-shape `DeformPercent` curves as `weights` channels. Non-linear curves are sampled at `sample_rate`; linear keys stay. Times start at 0. |

Not imported: cameras, lights, NURBS, constraints, per-instance material overrides, and a legacy Lambert or
Phong shininess map (a warning; the factors are used).

The editor's Content Browser `Import…` accepts `.fbx` and `.obj` files; they are copied into the project and
cooked like any other source (copy their textures and `.mtl` files beside them).

## Units and axes

`ufbx` converts the file to right-handed, +Y up, front +Z, one unit per meter (the glTF frame). The glTF
importer then applies Regia Engine's forward convention (`docs/formats/gltf-conventions.md`). A 100-unit
cube in a centimeter file (the FBX default) is a 1 m cube; a Z-up file (3ds Max style) stands up along +Y.
`scale` multiplies after the conversion: `scale: 0.01` fixes centimeter data saved with a meter unit.

## Texture lookup

A texture is looked up inside the FBX's directory only: first its relative path, then its bare file name
beside the FBX (artists' absolute paths name their own machines). A path with `..` or an absolute path is
never followed. A texture that is not found drops its slot with a warning; the import still succeeds.

## Import settings (`.meta`)

The `.meta` `settings` string (RON) takes the FBX keys next to the glTF keys (`meshlets`, `generate_lods`,
`drop_double_sided_on_closed`, `vertex_ao`), which apply to the converted scene as they do to a `.glb`:

```ron
(
    scale: 1.0,             // after the unit conversion
    import_animation: true,
    sample_rate: 30.0,      // 1..=240, for non-linear curves
    uv_set: Some("map1"),   // the set cooked as UV0
    limits: (max_vertices: 4000000),   // any FbxLimits field
    meshlets: true,         // a glTF importer key
)
```

## Untrusted input

The C parser sees untrusted bytes, so the importer checks the file before and after it runs:

- The file size (`max_file_bytes`, 512 MiB), from the file's metadata before reading.
- A Rust pre-scan of the record structure: in a binary file every record's end offset must stay inside its
  parent and the file (`fbx_malformed` otherwise), at most `max_records` records, nested at most
  `max_record_depth` (64) deep (`fbx_too_deep`); an ASCII file's `{ }` depth outside strings and comments obeys
  the same cap.
- The parse runs with memory and allocation limits (`max_memory_bytes`, `max_allocations`), a node-depth limit
  (`max_node_depth`), external file loading off and the format forced to FBX.
- After the parse: `max_nodes`, `max_vertices`, `max_indices`, `max_textures` and `max_animation_keys`.

No `ufbx` type leaves the reader module, and every index read from the parsed scene is bounds-checked.

## `editor.fbx_inspect`

Runs the importer and returns its report. Read-only: it writes nothing and changes nothing in the world.

| Param | Type | Meaning |
|---|---|---|
| `path` | string | The `.fbx` file (textures are looked up beside it). |
| `text` | string | Inline ASCII FBX instead (at most 16 MiB; no external textures). |
| `sample` | string | `"triangle"`: the built-in one-triangle file, a smoke check. |
| `scale` | number | Uniform scale after the unit conversion (default 1). |
| `import_animation` | boolean | Convert animation stacks (default true). |
| `sample_rate` | number | Samples per second for non-linear curves (1..=240, default 30). |
| `uv_set` | string | The UV set that becomes UV0. |

Give exactly one of `path`, `text` and `sample`. `path` (here and in `editor.obj_inspect`, `editor.usd_inspect`) is
relative to the project, or absolute under the project or the temp dir ([Request paths](safety.md#request-paths),
ADR-7388); any other path is `path_outside_project`. To inspect a file elsewhere, import it or copy it in first.

Result: `{ ascii, version, creator, source_unit_meters, source_up_axis, scale, nodes, meshes, vertices,
triangles, skins, joints, morph_targets, materials, uv_sets, textures: [{material, role, file, status}], clips:
[{name, duration_seconds, channels}], warnings, glb_bytes }`. A texture's `status` is `embedded`, `external`,
`missing` or `undecodable`.

```json
{"method": "editor.fbx_inspect", "params": {"path": "assets/hero.fbx"}}
```

### Error codes

Errors are `invalid_params` with `data.fbx_error`:

| Code | Meaning |
|---|---|
| `fbx_bad_params` | Missing or conflicting `path` / `text` / `sample`, or a bad setting value. |
| `fbx_io` | The file could not be read. |
| `fbx_too_large` | The file is larger than `max_file_bytes`. |
| `fbx_not_fbx` | Neither a binary nor an ASCII FBX (or empty). |
| `fbx_too_deep` | Records nest deeper than `max_record_depth`. |
| `fbx_malformed` | A binary record's offsets leave its parent or the file. |
| `fbx_parse` | The parser rejected the file. |
| `fbx_unsupported_version` | The FBX version is not supported. |
| `fbx_limit` | A count, memory or node-depth limit was exceeded. |
| `fbx_bad_settings` | The `.meta` settings string is malformed. |
| `fbx_no_mesh` | The scene has no mesh to cook. |

The cook reports the same codes in its error messages.

## `regia import`

```text
regia import <file.fbx> [--out GLB] [--scale F] [--no-animation] [--sample-rate HZ] [--uv-set NAME] [--json]
```

Prints the report (`--json` for tools). `--out` writes the converted `.glb`: the exact input the cook gives to
the glTF importer, so a conversion can be checked in any glTF viewer. `regia cook` cooks `.fbx` files in a
project without this command.

## Rust API

```rust,wrap
// `TRIANGLE_FBX` is a small valid mesh used by the importer tests.
# use std::path::Path;
# use regia_asset::AssetId;
# use regia_asset_pipeline::fbx_import::TRIANGLE_FBX;
# use regia_asset_pipeline::TextureCookQuality;
# use regia_asset_pipeline::Forward;
# let bytes = TRIANGLE_FBX.as_bytes();
# let dir = Path::new(".");
# let path = Path::new("model.fbx");
# let parent_id = AssetId::NIL;
# let quality = TextureCookQuality::High;
use regia_asset_pipeline::fbx_import::{convert_fbx_to_glb, import_fbx_path, FbxImportSettings};

let conversion = convert_fbx_to_glb(&bytes, Some(dir), &FbxImportSettings::default())?;
// conversion.glb: a glTF 2.0 binary; conversion.report: what was found.
let imported = import_fbx_path(&path, parent_id, Forward::Convert, "linux", None, quality)?;
// imported.gltf: the glTF importer's result (cooked meshes, materials, textures).
```

`prescan_fbx` and `read_fbx_file` are the pre-parse checks on their own.

## Verifying a conversion

Import the same model as glTF and FBX and compare: the integration test
`the_same_cube_as_gltf_and_fbx_cooks_equal_geometry` (`crates/regia_asset_pipeline/tests/fbx_import.rs`) checks
that a cube cooks to the same triangles and normals both ways. To check a real file, run
`regia import model.fbx --out model.glb` and open `model.glb` beside the source in a DCC tool.

## OBJ

Wavefront `.obj` files cook through the same path, with a pure-Rust reader
(`regia_asset_pipeline::obj_import`): the file and its `mtllib` libraries become an in-memory glTF binary for the
glTF importer. OBJ has no unit or axis metadata, so it is read as meters, +Y up, right-handed (the default of DCC
exporters); `scale` in the `.meta` settings changes the size.

| OBJ / MTL | Becomes |
|---|---|
| `o` | One mesh node per object (a file without `o` is one object) |
| `usemtl` | One primitive per material |
| `v` (optional `r g b` after the position), `vt` (V flipped), `vn` | `POSITION`, `COLOR_0`, `TEXCOORD_0`, `NORMAL`; an attribute some corners of a primitive lack is dropped for that primitive |
| `f` | Triangles (polygons fanned from their first corner); negative indices count back |
| `Kd`, `d` / `Tr`, `Ke`, `Ns` (roughness `sqrt(2 / (Ns + 2))`), `Pr`, `Pm` | Base color, opacity (blend), emissive, roughness, metallic |
| `map_Kd`, `map_Bump` / `bump` / `norm`, `map_Ke` | Base color, normal and emissive textures (inside the OBJ's directory only) |

Caps: `max_file_bytes` (each `.obj` and `.mtl`), `max_vertices`, `max_indices`, `max_polygon_corners`,
`max_objects` (`ObjLimits`).

### `editor.obj_inspect`

Runs the OBJ reader and returns its report; read-only. Params: `path` (an `.obj` file; `mtllib` files and
textures are looked up beside it) or `text` (inline OBJ, at most 16 MiB, no `mtllib`), and `scale`. Result:
`{ objects, vertices, triangles, materials, textures, warnings, glb_bytes }`.

```json
{"method": "editor.obj_inspect", "params": {"path": "assets/crate.obj"}}
```

Errors are `invalid_params` with `data.obj_error`: `obj_bad_params` (missing or conflicting `path` / `text`,
a bad `scale`), `obj_io`, `obj_too_large`, `obj_parse` (with the line number), `obj_limit`,
`obj_bad_settings`, `obj_no_mesh`. The cook reports the same codes. `regia import file.obj [--out GLB]
[--scale F] [--json]` prints the same report.

## USD

USD files cook through the same path (ADR-3235): `.usda` (text), `.usdc` (binary crate), `.usd` (either) and
`.usdz` (an uncompressed zip package). The pure-Rust `openusd` reader (MIT, pipeline only, never in a runtime crate)
composes the stage, and `regia_asset_pipeline::usd_import` converts it to an in-memory glTF binary for the glTF
importer. The cooked outputs are the glTF importer's; nothing at run time knows the source was USD.

### Files the import reads

Before the stage is composed, the importer walks the layer graph itself: the root file, then every sublayer,
reference and payload each layer names (in every variant, selected or not), depth first, and the texture files
materials name. Each file is read once, inside the source's project (the nearest folder with a project manifest),
or inside the file's own folder when it is in no project; symlinks are resolved and must stay inside. Absolute
paths, URLs and paths that leave that root are not read (a warning each). The reader then composes from these
bytes only. A reference to a missing layer composes as empty, with a warning.

- A sublayer, reference or payload chain that comes back to a file already on it is refused with
  `usd_reference_cycle`, naming the chain (`a.usda -> b.usda -> a.usda`). A file referencing itself counts,
  even for another prim of it: use an internal reference (`</Prim>`) instead.
- A chain longer than `max_reference_depth` (default 16) is refused with `usd_reference_too_deep`.
- Layers inside a `.usdz` resolve inside the package; a package nested in a package is not read.

### What is imported

| USD | Becomes |
|---|---|
| Prims (`Xform`, `Scope`, untyped `def`, `SkelRoot`, `Mesh`, `Camera`, lights) | One glTF node each, in hierarchy order, named after the prim |
| `xformOpOrder` with `translate`, `scale`, `rotateX/Y/Z`, `rotateXYZ` (and the other five orders), `orient`, `transform`, `!invert!` | The node's translation, rotation and scale (decomposed; shear is lost). `!resetXformStack!` is read as a plain transform, with a warning |
| Inactive, abstract (`class`), `over`-only, `visibility = "invisible"`, `purpose` `guide` or `proxy` prims | Skipped with their subtrees |
| `Mesh` `points`, `faceVertexCounts`, `faceVertexIndices` | Triangles, polygons fanned from their first corner; `orientation = "leftHanded"` reverses the winding |
| `normals` / `primvars:normals`, every two-component primvar (`primvars:st` first, then by name, up to 8) | `NORMAL`, `TEXCOORD_n` (V flipped to glTF); every interpolation (`constant`, `uniform`, `vertex`, `varying`, `faceVarying`) and indexed primvars (`:indices`) |
| `primvars:displayColor` / `displayOpacity` | `COLOR_0` when not constant; a constant color is the base color of a mesh with no bound material |
| `GeomSubset` (`elementType = "face"`, family `materialBind` or none) with a binding | One primitive per subset material |
| `material:binding:preview`, else `material:binding` (inherited down the hierarchy) | The primitive's material; `doubleSided` makes it double-sided |
| `UsdPreviewSurface`: `diffuseColor`, `emissiveColor`, `metallic`, `roughness`, `opacity`, `opacityThreshold`, `normal`, `occlusion` | Base color (alpha from `opacity`; `BLEND` below 1, `MASK` with a threshold), emissive, metallic, roughness, normal and occlusion. `useSpecularWorkflow` is read as metallic-roughness, with a warning |
| `UsdUVTexture` `inputs:file` through a `UsdPrimvarReader_float2` (`varname`) | An embedded image (PNG and JPEG as they are, others re-encoded as PNG) on the named UV set. Metallic and roughness from other channels or files are packed into one image (roughness in green, metallic in blue); occlusion from a channel other than red likewise. `UsdTransform2d` is not applied (a warning) |
| `Camera` | A glTF camera: `yfov = 2 atan(verticalAperture / (2 focalLength))`, `clippingRange` in meters; orthographic apertures in tenths of a unit |
| `DistantLight`, `SphereLight` (with `shaping:cone:angle`: spot), `DiskLight`, `RectLight` (spot lights, a warning) | `KHR_lights_punctual` lights; intensity `intensity × 2^exposure`, unconverted (USD light units are not glTF's) |
| References, payloads, sublayers, inherits, variant sets | Flattened by composition; variant sets at their authored selection (the report lists every selection) |
| Instances (`instanceable = true`) | Each instance becomes its own nodes; instances of one prototype mesh share one converted mesh |
| `Skeleton`, `SkelAnimation`, `BlendShape` | Not imported: meshes under a `SkelRoot` come in rigid in their bind pose, with a warning |
| Other prim types (`Cube`, `Sphere`, `Points`, `BasisCurves`, `PointInstancer`, `DomeLight`, ...) | Skipped with their subtrees, with a warning |

Values with time samples and no default read their earliest sample: the import is static.

### Units and axes

The stage's `metersPerUnit` scales positions, translations, camera clipping ranges and orthographic sizes to
meters. USD's own fallback when a stage authors none is 0.01 (centimeters), so an unmarked file comes in at a
hundredth of its numbers: set `meters_per_unit` in the `.meta` settings if it was authored in meters. A
`upAxis = "Z"` stage is turned to +Y up (the stage's +Z becomes +Y, +Y becomes -Z). Both are converted into the
data, so node transforms stay free of a root correction. Cameras and lights on a Z-up stage hang from a child node
(`<name>_axis`) that keeps them looking where they looked.

### Import settings (`.meta`)

`UsdImportSettings`, next to the glTF keys in the `settings` string:

| Key | Default | Meaning |
|---|---|---|
| `scale` | `1.0` | Uniform scale after the unit conversion |
| `meters_per_unit` | none | Overrides the stage's `metersPerUnit` |
| `up_axis` | none | Overrides the stage's `upAxis` (`"Y"` or `"Z"`) |
| `import_cameras`, `import_lights` | `true` | Convert cameras and lights |
| `limits` | below | Caps (`UsdLimits`) |

Caps: `max_file_bytes` (each file, 512 MiB), `max_total_bytes` (all files of one import, 2 GiB), `max_layers`
(1,024), `max_reference_depth` (16), `max_prims` (1 Mi), `max_hierarchy_depth` (256), `max_vertices` (16 Mi),
`max_indices` (48 Mi), `max_polygon_corners` (1,024), `max_text_nesting` (128 bracket levels in a text layer,
checked before it is parsed). A panic inside the reader on a hostile file is caught and refused as
`usd_reader_failed` (the fuzz tests found none).

A USD file in `assets/` cooks to a model, so a layer that holds no mesh (a material library, an overrides-only
sublayer) fails its cook with `usd_no_mesh`: keep such layers in the project outside `assets/`, where the files
that reference them still read them.

### `editor.usd_inspect`

Runs the USD importer and returns its report; read-only. Params: `path` (a `.usda`, `.usdc`, `.usd` or `.usdz`
file) or `text` (inline `.usda`, at most 16 MiB; nothing outside it is read), and `scale`, `meters_per_unit`,
`up_axis`. Result: `{ format, layers, meters_per_unit, up_axis, scale, prims, nodes, meshes, instanced_meshes,
vertices, triangles, materials, textures, cameras, lights, variant_selections, warnings, glb_bytes }`.

```json
{"method": "editor.usd_inspect", "params": {"path": "assets/chair.usdz"}}
```

Errors are `invalid_params` with `data.usd_error`:

| Code | Meaning |
|---|---|
| `usd_bad_params` | Missing or conflicting `path` / `text`, a bad `scale`, `meters_per_unit` or `up_axis` |
| `usd_io` | A file could not be read |
| `usd_too_large` | A file over `max_file_bytes`, or all files over `max_total_bytes` |
| `usd_not_usd` | The root is neither a text layer (`#usda`), a crate file (`PXR-USDC`) nor a zip package |
| `usd_parse` | A layer does not parse, the stage does not compose, or a mesh is malformed (counts and indices disagree, an index outside the points) |
| `usd_reference_cycle` | A sublayer, reference or payload cycle |
| `usd_reference_too_deep` | A chain deeper than `max_reference_depth` |
| `usd_limit` | Another cap exceeded |
| `usd_bad_settings` | The `.meta` settings string is malformed |
| `usd_no_mesh` | The composed stage has no mesh |
| `usd_reader_failed` | The reader stopped on an internal error (the input is refused) |

The cook reports the same codes. `regia import file.usdz [--out GLB] [--scale F] [--json]` prints the same report;
`--out` writes the converted GLB.

### Fuzzing

`import.usd` (mutated text layers) and `import.usdz` (mutated packages holding a binary crate layer and a PNG) run
the whole import, walk to glTF import, in the editor's `fuzz.targets`; `xtask fuzz` runs them too.
