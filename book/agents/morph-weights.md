# Morph weights: blend shapes, expression layers, GPU counters

Read and set the morph (blend shape) weights of a mesh while the game runs, then check the result
with a screenshot. Weights are per target, by name (`extras.targetNames`) or by index. The renderer
deforms the mesh on the next frame.

## List morphed entities

`anim.morph_weights` without an entity lists every entity with morph weights, targets or layers,
and the renderer's morph counters for its last frame:

```json
{}
→ { "count": 1,
    "instances": [ { "entity": 4294967310, "names": ["aa", "oh", "blink"], "weights": [0, 0.4, 0],
                     "animated": true, "layers": null,
                     "targets": { "count": 3, "vertex_count": 5120, "moved_vertices": 812,
                                  "gpu_bytes": 98112, "mode": "game" } } ],
    "renderer": { "instances": 1, "drawn": 1, "dispatched": 0, "dispatched_vertices": 0,
                  "mismatched": 0, "waiting_pipeline": 0, "gpu_bytes": 101860,
                  "buffers_created": 3, "motion_instances": 0, "counted": 0 } }
```

- `dispatched` counts the instances deformed last frame (their weights changed); steady weights
  dispatch nothing.
- `mismatched` counts draws whose mesh does not have the targets' vertex count (another LOD): they
  draw undeformed.
- `waiting_pipeline` is non-zero only while the morph pass compiles in the background (the mesh
  draws undeformed meanwhile).
- `buffers_created` grows once per mesh and per instance (two more for a rigid instance: its
  motion copies), never per frame and never on a weight change.
- `motion_instances` counts the instances drawn with motion vectors last frame (ADR-2652): on a
  frame whose weights change, the velocity pass draws the instance's last and current deformation,
  so TAA and TSR reproject a morphing face from where it was. Steady weights write no motion and do
  no work. A skinned morph writes its skinning motion only, and a material graph with a vertex stage
  its own.
- `counted` counts the instances drawn last frame from a counted mesh's upload (ADR-7721): a
  `MeshRenderer::counted` with the `MorphTargets` its upload packed
  (`AssetMeshUploads::morph_targets`). Its deformed copy is a GPU copy of the adopted vertex
  buffer, so the asset keeps no CPU vertices.

`{ "entity": id }` returns the same row for one entity.

## Set weights

```json
{ "entity": 4294967310, "weights": { "blink": 1.0, "0": 0.3 } }
→ { "path": "layer", "before": { ... }, "after": { ... }, "note": "layers compose into weights at the next frame's animation apply" }
```

- On a plain mesh entity, `weights` are written into `MorphWeights` (`path: "weights"`).
- On an animated rig, or one with `MorphLayers`, they go into an override layer named `"agent"`
  (`path: "layer"`), so the animation does not overwrite them. Remove it with
  `{ "entity": id, "remove_layer": true }`.
- `weights` is an object (name or index → weight) or an array in target order. An unknown name is
  an `INVALID_PARAMS` error that lists the target names. Indices run 0..4096 (weights are stored
  densely, so an index sizes the weight list); a larger index or a longer array is `INVALID_PARAMS`.

## Expression layers

```json
{ "entity": 4294967310, "layer": "smile", "mode": "additive", "layer_weight": 0.5,
  "weight_param": "Smile", "weights": { "mouthSmile": 1.0 } }
{ "entity": 4294967310, "normalized_sets": [ { "name": "visemes", "targets": ["aa", "oh", "ee"] } ] }
{ "entity": 4294967310, "clamp": [0, 1] }
```

Layers apply in order over the animator's weights: `override` moves each weight toward the value by
the layer weight, `additive` adds `value × weight`. `weight_param` multiplies the layer weight by a
float graph parameter (set it with `anim.set_param`). Every weight is then clamped (`clamp`, default
`[0, 1]`; `null` turns it off), and each normalized set that sums past 1 is scaled to 1.

## Verify

1. `anim.set_morph_weights` on the face mesh.
2. `play.step` one frame (or wait a frame).
3. `seeing.screenshot` and compare; `anim.morph_weights` shows `dispatched: 1` for that frame.

See [Morph targets and skinned meshes](../user-guide/morph-targets.md).

## Per-node tracks (several morphing meshes in one file)

A glTF `weights` animation channel targets a node, and that node's mesh owns the morph targets. Regia Engine
keeps the node on every imported track, so a file with two morphing meshes animated by one clip gives each
mesh its own weights.

- `anim.morph_weights { entity }` on a mesh spawned from a glTF reports `source_node` (`{ index, name }`, the
  node in the source file) and `node_driver`: what plays the per-node tracks, with its clip name and tracks.
  `node_driver.kind` is `player` (a `NodeMorphPlayer`, for models without a skeleton; it also reports `time` and
  `playing`) or `animator` (the rig's `Animator` on the entity or an ancestor).
- `anim.get_state` reports the single clip's `morph_tracks`: `[{ node: { index, name } | null, target, keys }]`.
  `node: null` is an old-style track that applies to every morph mesh of the rig.
- A mesh whose weights a per-node driver rewrites every frame takes `anim.set_morph_weights` as an override layer
  (`path: "layer"`), like an animated rig.
- Expression layers (`MorphLayers`) on a node's mesh compose over the clip's weights for that node.

Import rules (`regia_asset_pipeline::gltf_anim`): the channel's output count must be keys x the node mesh's morph
target count (x3 for cubic spline). A mismatch fails the import of that file with the channel error
`animation <name> channel <n>: morph weight output count <N> does not match <K> keys x <T> morph targets`
(cubic spline adds ` x 3 (cubic spline)`). A `weights` channel on a node without a mesh or morph targets is an
import warning (`GltfAnimationImport::warnings`) and the channel is skipped. Cooked per-node clips are the
sub-assets `<file>#nodeclip/<animation>`; clips cooked before ADR-297 load with every track unbound, so they play on
every morph mesh exactly as before (`regia upgrade-project` rewrites them at clip format v2).

## Example calls

Example calls (`regia agent call <method> '<params json>'`; entity ids come from `scene.query_entities`):

```text
anim.morph_weights {}                                               # list morphed entities
anim.set_morph_weights {"entity": 4294967310, "weights": {"blink": 1.0}}
```
