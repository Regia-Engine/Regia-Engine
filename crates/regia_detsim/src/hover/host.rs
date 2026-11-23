// SPDX-License-Identifier: MPL-2.0
//! The host's side of the hover step (plan row mp-hover-host-kit): the floor, the position and the
//! ground attitude that [`step_with`] leaves to its caller, so a host does not
//! rewrite them.
//!
//! The step owns velocity, yaw, attitude and altitude only (see the module's ownership contract).
//! Each tick a host still has to:
//!
//! 1. read the ground's pitch and roll under the hull into [`HoverState::ground_pitch`] and
//!    [`HoverState::ground_roll`] ([`ground_attitude`]): without that pass a hull never tilts with
//!    the ground;
//! 2. read the floor height under the hull and step ([`FloorSampler`]);
//! 3. move the hull: XZ by `velocity * dt`, Y to `floor + altitude` ([`integrate`]).
//!
//! [`HoverFleet`] does the three for every hull of a match in ascending slot order, with no heap
//! allocation per tick. Contacts between hulls are not handled here.
//!
//! Every function is lockstep code: IEEE `+ - * /` in source order, `sin`, `cos` and `atan2` through
//! [`crate::math`], no allocation per tick, no threads, no hash containers.
//!
//! ```
//! use regia_detsim::hover::host::{FleetHull, FleetInput, FloorFn, HoverBody, HoverFleet};
//! use regia_detsim::hover::{ClimbTuning, HoverLimits, HoverParams, WorldPhysics};
//! use regia_detsim::tick::tick_dt;
//!
//! // A floor that rises 1 m per 10 m toward +X.
//! let floor = FloorFn(|x: f32, _z: f32| 0.1 * x);
//! let mut fleet = HoverFleet::new(vec![FleetHull::new(HoverParams::DEFAULT_HULL, 2.0, 1.0)]);
//! let a = fleet.push(HoverBody::at_rest([0.0, 0.0, 0.0], 0.0, &HoverParams::DEFAULT_HULL), 0);
//! let b = fleet.push(HoverBody::at_rest([5.0, 0.0, 0.0], 0.0, &HoverParams::DEFAULT_HULL), 0);
//! let inputs = [FleetInput::new(b, [1.0, 0.0, 0.0])]; // slot `a` gets no input this tick: neutral
//! for _ in 0..60 {
//!     let report = fleet.step_all(
//!         &inputs, &floor, &WorldPhysics::EARTH, &HoverLimits::DEFAULT, &ClimbTuning::AIR_TIME, tick_dt(),
//!     );
//!     assert_eq!(report.recovered, 0);
//! }
//! assert!(fleet.body(b).pos[2] > fleet.body(a).pos[2]);
//! assert!(fleet.body(a).state.ground_roll > 0.09, "the ground rises on the hull's right (+X at yaw 0)");
//! ```

use crate::float::{canonical, det_clamp};
use crate::math;

use super::{ClimbTuning, HoverLimits, HoverParams, HoverState, WorldPhysics, step_with};

/// The floor height under a point of the XZ plane, for the hover host.
///
/// It must be a pure function of `(x, z)` with lockstep arithmetic (the same bits on every peer):
/// a heightfield ([`HeightGrid`]), an analytic surface ([`FloorFn`] over [`crate::math`]), a game's
/// own collision floor. A NaN or infinite answer is allowed; the hover step reads it as the
/// previous floor and [`ground_attitude`] as the center floor.
pub trait FloorSampler {
    /// Floor height, metres, at world `(x, z)`.
    fn floor(&self, x: f32, z: f32) -> f32;
}

impl<T: FloorSampler + ?Sized> FloorSampler for &T {
    fn floor(&self, x: f32, z: f32) -> f32 {
        (**self).floor(x, z)
    }
}

/// A closure as a [`FloorSampler`]: `FloorFn(|x, z| ...)`. The closure must be lockstep code
/// (transcendentals through [`crate::math`]).
#[derive(Debug, Clone, Copy)]
pub struct FloorFn<F>(pub F);

impl<F: Fn(f32, f32) -> f32> FloorSampler for FloorFn<F> {
    fn floor(&self, x: f32, z: f32) -> f32 {
        (self.0)(x, z)
    }
}

/// A flat floor at one height.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FlatFloor(pub f32);

impl FloorSampler for FlatFloor {
    fn floor(&self, _x: f32, _z: f32) -> f32 {
        self.0
    }
}

