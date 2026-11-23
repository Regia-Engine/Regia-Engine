# Draw counters and skinned instancing

A crowd of skinned units (soldiers, mechs, walkers) used to cost one colour draw per unit: skinned
meshes stayed out of the GPU-driven batches that rigid meshes use. Since v0.2.0 (ADR-1368) a
vertex-shader-skinned mesh with a joint-palette bound ([draw integrity](draw-integrity.md)) joins
those batches. Every unit of one mesh and material is one instanced indirect draw: each unit finds
its joints in the frame's shared palette through its own offset (`joint_base`), and the GPU cull
tests the unit's palette bound. The depth prepass and the shadow maps already instanced skinned
units this way.

What stays one draw per unit:

- compute-skinned units ([skinning](skinning.md)): each has its own skinned vertex buffer;
- masked instances ([mask decals](mask-decals.md)): each binds its own mask;
- skinned meshes without a bound (bounds switched off, object key 0, weights the bound cannot
  cover);
- every skinned mesh on a device without indirect first-instance draws (Mobile and Android GPUs
  without it) or a frame with `gpu_driven` off.

The picture is the same either way: the switch only changes how many draw calls the colour pass
makes.

## Read

`render.stats` (no parameters) returns the last main-view frame's draw counters:

```json
{}
→ { "draws": { "drawn": 214, "culled": 37, "instanced": 205, "per_object": 9,
               "double_sided_per_object": 0, "graph_instanced": 0, "graph_per_object": 2,
               "rigid_skin_folded": 0, "zero_key_moving_meshes": 0 },
    "skinned": { "instancing": true, "bounds": true, "draws": 170, "instanced": 170,
                 "per_object": 0, "batches": 2, "batches_possible": 2 },
    "mesh_cache": { "meshes": 41, "mesh_bytes": 18350080, "mesh_budget_bytes": 268435456,
                    "evicted": 0, "adopted_meshes": 3, "adopted_mesh_bytes": 393216,
                    "adopted_skinned_meshes": 1 },
    "counted": { "texture_rebinds": 2, "lod_index_uploads": 4,
                 "vis": { "meshes": 2, "clusters": 38, "repacks": 2, "forward_fallbacks": 0,
                          "kept_meshlet_bytes": 9728 },
                 "textures": { "resident": 4, "resident_bytes": 5592404, "sibling_views": 4,
                               "uploads": 6, "frees": 2 } } }
```

- `mesh_cache` (ADR-7330): the mesh cache as of its last eviction sweep. `meshes`, `mesh_bytes` and
  `mesh_budget_bytes` are its own entries against the mesh budget, `evicted` the evictions so far.
  `adopted_meshes` / `adopted_mesh_bytes` are counted meshes drawn from the asset server's uploads
  ([Assets](assets.md)): not in `mesh_bytes`, never evicted. `adopted_skinned_meshes` (ADR-8610)
  counts those uploaded with a skin: a skinned `MeshRenderer` on one skins on the vertex-shader path
  from the upload (its joints stay CPU), with no CPU vertices anywhere. On the compute path
  ([skinning](skinning.md)) its twin skins from the adopted vertex buffer (ADR-7721; counted in
  `render.skinning` `last_frame.counted`), and a morphed one deforms a GPU copy of it.

- `counted` (ADR-8610): `texture_rebinds` counts the material entries the renderer rebuilt because
  a counted texture they bind was uploaded again (a hot reload) while the material kept its key;
  the frame after the reload draws the new texture. `textures` is the App's counted texture table
  (`CountedMeshPlugin`; `null` without it): `resident` / `resident_bytes` on the GPU, `sibling_views`
  of them with a view in each color space (a base-color slot and a mask slot sample one texture
  each in its own space), `uploads` (a reload uploads again) and `frees` so far.
  `lod_index_uploads` (ADR-7428) counts the index lists uploaded for counted meshes' `MeshLodChain`
  levels: one per level a chain drew (LOD 0 draws the upload's own indices), shared by its colour,
  shadow and cross-fade draws; it grows again only after an idle entry was evicted or the mesh reloaded.
  `vis` (ADR-7462) covers counted meshes on the visibility buffer's cluster path. `meshes` and
  `clusters` are resident from the upload, `repacks` counts the GPU vertex copies (one per admitted
  mesh) and `forward_fallbacks` the counted whole draws of the last visibility-buffer frame that drew
  forward for want of clusters (no cooked meshlets, counts that do not match, cluster residency on).
  `kept_meshlet_bytes` (ADR-7542) is the CPU copy of the admitted meshes' cooked clusters the renderer
  keeps for a hot reload (shared with the meshlet bridge's loaded `.fmlt` when that is the same one).

- `draws.instanced` / `draws.per_object`: opaque and masked colour draws in GPU-driven batches
  and one by one.
- `skinned.draws`: visible skinned meshes in the colour pass (compute-skinned included; a one-joint
  skin folded into a rigid draw is not skinned here).
- `skinned.instanced`: of those, drawn in batches; `skinned.per_object` the rest.
- `skinned.batches`: GPU batches holding skinned units this frame, one indirect draw each.
- `skinned.batches_possible`: distinct (cull mode, mesh, material, pipeline) groups among the
  skinned draws, the fewest draws they could take. When `per_object` is large and
  `batches_possible` is small, instancing would save draws: check `instancing`, `bounds`, and
  whether the units are on the compute path or carry masks.

Telemetry frame summaries carry `counts.skinned_draws` and `counts.skinned_batches_possible`
(`{mean, max}` over the window, [telemetry](../user-guide/telemetry.md)).

## Switch skinned instancing

`render.set_skinned_instancing { enabled }` turns the batching off (every skinned mesh draws one
by one in the colour pass, the behavior before v0.2.0) or back on. Use it to compare CPU encode
time with and without, or to rule the batching out when a skinned unit looks wrong:

```json
{ "enabled": false }
→ { "before": { …, "skinned": { "instancing": true, … } },
    "after": { …, "skinned": { "instancing": false, … } } }
```

`after` shows the switch; its counters change once the next frame has run. Hosts call
`FrameRenderer::set_skinned_instancing` directly.

## Check it

On a crowd scene, `render.stats` should show `skinned.instanced` equal to `skinned.draws` and
`skinned.batches` equal to `skinned.batches_possible` (unless some units are compute-skinned or
masked). Pixels match with the switch on and off; a difference is a bug worth reporting (the
mesh, its material and the camera).

The measurement harness: `CHASE_UNITS=170 CHASE_SKINNED=1 do27_chase` skins the chase's units with
two joints and gives every copy one mesh key; `CHASE_SKINNED_INSTANCING=0` draws them one by one.
It prints the units' counters at the end of warm-up and the usual `cpu render_into` and
`cpu encode` rows ([performance and benches](../user-guide/perf-benches.md)).
