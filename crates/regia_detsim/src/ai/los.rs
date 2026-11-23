// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/ai/influence.rs lines 14-17 and 28-52, commit c9513623f, tag
// regia-absorb-4), ADR-8012. Mark: adapt (plan row S-10, ADR-8395). `heightfield_los_clear` is DO27's `los_clear`
// byte for byte apart from rustfmt, doc comments and its height source: DO27 read `TerrainSampler::height_at_world`
// directly, here the caller passes that function in, and the "no terrain is open ground" branch moved to
// `OpenField` and `Option<L>`. Named change: line of sight goes through the `LineOfSight` trait so a
// world-geometry query (tunnel mouths, lids, blockers) can plug in. `LineOfSight`, `OpenField`, `LosFn` and
// `MouthWall` are new.

//! The line-of-sight seam of the deterministic AI (plan row S-10, ADR-8395).
//!
//! Perception ([`super::SightingsBoard`]) and the influence queries ([`super::flank_bearing`]) never test
//! geometry themselves: they ask a [`LineOfSight`]. The host passes whatever its sim collides against:
//!
//! - [`OpenField`]: nothing blocks (a map with no terrain, or a test).
//! - [`HeightfieldLos`]: a heightfield, sampled every [`LOS_STEP_M`] along the line; the ground blocks when it
//!   rises more than [`LOS_CLEARANCE_M`] over the line. These are Direct Order 27's bits.
//! - A world-geometry query: `regia_world_geom` implements the trait for its terrain sampler (the heightfield
//!   rule above) and for its tunnel collision mesh (rock between two points), so sight follows a tunnel mouth.
//! - [`LosFn`]: any closure, for a host's own query.
//! - [`MouthWall`]: a test double, one cliff face with a tunnel mouth in it.
//!
//! `Option<L>` is a line of sight too (`None` is open field), as is `&L`, so a sim that keeps its terrain as
//! `Option<&Terrain>` passes it as it is.
//!
//! An implementation must be a pure function of the sim state and its two points, with lockstep math only
//! (`crate::math`, `crate::float`; see `cargo xtask det-scan`), so every peer gets the same answer.

/// Step between line-of-sight samples against a heightfield (metres).
pub const LOS_STEP_M: f32 = 4.0;
/// How far above the straight sight line the ground may rise before it blocks (metres).
pub const LOS_CLEARANCE_M: f32 = 2.0;

/// Whether a straight line between two points is unobstructed (positions in sim metres, `y` up).
///
/// Perception asks it once per observer and enemy pair it re-checks, so it runs on the sim tick: it must be
/// deterministic (lockstep math only) and must not allocate on its hot path.
pub trait LineOfSight {
    /// True when nothing blocks the straight line from `from` to `to`.
    fn clear(&self, from: [f32; 3], to: [f32; 3]) -> bool;
}

impl<L: LineOfSight + ?Sized> LineOfSight for &L {
    fn clear(&self, from: [f32; 3], to: [f32; 3]) -> bool {
        (**self).clear(from, to)
    }
}

/// `None` is open field (Direct Order 27's "no heightfield == open ground"); `Some(l)` is `l`.
impl<L: LineOfSight> LineOfSight for Option<L> {
    fn clear(&self, from: [f32; 3], to: [f32; 3]) -> bool {
        match self {
            Some(los) => los.clear(from, to),
            None => true,
        }
    }
}

/// Nothing blocks: every line is clear.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpenField;

impl LineOfSight for OpenField {
    fn clear(&self, _from: [f32; 3], _to: [f32; 3]) -> bool {
        true
    }
}

/// A host's own query as a line of sight: `LosFn(|from, to| my_query(from, to))`.
#[derive(Debug, Clone, Copy)]
pub struct LosFn<F>(pub F);

impl<F: Fn([f32; 3], [f32; 3]) -> bool> LineOfSight for LosFn<F> {
    fn clear(&self, from: [f32; 3], to: [f32; 3]) -> bool {
        (self.0)(from, to)
    }
}

/// A heightfield line of sight: `HeightfieldLos(|x, z| ground_height(x, z))`. See [`heightfield_los_clear`].
#[derive(Debug, Clone, Copy)]
pub struct HeightfieldLos<F>(pub F);

impl<F: Fn(f32, f32) -> f32> LineOfSight for HeightfieldLos<F> {
    fn clear(&self, from: [f32; 3], to: [f32; 3]) -> bool {
        heightfield_los_clear(&self.0, from, to)
    }
}

