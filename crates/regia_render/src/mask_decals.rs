// SPDX-License-Identifier: MPL-2.0
//! Material mask recolor and UV decal slots (Tincal engine item B7).
//!
//! One material and one set of textures, many looks: a [`MaskDecals`] component on a mesh entity
//! recolors that instance through a **mask texture** and paints up to [`MAX_DECAL_SLOTS`]
//! **decals** from an atlas, with no per-look texture bake and no material clone. DO27 uses it
//! for faction colors, chest numbers, callsign decals, faction marks and visor tint.
//!
//! - **Mask** (linear RGBA, sampled with the base UV set): R, G and B are the weights of the
//!   [`MaskPalette::primary`], [`MaskPalette::secondary`] and [`MaskPalette::trim`] colors. The
//!   base color is multiplied by `(1 − s) + r·primary + g·secondary + b·trim`, where `s = r + g + b`
//!   (weights are normalized when `s > 1`), so an unmasked texel keeps the material's color and
//!   the masked regions take the palette over the albedo's own shading (author masked regions in
//!   light grey). A adds `a · emissive · emissive_intensity` to the emission, in the units of
//!   [`crate::LabMaterial::emissive`] × [`crate::LabMaterial::emissive_intensity`].
//! - **Decals**: each [`DecalSlot`] places one atlas cell (or a solid rectangle) in a rectangle of
//!   the base UV set, rotated about its center, colored, and blended by [`DecalBlend`]. Slots
//!   apply in order, after the recolor.
//!
//! Everything here is per instance: the [`MaskDecalTextures`] behind the `Arc` are uploaded once
//! for every instance that shares them (keyed by image identity, like
//! [`crate::SharedImage`] material slots), and each instance owns one small uniform block
//! ([`BLOCK_BYTES`]) and one bind group, created when the instance first draws and rewritten only
//! when its palette or decals change. Steady frames create no GPU objects.
//!
//! Instances draw through the `MASK_DECALS` variant of the forward shader (its opaque and alpha
//! blend pipelines, [`crate::pipeline_warm::DeferredPipeline`]s like the other forward variants):
//! a warm frame builds them, mid-play the draw keeps the standard surface until a warm worker has.
//! Supported: standard (non-graph) opaque, masked and alpha-blended materials. A material graph,
//! a transmissive material, and the premultiplied and additive blend modes draw without the
//! feature ([`MaskDecalStats::unsupported_draws`]). Rigid instances leave the GPU-driven instanced
//! lists for the colour pass (one draw each, like a skinned mesh); depth and shadows are unchanged.
//!
//! Extraction: [`extract_mask_decals`] (a `Render` system after [`crate::extract_frame`]) fills
//! [`crate::ExtractedFrame::mask_decals`]; the runtime passes it to
//! [`crate::FrameRenderer::set_mask_decals`]. A host that renders an `ExtractedFrame` itself makes
//! that call every frame (an empty slice clears every instance). The component applies to its own
//! entity only: put it on each mesh entity whose UV layout the mask and decals were authored for.

use std::collections::HashMap;
use std::sync::Arc;

use regia_ecs::prelude::*;

use crate::SharedImage;
use crate::runtime::{ExtractedFrame, ExtractedMesh};

/// Most decal slots one instance draws; slots past this are ignored.
pub const MAX_DECAL_SLOTS: usize = 8;

/// Bytes of one instance's uniform block on the GPU (`MaskDecalBlock` in `lab_forward.wgsl`).
pub const BLOCK_BYTES: usize = std::mem::size_of::<GpuMaskBlock>();

/// Header flag: the mask texture is bound.
pub(crate) const FLAG_MASK: u32 = 1;
/// Header flag: the decal atlas is bound.
pub(crate) const FLAG_ATLAS: u32 = 2;

