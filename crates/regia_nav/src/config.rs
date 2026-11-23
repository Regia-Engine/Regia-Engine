// SPDX-License-Identifier: MPL-2.0
//! Navmesh build and agent parameters (`docs/specs/22-tier2.md` § T2.2).
//!
//! [`NavAgentParams`] describes the creature the mesh is built for (radius, standing height, step
//! and slope), while [`NavMeshBuildParams`] describes the voxelisation the [`crate::NavMeshBuilder`]
//! uses to turn level geometry into a walkable surface.

use regia_core::math::det_cos;
use regia_ecs::attrs::Range;
use regia_ecs::prelude::*;

/// Heights, in metres, that [`crate::NavQuery::locate_body`] tries under a body origin before it
/// gives up: the feet, then 0.9 m down, then 1.8 m down. [`crate::NavQuery::find_nearest`] does
/// not use this list.
pub const MESH_LOCATE_DROPS: [f32; 3] = [0.0, -0.9, -1.8];

/// Pilot foot radius (metres).
pub const PILOT_RADIUS: f32 = 0.35;

/// Pilot standing height (metres).
pub const PILOT_HEIGHT: f32 = 1.8;

/// Pilot step, up and down (metres). [`NavAgentParams::max_climb`] is one limit for both.
pub const PILOT_STEP: f32 = 0.55;

/// Hover-hull step, up and down (metres).
pub const HULL_STEP: f32 = 4.0;

/// A hover hull's standing height is its radius plus this clearance (metres).
pub const HOVER_HEIGHT_EXTRA: f32 = 2.0;

/// Extra side clearance a hover bake erodes, beyond the hull radius (metres). The passage must be
/// at least `2 * (radius + HULL_SIDE_CLEARANCE)` wide or the hull has no route through it.
pub const HULL_SIDE_CLEARANCE: f32 = 1.0;

/// The dimensions and movement limits of the agent a navmesh is built for.
///
/// These mirror the terms in `specs/22` § T2.2: `radius` erodes the walkable surface away from
/// walls, `height` is the clearance a span must have above it to be walkable, `max_climb` is the
/// largest step the agent can climb between adjacent cells, and `max_slope` is the steepest
/// surface it can stand on (radians, measured from horizontal).
#[derive(Reflect, Debug, Clone, PartialEq)]
#[reflect(Default)]
pub struct NavAgentParams {
    /// Collision radius of the agent (metres); the walkable surface is inset by this amount.
    /// A hover hull ([`Self::hover`]) also insets by [`HULL_SIDE_CLEARANCE`].
    #[reflect(@Range(0.0..=16.0))]
    pub radius: f32,
    /// Standing height of the agent (metres); a span needs this much headroom to be walkable.
    /// Must be positive (the builder refuses 0).
    #[reflect(@Range(0.01..=24.0))]
    pub height: f32,
    /// The highest vertical step the agent can climb (metres), up or down.
    #[reflect(@Range(0.0..=8.0))]
    pub max_climb: f32,
    /// The steepest walkable slope angle (radians, from horizontal), below pi/2 (the builder
    /// refuses pi/2 and more).
    #[reflect(@Range(0.0..=1.57))]
    pub max_slope: f32,
}

impl Default for NavAgentParams {
    fn default() -> Self {
        Self {
            radius: 0.35,
            height: 1.6,
            max_climb: 0.5,
            max_slope: 45f32.to_radians(),
        }
    }
}

impl NavAgentParams {
    /// A humanoid-scale agent: 0.35 m radius, 1.6 m tall, 0.5 m step, 45° slope.
    pub const HUMANOID: Self = Self {
        radius: 0.35,
        height: 1.6,
        max_climb: 0.5,
        max_slope: std::f32::consts::FRAC_PI_4,
    };

    /// The minimum y-component of a surface normal that is still within `max_slope`. From the
    /// software trig so the slope test compares against the same bits on every target (audit
    /// sprint-audit-g-nav-determinism L6): for the default 45° this is exactly `FRAC_1_SQRT_2`
    /// (`0x3f3504f3`), and a 45° ramp's normal is accepted (`>=`) everywhere.
    pub fn min_normal_y(&self) -> f32 {
        det_cos(self.max_slope)
    }

