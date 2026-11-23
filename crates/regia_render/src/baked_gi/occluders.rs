// SPDX-License-Identifier: MPL-2.0
//! Capsule occluders for dynamic units over the baked GI volume (near field, ADR-282).
//!
//! A unit standing in baked light should darken the ground and walls next to it; the bake
//! cannot know about it. The host lists a few capsules per unit
//! ([`crate::FrameRenderer::set_gi_occluders`], at most [`MAX_GI_OCCLUDERS`] a frame) and the
//! baked screen pass attenuates the baked irradiance toward each of its six ambient-cube
//! directions by the analytic occlusion of every capsule (`baked_gi_screen.wgsl`
//! `bgi_capsule_occlusion`; this module is its CPU mirror).
//!
//! **Occlusion of one sphere.** Seen from a point with normal `n`, a sphere of radius `r` whose
//! centre is `d` away at angle `θ` from `n` blocks the fraction `x²·cos θ` (with `x = r / d`) of
//! the cosine-weighted hemisphere while it is wholly above the horizon (`cos θ ≥ x`): the
//! differential-area-to-sphere configuration factor (Howell, *A Catalog of Radiation Heat
//! Transfer Configuration Factors*; the same expression as Quilez's sphere ambient occlusion).
//! Across the horizon (`|cos θ| < x`) the cosine is replaced by `(cos θ + x)² / (4x)`, a
//! quadratic that meets `cos θ` with the same value and slope at `cos θ = x` and falls to 0 at
//! `cos θ = −x`, where the sphere is wholly below the horizon (a smoothing derived here, not the
//! exact partial factor). A receiver inside the sphere uses `x = 1`.
//!
//! **A capsule** is treated as the sphere of its radius at the point of its segment closest to
//! the receiver (capsule occluders as swept spheres: Iwanicki, "Lighting Technology of The Last
//! of Us", SIGGRAPH 2013). That is exact for a sphere (`a == b`) and under-counts a long capsule
//! toward its ends; hosts split long bodies into a few capsules. Several occluders combine as
//! the product of their visibilities, the usual independence approximation.
//!
//! The occluder's own surface faces away from its centre (`cos θ ≤ −x`), so a capsule fitted
//! inside a unit's hull does not darken the unit; the six cube directions of a texel on the
//! unit that point into the capsule are darkened, but the shading's normal weights them by
//! nothing there.
//!
//! **Lightmap receivers** (ADR-2656) do not read the ambient cube, so while capsules are set the
//! screen pass also writes [`capsule_visibility`] at the depth normal, one word per lookup texel, and
//! a receiver multiplies its baked diffuse term by it. It is the whole visibility, not
//! [`capsule_visibility_split`]: a receiver's diffuse term takes no SSAO (ADR-2635).

/// Most capsule occluders one frame uses (`baked_gi_screen.wgsl` `BGI_MAX_OCCLUDERS`); more are
/// dropped and counted in [`crate::BakedGiStatus::occluders_dropped`].
pub const MAX_GI_OCCLUDERS: usize = 64;

/// Bytes of the occluder uniform: a 16-byte header (count) and two `vec4<f32>` per capsule.
pub(crate) const OCCLUDER_UNIFORM_BYTES: u64 = 16 + MAX_GI_OCCLUDERS as u64 * 32;

/// One capsule occluder: the segment `a`–`b` swept by a sphere of `radius`, metres, world space.
/// A sphere is `a == b`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GiCapsule {
    /// One end of the segment.
    pub a: [f32; 3],
    /// The other end of the segment.
    pub b: [f32; 3],
    /// Radius, metres (> 0).
    pub radius: f32,
    /// How much of the blocked light it removes, 0..=1 (1 = opaque; below 1 for a body that
    /// bounces some light back or is partly open).
    pub strength: f32,
}

impl GiCapsule {
    /// An opaque capsule from `a` to `b`.
    #[must_use]
    pub fn new(a: [f32; 3], b: [f32; 3], radius: f32) -> Self {
        Self {
            a,
            b,
            radius,
            strength: 1.0,
        }
    }

