# Skinning: compute path, dual quaternions, eight influences

<!-- index: Skinning -->
<!-- index: Dual quaternion -->
<!-- index: Eight influences -->

By default Regia Engine skins a mesh in each pass's vertex shader (`specs/13` § 8): four joint
influences per vertex, linear blending. Every pass that draws a character skins it again: the
sun shadow cascades, the depth prepass, the motion-vector pass and the colour pass.

The **compute path** skins each instance once per frame in a compute pass, into a vertex buffer
of its own. Every pass then draws those vertices like a rigid mesh, and the motion-vector pass
reads last frame's result. A pose that holds for two frames is not skinned again. The compute
path adds two things the vertex-shader path lacks:

- **Eight influences per vertex.** These come from the cooked skin's second influence set
  (`CpuSkin::joints1`, `weights1`; cooked format v2). Use them for hero characters and the
  character creator.
- **Dual quaternion skinning** (Kavan et al. 2008), chosen per entity. Linear blending
  collapses a joint that twists (the "candy wrapper": a forearm turned 160° pinches to a sixth
  of its width). Dual quaternions keep the volume. Joint scale and shear are applied first, so
  scaled skeletons still work.

The quality tier does not switch this path. VeryLow uses Low's content budgets and still skins
in the vertex shader until `SkinningConfig.compute` is set. The surface the skeleton draws is a
material: see [Materials](../how-to/picture/materials.md).

## Turning it on

```rust,ignore
use regia_render::{SkinInfluences8, SkinningConfig, SkinningOptions};

// Every skinned mesh on the compute path:
app.insert_resource(SkinningConfig { compute: true });

// One character with dual quaternions (its skinned children inherit the method):
commands.entity(character_root).insert(SkinningOptions::dual_quaternion());

// Eight influences on one mesh entity (never inherited; one entry per vertex):
if let Some(skin) = &cooked.skin {
    let influences8 = SkinInfluences8::from_cpu_skin(skin);
    commands.entity(body_mesh).insert(SkinningOptions { influences8, ..Default::default() });
}
```

### Opting a mesh into eight influences from its asset

The default is four influences, and a cooked skin that stores eight still draws its four
strongest. To make an asset draw all eight, put the importer setting in its `.meta`:

```ron
settings: Some("(skin_influences: 8)"),
```

The cook keeps the same influence streams and writes `skin_influences` 8 into the cooked mesh
(cooked mesh v3, ADR-1622). A skin without `JOINTS_1`/`WEIGHTS_1` stays at four, with a warning.
At runtime, ask the cooked skin for its options; a default mesh returns `None`, so it draws
exactly as before:

```rust,wrap
# use regia::prelude::*;
# let mut commands: Commands = todo!();
# let world: &mut World = todo!();
# let app: &mut App = todo!();
# let cooked: regia_asset::CpuMesh = todo!();
# let body_mesh: Entity = todo!();
# use regia_render::SkinningOptions;
if let Some(options) = cooked.skin.as_ref().and_then(SkinningOptions::for_cooked_skin) {
    commands.entity(body_mesh).insert(options);
}
```

`regia upgrade-project` rewrites v1 and v2 cooked meshes at v3 with four influences; the
loader also reads them in place.

A host that drives a `FrameRenderer` itself calls `set_compute_skinning(true)` and
`set_skinning_options(...)` every frame. The keys are object keys, as for
`set_render_overrides`. With `RenderPlugin`, `ExtractedSkinning` does this for the runtime
renderer.

## What changes and what does not

- Without `SkinningConfig` or `SkinningOptions`, nothing changes. Options that ask for nothing
  (linear, four influences, `compute: false`) are dropped.
- On the compute path with linear blending and four influences, frames match the vertex-shader
  path. The test suite checks this on the Fox and on a 67-joint skin: a few edge pixels may
  differ by rounding.
- On a device without compute shaders, or with fewer than four storage buffers per compute
  stage, every mesh stays on the vertex-shader path: linear, four influences.
  `FrameRenderer::skinning_stats().unsupported` and the agent's `render.skinning` say why.
- CPU-side consumers still skin with four influences and linear blending. These are picking
  (`editor.pick_hit`), `assets.skin_check`, and the editor's glTF hierarchy spawn.

## Cost

Each compute instance keeps one storage copy of its mesh's vertices (shared by every instance
of that mesh) and two vertex buffers of its own (this frame and last frame): 96 bytes per
vertex each. The palette is uploaded each frame it changes. A dual quaternion palette is
converted on the CPU, a few microseconds per hundred joints. An instance that is not drawn for
240 frames releases its buffers.

The agent page is [Skinning](../agents/skinning.md).
