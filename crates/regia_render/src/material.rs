// SPDX-License-Identifier: MPL-2.0
//! Runtime materials (`specs/13` § 7). The `.material.ron` asset format is owned by M4; this module
//! is the engine-side target struct.

/// glTF-compatible alpha modes (`specs/13` § 6).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum AlphaMode {
    /// Fully opaque.
    #[default]
    Opaque,
    /// Alpha test with cutoff.
    Mask {
        /// Discard fragments with alpha below this value.
        cutoff: f32,
    },
    /// Standard alpha blending.
    Blend,
    /// Premultiplied alpha blend.
    Premultiplied,
    /// Additive blend.
    Additive,
}

/// UV transform for albedo and other maps.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UvTransform {
    /// Scale (u, v).
    pub scale: [f32; 2],
    /// Offset (u, v).
    pub offset: [f32; 2],
    /// Rotation in radians.
    pub rotation: f32,
}

impl Default for UvTransform {
    fn default() -> Self {
        Self {
            scale: [1.0, 1.0],
            offset: [0.0, 0.0],
            rotation: 0.0,
        }
    }
}

/// Standard metallic–roughness material (`specs/13` § 6–7).
///
/// # Example
///
/// ```
/// use regia_render::{AlphaMode, StandardMaterial};
///
/// let material = StandardMaterial {
///     metallic: 0.0,
///     roughness: 0.4,
///     ..StandardMaterial::default()
/// };
/// assert_eq!(material.alpha_mode, AlphaMode::Opaque);
/// assert_eq!(material.metallic, 0.0);
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct StandardMaterial {
    /// Base colour (linear RGB).
    pub base_color: [f32; 4],
    /// Metallic factor.
    pub metallic: f32,
    /// Perceptual roughness.
    pub roughness: f32,
    /// Normal map strength.
    pub normal_scale: f32,
    /// Occlusion strength.
    pub occlusion_strength: f32,
    /// Emissive colour (linear RGB).
    pub emissive: [f32; 3],
    /// Emissive intensity scale (EV-ish).
    pub emissive_intensity: f32,
    /// Alpha mode.
    pub alpha_mode: AlphaMode,
    /// Double-sided (no back-face cull).
    pub double_sided: bool,
    /// Unlit (no lighting response).
    pub unlit: bool,
    /// UV transform for the base colour map.
    pub uv_transform: UvTransform,
    /// Optional clearcoat weight (Tier 1.5).
    pub clearcoat: f32,
    /// Optional clearcoat roughness.
    pub clearcoat_roughness: f32,
    /// Optional anisotropy strength (−1..1).
    pub anisotropy: f32,
    /// Detail, parallax, edge wear, specular occlusion, and multi-scatter.
    ///
    /// The GPU path reads the same knobs from [`crate::LabMaterial::hard_surface`].
    /// Clearcoat and anisotropy above are the authored factors; this block adds the
    /// maps that large hard surfaces need. The four masked layers on
    /// [`LayeredStandardMaterial`] stay authored data — this lane does not shade them.
    pub hard_surface: crate::HardSurfaceFactors,
}

impl Default for StandardMaterial {
    fn default() -> Self {
        Self {
            base_color: [0.8, 0.8, 0.8, 1.0],
            metallic: 0.0,
            roughness: 0.5,
            normal_scale: 1.0,
            occlusion_strength: 1.0,
            emissive: [0.0, 0.0, 0.0],
            emissive_intensity: 1.0,
            alpha_mode: AlphaMode::Opaque,
            double_sided: false,
            unlit: false,
            uv_transform: UvTransform::default(),
            clearcoat: 0.0,
            clearcoat_roughness: 0.0,
            anisotropy: 0.0,
            hard_surface: crate::HardSurfaceFactors::OFF,
        }
    }
}

/// One paint/wear layer in the layered standard material (`specs/26` § 4, M5 row).
#[derive(Debug, Clone, PartialEq)]
pub struct MaterialLayer {
    /// Layer albedo tint (linear RGB).
    pub tint: [f32; 3],
    /// Roughness override blended by mask.
    pub roughness: f32,
    /// Metallic override blended by mask.
    pub metallic: f32,
    /// Triplanar mapping for cliffs (terrain path).
    pub triplanar: bool,
}

impl Default for MaterialLayer {
    fn default() -> Self {
        Self {
            tint: [1.0, 1.0, 1.0],
            roughness: 0.5,
            metallic: 0.0,
            triplanar: false,
        }
    }
}

/// Base material plus up to four masked layers (paint, dust, wear, wetness).
#[derive(Debug, Clone, PartialEq)]
pub struct LayeredStandardMaterial {
    /// Base layer.
    pub base: StandardMaterial,
    /// Masked layers (unused entries are ignored).
    pub layers: [MaterialLayer; 4],
    /// Wet/dry blend (0 = dry, 1 = wet); affects top layer weight in shader (M15+).
    pub wetness: f32,
}

impl Default for LayeredStandardMaterial {
    fn default() -> Self {
        Self {
            base: StandardMaterial::default(),
            layers: [
                MaterialLayer::default(),
                MaterialLayer::default(),
                MaterialLayer::default(),
                MaterialLayer::default(),
            ],
            wetness: 0.0,
        }
    }
}