/// The textures a masked instance reads. Share one `Arc` between every instance of an asset: the
/// renderer uploads each image once and keys it by identity, never by content.
#[derive(Clone, Debug, Default)]
pub struct MaskDecalTextures {
    /// Recolor mask, linear data (create it with `srgb = false`): R primary, G secondary, B trim,
    /// A emissive, sampled with the base UV set and the material's sampler. `None` skips the
    /// recolor (decals only).
    pub mask: Option<SharedImage>,
    /// Decal atlas, sRGB color with coverage in alpha (`srgb = true`), split into
    /// [`Self::atlas_grid`] equal cells. `None` draws every slot as a solid rectangle. Leave a
    /// gutter of a few texels around each cell's artwork: the lower mips blend neighbors.
    pub atlas: Option<SharedImage>,
    /// Atlas cells per row and per column. Cell `i` is column `i % columns`, row `i / columns`,
    /// row 0 at the top of the image. Zero counts as one.
    pub atlas_grid: [u32; 2],
}

/// Per-instance recolor palette: linear RGB multipliers for the mask's three color channels and
/// the emissive color its alpha drives.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MaskPalette {
    /// Multiplier where the mask's R is 1 (linear RGB).
    pub primary: [f32; 3],
    /// Multiplier where the mask's G is 1 (linear RGB).
    pub secondary: [f32; 3],
    /// Multiplier where the mask's B is 1 (linear RGB).
    pub trim: [f32; 3],
    /// Emissive color where the mask's A is 1 (linear RGB).
    pub emissive: [f32; 3],
    /// Scale of [`Self::emissive`], in the units of [`crate::LabMaterial::emissive_intensity`].
    pub emissive_intensity: f32,
}

impl MaskPalette {
    /// No change: white multipliers, no emission.
    pub const NEUTRAL: Self = Self {
        primary: [1.0; 3],
        secondary: [1.0; 3],
        trim: [1.0; 3],
        emissive: [0.0; 3],
        emissive_intensity: 0.0,
    };

    /// A palette from sRGB-encoded 8-bit colors (what a faction table or a color picker stores),
    /// decoded to linear. No emission.
    #[must_use]
    pub fn from_srgb8(primary: [u8; 3], secondary: [u8; 3], trim: [u8; 3]) -> Self {
        Self {
            primary: srgb8(primary),
            secondary: srgb8(secondary),
            trim: srgb8(trim),
            ..Self::NEUTRAL
        }
    }

    /// This palette with an emissive color (sRGB-encoded 8-bit, decoded) at `intensity`.
    #[must_use]
    pub fn with_emissive_srgb8(self, emissive: [u8; 3], intensity: f32) -> Self {
        Self {
            emissive: srgb8(emissive),
            emissive_intensity: intensity,
            ..self
        }
    }
}

impl Default for MaskPalette {
    fn default() -> Self {
        Self::NEUTRAL
    }
}

fn srgb8(c: [u8; 3]) -> [f32; 3] {
    c.map(|v| regia_core::math::srgb_to_linear(f32::from(v) / 255.0))
}

/// How a decal combines with the surface under it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum DecalBlend {
    /// Paint over: the base color goes to the decal color by its coverage, and the metallic goes
    /// to 0 (paint is a dielectric). Numbers, marks, callsigns.
    #[default]
    Alpha,
    /// Multiply the base color by the decal color, by its coverage: tints, grime, visor tint.
    Multiply,
    /// Add light: the decal color × coverage × [`DecalSlot::emissive_intensity`] joins the
    /// emission; the base color is unchanged. Glowing marks and readouts.
    Emissive,
}

impl DecalBlend {
    /// Stable name (agent protocol, logs).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Alpha => "alpha",
            Self::Multiply => "multiply",
            Self::Emissive => "emissive",
        }
    }

    /// The mode named `name`.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "alpha" => Some(Self::Alpha),
            "multiply" => Some(Self::Multiply),
            "emissive" => Some(Self::Emissive),
            _ => None,
        }
    }

    fn code(self) -> f32 {
        match self {
            Self::Alpha => 0.0,
            Self::Multiply => 1.0,
            Self::Emissive => 2.0,
        }
    }
}

/// One decal on an instance.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DecalSlot {
    /// Atlas cell ([`MaskDecalTextures::atlas_grid`]); `None` (or no atlas) draws a solid
    /// rectangle of [`Self::color`].
    pub atlas_cell: Option<u32>,
    /// Where the decal goes in the base UV set: `[min_u, min_v, max_u, max_v]` before rotation.
    /// The cell's top-left corner lands on `(min_u, min_v)`.
    pub uv_rect: [f32; 4],
    /// Rotation about the rectangle's center, radians, counter-clockwise in UV space (u right,
    /// v down).
    pub rotation: f32,
    /// Linear RGB multiplier of the atlas color, and opacity in A.
    pub color: [f32; 4],
    /// How it combines with the surface.
    pub blend: DecalBlend,
    /// Emission scale for [`DecalBlend::Emissive`] (units of
    /// [`crate::LabMaterial::emissive_intensity`]); ignored by the other modes.
    pub emissive_intensity: f32,
}