    /// The on-foot pilot: radius [`PILOT_RADIUS`], height [`PILOT_HEIGHT`], step [`PILOT_STEP`],
    /// slope 45°.
    pub fn pilot() -> Self {
        Self {
            radius: PILOT_RADIUS,
            height: PILOT_HEIGHT,
            max_climb: PILOT_STEP,
            max_slope: std::f32::consts::FRAC_PI_4,
        }
    }

    /// An agent of `radius` that still walks the pilot step. At radius 1.5 m or below the height
    /// stays [`PILOT_HEIGHT`]; above that the height is the radius plus [`HOVER_HEIGHT_EXTRA`].
    pub fn for_radius(radius: f32) -> Self {
        Self {
            radius,
            height: if radius <= 1.5 {
                PILOT_HEIGHT
            } else {
                radius + HOVER_HEIGHT_EXTRA
            },
            max_climb: PILOT_STEP,
            max_slope: std::f32::consts::FRAC_PI_4,
        }
    }

    /// A hover hull: height is the radius plus [`HOVER_HEIGHT_EXTRA`], step [`HULL_STEP`], slope
    /// 45°. The bake erodes an extra [`HULL_SIDE_CLEARANCE`] on each side.
    pub fn hover(radius: f32) -> Self {
        Self {
            radius,
            height: radius + HOVER_HEIGHT_EXTRA,
            max_climb: HULL_STEP,
            max_slope: std::f32::consts::FRAC_PI_4,
        }
    }

    /// Whether this agent is [`Self::pilot`] (radius, height and step; the slope may differ).
    pub fn is_pilot(&self) -> bool {
        self.radius == PILOT_RADIUS && self.height == PILOT_HEIGHT && self.max_climb == PILOT_STEP
    }

    /// Whether this agent is a [`Self::hover`] hull: step [`HULL_STEP`] and height equal to the
    /// radius plus [`HOVER_HEIGHT_EXTRA`].
    pub fn is_hover_hull(&self) -> bool {
        self.max_climb == HULL_STEP && self.height == self.radius + HOVER_HEIGHT_EXTRA
    }

    /// The distance the walkable surface is inset. A hover hull adds [`HULL_SIDE_CLEARANCE`] so a
    /// passage narrower than `2 * (radius + HULL_SIDE_CLEARANCE)` does not survive. Every other
    /// agent insets by `radius` alone, which is what the single-surface bake has always done.
    pub fn erosion_radius(&self) -> f32 {
        if self.is_hover_hull() {
            self.radius + HULL_SIDE_CLEARANCE
        } else {
            self.radius
        }
    }
}

/// Parameters for turning level geometry into a navmesh (voxelisation settings).
///
/// `cell_size` is the horizontal voxel size; a smaller cell gives a finer mesh at more cost.
/// `cell_height` is not a vertical voxel: rasterization stores each triangle's plane height, and
/// two spans in one column merge when the gap between them is at most `cell_height / 2`.
/// `border_size` pads the mesh's world bounds so agents can navigate up to the edge of the
/// geometry without falling off the grid.
#[derive(Reflect, Debug, Clone, PartialEq)]
#[reflect(Default)]
pub struct NavMeshBuildParams {
    /// Horizontal voxel size (metres).
    #[reflect(@Range(0.05..=2.0))]
    pub cell_size: f32,
    /// Vertical merge gap (metres). Stacked spans in one column merge when the gap is at most half
    /// of this. Surface heights stay on the source triangles; they are not snapped to a grid of
    /// this size.
    #[reflect(@Range(0.05..=2.0))]
    pub cell_height: f32,
    /// Extra margin around the geometry bounds (metres).
    #[reflect(@Range(0.0..=16.0))]
    pub border_size: f32,
    /// The agent the mesh is built for.
    pub agent: NavAgentParams,
}

