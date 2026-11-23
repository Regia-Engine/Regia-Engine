// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-core/src/hover_ground.rs, commit e6c84ba6d, tag regia-absorb-2), ADR-8012.
// Lines 1-214 (162 code, 163-214 its tests), mark: adapt (Stage A: unchanged). With it, at the same tag:
// ir-sim/src/attitude.rs 108-111 (`ground_tilt`, as-is) and ir-sim/src/ruins.rs 79-100 (`crater`,
// `height_delta`, as-is, in the fixture crater book).
// Edits: module paths, doc comments (DO27 file, lane and engine names dropped, missing docs added); the
// spec 121 § 4.2 seams (`HoverGroundBody` for the runtime entity, `GroundDelta` for the ruin book,
// `TiltParams` for the three handling constants, `regia_detsim::math` for the trig, the source's
// canonical `atan2` steps in `compat::source_math`); float min/max/clamp
// through `regia_detsim::float::det_*` (ADR-8166).

//! The hover-hull ground rules, one implementation for host and client.
//!
//! Two rules sit on the host's ground pass: the ledge rule (a hover hull keeps its world height when
//! the floor under it changes by more than a step, on every map, not just tunnels) and the wall rule
//! (a hull whose body meets a face that rises more than [`HULL_WALL_M`] in one tick goes back to its
//! last position and loses its horizontal speed). A client predictor that ran the hover chain but not
//! these rules would see the hull a tick ahead of the host near a wall or a ledge.
//!
//! Both sides step hulls through [`ground_hover_hull`] and sample their floors through
//! [`sample_move_floor`] here, so the rules and the floors they judge cannot drift apart.
//! Deterministic (lockstep-safe): pure functions of their arguments, no RNG, no clock, no
//! iteration over unordered state.
//!
//! The body is reached through [`HoverGroundBody`] (accessors only, spec 121 § 4.3), the terrain
//! deformation through [`GroundDelta`] and the host's cushion-tilt tuning through [`TiltParams`].

use regia_detsim::float::{det_clamp, det_max};
use regia_detsim::math;

use crate::compat::sampler::{FloorSurface, TerrainSampler, sample_floor};
use crate::compat::source_math;
use crate::compat::state::{TunnelsState, tunnels_arm_active};

/// A change of floor under a body bigger than this in one tick is a ledge: the body keeps its height.
pub const LEDGE_M: f32 = 0.75;
/// A hull cannot climb a face that rises more than this above its body in one tick (the funnel walls,
/// a shaft rim): it stops against it instead of being set on top. Authored ramps rise under 0.5 m a tick.
pub const HULL_WALL_M: f32 = 2.5;

/// What [`ground_hover_hull`] reads and writes on a body (Stage A seam for the host's runtime entity,
/// spec 121 § 4.3). Accessors only; the call order of the copied function is unchanged.
pub trait HoverGroundBody {
    /// True for a body on foot (no wall rule, no tilt).
    fn on_foot(&self) -> bool;
    /// World position, metres.
    fn position(&self) -> [f32; 3];
    /// Sets the world position.
    fn set_position(&mut self, p: [f32; 3]);
    /// Hover altitude over the floor, metres.
    fn altitude(&self) -> f32;
    /// Sets the altitude.
    fn set_altitude(&mut self, a: f32);
    /// Sets the horizontal velocity components (x and z) to 0 and leaves the vertical one; the copied
    /// function only ever writes 0 there and never reads velocity.
    fn stop_horizontal(&mut self);
    /// Vertical velocity of the hover spring.
    fn vertical_vel(&self) -> f32;
    /// Sets the vertical velocity.
    fn set_vertical_vel(&mut self, v: f32);
    /// Sets the floor the spring measured last.
    fn set_prev_terrain(&mut self, y: Option<f32>);
    /// Heading, radians.
    fn yaw(&self) -> f32;
    /// Sets the ground pitch and roll under the hull, radians.
    fn set_ground_tilt(&mut self, pitch: f32, roll: f32);
}

/// A height added to the heightfield floor at (x, z) (Stage A: the host's crater book; removed in
/// Stage B by lane W-12). Never added to a mesh floor.
pub trait GroundDelta {
    /// The added height, metres (negative inside a crater).
    fn height_delta(&self, x: f32, z: f32) -> f32;
}

/// The [`GroundDelta`] of a world with no craters: `0.0` everywhere (the bits an empty crater book adds).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NoGroundDelta;

