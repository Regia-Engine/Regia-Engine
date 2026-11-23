// SPDX-License-Identifier: MPL-2.0
//! The deterministic CPU reference solver: PIC/FLIP liquid on a MAC grid (Zhu and Bridson 2005;
//! Brackbill and Ruppel 1986; Harlow and Welch 1965), with variational solid boundaries (Batty,
//! Bertails and Bridson 2007) and the MIC(0)-preconditioned CG pressure solve (Bridson 2015).
//!
//! One [`FluidSim::step`] advances by the fixed `dt` in `1..=max_substeps` CFL substeps. Each
//! substep: particles to grid (trilinear weights), velocity extrapolation, gravity and external
//! acceleration, liquid cell marking, pressure projection (free surface `p = 0` in air cells, the
//! open-face fractions of the solid SDF), grid to particles (PIC/FLIP blend), RK2 advection through
//! the grid velocity, a pairwise position correction against clumping (volume loss), and the push
//! out of solids and the domain walls. After the substeps the sinks remove particles, the reseeding
//! keeps interior cells near eight particles, and the whitewater advances.
//!
//! Everything runs single-threaded over plain vectors in particle and cell order, with a seeded
//! xorshift generator, so the same inputs give bit-identical particles ([`FluidSim::state_digest`]).

#[cfg(test)]
use std::cell::Cell;

use glam::Vec3;

// Times `drop_non_finite` built a keep-mask. Steady steps of finite particles must stay at zero.
#[cfg(test)]
thread_local! {
    static KEEP_MASKS: Cell<usize> = const { Cell::new(0) };
}

use crate::grid::{FaceField, Fnv1a, GridDims, MacVelocity, Rng};
use crate::pcg::{CgWorkspace, PoissonMatrix, SolveReport};
use crate::solid::{Collider, MovingSolid, NO_OWNER, SolidField, SolidImpulse, SolidShape};
use crate::surface::SurfaceScratch;
use crate::tally::{Scratch, Tally};
use crate::whitewater::{Whitewater, WhitewaterParams};

/// Particles each liquid cell is seeded and reseeded with (2 x 2 x 2).
pub const PARTICLES_PER_CELL: usize = 8;

/// Most grid cells one simulation may have (64³). A larger request is refused, not clamped.
pub const MAX_CELLS: usize = 262_144;

/// Most liquid particles one simulation may hold.
pub const MAX_PARTICLES: usize = 2_000_000;

/// Mobile caps (Android): the CPU low-resolution path only (ADR-2441 § 3).
pub const MOBILE_MAX_CELLS: usize = 32_768;
/// Mobile particle cap.
pub const MOBILE_MAX_PARTICLES: usize = 65_536;

/// Most emitters, sinks or colliders of one simulation.
pub const MAX_SHAPES: usize = 64;

/// Version of the digest's byte layout; bumped when the hashed state changes (2: the solver
/// settings, emitters, sinks, colliders and the generator states join the hash, ADR-4753).
const DIGEST_VERSION: u32 = 2;

/// A cell with more particles than this is thinned back to eight by the reseeding.
const THIN_ABOVE: usize = 12;

/// Particles per cell that enter the position-correction pairs.
///
/// A cell with more particles still corrects the first [`SEPARATE_CELL_CAP`] in bin order (the
/// same order reseeding keeps) and leaves the rest for the reseed thin. Cells at or under the
/// cap pair exactly as before (ADR-5313).
pub const SEPARATE_CELL_CAP: usize = THIN_ABOVE;

/// Layers of velocity extrapolation into the air.
const EXTRAPOLATION_LAYERS: usize = 3;

/// Most slides along a domain wall that move a particle out of a solid reaching through the wall.
const WALL_SLIDE_ITERATIONS: usize = 8;

/// Why a simulation could not be created or edited. [`FluidError::code`] is the stable protocol
/// code (`book/agents/fluid.md`).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum FluidError {
    /// The bounds are not finite or not positive in size.
    #[error("bounds must be finite with max > min on every axis")]
    BadBounds,
    /// The cell size is not finite or not positive.
    #[error("cell_size must be finite and positive")]
    BadCellSize,
    /// The bounds hold more cells than the cap.
    #[error("the grid would have {cells} cells (most {max})")]
    TooManyCells {
        /// Requested cells.
        cells: usize,
        /// The cap.
        max: usize,
    },
    /// A parameter is out of range.
    #[error("{0}")]
    BadParam(String),
    /// Too many emitters, sinks or colliders.
    #[error("at most {MAX_SHAPES} emitters, sinks or colliders")]
    TooManyShapes,
    /// A moving solid has a non-finite or out-of-range field (index into the list).
    #[error("moving solid {0} has a non-finite or out-of-range shape, pose or velocity")]
    BadSolid(usize),
}

impl FluidError {
    /// The lowercase protocol code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadBounds => "fluid_bad_bounds",
            Self::BadCellSize => "fluid_bad_cell_size",
            Self::TooManyCells { .. } => "fluid_too_many_cells",
            Self::BadParam(_) => "fluid_bad_param",
            Self::TooManyShapes => "fluid_too_many_shapes",
            Self::BadSolid(_) => "fluid_bad_solid",
        }
    }
}

/// Solver settings. Every field has a default; [`FluidParams::validate`] checks the ranges.
#[derive(Debug, Clone, PartialEq)]
pub struct FluidParams {
    /// Minimum corner of the closed domain box.
    pub bounds_min: Vec3,
    /// Maximum corner of the closed domain box.
    pub bounds_max: Vec3,
    /// Grid cell edge in metres.
    pub cell_size: f32,
    /// The fixed step in seconds.
    pub dt: f32,
    /// Gravity (m/s²).
    pub gravity: Vec3,
    /// A uniform external acceleration added to gravity (wind, a shaken tank).
    pub external_acceleration: Vec3,
    /// FLIP share of the PIC/FLIP blend (0 = PIC, 1 = FLIP; 0.95 is the classic value).
    pub flip_ratio: f32,
    /// Liquid density (kg/m³).
    pub density: f32,
    /// Most PCG iterations per projection.
    pub pcg_max_iterations: u32,
    /// PCG stop: residual infinity norm relative to the right-hand side's.
    pub pcg_tolerance: f64,
    /// Most CFL substeps per step.
    pub max_substeps: u32,
    /// CFL number: cells a particle may cross per substep.
    pub cfl: f32,
    /// Strength (0..=1) of the pairwise position correction against volume loss.
    pub separation: f32,
    /// Whether interior cells are reseeded toward eight particles.
    pub reseed: bool,
    /// Most liquid particles (emission and reseeding stop there).
    pub particle_cap: usize,
    /// Seed of the jitter and whitewater generators.
    pub seed: u64,
    /// Secondary particles.
    pub whitewater: WhitewaterParams,
}

impl Default for FluidParams {
    fn default() -> Self {
        Self {
            bounds_min: Vec3::ZERO,
            bounds_max: Vec3::new(1.0, 1.0, 1.0),
            cell_size: 0.05,
            dt: 1.0 / 60.0,
            gravity: Vec3::new(0.0, -9.81, 0.0),
            external_acceleration: Vec3::ZERO,
            flip_ratio: 0.95,
            density: 1000.0,
            pcg_max_iterations: 200,
            pcg_tolerance: 1e-6,
            max_substeps: 4,
            cfl: 1.0,
            separation: 0.5,
            reseed: true,
            particle_cap: 200_000,
            seed: 1,
            whitewater: WhitewaterParams::default(),
        }
    }
}

impl FluidParams {
    /// Grid cells the bounds and cell size give, or the error.
    pub fn grid(&self) -> Result<GridDims, FluidError> {
        let finite = |v: Vec3| v.is_finite();
        if !finite(self.bounds_min) || !finite(self.bounds_max) {
            return Err(FluidError::BadBounds);
        }
        let size = self.bounds_max - self.bounds_min;
        if !(size.x > 0.0 && size.y > 0.0 && size.z > 0.0) {
            return Err(FluidError::BadBounds);
        }
        if !(self.cell_size.is_finite() && self.cell_size > 0.0) {
            return Err(FluidError::BadCellSize);
        }
        let n = (size / self.cell_size).ceil();
        let max = max_cells();
        // f64 product: three huge counts must not overflow before the comparison.
        let cells = f64::from(n.x) * f64::from(n.y) * f64::from(n.z);
        if !(cells.is_finite() && cells <= max as f64) || n.x < 1.0 || n.y < 1.0 || n.z < 1.0 {
            let cells = if cells.is_finite() && cells < usize::MAX as f64 {
                cells as usize
            } else {
                usize::MAX
            };
            return Err(FluidError::TooManyCells { cells, max });
        }
        Ok(GridDims {
            nx: n.x as usize,
            ny: n.y as usize,
            nz: n.z as usize,
            dx: self.cell_size,
            origin: self.bounds_min,
        })
    }

    /// Checks every range.
    pub fn validate(&self) -> Result<GridDims, FluidError> {
        let dims = self.grid()?;
        let bad = |what: &str| Err(FluidError::BadParam(what.to_string()));
        if !(self.dt.is_finite() && self.dt > 0.0 && self.dt <= 0.1) {
            return bad("dt must be in (0, 0.1]");
        }
        if !self.gravity.is_finite() || !self.external_acceleration.is_finite() {
            return bad("gravity and external_acceleration must be finite");
        }
        // Compared on the software sqrt so validation cannot differ by platform (ADR-4753).
        if crate::math::len(self.gravity) > 1000.0 || crate::math::len(self.external_acceleration) > 1000.0 {
            return bad("gravity and external_acceleration must be at most 1000 m/s²");
        }
        if !(0.0..=1.0).contains(&self.flip_ratio) {
            return bad("flip_ratio must be in [0, 1]");
        }
        if !(self.density.is_finite() && self.density > 0.0) {
            return bad("density must be positive");
        }
        if self.pcg_max_iterations == 0 || self.pcg_max_iterations > 10_000 {
            return bad("pcg_max_iterations must be in 1..=10000");
        }
        if !(self.pcg_tolerance.is_finite() && self.pcg_tolerance > 0.0 && self.pcg_tolerance < 1.0) {
            return bad("pcg_tolerance must be in (0, 1)");
        }
        if self.max_substeps == 0 || self.max_substeps > 16 {
            return bad("max_substeps must be in 1..=16");
        }
        if !(self.cfl.is_finite() && self.cfl > 0.0 && self.cfl <= 10.0) {
            return bad("cfl must be in (0, 10]");
        }
        if !(0.0..=1.0).contains(&self.separation) {
            return bad("separation must be in [0, 1]");
        }
        if self.particle_cap == 0 || self.particle_cap > max_particles() {
            return Err(FluidError::BadParam(format!(
                "particle_cap must be in 1..={}",
                max_particles()
            )));
        }
        self.whitewater.validate().map_err(FluidError::BadParam)?;
        Ok(dims)
    }
}

/// The cell cap on this platform.
#[must_use]
pub fn max_cells() -> usize {
    if cfg!(target_os = "android") {
        MOBILE_MAX_CELLS
    } else {
        MAX_CELLS
    }
}

/// The particle cap on this platform.
#[must_use]
pub fn max_particles() -> usize {
    if cfg!(target_os = "android") {
        MOBILE_MAX_PARTICLES
    } else {
        MAX_PARTICLES
    }
}

/// The region of an emitter or a sink.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Shape {
    /// An axis-aligned box.
    Box {
        /// Centre.
        center: Vec3,
        /// Half extents.
        half_extents: Vec3,
    },
    /// A sphere.
    Sphere {
        /// Centre.
        center: Vec3,
        /// Radius.
        radius: f32,
    },
}