/// Why [`HeightGrid::new`] refused a grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeightGridError {
    /// Fewer than 2 columns or rows (bilinear needs a cell).
    TooSmall,
    /// The cell size is not finite and above 0, or the origin is not finite.
    BadCell,
    /// `heights.len()` is not `cols * rows`.
    WrongLength,
}

impl core::fmt::Display for HeightGridError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            HeightGridError::TooSmall => "a height grid needs at least 2 columns and 2 rows",
            HeightGridError::BadCell => "the cell size must be finite and above 0, and the origin finite",
            HeightGridError::WrongLength => "the height count is not cols * rows",
        })
    }
}

impl std::error::Error for HeightGridError {}

/// A regular heightfield sampled bilinearly, as a [`FloorSampler`].
///
/// **Index convention:** `heights` is row-major. Column `c` runs along +X and row `r` along +Z;
/// the post `(c, r)` is `heights[r * cols + c]`, at world `(origin[0] + c * cell, origin[1] + r * cell)`.
///
/// **Sampling:** `fx = (x - origin[0]) / cell` and `fz = (z - origin[1]) / cell`, each clamped to
/// the grid (`0 ..= cols - 1`, `0 ..= rows - 1`; a NaN coordinate reads as 0), so a point off the
/// grid reads the nearest edge. The cell is `c0 = trunc(fx)` (at most `cols - 2`) and likewise `r0`,
/// with `tx = fx - c0`, `tz = fz - r0`. The four posts are blended in one fixed order:
/// `near = h(c0, r0) + (h(c0 + 1, r0) - h(c0, r0)) * tx`,
/// `far = h(c0, r0 + 1) + (h(c0 + 1, r0 + 1) - h(c0, r0 + 1)) * tx`, then `near + (far - near) * tz`.
/// A post exactly on a grid line reads that post's height bit for bit.
#[derive(Debug, Clone, PartialEq)]
pub struct HeightGrid {
    origin: [f32; 2],
    cell: f32,
    cols: u32,
    rows: u32,
    heights: Vec<f32>,
}

impl HeightGrid {
    /// A grid of `cols` x `rows` posts `cell` metres apart, post `(0, 0)` at world
    /// `(origin[0], origin[1])` (XZ), `heights` row-major (see the index convention).
    pub fn new(
        origin: [f32; 2],
        cell: f32,
        cols: u32,
        rows: u32,
        heights: Vec<f32>,
    ) -> Result<Self, HeightGridError> {
        if cols < 2 || rows < 2 {
            return Err(HeightGridError::TooSmall);
        }
        if !(cell.is_finite() && cell > 0.0 && origin[0].is_finite() && origin[1].is_finite()) {
            return Err(HeightGridError::BadCell);
        }
        if heights.len() as u64 != u64::from(cols) * u64::from(rows) {
            return Err(HeightGridError::WrongLength);
        }
        Ok(HeightGrid {
            origin,
            cell,
            cols,
            rows,
            heights,
        })
    }

    /// A grid whose post `(c, r)` is `f(x, z)` at the post's world position.
    pub fn from_fn(
        origin: [f32; 2],
        cell: f32,
        cols: u32,
        rows: u32,
        f: impl Fn(f32, f32) -> f32,
    ) -> Result<Self, HeightGridError> {
        if !(cell.is_finite() && cell > 0.0) {
            return Err(HeightGridError::BadCell);
        }
        let mut heights = Vec::with_capacity((cols as usize).saturating_mul(rows as usize));
        for r in 0..rows {
            for c in 0..cols {
                let x = origin[0] + c as f32 * cell;
                let z = origin[1] + r as f32 * cell;
                heights.push(f(x, z));
            }
        }
        HeightGrid::new(origin, cell, cols, rows, heights)
    }

    /// Columns (posts along +X).
    pub fn cols(&self) -> u32 {
        self.cols
    }

    /// Rows (posts along +Z).
    pub fn rows(&self) -> u32 {
        self.rows
    }

    /// Height of post `(c, r)`, or `None` off the grid.
    pub fn post(&self, c: u32, r: u32) -> Option<f32> {
        if c < self.cols && r < self.rows {
            self.heights
                .get(r as usize * self.cols as usize + c as usize)
                .copied()
        } else {
            None
        }
    }

    /// Post height by index; `c < cols` and `r < rows` hold for every caller.
    fn h(&self, c: usize, r: usize) -> f32 {
        self.heights[r * self.cols as usize + c]
    }

