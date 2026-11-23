// SPDX-License-Identifier: MPL-2.0
//! Materials of the product viewer: what a host sets, and the fixed block a shader reads.
//!
//! A [`ViewerMaterial`] is plain data. It belongs to a [`ShaderFamily`], and each family has one
//! prebuilt pipeline, so switching a gem or a metal writes a [`MaterialBlock`] and at most picks another
//! prebuilt pipeline. The block's meaning per family is the contract with the shader hooks
//! (`hooks.rs`, `book/agents/viewer-renderer.md`): `p0`..`p2` are the family's own.

use bytemuck::{Pod, Zeroable};

/// The shader a material runs. One render pipeline per family (and per winding), all built up front.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ShaderFamily {
    /// Metal and anodized metal (the anodize film is data in the block).
    Metal,
    /// Cut stones: facet normals, faked refraction and reflection, sparkle.
    Gem,
    /// Opal: a body color and a play of color.
    Opal,
    /// Cabochons: smooth domed stones, a sharp coat over a soft body.
    Cabochon,
    /// Frosted stones and glass: rough reflection and rough transmission.
    Frosted,
}

impl ShaderFamily {
    /// Every family, in pipeline order.
    pub const ALL: [ShaderFamily; 5] = [
        ShaderFamily::Metal,
        ShaderFamily::Gem,
        ShaderFamily::Opal,
        ShaderFamily::Cabochon,
        ShaderFamily::Frosted,
    ];

    /// The family's lowercase name (labels, hook errors, the agent page).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            ShaderFamily::Metal => "metal",
            ShaderFamily::Gem => "gem",
            ShaderFamily::Opal => "opal",
            ShaderFamily::Cabochon => "cabochon",
            ShaderFamily::Frosted => "frosted",
        }
    }

    /// The position of this family in [`ShaderFamily::ALL`].
    #[must_use]
    pub fn index(self) -> usize {
        match self {
            ShaderFamily::Metal => 0,
            ShaderFamily::Gem => 1,
            ShaderFamily::Opal => 2,
            ShaderFamily::Cabochon => 3,
            ShaderFamily::Frosted => 4,
        }
    }
}

/// A simple metal: linear base color (the reflectance at normal incidence) and a perceptual roughness.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MetalParams {
    /// Linear Rec.709 reflectance at normal incidence, each channel 0..=1.
    pub base_color: [f32; 3],
    /// Perceptual roughness, 0 (mirror) to 1.
    pub roughness: f32,
}

/// One row of the viewer's small lookup atlas, returned by
/// [`ViewerRenderer::add_lut`](crate::ViewerRenderer::add_lut).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LutHandle(pub(crate) u32);

/// The color of an anodized film.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AnodizeFilm {
    /// One linear color at normal incidence.
    Color([f32; 3]),
    /// A baked cos(theta) to reflectance table.
    Lut(LutHandle),
}

/// Anodized metal: a substrate roughness and a film color. No film is solved at runtime.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AnodizedParams {
    /// Perceptual roughness of the finished surface, 0 to 1.
    pub roughness: f32,
    /// The film.
    pub film: AnodizeFilm,
}

/// A cut stone for the `gem` hook. The stone's facets come from the mesh (one normal and one
/// [`facet`](crate::ViewerVertex::facet) per facet).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GemParams {
    /// Linear color a light path keeps through the stone, each channel 0..=1 (white = colorless).
    pub tint: [f32; 3],
    /// Perceptual roughness of the polish, 0 to 1.
    pub roughness: f32,
    /// Refractive index at the d line, for example 1.77 for corundum.
    pub ior: f32,
    /// How far apart the red and blue refraction lookups sit, 0 (none) to 1 (strong).
    pub dispersion: f32,
    /// Strength of the facet glints, 0 (none) to 1 (strong).
    pub sparkle: f32,
    /// A small per-stone offset that decorrelates the glints of neighboring stones.
    pub seed: f32,
}

/// Opal for the `opal` hook.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OpalParams {
    /// Linear body color, each channel 0..=1.
    pub body: [f32; 3],
    /// Perceptual roughness of the polish, 0 to 1.
    pub roughness: f32,
    /// Strength of the play of color, 0 to 1.
    pub fire: f32,
    /// Size of the color patches in millimeters.
    pub fire_scale_mm: f32,
    /// A small per-stone offset of the pattern.
    pub seed: f32,
}

