# glTF geometry and rig lint (`asset.gltf_lint`)

Inspect a `.gltf` or `.glb` before cooking it. This read-only editor method reports
redundant mesh geometry, near-coincident triangle copies and animation channel warnings.
Permission: `read`. Read-only sessions may call it. No source or cooked file is rewritten.
The editor advertises the `gltf_lint` capability; the runtime-only schema has no pipeline dependency.

```json,ignore
{"jsonrpc":"2.0","id":1,"method":"asset.gltf_lint","params":{"path":"assets/props/panel.glb"}}
```

The result includes `ok: true`, `path`, counts `meshes`, `primitives`, `triangles`, and:

| Field | Rows |
|---|---|
| `duplicate_meshes` | `{original, duplicate}` mesh indices. |
| `coincident_triangles` | `{mesh, primitive, pair: {keeper, suspect}}`, all zero-based source indices. |
| `rig_channels` | `{code, animation, channel, node}`. |
| `skipped_primitives` | `{mesh, primitive, mode}` for points, lines, strips and fans. |

`ok` means the scan completed, even when findings exist. Treat any nonempty finding list
as requiring author review. A nonempty `skipped_primitives` means geometry coverage is partial.
Budgets fail with an error; an incomplete budget-limited scan is never returned as clean.

## Geometry policy

Duplicate meshes have exactly equal source-local triangle positions and multiplicities,
ignoring winding, triangle order, primitive grouping, materials, normals, UVs and rig data.
Signed zero compares equal. Distinct meshes with different shading or deformation can be
intentional: this is an advisory geometry warning, not a safe merge recommendation.
Multiple nodes referencing one mesh do not produce duplicate-mesh rows. Meshes containing
unsupported topology are excluded from duplicate-mesh comparison. Unused meshes are inspected.

Coincident triangles are compared **within each primitive**, before cook-time cleanup. Positions
are quantized to a configurable grid using ties-to-even rounding; triangles sharing the same
three quantized corners are candidates, regardless of winding. The least UV-anisotropic candidate
is the keeper; the earliest triangle breaks ties. Other candidates must be closer than `separation`
at every matched corner, plus satisfy one of:

- separation below `exact_separation`, irrespective of UVs;
- mean matched-corner UV distance below `uv_distance`;
- suspect anisotropy above `anisotropy_floor` and its ratio to the keeper above `anisotropy_ratio`.

File scans use `TEXCOORD_0`. Anisotropy is maximum/minimum UV-edge length per position-edge length.
Edges at most 1e-6 m are ignored; UV edges at most 1e-12 produce the degenerate score 1e6.
The minimum edge scale is floored at 1e-12; the keeper score is floored at 1e-9 when taking
the anisotropy ratio. These numerical stabilizers match the source predicate. Without UVs,
only the exact-separation rule applies. A thin shell with distinct, similarly stretched UVs
is retained. The threshold comparisons are exclusive.

This preserves the source predicate's **quantization-boundary limitation**: nearby corners in
different grid cells are not candidates. It does not detect intersecting triangles, coplanar
partial overlap, overlap across primitives or overlap after node/instance transforms. All
measurements use glTF source-local meters, before import scale or forward-axis conversion.
The report never recommends deleting rigged or morphing data automatically.

## Settings

Pass an optional `settings` object. Omitted fields take defaults; unknown fields are refused.
The Rust API takes the same `GltfLintSettings` struct.

```json,ignore
{"jsonrpc":"2.0","id":2,"method":"asset.gltf_lint","params":{
  "path":"assets/props/panel.glb",
  "settings":{"geometry":{"separation":0.0001},"max_triangles":500000}
}}
```

| Setting | Default |
|---|---:|
| `geometry.quantization` | 0.0001 m |
| `geometry.separation` | 0.0001 m |
| `geometry.exact_separation` | 0.000001 m |
| `geometry.uv_distance` | 0.02 |
| `geometry.anisotropy_ratio` | 2 |
| `geometry.anisotropy_floor` | 3 |
| `max_vertices` | 4,000,000 POSITION elements across primitive uses |
| `max_triangles` | 2,000,000 triangle-list triangles |
| `max_channels` | 100,000 channels |
| `max_animation_elements` | 4,000,000 input/output elements across channel uses |
| `max_findings` | 10,000 rows across all four lists |