impl Shape {
    /// Whether `p` is inside.
    #[must_use]
    pub fn contains(&self, p: Vec3) -> bool {
        match *self {
            Self::Box { center, half_extents } => {
                let d = (p - center).abs();
                d.x <= half_extents.x && d.y <= half_extents.y && d.z <= half_extents.z
            }
            Self::Sphere { center, radius } => (p - center).length_squared() <= radius * radius,
        }
    }

    /// The bounding box.
    #[must_use]
    pub fn aabb(&self) -> (Vec3, Vec3) {
        match *self {
            Self::Box { center, half_extents } => (center - half_extents, center + half_extents),
            Self::Sphere { center, radius } => (center - Vec3::splat(radius), center + Vec3::splat(radius)),
        }
    }

    fn is_valid(&self) -> bool {
        match *self {
            Self::Box { center, half_extents } => {
                center.is_finite() && half_extents.is_finite() && half_extents.min_element() >= 0.0
            }
            Self::Sphere { center, radius } => center.is_finite() && radius.is_finite() && radius >= 0.0,
        }
    }
}

/// How an emitter adds liquid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmitterKind {
    /// Fills the shape once, at the next step.
    Fill,
    /// A source: every step it tops the cells inside up to eight particles and sets the velocity of
    /// the liquid inside to its own.
    Source,
}

/// A liquid emitter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Emitter {
    /// Region.
    pub shape: Shape,
    /// Velocity of the emitted liquid.
    pub velocity: Vec3,
    /// Fill once or keep sourcing.
    pub kind: EmitterKind,
    /// Whether a fill emitter has run.
    pub done: bool,
}

/// Counts and measures of the current state.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct FluidStats {
    /// Steps taken.
    pub steps: u64,
    /// Liquid particles.
    pub particles: usize,
    /// Liquid volume (m³): the liquid region the projection sees, interior liquid cells (every open
    /// face leads to liquid) counting whole and the other liquid cells by their particle fill
    /// `min(1, n / 8)`.
    pub volume: f32,
    /// The particles' nominal volume, `particles / 8 * dx³` (m³).
    pub nominal_volume: f32,
    /// `volume / nominal_volume - 1`: how far the liquid region and the particle count disagree
    /// (negative when particles clump and the region shrinks, the volume loss of FLIP).
    pub volume_drift: f32,
    /// Largest weighted divergence of a liquid cell after the last projection (1/s).
    pub divergence_max: f32,
    /// PCG iterations of the last projection.
    pub pcg_iterations: u32,
    /// Final PCG residual of the last projection (infinity norm).
    pub pcg_residual: f64,
    /// Whether the last projection met its tolerance.
    pub pcg_converged: bool,
    /// Substeps of the last step.
    pub substeps: u32,
    /// Liquid cells.
    pub liquid_cells: usize,
    /// Particles dropped as non-finite since creation.
    pub dropped_non_finite: u64,
    /// Foam particles.
    pub foam: usize,
    /// Spray particles.
    pub spray: usize,
    /// Bubble particles.
    pub bubbles: usize,
    /// Whitewater particles generated in the last step.
    pub whitewater_generated: usize,
    /// Moving solids (kinematic and dynamic bodies) in the last step.
    pub moving_solids: usize,
    /// Faces of moving solids that touched liquid in the last step (summed over substeps).
    pub wetted_faces: u32,
}

/// Scratch for [`FluidSim::bin`]: counting sort buffers kept across steps.
#[derive(Debug, Clone, Default)]
struct BinScratch {
    count: Vec<u32>,
    index: Vec<usize>,
    start: Vec<u32>,
    items: Vec<u32>,
    fill: Vec<u32>,
}

/// Bins held outside the scratch mutex so a caller can read them while using the sim.
pub(crate) struct HeldBins {
    scratch: BinScratch,
    /// The bins of this hold.
    pub(crate) bins: Bins,
}

/// The particle bins of one moment: per cell, the particles in it (in particle order).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Bins {
    /// `start[c]..start[c + 1]` indexes `items` for cell `c`.
    pub start: Vec<u32>,
    /// Particle indices sorted by cell (stable).
    pub items: Vec<u32>,
}

impl Bins {
    /// Particles in cell `c`.
    #[must_use]
    pub fn cell(&self, c: usize) -> &[u32] {
        &self.items[self.start[c] as usize..self.start[c + 1] as usize]
    }

    /// Count of cell `c`.
    #[must_use]
    pub fn count(&self, c: usize) -> usize {
        (self.start[c + 1] - self.start[c]) as usize
    }
}

/// A PIC/FLIP liquid.
#[derive(Debug, Clone)]
pub struct FluidSim {
    params: FluidParams,
    dims: GridDims,
    solid: SolidField,
    /// Particle positions (world).
    pub(crate) pos: Vec<Vec3>,
    /// Particle velocities.
    pub(crate) vel: Vec<Vec3>,
    emitters: Vec<Emitter>,
    sinks: Vec<Shape>,
    grid: MacVelocity,
    old: MacVelocity,
    weight: [Vec<f32>; 3],
    pressure: Vec<f64>,
    liquid: Vec<bool>,
    steps: u64,
    rng: Rng,
    last: SolveReport,
    divergence_max: f32,
    substeps: u32,
    dropped: u64,
    pub(crate) whitewater: Whitewater,
    /// Moving solids at the start of the next step.
    moving: Vec<MovingSolid>,
    /// Pressure impulses on the moving solids over the last step (f64 sums, same order).
    impulses: Vec<SolidImpulse>,
    impulse_sums: Vec<[f64; 6]>,
    /// Times the velocity scratch (`old`, extrapolation buffers) allocated.
    velocity_scratch_grows: Tally,
    /// Times the pressure matrix or the CG workspace allocated.
    poisson_workspace_grows: Tally,
    /// Times the particle bins allocated.
    bin_scratch_grows: Tally,
    /// Capacity of the bin `start` buffer from the last bin.
    bin_cap_start: Tally,
    /// Capacity of the bin `items` buffer from the last bin.
    bin_cap_items: Tally,
    /// Capacity of the bin cell-index buffer from the last bin.
    bin_cap_index: Tally,
    /// Particle pairs `separate` considered.
    pair_visits: Tally,
    /// Times a sink or non-finite mask was allocated.
    drop_mask_allocs: Tally,
    /// Nodes the last level set walked.
    level_set_node_visits: Tally,
    /// Capacity of the stored level-set samples.
    level_set_phi_capacity: Tally,
    /// Times the level set allocated its sample buffer.
    level_set_allocs: Tally,
    /// Valid mask, next mask and Jacobi output for velocity extrapolation.
    extrap_valid: Vec<bool>,
    /// Second extrapolation mask (the layer being written).
    extrap_next: Vec<bool>,
    /// Jacobi output for one extrapolation layer.
    extrap_out: Vec<f32>,
    /// Poisson matrix reused across projections.
    poisson: PoissonMatrix,
    /// Pressure right-hand side, reused across projections.
    rhs: Vec<f64>,
    /// Conjugate-gradient vectors, reused across projections.
    cg: CgWorkspace,
    /// Particle-bin scratch.
    bin_scratch: Scratch<BinScratch>,
    /// Level-set and surface-mesh scratch.
    pub(crate) surface_scratch: Scratch<SurfaceScratch>,
    /// Keep-mask reused when a sink or a non-finite particle actually drops something.
    keep_mask: Vec<bool>,
}

impl FluidSim {
    /// A simulation with no liquid yet.
    pub fn new(params: FluidParams) -> Result<Self, FluidError> {
        Self::with_colliders(params, Vec::new())
    }

    /// A simulation with static colliders. A heightfield with a bad size or spacing is refused
    /// with [`FluidError::BadParam`].
    pub fn with_colliders(params: FluidParams, colliders: Vec<Collider>) -> Result<Self, FluidError> {
        let dims = params.validate()?;
        if colliders.len() > MAX_SHAPES {
            return Err(FluidError::TooManyShapes);
        }
        if let Some((i, problem)) = colliders
            .iter()
            .enumerate()
            .find_map(|(i, c)| c.problem().map(|p| (i, p)))
        {
            return Err(FluidError::BadParam(format!("collider {i}: {problem}")));
        }
        let solid = SolidField::new(&dims, colliders);
        let cells = dims.cells();
        let face_len = |axis: usize| {
            let s = dims.face_dims(axis);
            s[0] * s[1] * s[2]
        };
        let faces = [face_len(0), face_len(1), face_len(2)];
        let max_faces = faces.into_iter().max().unwrap_or(0);
        let rng = Rng::new(params.seed);
        let whitewater = Whitewater::new(params.whitewater.clone(), params.seed);
        let n = [dims.nx, dims.ny, dims.nz];
        Ok(Self {
            grid: MacVelocity::new(&dims),
            old: MacVelocity::new(&dims),
            weight: [vec![0.0; faces[0]], vec![0.0; faces[1]], vec![0.0; faces[2]]],
            pressure: vec![0.0; cells],
            liquid: vec![false; cells],
            params,
            dims,
            solid,
            pos: Vec::new(),
            vel: Vec::new(),
            emitters: Vec::new(),
            sinks: Vec::new(),
            steps: 0,
            rng,
            last: SolveReport::default(),
            divergence_max: 0.0,
            substeps: 0,
            dropped: 0,
            whitewater,
            moving: Vec::new(),
            impulses: Vec::new(),
            impulse_sums: Vec::new(),
            velocity_scratch_grows: Tally::default(),
            poisson_workspace_grows: Tally::default(),
            bin_scratch_grows: Tally::default(),
            bin_cap_start: Tally::default(),
            bin_cap_items: Tally::default(),
            bin_cap_index: Tally::default(),
            pair_visits: Tally::default(),
            drop_mask_allocs: Tally::default(),
            level_set_node_visits: Tally::default(),
            level_set_phi_capacity: Tally::default(),
            level_set_allocs: Tally::default(),
            extrap_valid: vec![false; max_faces],
            extrap_next: vec![false; max_faces],
            extrap_out: vec![0.0; max_faces],
            poisson: PoissonMatrix::new(n),
            rhs: vec![0.0; cells],
            cg: CgWorkspace {
                r: vec![0.0; cells],
                precon: vec![0.0; cells],
                q: vec![0.0; cells],
                z: vec![0.0; cells],
                s: vec![0.0; cells],
            },
            bin_scratch: Scratch::default(),
            surface_scratch: Scratch::default(),
            keep_mask: Vec::new(),
        })
    }

    /// Times the velocity scratch allocated since this sim was built.
    ///
    /// A step that reuses the buffers from construction leaves the count unchanged.
    #[must_use]
    pub fn velocity_scratch_grows(&self) -> u64 {
        self.velocity_scratch_grows.get()
    }

    /// Times the pressure matrix or the CG workspace allocated since this sim was built.
    #[must_use]
    pub fn poisson_workspace_grows(&self) -> u64 {
        self.poisson_workspace_grows.get()
    }

    /// Times the particle bins allocated since this sim was built.
    #[must_use]
    pub fn bin_scratch_grows(&self) -> u64 {
        self.bin_scratch_grows.get()
    }

    /// Capacities `(start, items, cell index)` of the bin scratch after the last bin.
    #[must_use]
    pub fn bin_scratch_capacity(&self) -> (usize, usize, usize) {
        (
            self.bin_cap_start.get() as usize,
            self.bin_cap_items.get() as usize,
            self.bin_cap_index.get() as usize,
        )
    }

    /// Particle pairs the position correction has considered.
    #[must_use]
    pub fn pair_visits(&self) -> u64 {
        self.pair_visits.get()
    }

    /// Times a sink or non-finite keep-mask was allocated.
    #[must_use]
    pub fn drop_mask_allocs(&self) -> u64 {
        self.drop_mask_allocs.get()
    }