impl GroundDelta for NoGroundDelta {
    fn height_delta(&self, _x: f32, _z: f32) -> f32 {
        0.0
    }
}

/// The host's cushion-tilt tuning the tilt pass reads (spec 121 § 4.3: host handling values, passed
/// in, never engine constants).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TiltParams {
    /// The tilt sample span: the ground is read this far ahead, behind, right and left of the hull
    /// centre, metres.
    pub span_m: f32,
    /// The steepest ground tilt the cushion reads, radians: past it a sample is a wall or a ledge.
    pub max: f32,
    /// Height difference (metres) between a tilt sample and the floor under the hull past which that
    /// sample is a ledge, ignored.
    pub ledge_m: f32,
}

/// Floor Y under a body whose origin (a hull's hover point, a pilot's feet) is at `body_y`, for host
/// movement and client prediction.
///
/// T3: the one surface query, asked at the body's waist ([`TunnelsState::move_floor`]). Flag off or
/// no mesh: the heightfield, bit-identical to `sample_floor`. No last floor goes in: the answer is a
/// pure function of where the body is. Craters deform the terrain layer only, so a crater on the
/// canyon floor does not dip the tube under it.
///
/// Lives here so the predictor samples exactly the floor the host judges a hull against.
pub fn sample_move_floor<N>(
    terrain: Option<&TerrainSampler>,
    tunnels: &TunnelsState<N>,
    ruins: &impl GroundDelta,
    x: f32,
    z: f32,
    body_y: f32,
) -> f32 {
    if tunnels_arm_active(tunnels.enabled, tunnels.mesh.as_ref()) {
        let floor = tunnels.move_floor(terrain, x, z, body_y);
        match floor.surface {
            FloorSurface::Heightfield => floor.point[1] + ruins.height_delta(x, z),
            FloorSurface::TunnelFloor => floor.point[1],
        }
    } else {
        sample_floor(terrain, x, z, body_y, FloorSurface::Heightfield).point[1] + ruins.height_delta(x, z)
    }
}

/// One ground step for a hull the hover chain moved this tick: the rules the host's ground pass
/// applies, shared with the client predictor.
///
/// * Wall rule: a hull whose body meets a floor rising more than [`HULL_WALL_M`] above it in one
///   tick goes back to `last_pos` (its position before this tick's move) and loses its horizontal
///   speed. `last_pos` is `None` on the hull's first tick (the host has no audited last position
///   yet) and then the rule waits a tick.
/// * Ledge rule: when the floor under the hull changed by more than [`LEDGE_M`] in one tick the
///   hull keeps its world height (`altitude` becomes the height over the new floor) and the
///   spring falls, or a floor over it lifts it. Otherwise the hull grounds at its altitude.
///
/// Callers: the host's ground pass (hulls) and the joiner's predictor. Reads the position the mover
/// left (`floor_pre + altitude`) exactly like the host pass, so both sides compute the same ridden
/// floor (`hint_y - altitude`, for the ledge rule) bit for bit.
///
/// Returns the floor the hull stands on (for tunnel confinement).
pub fn ground_hover_hull<N>(
    entity: &mut impl HoverGroundBody,
    last_pos: Option<[f32; 3]>,
    terrain: Option<&TerrainSampler>,
    tunnels: &TunnelsState<N>,
    ruins: &impl GroundDelta,
    tilt: &TiltParams,
) -> f32 {
    let on_foot = entity.on_foot();
    let hint_y = entity.position()[1];
    let altitude = entity.altitude();
    let prev_floor = hint_y - altitude;
    let mut floor_y = sample_move_floor(
        terrain,
        tunnels,
        ruins,
        entity.position()[0],
        entity.position()[2],
        hint_y,
    );
    // The wall rule: a hull whose body meets a face rising over HULL_WALL_M stops against it.
    if !on_foot
        && floor_y - hint_y > HULL_WALL_M
        && let Some(last) = last_pos
    {
        floor_y = sample_move_floor(terrain, tunnels, ruins, last[0], last[2], hint_y);
        let mut position = entity.position();
        position[0] = last[0];
        position[2] = last[2];
        entity.set_position(position);
        entity.stop_horizontal();
    }
    if (floor_y - prev_floor).abs() > LEDGE_M {
        // The ledge rule on any map. Re-grounding at the old altitude once carried a hull 11 m up
        // the far wall of a shaft in one tick (68 m over the pit floor). The body keeps its world
        // height and falls (or is lifted by a floor over it), as the hover step does with ground
        // that drops away.
        let lifted = det_max(hint_y - floor_y, 0.0);
        entity.set_altitude(lifted);
        entity.set_prev_terrain(Some(floor_y));
        if lifted == 0.0 {
            entity.set_vertical_vel(det_max(entity.vertical_vel(), 0.0));
        }
        let mut position = entity.position();
        position[1] = floor_y + lifted;
        entity.set_position(position);
    } else {
        let mut position = entity.position();
        position[1] = floor_y + entity.altitude();
        entity.set_position(position);
    }
    if !on_foot {
        let (pitch, roll) = ground_tilt_under(entity, floor_y, terrain, tunnels, ruins, tilt);
        entity.set_ground_tilt(pitch, roll);
    }
    floor_y
}

