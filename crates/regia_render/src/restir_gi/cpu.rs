// SPDX-License-Identifier: MPL-2.0
//! Device-independent reservoir math and the work counters shared with `render.restir_gi`.

#[cfg(test)]
use regia_core::glam::Vec3;

/// Maximum world-space displacement of a reused opaque hit
/// material point under its instance transform (ADR-4001).
///
/// The shader constant `GI_HIT_MOTION_M` is this value. A still surface survives a camera move
/// because the hit is stored in world space; the receiver reprojection remains the camera test.
pub const HIT_MOTION_METRES: f32 = 0.05;

/// Whether a stored hit still matches its transformed material point inside [`HIT_MOTION_METRES`].
///
/// `stored_instance == 0` is a sky sample. The camera reprojection test owns those, so this
/// predicate accepts them. `found` is the stored object-space point transformed by the confirmed instance (world position
/// and instance custom data). `None` means that query missed, which drops an opaque hit.
#[cfg(test)]
pub(crate) fn hit_motion_agrees(stored_hit: Vec3, stored_instance: u32, found: Option<(Vec3, u32)>) -> bool {
    if stored_instance == 0 {
        return true;
    }
    let Some((position, instance)) = found else {
        return false;
    };
    if instance != stored_instance || !position.is_finite() || !stored_hit.is_finite() {
        return false;
    }
    let gap = (position - stored_hit).length();
    gap.is_finite() && gap <= HIT_MOTION_METRES
}

/// One completed frame's GPU work. Readback is asynchronous, so the agent can report these a
/// few frames after the work happened without polling the device.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RestirGiWork {
    /// Every ray query (initial, sun shadow, temporal/spatial visibility, object-motion confirms
    /// and periodic validation).
    pub rays: u64,
    /// Cosine-distributed initial path rays.
    pub initial_rays: u64,
    /// Direct-sun shadow rays at initial hit points.
    pub shadow_rays: u64,
    /// Reused-path visibility rays.
    pub visibility_rays: u64,
    /// Accepted temporal reservoirs.
    pub temporal_reused: u64,
    /// Accepted spatial neighbor reservoirs (not the pixel's own temporal reservoir).
    pub spatial_reused: u64,
    /// Temporal histories rejected by reprojection, surface, visibility or object motion.
    pub history_rejections: u64,
    /// Temporal histories rejected by the periodic hit/radiance revalidation.
    pub validation_rejections: u64,
    /// Initial rays that committed a geometry hit rather than missing to sky.
    pub initial_hits: u64,
    /// Samples that took the flat-sky fallback (a ray miss or no probe coverage).
    pub sky_samples: u64,
    /// Direct-sun shadow rays that found the sun visible.
    pub sun_visible: u64,
    /// Sum of Rec. 709 probe radiance at initial hits, before direct sun or emissive (whole units).
    pub probe_lum_sum: u64,
    /// Temporal histories dropped because the opaque hit's surface moved farther than
    /// [`HIT_MOTION_METRES`]. Also included in [`Self::history_rejections`].
    pub motion_rejections: u64,
}

impl RestirGiWork {
    /// Adds another frame's counters.
    pub fn accumulate(&mut self, other: Self) {
        for (a, b) in [
            (&mut self.rays, other.rays),
            (&mut self.initial_rays, other.initial_rays),
            (&mut self.shadow_rays, other.shadow_rays),
            (&mut self.visibility_rays, other.visibility_rays),
            (&mut self.temporal_reused, other.temporal_reused),
            (&mut self.spatial_reused, other.spatial_reused),
            (&mut self.history_rejections, other.history_rejections),
            (&mut self.validation_rejections, other.validation_rejections),
            (&mut self.initial_hits, other.initial_hits),
            (&mut self.sky_samples, other.sky_samples),
            (&mut self.sun_visible, other.sun_visible),
            (&mut self.probe_lum_sum, other.probe_lum_sum),
            (&mut self.motion_rejections, other.motion_rejections),
        ] {
            *a = a.saturating_add(b);
        }
    }
}

/// Rec. 709 luminance, the target function of Ouyang et al. Equation 10.
#[cfg(test)]
pub(crate) fn luminance(c: Vec3) -> f32 {
    0.2126 * c.x + 0.7152 * c.y + 0.0722 * c.z
}

/// The path vertex carried by a reservoir.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg(test)]
pub(crate) struct PathSample {
    /// Visible point that generated the path.
    pub visible: Vec3,
    /// Visible surface normal.
    pub visible_normal: Vec3,
    /// Indirect path vertex.
    pub hit: Vec3,
    /// Unit normal at the hit.
    pub hit_normal: Vec3,
    /// Outgoing radiance at the hit toward the visible point.
    pub radiance: Vec3,
}