    /// An opaque sphere.
    #[must_use]
    pub fn sphere(centre: [f32; 3], radius: f32) -> Self {
        Self::new(centre, centre, radius)
    }

    /// Finite ends, a positive finite radius and a positive strength: the capsules
    /// [`crate::FrameRenderer::set_gi_occluders`] keeps.
    #[must_use]
    pub fn is_usable(&self) -> bool {
        self.a.iter().chain(&self.b).all(|v| v.is_finite())
            && self.radius.is_finite()
            && self.radius > 0.0
            && self.strength.is_finite()
            && self.strength > 0.0
    }

    /// The capsule with its strength clamped to 0..=1.
    #[must_use]
    pub(crate) fn clamped(self) -> Self {
        Self {
            strength: self.strength.min(1.0),
            ..self
        }
    }
}

use super::ProbeAoSplit;

type V3 = [f32; 3];

fn sub(a: V3, b: V3) -> V3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn dot(a: V3, b: V3) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// Blocked fraction of the cosine-weighted hemisphere for a sphere at cosine `c` from the
/// direction and `x` = r / d (module docs; `bgi_sphere_blocked`).
fn sphere_blocked(c: f32, x: f32) -> f32 {
    if c >= x {
        x * x * c
    } else {
        let t = (c + x).max(0.0);
        0.25 * x * t * t
    }
}

/// Unit direction from `p` toward the capsule's nearest sphere, its `x` = r / d (≤ 1) and its
/// centre's distance `d`, as the shader computes them.
fn nearest_sphere(c: &GiCapsule, p: V3) -> (V3, f32, f32) {
    let ab = sub(c.b, c.a);
    let t = (dot(sub(p, c.a), ab) / dot(ab, ab).max(1e-12)).clamp(0.0, 1.0);
    let centre = [c.a[0] + ab[0] * t, c.a[1] + ab[1] * t, c.a[2] + ab[2] * t];
    let to_c = sub(centre, p);
    let d = dot(to_c, to_c).sqrt();
    let inv = 1.0 / d.max(1e-6);
    let x = (c.radius * inv).min(1.0);
    ([to_c[0] * inv, to_c[1] * inv, to_c[2] * inv], x, d)
}

/// Visibility (1 = open, 0 = fully blocked) of the cosine-weighted hemisphere around the unit
/// normal `n` at `p` under the capsules: the factor on the baked irradiance there. The screen
/// pass applies it toward the six ambient-cube directions; with an axis normal this is exactly
/// that direction's factor.
#[must_use]
pub fn capsule_visibility(capsules: &[GiCapsule], p: [f32; 3], n: [f32; 3]) -> f32 {
    let mut vis = 1.0f32;
    for c in capsules.iter().take(MAX_GI_OCCLUDERS) {
        let (dir, x, _) = nearest_sphere(c, p);
        vis *= 1.0 - c.strength * sphere_blocked(dot(n, dir), x);
    }
    vis
}

/// [`capsule_visibility`] with the screen-space AO's share of each capsule taken out
/// (ADR-1104): what the screen pass applies while an SSAO kernel of radius `ssao_radius` (m;
/// 0 = SSAO off, the same as [`capsule_visibility`]) also darkens the baked light, at a texel
/// whose probe-relative AO split is `split` ([`ProbeAoSplit::capsule_ssao_share`]). The share
/// is taken toward the normal `n`; the screen pass takes it toward the depth normal for all six
/// directions.
#[must_use]
pub fn capsule_visibility_split(
    capsules: &[GiCapsule],
    p: [f32; 3],
    n: [f32; 3],
    split: &ProbeAoSplit,
    ssao_radius: f32,
) -> f32 {
    let mut vis = 1.0f32;
    for c in capsules.iter().take(MAX_GI_OCCLUDERS) {
        let (dir, x, d) = nearest_sphere(c, p);
        let keep = 1.0 - split.capsule_ssao_share(d, c.radius, dot(n, dir), ssao_radius);
        vis *= 1.0 - c.strength * keep * sphere_blocked(dot(n, dir), x);
    }
    vis
}