impl DecalSlot {
    /// Atlas cell `cell` painted over `uv_rect`, white, opaque, unrotated.
    #[must_use]
    pub fn atlas(cell: u32, uv_rect: [f32; 4]) -> Self {
        Self {
            atlas_cell: Some(cell),
            uv_rect,
            rotation: 0.0,
            color: [1.0; 4],
            blend: DecalBlend::Alpha,
            emissive_intensity: 0.0,
        }
    }

    /// A solid rectangle of `color` over `uv_rect` (no atlas sample), blended by `blend`.
    #[must_use]
    pub fn solid(uv_rect: [f32; 4], color: [f32; 4], blend: DecalBlend) -> Self {
        Self {
            atlas_cell: None,
            uv_rect,
            rotation: 0.0,
            color,
            blend,
            emissive_intensity: 0.0,
        }
    }

    /// This slot tinted with `color` (linear RGB, opacity in A).
    #[must_use]
    pub fn with_color(self, color: [f32; 4]) -> Self {
        Self { color, ..self }
    }

    /// This slot rotated by `radians` about its center.
    #[must_use]
    pub fn with_rotation(self, radians: f32) -> Self {
        Self {
            rotation: radians,
            ..self
        }
    }

    /// This slot with blend mode `blend`.
    #[must_use]
    pub fn with_blend(self, blend: DecalBlend) -> Self {
        Self { blend, ..self }
    }

    /// Where `uv` falls in the decal's own square (`[0, 1]²`, `(0, 0)` = the cell's top-left),
    /// or `None` outside it. Mirrors the shader.
    #[must_use]
    pub fn local_uv(&self, uv: [f32; 2]) -> Option<[f32; 2]> {
        let (center, half) = rect_center_half(self.uv_rect);
        let size = [(half[0] * 2.0).max(1e-6), (half[1] * 2.0).max(1e-6)];
        let rel = [uv[0] - center[0], uv[1] - center[1]];
        let (s, c) = self.rotation.sin_cos();
        // Into the decal's frame: rotate by −θ about the center.
        let unrot = [c * rel[0] + s * rel[1], -s * rel[0] + c * rel[1]];
        let local = [unrot[0] / size[0] + 0.5, unrot[1] / size[1] + 0.5];
        ((0.0..=1.0).contains(&local[0]) && (0.0..=1.0).contains(&local[1])).then_some(local)
    }
}

/// `(center, half extents)` of a `[min_u, min_v, max_u, max_v]` rectangle (any corner order).
fn rect_center_half(rect: [f32; 4]) -> ([f32; 2], [f32; 2]) {
    let (u0, u1) = (rect[0].min(rect[2]), rect[0].max(rect[2]));
    let (v0, v1) = (rect[1].min(rect[3]), rect[1].max(rect[3]));
    (
        [(u0 + u1) * 0.5, (v0 + v1) * 0.5],
        [(u1 - u0) * 0.5, (v1 - v0) * 0.5],
    )
}

/// Atlas UV of `local` (a [`DecalSlot::local_uv`]) in cell `cell` of a `grid` atlas. Mirrors the
/// shader without its half-texel inset.
#[must_use]
pub fn atlas_uv(cell: u32, grid: [u32; 2], local: [f32; 2]) -> [f32; 2] {
    let columns = grid[0].max(1);
    let rows = grid[1].max(1);
    let (column, row) = (cell % columns, cell / columns);
    [
        (column as f32 + local[0]) / columns as f32,
        (row as f32 + local[1]) / rows as f32,
    ]
}

