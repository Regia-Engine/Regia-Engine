# Cooked mesh (`regia_asset::CookedMesh`), format version 3

The runtime mesh the glTF importer writes for every primitive: the payload of a cooked file
(`FCKD` header, type tag `regia_asset::CookedMesh`) in the dev cache
(`.regia/cooked/<os>/<id>.fckd`) and in paks. Version 2 (ADR-226, importer 1.8.0) adds the
skin, vertex colour and sparse morph-target blocks; nothing before them changed. Version 3
(ADR-1622) appends one byte, `skin_influences`: how many joint influences the renderer draws. The schema hash
is `cooked-mesh` in [`format-hashes.ron`](format-hashes.ron).

The payload is [postcard](https://docs.rs/postcard) (not self-describing): fields are written in
the order below, integers as varints, `Vec<u8>` as a length plus bytes, `Option` as a 0/1 tag.
Every stream inside a `Vec<u8>` is little endian.

## Layout

| # | Field | Encoding | Since |
|---|---|---|---|
| 1 | `aabb_min`, `aabb_max` | `[f32; 3]` each, metres | v1 |
| 2 | `bounding_sphere` | centre xyz + radius | v1 |
| 3 | `stream_positions` | stream 0: `f32x3` per vertex | v1 |
| 4 | `stream_normals_tangents` | stream 1: `snorm16x4` (octahedral normal xy, 0, tangent sign) | v1 |
| 5 | `stream_uv0` | stream 2: `f16x2` per vertex | v1 |
| 6 | `indices` | `u16` or `u32` per index (`index_width`) | v1 |
| 7 | `index_width` | `U16` / `U32` | v1 |
| 8 | `vertex_count` | `u32` | v1 |
| 9 | `lods` | `[(screen_relative_size, index_byte_offset, index_byte_length)]` | v1 |
| 10 | `submeshes` | `[(index_byte_offset, index_byte_length, material_slot)]` | v1 |
| 11 | `lod_metric` | `u32` (absent in importer 1.6.0 payloads) | v1 (1.7.0) |
| 12 | `format_version` | `u32` = 3 (2 in v2 payloads) | **v2** |
| 13 | `skin` | `Option<CookedSkin>` | **v2** |
| 14 | `stream_color0` | stream 3: `COLOR_0` as linear `unorm16x4` RGBA per vertex, or empty | **v2** |
| 15 | `morph_targets` | `[CookedMorphTarget]` | **v2** |
| 16 | `skin_influences` | `u8`: 4 (default) or 8 (opt-in) | **v3** |

### `skin_influences` (v3)

How many influences per vertex the renderer draws. **4** is the default and what every v1 and
v2 payload loads as: the vertex-shader path draws the four strongest (`CpuSkin::top4`), whatever
the skin stores. **8** is an opt-in, written only when the glTF importer setting
`skin_influences: 8` is in the source's `.meta` settings and the skin has a second influence set
(`JOINTS_1`/`WEIGHTS_1`). The influence streams are the same either way: the opt-in changes
this byte only. Validation: any other value, or 8 on a mesh without a skin or without a second
set, is an error.

`CookedMesh::to_cpu_mesh` sets `CpuSkin::eight_influences` from it. A host turns it into
render options with `regia_render::SkinningOptions::for_cooked_skin` (eight influences on the
compute skinning path; `None`, so nothing changes, for a default mesh). A device without compute
shaders draws the four strongest.

### `CookedSkin` (stream 4)

| Field | Encoding |
|---|---|
| `joint_names` | `[String]`: one per joint index, **in the cooked skeleton's joint order** |
| `joint_width` | `U8` when the skin has ≤ 256 joints, `U16` otherwise |
| `stream_joints0` | four joint indices per vertex (`u8x4` or `u16x4`) |
| `stream_weights0` | `unorm16x4` per vertex |
| `stream_joints1` | influences 5–8 (same width), or empty |
| `stream_weights1` | `unorm16x4`, empty exactly when `stream_joints1` is |

- **Joint indices are skeleton ids.** The importer maps each glTF skin slot (`JOINTS_n` value)
  to the id the `regia_animation::Skeleton` cooked from the same file gives that joint (the
  skeleton orders joints topologically, so slot ≠ id in general). A palette built from that
  skeleton indexes the stream directly; `joint_names` lets a runtime check (or remap to) another
  skeleton by name.
- **Influence slots keep the authored order** (no sorting by weight). The second set holds
  glTF `JOINTS_1`/`WEIGHTS_1`; sets past the second are dropped with a warning.
- **Weights of one vertex (both sets) sum to exactly 65535.** Float weights are normalised and
  quantised by largest remainder (`regia_asset::quantize_skin_weights`: floor of
  `w / Σw × 65535`, the leftover units to the largest fractional parts, earlier slot first on a
  tie), so each weight is within 1/65535 of its normalised value and the same input always gives
  the same bits. `unorm8` weights are widened ×257 and `unorm16` weights are kept bit for bit when
  they already sum to one (`normalize_unorm16_weights`). A vertex whose weights are all zero gets
  65535 on slot 0. A zero-weight slot pointing past the skin writes joint 0; a weighted one is an
  import error.
- The renderer draws four influences per vertex by default (`specs/13` § 8): `CpuSkin::top4`
  keeps a four-influence vertex as stored and the four largest of eight (renormalised). A mesh
  with `skin_influences` 8 draws all eight on the compute skinning path (v3, below).
- **Skinned positions and the skeleton share one frame** (ADR-226). A skinned primitive's
  positions are baked with the skin's matrix (the import's axis yaw times the world of the first
  node that uses the skin), not its own node world; the cooked skeleton's inverse bind matrices
  undo that bake and its root joints carry their parent nodes' transform. Skinning with the cooked
  skeleton's palette therefore gives the glTF 2.0 § 3.7.3 result for any node hierarchy, and the
  bind pose lands on the cooked positions. Data only: the layout is unchanged.