    /// Nodes the last level-set build walked.
    #[must_use]
    pub fn level_set_node_visits(&self) -> u64 {
        self.level_set_node_visits.get()
    }

    /// Capacity of the stored level-set samples (`0` before the first build).
    #[must_use]
    pub fn level_set_phi_capacity(&self) -> usize {
        self.level_set_phi_capacity.get() as usize
    }

    /// Times the level set allocated its sample buffer.
    #[must_use]
    pub fn level_set_allocs(&self) -> u64 {
        self.level_set_allocs.get()
    }

    pub(crate) fn note_level_set(&self, nodes: u64, phi_capacity: usize, allocated: bool) {
        self.level_set_node_visits.set(nodes);
        self.level_set_phi_capacity.set(phi_capacity as u64);
        if allocated {
            self.level_set_allocs.add(1);
        }
    }

    /// Replaces the moving solids (kinematic and dynamic bodies, ADR-2577). Each pose and velocity
    /// is the one at the start of the next step; the solver advances the pose through the step's
    /// substeps, and after the step by the whole `dt` (a solid nobody updates keeps moving at its
    /// velocity). At most [`MAX_SHAPES`].
    ///
    /// The impulses of the previous list are dropped: [`FluidSim::solid_impulses`] is empty until
    /// the next step fills it for the new list.
    pub fn set_moving_solids(&mut self, solids: Vec<MovingSolid>) -> Result<(), FluidError> {
        if solids.len() > MAX_SHAPES {
            return Err(FluidError::TooManyShapes);
        }
        if let Some(bad) = solids.iter().position(|s| !s.is_valid()) {
            return Err(FluidError::BadSolid(bad));
        }
        self.moving = solids;
        self.impulses.clear();
        Ok(())
    }

    /// The moving solids at the start of the next step.
    #[must_use]
    pub fn moving_solids(&self) -> &[MovingSolid] {
        &self.moving
    }

    /// The pressure impulse the liquid gave each moving solid over the last step, in the order of
    /// [`FluidSim::moving_solids`] (two-way coupling: apply the dynamic ones to their bodies).
    /// Empty after [`FluidSim::set_moving_solids`] until the next step, and after GPU steps handed
    /// back with `GpuFluid::sync_into` (the GPU path computes none) until the next CPU step.
    #[must_use]
    pub fn solid_impulses(&self) -> &[SolidImpulse] {
        &self.impulses
    }

    /// The settings.
    #[must_use]
    pub fn params(&self) -> &FluidParams {
        &self.params
    }

    /// The grid.
    #[must_use]
    pub fn dims(&self) -> &GridDims {
        &self.dims
    }

    /// The solid field.
    #[must_use]
    pub fn solid(&self) -> &SolidField {
        &self.solid
    }

    /// Particle positions.
    #[must_use]
    pub fn positions(&self) -> &[Vec3] {
        &self.pos
    }

    /// Particle velocities.
    #[must_use]
    pub fn velocities(&self) -> &[Vec3] {
        &self.vel
    }

    /// The whitewater.
    #[must_use]
    pub fn whitewater(&self) -> &Whitewater {
        &self.whitewater
    }

    /// Steps taken.
    #[must_use]
    pub fn steps(&self) -> u64 {
        self.steps
    }

    /// The emitters.
    #[must_use]
    pub fn emitters(&self) -> &[Emitter] {
        &self.emitters
    }

    /// The sinks.
    #[must_use]
    pub fn sinks(&self) -> &[Shape] {
        &self.sinks
    }

    /// Pressure (Pa) of cell `(i, j, k)` after the last projection (0 outside the liquid).
    #[must_use]
    pub fn pressure(&self, i: usize, j: usize, k: usize) -> f64 {
        if i < self.dims.nx && j < self.dims.ny && k < self.dims.nz {
            self.pressure[self.dims.cell(i, j, k)]
        } else {
            0.0
        }
    }

    /// Adds an emitter.
    pub fn add_emitter(&mut self, emitter: Emitter) -> Result<usize, FluidError> {
        if self.emitters.len() >= MAX_SHAPES {
            return Err(FluidError::TooManyShapes);
        }
        // The software sqrt keeps this validation platform-independent (ADR-4753).
        if !emitter.shape.is_valid()
            || !emitter.velocity.is_finite()
            || crate::math::len(emitter.velocity) > 1000.0
        {
            return Err(FluidError::BadParam(
                "emitter shape and velocity must be finite (speed at most 1000 m/s)".into(),
            ));
        }
        self.emitters.push(emitter);
        Ok(self.emitters.len() - 1)
    }

    /// Adds a sink: liquid inside it is removed every step.
    pub fn add_sink(&mut self, shape: Shape) -> Result<usize, FluidError> {
        if self.sinks.len() >= MAX_SHAPES {
            return Err(FluidError::TooManyShapes);
        }
        if !shape.is_valid() {
            return Err(FluidError::BadParam("sink shape must be finite".into()));
        }
        self.sinks.push(shape);
        Ok(self.sinks.len() - 1)
    }

    /// Adds particles directly (clamped to the domain; stops at the cap). Returns how many were added.
    pub fn add_particles(&mut self, particles: &[(Vec3, Vec3)]) -> usize {
        let mut added = 0;
        for &(p, v) in particles {
            if self.pos.len() >= self.params.particle_cap {
                break;
            }
            if !p.is_finite() || !v.is_finite() {
                continue;
            }
            self.pos.push(self.clamp_domain(p));
            self.vel.push(v);
            added += 1;
        }
        added
    }

    /// Advances one fixed step.
    pub fn step(&mut self) {
        let _span = tracing::info_span!(target: "regia_fluid", "fluid_step").entered();
        // The solids at the step's start pose first, so emitters do not fill them.
        self.rasterize_solids(0.0);
        self.emit();
        let substeps = self.substep_count();
        self.substeps = substeps;
        let dt = self.params.dt / substeps as f32;
        self.impulse_sums.clear();
        self.impulse_sums.resize(self.moving.len(), [0.0; 6]);
        let mut wetted = vec![0u32; self.moving.len()];
        for s in 0..substeps {
            if s > 0 {
                self.rasterize_solids(dt * s as f32);
            }
            self.substep(dt, &mut wetted);
        }
        self.impulses = self
            .moving
            .iter()
            .zip(&self.impulse_sums)
            .zip(&wetted)
            .map(|((m, sum), &wetted_faces)| SolidImpulse {
                tag: m.tag,
                linear: Vec3::new(sum[0] as f32, sum[1] as f32, sum[2] as f32),
                angular: Vec3::new(sum[3] as f32, sum[4] as f32, sum[5] as f32),
                wetted_faces,
            })
            .collect();
        // The solids keep moving at their velocities until the caller sets new poses.
        let step_dt = self.params.dt;
        for s in &mut self.moving {
            *s = s.advanced(step_dt);
        }
        self.apply_sinks();
        if self.params.reseed {
            self.reseed();
        }
        self.drop_non_finite();
        {
            let _span = tracing::info_span!(target: "regia_fluid", "fluid_whitewater").entered();
            let held = self.hold_bins();
            let mut ww = std::mem::take(&mut self.whitewater);
            ww.step(self, &held.bins, self.params.dt);
            self.whitewater = ww;
            self.restore_bins(held);
        }
        self.steps += 1;
    }

    /// The CFL substep count: the smallest `n` in `1..=max_substeps` with
    /// `max_speed <= n * cfl * dx / dt`.
    ///
    /// `max_speed` covers the particles and the moving solids (a solid's fastest surface point is
    /// bounded by `|v| + |w| r`, `r` its bounding radius): the solids are rasterized only at the
    /// substep times, so a fast thin solid sampled less often would jump over the liquid in
    /// between.
    ///
    /// Compared on the squares in f64, so no `sqrt` decides it: Rust documents `f32::sqrt` as
    /// possibly differing between platforms, and one ulp at the boundary changes the substep
    /// count (ADR-4753). The solid bound takes its two norms with the software square root.
    fn substep_count(&self) -> u32 {
        let mut max_sq = 0.0f64;
        let mut non_finite = false;
        for v in &self.vel {
            let sq = v.length_squared();
            if sq.is_finite() {
                max_sq = max_sq.max(f64::from(sq));
            } else {
                non_finite = true;
            }
        }
        for s in &self.moving {
            let linear = crate::math::sqrt_f64(f64::from(s.linear_velocity.length_squared()));
            let angular = crate::math::sqrt_f64(f64::from(s.angular_velocity.length_squared()));
            // integ-v048c: audfix2-fluid-g took the box radius as `sqrt_f64` of the f64-widened
            // square; integ-v048's `bounding_radius` already takes it with the software f32 root
            // (ADR-4753). Both are platform-independent, but only the f32 root passes both lanes'
            // pins (`box_cfl_follows_the_software_radius` and gaps_sim's
            // `box_substep_count_uses_the_software_radius`), so the f32 root stays.
            let speed = linear + angular * f64::from(s.shape.bounding_radius());
            max_sq = max_sq.max(speed * speed);
        }
        if non_finite {
            return self.params.max_substeps;
        }
        if max_sq == 0.0 {
            return 1;
        }
        let limit = f64::from(self.params.cfl) * f64::from(self.dims.dx) / f64::from(self.params.dt);
        let mut n = 1u32;
        while n < self.params.max_substeps && max_sq > (f64::from(n) * limit) * (f64::from(n) * limit) {
            n += 1;
        }
        n
    }

    /// Folds the moving solids, advanced `t` seconds into the step, into the solid field.
    fn rasterize_solids(&mut self, t: f32) {
        if self.moving.is_empty() && self.solid.moving.is_empty() {
            return;
        }
        let posed: Vec<MovingSolid> = self.moving.iter().map(|s| s.advanced(t)).collect();
        self.solid.rasterize(&self.dims, &posed);
    }

    fn substep(&mut self, dt: f32, wetted: &mut [u32]) {
        self.particles_to_grid();
        self.mark_liquid();
        for axis in 0..3 {
            self.old.c[axis].data.copy_from_slice(&self.grid.c[axis].data);
        }
        self.add_forces(dt);
        self.project(dt);
        self.accumulate_solid_impulses(dt, wetted);
        self.extrapolate_after_projection();
        self.grid_to_particles();
        self.advect(dt);
        self.separate();
        self.resolve_boundaries();
    }

    /// Bins the particles by cell (a stable counting sort; particles outside the grid go to the nearest cell).
    #[must_use]
    pub fn bin(&self) -> Bins {
        let held = self.hold_bins();
        let out = held.bins.clone();
        self.restore_bins(held);
        out
    }

    /// Fills the bin scratch and hands the bins back owned, so the caller can read them while
    /// borrowing the sim. [`Self::restore_bins`] puts the buffers back.
    pub(crate) fn hold_bins(&self) -> HeldBins {
        let mut scratch = std::mem::take(&mut *self.bin_scratch.lock());
        // Index and items are reserved to the particle cap, so an emitter that adds particles
        // on the next step does not allocate again (the cell tables stay at the grid size).
        fill_bin_scratch(
            &mut scratch,
            &self.pos,
            self.dims,
            self.params.particle_cap,
            &self.bin_scratch_grows,
        );
        self.bin_cap_start.set(scratch.start.capacity() as u64);
        self.bin_cap_items.set(scratch.items.capacity() as u64);
        self.bin_cap_index.set(scratch.index.capacity() as u64);
        let bins = Bins {
            start: std::mem::take(&mut scratch.start),
            items: std::mem::take(&mut scratch.items),
        };
        HeldBins { scratch, bins }
    }