/// The recolor of one texel: `(base color, added emission)` for linear `base`, a `mask` sample
/// (linear RGBA in 0–1) and `palette`. Mirrors the shader.
#[must_use]
pub fn recolor(base: [f32; 3], mask: [f32; 4], palette: &MaskPalette) -> ([f32; 3], [f32; 3]) {
    let m = mask.map(|v| v.clamp(0.0, 1.0));
    let sum = m[0] + m[1] + m[2];
    let norm = sum.max(1.0);
    let w = [m[0] / norm, m[1] / norm, m[2] / norm];
    let keep = 1.0 - sum.min(1.0);
    let out = std::array::from_fn(|c| {
        base[c] * (keep + w[0] * palette.primary[c] + w[1] * palette.secondary[c] + w[2] * palette.trim[c])
    });
    let glow = std::array::from_fn(|c| palette.emissive[c] * palette.emissive_intensity * m[3]);
    (out, glow)
}

/// One masked instance: the component's values for one draw ([`crate::FrameMesh::object_key`]).
#[derive(Clone, Debug)]
pub struct MaskDecalInstance {
    /// The draw this applies to.
    pub object_key: u64,
    /// Its recolor and decals.
    pub decals: MaskDecals,
}

/// Recolor and decal slots of one mesh instance (see the module docs).
#[derive(Component, Clone, Debug, Default)]
pub struct MaskDecals {
    /// Mask and atlas, shared by every instance of the asset.
    pub textures: Arc<MaskDecalTextures>,
    /// Recolor palette.
    pub palette: MaskPalette,
    /// Decal slots, applied in order; the first [`MAX_DECAL_SLOTS`] draw.
    pub decals: Vec<DecalSlot>,
}

impl MaskDecals {
    /// A recolor with `palette` over `textures`, no decals.
    #[must_use]
    pub fn new(textures: Arc<MaskDecalTextures>, palette: MaskPalette) -> Self {
        Self {
            textures,
            palette,
            decals: Vec::new(),
        }
    }

    /// This instance with `slot` appended.
    #[must_use]
    pub fn with_decal(mut self, slot: DecalSlot) -> Self {
        self.decals.push(slot);
        self
    }

    /// The uniform block the shader reads for this instance.
    pub(crate) fn pack(&self) -> GpuMaskBlock {
        let tex = &self.textures;
        let mut flags = 0;
        if tex.mask.is_some() {
            flags |= FLAG_MASK;
        }
        if tex.atlas.is_some() {
            flags |= FLAG_ATLAS;
        }
        let p = &self.palette;
        let emissive = p.emissive.map(|c| c * p.emissive_intensity.max(0.0));
        let count = self.decals.len().min(MAX_DECAL_SLOTS);
        let mut decals = [[[0.0f32; 4]; 3]; MAX_DECAL_SLOTS];
        for (out, slot) in decals.iter_mut().zip(&self.decals) {
            let (center, half) = rect_center_half(slot.uv_rect);
            let (s, c) = slot.rotation.sin_cos();
            let cell = slot.atlas_cell.map_or(-1.0, |cell| cell as f32);
            let mut color = slot.color;
            if slot.blend == DecalBlend::Emissive {
                for v in &mut color[..3] {
                    *v *= slot.emissive_intensity.max(0.0);
                }
            }
            *out = [
                [center[0], center[1], half[0], half[1]],
                [c, s, cell, slot.blend.code()],
                color,
            ];
        }
        GpuMaskBlock {
            palette: [
                [p.primary[0], p.primary[1], p.primary[2], 1.0],
                [p.secondary[0], p.secondary[1], p.secondary[2], 1.0],
                [p.trim[0], p.trim[1], p.trim[2], 1.0],
                [emissive[0], emissive[1], emissive[2], 0.0],
            ],
            header: [
                count as u32,
                tex.atlas_grid[0].max(1),
                tex.atlas_grid[1].max(1),
                flags,
            ],
            decals,
        }
    }
}

/// `MaskDecalBlock` in `lab_forward.wgsl`: palette (primary, secondary, trim, emissive × intensity),
/// header (decal count, atlas columns, rows, flags), then per decal: rect (center, half extents),
/// params (cos, sin, atlas cell or −1, blend code), color.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct GpuMaskBlock {
    pub palette: [[f32; 4]; 4],
    pub header: [u32; 4],
    pub decals: [[[f32; 4]; 3]; MAX_DECAL_SLOTS],
}

