// SPDX-License-Identifier: MPL-2.0
//! GPU uniform layouts shared by the frame pipelines (`specs/13` § 6).
//!
//! The structs here must match the WGSL uniform layouts byte-for-byte; the
//! `const` asserts below pin the offsets that have regressed before.
//!
//! ## Provenance
//!
//! - **AgX tonemapping** (CPU version in `tonemap.rs`): Sobotka's public AgX;
//!   polynomial fit by C.E. Hill (CC0), used in Filament (Apache-2.0).
//! - **Cascaded shadow maps**: Wronski, "Stable Cascaded Shadow Maps" (SIGGRAPH
//!   2014). Practical Split Scheme (Zhang et al. 2006, GPU Gems 3).

// ---------------------------------------------------------------------------
// Directional light uniform
// ---------------------------------------------------------------------------

/// Directional light parameters (matches the WGSL `DirectionalLight` struct).
///
/// WGSL layout (uniform buffer rules, vec3 = 16-byte align):
///   `vec3<f32>`  offset  0, size 12
///   u32        offset 12, size  4  (`_pad0` in WGSL)
///   `vec3<f32>`  offset 16, size 12
///   f32        offset 28, size  4  (`illuminance`)
///   total: 32 (next 16-byte multiple)
///
/// Note: a previous 48-byte Rust layout placed `illuminance` at offset 40, so the shader
/// read zero and every scene looked ambient-only.
#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct DirectionalLightUniform {
    /// World-space direction of light travel (from the light toward the scene, normalized).
    ///
    /// The shader converts this to a vector toward the light via negation.
    pub direction: [f32; 3],
    /// The word after `direction` (WGSL `_pad0`): the sun's angular radius in radians as `f32` bits,
    /// which the forward pass's specular integrates over (ADR-3190). 0 (the default, and every
    /// light [`Self::new`] makes) is a point sun. Set it through [`Self::with_specular_disc`].
    pub _pad_dir: u32,
    /// Linear RGB colour of the light.
    pub color: [f32; 3],
    /// Illuminance in lux.
    pub illuminance: f32,
}

impl DirectionalLightUniform {
    /// A directional light travelling along `direction` (normalized here) with linear `color` and
    /// `illuminance` in lux. Prefer this over a struct literal: it keeps callers free of padding fields.
    ///
    /// The sun term reaches every lit pixel, so non-finite input is not passed on (ADR-869): a
    /// zero, NaN or infinite `direction` (or one whose length overflows `f32`) travels straight down, and a NaN or infinite `color`
    /// component or `illuminance` is 0 (no light rather than a NaN frame).
    pub fn new(direction: [f32; 3], color: [f32; 3], illuminance: f32) -> Self {
        let len =
            (direction[0] * direction[0] + direction[1] * direction[1] + direction[2] * direction[2]).sqrt();
        let direction = if len.is_finite() && len > 0.0 {
            [direction[0] / len, direction[1] / len, direction[2] / len]
        } else {
            [0.0, -1.0, 0.0]
        };
        let finite_or_zero = |v: f32| if v.is_finite() { v } else { 0.0 };
        Self {
            direction,
            _pad_dir: 0,
            color: color.map(finite_or_zero),
            illuminance: finite_or_zero(illuminance),
        }
    }

    /// The largest angular radius [`Self::with_specular_disc`] keeps, radians (about 11°; the
    /// sun is 0.27°). The disc term is a small-source form; larger sources take a sized light.
    pub const MAX_SPECULAR_DISC: f32 = 0.2;