/// The six factors the screen pass applies at `p`, in its direction order (+x, −x, +y, −y,
/// +z, −z).
#[cfg(test)]
pub(crate) fn cube_visibility(capsules: &[GiCapsule], p: [f32; 3]) -> [f32; 6] {
    let mut pos = [1.0f32; 3];
    let mut neg = [1.0f32; 3];
    for c in capsules.iter().take(MAX_GI_OCCLUDERS) {
        let (dir, x, _) = nearest_sphere(c, p);
        for ((vp, vn), d) in pos.iter_mut().zip(neg.iter_mut()).zip(dir) {
            *vp *= 1.0 - c.strength * sphere_blocked(d, x);
            *vn *= 1.0 - c.strength * sphere_blocked(-d, x);
        }
    }
    [pos[0], neg[0], pos[1], neg[1], pos[2], neg[2]]
}

/// A key that changes with a capsule list: 0 for an empty one, so a frame that never had a capsule
/// keeps every key this is mixed into. For what reuses a frame's screen passes while nothing else
/// moved (`frame/steps/screen_reuse.rs`).
#[must_use]
pub fn capsule_list_key(capsules: &[GiCapsule]) -> u64 {
    capsules.iter().fold(0_u64, |key, c| {
        [c.a, c.b, [c.radius, c.strength, 0.0]]
            .iter()
            .flatten()
            .fold(key.rotate_left(7) ^ 0x9e37_79b9_7f4a_7c15, |k, v| {
                k.wrapping_mul(0x0000_0100_0000_01b3) ^ u64::from(v.to_bits())
            })
    })
}

/// The occluder uniform's bytes (`BakedGiOccluders`): count, three zero words, then per
/// capsule `(a.xyz, radius)`, `(b.xyz, strength)`. At most [`MAX_GI_OCCLUDERS`].
pub(crate) fn encode_occluders(capsules: &[GiCapsule]) -> Vec<u8> {
    let used = &capsules[..capsules.len().min(MAX_GI_OCCLUDERS)];
    let mut words: Vec<u32> = Vec::with_capacity(4 + used.len() * 8);
    words.extend([used.len() as u32, 0, 0, 0]);
    for c in used {
        words.extend(
            [
                c.a[0], c.a[1], c.a[2], c.radius, c.b[0], c.b[1], c.b[2], c.strength,
            ]
            .map(f32::to_bits),
        );
    }
    bytemuck::cast_slice(&words).to_vec()
}

/// What to write to the occluder uniform this frame, given what it holds (`cache`, updated):
/// `None` when nothing changed. An empty list over a buffer that already holds a zero count
/// (a fresh, zero-initialized buffer included) writes nothing and allocates nothing.
pub(crate) fn stage_occluders(cache: &mut Vec<u8>, capsules: &[GiCapsule]) -> Option<Vec<u8>> {
    if capsules.is_empty() && cache.len() >= 4 && cache[..4].iter().all(|b| *b == 0) {
        return None;
    }
    let bytes = encode_occluders(capsules);
    if *cache == bytes {
        return None;
    }
    cache.clone_from(&bytes);
    Some(bytes)
}

/// The occluders a frame uses: the usable capsules of `capsules` (strength clamped to 1), at
/// most [`MAX_GI_OCCLUDERS`], and how many were dropped (unusable or over the cap).
pub(crate) fn select_occluders(capsules: &[GiCapsule], out: &mut Vec<GiCapsule>) -> usize {
    out.clear();
    let mut dropped = 0;
    for c in capsules {
        if c.is_usable() && out.len() < MAX_GI_OCCLUDERS {
            out.push(c.clamped());
        } else {
            dropped += 1;
        }
    }
    dropped
}

#[cfg(test)]
mod tests {
    use super::*;

    const UP: [f32; 3] = [0.0, 1.0, 0.0];