### `CookedMorphTarget`

| Field | Encoding |
|---|---|
| `name` | glTF `mesh.extras.targetNames[i]`, or `target_<i>` |
| `vertex_indices` | `[u32]`, strictly increasing: the vertices this target moves |
| `position_deltas` | `f16x3` per listed vertex (metres, mesh space) |
| `normal_deltas` | `f16x3` per listed vertex, or empty |
| `tangent_deltas` | `f16x3` (xyz) per listed vertex, or empty |
| `delta_min`, `delta_max` | `[f32; 3]` component bounds of the stored position deltas (0 when none) |

A vertex is listed when any of its deltas is non-zero after rounding to `f16` (round to nearest
even). Each delta follows its base attribute: position deltas turn by the bake's linear part
like the base positions; normal deltas by the base normals' inverse-transpose matrix, divided by
the length the base normal had before it was renormalised (so a 0.01-scaled node does not
shrink them); tangent deltas turn by the linear part and keep their length. Morph targets are
drawn by the GPU morph pass (`regia_render::MorphTargets::from_cooked`, `specs/13` § 8).

## Reading older payloads

`CookedMesh::from_postcard` tries, newest first:

1. **v3**: `format_version` must be 3 (a larger value is `UnsupportedVersion`).
2. **v2** (`format_version` 2, no `skin_influences` byte): loads with `skin_influences` 4 and
   `format_version` 3. Everything it held is unchanged, so it draws as a v2 build drew it.
3. **v1 with `lod_metric`** (importer 1.7.0): loads with `skin: None`, no colours, no morph
   targets, `skin_influences` 4 and the current `format_version`. The vertices are exactly what
   a v1 build loaded.
4. **v1 without `lod_metric`** (importer ≤ 1.6.0): as before, `lod_metric` legacy and the LOD
   chain moved onto the scatter curve.

An older layout must consume the whole payload, so a damaged v2 or v3 payload is an error, never
an older mesh that silently lost its skin. A v3 payload cut by its last byte still says version
3, so it is an error rather than a v2 mesh. `CookedMesh::payload_version` reads the version without
validating the streams.

## Migration

- **Loader:** paks and caches holding v1 or v2 meshes load unchanged (above); nothing must be
  rebuilt.
- **`regia upgrade-project`:** the `cooked-mesh` format scans `*.fckd` files; a v1 or v2 mesh is
  re-encoded at v3 (`regia_asset::migrate_cooked_mesh_file`, header kept except
  `payload_len`, original saved as `<file>.bak`). Cooked files of other types are skipped. Paks
  are not rewritten. A migrated v2 file is byte for byte the file a v3 cook of the same mesh
  with the default setting writes.
- **Re-cook:** the glTF importer version is 1.8.0, so `regia cook` rebuilds every glTF once and
  skinned meshes gain their skin.

## Example (CPU side)

```rust
let mesh = regia_asset::load_mesh_bytes(&bytes)?.mesh; // CpuMesh
if let Some(skin) = &mesh.skin {
    assert_eq!(skin.joint_names, skeleton.joint_names);  // same import
    let render = regia_render::CpuMesh::from_cooked(&mesh)?; // joints + weights in LabVertex
}
for target in &mesh.morph_targets {
    println!("{}: {} vertices", target.name, target.vertex_indices.len());
}
```