    /// This light with its specular lit from a disc of `angular_radius` radians instead of a
    /// point (ADR-3190): the forward pass's isotropic GGX lobe becomes its mean over the disc, as
    /// the path tracer samples the sun's cone. Use the planet's `sun_angular_radius`. A
    /// non-finite or non-positive radius is the point sun (bit-identical to [`Self::new`]); a
    /// larger one is clamped to [`Self::MAX_SPECULAR_DISC`]. The diffuse term and the shadows
    /// are unchanged.
    #[must_use]
    pub fn with_specular_disc(mut self, angular_radius: f32) -> Self {
        self._pad_dir = if angular_radius.is_finite() && angular_radius > 0.0 {
            angular_radius.min(Self::MAX_SPECULAR_DISC).to_bits()
        } else {
            0
        };
        self
    }

    /// The angular radius the specular integrates over (radians); 0 for a point sun. A word
    /// that is not a radius [`Self::with_specular_disc`] would write (NaN, infinite, above
    /// [`Self::MAX_SPECULAR_DISC`]) reads as 0, as the shader treats it.
    #[must_use]
    pub fn specular_disc(&self) -> f32 {
        let r = f32::from_bits(self._pad_dir);
        if r > 0.0 && r <= Self::MAX_SPECULAR_DISC {
            r
        } else {
            0.0
        }
    }
}

const _: () = assert!(size_of::<DirectionalLightUniform>() == 32);
const _: () = assert!(std::mem::offset_of!(DirectionalLightUniform, illuminance) == 28);

// ---------------------------------------------------------------------------
// Shadow uniforms (cascaded shadow maps)
// ---------------------------------------------------------------------------

/// Per-cascade shadow uniform (matches the WGSL `ShadowUniform` struct).
///
/// WGSL layout (uniform buffer rules):
///   `mat4x4<f32>`  offset  0, size 64, align 16
///   f32           offset 64, size  4, align  4
///   `vec3<f32>`   offset 80, size 12, align 16  (needs 12 bytes of inter-field padding)
///   total size: 96 (92 rounded up to next multiple of 16)
#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct ShadowUniform {
    /// World-to-shadow-UV matrix (view-projection).
    pub light_view_proj: [[f32; 4]; 4],
    /// Cascade split distance (far plane in view space).
    pub split_distance: f32,
    /// Inter-field padding: 12 bytes to reach the 16-byte alignment of `bias` (vec3).
    _pad_align: [u32; 3],
    /// Receiver-side shadow bias: `x` = world size of one cascade texel,
    /// `y` = 1 / orthographic depth extent (see the WGSL `biased_shadow_depth`).
    pub bias: [f32; 2],
    /// Unused third component of the WGSL `vec3`.
    pub _pad0: f32,
    /// Pad struct size to 96 bytes (16-byte multiple, matching WGSL struct end padding).
    _pad_end: u32,
}

impl ShadowUniform {
    pub(crate) const IDENTITY: Self = Self {
        light_view_proj: [[0.0; 4]; 4],
        split_distance: 0.0,
        _pad_align: [0; 3],
        bias: [0.0; 2],
        _pad0: 0.0,
        _pad_end: 0,
    };

    /// Creates a cascade entry; `texel_world` is the world size of one shadow-map
    /// texel and `depth_extent` the orthographic depth range the cascade covers
    /// (both feed the shader's deterministic receiver-side bias).
    pub(crate) fn new(
        light_view_proj: [[f32; 4]; 4],
        split_distance: f32,
        texel_world: f32,
        depth_extent: f32,
    ) -> Self {
        Self {
            light_view_proj,
            split_distance,
            _pad_align: [0; 3],
            bias: [
                texel_world,
                if depth_extent > 0.0 {
                    1.0 / depth_extent
                } else {
                    0.0
                },
            ],
            _pad0: 0.0,
            _pad_end: 0,
        }
    }
}

/// Maximum number of shadow cascades (the WGSL array size; profiles may use fewer).
pub const CASCADE_COUNT: u32 = 4;

/// Aggregate shadow uniforms (up to [`CASCADE_COUNT`] cascades + the active count).
///
/// `count[0]` is the number of cascades the frame actually uses; unused entries are
/// ignored by the shader.
#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct ShadowUniforms {
    /// Per-cascade data (only `count[0]` entries are meaningful).
    pub cascades: [ShadowUniform; CASCADE_COUNT as usize],
    /// `x` = active cascade count; the rest is padding.
    pub count: [f32; 4],
}

