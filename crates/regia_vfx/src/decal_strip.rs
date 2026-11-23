// SPDX-License-Identifier: MPL-2.0
//! A rut / trail decal strip: overlapping projected stamps that follow a polyline.
//!
//! Tyre ruts are not a single quad. Each stamp is a short decal whose +Y axis follows the
//! path tangent and whose +Z axis is the surface normal, so a curve stays on the ground
//! instead of cutting a straight chord.

use regia_core::math::Vec3;

use crate::decal::{Decal, DecalSurface, align_along};

/// A path of projected decals.
#[derive(Debug, Clone, PartialEq)]
pub struct DecalStrip {
    /// World-space samples, in order. At least two.
    pub points: Vec<Vec3>,
    /// Surface normal (tyre ruts use world +Y).
    pub normal: Vec3,
    /// Half-width across the path, in metres.
    pub half_width: f32,
    /// Half-depth of the projection volume, in metres.
    pub half_depth: f32,
    /// Distance between stamp centres along the path.
    pub spacing: f32,
    /// Material copied onto every stamp.
    pub surface: DecalSurface,
    /// Lifetime of each stamp (`None` = until the pool recycles it).
    pub lifetime: Option<f32>,
}

/// Most stamps one strip builds; a longer path is truncated here.
pub const MAX_STAMPS: usize = 65_536;

impl DecalStrip {
    /// Builds the stamps. Empty when the path has fewer than two points or any point is not
    /// finite; truncated at [`MAX_STAMPS`].
    pub fn stamps(&self) -> Vec<(Decal, DecalSurface)> {
        if self.points.len() < 2 || self.points.iter().any(|p| !p.is_finite()) {
            return Vec::new();
        }
        let spacing = self.spacing.max(0.05);
        // Each stamp has to reach the next centre. A shorter quad leaves a gap on the
        // outside of a curve, which reads as a dashed line instead of a rut.
        let along = spacing * 1.05;
        let mut out = Vec::new();
        let mut carry = 0.0f32;
        for window in self.points.windows(2) {
            let a = window[0];
            let b = window[1];
            let delta = b - a;
            let len = delta.length();
            if len < 1e-5 {
                continue;
            }
            let dir = delta / len;
            let mut dist = if out.is_empty() && carry == 0.0 {
                0.0
            } else {
                spacing - carry
            };
            if out.is_empty() && carry == 0.0 {
                out.push(self.stamp(a, dir, along));
                dist = spacing;
            }
            while dist <= len && out.len() < MAX_STAMPS {
                out.push(self.stamp(a + dir * dist, dir, along));
                dist += spacing;
            }
            if out.len() >= MAX_STAMPS {
                break;
            }
            carry = len - (dist - spacing);
            if carry < 0.0 {
                carry = 0.0;
            }
        }
        out
    }

    fn stamp(&self, position: Vec3, tangent: Vec3, along: f32) -> (Decal, DecalSurface) {
        let decal = Decal {
            position,
            rotation: align_along(self.normal, tangent),
            half_size: Vec3::new(
                self.half_width.max(0.01),
                along.max(0.01),
                self.half_depth.max(0.01),
            ),
            color: [
                self.surface.albedo[0],
                self.surface.albedo[1],
                self.surface.albedo[2],
                self.surface.opacity,
            ],
            age: 0.0,
            lifetime: self.lifetime,
        };
        (decal, self.surface)
    }
}

/// A quadratic Bezier sampled into `steps` points (inclusive of both ends).
pub fn sample_quadratic(a: Vec3, b: Vec3, c: Vec3, steps: u32) -> Vec<Vec3> {
    let steps = steps.max(1);
    (0..=steps)
        .map(|i| {
            let t = i as f32 / steps as f32;
            let ab = a.lerp(b, t);
            let bc = b.lerp(c, t);
            ab.lerp(bc, t)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decal::DecalSurface;

    #[test]
    fn ruts_follow_the_curve_not_the_chord() {
        let a = Vec3::new(-4.0, 0.0, 2.0);
        let bow = Vec3::new(0.0, 0.0, -2.0);
        let c = Vec3::new(4.0, 0.0, 2.0);
        let points = sample_quadratic(a, bow, c, 16);
        let surface = DecalSurface::from_decal(&Decal {
            position: Vec3::ZERO,
            rotation: regia_core::math::Quat::IDENTITY,
            half_size: Vec3::ONE,
            color: [0.1, 0.08, 0.05, 0.9],
            age: 0.0,
            lifetime: None,
        });
        let strip = DecalStrip {
            points,
            normal: Vec3::Y,
            half_width: 0.2,
            half_depth: 0.1,
            spacing: 0.4,
            surface,
            lifetime: None,
        };
        let stamps = strip.stamps();
        assert!(
            stamps.len() >= 8,
            "a curve needs a run of stamps, got {}",
            stamps.len()
        );
        // The middle of the path bows toward -Z. A straight chord between the ends stays at z=2.
        let mid = stamps[stamps.len() / 2].0.position;
        assert!(
            mid.z < 0.5,
            "middle stamp should sit on the bow, not the chord (z={})",
            mid.z
        );
        // Quadratic t = 0.5 is the midpoint of the control and the chord, at z = 0.
        let chord_mid = (a + c) * 0.5;
        let apex = Vec3::new(0.0, 0.0, 0.0);
        assert!(
            (mid - apex).length() < (mid - chord_mid).length(),
            "middle stamp should sit on the bow (z={}), not the chord at {chord_mid}",
            mid.z
        );
        // Each stamp's +Y follows the path (not all the same direction).
        let first_along = stamps[0].0.rotation * Vec3::Y;
        let last_along = stamps.last().expect("stamps").0.rotation * Vec3::Y;
        assert!(
            first_along.dot(last_along) < 0.95,
            "the strip turns: first {first_along} last {last_along}"
        );
        // Projection axis stays the surface normal.
        let up = stamps[0].0.rotation * Vec3::Z;
        assert!((up - Vec3::Y).length() < 1e-3);
    }
    fn strip_of(points: Vec<Vec3>) -> DecalStrip {
        let d = Decal {
            position: Vec3::ZERO,
            rotation: regia_core::math::Quat::IDENTITY,
            half_size: Vec3::splat(0.1),
            color: [1.0; 4],
            age: 0.0,
            lifetime: None,
        };
        DecalStrip {
            points,
            normal: Vec3::Y,
            half_width: 0.1,
            half_depth: 0.1,
            spacing: 0.05,
            surface: DecalSurface::from_decal(&d),
            lifetime: None,
        }
    }

    #[test]
    fn non_finite_points_build_nothing() {
        let s = strip_of(vec![Vec3::ZERO, Vec3::new(f32::INFINITY, 0.0, 0.0)]);
        assert!(s.stamps().is_empty());
        let s = strip_of(vec![Vec3::ZERO, Vec3::new(f32::NAN, 0.0, 0.0)]);
        assert!(s.stamps().is_empty());
    }

    #[test]
    fn very_long_segment_is_capped() {
        let s = strip_of(vec![Vec3::ZERO, Vec3::new(1.0e6, 0.0, 0.0)]);
        assert_eq!(s.stamps().len(), MAX_STAMPS);
    }
}