#[cfg(test)]
impl PathSample {
    pub(crate) fn target(self) -> f32 {
        luminance(self.radiance).max(0.0)
    }
}

/// Streaming weighted reservoir, mirrored by `Reservoir` in `restir_gi.wgsl`.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg(test)]
pub(crate) struct Reservoir {
    /// Selected path vertex.
    pub sample: PathSample,
    /// Sum of RIS weights.
    pub weight_sum: f32,
    /// Number of candidates represented.
    pub count: f32,
    /// Contribution weight.
    pub contribution: f32,
}

#[cfg(test)]
impl Default for Reservoir {
    fn default() -> Self {
        let zero = PathSample {
            visible: Vec3::ZERO,
            visible_normal: Vec3::Y,
            hit: Vec3::ZERO,
            hit_normal: Vec3::Y,
            radiance: Vec3::ZERO,
        };
        Self {
            sample: zero,
            weight_sum: 0.0,
            count: 0.0,
            contribution: 0.0,
        }
    }
}

#[cfg(test)]
impl Reservoir {
    /// Ouyang et al. Algorithm 1, with the random roll supplied by the caller's fixed stream.
    pub(crate) fn update(&mut self, sample: PathSample, weight: f32, count: f32, roll: f32) {
        if !(weight.is_finite() && weight > 0.0 && count.is_finite() && count > 0.0) {
            return;
        }
        self.weight_sum += weight;
        self.count += count;
        if roll * self.weight_sum < weight {
            self.sample = sample;
        }
    }

    /// Merge another reservoir at `target` (Algorithm 1 `MERGE`).
    pub(crate) fn merge(&mut self, other: &Self, target: f32, roll: f32) {
        self.update(
            other.sample,
            target.max(0.0) * other.contribution * other.count,
            other.count,
            roll,
        );
    }

    /// Equation 7: `W = w / (Z · p̂(z))`; `support` is the sum of counts whose source
    /// distributions support the selected path. A zero target or support has zero contribution.
    pub(crate) fn finish(&mut self, target: f32, support: f32) {
        self.contribution = if target.is_finite()
            && target > 0.0
            && support.is_finite()
            && support > 0.0
            && self.weight_sum.is_finite()
        {
            self.weight_sum / (support * target)
        } else {
            0.0
        };
    }

    /// Temporal cap, preserving the mean candidate weight when M is shortened (Bitterli et al.).
    pub(crate) fn cap_count(&mut self, cap: f32) {
        if self.count > cap && self.count > 0.0 {
            self.weight_sum *= cap / self.count;
            self.count = cap;
        }
    }
}

