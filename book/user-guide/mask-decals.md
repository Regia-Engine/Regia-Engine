# Mask recolor and decal slots

One character, four factions, a number on every chest: `MaskDecals` recolors and decorates a mesh
instance at draw time from per-instance values, so there are no per-faction texture bakes, no
material clones and no extra draws.

```rust
use std::sync::Arc;
use regia::prelude::*;
use regia::render::{
    DecalBlend, DecalSlot, MaskDecalTextures, MaskDecals, MaskPalette, SharedImage, SharedImageError,
};

/// Once per asset: the mask (linear) and the decal atlas (sRGB), shared by every instance.
fn pilot_textures(mask_rgba: Vec<u8>, atlas_rgba: Vec<u8>) -> Result<Arc<MaskDecalTextures>, SharedImageError> {
    Ok(Arc::new(MaskDecalTextures {
        mask: Some(SharedImage::rgba(1024, 1024, mask_rgba, false)?),
        atlas: Some(SharedImage::rgba(512, 512, atlas_rgba, true)?),
        atlas_grid: [8, 8], // 64 cells: digits, faction marks, callsign letters
    }))
}

/// Per instance: a faction palette, a chest number, a faction mark and a visor tint.
fn dress_pilot(commands: &mut Commands, torso: Entity, textures: &Arc<MaskDecalTextures>) {
    let red = MaskPalette::from_srgb8([200, 16, 46], [40, 40, 44], [230, 200, 40])
        .with_emissive_srgb8([255, 140, 0], 2.0);
    commands.entity(torso).insert(
        MaskDecals::new(textures.clone(), red)
            // "7" on the chest
            .with_decal(DecalSlot::atlas(7, [0.42, 0.30, 0.58, 0.46]))
            // faction mark
            .with_decal(DecalSlot::atlas(52, [0.10, 0.62, 0.22, 0.74]).with_rotation(0.3))
            // visor tint
            .with_decal(DecalSlot::solid(
                [0.70, 0.05, 0.95, 0.20],
                [0.55, 0.42, 0.10, 0.6],
                DecalBlend::Multiply,
            )),
    );
}
```

## Authoring

- **Mask** (linear RGBA, base UV set): R = primary, G = secondary, B = trim weight; A = emissive.
  The base color is multiplied by `(1 − s) + r·primary + g·secondary + b·trim` with `s = r + g + b`
  (normalized when `s > 1`): unmasked texels keep the material color, masked regions take the
  palette over the albedo's own shading. Author masked regions of the albedo in light neutral grey
  (the palette color times that grey is what shows), and keep the detail (wear, grime, AO) in it.
  The mask is sampled with the material's sampler (repeat, trilinear): pad UV islands in the
  mask as you do in the albedo, or a region at the UV border blends with the opposite edge.
- **Emissive** (mask A): adds `a · emissive · emissive_intensity`, in the units of the material's
  `emissive × emissive_intensity`.
- **Atlas**: equal cells, `atlas_grid = [columns, rows]`, cell `i` at column `i % columns`, row
  `i / columns` (row 0 at the top). Color in RGB (sRGB), coverage in A. Leave a gutter of a few
  texels around each cell's artwork: the lower mips blend neighbors (level 0 is inset by half a
  texel).
- **Slots** apply in order after the recolor, at most `MAX_DECAL_SLOTS` (8). `uv_rect` is in the
  base UV set; the cell's top-left corner lands on `(min_u, min_v)` before `rotation`.
  `DecalBlend::Alpha` paints over (and makes the surface a dielectric under the paint),
  `Multiply` tints, `Emissive` adds light × `emissive_intensity`.

The component applies to its own entity only (it is not inherited by children): put it on each
mesh entity whose UV layout the mask and decals were authored for. A model whose parts share one
UV layout gets the same `Arc` and palette on every part, and the decal slots on the part they
belong to.

## What it supports

Standard opaque, alpha-tested and alpha-blended materials, rigid or skinned, single- or
double-sided, every quality preset. A material graph, a transmissive material (glass) and the
premultiplied and additive blend modes draw without it (`MaskDecalStats::unsupported_draws`).

## Cost

- **Memory, per instance:** one 464-byte uniform block and one bind group, created the first frame
  the instance draws and released the frame it leaves the list. Mask and atlas are uploaded once
  per image, whatever the number of instances (keyed by image identity).
- **CPU, per frame:** the block is packed and compared; it is uploaded only when the palette or
  decals changed. Steady frames create no GPU object (`tests/mask_decals.rs` checks it).
- **GPU, per pixel of a masked instance:** one mask sample, plus one atlas sample for each decal
  slot whose rectangle covers the pixel (the others are a rotation and a bounds test).
- **Draw path:** a masked rigid instance leaves the GPU-driven instanced colour batches and draws
  on its own (as a skinned mesh does); depth prepass, shadows and motion vectors are unchanged. It
  also stays out of the visibility buffer.
- **Pipelines:** two `MASK_DECALS` variants (opaque, blend) per target configuration, built by the
  warm when its frame has masked instances, or in the background once the first one appears;
  until then the instance draws with its plain material for a few frames. Nothing compiles on the
  frame path.

Not measured yet: the GPU cost on a real device. The Tincal acceptance asks for the per-instance
cost on Android; the numbers above are by construction, the mobile measurement (a pilot with 4
slots at 1080p on an Adreno or Mali phone) is still to run.

## Agent protocol

`render.mask_decals` and `render.set_mask_decals` read and edit instances and report the counters;
see [the agent page](../agents/mask-decals.md).

## Hosts that render `ExtractedFrame` themselves

Call `renderer.set_mask_decals(&extracted.mask_decals)` every frame next to `set_glass` (an empty
slice clears). A host building `FrameMesh` lists builds `MaskDecalInstance { object_key, decals }`
with the same `object_key` it gives the draw.
