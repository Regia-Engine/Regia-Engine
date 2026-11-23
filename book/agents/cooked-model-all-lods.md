# Load every cooked model LOD

`regia_render::cooked_model::load_cooked_models_all_lods(&mut server, &gltf_ids)` is the opt-in
host API for authored LODs. Pass the glTF asset IDs, not their `model` sub-asset IDs. Results follow
input order; parts follow the cooked manifest order. The existing `load_cooked_models` and
`load_cooked_model` still return only authored LOD 0 and generated index LOD 0.

Each `CookedModelLods` contains:

- `parts`: exact `node` names, stable `mesh_id` primitive identities, `authored_lod`, a `material`
  index, cooked `vertices`, the complete `indices` buffer and generated `index_lods` ranges.
- `materials`: complete resolved `LabMaterial` values for all authored LODs, in first-use order.
  Distinct cooked material IDs remain distinct, even when their factors happen to match.
- `material_ids`: identities parallel to that table; `None` names the default material.

Authored LOD comes from a case-insensitive `_LOD<n>` node suffix. Names without a valid suffix are
LOD 0. Several primitives can have the same node name; distinguish them by `mesh_id` and manifest
position. The loader does not guess a distance policy or hide parts: the host chooses which
authored parts to submit.

Generated index LOD is a separate choice. `part.lod_indices(level)` borrows a selected index
slice, and `part.lab_mesh(level)` copies that level into a rigid `LabMesh` with the part's material
index. Both return `None` for an absent level or invalid edited range. When combining model
material tables, add each model's table offset to its draw's material index. Keep the table and
selected parts together; do not map every LOD to the LOD-zero material.

```rust
fn draw_lod_one(
    server: &mut regia_asset::AssetServer,
    gltf_ids: &[regia_asset::AssetId],
    mut submit: impl FnMut(regia_render::LabMesh, &[regia_render::LabMaterial]),
) -> Result<(), regia_render::cooked_model::CookedModelLodsError> {
    let models = regia_render::cooked_model::load_cooked_models_all_lods(server, gltf_ids)?;
    for model in &models {
        for part in model.parts.iter().filter(|part| part.authored_lod == 1) {
            if let Some(mesh) = part.lab_mesh(0) {
                // mesh.material indexes this model's table.
                submit(mesh, &model.materials);
            }
        }
    }
    Ok(())
}
```

## Tangents and cooked formats

The loader accepts both cooked mesh forms, and one batch may mix them:

- **GPU-ready meshes** are the fast path. Their vertex bytes, tangents included, are copied as
  they are, with no decode and no tangent generation. A desktop pak stores meshes GPU-ready only
  when it is built with `REGIA_PAK_GPU_MESHES=1` in the environment of `regia cook` / `regia
  package` (or `pack_cooked_directory_with(.., gpu_meshes: true)`). Android paks never do.
- **Compact meshes** are what everything else holds: the default cook output, the loose dev cache
  (`.regia/cooked/<platform>`), every Android pak, and skinned or vertex-colored meshes in any pak
  (the GPU-ready layout cannot keep those streams). A compact mesh stores only tangent handedness,
  so it goes through the legacy decode once at load: MikkTSpace tangents over LOD 0 for the one
  vertex array that every generated LOD shares, and the compact mesh's own generated LOD table.
  The vertices are exactly the legacy loader's, and exactly what the offline
  `gpu_mesh_file_from_cooked` conversion writes. As with `load_cooked_models`, a skinned mesh
  draws rigid in its bind pose and `COLOR_0` is not applied.

`assets.status` and `AssetServer::load_forms()` (`gpu_meshes`, `compact_meshes`) tell which form
loaded. Every index of every generated LOD is validated before any range is exposed. A mesh with
no generated LOD table exposes one level covering all indices. This API adds no file format.

Other Rust errors are `Load(CookedModelError)` for asset loading/validation and `Empty(model_id)`
for an empty manifest. A missing texture warns and retains the existing material fallback policy.
An empty input batch succeeds with an empty result.

## Texture sharing and verification

One compute-pool batch reads each referenced texture once across all models and authored LODs.
Materials carry shared image identities, so the renderer reuses uploads across those models and
LODs. A color image and the same image sampled as linear data require separate GPU views/uploads;
sharing is per image and color space. The loader performs no GPU upload itself.

Shared images are built from the maps `LabMaterial::from_cooked` has already fitted to the active
tier's resident cap (`regia_render::resident_cap`, for example 4096 on Desktop High and 2048 on
Android), so an image never uploads larger than the legacy loader's would. Load after the tier is
published (`set_resident_cap`, or the first frame) for the cap to apply. A block-compressed chain is
shared as views of the same cooked bytes the material holds, so sharing copies no texels. An RGBA8
image (a cook with `compress: false`, or an edge under 4 texels) keeps one extra copy at the capped
size per image per batch. A material that draws the neutral fallback (its base color did not load)
gets no shared image. Packed detail and aux maps share only a cooked chain the material already
holds; RGBA8 detail and aux images are packed at upload, as the legacy loader's are. Shared images
bypass automatic per-material texture streaming. Repeated loader calls make new identities;
load models that should share textures in the same call and keep those handles alive.

Use existing agent methods after the host loads the models:

- `agent.guide {"topic":"cooked-model-all-lods"}` reads this contract.
- `assets.status {"asset":"<mesh-or-texture-id>"}` reports successful external loads as loaded. `AssetServer::source_of`
  provides the pak/cache source to the host. A missing texture is not marked loaded.
- `assets.texture_memory` inspects residency after the host submits the selected parts.

This is a host-side loading API, not a new agent command that mutates the scene. No new protocol
error codes are introduced. Agent selection of parts remains the host's responsibility.

Regression commands (lavapipe or a healthy device):

```text
cargo test -p regia_render --test cooked_model_all_lods
cargo test -p regia_render --test cooked_model_all_lods_cap
cargo test -p regia_render --lib cooked_model::tests
cargo test -p regia_agent --lib cooked_model_all_lods
```

The glTF regression cooks `Hull_LOD0` and `Hull_LOD1` with distinct materials, removes its source,
and loads both parts. A separate glTF fixture verifies decreasing meshopt index levels on each
part. A residency test counts resident images and actual GPU upload bytes across two models and
both authored LODs; unusual cooked tangents are compared byte for byte after the new load. A pak
test runs the real cook and pak writer with and without GPU-ready meshes (a colored mesh stays
compact in both) and checks that both paks load the same vertices and index LODs as each other and
as the legacy loader's LOD 0. A cap test loads under a 16-texel resident cap and checks every
shared image's size and that a cooked chain is shared without a copy. The legacy fixture is checked
field by field (positions, coverage, normal and tangent contract, material factors).