/// A cabochon for the `cabochon` hook.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CabochonParams {
    /// Linear body color, each channel 0..=1.
    pub body: [f32; 3],
    /// Perceptual roughness of the polish, 0 to 1.
    pub roughness: f32,
    /// Refractive index of the stone.
    pub ior: f32,
    /// How much light seems to pass through the body, 0 (opaque) to 1 (clear).
    pub translucency: f32,
}

/// A frosted stone for the `frosted` hook.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrostedParams {
    /// Linear color kept through the stone, each channel 0..=1.
    pub tint: [f32; 3],
    /// Perceptual roughness of the frost, 0 to 1.
    pub roughness: f32,
    /// Refractive index of the stone.
    pub ior: f32,
}

/// What a surface looks like. All values are data, usually from the material library.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ViewerMaterial {
    /// Plain metal.
    Metal(MetalParams),
    /// Anodized metal.
    Anodized(AnodizedParams),
    /// A cut stone.
    Gem(GemParams),
    /// Opal.
    Opal(OpalParams),
    /// A cabochon.
    Cabochon(CabochonParams),
    /// A frosted stone.
    Frosted(FrostedParams),
    /// A block a look packs itself, for a hook whose fields differ from the table on [`MaterialBlock`].
    /// Values that are not finite read as zero.
    Custom {
        /// The family whose hook reads the block.
        family: ShaderFamily,
        /// The block.
        block: MaterialBlock,
    },
}

impl Default for ViewerMaterial {
    fn default() -> Self {
        ViewerMaterial::Metal(MetalParams {
            base_color: [0.56, 0.57, 0.58],
            roughness: 0.2,
        })
    }
}

/// `p0.x` of a metal block: no film.
pub const FILM_NONE: f32 = 0.0;
/// `p0.x` of a metal block: the film is a lookup row (`p0.y`).
pub const FILM_LUT: f32 = 1.0;

/// The fixed material block every family reads (`MaterialBlock` in `prelude.wgsl`).
///
/// | Family | `base` | `p0` | `p1` | `p2` |
/// |---|---|---|---|---|
/// | metal | rgb, roughness | film mode, lookup row | | |
/// | gem | tint, roughness | ior, dispersion, sparkle, seed | | |
/// | opal | body, roughness | fire, patch size in mm, seed | | |
/// | cabochon | body, roughness | ior, translucency | | |
/// | frosted | tint, roughness | ior | | |
///
/// Unused fields are zero and free for a hook's extension: a new parameter means a new field in the
/// family's params struct, never a new pipeline.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct MaterialBlock {
    /// Family color and roughness.
    pub base: [f32; 4],
    /// Family parameters.
    pub p0: [f32; 4],
    /// Family parameters.
    pub p1: [f32; 4],
    /// Family parameters.
    pub p2: [f32; 4],
}

/// Size of [`MaterialBlock`] in bytes.
pub const MATERIAL_BLOCK_SIZE: u64 = std::mem::size_of::<MaterialBlock>() as u64;

fn unit(v: f32) -> f32 {
    if v.is_finite() { v.clamp(0.0, 1.0) } else { 0.0 }
}

fn unit3(v: [f32; 3]) -> [f32; 3] {
    [unit(v[0]), unit(v[1]), unit(v[2])]
}

fn finite_or(v: f32, fallback: f32) -> f32 {
    if v.is_finite() { v } else { fallback }
}

/// A refractive index the shaders can use: 1 to 3 (diamond's 2.42 fits, a bad value reads as glass).
fn ior(v: f32) -> f32 {
    if v.is_finite() && v > 0.0 {
        v.clamp(1.0, 3.0)
    } else {
        1.5
    }
}

fn color_block(color: [f32; 3], roughness: f32) -> [f32; 4] {
    let c = unit3(color);
    [c[0], c[1], c[2], unit(roughness)]
}

impl ViewerMaterial {
    /// The shader family that draws this material.
    #[must_use]
    pub fn family(&self) -> ShaderFamily {
        match self {
            ViewerMaterial::Metal(_) | ViewerMaterial::Anodized(_) => ShaderFamily::Metal,
            ViewerMaterial::Gem(_) => ShaderFamily::Gem,
            ViewerMaterial::Opal(_) => ShaderFamily::Opal,
            ViewerMaterial::Cabochon(_) => ShaderFamily::Cabochon,
            ViewerMaterial::Frosted(_) => ShaderFamily::Frosted,
            ViewerMaterial::Custom { family, .. } => *family,
        }
    }