Thresholds must be finite and positive, exact separation cannot exceed separation, anisotropy
thresholds must be at least 1, and budgets must be positive. The checked loader's own parsing,
buffer and extension limits still apply before these lint budgets. Images are not decoded.
The pure `coincident_pairs` slice API validates numeric inputs but has no file-work budget;
callers already own those input arrays and must bound their sizes.

## Rig warnings

| Code | Meaning |
|---|---|
| `gltf_rig_non_joint_channel` | Translation/rotation/scale targets a node absent from every skin's joints. Regia's skeleton clip importer does not import this node track. This is legal glTF. |
| `gltf_rig_duplicate_channel` | Another channel in the same animation writes the same node/property. |
| `gltf_rig_morph_target_missing` | Weights target a node with no morph targets or inconsistent target counts across its primitives. |
| `gltf_rig_input_times` | Missing, negative, non-finite or non-increasing key times. |
| `gltf_rig_output_count` | Sample count disagrees with key count, interpolation or morph-target count. Cubic spline requires three values per key. |
| `gltf_rig_nonfinite_output` | Unreadable or non-finite output samples. |

Structural file errors may be rejected by the loader before channel warnings can be produced.
Warnings describe source channels, not a full skeleton or animation-quality audit.

## Errors

Application errors use `F0002` with a lowercase `data.reason`. Missing files use `F0003`.
The dispatcher also enforces parameter types and the `read` permission.

| Reason | Meaning |
|---|---|
| `gltf_lint_params` | The handler did not receive a path string. |
| `gltf_lint_settings` | Invalid or unknown settings. |
| `gltf_lint_geometry` | Non-finite positions/UVs, invalid indices/counts, unreadable mesh data or unrepresentable quantized coordinates. |
| `gltf_lint_limit` | A work or findings budget was exceeded. Raise the named setting deliberately. |
| `gltf_unreadable` | File, buffer, accessor, structure or required extension rejected by the checked loader. |
| `path_outside_project` | The requested source leaves the project. |

## Rust and acceptance

`regia_asset_pipeline::gltf_lint::lint_gltf(path, &settings)` returns `GltfLintReport`.
`coincident_pairs` accepts positions, optional UVs and triangle indices directly.
Neither API changes importer behavior or frozen content formats.

Run `cargo test -p regia_asset_pipeline --lib gltf_lint`,
`cargo test -p regia_asset_pipeline --test gltf_lint`, and
`cargo test -p regia_editor --test gltf_lint_agent`. The last test exercises the actual
dispatcher, read-only mode, permission denial, settings, limits and path rejection without a GPU.

## Shared input budgets (ADR-8226)