/// True when the straight line from `from` to `to` is not occluded by the heightfield `height_at(x, z)`.
///
/// Deterministic point samples, no randomness: the line is cut into `ceil(dist_xz / LOS_STEP_M)` equal steps
/// and each inner sample is blocked when the ground there is more than [`LOS_CLEARANCE_M`] above the line.
/// A line no longer than one step in plan is always clear.
pub fn heightfield_los_clear(height_at: impl Fn(f32, f32) -> f32, from: [f32; 3], to: [f32; 3]) -> bool {
    let dx = to[0] - from[0];
    let dz = to[2] - from[2];
    let dist = crate::math::sqrt(dx * dx + dz * dz);
    if dist <= LOS_STEP_M {
        return true;
    }
    let steps = (dist / LOS_STEP_M).ceil() as usize;
    let inv = 1.0 / steps as f32;
    for i in 1..steps {
        let f = i as f32 * inv;
        let x = from[0] + dx * f;
        let z = from[2] + dz * f;
        let line_y = from[1] + (to[1] - from[1]) * f;
        let ground = height_at(x, z);
        if ground > line_y + LOS_CLEARANCE_M {
            return false;
        }
    }
    true
}

/// A test double: an infinite vertical cliff face across `z = wall_z` with one rectangular tunnel mouth in it.
///
/// A line that stays on one side of the face is clear. A line that crosses it is clear only through the open
/// mouth: where it meets the plane, `x` is within `mouth_x` and `y` within `mouth_y` (edges count as open).
/// [`MouthWall::closed`] is the same face with the mouth blocked (a collapse, a door). Plain `+ - * /`, so the
/// answer is the same on every peer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MouthWall {
    /// The plane of the face (sim metres).
    pub wall_z: f32,
    /// The mouth's `[min, max]` x extent.
    pub mouth_x: [f32; 2],
    /// The mouth's `[min, max]` height extent.
    pub mouth_y: [f32; 2],
    /// False when the mouth is blocked.
    pub open: bool,
}

impl MouthWall {
    /// A face at `wall_z` with an open mouth spanning `mouth_x` by `mouth_y`.
    pub fn new(wall_z: f32, mouth_x: [f32; 2], mouth_y: [f32; 2]) -> Self {
        MouthWall {
            wall_z,
            mouth_x,
            mouth_y,
            open: true,
        }
    }

    /// The same face with its mouth blocked.
    pub fn closed(self) -> Self {
        MouthWall { open: false, ..self }
    }
}

impl LineOfSight for MouthWall {
    fn clear(&self, from: [f32; 3], to: [f32; 3]) -> bool {
        let (a, b) = (from[2] - self.wall_z, to[2] - self.wall_z);
        // Only ends strictly on opposite sides cross the face; touching it is not crossing it.
        if !((a < 0.0 && b > 0.0) || (a > 0.0 && b < 0.0)) {
            return true;
        }
        if !self.open {
            return false;
        }
        let t = a / (a - b);
        let x = from[0] + (to[0] - from[0]) * t;
        let y = from[1] + (to[1] - from[1]) * t;
        x >= self.mouth_x[0] && x <= self.mouth_x[1] && y >= self.mouth_y[0] && y <= self.mouth_y[1]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_field_and_none_see_everything() {
        assert!(OpenField.clear([0.0; 3], [1.0e4, -50.0, 1.0e4]));
        assert!(None::<OpenField>.clear([0.0; 3], [1.0, 0.0, 1.0]));
        let blocked = LosFn(|_: [f32; 3], _: [f32; 3]| false);
        assert!(!Some(blocked).clear([0.0; 3], [1.0, 0.0, 1.0]));
        assert!(!(&&blocked).clear([0.0; 3], [1.0, 0.0, 1.0]));
    }

    #[test]
    fn a_line_inside_one_step_is_clear_even_under_a_cliff() {
        let cliff = HeightfieldLos(|_: f32, _: f32| 1000.0);
        assert!(cliff.clear([0.0; 3], [LOS_STEP_M, 0.0, 0.0]));
        assert!(!cliff.clear([0.0; 3], [LOS_STEP_M * 2.0, 0.0, 0.0]));
    }

    #[test]
    fn ground_within_the_clearance_does_not_block() {
        let bump = HeightfieldLos(|_: f32, _: f32| LOS_CLEARANCE_M);
        assert!(bump.clear([0.0; 3], [0.0, 0.0, 100.0]));
        let wall = HeightfieldLos(|_: f32, _: f32| LOS_CLEARANCE_M + 0.01);
        assert!(!wall.clear([0.0; 3], [0.0, 0.0, 100.0]));
    }

    #[test]
    fn the_mouth_edges_are_open_and_the_face_is_two_sided() {
        let m = MouthWall::new(10.0, [-1.0, 1.0], [0.0, 2.0]);
        // Straight through the edge corner, from either side.
        assert!(m.clear([1.0, 2.0, 0.0], [1.0, 2.0, 20.0]));
        assert!(m.clear([1.0, 2.0, 20.0], [1.0, 2.0, 0.0]));
        // Just past the edge.
        assert!(!m.clear([1.01, 1.0, 0.0], [1.01, 1.0, 20.0]));
        assert!(!m.clear([0.0, -0.01, 0.0], [0.0, -0.01, 20.0]));
        // Touching the face from one side does not cross it.
        assert!(m.clear([50.0, 1.0, 0.0], [50.0, 1.0, 10.0]));
        assert!(!m.closed().clear([0.0, 1.0, 0.0], [0.0, 1.0, 20.0]));
    }
}