    /// The cell and the fraction along one axis: `(i0, t)`.
    fn axis(v: f32, origin: f32, cell: f32, posts: u32) -> (usize, f32) {
        let last = (posts - 1) as f32;
        let f = (v - origin) / cell;
        let f = if f.is_nan() { 0.0 } else { det_clamp(f, 0.0, last) };
        // f is in [0, last]: the cast truncates toward 0, which is floor here.
        let i0 = (f as usize).min(posts as usize - 2);
        (i0, f - i0 as f32)
    }
}

impl FloorSampler for HeightGrid {
    fn floor(&self, x: f32, z: f32) -> f32 {
        let (c0, tx) = HeightGrid::axis(x, self.origin[0], self.cell, self.cols);
        let (r0, tz) = HeightGrid::axis(z, self.origin[1], self.cell, self.rows);
        let h00 = self.h(c0, r0);
        let h10 = self.h(c0 + 1, r0);
        let h01 = self.h(c0, r0 + 1);
        let h11 = self.h(c0 + 1, r0 + 1);
        let near = h00 + (h10 - h00) * tx;
        let far = h01 + (h11 - h01) * tx;
        near + (far - near) * tz
    }
}

/// One hull as a host keeps it: its world position and its hover state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HoverBody {
    /// World position `[x, y, z]`, metres. Y is `floor + altitude` after [`integrate`].
    pub pos: [f32; 3],
    /// The hover state [`step_with`] steps.
    pub state: HoverState,
}

impl HoverBody {
    /// A hull at `pos` (X and Z; Y is set by the first [`integrate`]) with the default state.
    pub fn new(pos: [f32; 3]) -> Self {
        HoverBody {
            pos,
            state: HoverState::default(),
        }
    }

    /// A hull resting on its cushion: `altitude` = the hull's ride height, facing `yaw` (radians,
    /// wrapped by the step), `pos[1]` = `pos[1] + ride_height` (pass the floor height in `pos[1]`).
    pub fn at_rest(pos: [f32; 3], yaw: f32, params: &HoverParams) -> Self {
        HoverBody {
            pos: [pos[0], pos[1] + params.ride_height, pos[2]],
            state: HoverState {
                altitude: params.ride_height,
                yaw,
                ..HoverState::default()
            },
        }
    }
}

/// Move `body` by one tick after its step: `x += velocity_x * dt`, `z += velocity_z * dt`, then
/// `y = floor + altitude`, in that order (the libm-1000 vector's loop, bit for bit). `floor` is the
/// floor height the step was given this tick (sampled before the move). The step owns the vertical
/// motion through `altitude`, so `velocity[1]` is not added.
#[inline]
pub fn integrate(body: &mut HoverBody, dt: f32, floor: f32) {
    body.pos[0] += body.state.velocity[0] * dt;
    body.pos[2] += body.state.velocity[2] * dt;
    body.pos[1] = floor + body.state.altitude;
}

/// The ground's pitch and roll under a hull, radians: `(pitch, roll)`, pitch positive when the
/// ground rises ahead and roll positive when it rises on the hull's right (the signs of
/// [`HoverState::ground_pitch`] and [`HoverState::ground_roll`]).
///
/// Four floor samples on the hull's yaw basis (forward `(sin yaw, cos yaw)`, right
/// `(cos yaw, -sin yaw)` in XZ): nose and tail `half_length` ahead and behind, right and left
/// `half_width` to each side. Then `pitch = atan2(nose - tail, 2 * half_length)` and
/// `roll = atan2(right - left, 2 * half_width)`: the central difference of the floor along each
/// axis (for a plane, its exact slope). `pos` is the hull's position (X and Z are read).
///
/// NaN-safe: a sample that is NaN or infinite reads the center floor (`sampler.floor(x, z)`, sampled
/// only then), and a center that is not finite either reads 0, so one bad sample gives a half-length
/// difference and a bad pair gives 0. A `half_length` or `half_width` that is not finite and above 0
/// gives 0 for that axis. The results are canonical (`-0` is `+0`).
///
/// This is Regia's own ground pass (the brief's four-sample plane fit), not a port: a game that has
/// its own pass keeps it and writes the two state words itself.
pub fn ground_attitude<S: FloorSampler + ?Sized>(
    sampler: &S,
    pos: [f32; 3],
    yaw: f32,
    half_length: f32,
    half_width: f32,
) -> (f32, f32) {
    let (x, z) = (pos[0], pos[2]);
    let (s, c) = (math::sin(yaw), math::cos(yaw));
    // The center, sampled at most once and only for a bad sample.
    let mut center: Option<f32> = None;
    let mut read = |sx: f32, sz: f32| -> f32 {
        let v = sampler.floor(sx, sz);
        if v.is_finite() {
            return v;
        }
        *center.get_or_insert_with(|| {
            let mid = sampler.floor(x, z);
            if mid.is_finite() { mid } else { 0.0 }
        })
    };
    let pitch = if half_length.is_finite() && half_length > 0.0 {
        let (dx, dz) = (s * half_length, c * half_length);
        let nose = read(x + dx, z + dz);
        let tail = read(x - dx, z - dz);
        math::atan2(canonical(nose - tail), half_length + half_length)
    } else {
        0.0
    };
    let roll = if half_width.is_finite() && half_width > 0.0 {
        let (dx, dz) = (c * half_width, -s * half_width);
        let right = read(x + dx, z + dz);
        let left = read(x - dx, z - dz);
        math::atan2(canonical(right - left), half_width + half_width)
    } else {
        0.0
    };
    (canonical(pitch), canonical(roll))
}