`settings.input_limits` selects the [shared content resource policy](../user-guide/content-lint.md#resource-policy).
The report adds `input_limits` (the effective values) and `input_usage` (deterministic
encoded/external/decoded/accessor/work counters). Sparse and normalized accessor
expansion is admitted before loading buffers. Existing geometry budgets still apply.

```json,ignore
{"jsonrpc":"2.0","id":4,"method":"asset.gltf_lint","params":{"path":"assets/props/part.glb","settings":{"input_limits":{"max_json_bytes":1048576,"max_external_bytes":16777216,"max_decoded_bytes":33554432}}}}
```

Shared errors use `F0002` and `data.reason`: `content_lint_limit` for admission
failure, `content_lint_cancelled` for cooperative cancellation, and
`content_lint_changed` for a file changed during loading. Malformed input remains
`gltf_unreadable`; geometry-specific limits remain `gltf_lint_limit`. A failed
operation never returns a successful partial report. `lint_gltf_bounded` and
`open_gltf_bounded` accept `ContentBudget` with a cancellation probe. The existing
synchronous agent method has no separate in-flight cancel RPC. A file over the synchronous
ceiling returns `content_lint_limit` and names `asset.content_lint_start`.

Acceptance: `cargo test -p regia_editor --test content_lint_budget_agent` calls both
lint methods through the dispatcher, places their report counters in a reflected
test evidence component, and verifies those counters with `play.wait_until` and
`play.expect` on the next deterministic update. No simulation behavior is changed.
The editor's `fuzz.targets` includes `content.gltf_bounded`; its fixed input, JSON,
decoded, accessor and work ceilings apply on successful and rejected inputs.

## Robust coincidence mode (ADR-8347)

`settings.mode` selects the coincidence analysis. `legacy` (the default) is the predicate
above, unchanged, including its quantization-boundary limitation. `robust` is an explicit
diagnostic mode that replaces the legacy rows with `robust_coincidences`:

```json,ignore
{"jsonrpc":"2.0","id":3,"method":"asset.gltf_lint","params":{
  "path":"assets/props/panel.glb",
  "settings":{"mode":"robust","robust":{"tolerance_meters":0.0001,"max_candidate_pairs":200000}}
}}
```

What robust mode does differently:

- Strips (mode 5) and fans (mode 6) are expanded with the glTF 2.0 formulas, so they are
  compared and take part in duplicate-mesh detection. Points and lines stay in `skipped_primitives`.
  Triangle numbers in a strip or fan are positions in the expanded list.
- Triangles are bucketed by centroid in cells one tolerance wide. Each triangle is compared
  with its own cell and the 26 neighbors, so pairs across a cell boundary are found. All
  primitives of one mesh are compared together. Different meshes and instances are not.
- Each unordered candidate pair is counted against `robust.max_candidate_pairs` for the whole
  operation. Running out fails with `gltf_lint_limit` (named `max_candidate_pairs`). A dense
  cluster of identical triangles cannot turn into an unbounded quadratic scan.
- Positional agreement and attribute equivalence are separate fields. Nothing is welded or deleted.

| `robust_coincidences` field | Meaning |
|---|---|
| `mesh`, `keeper`, `suspect` | Mesh index and `{primitive, triangle}` locations. The keeper has the smaller sorted corner set. For identical corners it is the earlier location. |
| `agreement` | `exact` (every matched corner bitwise equal; `-0` equals `0`) or `tolerance`. |
| `max_separation_meters`, `tolerance_meters` | Largest matched-corner distance and the effective tolerance for the mesh. |
| `equivalence` | `equivalent`, `differs` (see `differences`) or `unknown` (only `unavailable` is nonempty). |
| `differences` | `material`, `texcoord_<n>`, `normal`, `skin`, `morph`. |
| `unavailable` | `texcoord_<n>_missing`, `khr_texture_transform_unread`, `material_extension_unread`. |
| `uv_sets_compared` | The `texCoord` sets the two materials sample. A material that samples no texture compares no UV set. |
| `safe_delete_candidate` | `true` only for `exact` agreement with `equivalent` data. A review label; the lint never deletes. |
| `fingerprint` | BLAKE3 hex of both sorted corner sets. It does not depend on primitive or triangle order. |

UV sets come from the material's base color, metallic-roughness, normal, occlusion, emissive,
transmission and thickness textures. This build does not read `KHR_texture_transform` or other
`KHR_materials_*` extensions. When the document uses them, the row says so in `unavailable`
and is never a safe-delete candidate. Skin influences compare as sorted (joint, weight) pairs
with nonzero weight. Morph targets compare position, normal and tangent deltas per target.

`mesh_evidence` has one row per primitive with excluded triangles: `non_finite`,
`repeated_index` (including strip stitching) and `zero_area`, each with the first triangle
index. Robust mode reports these and continues. Legacy mode still fails on non-finite input.
Out-of-range indices and mismatched attribute counts fail in both modes.
`robust_work` holds deterministic counters: `eligible_triangles`, `cells`, `candidate_pairs`.

| Robust setting | Default | Unit |
|---|---:|---|
| `robust.tolerance_meters` | 0.0001 | meters, exclusive |
| `robust.tolerance_relative` | 0 | fraction of the mesh bounding-box diagonal; the larger tolerance wins |
| `robust.uv_tolerance` | 0.02 | UV units, mean corner distance, exclusive |
| `robust.attribute_tolerance` | 0.00001 | per component, inclusive (meters for morph position deltas) |
| `robust.min_area_square_meters` | 1e-12 | square meters, inclusive |
| `robust.max_candidate_pairs` | 1,000,000 | pairs per operation |
| `robust.max_attribute_elements` | 32,000,000 | decoded non-POSITION elements per operation |

Robust rows and evidence rows count toward `max_findings`. Project policy can set all of these
(see [Source lint policy (agents)](content-lint.md#robust-geometry-controls)).

Acceptance: `cargo test -p regia_asset_pipeline --test geometry_robust_lint` and
`cargo test -p regia_editor --test gltf_lint_agent` (robust mode through the dispatcher,
checked with `play.wait_until` and `play.expect`).
