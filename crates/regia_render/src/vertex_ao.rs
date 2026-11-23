// SPDX-License-Identifier: MPL-2.0
//! Per-vertex ambient occlusion on opaque lab materials (ADR-1512, `book/agents/vertex-ao.md`).
//!
//! A mesh that fakes interior sky falloff with stepped material variants prints a hard edge at
//! every step. With [`LabMaterial::vertex_ao`] on, the fourth component of [`LabVertex::color`]
//! is a 0..1 occlusion factor instead of an opacity: it is interpolated across each triangle
//! and multiplies the indirect light (sky, irradiance probes, baked GI and the reflections that
//! read them) and nothing else. Direct sun and local lights are untouched.
//!
//! This module is the CPU reference of that scaling (the shader in `lab_forward.wgsl` mirrors
//! it) plus helpers that write the factor into vertices. The flag is opaque-only: a masked,
//! blended, additive, unlit or hair material keeps the vertex alpha as its opacity.
//!
//! [`LabVertex::color`]: crate::LabVertex::color

use crate::lab_frame::{ALPHA_OPAQUE, LabMaterial, LabVertex};

/// Bit of the packed `alpha.w` material constant that turns the per-vertex occlusion on (bit 0
/// is the packed-ORM flag).
pub const VERTEX_AO_FLAG: f32 = 2.0;

/// The occlusion factor a vertex color alpha stands for: clamped to 0..1, linear. A NaN or
/// infinite alpha is no occlusion (1.0), so a broken vertex cannot black out a surface.
#[must_use]
pub fn factor(color_alpha: f32) -> f32 {
    if color_alpha.is_finite() {
        color_alpha.clamp(0.0, 1.0)
    } else {
        1.0
    }
}

/// CPU reference of the indirect scaling: the indirect radiance `rgb` (diffuse or specular,
/// before the surface color) times [`factor`]. The direct light never goes through this.
#[must_use]
pub fn scale_indirect(indirect: [f32; 3], color_alpha: f32) -> [f32; 3] {
    let k = factor(color_alpha);
    indirect.map(|c| c * k)
}

/// Whether the renderer honors [`LabMaterial::vertex_ao`] for `material`: only on an opaque,
/// lit, non-hair material, where the vertex alpha is not an opacity.
#[must_use]
pub fn is_active(material: &LabMaterial) -> bool {
    material.vertex_ao
        && !material.unlit
        && !material.hair.enabled
        && material.alpha_mode == ALPHA_OPAQUE
        && material.transmission <= 0.0
}

/// The `alpha.w` contribution of `material`: [`VERTEX_AO_FLAG`] when [`is_active`], else 0.
#[must_use]
pub(crate) fn flag_bit(material: &LabMaterial) -> f32 {
    if is_active(material) { VERTEX_AO_FLAG } else { 0.0 }
}

/// Writes `ao[i]` (clamped 0..1) into the color alpha of `vertices[i]`. Extra entries on either
/// side are ignored. Returns how many vertices were written.
pub fn write_vertex_ao(vertices: &mut [LabVertex], ao: &[f32]) -> usize {
    let mut written = 0;
    for (vertex, &value) in vertices.iter_mut().zip(ao) {
        vertex.color[3] = factor(value);
        written += 1;
    }
    written
}

/// Smallest and largest color alpha over `vertices`, or `None` for an empty slice. The agent
/// uses it to tell a mesh that carries an occlusion ramp from a flat one.
#[must_use]
pub fn alpha_range(vertices: &[LabVertex]) -> Option<(f32, f32)> {
    let mut it = vertices.iter().map(|v| factor(v.color[3]));
    let first = it.next()?;
    Some(it.fold((first, first), |(lo, hi), a| (lo.min(a), hi.max(a))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_factor_is_clamped_linear_and_nan_safe() {
        assert_eq!(factor(0.25), 0.25);
        assert_eq!(factor(-3.0), 0.0);
        assert_eq!(factor(7.0), 1.0);
        assert_eq!(factor(f32::NAN), 1.0);
        assert_eq!(factor(f32::INFINITY), 1.0);
    }

    #[test]
    fn scaling_is_linear_per_channel_and_one_is_identity() {
        let e = [0.5, 2.0, 8.0];
        assert_eq!(scale_indirect(e, 1.0), e);
        assert_eq!(scale_indirect(e, 0.0), [0.0; 3]);
        assert_eq!(scale_indirect(e, 0.5), [0.25, 1.0, 4.0]);
        // Half of half is a quarter: the scaling composes linearly.
        let twice = scale_indirect(scale_indirect(e, 0.5), 0.5);
        assert_eq!(twice, scale_indirect(e, 0.25));
    }

    #[test]
    fn the_flag_is_off_by_default_and_only_opaque_lit_materials_honor_it() {
        let mut m = LabMaterial::default();
        assert_eq!(flag_bit(&m), 0.0);
        m.vertex_ao = true;
        assert_eq!(flag_bit(&m), VERTEX_AO_FLAG);
        for mode in [
            crate::lab_frame::ALPHA_MASK,
            crate::lab_frame::ALPHA_BLEND,
            crate::lab_frame::ALPHA_PREMULTIPLIED,
            crate::lab_frame::ALPHA_ADDITIVE,
        ] {
            let masked = LabMaterial {
                alpha_mode: mode,
                ..m.clone()
            };
            assert_eq!(flag_bit(&masked), 0.0, "alpha mode {mode}");
        }
        let unlit = LabMaterial {
            unlit: true,
            ..m.clone()
        };
        assert_eq!(flag_bit(&unlit), 0.0);
        let glass = LabMaterial {
            transmission: 0.5,
            ..m
        };
        assert_eq!(flag_bit(&glass), 0.0);
    }

    #[test]
    fn write_vertex_ao_clamps_and_stops_at_the_shorter_side() {
        let mut v = vec![LabVertex::from_pos_normal_uv([0.0; 3], [0.0, 1.0, 0.0], [0.0; 2]); 3];
        assert_eq!(write_vertex_ao(&mut v, &[0.5, 2.0]), 2);
        assert_eq!(v[0].color, [1.0, 1.0, 1.0, 0.5]);
        assert_eq!(v[1].color[3], 1.0);
        assert_eq!(v[2].color[3], 1.0);
        assert_eq!(alpha_range(&v), Some((0.5, 1.0)));
        assert_eq!(alpha_range(&[]), None);
    }
}