    pub(crate) fn restore_bins(&self, mut held: HeldBins) {
        held.scratch.start = held.bins.start;
        held.scratch.items = held.bins.items;
        *self.bin_scratch.lock() = held.scratch;
    }

    pub(crate) fn cell_coords_clamped(&self, p: Vec3) -> (usize, usize, usize) {
        cell_coords_clamped(&self.dims, p)
    }

    fn cell_index_clamped(&self, p: Vec3) -> usize {
        let (i, j, k) = self.cell_coords_clamped(p);
        self.dims.cell(i, j, k)
    }

    fn clamp_domain(&self, p: Vec3) -> Vec3 {
        let eps = self.dims.dx * 1e-3;
        p.clamp(
            self.dims.origin + Vec3::splat(eps),
            self.dims.max() - Vec3::splat(eps),
        )
    }

    /// Fill emitters once; sources every step.
    fn emit(&mut self) {
        let _span = tracing::info_span!(target: "regia_fluid", "fluid_emit").entered();
        for e in 0..self.emitters.len() {
            let emitter = self.emitters[e];
            match emitter.kind {
                EmitterKind::Fill if !emitter.done => {
                    self.fill_shape(&emitter.shape, emitter.velocity, None);
                    self.emitters[e].done = true;
                }
                EmitterKind::Fill => {}
                EmitterKind::Source => {
                    for (p, v) in self.pos.iter().zip(self.vel.iter_mut()) {
                        if emitter.shape.contains(*p) {
                            *v = emitter.velocity;
                        }
                    }
                    let held = self.hold_bins();
                    self.fill_shape(&emitter.shape, emitter.velocity, Some(&held.bins));
                    self.restore_bins(held);
                }
            }
        }
    }

    /// Seeds the cells inside `shape` with 2 x 2 x 2 jittered particles. With `bins`, a cell is only
    /// topped up to eight.
    fn fill_shape(&mut self, shape: &Shape, velocity: Vec3, bins: Option<&Bins>) {
        let (lo, hi) = shape.aabb();
        let d = &self.dims;
        let to_cell = |v: f32, n: usize| -> usize {
            if v.is_finite() {
                (v.max(0.0) as usize).min(n.saturating_sub(1))
            } else {
                0
            }
        };
        let (ql, qh) = (d.local(lo), d.local(hi));
        let (i0, j0, k0) = (to_cell(ql.x, d.nx), to_cell(ql.y, d.ny), to_cell(ql.z, d.nz));
        let (i1, j1, k1) = (to_cell(qh.x, d.nx), to_cell(qh.y, d.ny), to_cell(qh.z, d.nz));
        let dx = d.dx;
        let half = dx * 0.5;
        for k in k0..=k1 {
            for j in j0..=j1 {
                for i in i0..=i1 {
                    let c = d.cell(i, j, k);
                    let have = bins.map_or(0, |b| b.count(c));
                    if have >= PARTICLES_PER_CELL {
                        continue;
                    }
                    let corner = d.origin + Vec3::new(i as f32, j as f32, k as f32) * dx;
                    let mut slots = PARTICLES_PER_CELL - have;
                    for s in 0..PARTICLES_PER_CELL {
                        if slots == 0 || self.pos.len() >= self.params.particle_cap {
                            break;
                        }
                        let sub = Vec3::new((s & 1) as f32, ((s >> 1) & 1) as f32, ((s >> 2) & 1) as f32);
                        let jitter =
                            Vec3::new(self.rng.signed(), self.rng.signed(), self.rng.signed()) * 0.25;
                        let p = corner + (sub + Vec3::splat(0.5) + jitter) * half;
                        if !shape.contains(p) || self.solid.distance_near_cached(p, 0.0) < 0.0 {
                            continue;
                        }
                        self.pos.push(self.clamp_domain(p));
                        self.vel.push(velocity);
                        slots -= 1;
                    }
                }
            }
        }
    }

    fn apply_sinks(&mut self) {
        if self.sinks.is_empty() {
            return;
        }
        let hit = self.pos.iter().any(|p| self.sinks.iter().any(|s| s.contains(*p)));
        if !hit {
            return;
        }
        let sinks = std::mem::take(&mut self.sinks);
        self.fill_keep_mask(|p, _| !sinks.iter().any(|s| s.contains(p)));
        self.sinks = sinks;
        let keep = std::mem::take(&mut self.keep_mask);
        retain_by(&mut self.pos, &keep);
        retain_by(&mut self.vel, &keep);
        self.keep_mask = keep;
    }

    fn drop_non_finite(&mut self) {
        let bad = self
            .pos
            .iter()
            .zip(&self.vel)
            .any(|(p, v)| !p.is_finite() || !v.is_finite());
        if !bad {
            return;
        }
        #[cfg(test)]
        KEEP_MASKS.with(|n| n.set(n.get() + 1));
        self.fill_keep_mask(|p, v| p.is_finite() && v.is_finite());
        let dropped = self.keep_mask.iter().filter(|k| !**k).count();
        self.dropped += dropped as u64;
        let keep = std::mem::take(&mut self.keep_mask);
        retain_by(&mut self.pos, &keep);
        retain_by(&mut self.vel, &keep);
        self.keep_mask = keep;
    }

    /// Fills [`Self::keep_mask`] for the current particles, counting a growth of its capacity.
    fn fill_keep_mask(&mut self, mut keep: impl FnMut(Vec3, Vec3) -> bool) {
        let n = self.pos.len();
        if self.keep_mask.capacity() < n {
            self.drop_mask_allocs.add(1);
        }
        self.keep_mask.clear();
        self.keep_mask.reserve(n);
        for i in 0..n {
            let bit = keep(self.pos[i], self.vel[i]);
            self.keep_mask.push(bit);
        }
    }

    /// Particles to grid: trilinear weights, normalised per face.
    fn particles_to_grid(&mut self) {
        let _span = tracing::info_span!(target: "regia_fluid", "fluid_p2g").entered();
        for axis in 0..3 {
            {
                let field = &mut self.grid.c[axis];
                let weight = &mut self.weight[axis];
                field.data.iter_mut().for_each(|v| *v = 0.0);
                weight.iter_mut().for_each(|v| *v = 0.0);
                for (p, v) in self.pos.iter().zip(&self.vel) {
                    // A particle that went non-finite is dropped at the end of the step; it must
                    // not splat NaN into the faces its neighbours read first.
                    if !(p.is_finite() && v.is_finite()) {
                        continue;
                    }
                    let q = self.dims.local(*p);
                    for (i, w) in field.weights(q) {
                        field.data[i] += w * v[axis];
                        weight[i] += w;
                    }
                }
                for (value, w) in field.data.iter_mut().zip(weight.iter()) {
                    if *w > 0.0 {
                        *value /= *w;
                    }
                }
            }
            let n = self.grid.c[axis].data.len();
            let mut valid = std::mem::take(&mut self.extrap_valid);
            let mut next = std::mem::take(&mut self.extrap_next);
            let mut out = std::mem::take(&mut self.extrap_out);
            grow_scratch(&mut valid, n, false, &self.velocity_scratch_grows);
            for (slot, w) in valid.iter_mut().zip(self.weight[axis].iter()) {
                *slot = *w > 0.0;
            }
            extrapolate(
                &mut self.grid.c[axis],
                &mut valid,
                &mut next,
                &mut out,
                &self.velocity_scratch_grows,
            );
            set_closed_faces(
                &mut self.grid.c[axis],
                &self.solid.face_open[axis],
                &self.solid.face_velocity[axis],
            );
            self.extrap_valid = valid;
            self.extrap_next = next;
            self.extrap_out = out;
        }
    }

    fn mark_liquid(&mut self) {
        self.liquid.iter_mut().for_each(|l| *l = false);
        for p in &self.pos {
            // A non-finite position bins to cell 0. It is dropped at the end of the step and must
            // not mark that cell liquid for the pressure solve.
            if !p.is_finite() {
                continue;
            }
            let c = self.cell_index_clamped(*p);
            self.liquid[c] = true;
        }
        if self.solid.has_solids() {
            self.mark_solid_cells_liquid();
        }
    }

