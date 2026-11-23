// SPDX-License-Identifier: MPL-2.0
//! Bent normals from the SSAO kernel and specular occlusion from the visibility cone (ADR-1375).
//!
//! CPU mirror of the WGSL in `shaders/ssao.wgsl` (the `bent normal` block: the lean and its
//! encoding) and `shaders/lab_forward.wgsl` (the `cone specular occlusion` block). Keep the two in
//! step; `wgsl_copies_match` checks the constants and the shared decode.
//!
//! - **Bent normal** (Landis, "Production-Ready Global Illumination", SIGGRAPH 2002 course 16: the
//!   mean unoccluded direction; in screen space, Klehm, Ritschel, Eisemann, Seidel, "Bent Normals
//!   and Cones in Screen-space", VMV 2011). The kernel's samples are surface points, not rays: each
//!   one's occluded share `w` (its obscurance × intensity / sample count, the share it takes off
//!   the AO) blocks the directions around it. Over the cosine-weighted hemisphere the open
//!   directions' mean vector is `(2/3) n − Σ w · v̂`, so the bent normal lies along
//!   `n − 1.5 Σ w · v̂` (`BENT_TILT`): a floor beside a tall wall leans 32.5°, what integrating its
//!   open quarter-sphere gives. The lean is capped at 60°. Where the baked probes already hold
//!   an occluder (the probe-relative split, ADR-1104) its share does not lean the normal: their
//!   light already comes from the open side.
//! - **Cone specular occlusion** (Jimenez, Wu, Pesce, Jarabek, "Practical Realtime Strategies for
//!   Accurate Indirect Occlusion", SIGGRAPH 2016 § 5): the visible directions are a cone around the
//!   bent normal whose half-angle `α_v` keeps the AO, `cos α_v = √(1 − AO)` (a cosine-weighted cap of
//!   half-angle α holds sin²α of the hemisphere); the specular lobe is a cone around the mirror
//!   direction. The occlusion is the share of the lobe cone inside the visibility cone, with the
//!   caps' intersection from Oat & Sander, "Ambient Aperture Lighting", I3D 2007 (exact when one
//!   cap holds the other, zero when they are apart, a smoothstep between), normalized by the lobe's
//!   part above the surface's horizon (what the split-sum BRDF already integrates), so an open
//!   surface keeps 1. The lobe's half-angle is twice the GGX half-vector angle at which the
//!   distribution falls to half its peak (`cos²θ = (1 − √2 α²) / (1 − α²)`; a lobe that never
//!   halves inside the hemisphere, α > 0.84, is the whole hemisphere), at least
//!   `SPEC_CONE_MIN_ANGLE`. The forward pass applies it to the SSAO's AO only; the material's
//!   occlusion texture keeps its AO form (`hard_surface::indirect_specular_occlusion`) and the two
//!   multiply.

use regia_core::math::Vec3;

/// How far each unit of occluded share leans the normal (`ssao.wgsl` `BENT_TILT`).
pub(super) const BENT_TILT: f32 = 1.5;
/// The largest lean of the bent normal from the normal, in radians (60°; `BENT_MAX_ANGLE`).
pub(super) const BENT_MAX_ANGLE: f32 = std::f32::consts::FRAC_PI_3;
/// The narrowest specular cone, in radians (about 11°; `lab_forward.wgsl` `SPEC_CONE_MIN_ANGLE`): the
/// visibility cone from a scalar AO is no sharper than that, and a mirror-thin cone turned its edge
/// into a hard line along wall bases.
pub(super) const SPEC_CONE_MIN_ANGLE: f32 = 0.2;

/// The bent normal minus `n` (unit) from `blocked` = Σ share · unit direction to each occluder
/// point and `share` = Σ share (`ssao.wgsl` `bent_normal_lean`).
pub(super) fn bent_normal_lean(n: Vec3, blocked: Vec3, share: f32) -> Vec3 {
    let scale = if share > 1.0 { 1.0 / share } else { 1.0 };
    let b = n - BENT_TILT * scale * blocked;
    let along = b.dot(n);
    let t = b - n * along;
    let tl = t.length();
    if tl.is_nan() || tl <= 1e-6 {
        return Vec3::ZERO;
    }
    let angle = tl.atan2(along).min(BENT_MAX_ANGLE);
    n * angle.cos() + t * (angle.sin() / tl) - n
}

/// One kernel sample as `ssao.wgsl` `cs_main` measures it: the vector from the receiver to the
/// sample point (view space, m) and the share of its obscurance no probe holds (1 without a split).
#[derive(Clone, Copy, Debug)]
pub(super) struct KernelSample {
    /// Sample point minus receiver (m).
    pub(super) v: Vec3,
    /// `rest + brick share × keep + terrain share × keep` (`probe_ao_keep`); 1 without a split.
    pub(super) kept: f32,
}

