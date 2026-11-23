# Morph targets and skinned meshes

Faces, visemes, correctives and a character creator's body sliders are **morph targets** (blend
shapes): per-vertex offsets the renderer adds to a mesh, scaled by a weight per target. Regia Engine
applies them on the GPU, per instance, under skinning, and drives the weights from animation clips,
anim graphs, expression layers, game code and the agent protocol.

## Drawing a morphed mesh

A cooked mesh (cooked mesh format v2 and later; v3 is current) carries its sparse targets. Pack them once per mesh and put
them, with the weights, on the entity that draws the mesh:

```rust,wrap
use std::sync::Arc;
use regia_render::{CpuMaterial, CpuMesh, MeshMorphTargets, MeshRenderer, MorphTargets};
use regia_scene::components::MorphWeights;
# use regia_ecs::prelude::World;
# let bytes: &[u8] = &[];
# let material = Arc::new(CpuMaterial::default());
# let mut world = World::new();

let cooked = regia_asset::load_mesh_bytes(&bytes)?.mesh;          // regia_asset::CpuMesh
let mesh = Arc::new(CpuMesh::from_cooked(&cooked)?);
let targets = MorphTargets::from_cooked(&mesh, &cooked)?;           // None without targets
let mut entity = world.spawn(MeshRenderer::new(mesh, material));
if let Some(targets) = targets {
    let count = targets.len();
    entity.insert((MeshMorphTargets::new(targets), MorphWeights(vec![0.0; count])));
}
```

- `MeshMorphTargets` holds the packed targets (shared by every instance of the mesh through the
  `Arc`) and the **mode**: `MorphMode::Game` (the default) applies the 16 largest weights,
  `MorphMode::Creator` applies every weight (hundreds of sliders).
- `MorphWeights` (one weight per target, target order) is the instance's weights. Without it the
  mesh draws undeformed.
- The render plugin gives the entity a `MorphTargetNames` (the target names) on its own, so weights
  can be set by name.
- `spawn_gltf_hierarchy` (editor and tools) does all of this for glTF meshes with targets, with the
  file's default weights and `extras.targetNames`.

How it works: when an instance's weights change, a compute pass writes its deformed vertices into
the instance's own copy of the mesh, and every pass (shadows, depth, colour, motion vectors,
instancing) draws that copy. Instances whose weights did not change cost nothing; weight changes
create no GPU buffers. Skinning deforms the morphed vertices, so faces animate on skinned
characters. `RenderOverride` (tint, ghost, x-ray) and mask recolor work on morphed instances as on
any other.

Limits:

- A morphed instance does not batch with other instances of the same mesh (its vertices are its
  own). A crowd with identical expressions is cheaper with the weights left at zero.
- A rigid morphed instance writes motion vectors on frames its weights change (ADR-2652), so TAA
  reprojects it. A skinned morph writes its skinning motion only, so a fast expression change on a
  skinned face can ghost slightly under TAA.
- A mesh whose vertex count differs from the targets' (another LOD), and a shadow-only LOD draw,
  draw undeformed.
- Mid-play, the first frame that needs the morph pass may draw undeformed while the pass compiles
  in the background. A warmed level (`warm_extracted` with the morphed instances in the frame) has
  it ready.

## Weights from animation

Clips carry morph weight tracks (`MorphTrack`, from glTF `weights` channels). An `Animator` on a rig
with a `SkinnedMesh` blends them like joint tracks and writes `MorphWeights` every frame.

On top of the animator, `MorphLayers` composes expression layers, in order:

```rust,wrap
use regia_animation::prelude::*;
# use regia_ecs::prelude::World;
# let mut world = World::new();
# let rig = world.spawn_empty().id();

let layers = MorphLayers {
    layers: vec![
        // Visemes from the dialogue system replace the clip's mouth shapes.
        MorphLayer::new("visemes", MorphLayerMode::Override).with("aa", 0.7),
        // A smile on top of whatever plays, as strong as the graph's "Smile" parameter.
        MorphLayer {
            weight_param: Some("Smile".into()),
            ..MorphLayer::new("smile", MorphLayerMode::Additive).with("mouthSmile", 1.0)
        },
    ],
    // Every weight is clamped to 0..=1 (the default), then each set is normalized: one mouth
    // shape at a time.
    normalized_sets: vec![MorphSet {
        name: "visemes".into(),
        targets: vec!["aa".into(), "oh".into(), "ee".into()],
    }],
    ..MorphLayers::default()
};
world.entity_mut(rig).insert(layers);
```

- **Override** moves a weight toward the layer's value by the layer weight; **Additive** adds
  `value × weight`.
- `weight_param` multiplies the layer weight by a float graph parameter (`animator.set("Smile", 0.5)`).
- Targets are named by index or by name (resolved through `MorphTargetNames`).
- With `MorphLayers`, `MorphWeights` is an output, rewritten every frame. Without an animator the
  layers compose over zero.
- Without either, `MorphWeights` is yours: set it from game code
  (`weights.set_named(&names, "blink", 1.0)`).

## Skinned meshes

`SkinnedMesh { skeleton, joints }` binds a mesh entity to a `Skeleton` and its joint entities (it
replaces `SkeletonBinding`, which is now a deprecated alias of the same type). Add
`regia_framework::skinning::SkinnedMeshPlugin` and the renderer gets the joint palette without
any glue:

- with an `Animator`, the palette the animation publishes (`SkinPalette`);
- without one, the palette of the joint entities' world transforms, so joints posed by a glTF
  hierarchy, a pose sampler or physics deform the mesh.

The plugin inserts `SkinJoints` when it is missing. Hosts that fill `SkinJoints` themselves (an
external simulation's poses) leave the plugin out.

## Agents

`anim.morph_weights` and `anim.set_morph_weights` read and set weights by name or index
([Morph weights](../agents/morph-weights.md)).