    /// A cell whose centre is inside a solid but that shares an open face with a particle cell
    /// counts as liquid (Batty, Bertails and Bridson 2007 extend the liquid into the solid). Left as
    /// air, its `p = 0` would sit against the liquid at the solid's surface and drain it into the
    /// wall; as liquid, its open faces get the solid's boundary velocity through the projection
    /// (its closed faces carry no weight), so nothing flows through the wall (ADR-2577).
    fn mark_solid_cells_liquid(&mut self) {
        let d = self.dims;
        let n = [d.nx, d.ny, d.nz];
        let mut extra = Vec::new();
        for k in 0..d.nz {
            for j in 0..d.ny {
                for i in 0..d.nx {
                    let c = d.cell(i, j, k);
                    if self.liquid[c] {
                        continue;
                    }
                    let id = [i, j, k];
                    let mut touches = false;
                    'faces: for axis in 0..3 {
                        for side in 0..2 {
                            let mut face = id;
                            face[axis] += side;
                            let f = self.grid.c[axis].idx(face[0], face[1], face[2]);
                            if self.solid.face_open[axis][f] <= 0.0 {
                                continue;
                            }
                            let mut nb = id;
                            if side == 0 {
                                if nb[axis] == 0 {
                                    continue;
                                }
                                nb[axis] -= 1;
                            } else {
                                nb[axis] += 1;
                                if nb[axis] >= n[axis] {
                                    continue;
                                }
                            }
                            if self.liquid[d.cell(nb[0], nb[1], nb[2])] {
                                touches = true;
                                break 'faces;
                            }
                        }
                    }
                    if touches && self.solid.distance_near_cached(d.cell_center(i, j, k), 0.0) < 0.0 {
                        extra.push(c);
                    }
                }
            }
        }
        for c in extra {
            self.liquid[c] = true;
        }
    }

    fn add_forces(&mut self, dt: f32) {
        let a = self.params.gravity + self.params.external_acceleration;
        for axis in 0..3 {
            let delta = a[axis] * dt;
            if delta == 0.0 {
                continue;
            }
            let open = &self.solid.face_open[axis];
            for (value, o) in self.grid.c[axis].data.iter_mut().zip(open) {
                if *o > 0.0 {
                    *value += delta;
                }
            }
        }
    }

    /// The cells on the two sides of face `(i, j, k)` of `axis`, if inside the grid.
    fn face_cells(&self, axis: usize, i: usize, j: usize, k: usize) -> (Option<usize>, Option<usize>) {
        let id = [i, j, k];
        let n = [self.dims.nx, self.dims.ny, self.dims.nz];
        let lo = (id[axis] > 0).then(|| {
            let mut c = id;
            c[axis] -= 1;
            self.dims.cell(c[0], c[1], c[2])
        });
        let hi = (id[axis] < n[axis]).then(|| self.dims.cell(i, j, k));
        (lo, hi)
    }

    /// Pressure projection: builds the variational Poisson system over the liquid cells and
    /// subtracts the pressure gradient from the open faces.
    fn project(&mut self, dt: f32) {
        let _span = tracing::info_span!(target: "regia_fluid", "fluid_pressure").entered();
        let d = self.dims;
        let n = [d.nx, d.ny, d.nz];
        self.poisson.n = n;
        self.poisson.diag.fill(0.0);
        for plus in &mut self.poisson.plus {
            plus.fill(0.0);
        }
        self.rhs.fill(0.0);
        let k_coef = f64::from(dt) / (f64::from(self.params.density) * f64::from(d.dx));
        for kk in 0..d.nz {
            for jj in 0..d.ny {
                for ii in 0..d.nx {
                    let c = d.cell(ii, jj, kk);
                    if !self.liquid[c] {
                        continue;
                    }
                    let mut diag = 0.0f64;
                    let mut div = 0.0f64;
                    for axis in 0..3 {
                        let field = &self.grid.c[axis];
                        let open = &self.solid.face_open[axis];
                        let id = [ii, jj, kk];
                        // The lower face has the cell's own index; the upper one is +1 on `axis`.
                        let mut up = id;
                        up[axis] += 1;
                        let f_lo = field.idx(id[0], id[1], id[2]);
                        let f_hi = field.idx(up[0], up[1], up[2]);
                        let (w_lo, w_hi) = (f64::from(open[f_lo]), f64::from(open[f_hi]));
                        div += w_hi * f64::from(field.data[f_hi]) - w_lo * f64::from(field.data[f_lo]);
                        // The solid share of each face moves at the solid's velocity (zero for
                        // static solids, which leave the sum untouched).
                        let solid_vel = &self.solid.face_velocity[axis];
                        let (s_lo, s_hi) = (solid_vel[f_lo], solid_vel[f_hi]);
                        if s_hi != 0.0 {
                            div += (1.0 - w_hi) * f64::from(s_hi);
                        }
                        if s_lo != 0.0 {
                            div -= (1.0 - w_lo) * f64::from(s_lo);
                        }
                        diag += w_lo + w_hi;
                        if up[axis] < n[axis] {
                            let nb = d.cell(up[0], up[1], up[2]);
                            if self.liquid[nb] {
                                self.poisson.plus[axis][c] = -w_hi;
                            }
                        }
                    }
                    if diag > 0.0 {
                        self.poisson.diag[c] = diag;
                        self.rhs[c] = -div / k_coef;
                    }
                }
            }
        }
        // A liquid cell sealed on every side has a zero row: it keeps p = 0 and no coupling.
        for axis in 0..3 {
            for c in 0..d.cells() {
                if self.poisson.diag[c] == 0.0 {
                    self.poisson.plus[axis][c] = 0.0;
                }
            }
        }
        for axis in 0..3 {
            let stride = match axis {
                0 => 1,
                1 => d.nx,
                _ => d.nx * d.ny,
            };
            for c in 0..d.cells() {
                if self.poisson.plus[axis][c] != 0.0 && self.poisson.diag[c + stride] == 0.0 {
                    self.poisson.plus[axis][c] = 0.0;
                }
            }
        }
        let report = self.poisson.solve_with(
            &self.rhs,
            &mut self.pressure,
            &mut self.cg,
            self.params.pcg_max_iterations,
            self.params.pcg_tolerance,
        );
        self.last = report;
        // Velocity update across every open face next to the liquid.
        for axis in 0..3 {
            let size = self.grid.c[axis].size;
            for k in 0..size[2] {
                for j in 0..size[1] {
                    for i in 0..size[0] {
                        let f = self.grid.c[axis].idx(i, j, k);
                        if self.solid.face_open[axis][f] <= 0.0 {
                            self.grid.c[axis].data[f] = self.solid.face_velocity[axis][f];
                            continue;
                        }
                        let (lo, hi) = self.face_cells(axis, i, j, k);
                        let liquid = |c: Option<usize>| c.is_some_and(|c| self.liquid[c]);
                        if !liquid(lo) && !liquid(hi) {
                            continue;
                        }
                        let p = |c: Option<usize>| {
                            c.map_or(0.0, |c| if self.liquid[c] { self.pressure[c] } else { 0.0 })
                        };
                        let grad = p(hi) - p(lo);
                        self.grid.c[axis].data[f] -= (k_coef * grad) as f32;
                    }
                }
            }
        }
        // Remaining divergence of the liquid cells.
        let mut worst = 0.0f32;
        for kk in 0..d.nz {
            for jj in 0..d.ny {
                for ii in 0..d.nx {
                    let c = d.cell(ii, jj, kk);
                    if !self.liquid[c] || self.poisson.diag[c] == 0.0 {
                        continue;
                    }
                    let mut div = 0.0f32;
                    for axis in 0..3 {
                        let field = &self.grid.c[axis];
                        let open = &self.solid.face_open[axis];
                        let id = [ii, jj, kk];
                        let mut up = id;
                        up[axis] += 1;
                        let f_lo = field.idx(id[0], id[1], id[2]);
                        let f_hi = field.idx(up[0], up[1], up[2]);
                        div += open[f_hi] * field.data[f_hi] - open[f_lo] * field.data[f_lo];
                        let solid_vel = &self.solid.face_velocity[axis];
                        if solid_vel[f_hi] != 0.0 {
                            div += (1.0 - open[f_hi]) * solid_vel[f_hi];
                        }
                        if solid_vel[f_lo] != 0.0 {
                            div -= (1.0 - open[f_lo]) * solid_vel[f_lo];
                        }
                    }
                    worst = worst.max((div / d.dx).abs());
                }
            }
        }
        self.divergence_max = worst;
    }

    /// Two-way coupling (ADR-2577): the pressure force on each moving solid is the adjoint of the
    /// solid term of the divergence (Batty, Bertails and Bridson 2007, § 5). A face whose closed
    /// share `1 - open` belongs to solid `s` pushes `s` along the face's axis with
    /// `(1 - open) (p_lo - p_hi) dx²` (air and solid cells have `p = 0`) and turns it with that
    /// force's moment about the solid's position at the start of the step. Summed in f64 in face order and
    /// multiplied by the substep's `dt`, so the step's impulse is deterministic.
    fn accumulate_solid_impulses(&mut self, dt: f32, wetted: &mut [u32]) {
        if self.solid.moving.is_empty() {
            return;
        }
        let _span = tracing::info_span!(target: "regia_fluid", "fluid_couple_solids").entered();
        let d = self.dims;
        let area_dt = f64::from(d.dx) * f64::from(d.dx) * f64::from(dt);
        for axis in 0..3 {
            let size = self.grid.c[axis].size;
            for k in 0..size[2] {
                for j in 0..size[1] {
                    for i in 0..size[0] {
                        let f = self.grid.c[axis].idx(i, j, k);
                        let owner = self.solid.face_owner[axis][f];
                        if owner == NO_OWNER {
                            continue;
                        }
                        let closed = 1.0 - f64::from(self.solid.face_open[axis][f]);
                        if closed <= 0.0 {
                            continue;
                        }
                        let (lo, hi) = self.face_cells(axis, i, j, k);
                        let liquid = |c: Option<usize>| c.is_some_and(|c| self.liquid[c]);
                        if !liquid(lo) && !liquid(hi) {
                            continue;
                        }
                        let p = |c: Option<usize>| {
                            c.map_or(0.0, |c| if self.liquid[c] { self.pressure[c] } else { 0.0 })
                        };
                        let impulse = closed * (p(lo) - p(hi)) * area_dt;
                        let o = usize::from(owner);
                        // The moment is taken about the start-of-step position (`self.moving`
                        // is advanced only after the substeps), the point the physics coupling
                        // applies the step's impulse at. The posed copy in `self.solid.moving`
                        // moves every substep, and moments about different points do not add.
                        let Some(start) = self.moving.get(o) else {
                            continue;
                        };
                        let mut local = Vec3::new(i as f32, j as f32, k as f32) + Vec3::splat(0.5);
                        local[axis] -= 0.5;
                        let r = d.origin + local * d.dx - start.position;
                        let (rx, ry, rz) = (f64::from(r.x), f64::from(r.y), f64::from(r.z));
                        let Some(sum) = self.impulse_sums.get_mut(o) else {
                            continue;
                        };
                        sum[axis] += impulse;
                        // r x (impulse along `axis`).
                        match axis {
                            0 => {
                                sum[4] += rz * impulse;
                                sum[5] -= ry * impulse;
                            }
                            1 => {
                                sum[3] -= rz * impulse;
                                sum[5] += rx * impulse;
                            }
                            _ => {
                                sum[3] += ry * impulse;
                                sum[4] -= rx * impulse;
                            }
                        }
                        if let Some(w) = wetted.get_mut(o) {
                            *w += 1;
                        }
                    }
                }
            }
        }
    }

    /// Extends the projected velocity from the faces next to liquid into the air.
    fn extrapolate_after_projection(&mut self) {
        let _span = tracing::info_span!(target: "regia_fluid", "fluid_extrapolate").entered();
        for axis in 0..3 {
            let size = self.grid.c[axis].size;
            let n = self.grid.c[axis].data.len();
            let mut valid = std::mem::take(&mut self.extrap_valid);
            let mut next = std::mem::take(&mut self.extrap_next);
            let mut out = std::mem::take(&mut self.extrap_out);
            grow_scratch(&mut valid, n, false, &self.velocity_scratch_grows);
            for k in 0..size[2] {
                for j in 0..size[1] {
                    for i in 0..size[0] {
                        let (lo, hi) = self.face_cells(axis, i, j, k);
                        let f = self.grid.c[axis].idx(i, j, k);
                        valid[f] = lo.is_some_and(|c| self.liquid[c]) || hi.is_some_and(|c| self.liquid[c]);
                    }
                }
            }
            extrapolate(
                &mut self.grid.c[axis],
                &mut valid,
                &mut next,
                &mut out,
                &self.velocity_scratch_grows,
            );
            set_closed_faces(
                &mut self.grid.c[axis],
                &self.solid.face_open[axis],
                &self.solid.face_velocity[axis],
            );
            self.extrap_valid = valid;
            self.extrap_next = next;
            self.extrap_out = out;
        }
    }

    /// Grid to particles: the PIC/FLIP blend.
    fn grid_to_particles(&mut self) {
        let _span = tracing::info_span!(target: "regia_fluid", "fluid_g2p").entered();
        let flip = self.params.flip_ratio;
        for (p, v) in self.pos.iter().zip(self.vel.iter_mut()) {
            let q = self.dims.local(*p);
            let new = self.grid.sample(q);
            let old = self.old.sample(q);
            *v = (*v + (new - old)) * flip + new * (1.0 - flip);
        }
    }

    /// RK2 (midpoint) advection through the grid velocity.
    fn advect(&mut self, dt: f32) {
        let _span = tracing::info_span!(target: "regia_fluid", "fluid_advect").entered();
        for p in &mut self.pos {
            let v1 = self.grid.sample(self.dims.local(*p));
            let mid = *p + v1 * (0.5 * dt);
            let v2 = self.grid.sample(self.dims.local(mid));
            *p += v2 * dt;
        }
    }

    /// Position correction against volume loss: particle pairs closer than a share of the rest
    /// spacing are pushed apart (a Jacobi pass over the cell bins, applied at once). Only pairs in
    /// interior cells move, and a particle stays inside its cell: the pass undoes clumping without
    /// moving mass between cells (that is the pressure solve's job) or spreading the surface into
    /// the air.
    fn separate(&mut self) {
        if self.params.separation <= 0.0 || self.pos.is_empty() {
            return;
        }
        let _span = tracing::info_span!(target: "regia_fluid", "fluid_separate").entered();
        let held = self.hold_bins();
        let bins = &held.bins;
        let d = self.dims;
        // The closest pair the 2 x 2 x 2 jittered seeding makes (half the rest spacing): a fresh fill
        // is left as it is.
        let r0 = d.dx * 0.25;
        let r0_sq = r0 * r0;
        let strength = self.params.separation * 0.5;
        // One atomic add per pair showed up at hundred-thousand-particle scale: counted locally,
        // added once.
        let mut pair_visits = 0u64;
        let mut delta = vec![Vec3::ZERO; self.pos.len()];
        let liquid: Vec<bool> = (0..d.cells()).map(|c| bins.count(c) > 0).collect();
        let mut interior = vec![false; d.cells()];
        for k in 0..d.nz {
            for j in 0..d.ny {
                for i in 0..d.nx {
                    let c = d.cell(i, j, k);
                    interior[c] = liquid[c] && self.interior(&liquid, i, j, k);
                }
            }
        }
        for k in 0..d.nz {
            for j in 0..d.ny {
                for i in 0..d.nx {
                    let c = d.cell(i, j, k);
                    let here = capped_cell(bins, c);
                    if here.is_empty() || !interior[c] {
                        continue;
                    }
                    // Pairs inside this cell and with the 13 "forward" neighbour cells, once each.
                    // Only the first [`SEPARATE_CELL_CAP`] particles of a cell enter; the rest wait
                    // for the reseed thin.
                    for (n, &a) in here.iter().enumerate() {
                        for &b in &here[n + 1..] {
                            pair_visits += 1;
                            push_pair(&self.pos, &mut delta, a, b, r0, r0_sq, strength);
                        }
                    }
                    for (di, dj, dk) in FORWARD_NEIGHBOURS {
                        let (ni, nj, nk) = (i as i64 + di, j as i64 + dj, k as i64 + dk);
                        if ni < 0
                            || nj < 0
                            || nk < 0
                            || ni >= d.nx as i64
                            || nj >= d.ny as i64
                            || nk >= d.nz as i64
                        {
                            continue;
                        }
                        let nc = d.cell(ni as usize, nj as usize, nk as usize);
                        if !interior[nc] {
                            continue;
                        }
                        let other = capped_cell(bins, nc);
                        for &a in here {
                            for &b in other {
                                pair_visits += 1;
                                push_pair(&self.pos, &mut delta, a, b, r0, r0_sq, strength);
                            }
                        }
                    }
                }
            }
        }
        let margin = d.dx * 1e-3;
        for (p, dp) in self.pos.iter_mut().zip(&delta) {
            if *dp == Vec3::ZERO {
                continue;
            }
            let q = d.local(*p);
            let corner = d.origin + q.floor() * d.dx;
            *p = (*p + *dp).clamp(corner + Vec3::splat(margin), corner + Vec3::splat(d.dx - margin));
        }
        self.pair_visits.add(pair_visits);
        self.restore_bins(held);
    }

    /// Pushes particles out of the solids and inside the domain, dropping the velocity into them.
    fn resolve_boundaries(&mut self) {
        let _span = tracing::info_span!(target: "regia_fluid", "fluid_boundaries").entered();
        let margin = self.dims.dx * 0.05;
        let eps = self.dims.dx * 1e-3;
        let lo = self.dims.origin + Vec3::splat(eps);
        let hi = self.dims.max() - Vec3::splat(eps);
        let has_solids = self.solid.has_solids();
        let has_moving = !self.solid.moving.is_empty();
        for (p, v) in self.pos.iter_mut().zip(self.vel.iter_mut()) {
            if has_solids {
                let phi = self.solid.distance_near_cached(*p, margin);
                if phi < margin {
                    let n = self.solid.normal_cached(*p, self.dims.dx * 0.25);
                    if n != Vec3::ZERO {
                        // Relative to the solid's own velocity: a moving wall carries the liquid
                        // it touches along.
                        let vs = if has_moving {
                            self.solid.velocity_at(*p)
                        } else {
                            Vec3::ZERO
                        };
                        *p += n * (margin - phi);
                        let vn = (*v - vs).dot(n);
                        if vn < 0.0 {
                            *v -= n * vn;
                        }
                    }
                }
            }
            // -1 / +1: clamped onto the low / high wall of that axis.
            let mut walls = [0i8; 3];
            for a in 0..3 {
                if p[a] < lo[a] {
                    p[a] = lo[a];
                    v[a] = v[a].max(0.0);
                    walls[a] = -1;
                } else if p[a] > hi[a] {
                    p[a] = hi[a];
                    v[a] = v[a].min(0.0);
                    walls[a] = 1;
                }
            }
            if has_solids && walls != [0; 3] {
                // The clamp can undo the push out of a solid that reaches through the wall (its
                // nearest surface lies outside the domain): slide along the wall instead.
                for _ in 0..WALL_SLIDE_ITERATIONS {
                    let phi = self.solid.distance_cached(*p);
                    if phi >= margin {
                        break;
                    }
                    let mut n = self.solid.normal_cached(*p, self.dims.dx * 0.25);
                    for a in 0..3 {
                        if (walls[a] < 0 && n[a] < 0.0) || (walls[a] > 0 && n[a] > 0.0) {
                            n[a] = 0.0;
                        }
                    }
                    let len = crate::math::sqrt_f32(n.length_squared());
                    // Dead centre: the outward normal is perpendicular to every free wall. A fixed
                    // +axis can point deeper in (audfix2-fluid-g, e168a0226): step along the
                    // inward wall normal instead (below).
                    let (n, step) = if len > 0.0 {
                        (n / len, margin - phi)
                    } else {
                        // The wall cancelled the outward normal. Step into the domain along the
                        // inward wall normal (low wall is +axis, high wall is -axis). A tangent
                        // step stays inside a solid that still fills the wall.
                        let mut inward = Vec3::ZERO;
                        for a in 0..3 {
                            if walls[a] < 0 {
                                inward[a] = 1.0;
                            } else if walls[a] > 0 {
                                inward[a] = -1.0;
                            }
                        }
                        let ilen = crate::math::sqrt_f32(inward.length_squared());
                        if ilen > 0.0 {
                            (inward / ilen, margin - phi)
                        } else {
                            (Vec3::ZERO, 0.0)
                        }
                    };
                    let vs = if has_moving {
                        self.solid.velocity_at(*p)
                    } else {
                        Vec3::ZERO
                    };
                    *p = (*p + n * step).clamp(lo, hi);
                    let vn = (*v - vs).dot(n);
                    if vn < 0.0 {
                        *v -= n * vn;
                    }
                }
            }
        }
    }

    /// Keeps liquid cells near eight particles: tops up interior cells below four, thins any cell
    /// above twelve back to eight (the last in bin order go). Surface cells are not topped up (adding
    /// there would add volume).
    fn reseed(&mut self) {
        let _span = tracing::info_span!(target: "regia_fluid", "fluid_reseed").entered();
        let held = self.hold_bins();
        let bins = &held.bins;
        let d = self.dims;
        let mut liquid = vec![false; d.cells()];
        for (c, l) in liquid.iter_mut().enumerate() {
            *l = bins.count(c) > 0;
        }
        let mut remove = vec![false; self.pos.len()];
        let mut add: Vec<(Vec3, Vec3)> = Vec::new();
        let dx = d.dx;
        for k in 0..d.nz {
            for j in 0..d.ny {
                for i in 0..d.nx {
                    let c = d.cell(i, j, k);
                    let n = bins.count(c);
                    if n == 0 {
                        continue;
                    }
                    if n > THIN_ABOVE {
                        for &p in &bins.cell(c)[PARTICLES_PER_CELL..] {
                            remove[p as usize] = true;
                        }
                        continue;
                    }
                    if n >= PARTICLES_PER_CELL / 2 || !self.interior(&liquid, i, j, k) {
                        continue;
                    }
                    let corner = d.origin + Vec3::new(i as f32, j as f32, k as f32) * dx;
                    for _ in n..PARTICLES_PER_CELL {
                        let p = corner + Vec3::new(self.rng.unit(), self.rng.unit(), self.rng.unit()) * dx;
                        if self.solid.distance_near_cached(p, 0.0) < 0.0 {
                            continue;
                        }
                        let v = self.grid.sample(d.local(p));
                        add.push((p, v));
                    }
                }
            }
        }
        if remove.iter().any(|r| *r) {
            let keep: Vec<bool> = remove.iter().map(|r| !r).collect();
            retain_by(&mut self.pos, &keep);
            retain_by(&mut self.vel, &keep);
        }
        for (p, v) in add {
            if self.pos.len() >= self.params.particle_cap {
                break;
            }
            self.pos.push(p);
            self.vel.push(v);
        }
        self.restore_bins(held);
    }

    /// Whether every face of cell `(i, j, k)` leads to liquid or is closed.
    fn interior(&self, liquid: &[bool], i: usize, j: usize, k: usize) -> bool {
        let d = &self.dims;
        let id = [i, j, k];
        let n = [d.nx, d.ny, d.nz];
        for axis in 0..3 {
            for side in 0..2 {
                let mut face = id;
                face[axis] += side;
                let f = self.grid.c[axis].idx(face[0], face[1], face[2]);
                if self.solid.face_open[axis][f] <= 0.0 {
                    continue;
                }
                let mut nb = id;
                if side == 0 {
                    if nb[axis] == 0 {
                        continue;
                    }
                    nb[axis] -= 1;
                } else {
                    nb[axis] += 1;
                    if nb[axis] >= n[axis] {
                        continue;
                    }
                }
                if !liquid[d.cell(nb[0], nb[1], nb[2])] {
                    return false;
                }
            }
        }
        true
    }

    /// Liquid volume (see [`FluidStats::volume`]).
    #[must_use]
    pub fn liquid_volume(&self) -> f32 {
        let held = self.hold_bins();
        let bins = &held.bins;
        let d = self.dims;
        let cell = d.dx * d.dx * d.dx;
        let liquid: Vec<bool> = (0..d.cells()).map(|c| bins.count(c) > 0).collect();
        let mut fill = 0.0f64;
        for k in 0..d.nz {
            for j in 0..d.ny {
                for i in 0..d.nx {
                    let c = d.cell(i, j, k);
                    let n = bins.count(c);
                    if n == 0 {
                        continue;
                    }
                    fill += if self.interior(&liquid, i, j, k) {
                        1.0
                    } else {
                        (n as f64 / PARTICLES_PER_CELL as f64).min(1.0)
                    };
                }
            }
        }
        let volume = (fill * f64::from(cell)) as f32;
        self.restore_bins(held);
        volume
    }

    /// Fill height of every (x, z) column, `sum_j min(1, n / 8) * dx` above the grid floor, x fastest.
    #[must_use]
    pub fn column_heights(&self) -> Vec<f32> {
        let held = self.hold_bins();
        let bins = &held.bins;
        let d = &self.dims;
        let mut out = vec![0.0f32; d.nx * d.nz];
        for k in 0..d.nz {
            for i in 0..d.nx {
                let mut h = 0.0f32;
                for j in 0..d.ny {
                    h += (bins.count(d.cell(i, j, k)) as f32 / PARTICLES_PER_CELL as f32).min(1.0);
                }
                out[i + d.nx * k] = h * d.dx;
            }
        }
        self.restore_bins(held);
        out
    }

    /// The current counts and measures.
    #[must_use]
    pub fn stats(&self) -> FluidStats {
        let volume = self.liquid_volume();
        let cell = self.dims.dx * self.dims.dx * self.dims.dx;
        let nominal = self.pos.len() as f32 / PARTICLES_PER_CELL as f32 * cell;
        let counts = self.whitewater.counts();
        FluidStats {
            steps: self.steps,
            particles: self.pos.len(),
            volume,
            nominal_volume: nominal,
            volume_drift: if nominal > 0.0 {
                volume / nominal - 1.0
            } else {
                0.0
            },
            divergence_max: self.divergence_max,
            pcg_iterations: self.last.iterations,
            pcg_residual: self.last.residual,
            pcg_converged: self.last.converged,
            substeps: self.substeps,
            liquid_cells: self.liquid.iter().filter(|l| **l).count(),
            dropped_non_finite: self.dropped,
            foam: counts[1],
            spray: counts[2],
            bubbles: counts[3],
            whitewater_generated: self.whitewater.generated_last_step(),
            moving_solids: self.moving.len(),
            wetted_faces: self.impulses.iter().map(|i| i.wetted_faces).sum(),
        }
    }

    /// A digest of the full state: equal digests mean bit-identical runs whose next step reads
    /// the same inputs.
    ///
    /// The hash covers the step count, the particles, the whitewater (particles and generator
    /// state), the moving solids and their impulses, the solver settings, the emitters (their
    /// `done` flags), the sinks, the static colliders and both generator states: two sims that
    /// would diverge on the next step (a pending fill, a different gravity, an advanced
    /// generator) must not hash equal (ADR-4753).
    #[must_use]
    pub fn state_digest(&self) -> u64 {
        let mut h = Fnv1a::new();
        h.write(&DIGEST_VERSION.to_le_bytes());
        h.write(&self.steps.to_le_bytes());
        hash_params(&mut h, &self.params);
        h.write(&self.rng.state().to_le_bytes());
        h.write(&(self.emitters.len() as u64).to_le_bytes());
        for e in &self.emitters {
            hash_shape(&mut h, &e.shape);
            for x in e.velocity.to_array() {
                h.write_f32(x);
            }
            h.write(&[e.kind as u8, e.done as u8]);
        }
        h.write(&(self.sinks.len() as u64).to_le_bytes());
        for s in &self.sinks {
            hash_shape(&mut h, s);
        }
        h.write(&(self.solid.colliders.len() as u64).to_le_bytes());
        for c in &self.solid.colliders {
            hash_collider(&mut h, c);
        }
        h.write(&(self.pos.len() as u64).to_le_bytes());
        for (p, v) in self.pos.iter().zip(&self.vel) {
            for x in p.to_array().into_iter().chain(v.to_array()) {
                h.write_f32(x);
            }
        }
        self.whitewater.hash_into(&mut h);
        h.write(&(self.moving.len() as u64).to_le_bytes());
        for s in &self.moving {
            hash_solid_shape(&mut h, &s.shape);
            for x in s
                .position
                .to_array()
                .into_iter()
                .chain(s.rotation.to_array())
                .chain(s.linear_velocity.to_array())
                .chain(s.angular_velocity.to_array())
            {
                h.write_f32(x);
            }
            h.write(&[s.motion as u8]);
            h.write(&s.tag.to_le_bytes());
        }
        // Hashed under its own length: before the first step `impulses` is empty while
        // `moving` is not, and a zip of the two would hash nothing about the solids.
        h.write(&(self.impulses.len() as u64).to_le_bytes());
        for i in &self.impulses {
            for x in i.linear.to_array().into_iter().chain(i.angular.to_array()) {
                h.write_f32(x);
            }
            h.write(&i.wetted_faces.to_le_bytes());
        }
        h.finish()
    }

    /// The grid velocity after the last step (world units; sampling helper for whitewater and tests).
    #[must_use]
    pub fn grid_velocity(&self, p: Vec3) -> Vec3 {
        self.grid.sample(self.dims.local(p))
    }

    /// Whether cell `(i, j, k)` held liquid in the last projection.
    #[must_use]
    pub fn is_liquid(&self, i: usize, j: usize, k: usize) -> bool {
        i < self.dims.nx && j < self.dims.ny && k < self.dims.nz && self.liquid[self.dims.cell(i, j, k)]
    }

    /// Replaces the particles (the GPU path's read-back and the tests use this).
    pub fn set_particles(&mut self, pos: Vec<Vec3>, vel: Vec<Vec3>) {
        let n = pos.len().min(vel.len()).min(self.params.particle_cap);
        self.pos = pos;
        self.vel = vel;
        self.pos.truncate(n);
        self.vel.truncate(n);
    }

    /// Mutable access to the step counter for the GPU path, which advances steps itself.
    #[cfg(feature = "gpu")]
    pub(crate) fn advance_step_counter(&mut self, by: u64) {
        self.steps += by;
        // The GPU path computes no solid impulses: the last CPU step's are not "the last step"
        // any more (`solid_impulses` is empty until the next CPU step).
        self.impulses.clear();
        // The moving solids moved through those steps on the GPU too.
        let t = self.params.dt * by as f32;
        for s in &mut self.moving {
            *s = s.advanced(t);
        }
    }
}