impl Default for NavMeshBuildParams {
    fn default() -> Self {
        Self {
            cell_size: 0.25,
            cell_height: 0.2,
            border_size: 1.0,
            agent: NavAgentParams::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanoid_defaults_are_sensible() {
        let p = NavAgentParams::default();
        assert!(p.radius > 0.0);
        assert!(p.height > p.radius);
        assert!(p.max_climb > 0.0);
        assert!(p.max_slope > 0.0 && p.max_slope < std::f32::consts::FRAC_PI_2);
        assert!((p.min_normal_y() - det_cos(std::f32::consts::FRAC_PI_4)).abs() < 1e-6);
    }

    /// L6 (audit sprint-audit-g-nav-determinism): the default and HUMANOID slope thresholds are
    /// the software cos of 45°, pinned to `FRAC_1_SQRT_2` bits, on every target.
    #[test]
    fn the_default_slope_threshold_is_pinned() {
        for p in [NavAgentParams::default(), NavAgentParams::HUMANOID] {
            assert_eq!(p.max_slope.to_bits(), std::f32::consts::FRAC_PI_4.to_bits());
            assert_eq!(p.min_normal_y().to_bits(), 0x3f35_04f3);
            assert_eq!(
                p.min_normal_y().to_bits(),
                std::f32::consts::FRAC_1_SQRT_2.to_bits()
            );
        }
        // A custom slope goes through the same software cos. This slope is one where the platform
        // libm rounds one ulp above the kernel (`0x3f7ffda2`), so the pin fails under
        // `f32::cos` on this host.
        // `black_box` keeps the slope off the compile-time constant folder: the platform libm
        // call then happens at runtime, where glibc's `cosf` rounds one ulp above the kernel.
        let custom = NavAgentParams {
            max_slope: std::hint::black_box(f32::from_bits(0x3c0b_32b8)),
            ..Default::default()
        };
        assert_eq!(custom.min_normal_y().to_bits(), 0x3f7f_fda2);
        assert_eq!(
            custom.min_normal_y().to_bits(),
            det_cos(custom.max_slope).to_bits()
        );
    }

    #[test]
    fn steeper_slope_requires_a_higher_normal() {
        let shallow = NavAgentParams {
            max_slope: 30f32.to_radians(),
            ..Default::default()
        };
        let steep = NavAgentParams {
            max_slope: 60f32.to_radians(),
            ..Default::default()
        };
        assert!(shallow.min_normal_y() > steep.min_normal_y());
    }

    #[test]
    fn build_params_defaults() {
        let p = NavMeshBuildParams::default();
        assert!(p.cell_size > 0.0 && p.cell_height > 0.0);
        assert!(p.border_size >= 0.0);
    }

    #[test]
    fn pilot_hover_and_for_radius_match_the_hull_table() {
        let pilot = NavAgentParams::pilot();
        assert!(pilot.is_pilot());
        assert!(!pilot.is_hover_hull());
        assert_eq!(pilot.erosion_radius(), PILOT_RADIUS);
        assert!(!NavAgentParams::default().is_pilot());
        assert!(!NavAgentParams::HUMANOID.is_hover_hull());

        let walker = NavAgentParams::for_radius(1.5);
        assert_eq!(walker.height, PILOT_HEIGHT);
        assert_eq!(walker.max_climb, PILOT_STEP);
        assert!(!walker.is_hover_hull());
        assert_eq!(walker.erosion_radius(), 1.5);

        let wide = NavAgentParams::for_radius(2.5);
        assert_eq!(wide.height, 2.5 + HOVER_HEIGHT_EXTRA);
        assert_eq!(wide.max_climb, PILOT_STEP);
        assert!(!wide.is_hover_hull());
        assert_eq!(wide.erosion_radius(), 2.5);

        for radius in [2.5_f32, 3.6, 4.5] {
            let hull = NavAgentParams::hover(radius);
            assert!(hull.is_hover_hull());
            assert!(!hull.is_pilot());
            assert_eq!(hull.height, radius + HOVER_HEIGHT_EXTRA);
            assert_eq!(hull.max_climb, HULL_STEP);
            assert_eq!(hull.erosion_radius(), radius + HULL_SIDE_CLEARANCE);
        }
    }
}