/// A hull type of a [`HoverFleet`]: its parameters and the footprint [`ground_attitude`] samples.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FleetHull {
    /// The hull's parameters (validate them when loading: [`HoverParams::validate`]).
    pub params: HoverParams,
    /// Half the hull's length, metres: the nose and tail samples are this far ahead and behind.
    pub half_length: f32,
    /// Half the hull's width, metres: the left and right samples are this far to each side.
    pub half_width: f32,
}

impl FleetHull {
    /// A hull type.
    pub const fn new(params: HoverParams, half_length: f32, half_width: f32) -> Self {
        FleetHull {
            params,
            half_length,
            half_width,
        }
    }
}

/// One hull's input for a tick: the slot it drives and `[throttle, strafe, steer]`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FleetInput {
    /// The hull's slot ([`HoverFleet::push`]).
    pub slot: u32,
    /// `[throttle, strafe, steer]`, as [`step_with`] reads them.
    pub axes: [f32; 3],
}

impl FleetInput {
    /// Input `axes` for `slot`.
    pub const fn new(slot: u32, axes: [f32; 3]) -> Self {
        FleetInput { slot, axes }
    }
}

/// What one [`HoverFleet::step_all`] did besides stepping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FleetReport {
    /// Hulls stepped.
    pub stepped: u32,
    /// Hulls whose state was poisoned and reset to rest at the floor ([`super::StepReport::recovered`]).
    pub recovered: u32,
    /// Input entries naming a slot the fleet does not have (ignored).
    pub unknown_slots: u32,
    /// Slots named by more than one input entry this tick: each reads neutral input `[0, 0, 0]`, so
    /// the answer does not depend on the entries' order.
    pub duplicate_slots: u32,
    /// The tick was not stepped because `dt` was not finite and above 0.
    pub skipped: bool,
}

/// Every hover hull of a match, in slot order, stepped with one call per tick.
///
/// Slots are dense: [`HoverFleet::push`] returns `0, 1, 2, ...`. [`HoverFleet::step_all`] steps the
/// hulls in ascending slot order, and for each one: writes [`ground_attitude`] into the state,
/// samples the floor at the hull, calls [`step_with`] with the hull type's parameters and the
/// match's limits, climb rules and world, then [`integrate`]s it. The inputs arrive as a list of
/// `(slot, axes)` entries in any order (a network delivers them in arrival order); a slot with no
/// entry reads neutral input. Hulls do not touch each other here (contacts are a later pass).
///
/// The fleet's buffers grow in [`HoverFleet::push`] only: [`HoverFleet::step_all`] allocates nothing.
#[derive(Debug, Clone, Default)]
pub struct HoverFleet {
    hulls: Vec<FleetHull>,
    bodies: Vec<HoverBody>,
    hull_of: Vec<u32>,
    /// Per-slot scratch, refilled each tick: the slot's axes and how many entries named it.
    axes: Vec<[f32; 3]>,
    hits: Vec<u8>,
}

impl HoverFleet {
    /// A fleet with these hull types and no hulls.
    pub fn new(hulls: Vec<FleetHull>) -> Self {
        HoverFleet {
            hulls,
            ..HoverFleet::default()
        }
    }

