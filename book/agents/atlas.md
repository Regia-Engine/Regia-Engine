# Lightmap UV atlas (`editor.atlas_generate`)

`regia_asset_pipeline::atlas` unwraps a triangle mesh into charts and packs them into one square atlas, for
lightmap UVs (UV2). It is a Rust port of the open-source xatlas generator (MIT; ADR-1483,
`docs/ported-sources.toml`). It runs on the editor and pipeline side only; shipped games never link it.

## What it does

1. **Charts.** Vertices closer than a millionth of the mesh's bounding-box diagonal are welded, so seams in the
   source UVs or normals and an exporter's rounding at a pole do not cut the surface. Flat regions whose every
   outer edge bends by 90° or more (a cube's sides) become charts as they are. The rest grows from seeds, one
   face at a time, cheapest first. The cost weighs how far a face turns from the chart's normal, whether the
   chart stays round, whether the face closes a gap in the boundary, and whether it crosses a normal or texture
   seam. A face that turns more than about 73° from its chart never joins.
2. **Flattening.** A chart is projected onto its plane when that has no fold and little stretch (`ortho`).
   Otherwise a least squares conformal map is solved (`lscm`). A chart that still folds is split into pieces
   that project cleanly (`piecewise`).
3. **Packing.** All charts get the same texel density. Each is turned to its tightest bounding box, rasterized
   conservatively, grown by the texels a bilinear lookup can reach, padded, and placed where it touches no other
   chart (90° turns allowed). With `texels_per_unit` 0 the largest density that fits `resolution` is searched;
   a fixed density that does not fit is lowered, with a warning.

The result is deterministic: the same mesh and options give the same atlas for any number of threads.

## Rust API

```rust,wrap
# let positions: Vec<[f32; 3]> = Vec::new();
# let indices: Vec<u32> = Vec::new();
use regia_asset_pipeline::atlas::{generate, AtlasMesh, AtlasOptions};

let result = generate(&AtlasMesh::new(&positions, &indices), &AtlasOptions::default())?;
for v in &result.vertices {
    // v.uv in [0, 1]², v.xref = source vertex, v.chart = Some(chart) (None for skipped faces)
}
// result.indices: one triangle per source face, in source order, on the output vertices.
```

`AtlasMesh` takes optional `normals` and `uvs` (as long as `positions`) as seam hints. `AtlasOptions` holds
the chart weights (`normal_deviation_weight` 2, `roundness_weight` 0.01, `straightness_weight` 6,
`normal_seam_weight` 4, `texture_seam_weight` 0.5, `max_cost` 2, `max_iterations` 1, `max_chart_area` and
`max_boundary_length` 0 = no limit) and the packing settings (`resolution` 1024, `texels_per_unit` 0, `padding`
2, `bilinear`, `rotate_charts`, `rotate_charts_to_axis`, `brute_force`, `threads` 0 = all cores).
The Rust API also has `min_chart_texels` (default 1): a chart smaller than that many texels a side is stretched to it, which
`regia bake-lightmaps` uses for its density rule.

Two charts' footprints end up at least `2 * padding + 1` texels apart, and every chart stays `padding` texels
inside the atlas border.

## `editor.atlas_generate`

Read-only. Give a built-in `shape` (`"cube"`, or `"sphere"` with `segments` 3..=512, default 32) or inline
`positions` (flat x, y, z numbers) and `indices` (at most 3,000,000). Options: `resolution`, `padding`,
`texels_per_unit`, `max_cost`, `max_iterations`, `brute_force`, `rotate_charts`, `bilinear`. The call uses one
worker thread. `include_uvs: true` also returns the output buffers.

```json
{ "method": "editor.atlas_generate", "params": { "shape": "sphere", "segments": 48, "resolution": 1024 } }
```

Returns `width`, `height`, `charts`, `chart_kinds` (`planar`, `ortho`, `lscm`, `piecewise` counts), `faces`,
`vertices`, `texels_per_unit`, `utilization` (share of the atlas covered by chart triangles, padding not
counted), `pack_passes` and `warnings` (`[{ code, message }]`). With `include_uvs`: `uvs` (flat u, v per output
vertex), `xref`, `indices` and `chart_faces`.

### Codes

Warnings (`warnings[].code`; the atlas is still made):

| Code | Meaning |
|---|---|
| `atlas_degenerate_triangle` | A face with zero area, a repeated or co-located corner, or a non-finite normal was skipped. Its corners get chart-less vertices at uv (0, 0). |
| `atlas_density_reduced` | The fixed `texels_per_unit` did not fit and was lowered. |

Errors are `invalid_params` errors with `data.atlas_error` set to:

| Code | Meaning |
|---|---|
| `atlas_bad_params` | No `shape` and no `positions` + `indices`, an unknown shape, `segments` out of range, positions that are not x, y, z numbers, indices that are not non-negative 32-bit integers, or too many indices. |
| `atlas_index_count` | The index count is not a multiple of three. |
| `atlas_index_out_of_range` | An index names a vertex past the end of `positions`. |
| `atlas_non_finite` | A position is NaN or infinite. |
| `atlas_attribute_length` | `normals` or `uvs` is not as long as `positions` (Rust API only). |
| `atlas_invalid_option` | An option is out of range (`resolution` 8..=16384, `padding` at most 64, finite non-negative weights, `max_cost` > 0, `max_iterations` at most 64). |
| `atlas_no_triangles` | Every face is degenerate, or there are none. |
| `atlas_does_not_fit` | The charts do not fit even at the smallest density tried. |

## `regia atlas`

```
regia atlas rock.obj --resolution 1024 --out rock_uv2.obj
regia atlas rock.obj --json
```

Reads the `v` and `f` lines of a Wavefront `.obj` (polygons are fanned; `i/t/n` corners and negative indices
work) and prints the same report as `editor.atlas_generate`. `--out` writes the unwrapped mesh as `.obj`: one
`v` and one `vt` per output vertex (atlas coordinates, `vt` y up as `.obj` expects) and one `f` per source face,
so the charts can be inspected in any 3D tool. A generator error is printed with its `atlas_*` code and exits 1.
Options: `--resolution`, `--padding`, `--texels-per-unit`, `--brute-force`, `--threads`.

## Checking an unwrap without a clock

The tests check geometry, not timings. Every chart triangle is counter-clockwise with positive area and lies
inside the atlas. No two triangles overlap (an exact pairwise test). Chart footprints keep the padding apart.
A cube gets six planar charts. A 48×24 sphere packs above 60 % at 1024. One, three and eight threads give the
same result. An agent can make the same checks from `include_uvs`.

## Relation to `regia bake-lightmaps`

`regia bake-lightmaps` still unwraps with `lightmap_uv` (ADR-860), which projects planar charts and packs
rectangles. The lightmap work in v0.3.0 is meant to switch it to this generator.