/// The cell of `p` on `dims`, clamped into the grid (a non-finite coordinate goes to 0).
fn cell_coords_clamped(dims: &GridDims, p: Vec3) -> (usize, usize, usize) {
    let q = dims.local(p);
    let clamp = |v: f32, n: usize| {
        if v.is_finite() {
            (v.max(0.0) as usize).min(n - 1)
        } else {
            0
        }
    };
    (clamp(q.x, dims.nx), clamp(q.y, dims.ny), clamp(q.z, dims.nz))
}

/// Bins `pos` by cell of `dims` (a stable counting sort; points outside the grid go to the
/// nearest cell). Only cached-frame playback (`draw`) bins positions outside a simulator.
#[cfg(feature = "draw")]
pub(crate) fn bin_positions(dims: &GridDims, pos: &[Vec3]) -> Bins {
    let cells = dims.cells();
    let mut count = vec![0u32; cells + 1];
    let index: Vec<usize> = pos
        .iter()
        .map(|p| {
            let (i, j, k) = cell_coords_clamped(dims, *p);
            dims.cell(i, j, k)
        })
        .collect();
    for &c in &index {
        count[c + 1] += 1;
    }
    for c in 0..cells {
        count[c + 1] += count[c];
    }
    let start = count.clone();
    let mut fill = count;
    let mut items = vec![0u32; pos.len()];
    for (p, &c) in index.iter().enumerate() {
        items[fill[c] as usize] = p as u32;
        fill[c] += 1;
    }
    Bins { start, items }
}