    #[test]
    fn a_sphere_overhead_blocks_its_configuration_factor() {
        // Radius 1 m, centre 2 m above: x = 1/2, wholly above the horizon, blocks x² = 1/4.
        let s = GiCapsule::sphere([0.0, 2.0, 0.0], 1.0);
        let v = capsule_visibility(&[s], [0.0; 3], UP);
        assert!((v - 0.75).abs() < 1e-6, "{v}");
        // At 60° off the normal (still above the horizon: cos 60° = 0.5 ≥ x = 0.25) the
        // factor is x²·cos θ.
        let d = 4.0;
        let c = [d * 60f32.to_radians().sin(), d * 60f32.to_radians().cos(), 0.0];
        let v = capsule_visibility(&[GiCapsule::sphere(c, 1.0)], [0.0; 3], UP);
        assert!((v - (1.0 - 0.0625 * 0.5)).abs() < 1e-5, "{v}");
        // Half strength removes half of it.
        let half = GiCapsule { strength: 0.5, ..s };
        assert!((capsule_visibility(&[half], [0.0; 3], UP) - 0.875).abs() < 1e-6);
    }

    #[test]
    fn a_capsule_above_a_point_reads_its_nearest_sphere() {
        // A horizontal capsule 3 m above, radius 1: the nearest sphere is straight above.
        let c = GiCapsule::new([-4.0, 3.0, 0.0], [4.0, 3.0, 0.0], 1.0);
        let v = capsule_visibility(&[c], [0.0; 3], UP);
        assert!((v - (1.0 - 1.0 / 9.0)).abs() < 1e-6, "{v}");
        // Past an end, the end sphere.
        let v = capsule_visibility(&[c], [7.0, 0.0, 0.0], UP);
        let x2 = 1.0 / 18.0; // d² = 3² + 3²
        let cos = 3.0 / 18f32.sqrt();
        assert!((v - (1.0 - x2 * cos)).abs() < 1e-5, "{v}");
    }

    #[test]
    fn a_capsule_behind_the_normal_does_nothing() {
        // Below the surface, or level with it far enough out that it is wholly below the
        // horizon: visibility exactly 1.
        let below = GiCapsule::new([-1.0, -3.0, 0.0], [1.0, -3.0, 0.0], 1.0);
        assert_eq!(capsule_visibility(&[below], [0.0; 3], UP), 1.0);
        let low = GiCapsule::sphere([5.0, -1.5, 0.0], 1.0);
        assert_eq!(capsule_visibility(&[low], [0.0; 3], UP), 1.0);
        assert_eq!(capsule_visibility(&[], [0.0; 3], UP), 1.0);
    }

    #[test]
    fn the_horizon_ramp_is_continuous_and_inside_is_full() {
        let x = 0.4;
        for c in [-x, x] {
            let lo = sphere_blocked(c - 1e-4, x);
            let hi = sphere_blocked(c + 1e-4, x);
            assert!((lo - hi).abs() < 1e-4, "{c}: {lo} {hi}");
        }
        assert_eq!(sphere_blocked(-x, x), 0.0);
        // Across the horizon, between the two sides.
        let mid = sphere_blocked(0.0, x);
        assert!(mid > 0.0 && mid < x * x * x, "{mid}");
        // A receiver inside a sphere, facing its centre: fully blocked.
        let s = GiCapsule::sphere([0.0, 0.5, 0.0], 1.0);
        assert!(capsule_visibility(&[s], [0.0; 3], UP).abs() < 1e-6);
    }

    #[test]
    fn occluders_multiply_and_the_cube_matches_the_axis_normals() {
        let a = GiCapsule::sphere([0.0, 2.0, 0.0], 1.0);
        let b = GiCapsule::sphere([2.0, 0.0, 0.0], 1.0);
        let p = [0.0; 3];
        let both = capsule_visibility(&[a, b], p, UP);
        let product = capsule_visibility(&[a], p, UP) * capsule_visibility(&[b], p, UP);
        assert!((both - product).abs() < 1e-6);
        let cube = cube_visibility(&[a, b], p);
        let axes = [
            [1.0, 0.0, 0.0],
            [-1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, -1.0, 0.0],
            [0.0, 0.0, 1.0],
            [0.0, 0.0, -1.0],
        ];
        for (got, n) in cube.iter().zip(axes) {
            assert_eq!(*got, capsule_visibility(&[a, b], p, n), "{n:?}");
        }
        // Up (toward the sphere overhead) is darker than down, which only the other sphere's
        // horizon band reaches.
        assert!(cube[2] < cube[3] && cube[0] < cube[1], "{cube:?}");
    }