/// What the renderer did with masked instances on its last frame
/// ([`crate::FrameRenderer::mask_decal_stats`], agent method `render.mask_decals`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MaskDecalStats {
    /// Instances set with [`crate::FrameRenderer::set_mask_decals`].
    pub instances: usize,
    /// Draws shaded with the recolor and decals last frame.
    pub masked_draws: usize,
    /// Draws of masked instances that kept the standard surface because their pipeline variant
    /// was still building in the background (mid-play; a warm frame builds it).
    pub fallback_draws: usize,
    /// Draws of masked instances whose material cannot take the feature (graph, transmissive,
    /// premultiplied or additive).
    pub unsupported_draws: usize,
    /// Per-instance GPU entries resident (one uniform block and one bind group each).
    pub gpu_entries: usize,
    /// Uniform blocks created since the renderer started (one per instance, never per frame).
    pub blocks_created: u64,
    /// Block uploads last frame (instances whose palette or decals changed).
    pub block_writes: usize,
}

/// `Render`: fills [`ExtractedFrame::mask_decals`] for the meshes [`crate::extract_frame`] just
/// extracted, one entry per draw of an entity that carries [`MaskDecals`] (a LOD cross-fade pair
/// gets one per copy).
pub fn extract_mask_decals(mut extracted: ResMut<ExtractedFrame>, query: Query<(Entity, &MaskDecals)>) {
    if query.is_empty() {
        if !extracted.mask_decals.is_empty() {
            extracted.mask_decals.clear();
        }
        return;
    }
    let _span = tracing::info_span!("extract_mask_decals").entered();
    let by_entity: HashMap<Entity, &MaskDecals> = query.iter().collect();
    let instances = collect_mask_decals(&extracted.meshes, |entity| {
        by_entity.get(&entity).map(|d| (*d).clone())
    });
    extracted.mask_decals = instances;
}

/// One [`MaskDecalInstance`] for each of `meshes` whose entity `resolve` gives values for, keyed
/// like [`ExtractedFrame::frame_scene`] keys the draw. A host with its own entity mapping (the
/// editor gives a model's part entities their owner's component) uses this instead of
/// [`extract_mask_decals`].
pub fn collect_mask_decals(
    meshes: &[ExtractedMesh],
    resolve: impl Fn(Entity) -> Option<MaskDecals>,
) -> Vec<MaskDecalInstance> {
    meshes
        .iter()
        .filter_map(|mesh| {
            resolve(mesh.entity).map(|decals| MaskDecalInstance {
                object_key: mesh.entity.to_bits() ^ mesh.object_key_xor,
                decals,
            })
        })
        .collect()
}