/// Neighbour cell offsets with a lexicographically positive direction: each unordered cell pair once.
const FORWARD_NEIGHBOURS: [(i64, i64, i64); 13] = [
    (1, 0, 0),
    (-1, 1, 0),
    (0, 1, 0),
    (1, 1, 0),
    (-1, -1, 1),
    (0, -1, 1),
    (1, -1, 1),
    (-1, 0, 1),
    (0, 0, 1),
    (1, 0, 1),
    (-1, 1, 1),
    (0, 1, 1),
    (1, 1, 1),
];

/// The first [`SEPARATE_CELL_CAP`] particles of cell `c`, in bin order.
fn capped_cell(bins: &Bins, c: usize) -> &[u32] {
    let all = bins.cell(c);
    let n = all.len().min(SEPARATE_CELL_CAP);
    &all[..n]
}

/// Counting sort of `pos` into `scratch`. Grows a buffer only when its capacity is short.
///
/// `slots` is the particle cap. The per-particle tables are sized to `max(slots, pos.len())`
/// on the first fill, so a later step whose live set is still inside the cap does not allocate.
fn fill_bin_scratch(scratch: &mut BinScratch, pos: &[Vec3], dims: GridDims, slots: usize, grows: &Tally) {
    let cells = dims.cells();
    let n = pos.len();
    let slots = slots.max(n);
    let BinScratch {
        count,
        index,
        start,
        items,
        fill,
    } = scratch;
    crate::tally::fit(count, cells, 0, grows);
    count[..cells].fill(0);
    crate::tally::fit(index, slots, 0, grows);
    let clamp = |v: f32, limit: usize| {
        if v.is_finite() {
            (v.max(0.0) as usize).min(limit.saturating_sub(1))
        } else {
            0
        }
    };
    for (i, p) in pos.iter().enumerate() {
        let q = dims.local(*p);
        let c = dims.cell(clamp(q.x, dims.nx), clamp(q.y, dims.ny), clamp(q.z, dims.nz));
        index[i] = c;
        count[c] += 1;
    }
    crate::tally::fit(start, cells + 1, 0, grows);
    start[0] = 0;
    for c in 0..cells {
        start[c + 1] = start[c] + count[c];
    }
    crate::tally::fit(fill, cells, 0, grows);
    fill[..cells].copy_from_slice(&start[..cells]);
    crate::tally::fit(items, slots, 0, grows);
    for (i, &c) in index[..n].iter().enumerate() {
        let slot = fill[c] as usize;
        items[slot] = i as u32;
        fill[c] += 1;
    }
    // Length stays the live set. Capacity stays at the cap, so the next step does not allocate,
    // and `Bins::items` is not padded out to the cap.
    index.truncate(n);
    items.truncate(n);
}

fn push_pair(pos: &[Vec3], delta: &mut [Vec3], a: u32, b: u32, r0: f32, r0_sq: f32, strength: f32) {
    let (a, b) = (a as usize, b as usize);
    let (pa, pb) = (pos[a], pos[b]);
    // A non-finite position bins into cell 0 and makes `dist_sq` NaN, which fails the range test
    // (`NaN >= r0_sq` is false). The NaN correction then clamps the partner onto the cell corner.
    if !(pa.is_finite() && pb.is_finite()) {
        return;
    }
    let d = pb - pa;
    let dist_sq = d.length_squared();
    if dist_sq >= r0_sq {
        return;
    }
    // The software sqrt: platform `f32::sqrt` results may differ by an ulp, which would move
    // the correction by an ulp per pair (ADR-4753).
    let dist = crate::math::sqrt_f32(dist_sq);
    let dir = if dist > r0 * 1e-4 {
        d / dist
    } else {
        // Coincident: a fixed direction from the pair's indices, so the push is still deterministic.
        [Vec3::X, Vec3::Y, Vec3::Z][(a + b) % 3]
    };
    let push = dir * ((r0 - dist) * strength);
    delta[a] -= push;
    delta[b] += push;
}

/// Grows `v` to at least `n` elements. Counts one growth when the capacity is short.
fn grow_scratch<T: Clone>(v: &mut Vec<T>, n: usize, fill: T, grows: &Tally) {
    if v.len() >= n {
        return;
    }
    if v.capacity() < n {
        grows.add(1);
    }
    v.resize(n, fill);
}

/// Extends `field` from its `valid` faces into the rest, `EXTRAPOLATION_LAYERS` breadth-first
/// layers, each face taking the mean of its valid neighbours (Jacobi: one layer reads the last).
///
/// `valid`, `next` and `out` are reused. Only the prefix of `field.data.len()` is read or written,
/// so a buffer sized for the longest axis serves the shorter ones.
fn extrapolate(
    field: &mut FaceField,
    valid: &mut Vec<bool>,
    next: &mut Vec<bool>,
    out: &mut Vec<f32>,
    grows: &Tally,
) {
    let n = field.data.len();
    grow_scratch(valid, n, false, grows);
    grow_scratch(next, n, false, grows);
    grow_scratch(out, n, 0.0, grows);
    let [sx, sy, sz] = field.size;
    for _ in 0..EXTRAPOLATION_LAYERS {
        next[..n].copy_from_slice(&valid[..n]);
        out[..n].copy_from_slice(&field.data[..n]);
        let mut changed = false;
        for k in 0..sz {
            for j in 0..sy {
                for i in 0..sx {
                    let f = field.idx(i, j, k);
                    if valid[f] {
                        continue;
                    }
                    let mut sum = 0.0;
                    let mut count = 0u32;
                    let mut visit = |ii: usize, jj: usize, kk: usize| {
                        let g = field.idx(ii, jj, kk);
                        if valid[g] {
                            sum += field.data[g];
                            count += 1;
                        }
                    };
                    if i > 0 {
                        visit(i - 1, j, k);
                    }
                    if i + 1 < sx {
                        visit(i + 1, j, k);
                    }
                    if j > 0 {
                        visit(i, j - 1, k);
                    }
                    if j + 1 < sy {
                        visit(i, j + 1, k);
                    }
                    if k > 0 {
                        visit(i, j, k - 1);
                    }
                    if k + 1 < sz {
                        visit(i, j, k + 1);
                    }
                    if count > 0 {
                        out[f] = sum / count as f32;
                        next[f] = true;
                        changed = true;
                    }
                }
            }
        }
        field.data[..n].copy_from_slice(&out[..n]);
        valid[..n].copy_from_slice(&next[..n]);
        if !changed {
            break;
        }
    }
    // Faces still unreached carry no velocity.
    for (v, ok) in field.data[..n].iter_mut().zip(valid[..n].iter()) {
        if !ok {
            *v = 0.0;
        }
    }
}