    #[test]
    fn the_split_takes_out_only_the_kernel_share() {
        let s = GiCapsule::sphere([1.5, 1.0, 0.0], 1.0);
        let p = [0.0; 3];
        let whole = capsule_visibility(&[s], p, UP);
        let none = ProbeAoSplit::default();
        // SSAO off: exactly the whole capsule.
        assert_eq!(capsule_visibility_split(&[s], p, UP, &none, 0.0), whole);
        // With the kernel: lighter, never past open.
        let split = capsule_visibility_split(&[s], p, UP, &none, 1.2);
        assert!(split > whole && split <= 1.0, "{whole} {split}");
        // A capsule past the kernel's reach keeps all of it.
        let far = GiCapsule::sphere([6.0, 1.0, 0.0], 1.0);
        assert_eq!(
            capsule_visibility_split(&[far], p, UP, &none, 1.2),
            capsule_visibility(&[far], p, UP)
        );
    }

    #[test]
    fn the_list_key_follows_every_capsule_and_is_zero_when_empty() {
        let a = GiCapsule::sphere([0.0, 2.0, 0.0], 1.0);
        let b = GiCapsule::new([0.0; 3], [1.0; 3], 0.5);
        assert_eq!(capsule_list_key(&[]), 0);
        let both = capsule_list_key(&[a, b]);
        assert_ne!(both, 0);
        assert_eq!(both, capsule_list_key(&[a, b]), "stable");
        assert_ne!(both, capsule_list_key(&[b, a]), "the order counts");
        assert_ne!(both, capsule_list_key(&[a]));
        let moved = GiCapsule {
            a: [0.0, 2.5, 0.0],
            ..a
        };
        assert_ne!(both, capsule_list_key(&[moved, b]));
        let weaker = GiCapsule { strength: 0.5, ..b };
        assert_ne!(both, capsule_list_key(&[a, weaker]));
    }

    #[test]
    fn an_empty_list_leaves_the_uniform_untouched() {
        // A fresh buffer is zero-initialized: the renderer's cache starts as its zero header.
        let mut cache = vec![0u8; 16];
        assert_eq!(stage_occluders(&mut cache, &[]), None, "nothing to write");
        assert_eq!(cache, vec![0u8; 16]);
        let caps = [
            GiCapsule::sphere([0.0, 2.0, 0.0], 1.0),
            GiCapsule::new([0.0; 3], [1.0; 3], 0.5),
        ];
        let written = stage_occluders(&mut cache, &caps).expect("a new list uploads");
        assert_eq!(written.len(), 16 + 2 * 32);
        assert_eq!(
            u32::from_le_bytes([written[0], written[1], written[2], written[3]]),
            2
        );
        assert_eq!(stage_occluders(&mut cache, &caps), None, "unchanged");
        // Clearing writes the zero count once, then nothing.
        let cleared = stage_occluders(&mut cache, &[]).expect("the count drops to 0");
        assert_eq!(cleared, vec![0u8; 16]);
        assert_eq!(stage_occluders(&mut cache, &[]), None);
    }

    #[test]
    fn the_list_drops_unusable_capsules_and_caps_at_the_limit() {
        let ok = GiCapsule::sphere([0.0; 3], 1.0);
        let bad = [
            GiCapsule { radius: 0.0, ..ok },
            GiCapsule { strength: 0.0, ..ok },
            GiCapsule {
                a: [f32::NAN, 0.0, 0.0],
                ..ok
            },
        ];
        let mut out = Vec::new();
        assert_eq!(select_occluders(&bad, &mut out), 3);
        assert!(out.is_empty());
        let many = vec![GiCapsule { strength: 3.0, ..ok }; MAX_GI_OCCLUDERS + 5];
        assert_eq!(select_occluders(&many, &mut out), 5);
        assert_eq!(out.len(), MAX_GI_OCCLUDERS);
        assert!(out.iter().all(|c| c.strength == 1.0), "strength clamps to 1");
        assert_eq!(encode_occluders(&many).len() as u64, OCCLUDER_UNIFORM_BYTES);
    }
}