/// Ouyang et al. Equation 11. Converts a sample's source solid-angle measure at `source_visible`
/// into the destination's, including the cosine at the shared path vertex and both squared
/// distances. Degenerate geometry returns zero, which rejects the reuse rather than dividing.
#[cfg(test)]
pub(crate) fn solid_angle_jacobian(
    source_visible: Vec3,
    destination_visible: Vec3,
    sample: &PathSample,
) -> f32 {
    let source = source_visible - sample.hit;
    let destination = destination_visible - sample.hit;
    let source_len = source.length();
    let destination_len = destination.length();
    if !(source_len.is_finite() && destination_len.is_finite() && source_len > 1e-6 && destination_len > 1e-6)
    {
        return 0.0;
    }
    let source_cos = sample.hit_normal.dot(source / source_len).abs();
    let destination_cos = sample.hit_normal.dot(destination / destination_len).abs();
    let denominator = source_cos * destination_len * destination_len;
    if !(denominator.is_finite() && denominator > 1e-12) {
        return 0.0;
    }
    let j = destination_cos * source_len * source_len / denominator;
    if j.is_finite() { j.clamp(1e-4, 1e4) } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> PathSample {
        PathSample {
            visible: Vec3::new(0.0, 0.0, 0.0),
            visible_normal: Vec3::Y,
            hit: Vec3::new(0.0, 2.0, 0.0),
            hit_normal: Vec3::NEG_Y,
            radiance: Vec3::new(1.0, 2.0, 3.0),
        }
    }

    #[test]
    fn jacobian_is_one_at_the_source_and_tracks_cosine_and_squared_distance() {
        let s = sample();
        let identity = solid_angle_jacobian(s.visible, s.visible, &s);
        assert!((identity - 1.0).abs() < 1e-5, "{identity}");
        let destination = Vec3::new(0.0, 0.0, 4.0);
        let j = solid_angle_jacobian(s.visible, destination, &s);
        // |x1-x2|² = 4, |x1r-x2|² = 20, source cosine = 1 and destination cosine = 2/√20.
        let expected = 0.4 / 20.0_f32.sqrt();
        assert!((j - expected).abs() < 1e-5, "{j} != {expected}");
        assert_eq!(solid_angle_jacobian(s.visible, s.hit, &s), 0.0);
        assert_eq!(solid_angle_jacobian(s.visible, Vec3::splat(f32::NAN), &s), 0.0);
    }

    #[test]
    fn streaming_update_keeps_weight_and_count_and_selects_by_weight() {
        let mut r = Reservoir::default();
        let s = sample();
        r.update(s, 2.0, 1.0, 1.0);
        assert_ne!(r.sample, s);
        r.update(s, 2.0, 1.0, 0.49);
        assert_eq!(r.sample, s);
        assert_eq!(r.weight_sum, 4.0);
        assert_eq!(r.count, 2.0);
        r.finish(s.target(), 2.0);
        // W = w_sum / (Z · p̂) = 4 / (2 · p̂).
        assert!((r.contribution - 2.0 / s.target()).abs() < 1e-6);
    }

    #[test]
    fn merge_uses_the_other_contribution_count_and_normalizes() {
        let s = sample();
        let mut a = Reservoir::default();
        a.update(s, 3.0, 2.0, 0.0);
        a.finish(s.target(), 2.0);
        let mut b = Reservoir::default();
        b.merge(&a, s.target(), 0.0);
        assert_eq!(b.weight_sum, 3.0);
        assert_eq!(b.count, 2.0);
        b.finish(s.target(), 2.0);
        assert!((b.contribution - a.contribution).abs() < 1e-6);
    }

    #[test]
    fn caps_rescale_weight_and_zero_targets_never_create_nan() {
        let mut r = Reservoir::default();
        r.update(sample(), 8.0, 40.0, 0.0);
        r.cap_count(30.0);
        assert_eq!(r.count, 30.0);
        assert_eq!(r.weight_sum, 6.0);
        r.finish(0.0, 30.0);
        assert_eq!(r.contribution, 0.0);
        r.finish(f32::NAN, 30.0);
        assert_eq!(r.contribution, 0.0);
    }

    #[test]
    fn work_counters_saturate() {
        let mut a = RestirGiWork {
            rays: u64::MAX,
            ..RestirGiWork::default()
        };
        a.accumulate(RestirGiWork {
            rays: 1,
            initial_rays: 2,
            ..RestirGiWork::default()
        });
        assert_eq!(a.rays, u64::MAX);
        assert_eq!(a.initial_rays, 2);
        a.accumulate(RestirGiWork {
            motion_rejections: 4,
            ..RestirGiWork::default()
        });
        assert_eq!(a.motion_rejections, 4);
    }

    #[test]
    fn tangential_instance_translation_rejects_even_when_surface_still_overlaps() {
        // A plane translated along X still occupies the old world hit. Its material point moved.
        let local_hit = Vec3::new(2.0, 0.0, -1.0);
        let previous_transform = regia_core::glam::Mat4::from_translation(Vec3::Y);
        let current_transform = regia_core::glam::Mat4::from_translation(Vec3::Y + Vec3::X * 0.25);
        let stored = previous_transform.transform_point3(local_hit);
        let moved_material_point = current_transform.transform_point3(local_hit);
        assert!(!hit_motion_agrees(stored, 7, Some((moved_material_point, 7))));
        let shader = include_str!("restir_gi.wgsl");
        assert!(shader.contains("intersection.object_to_world * previous.object_hit"));
        assert!(shader.contains("(*r).object_hit = sample.object_hit;"));
    }

    #[test]
    fn hit_motion_keeps_sky_and_a_still_surface_and_drops_a_moved_one() {
        let stored = Vec3::new(1.0, 0.4, -0.2);
        assert!(hit_motion_agrees(stored, 0, None));
        assert!(hit_motion_agrees(stored, 3, Some((stored, 3))));
        assert!(hit_motion_agrees(
            stored,
            3,
            Some((stored + Vec3::Y * (HIT_MOTION_METRES - 0.001), 3))
        ));
        assert!(!hit_motion_agrees(
            stored,
            3,
            Some((stored + Vec3::Y * (HIT_MOTION_METRES + 0.001), 3))
        ));
        assert!(!hit_motion_agrees(stored, 3, Some((stored, 4))));
        assert!(!hit_motion_agrees(stored, 3, None));
        assert!(!hit_motion_agrees(Vec3::splat(f32::NAN), 3, Some((stored, 3))));
        assert!((HIT_MOTION_METRES - 0.05).abs() < f32::EPSILON);
        let shader = include_str!("restir_gi.wgsl");
        assert!(
            shader.contains("const GI_HIT_MOTION_M: f32 = 0.05;"),
            "shader threshold drifted from HIT_MOTION_METRES"
        );
        assert!(shader.contains("GI_STAT_MOTION_REJECT"));
    }
}