/// Closed faces carry the solid's velocity (zero for static solids and the domain walls).
fn set_closed_faces(field: &mut FaceField, open: &[f32], solid_velocity: &[f32]) {
    for ((value, o), vs) in field.data.iter_mut().zip(open).zip(solid_velocity) {
        if *o <= 0.0 {
            *value = *vs;
        }
    }
}

fn retain_by<T>(items: &mut Vec<T>, keep: &[bool]) {
    let mut n = 0;
    items.retain(|_| {
        let k = keep[n];
        n += 1;
        k
    });
}

/// Hashes every field of `params`: the step reads them all (directly or through the pressure
/// solve and the whitewater), so each one must separate digests.
fn hash_params(h: &mut Fnv1a, p: &FluidParams) {
    for v in [p.bounds_min, p.bounds_max, p.gravity, p.external_acceleration] {
        for x in v.to_array() {
            h.write_f32(x);
        }
    }
    for x in [p.cell_size, p.dt, p.flip_ratio, p.density, p.cfl, p.separation] {
        h.write_f32(x);
    }
    h.write(&p.pcg_max_iterations.to_le_bytes());
    h.write(&p.pcg_tolerance.to_le_bytes());
    h.write(&p.max_substeps.to_le_bytes());
    h.write(&[p.reseed as u8]);
    h.write(&(p.particle_cap as u64).to_le_bytes());
    h.write(&p.seed.to_le_bytes());
    let w = &p.whitewater;
    h.write(&[w.enabled as u8]);
    h.write(&(w.max_foam as u64).to_le_bytes());
    h.write(&(w.max_spray as u64).to_le_bytes());
    h.write(&(w.max_bubbles as u64).to_le_bytes());
    for range in [w.trapped_air, w.wave_crest, w.kinetic] {
        for x in range {
            h.write_f32(x);
        }
    }
    for x in [
        w.rate_trapped_air,
        w.rate_wave_crest,
        w.foam_lifetime,
        w.buoyancy,
        w.drag,
    ] {
        h.write_f32(x);
    }
    h.write(&w.spray_neighbours.to_le_bytes());
    h.write(&w.bubble_neighbours.to_le_bytes());
}

/// Hashes a [`Shape`].
fn hash_shape(h: &mut Fnv1a, shape: &Shape) {
    match *shape {
        Shape::Box { center, half_extents } => {
            h.write(&0u8.to_le_bytes());
            for v in [center, half_extents] {
                for x in v.to_array() {
                    h.write_f32(x);
                }
            }
        }
        Shape::Sphere { center, radius } => {
            h.write(&1u8.to_le_bytes());
            for x in center.to_array() {
                h.write_f32(x);
            }
            h.write_f32(radius);
        }
    }
}

/// Hashes a moving solid's [`SolidShape`] (the shape discriminant and its sizes: two solids at
/// the same pose with different shapes must not hash equal).
fn hash_solid_shape(h: &mut Fnv1a, shape: &SolidShape) {
    match *shape {
        SolidShape::Box { half_extents } => {
            h.write(&0u8.to_le_bytes());
            for x in half_extents.to_array() {
                h.write_f32(x);
            }
        }
        SolidShape::Sphere { radius } => {
            h.write(&1u8.to_le_bytes());
            h.write_f32(radius);
        }
        SolidShape::Capsule { half_height, radius } => {
            h.write(&2u8.to_le_bytes());
            h.write_f32(half_height);
            h.write_f32(radius);
        }
    }
}

/// Hashes a static [`Collider`].
fn hash_collider(h: &mut Fnv1a, collider: &Collider) {
    match *collider {
        Collider::Box { center, half_extents } => {
            h.write(&0u8.to_le_bytes());
            for v in [center, half_extents] {
                for x in v.to_array() {
                    h.write_f32(x);
                }
            }
        }
        Collider::Sphere { center, radius } => {
            h.write(&1u8.to_le_bytes());
            for x in center.to_array() {
                h.write_f32(x);
            }
            h.write_f32(radius);
        }
        Collider::Heightfield {
            origin,
            spacing,
            columns,
            rows,
            ref heights,
        } => {
            h.write(&2u8.to_le_bytes());
            for x in origin {
                h.write_f32(x);
            }
            h.write_f32(spacing);
            h.write(&(columns as u64).to_le_bytes());
            h.write(&(rows as u64).to_le_bytes());
            h.write(&(heights.len() as u64).to_le_bytes());
            for x in heights {
                h.write_f32(*x);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::solid::{Collider, MovingSolid, SolidShape};

    /// (Audit G-fluid #4) Over two forced substeps of a translating solid, the angular impulse is
    /// taken about the solid's start-of-step position (the point the physics coupling applies it
    /// at), not about each substep's moved pose: with a pressure field symmetric about the moving
    /// centre it equals `sum_s (position(t_s) - position(0)) x J_s`.
    #[test]
    fn translating_solid_torque_uses_start_pose() {
        let params = FluidParams {
            bounds_min: Vec3::ZERO,
            bounds_max: Vec3::splat(0.4),
            cell_size: 0.05,
            ..FluidParams::default()
        };
        let mut sim = FluidSim::new(params).expect("sim");
        let mut solid = MovingSolid::kinematic(
            SolidShape::Box {
                half_extents: Vec3::splat(0.075),
            },
            Vec3::splat(0.2),
        );
        solid.linear_velocity = Vec3::new(1.0, 0.0, 0.0);
        sim.set_moving_solids(vec![solid]).expect("solid");
        // Every cell liquid, a hydrostatic pressure (linear in y): symmetric about the box centre
        // in x at every substep, since each substep moves it exactly one cell.
        let d = sim.dims;
        sim.liquid.iter_mut().for_each(|l| *l = true);
        for k in 0..d.nz {
            for j in 0..d.ny {
                for i in 0..d.nx {
                    let y = (j as f64 + 0.5) * f64::from(d.dx);
                    sim.pressure[d.cell(i, j, k)] = 1000.0 * 9.81 * (0.4 - y);
                }
            }
        }
        let dt = 0.05f32;
        sim.impulse_sums = vec![[0.0; 6]];
        let mut wetted = vec![0u32];
        let mut expected = 0.0f64;
        for s in 0..2 {
            sim.rasterize_solids(dt * s as f32);
            let before = sim.impulse_sums[0];
            sim.accumulate_solid_impulses(dt, &mut wetted);
            let j_y = sim.impulse_sums[0][1] - before[1];
            assert!(j_y > 0.0, "substep {s}: buoyancy {j_y}");
            // (position(t_s) - position(0)) x (0, J_y, 0), z component (the speed is 1 m/s).
            expected += f64::from(dt * s as f32) * j_y;
        }
        let angular_z = sim.impulse_sums[0][5];
        assert!(
            (angular_z - expected).abs() <= 1e-4 * expected.abs(),
            "angular impulse z {angular_z}, expected {expected} about the start pose"
        );
    }

    /// A box whose bounding radius sits on a one-ulp split between `f32::sqrt` and the software
    /// square root must take the software side: that is the CFL substep count every platform
    /// shares (ADR-4753). These bits are the split on the lane host (platform radius stays at
    /// one substep, software radius needs two).
    #[test]
    fn box_cfl_follows_the_software_radius() {
        let params = FluidParams {
            bounds_min: Vec3::ZERO,
            bounds_max: Vec3::splat(0.4),
            cell_size: 0.1,
            dt: 0.1,
            cfl: 1.0,
            max_substeps: 4,
            reseed: false,
            ..FluidParams::default()
        };
        let mut sim = FluidSim::new(params).expect("sim");
        let mut solid = MovingSolid::kinematic(
            SolidShape::Box {
                half_extents: Vec3::new(
                    f32::from_bits(0x3ffe_2bfb),
                    f32::from_bits(0x3ec0_6920),
                    f32::from_bits(0x3fe1_569e),
                ),
            },
            Vec3::splat(0.2),
        );
        solid.angular_velocity = Vec3::new(f32::from_bits(0x3ebf_07b3), 0.0, 0.0);
        sim.set_moving_solids(vec![solid]).expect("solid");
        assert_eq!(
            sim.substep_count(),
            2,
            "software box radius crosses the CFL limit"
        );
    }

    /// A solid that reaches through a domain wall must not keep a particle that the outward push
    /// clamped back onto that wall. The slide has to step into the domain, not along a tangent
    /// the solid still fills.
    #[test]
    fn wall_crossing_solid_releases_the_particle() {
        let params = FluidParams {
            bounds_min: Vec3::ZERO,
            bounds_max: Vec3::splat(0.4),
            cell_size: 0.1,
            reseed: false,
            ..FluidParams::default()
        };
        let collider = Collider::Box {
            center: Vec3::new(0.1, 0.2, 0.2),
            half_extents: Vec3::new(0.2, 0.25, 0.25),
        };
        let mut sim = FluidSim::with_colliders(params, vec![collider]).expect("sim");
        let added = sim.add_particles(&[(Vec3::new(0.02, 0.2, 0.2), Vec3::ZERO)]);
        assert_eq!(added, 1);
        sim.resolve_boundaries();
        let p = sim.positions()[0];
        let phi = sim.solid().distance(p);
        let margin = sim.dims().dx * 0.05;
        assert!(
            phi >= margin - 1.0e-3,
            "particle stayed inside the wall solid at {p:?}, phi {phi}, margin {margin}"
        );
        let (lo, hi) = (sim.dims().origin, sim.dims().max());
        assert!(p.cmpge(lo).all() && p.cmple(hi).all(), "left the domain: {p:?}");
    }

    /// The moving-solid wall slide in `fluid_flip.wgsl` has to stay valid WGSL. This is the same
    /// parse and DX12 limit check the engine shader list runs; it does not execute the kernel.
    #[test]
    fn fluid_flip_wgsl_passes_the_shader_check() {
        let problems = regia_render::shader_hlsl::wgsl_dx12_problems(&[(
            "fluid_flip.wgsl",
            include_str!("fluid_flip.wgsl"),
        )]);
        assert!(problems.is_empty(), "{problems:#?}");
    }

    /// A step whose particles stay finite must not build the keep-mask (one bool per particle).
    #[test]
    fn finite_step_allocates_no_keep_mask() {
        let params = FluidParams {
            bounds_min: Vec3::ZERO,
            bounds_max: Vec3::splat(0.2),
            cell_size: 0.05,
            gravity: Vec3::ZERO,
            external_acceleration: Vec3::ZERO,
            separation: 0.0,
            reseed: false,
            max_substeps: 1,
            ..FluidParams::default()
        };
        let mut sim = FluidSim::new(params).expect("sim");
        let particles: Vec<(Vec3, Vec3)> = (0..8)
            .map(|i| {
                (
                    Vec3::new(0.04 + (i % 4) as f32 * 0.03, 0.1, 0.05 + (i / 4) as f32 * 0.05),
                    Vec3::ZERO,
                )
            })
            .collect();
        assert_eq!(sim.add_particles(&particles), 8);
        KEEP_MASKS.with(|n| n.set(0));
        sim.step();
        let masks = KEEP_MASKS.with(|n| n.get());
        assert_eq!(masks, 0, "keep-mask allocations {masks}");
        assert_eq!(sim.dropped, 0);
        assert_eq!(sim.pos.len(), 8);
    }
}