const _: () = assert!(size_of::<ShadowUniforms>() == 400);

// ---------------------------------------------------------------------------
// Readback result
// ---------------------------------------------------------------------------

/// A rendered frame read back from the GPU: tightly packed sRGB-encoded RGBA8.
#[derive(Clone, Debug)]
pub struct ForwardResult {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Pixel data, row-major, 4 bytes per pixel.
    pub pixels: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directional_light_uniform_matches_wgsl_layout() {
        assert_eq!(std::mem::size_of::<DirectionalLightUniform>(), 32);
        assert_eq!(std::mem::offset_of!(DirectionalLightUniform, illuminance), 28);
    }

    #[test]
    fn directional_light_never_carries_a_non_finite_value() {
        let ok = DirectionalLightUniform::new([0.0, -3.0, 4.0], [1.0, 0.5, 0.25], 1000.0);
        assert_eq!(ok.direction, [0.0, -0.6, 0.8]);
        assert_eq!(ok.color, [1.0, 0.5, 0.25]);
        assert_eq!(ok.illuminance, 1000.0);
        for direction in [
            [0.0; 3],
            [f32::NAN, 0.0, 0.0],
            [f32::INFINITY, -1.0, 0.0],
            [1.0e30, 1.0e30, 0.0],
        ] {
            let light = DirectionalLightUniform::new(direction, [1.0; 3], 1.0);
            assert!(
                light.direction.iter().all(|c| c.is_finite()),
                "{direction:?} → {:?}",
                light.direction
            );
            let len2: f32 = light.direction.iter().map(|c| c * c).sum();
            assert!((len2 - 1.0).abs() < 1e-5, "{direction:?} → {:?}", light.direction);
        }
        let bad = DirectionalLightUniform::new([0.0, -1.0, 0.0], [f32::NAN, 1.0, f32::INFINITY], f32::NAN);
        assert_eq!(bad.color, [0.0, 1.0, 0.0]);
        assert_eq!(bad.illuminance, 0.0);
    }

    #[test]
    fn specular_disc_is_off_by_default_and_sanitized() {
        let point = DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 1.0);
        assert_eq!(point._pad_dir, 0);
        assert_eq!(point.specular_disc(), 0.0);
        let sun = point.with_specular_disc(0.004_675);
        assert_eq!(sun.specular_disc(), 0.004_675);
        assert_eq!(sun._pad_dir, 0.004_675f32.to_bits());
        for bad in [0.0, -0.1, f32::NAN, f32::NEG_INFINITY, f32::INFINITY] {
            assert_eq!(point.with_specular_disc(bad)._pad_dir, 0, "{bad}");
        }
        let wide = point.with_specular_disc(1.0);
        assert_eq!(wide.specular_disc(), DirectionalLightUniform::MAX_SPECULAR_DISC);
        // Turning it off again gives back the point sun's exact bytes.
        assert_eq!(
            bytemuck::bytes_of(&sun.with_specular_disc(0.0)),
            bytemuck::bytes_of(&point)
        );
        // A word that is not a radius (a host's struct literal) reads as the point sun.
        for word in [f32::NAN, f32::INFINITY, 0.5] {
            let junk = DirectionalLightUniform {
                _pad_dir: word.to_bits(),
                ..point
            };
            assert_eq!(junk.specular_disc(), 0.0, "{word}");
        }
    }

    #[test]
    fn shadow_uniforms_match_wgsl_layout() {
        assert_eq!(std::mem::size_of::<ShadowUniform>(), 96);
        assert_eq!(std::mem::size_of::<ShadowUniforms>(), 400);
        assert_eq!(
            std::mem::offset_of!(ShadowUniforms, count),
            96 * CASCADE_COUNT as usize
        );
    }
}