    /// Packs the material for the shader. Out-of-range and non-finite values are clamped, so bad
    /// data can never produce a NaN pixel.
    #[must_use]
    pub fn to_block(&self) -> MaterialBlock {
        let mut block = MaterialBlock::zeroed();
        match self {
            ViewerMaterial::Metal(m) => {
                block.base = color_block(m.base_color, m.roughness);
                block.p0 = [FILM_NONE, 0.0, 0.0, 0.0];
            }
            ViewerMaterial::Anodized(a) => match a.film {
                AnodizeFilm::Color(c) => {
                    block.base = color_block(c, a.roughness);
                    block.p0 = [FILM_NONE, 0.0, 0.0, 0.0];
                }
                AnodizeFilm::Lut(row) => {
                    block.base = color_block([1.0, 1.0, 1.0], a.roughness);
                    block.p0 = [FILM_LUT, row.0 as f32, 0.0, 0.0];
                }
            },
            ViewerMaterial::Gem(g) => {
                block.base = color_block(g.tint, g.roughness);
                block.p0 = [
                    ior(g.ior),
                    unit(g.dispersion),
                    unit(g.sparkle),
                    finite_or(g.seed, 0.0),
                ];
            }
            ViewerMaterial::Opal(o) => {
                block.base = color_block(o.body, o.roughness);
                let scale = finite_or(o.fire_scale_mm, 0.5).max(0.01);
                block.p0 = [unit(o.fire), scale, finite_or(o.seed, 0.0), 0.0];
            }
            ViewerMaterial::Cabochon(c) => {
                block.base = color_block(c.body, c.roughness);
                block.p0 = [ior(c.ior), unit(c.translucency), 0.0, 0.0];
            }
            ViewerMaterial::Frosted(f) => {
                block.base = color_block(f.tint, f.roughness);
                block.p0 = [ior(f.ior), 0.0, 0.0, 0.0];
            }
            ViewerMaterial::Custom { block: custom, .. } => {
                let clean = |v: [f32; 4]| v.map(|c| finite_or(c, 0.0));
                block.base = clean(custom.base);
                block.p0 = clean(custom.p0);
                block.p1 = clean(custom.p1);
                block.p2 = clean(custom.p2);
            }
        }
        block
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_size_matches_the_prelude() {
        assert_eq!(MATERIAL_BLOCK_SIZE, 64);
    }

    #[test]
    fn metal_and_anodized_share_one_family_and_each_stone_has_its_own() {
        let metal = ViewerMaterial::default();
        let anodized = ViewerMaterial::Anodized(AnodizedParams {
            roughness: 0.2,
            film: AnodizeFilm::Lut(LutHandle(3)),
        });
        assert_eq!(metal.family(), anodized.family());
        assert_eq!(anodized.to_block().p0[0], FILM_LUT);
        assert_eq!(anodized.to_block().p0[1], 3.0);
        for (i, family) in ShaderFamily::ALL.iter().enumerate() {
            assert_eq!(family.index(), i);
        }
    }

    #[test]
    fn a_custom_block_keeps_its_family_and_loses_only_bad_values() {
        let custom = ViewerMaterial::Custom {
            family: ShaderFamily::Opal,
            block: MaterialBlock {
                base: [1.0, 2.0, -3.0, f32::NAN],
                p0: [f32::INFINITY, 4.0, 5.0, 6.0],
                p1: [7.0; 4],
                p2: [8.0; 4],
            },
        };
        assert_eq!(custom.family(), ShaderFamily::Opal);
        let block = custom.to_block();
        assert_eq!(block.base, [1.0, 2.0, -3.0, 0.0]);
        assert_eq!(block.p0, [0.0, 4.0, 5.0, 6.0]);
        assert_eq!(block.p2, [8.0; 4]);
    }

    #[test]
    fn bad_values_never_reach_the_shader() {
        let gem = ViewerMaterial::Gem(GemParams {
            tint: [f32::NAN, 2.0, -1.0],
            roughness: f32::INFINITY,
            ior: f32::NAN,
            dispersion: -3.0,
            sparkle: f32::NAN,
            seed: f32::NEG_INFINITY,
        });
        let block = gem.to_block();
        for v in block.base.iter().chain(block.p0.iter()) {
            assert!(v.is_finite(), "{block:?}");
        }
        assert_eq!(block.base, [0.0, 1.0, 0.0, 0.0]);
        assert_eq!(block.p0, [1.5, 0.0, 0.0, 0.0]);
    }
}
