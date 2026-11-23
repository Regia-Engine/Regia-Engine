// SPDX-License-Identifier: MPL-2.0
//! Ribbons / trails: camera-facing strips following a moving point.
//!
//! A [`TrailRig`] keeps a short history of its source's positions (a ring of samples, dropped when
//! older than `lifetime`) and rebuilds a triangle-list strip each frame on the CPU — a few hundred
//! vertices per trail, negligible next to the particle simulation, and it keeps the GPU side a
//! single trivial pipeline. The strip is camera-facing (offset along `camera_right`).

use std::collections::VecDeque;

use regia_core::math::Vec3;

use crate::gpu::TrailVertex;

/// How a trail looks.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrailStyle {
    /// Ribbon half-width at the head, metres.
    pub width: f32,
    /// Seconds a sample stays in the ribbon.
    pub lifetime: f32,
    /// Linear RGBA tint (head = full alpha, tail = 0).
    pub color: [f32; 4],
    /// Metres between recorded samples (smaller = smoother, more vertices).
    pub min_segment: f32,
}

impl Default for TrailStyle {
    fn default() -> Self {
        Self {
            width: 0.05,
            lifetime: 0.5,
            color: [1.0, 0.9, 0.6, 0.9],
            min_segment: 0.05,
        }
    }
}

/// One trail following one point.
#[derive(Debug)]
pub struct TrailRig {
    style: TrailStyle,
    samples: VecDeque<(Vec3, f32)>,
    last_pushed: Option<Vec3>,
    last_position: Option<Vec3>,
    total_distance: f32,
}

impl TrailRig {
    /// Moves every stored sample by `-shift` (the floating-origin step of spec 78 § 12.2). A rig is
    /// game-owned, so the game calls this on `OriginShifted`; the distance walked stays.
    pub fn shift_origin(&mut self, shift: Vec3) {
        for (point, _) in &mut self.samples {
            *point -= shift;
        }
        if let Some(point) = &mut self.last_pushed {
            *point -= shift;
        }
        if let Some(point) = &mut self.last_position {
            *point -= shift;
        }
    }

    /// A rig with `style`.
    pub fn new(style: TrailStyle) -> Self {
        Self {
            style,
            samples: VecDeque::new(),
            last_pushed: None,
            last_position: None,
            total_distance: 0.0,
        }
    }

    /// This trail's style.
    pub fn style(&self) -> &TrailStyle {
        &self.style
    }

    /// Distance the source has travelled since the rig started.
    pub fn total_distance(&self) -> f32 {
        self.total_distance
    }

    /// Advances the trail by `dt` with the source at `position`.
    pub fn update(&mut self, position: Vec3, dt: f32) {
        // A non-finite step would poison every sample age and stop expiry for good.
        let dt = if dt.is_finite() { dt } else { 0.0 };
        for sample in &mut self.samples {
            sample.1 += dt;
        }
        while let Some(front) = self.samples.front() {
            // A NaN age compares false against everything; drop it rather than stall expiry.
            if front.1.is_nan() || front.1 > self.style.lifetime {
                self.samples.pop_front();
            } else {
                break;
            }
        }
        let push = match self.last_pushed {
            None => true,
            Some(last) => last.distance(position) >= self.style.min_segment,
        };
        if push {
            self.samples.push_back((position, 0.0));
            self.last_pushed = Some(position);
        } else if let Some(back) = self.samples.back_mut() {
            // keep the head pinned to the source between samples
            back.0 = position;
        }
        if let Some(previous) = self.last_position {
            self.total_distance += previous.distance(position);
        }
        self.last_position = Some(position);
    }

    /// Number of recorded samples.
    pub fn sample_count(&self) -> usize {
        self.samples.len()
    }

    /// Debug: `(age, position)` of every sample (tests).
    pub fn debug_samples(&self) -> Vec<(f32, Vec3)> {
        self.samples.iter().map(|(p, a)| (*a, *p)).collect()
    }