/// The kernel's AO (whole kernel, no split) and the bent normal's lean for receiver normal `n`
/// over `samples` (all taken samples, misses as zero vectors), as `cs_main` computes them.
pub(super) fn kernel_ao_and_lean(
    n: Vec3,
    samples: &[KernelSample],
    radius: f32,
    bias: f32,
    intensity: f32,
) -> (f32, Vec3) {
    let (mut occlusion, mut share, mut dir) = (0.0f32, 0.0f32, Vec3::ZERO);
    for s in samples {
        let vv = s.v.dot(s.v);
        let falloff = (1.0 - vv / (radius * radius)).clamp(0.0, 1.0);
        let inv = 1.0 / vv.max(1e-8).sqrt();
        let o = (s.v.dot(n) * inv - bias).max(0.0) * falloff;
        occlusion += o;
        let kept = o * s.kept;
        share += kept;
        dir += s.v * (kept * inv);
    }
    let k = intensity / samples.len().max(1) as f32;
    let ao = (1.0 - occlusion * k).clamp(0.0, 1.0);
    (ao, bent_normal_lean(n, dir * k, share * k))
}

/// A lean component as the rgba8unorm code the storage write stores (`bent_encode`;
/// round to nearest).
pub(super) fn encode_component(lean: f32) -> u8 {
    (lean.clamp(-1.0, 1.0) * 127.0 + 128.0).round() as u8
}

/// A lean component from its code (`bent_decode`): one code either side of 128 reads as 0.
pub(super) fn decode_component(code: u8) -> f32 {
    let d = (f32::from(code) - 128.0) / 127.0;
    if d.abs() < 1.5 / 127.0 { 0.0 } else { d }
}

/// The shading normal leaned by a decoded `lean` (`lab_forward.wgsl` `bent_shading_normal`): the
/// lean moves from the depth normal to the (normal-mapped) shading normal `n`; a sum that falls
/// below the surface keeps `n`.
pub(super) fn bent_shading_normal(n: Vec3, lean: Vec3) -> Vec3 {
    let b = n + lean;
    if b.dot(n) <= 1e-3 {
        return n;
    }
    b.normalize()
}

/// Half-angle (radians) of the cone standing for a GGX lobe of roughness `alpha` (α = perceptual²)
/// around the mirror direction (`spec_cone_half_angle`).
pub(super) fn spec_cone_half_angle(alpha: f32) -> f32 {
    let a2 = alpha * alpha;
    let c2 = (1.0 - std::f32::consts::SQRT_2 * a2) / (1.0 - a2).max(1e-6);
    if c2.is_nan() || c2 <= 0.0 {
        return std::f32::consts::FRAC_PI_2;
    }
    (2.0 * c2.min(1.0).sqrt().acos()).clamp(SPEC_CONE_MIN_ANGLE, std::f32::consts::FRAC_PI_2)
}

/// Solid angle (sr) of a spherical cap of half-angle `a`.
pub(super) fn cap_solid_angle(a: f32) -> f32 {
    std::f32::consts::TAU * (1.0 - a.cos())
}

/// Approximate solid angle (sr) of the intersection of two caps of half-angles `a1`, `a2` whose
/// axes are `d` radians apart (Oat & Sander 2007; `cap_intersection`).
pub(super) fn cap_intersection(a1: f32, a2: f32, d: f32) -> f32 {
    let full = cap_solid_angle(a1.min(a2));
    let diff = (a1 - a2).abs();
    if d <= diff {
        return full;
    }
    if d >= a1 + a2 {
        return 0.0;
    }
    let x = 1.0 - ((d - diff) / (a1 + a2 - diff).max(1e-6)).clamp(0.0, 1.0);
    full * x * x * (3.0 - 2.0 * x)
}

/// Specular occlusion from the visibility cone (`cone_specular_occlusion`): the share of the
/// specular cone (mirror direction `r`, roughness `alpha`) inside the visibility cone around the
/// bent normal `bent` that keeps `ao`, over its share above the horizon of the normal `n`.
pub(super) fn cone_specular_occlusion(bent: Vec3, ao: f32, n: Vec3, r: Vec3, alpha: f32) -> f32 {
    let spec = spec_cone_half_angle(alpha);
    let hemi = 0.0f32.acos();
    let vis = (1.0 - ao).clamp(0.0, 1.0).sqrt().acos();
    let seen = cap_intersection(vis, spec, bent.dot(r).clamp(-1.0, 1.0).acos());
    let open = cap_intersection(hemi, spec, n.dot(r).clamp(-1.0, 1.0).acos());
    if open <= 1e-6 {
        return 1.0;
    }
    (seen / open).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests;