/// Rust and WGSL struct rows of this module (`shader_contract`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<crate::ContractRow> {
    vec![crate::ContractRow::new(
        "lab_forward.wgsl",
        "MaskDecalBlock",
        size_of::<GpuMaskBlock>(),
        vec![
            ("primary", std::mem::offset_of!(GpuMaskBlock, palette)),
            ("secondary", std::mem::offset_of!(GpuMaskBlock, palette) + 16),
            ("trim", std::mem::offset_of!(GpuMaskBlock, palette) + 32),
            ("emissive", std::mem::offset_of!(GpuMaskBlock, palette) + 48),
            ("header", std::mem::offset_of!(GpuMaskBlock, header)),
            ("decals", std::mem::offset_of!(GpuMaskBlock, decals)),
        ],
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: [f32; 3], b: [f32; 3]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-5)
    }

    #[test]
    fn neutral_palette_and_empty_mask_change_nothing() {
        let base = [0.4, 0.5, 0.6];
        let (out, glow) = recolor(base, [1.0, 0.0, 0.0, 1.0], &MaskPalette::NEUTRAL);
        assert!(close(out, base));
        assert!(close(glow, [0.0; 3]));
        let red = MaskPalette {
            primary: [1.0, 0.0, 0.0],
            ..MaskPalette::NEUTRAL
        };
        let (out, _) = recolor(base, [0.0; 4], &red);
        assert!(close(out, base), "an unmasked texel keeps the material color");
    }

    #[test]
    fn mask_channels_pick_palette_entries_and_overlaps_normalize() {
        let palette = MaskPalette {
            primary: [1.0, 0.0, 0.0],
            secondary: [0.0, 1.0, 0.0],
            trim: [0.0, 0.0, 1.0],
            emissive: [1.0, 0.5, 0.0],
            emissive_intensity: 4.0,
        };
        let white = [1.0; 3];
        assert!(close(
            recolor(white, [1.0, 0.0, 0.0, 0.0], &palette).0,
            [1.0, 0.0, 0.0]
        ));
        assert!(close(
            recolor(white, [0.0, 1.0, 0.0, 0.0], &palette).0,
            [0.0, 1.0, 0.0]
        ));
        assert!(close(
            recolor(white, [0.0, 0.0, 1.0, 0.0], &palette).0,
            [0.0, 0.0, 1.0]
        ));
        // Half primary: halfway between the material and the palette.
        assert!(close(
            recolor(white, [0.5, 0.0, 0.0, 0.0], &palette).0,
            [1.0, 0.5, 0.5]
        ));
        // R + G = 2: normalized to half each, nothing of the material left.
        assert!(close(
            recolor(white, [1.0, 1.0, 0.0, 0.0], &palette).0,
            [0.5, 0.5, 0.0]
        ));
        let (_, glow) = recolor(white, [0.0, 0.0, 0.0, 0.5], &palette);
        assert!(close(glow, [2.0, 1.0, 0.0]));
    }

    #[test]
    fn srgb_palette_decodes_to_linear() {
        let p = MaskPalette::from_srgb8([255, 128, 0], [0, 0, 0], [255, 255, 255]);
        assert!((p.primary[0] - 1.0).abs() < 1e-6);
        assert!((p.primary[1] - 0.2158605).abs() < 1e-4);
        assert_eq!(p.secondary, [0.0; 3]);
        assert_eq!(p.emissive_intensity, 0.0);
        let glowing = p.with_emissive_srgb8([255, 255, 255], 3.0);
        assert_eq!(glowing.emissive, [1.0; 3]);
        assert_eq!(glowing.emissive_intensity, 3.0);
    }

    #[test]
    fn decal_local_uv_follows_rect_and_rotation() {
        let slot = DecalSlot::atlas(0, [0.2, 0.4, 0.4, 0.8]);
        let corner = slot.local_uv([0.2001, 0.4001]).expect("top-left corner inside");
        assert!(corner[0] < 1e-3 && corner[1] < 1e-3, "{corner:?}");
        let center = slot.local_uv([0.3, 0.6]).expect("center inside");
        assert!((center[0] - 0.5).abs() < 1e-6 && (center[1] - 0.5).abs() < 1e-6);
        assert_eq!(slot.local_uv([0.1, 0.6]), None);
        // A quarter turn swaps the axes: a point right of the center lands below it in the
        // decal (u right, v down).
        let turned = slot.with_rotation(std::f32::consts::FRAC_PI_2);
        let p = turned.local_uv([0.35, 0.6]).expect("inside after rotation");
        assert!((p[0] - 0.5).abs() < 1e-5, "{p:?}");
        assert!((p[1] - 0.375).abs() < 1e-5, "{p:?}");
    }

    #[test]
    fn atlas_cells_index_rows_from_the_top() {
        assert_eq!(atlas_uv(0, [4, 4], [0.0, 0.0]), [0.0, 0.0]);
        assert_eq!(atlas_uv(5, [4, 4], [0.5, 0.5]), [0.375, 0.375]);
        assert_eq!(
            atlas_uv(3, [0, 0], [1.0, 1.0]),
            [1.0, 4.0],
            "a zero grid counts as one column and one row: cell 3 is row 3"
        );
    }

    #[test]
    fn pack_writes_palette_header_and_slots() {
        let textures = Arc::new(MaskDecalTextures {
            mask: Some(SharedImage::rgba(1, 1, vec![255, 0, 0, 0], false).expect("mask")),
            atlas: None,
            atlas_grid: [4, 2],
        });
        let palette = MaskPalette {
            emissive: [1.0, 0.5, 0.25],
            emissive_intensity: 2.0,
            ..MaskPalette::NEUTRAL
        };
        let mut decals = MaskDecals::new(textures, palette)
            .with_decal(DecalSlot::atlas(3, [0.6, 0.2, 0.2, 0.4]).with_rotation(std::f32::consts::PI))
            .with_decal(DecalSlot::solid(
                [0.0, 0.0, 1.0, 1.0],
                [0.5, 0.5, 0.5, 1.0],
                DecalBlend::Emissive,
            ));
        decals.decals[1].emissive_intensity = 3.0;
        let block = decals.pack();
        assert_eq!(block.palette[3], [2.0, 1.0, 0.5, 0.0]);
        assert_eq!(block.header, [2, 4, 2, FLAG_MASK]);
        // Corners in any order give the same rect.
        let rect = block.decals[0][0];
        assert!(
            rect.iter()
                .zip([0.4, 0.3, 0.2, 0.1])
                .all(|(a, b)| (a - b).abs() < 1e-6),
            "{rect:?}"
        );
        assert!((block.decals[0][1][0] + 1.0).abs() < 1e-6, "cos π");
        assert_eq!(block.decals[0][1][2], 3.0);
        assert_eq!(block.decals[1][1][2], -1.0, "solid slot");
        assert_eq!(block.decals[1][1][3], 2.0, "emissive code");
        assert_eq!(
            block.decals[1][2],
            [1.5, 1.5, 1.5, 1.0],
            "intensity folded into the color"
        );
        assert_eq!(block.decals[2], [[0.0; 4]; 3]);
        assert_eq!(BLOCK_BYTES, 16 * (4 + 1 + 3 * MAX_DECAL_SLOTS));
    }

    #[test]
    fn slots_past_the_limit_are_ignored() {
        let mut decals = MaskDecals::default();
        for i in 0..MAX_DECAL_SLOTS + 3 {
            decals
                .decals
                .push(DecalSlot::atlas(i as u32, [0.0, 0.0, 0.1, 0.1]));
        }
        let block = decals.pack();
        assert_eq!(block.header[0], MAX_DECAL_SLOTS as u32);
        assert_eq!(
            block.decals[MAX_DECAL_SLOTS - 1][1][2],
            (MAX_DECAL_SLOTS - 1) as f32
        );
    }

    #[test]
    fn extraction_keys_every_draw_of_a_masked_entity() {
        use regia_ecs::ecs::system::RunSystemOnce;
        let mut world = World::new();
        let masked = world
            .spawn(MaskDecals::new(
                Arc::default(),
                MaskPalette::from_srgb8([255, 0, 0], [0; 3], [0; 3]),
            ))
            .id();
        let plain = world.spawn_empty().id();
        let (vertices, indices) = crate::box_mesh([0.5; 3]);
        let mesh = Arc::new(crate::CpuMesh { vertices, indices });
        let extracted = |entity: Entity, object_key_xor: u64| crate::ExtractedMesh {
            is_static: false,
            entity,
            mesh: mesh.clone(),
            material: Arc::new(crate::CpuMaterial::default()),
            layer: 1,
            model: regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
            joints: Vec::new(),
            decal_layer: 0,
            uv1: None,
            shadow: crate::ShadowCast::All,
            draw_indices: None,
            lod_key: 0,
            object_key_xor,
            gi_proxy: true,
            lod_dither: 0.0,
        };
        world.insert_resource(ExtractedFrame {
            // A LOD cross-fade pair of the masked entity and one plain mesh.
            meshes: vec![extracted(masked, 0), extracted(masked, 5), extracted(plain, 0)],
            ..ExtractedFrame::default()
        });
        world.run_system_once(extract_mask_decals).expect("system runs");
        let keys: Vec<u64> = world
            .resource::<ExtractedFrame>()
            .mask_decals
            .iter()
            .map(|i| i.object_key)
            .collect();
        assert_eq!(keys, vec![masked.to_bits(), masked.to_bits() ^ 5]);
        world.entity_mut(masked).remove::<MaskDecals>();
        world.run_system_once(extract_mask_decals).expect("system runs");
        assert!(world.resource::<ExtractedFrame>().mask_decals.is_empty());
    }

    #[test]
    fn blend_names_round_trip() {
        for blend in [DecalBlend::Alpha, DecalBlend::Multiply, DecalBlend::Emissive] {
            assert_eq!(DecalBlend::from_name(blend.name()), Some(blend));
        }
        assert_eq!(DecalBlend::from_name("screen"), None);
    }
}