/// The ground's pitch and roll under a hull (radians; pitch positive when the ground rises ahead, roll
/// positive when it rises on the right), from four floor samples [`TiltParams::span_m`] ahead, behind,
/// right and left of the hull centre along its heading. A sample more than [`TiltParams::ledge_m`] off
/// the floor under the hull is a ledge or a wall, not a slope: that side reads the centre floor
/// instead. Deterministic (the same floor sampler and `regia_detsim::math` trig as the step).
pub fn ground_tilt_under<N>(
    entity: &impl HoverGroundBody,
    floor_y: f32,
    terrain: Option<&TerrainSampler>,
    tunnels: &TunnelsState<N>,
    ruins: &impl GroundDelta,
    tilt: &TiltParams,
) -> (f32, f32) {
    ground_tilt_at(
        entity.position(),
        entity.yaw(),
        floor_y,
        terrain,
        tunnels,
        ruins,
        tilt,
    )
}

/// [`ground_tilt_under`] for a body at `pos` (world, its height as the floor hint) facing `yaw`, over
/// the floor `floor_y`.
pub fn ground_tilt_at<N>(
    pos: [f32; 3],
    yaw: f32,
    floor_y: f32,
    terrain: Option<&TerrainSampler>,
    tunnels: &TunnelsState<N>,
    ruins: &impl GroundDelta,
    tilt: &TiltParams,
) -> (f32, f32) {
    let [x, hint_y, z] = pos;
    let (sin_yaw, cos_yaw) = (math::sin(yaw), math::cos(yaw));
    let span = tilt.span_m;
    let at = |dx: f32, dz: f32| {
        let y = sample_move_floor(terrain, tunnels, ruins, x + dx, z + dz, hint_y);
        if (y - floor_y).abs() > tilt.ledge_m {
            floor_y
        } else {
            y
        }
    };
    let front = at(sin_yaw * span, cos_yaw * span);
    let back = at(-sin_yaw * span, -cos_yaw * span);
    let right = at(cos_yaw * span, -sin_yaw * span);
    let left = at(-cos_yaw * span, sin_yaw * span);
    let (pitch, roll) = ground_tilt(front, back, right, left, span);
    (
        det_clamp(pitch, -tilt.max, tilt.max),
        det_clamp(roll, -tilt.max, tilt.max),
    )
}

/// Ground pitch and roll from four height samples taken `span` metres from the hull centre along its
/// own axes. Pitch is positive when the ground rises ahead; roll is positive when it rises on the right.
pub fn ground_tilt(h_front: f32, h_back: f32, h_right: f32, h_left: f32, span: f32) -> (f32, f32) {
    let run = det_max(2.0 * span, 1.0e-3);
    (
        source_math::atan2(h_front - h_back, run),
        source_math::atan2(h_right - h_left, run),
    )
}

/// A crater book as the host keeps one (test fixture; the host's own book implements [`GroundDelta`]).
#[cfg(any(test, feature = "fixtures"))]
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CraterBook {
    /// The craters, in the order they were made.
    pub craters: Vec<Crater>,
}

/// One crater: a flat disc lowered by `depth`.
#[cfg(any(test, feature = "fixtures"))]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Crater {
    /// Centre x, metres.
    pub x: f32,
    /// Centre z, metres.
    pub z: f32,
    /// Radius, metres.
    pub radius: f32,
    /// Depth, metres.
    pub depth: f32,
}

