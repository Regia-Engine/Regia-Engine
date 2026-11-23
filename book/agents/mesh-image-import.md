# PLY, STL and PSD import (`editor.ply_inspect`, `editor.stl_inspect`, `editor.psd_inspect`)

Three source formats artists and engineers already have are cooked directly (ADR-3244): **PLY** (photogrammetry and scan
meshes with vertex colors), **STL** (CAD and 3D-print parts) and **PSD** (Photoshop's native image). Drop the file under
`assets/` and `regia cook` (or the editor import) does the rest. The readers are pure Rust, small, and hardened for
untrusted input; no dependency was added. No cooked format changes, so there is no new format version and no migrator.

- `.ply` and `.stl` are converted to an in-memory glTF 2.0 binary and cooked by the glTF importer, as `.obj` is
  (ADR-1602): you get the same cooked mesh, `model`, meshlets and LODs. One mesh node, one primitive, no material (assign
  one in the scene).
- `.psd` decodes to RGBA8 and takes the normal texture path, so it behaves like `.png`: usage from the file name,
  `max_size`, `compress`, `quality`, mips and streaming settings all apply.

All settings are RON in the `.meta` `settings` string; every field is optional. A blank string is the defaults, the
texture keys of a PSD and the glTF keys of a mesh are ignored by the format's own parser.

## PLY

Greg Turk's format: an ASCII header, then the body as `ascii`, `binary_little_endian` or `binary_big_endian`.

| Read | Names |
| --- | --- |
| Position | `x y z` |
| Normal | `nx ny nz` |
| Color | `red green blue [alpha]`, also `r g b a` and `diffuse_red ...`. `uchar`/`ushort` and other integers are normalized to 0..1, floats are taken as 0..1. |
| UV | `s t`, `u v`, `texture_u texture_v`, `texture_s texture_t`. V is flipped to glTF's top-left origin. |
| Faces | the list property `vertex_indices` (or `vertex_index`); polygons are fan-triangulated from the first corner. |
| Corner UVs | an optional face list `texcoord` (two floats a corner, as MeshLab writes it); corners with different coordinates split the vertex. |

Everything else (other elements, other properties, `comment`, `obj_info`) is read past and ignored. A file with vertices
but no faces (a point cloud) is refused with `ply_no_faces`; point clouds are a follow-up. Colors are stored as
`COLOR_0` as written (no sRGB conversion).

| Setting | Default | Meaning |
| --- | --- | --- |
| `scale` | `1.0` | Uniform scale. PLY has no unit; it is read as meters. |
| `up_axis` | `Y` | `Z` turns a Z-up file to Y up. |
| `limits` | below | Hard caps. |

Limits: `max_file_bytes` 1 GiB, `max_vertices` 16 Mi, `max_faces` 16 Mi, `max_triangles` 32 Mi, `max_polygon_corners` 1,024,
`max_header_items` 64 (elements, and properties in one element), `max_header_bytes` 64 KiB. Counts are checked against the
limits before anything is allocated, so a header that claims four billion vertices is `ply_limit`, not an allocation.

## STL

The 3D Systems format: a list of triangles, as text (`solid ... endsolid`) or binary (80-byte header, `u32` count, 50
bytes a triangle). A file whose length is exactly `84 + 50 * count` is binary (many binary files begin with `solid`);
otherwise a file that starts with `solid` is text. Binary attribute bytes are ignored; several `solid`s in one text file
are merged. A text file must end each `solid` with `endsolid`; one that does not is `stl_parse` (the usual sign of a
truncated file).

- **Unit: millimeters by default.** STL carries none, and CAD and slicers write millimeters. `unit` is `Millimeters`,
  `Centimeters`, `Meters` or `Inches`; `scale` multiplies on top.
- **Axis: Z up by default**, as CAD and 3D printing write it, turned to Y up. `up_axis: Y` keeps the file as written.
- **Welding:** every triangle lists its own three vertices. Vertices closer than `weld_tolerance` (file units, default
  `0.0001`; `0` turns welding off) merge on a grid of that size, so two points closer than the tolerance can still stay
  apart when a grid line falls between them. Triangles that collapse are dropped (`degenerate_removed`).
- **Normals:** a stored facet normal is used when it is finite and not zero; otherwise it is recomputed from the winding
  (counter-clockwise is the front face). Faces whose normals differ by less than `crease_angle_degrees` (default `40`)
  share one area-weighted vertex normal at a welded position; sharper edges keep separate vertices, so a cube stays hard
  edged (24 vertices) while `crease_angle_degrees: 180` fully smooths it (8).

Limits: `max_file_bytes` 1 GiB, `max_triangles` 20 Mi. A binary header that claims more triangles than the file holds is
`stl_parse`.

```ron
(unit: Millimeters, weld_tolerance: 0.001, crease_angle_degrees: 60)
```

## PSD

Adobe's Photoshop File Format. By default the **merged composite** (the image data section) is decoded:

- 8 and 16 bits a channel (16-bit rounds to 8: `(v * 255 + 32767) / 65535`), RGB and grayscale.
- Raw, RLE (PackBits), and ZIP compression with and without prediction.
- Alpha is the channel after the color channels **when the file flags it as transparency** (a negative layer count, the
  way Photoshop writes it). `alpha: Always` uses that channel whenever it exists, `alpha: Never` makes the image opaque.
- A file saved without "Maximize Compatibility" can hold a placeholder composite; import a layer instead.

**One layer:** `layer: Some("Name")` imports the pixels of the single top-level layer with that exact name (the Unicode
name when stored, else the Pascal name). The output is the layer's own rectangle, not the canvas. Limits: layer opacity,
blend mode, layer masks, clipping and visibility are ignored; text and smart-object layers import as their stored raster; a
layer inside a group is not top level and is not found; a layer with layer effects (`lfx2`, `lrFX`) is refused
(`psd_layer_effects`: the effects would not be in its pixels), as are groups and empty layers (`psd_layer_empty`); a name
shared by two top-level layers is `psd_layer_ambiguous`.

Refused with a code: PSB (`psd_psb`), bitmap, indexed, CMYK, multichannel, duotone and Lab modes
(`psd_unsupported_mode`), 1-bit and 32-bit depths (`psd_unsupported_depth`).

```ron
(layer: Some("Sprite"), usage: Some(Color), compress: Some(false))
```

Limits: `max_file_bytes` 1 GiB, `max_pixels` 64 Mi (8192 by 8192), `max_layers` 8,192. The pixel count is checked from the
header, before any plane is allocated.

## Agent methods

`editor.ply_inspect`, `editor.stl_inspect` and `editor.psd_inspect` are read-only (`path` of the file or `sample:
"fixture"`, and `settings` as RON) and report what a cook would produce; nothing is written. `path` is relative to the
project, or absolute under the project or the temp dir ([Request paths](safety.md#request-paths), ADR-7388); any other
path is `path_outside_project`.

```json
{"method": "editor.stl_inspect", "params": {"path": "assets/parts/bracket.stl", "settings": "(unit: Millimeters)"}}
```

Replies: PLY `{ encoding, vertices, triangles, has_normals, has_colors, has_uvs, faces_skipped, warnings }`; STL
`{ encoding, triangles_in, triangles, vertices_in, vertices, welded_positions, degenerate_removed, normals_recomputed,
warnings }`; PSD `{ width, height, bits_per_channel, grayscale, has_alpha, layers, source }`.

### Error codes

`data.ply_error`, `data.stl_error` and `data.psd_error` hold one of these (lowercase; the cook reports the same text):

| Code | Cause |
| --- | --- |
| `ply_bad_params`, `stl_bad_params`, `psd_bad_params` | Not exactly one of `path`/`sample`, or `settings` is not a string. |
| `ply_io`, `stl_io`, `psd_io` | The file cannot be read. |
| `ply_too_large`, `stl_too_large`, `psd_too_large` | The file is over `max_file_bytes`. |
| `ply_parse`, `stl_parse`, `psd_parse` | A malformed or truncated file (unknown format line, bad property type, a face index out of range, a negative or wrong list length, an STL count that does not match the length, a PSD that ends early or has bad RLE or ZIP data). |
| `ply_limit`, `stl_limit`, `psd_limit` | A count or size past `limits` (vertices, faces, triangles, polygon corners, header items, pixels, layers). |
| `ply_bad_settings`, `stl_bad_settings`, `psd_bad_settings` | The settings RON does not parse. |
| `ply_no_faces` | The PLY has no faces (a point cloud). |
| `stl_no_mesh` | The STL has no triangles, or all collapsed. |
| `psd_psb` | A PSB (large document) file. |
| `psd_unsupported_mode` | A color mode other than grayscale and RGB. |
| `psd_unsupported_depth` | A bit depth other than 8 and 16. |
| `psd_unsupported_compression` | An unknown compression method. |
| `psd_layer_not_found` | No top-level layer has the name. |
| `psd_layer_ambiguous` | Two top-level layers have the name. |
| `psd_layer_effects` | The layer has layer effects. |
| `psd_layer_empty` | The layer is a group or has no pixels. |

## Fuzzing

`import.ply`, `import.stl` and `import.psd` are in `fuzz.targets` in the editor (convert or decode, then for the meshes the
whole glTF import of the result; the PSD target decodes the composite and the named layer).