    /// Appends this trail's strip vertices (triangle list) to `out`.
    ///
    /// `camera_right` makes the ribbon face the camera; `head_color` overrides the tint (e.g. a
    /// tracer's muzzle colour), scaled by the per-sample fade.
    pub fn build_vertices(&self, out: &mut Vec<TrailVertex>, camera_right: Vec3, head_color: [f32; 4]) {
        if self.samples.len() < 2 {
            return;
        }
        let right = camera_right.normalize_or_zero();
        for i in 0..self.samples.len() - 1 {
            let (pa, aa) = self.samples[i];
            let (pb, ab) = self.samples[i + 1];
            let fa = 1.0 - (aa / self.style.lifetime).clamp(0.0, 1.0);
            let fb = 1.0 - (ab / self.style.lifetime).clamp(0.0, 1.0);
            // The ribbon keeps a width floor toward the tail (0.35 × width): a width that
            // tapers to zero makes most of the trail sub-pixel. Alpha fades the tail instead.
            let wa = self.style.width * (0.35 + 0.65 * fa);
            let wb = self.style.width * (0.35 + 0.65 * fb);
            let (ta, tb) = (aa / self.style.lifetime, ab / self.style.lifetime);
            let va = |pos: Vec3, side: f32, t: f32, fade: f32, width: f32| TrailVertex {
                position: (pos + right * (side * width)).to_array(),
                _pad: 0.0,
                uv: [t, side],
                _pad2: [0.0; 2],
                color: [
                    head_color[0] * fade,
                    head_color[1] * fade,
                    head_color[2] * fade,
                    head_color[3] * fade,
                ],
            };
            let a_l = va(pa, -1.0, ta, fa, wa);
            let a_r = va(pa, 1.0, ta, fa, wa);
            let b_l = va(pb, -1.0, tb, fb, wb);
            let b_r = va(pb, 1.0, tb, fb, wb);
            out.extend_from_slice(&[a_l, b_l, a_r, a_r, b_l, b_r]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_expire_with_age() {
        let mut rig = TrailRig::new(TrailStyle {
            lifetime: 0.2,
            min_segment: 0.0,
            ..Default::default()
        });
        rig.update(Vec3::ZERO, 0.1);
        rig.update(Vec3::new(1.0, 0.0, 0.0), 0.1);
        assert_eq!(rig.sample_count(), 2);
        rig.update(Vec3::new(2.0, 0.0, 0.0), 0.05);
        assert_eq!(rig.sample_count(), 3);
        rig.update(Vec3::new(3.0, 0.0, 0.0), 0.3);
        assert!(rig.sample_count() <= 2, "old samples retired");
    }

    #[test]
    fn non_finite_dt_does_not_stall_expiry() {
        let mut rig = TrailRig::new(TrailStyle {
            lifetime: 0.2,
            min_segment: 0.0,
            ..Default::default()
        });
        rig.update(Vec3::ZERO, 0.1);
        rig.update(Vec3::X, f32::NAN);
        rig.update(Vec3::new(2.0, 0.0, 0.0), f32::INFINITY);
        assert!(rig.debug_samples().iter().all(|(age, _)| age.is_finite()));
        rig.update(Vec3::new(3.0, 0.0, 0.0), 0.5);
        rig.update(Vec3::new(4.0, 0.0, 0.0), 0.5);
        assert_eq!(rig.sample_count(), 1, "old samples still retire");
    }

    #[test]
    fn segments_respect_min_distance() {
        let mut rig = TrailRig::new(TrailStyle {
            min_segment: 1.0,
            ..Default::default()
        });
        rig.update(Vec3::ZERO, 1.0 / 60.0);
        rig.update(Vec3::new(0.1, 0.0, 0.0), 1.0 / 60.0); // too close → same sample, pinned
        assert_eq!(rig.sample_count(), 1);
        assert_eq!(rig.samples.back().map(|s| s.0), Some(Vec3::new(0.1, 0.0, 0.0)));
    }

    #[test]
    fn strip_builds_triangle_list_vertices() {
        let mut rig = TrailRig::new(TrailStyle {
            min_segment: 0.0,
            ..Default::default()
        });
        rig.update(Vec3::ZERO, 1.0 / 60.0);
        rig.update(Vec3::new(0.2, 0.0, 0.0), 1.0 / 60.0);
        rig.update(Vec3::new(0.4, 0.0, 0.0), 1.0 / 60.0);
        let mut out = Vec::new();
        rig.build_vertices(&mut out, Vec3::new(0.0, 0.0, 1.0), [1.0, 1.0, 1.0, 1.0]);
        assert_eq!(out.len(), 12, "two segments × 6 vertices");
        // vertices are offset around the source line by ±width along camera right (z here)
        assert!(out.iter().any(|v| v.position[2] > 0.0));
        assert!(out.iter().any(|v| v.position[2] < 0.0));
        // head vertices are fully faded in, tail faded out
        let alphas: Vec<f32> = out.iter().map(|v| v.color[3]).collect();
        assert!(alphas.iter().cloned().fold(0.0f32, f32::max) > 0.9);
    }

    #[test]
    fn short_trails_build_nothing() {
        let mut rig = TrailRig::new(Default::default());
        rig.update(Vec3::ZERO, 1.0 / 60.0);
        let mut out = Vec::new();
        rig.build_vertices(&mut out, Vec3::X, [1.0; 4]);
        assert!(out.is_empty());
    }

    #[test]
    fn total_distance_counts_travel_not_pinned_gap() {
        let mut rig = TrailRig::new(TrailStyle {
            min_segment: 1.0,
            ..Default::default()
        });
        rig.update(Vec3::ZERO, 0.01);
        rig.update(Vec3::new(0.4, 0.0, 0.0), 0.01);
        // The source now sits still under `min_segment`; no further travel.
        for _ in 0..10 {
            rig.update(Vec3::new(0.4, 0.0, 0.0), 0.01);
        }
        assert!(
            (rig.total_distance() - 0.4).abs() < 1e-5,
            "{}",
            rig.total_distance()
        );
    }
}