#[cfg(any(test, feature = "fixtures"))]
impl CraterBook {
    /// A shell crater at `(x, z)`: 3 m across radius, 0.45 m deep.
    pub fn crater(&mut self, x: f32, z: f32) {
        self.craters.push(Crater {
            x,
            z,
            radius: 3.0,
            depth: 0.45,
        });
    }
}

#[cfg(any(test, feature = "fixtures"))]
impl GroundDelta for CraterBook {
    /// Added to the heightfield. Negative inside a crater.
    fn height_delta(&self, x: f32, z: f32) -> f32 {
        let mut drop = 0.0f32;
        for crater in &self.craters {
            let dx = x - crater.x;
            let dz = z - crater.z;
            if dx * dx + dz * dz <= crater.radius * crater.radius {
                drop = regia_detsim::float::det_min(drop, -crater.depth);
            }
        }
        drop
    }
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::disallowed_methods,
        reason = "test reference values; DO27's asserts are kept as copied (ADR-8167)"
    )]
    use super::*;
    use crate::compat::sampler::HeightGrid;

    /// The host's tuning the source passes (`CUSHION_TILT_SPAN_M` 3.0, `CUSHION_TILT_MAX` 0.61,
    /// `CUSHION_TILT_LEDGE_M` 2.5).
    const TILT: TiltParams = TiltParams {
        span_m: 3.0,
        max: 0.61,
        ledge_m: 2.5,
    };

    /// A heightfield rising 1 m per sample step (5.08 m, 11.1 degrees) toward +Z, flat across X, 320 m wide.
    fn slope_z() -> TerrainSampler {
        let side = 64;
        let mut heights = Vec::with_capacity(side * side);
        for row in 0..side {
            for _ in 0..side {
                heights.push((16_384 + 32 * row) as u16);
            }
        }
        TerrainSampler::new(HeightGrid::square(side, heights).expect("grid"), 320.0)
    }

    /// The ground tilt the cushion reads, in the hull's own axes.
    #[test]
    fn ground_tilt_reads_the_slope_along_the_hull() {
        let terrain = slope_z();
        let (tunnels, ruins) = (TunnelsState::<()>::disabled(), CraterBook::default());
        let floor = sample_move_floor(Some(&terrain), &tunnels, &ruins, 0.0, 0.0, 1.0);
        let slope = (1.0_f32 / terrain.spacing).atan(); // 1 m per sample step (320 m over 63 steps)
        let tilt = |yaw: f32| {
            ground_tilt_at(
                [0.0, floor + 1.0, 0.0],
                yaw,
                floor,
                Some(&terrain),
                &tunnels,
                &ruins,
                &TILT,
            )
        };
        // Facing +Z (yaw 0): nose up the slope, level across.
        let (p, r) = tilt(0.0);
        assert!(
            (p - slope).abs() < 1.0e-3 && r.abs() < 1.0e-3,
            "facing uphill: pitch {p} roll {r}"
        );
        // Facing -Z: nose down.
        let (p, _) = tilt(core::f32::consts::PI);
        assert!((p + slope).abs() < 1.0e-3, "facing downhill: pitch {p}");
        // Facing +X (yaw pi/2): the right side is -Z, downhill, so the ground falls on the right (negative roll).
        let (p, r) = tilt(core::f32::consts::FRAC_PI_2);
        assert!(
            p.abs() < 1.0e-3 && (r + slope).abs() < 1.0e-3,
            "facing across: pitch {p} roll {r}"
        );
    }

    /// A sample past a ledge (more than the tilt ledge off the floor under the hull) is ignored, not read as a slope.
    #[test]
    fn a_ledge_beside_the_hull_is_not_a_slope() {
        let side = 64;
        let mut heights = vec![16_384_u16; side * side];
        for row in 33..side {
            for col in 0..side {
                heights[row * side + col] = 16_384 + 32 * 10; // a 10 m step a few metres ahead
            }
        }
        let terrain = TerrainSampler::new(HeightGrid::square(side, heights).expect("grid"), 320.0);
        let (tunnels, ruins) = (TunnelsState::<()>::disabled(), CraterBook::default());
        let (p, r) = ground_tilt_at([0.0, 1.0, 2.0], 0.0, 0.0, Some(&terrain), &tunnels, &ruins, &TILT);
        assert!(
            p.abs() < 1.0e-3 && r.abs() < 1.0e-3,
            "ledge read as tilt: pitch {p} roll {r}"
        );
    }
}