    /// Add a hull of type `hull` (an index into the hull types; a missing type is clamped to the
    /// last one, and a fleet with no types gets [`HoverParams::DEFAULT_HULL`] with a 2 m x 1 m half
    /// footprint) and return its slot.
    pub fn push(&mut self, body: HoverBody, hull: u32) -> u32 {
        if self.hulls.is_empty() {
            self.hulls
                .push(FleetHull::new(HoverParams::DEFAULT_HULL, 2.0, 1.0));
        }
        let hull = hull.min(self.hulls.len() as u32 - 1);
        let slot = self.bodies.len() as u32;
        self.bodies.push(body);
        self.hull_of.push(hull);
        self.axes.push([0.0; 3]);
        self.hits.push(0);
        slot
    }

    /// Hulls in the fleet.
    pub fn len(&self) -> usize {
        self.bodies.len()
    }

    /// True when the fleet has no hulls.
    pub fn is_empty(&self) -> bool {
        self.bodies.is_empty()
    }

    /// The hull types.
    pub fn hulls(&self) -> &[FleetHull] {
        &self.hulls
    }

    /// Every hull, in slot order.
    pub fn bodies(&self) -> &[HoverBody] {
        &self.bodies
    }

    /// Every hull, in slot order, for a host's own writes (a hop, a respawn, the aim pitch).
    pub fn bodies_mut(&mut self) -> &mut [HoverBody] {
        &mut self.bodies
    }

    /// The hull in `slot`. Panics if the slot does not exist (a host bug, not data).
    pub fn body(&self, slot: u32) -> &HoverBody {
        &self.bodies[slot as usize]
    }

    /// The hull in `slot`, if it exists.
    pub fn get(&self, slot: u32) -> Option<&HoverBody> {
        self.bodies.get(slot as usize)
    }

    /// The hull in `slot`, if it exists, for a host's own writes.
    pub fn get_mut(&mut self, slot: u32) -> Option<&mut HoverBody> {
        self.bodies.get_mut(slot as usize)
    }

    /// The hull type index of `slot`.
    pub fn hull_of(&self, slot: u32) -> Option<u32> {
        self.hull_of.get(slot as usize).copied()
    }

    /// Step every hull one tick, in ascending slot order (see the type's docs). `inputs` may be in
    /// any order; `sampler` gives the floor; `world`, `limits` and `climb` are the match's settings
    /// ([`step_with`]). A `dt` that is not finite and above 0 steps nothing (`skipped`).
    pub fn step_all<S: FloorSampler + ?Sized>(
        &mut self,
        inputs: &[FleetInput],
        sampler: &S,
        world: &WorldPhysics,
        limits: &HoverLimits,
        climb: &ClimbTuning,
        dt: f32,
    ) -> FleetReport {
        let mut report = FleetReport::default();
        if !(dt.is_finite() && dt > 0.0) {
            report.skipped = true;
            return report;
        }
        // Gather the inputs by slot: the result is the same for any order of `inputs`.
        self.hits.fill(0);
        for entry in inputs {
            let Some(hits) = self.hits.get_mut(entry.slot as usize) else {
                report.unknown_slots += 1;
                continue;
            };
            *hits = hits.saturating_add(1);
            if *hits == 1 {
                self.axes[entry.slot as usize] = entry.axes;
            } else if *hits == 2 {
                report.duplicate_slots += 1;
            }
        }
        for (slot, body) in self.bodies.iter_mut().enumerate() {
            let hull = &self.hulls[self.hull_of[slot] as usize];
            let axes = if self.hits[slot] == 1 {
                self.axes[slot]
            } else {
                [0.0; 3]
            };
            let (pitch, roll) = ground_attitude(
                sampler,
                body.pos,
                body.state.yaw,
                hull.half_length,
                hull.half_width,
            );
            body.state.ground_pitch = pitch;
            body.state.ground_roll = roll;
            let floor = sampler.floor(body.pos[0], body.pos[2]);
            let step = step_with(
                &hull.params,
                limits,
                climb,
                world,
                &mut body.state,
                &axes,
                floor,
                dt,
            );
            report.stepped += 1;
            if step.recovered {
                report.recovered += 1;
            }
            // The floor the step used: a non-finite sample read as the previous floor.
            let used = if floor.is_finite() {
                floor
            } else {
                body.state.prev_terrain.unwrap_or(0.0)
            };
            integrate(body, dt, used);
        }
        report
    }
}
