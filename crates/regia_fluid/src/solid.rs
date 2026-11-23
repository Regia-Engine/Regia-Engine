// SPDX-License-Identifier: MPL-2.0
//! Solid boundaries as a signed-distance field (negative inside), and the open-face fractions of
//! the variational pressure solve (Batty, Bertails and Bridson 2007).
//!
//! Static [`Collider`]s are sampled on the grid nodes once. [`MovingSolid`]s (kinematic or
//! dynamic rigid bodies, ADR-2577) are rasterized into the same node field every substep: the
//! static field is restored, each moving solid's analytic distance is folded in over its bounding
//! box, the open fractions of the faces there are recomputed, and every face a moving solid
//! (partly) closes gets that solid's velocity as its boundary velocity. The pressure projection
//! uses that velocity in the solid share `1 - open` of the face, so liquid moves with a moving
//! wall and does not flow through it.

use glam::{Quat, Vec3};

use crate::grid::GridDims;
use crate::tally::Tally;

/// One static collider.
#[derive(Debug, Clone, PartialEq)]
pub enum Collider {
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
    /// A heightfield (terrain): solid below `height(x, z)`. Heights are on a regular grid of
    /// `columns x rows` samples, `spacing` apart, from `origin` (x, z); outside it the edge sample holds.
    Heightfield {
        /// World x, z of sample (0, 0).
        origin: [f32; 2],
        /// Sample spacing in metres.
        spacing: f32,
        /// Samples along x.
        columns: usize,
        /// Samples along z.
        rows: usize,
        /// `columns * rows` heights, x fastest.
        heights: Vec<f32>,
    },
}

impl Collider {
    /// Signed distance from `p` (negative inside). The heightfield answers the vertical distance,
    /// a bound that is exact on flat ground and conservative on slopes. The euclidean parts run
    /// on the software sqrt (ADR-4753): these distances reach the open-face fractions and the
    /// digest, so a platform `sqrt` ulp would fork runs.
    #[must_use]
    pub fn distance(&self, p: Vec3) -> f32 {
        match self {
            Self::Box { center, half_extents } => box_distance(p - *center, *half_extents),
            Self::Sphere { center, radius } => crate::math::len(p - *center) - radius,
            Self::Heightfield {
                origin,
                spacing,
                columns,
                rows,
                heights,
            } => {
                // `FluidSim::with_colliders` refuses these; a hand-built field still answers.
                let samples = columns.checked_mul(*rows);
                if *columns == 0 || *rows == 0 || samples.is_none_or(|n| heights.len() < n) || *spacing <= 0.0
                {
                    return f32::MAX;
                }
                let fx = ((p.x - origin[0]) / spacing).clamp(0.0, (*columns - 1) as f32);
                let fz = ((p.z - origin[1]) / spacing).clamp(0.0, (*rows - 1) as f32);
                let fx = if fx.is_finite() { fx } else { 0.0 };
                let fz = if fz.is_finite() { fz } else { 0.0 };
                // Past 2^24 columns `(columns - 1) as f32` can round up to `columns`: clamp the
                // index too.
                let (i0, k0) = (
                    (fx.floor() as usize).min(columns - 1),
                    (fz.floor() as usize).min(rows - 1),
                );
                let (i1, k1) = ((i0 + 1).min(columns - 1), (k0 + 1).min(rows - 1));
                let (tx, tz) = (fx - i0 as f32, fz - k0 as f32);
                let h = |i: usize, k: usize| heights[i + columns * k];
                let a = h(i0, k0) * (1.0 - tx) + h(i1, k0) * tx;
                let b = h(i0, k1) * (1.0 - tx) + h(i1, k1) * tx;
                p.y - (a * (1.0 - tz) + b * tz)
            }
        }
    }

    /// Why the collider cannot be used, if it cannot: a heightfield needs a finite origin, a finite
    /// positive spacing, at least one column and row, and `columns * rows` finite heights (the
    /// product must not overflow). Samples past that count are not read. Box and sphere are not
    /// checked here.
    pub(crate) fn problem(&self) -> Option<&'static str> {
        let Self::Heightfield {
            origin,
            spacing,
            columns,
            rows,
            heights,
        } = self
        else {
            return None;
        };
        if !(origin[0].is_finite() && origin[1].is_finite()) {
            return Some("heightfield origin must be finite");
        }
        if !(spacing.is_finite() && *spacing > 0.0) {
            return Some("heightfield spacing must be finite and positive");
        }
        if *columns == 0 || *rows == 0 {
            return Some("heightfield needs at least one column and one row");
        }
        match columns.checked_mul(*rows) {
            None => Some("heightfield columns * rows overflows"),
            Some(n) if heights.len() < n => Some("heightfield has fewer than columns * rows heights"),
            Some(n) if heights[..n].iter().any(|h| !h.is_finite()) => {
                Some("heightfield heights must be finite")
            }
            Some(_) => None,
        }
    }
}

/// Signed distance of a box of `half` extents centred at the origin, at local point `d`.
fn box_distance(d: Vec3, half: Vec3) -> f32 {
    let q = d.abs() - half;
    let outside = crate::math::len(q.max(Vec3::ZERO));
    let inside = q.x.max(q.y).max(q.z).min(0.0);
    outside + inside
}

/// The shape of a [`MovingSolid`] in its own frame (centred on its position, rotated by its
/// rotation).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SolidShape {
    /// A box.
    Box {
        /// Half extents along the local axes.
        half_extents: Vec3,
    },
    /// A sphere.
    Sphere {
        /// Radius.
        radius: f32,
    },
    /// A capsule along the local y axis.
    Capsule {
        /// Half length of the cylinder part.
        half_height: f32,
        /// Radius.
        radius: f32,
    },
}

impl SolidShape {
    /// Signed distance at local point `q` (negative inside).
    #[must_use]
    pub fn local_distance(&self, q: Vec3) -> f32 {
        match *self {
            Self::Box { half_extents } => box_distance(q, half_extents),
            Self::Sphere { radius } => crate::math::len(q) - radius,
            Self::Capsule { half_height, radius } => {
                let y = q.y.clamp(-half_height, half_height);
                crate::math::len(q - Vec3::new(0.0, y, 0.0)) - radius
            }
        }
    }

    /// Radius of a sphere around the local origin that holds the whole shape.
    #[must_use]
    pub fn bounding_radius(&self) -> f32 {
        match *self {
            // Software sqrt: this radius is the `|w| r` term of the CFL substep count, and
            // platform `f32::sqrt` can sit on the other side of that limit (ADR-4753).
            Self::Box { half_extents } => crate::math::len(half_extents),
            Self::Sphere { radius } => radius,
            Self::Capsule { half_height, radius } => half_height + radius,
        }
    }

    /// Volume (m³).
    #[must_use]
    pub fn volume(&self) -> f32 {
        match *self {
            Self::Box { half_extents } => 8.0 * half_extents.x * half_extents.y * half_extents.z,
            Self::Sphere { radius } => 4.0 / 3.0 * std::f32::consts::PI * radius * radius * radius,
            Self::Capsule { half_height, radius } => {
                std::f32::consts::PI * radius * radius * (2.0 * half_height + 4.0 / 3.0 * radius)
            }
        }
    }

    fn is_valid(&self) -> bool {
        let ok = |x: f32| x.is_finite() && (0.0..=MAX_SOLID_EXTENT).contains(&x);
        match *self {
            Self::Box { half_extents } => ok(half_extents.x) && ok(half_extents.y) && ok(half_extents.z),
            Self::Sphere { radius } => ok(radius),
            Self::Capsule { half_height, radius } => ok(half_height) && ok(radius),
        }
    }
}

/// Largest half extent, radius or half height of a moving solid (metres).
pub const MAX_SOLID_EXTENT: f32 = 1000.0;

/// Largest linear (m/s) or angular (rad/s) speed of a moving solid.
pub const MAX_SOLID_SPEED: f32 = 1000.0;

/// How a moving solid's motion is decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SolidMotion {
    /// Driven by gameplay (a paddle, a door): the liquid follows it, nothing pushes it back.
    #[default]
    Kinematic,
    /// Moved by the physics solver: the liquid's pressure impulses are applied back to it
    /// (two-way coupling).
    Dynamic,
}

/// A rigid solid moving through the liquid: a kinematic or dynamic physics body, rasterized into
/// the solid field every substep (ADR-2577).
///
/// The pose and velocities are the ones at the start of the fluid step; inside the step the solver
/// advances the pose by the velocities for each substep.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MovingSolid {
    /// Shape in the solid's frame.
    pub shape: SolidShape,
    /// World position of the shape's centre.
    pub position: Vec3,
    /// World rotation.
    pub rotation: Quat,
    /// Linear velocity (m/s).
    pub linear_velocity: Vec3,
    /// Angular velocity (rad/s, world axes).
    pub angular_velocity: Vec3,
    /// Kinematic or dynamic.
    pub motion: SolidMotion,
    /// The caller's tag (the plugin stores the body entity's bits), copied into
    /// [`SolidImpulse::tag`].
    pub tag: u64,
}

impl MovingSolid {
    /// A kinematic solid at rest at `position`.
    #[must_use]
    pub fn kinematic(shape: SolidShape, position: Vec3) -> Self {
        Self {
            shape,
            position,
            rotation: Quat::IDENTITY,
            linear_velocity: Vec3::ZERO,
            angular_velocity: Vec3::ZERO,
            motion: SolidMotion::Kinematic,
            tag: 0,
        }
    }

    /// Signed distance at world point `p` (negative inside).
    #[must_use]
    pub fn distance(&self, p: Vec3) -> f32 {
        self.shape
            .local_distance(self.rotation.inverse() * (p - self.position))
    }

    /// The solid's velocity at world point `p` (rigid motion: `v + w x r`).
    #[must_use]
    pub fn velocity_at(&self, p: Vec3) -> Vec3 {
        self.linear_velocity + self.angular_velocity.cross(p - self.position)
    }

    /// The pose `t` seconds later, moving at the current velocities.
    #[must_use]
    pub fn advanced(&self, t: f32) -> Self {
        let mut out = *self;
        if t == 0.0 {
            return out;
        }
        out.position += self.linear_velocity * t;
        let turn = self.angular_velocity * t;
        if turn != Vec3::ZERO {
            // The software sincos and sqrt: platform trig would put different rotation bits into
            // the SDF and the digest on different targets (ADR-4753).
            out.rotation =
                crate::math::normalize_quat(crate::math::quat_from_scaled_axis(turn) * self.rotation);
        }
        out
    }

    /// World bounding box (a sphere bound, so rotation does not matter).
    #[must_use]
    pub fn aabb(&self) -> (Vec3, Vec3) {
        let r = Vec3::splat(self.shape.bounding_radius());
        (self.position - r, self.position + r)
    }

    /// Whether every field is finite and in range. The speed check runs on the software sqrt
    /// (ADR-4753) so validation cannot accept a solid on one platform and reject it on another.
    ///
    /// The rotation must be a unit quaternion within tolerance: `distance` applies it to the
    /// point offset, which is only a rotation for |q| = 1, while `moving_lower` keeps assuming
    /// the shape sits inside its unscaled bounding sphere — a non-unit quaternion silently
    /// breaks that bound and can skip a solid the point is actually inside.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.shape.is_valid()
            && self.position.is_finite()
            && self.position.abs().max_element() <= 1.0e6
            && self.rotation.is_finite()
            && (self.rotation.length_squared() - 1.0).abs() <= 0.001
            && self.linear_velocity.is_finite()
            && self.angular_velocity.is_finite()
            && crate::math::len(self.linear_velocity) <= MAX_SOLID_SPEED
            && crate::math::len(self.angular_velocity) <= MAX_SOLID_SPEED
    }
}

/// The impulse the liquid's pressure gave one moving solid over the last step (two-way coupling).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SolidImpulse {
    /// The solid's tag ([`MovingSolid::tag`]).
    pub tag: u64,
    /// Linear impulse (N s, world).
    pub linear: Vec3,
    /// Angular impulse about the solid's position at the start of the step (N m s, world).
    pub angular: Vec3,
    /// Faces of the solid's surface that touched liquid in the last step's substeps.
    pub wetted_faces: u32,
}

/// No moving solid owns the face.
pub const NO_OWNER: u16 = u16::MAX;

/// The union of the static colliders and the moving solids of the current substep.
#[derive(Debug, Clone, PartialEq)]
pub struct SolidField {
    /// The static colliders. A host may replace, push or remove entries between queries; after
    /// raising heights inside an existing heightfield's buffer, call [`Self::refresh_heights`].
    pub colliders: Vec<Collider>,
    /// Signed distance at every grid node, `(nx + 1) * (ny + 1) * (nz + 1)`, x fastest.
    pub node_phi: Vec<f32>,
    /// Open fraction of every face of each axis (0 = closed, 1 = fully open). The domain walls
    /// are closed.
    pub face_open: [Vec<f32>; 3],
    /// Boundary velocity of every face of each axis (the face's axis component of the owning moving
    /// solid's velocity at the face centre; 0 for static solids, walls and open faces).
    pub face_velocity: [Vec<f32>; 3],
    /// The moving solid that owns each face (index into [`SolidField::moving`]), or [`NO_OWNER`].
    pub face_owner: [Vec<u16>; 3],
    /// The moving solids at the current substep's pose.
    pub moving: Vec<MovingSolid>,
    static_phi: Vec<f32>,
    static_open: [Vec<f32>; 3],
    rasterized: bool,
    /// Analytic signed-distance evaluations (static colliders and moving solids).
    distance_evals: Tally,
    /// Highest sample of each heightfield (`0` for every other collider). Reused while the
    /// collider's key (variant, heightfield dimensions, its `heights` buffer and a probe of its
    /// samples) is unchanged and the ceiling's sample still holds it, so a collider replaced in
    /// place or pushed into a reused buffer — the fields are public — is not answered from a stale
    /// ceiling ([`Self::refresh_heights`] for other in-place edits). Equal to every other cache:
    /// it is derived from `colliders`.
    height_max: HeightCache,
}

/// Subsamples per face edge when integrating the open fraction.
pub const FACE_SUBSAMPLES: usize = 4;

impl SolidField {
    /// Samples `colliders` on `dims`.
    #[must_use]
    pub fn new(dims: &GridDims, colliders: Vec<Collider>) -> Self {
        let _span = tracing::info_span!(target: "regia_fluid", "fluid_solid_field").entered();
        let (nx, ny, nz) = (dims.nx + 1, dims.ny + 1, dims.nz + 1);
        let mut node_phi = vec![f32::MAX; nx * ny * nz];
        if !colliders.is_empty() {
            for k in 0..nz {
                for j in 0..ny {
                    for i in 0..nx {
                        let p = dims.origin + Vec3::new(i as f32, j as f32, k as f32) * dims.dx;
                        node_phi[i + nx * (j + ny * k)] = distance_of(&colliders, p);
                    }
                }
            }
        }
        let faces = |axis: usize| {
            let s = dims.face_dims(axis);
            s[0] * s[1] * s[2]
        };
        let height_max = HeightCache::from_colliders(&colliders);
        let mut field = Self {
            colliders,
            node_phi,
            face_open: [Vec::new(), Vec::new(), Vec::new()],
            face_velocity: [vec![0.0; faces(0)], vec![0.0; faces(1)], vec![0.0; faces(2)]],
            face_owner: [
                vec![NO_OWNER; faces(0)],
                vec![NO_OWNER; faces(1)],
                vec![NO_OWNER; faces(2)],
            ],
            moving: Vec::new(),
            static_phi: Vec::new(),
            static_open: [Vec::new(), Vec::new(), Vec::new()],
            rasterized: false,
            distance_evals: Tally::default(),
            height_max,
        };
        for axis in 0..3 {
            field.face_open[axis] = field.open_fractions(dims, axis);
        }
        field.static_phi = field.node_phi.clone();
        field.static_open = field.face_open.clone();
        field
    }

    /// Analytic signed-distance evaluations since this field was built.
    ///
    /// Construction samples the nodes before the count a step snapshots. A later call measures
    /// only what that call did.
    #[must_use]
    pub fn distance_evals(&self) -> u64 {
        self.distance_evals.get()
    }

    /// Signed distance of the union at world `p` (`f32::MAX` with no solid).
    ///
    /// A collider whose cheap lower bound cannot beat the best distance so far is not evaluated.
    /// The value is the analytic union distance.
    #[must_use]
    pub fn distance(&self, p: Vec3) -> f32 {
        self.height_max.sync(&self.colliders);
        self.distance_cached(p)
    }

    /// [`Self::distance`] without re-checking the heightfield ceilings against
    /// [`Self::colliders`]: for callers whose colliders have not changed since [`Self::new`] or
    /// the last sync. `FluidSim` keeps its field private and never edits the colliders, so the
    /// solver's per-particle queries skip the check.
    pub(crate) fn distance_cached(&self, p: Vec3) -> f32 {
        let mut best = f32::MAX;
        for (index, collider) in self.colliders.iter().enumerate() {
            if collider_lower(collider, self.height_max.get(index), p) >= best {
                continue;
            }
            self.distance_evals.add(1);
            best = best.min(collider.distance(p));
        }
        for solid in &self.moving {
            if moving_lower(solid, p) >= best {
                continue;
            }
            self.distance_evals.add(1);
            best = best.min(solid.distance(p));
        }
        best
    }

    /// `distance(p)` when any solid might be inside `near`, otherwise a value `>= near`.
    ///
    /// The boundary push only needs to know whether the particle is inside the margin. Solids whose
    /// lower bound is already past `near` are not evaluated.
    #[must_use]
    pub fn distance_near(&self, p: Vec3, near: f32) -> f32 {
        self.height_max.sync(&self.colliders);
        self.distance_near_cached(p, near)
    }

    /// [`Self::distance_near`] without the ceiling check (see `distance_cached`).
    pub(crate) fn distance_near_cached(&self, p: Vec3, near: f32) -> f32 {
        let clear = self
            .colliders
            .iter()
            .enumerate()
            .all(|(index, collider)| collider_lower(collider, self.height_max.get(index), p) >= near)
            && self.moving.iter().all(|solid| moving_lower(solid, p) >= near);
        if clear { near } else { self.distance_cached(p) }
    }

    /// The open fractions of the faces of `axis` from the static colliders alone (empty for an
    /// axis above 2).
    #[must_use]
    pub fn static_open(&self, axis: usize) -> &[f32] {
        self.static_open.get(axis).map_or(&[], Vec::as_slice)
    }

    /// Recomputes every heightfield's cached ceiling from all its samples.
    ///
    /// [`Self::distance`] and [`Self::distance_near`] notice a replaced, resized or re-dimensioned
    /// collider, and a new field pushed into a freed or recycled buffer, by a cheap per-query key
    /// (the buffer, its length, a probe of 16 evenly spaced samples, and the sample the ceiling
    /// came from). An edit of [`Self::colliders`] that changes heights inside the same buffer
    /// without touching those samples can raise the field unseen: call this after one. Costs one
    /// pass over every heightfield's samples.
    pub fn refresh_heights(&mut self) {
        self.height_max.rebuild(&self.colliders);
    }

    /// Distance of the static colliders only.
    #[must_use]
    pub fn static_distance(&self, p: Vec3) -> f32 {
        distance_of(&self.colliders, p)
    }

    /// Whether there is any static collider or moving solid.
    #[must_use]
    pub fn has_solids(&self) -> bool {
        !self.colliders.is_empty() || !self.moving.is_empty()
    }

    /// The velocity of the solid nearest to `p`: the nearest moving solid's rigid velocity when it
    /// is nearer than every static collider, else zero.
    #[must_use]
    pub fn velocity_at(&self, p: Vec3) -> Vec3 {
        match self.nearest_moving(p) {
            Some((index, d)) if d <= self.static_distance(p) => self.moving[index].velocity_at(p),
            _ => Vec3::ZERO,
        }
    }

    fn nearest_moving(&self, p: Vec3) -> Option<(usize, f32)> {
        let mut best: Option<(usize, f32)> = None;
        for (index, s) in self.moving.iter().enumerate() {
            let d = s.distance(p);
            if best.is_none_or(|(_, b)| d < b) {
                best = Some((index, d));
            }
        }
        best
    }

    /// Outward unit normal of the union at `p` (central differences), or zero where flat.
    ///
    /// The length runs through the software `crate::math::sqrt_f32` (via `crate::math::len`),
    /// not the platform `sqrt`: this normal drives the boundary push-out in
    /// `crate::sim`, so a platform ulp fork would move the pinned digests.
    #[must_use]
    pub fn normal(&self, p: Vec3, h: f32) -> Vec3 {
        self.height_max.sync(&self.colliders);
        self.normal_cached(p, h)
    }

    /// [`Self::normal`] without the ceiling check (see `distance_cached`).
    pub(crate) fn normal_cached(&self, p: Vec3, h: f32) -> Vec3 {
        let dx = self.distance_cached(p + Vec3::X * h) - self.distance_cached(p - Vec3::X * h);
        let dy = self.distance_cached(p + Vec3::Y * h) - self.distance_cached(p - Vec3::Y * h);
        let dz = self.distance_cached(p + Vec3::Z * h) - self.distance_cached(p - Vec3::Z * h);
        let g = Vec3::new(dx, dy, dz);
        let l = crate::math::len(g);
        if l > 0.0 { g / l } else { Vec3::ZERO }
    }

    /// Rasterizes `solids` (already posed for this substep) over the static field: restores the
    /// static node distances and open fractions, folds each solid's distance into the nodes of its
    /// bounding box, recomputes the open fractions there and gives each face a moving solid
    /// (partly) closes that solid's velocity.
    pub fn rasterize(&mut self, dims: &GridDims, solids: &[MovingSolid]) {
        if solids.is_empty() && !self.rasterized {
            self.moving.clear();
            return;
        }
        let _span = tracing::info_span!(target: "regia_fluid", "fluid_rasterize_solids").entered();
        self.node_phi.copy_from_slice(&self.static_phi);
        for axis in 0..3 {
            self.face_open[axis].copy_from_slice(&self.static_open[axis]);
            self.face_velocity[axis].iter_mut().for_each(|v| *v = 0.0);
            self.face_owner[axis].iter_mut().for_each(|o| *o = NO_OWNER);
        }
        self.moving.clear();
        self.moving.extend_from_slice(solids);
        self.rasterized = !solids.is_empty();
        let (nx, ny) = (dims.nx + 1, dims.ny + 1);
        let boxes: Vec<[[usize; 3]; 2]> = solids.iter().map(|s| node_box(dims, s)).collect();
        for (s, b) in solids.iter().zip(&boxes) {
            for k in b[0][2]..=b[1][2] {
                for j in b[0][1]..=b[1][1] {
                    for i in b[0][0]..=b[1][0] {
                        let p = dims.origin + Vec3::new(i as f32, j as f32, k as f32) * dims.dx;
                        let n = i + nx * (j + ny * k);
                        self.node_phi[n] = self.node_phi[n].min(s.distance(p));
                    }
                }
            }
        }
        for b in &boxes {
            for axis in 0..3 {
                let size = dims.face_dims(axis);
                // Every face with a corner node inside the node box.
                let (mut lo, mut hi) = (b[0], b[1]);
                for a in 0..3 {
                    if a != axis {
                        lo[a] = lo[a].saturating_sub(1);
                    }
                    hi[a] = hi[a].min(size[a] - 1);
                }
                for k in lo[2]..=hi[2] {
                    for j in lo[1]..=hi[1] {
                        for i in lo[0]..=hi[0] {
                            self.rasterize_face(dims, axis, [i, j, k]);
                        }
                    }
                }
            }
        }
    }

    /// Recomputes face `id` of `axis` after the moving solids were folded into the nodes.
    fn rasterize_face(&mut self, dims: &GridDims, axis: usize, id: [usize; 3]) {
        let size = dims.face_dims(axis);
        let index = id[0] + size[0] * (id[1] + size[1] * id[2]);
        if id[axis] == 0 || id[axis] == size[axis] - 1 {
            return;
        }
        let open = self.face_open_at(dims, axis, id);
        self.face_open[axis][index] = open;
        if open >= 1.0 {
            self.face_velocity[axis][index] = 0.0;
            self.face_owner[axis][index] = NO_OWNER;
            return;
        }
        let mut center = Vec3::new(id[0] as f32, id[1] as f32, id[2] as f32) + Vec3::splat(0.5);
        center[axis] -= 0.5;
        let p = dims.origin + center * dims.dx;
        match self.nearest_moving(p) {
            Some((owner, d)) if d <= self.static_distance(p) => {
                self.face_owner[axis][index] = owner as u16;
                self.face_velocity[axis][index] = self.moving[owner].velocity_at(p)[axis];
            }
            _ => {
                self.face_owner[axis][index] = NO_OWNER;
                self.face_velocity[axis][index] = 0.0;
            }
        }
    }

    fn node(&self, dims: &GridDims, i: usize, j: usize, k: usize) -> f32 {
        self.node_phi[i + (dims.nx + 1) * (j + (dims.ny + 1) * k)]
    }

    /// The open fraction of every face of `axis`: the share of the face's area outside the solid,
    /// from the bilinear node distances over `FACE_SUBSAMPLES`² points.
    fn open_fractions(&self, dims: &GridDims, axis: usize) -> Vec<f32> {
        let size = dims.face_dims(axis);
        let mut out = vec![1.0f32; size[0] * size[1] * size[2]];
        for k in 0..size[2] {
            for j in 0..size[1] {
                for i in 0..size[0] {
                    let id = [i, j, k];
                    let index = i + size[0] * (j + size[1] * k);
                    if id[axis] == 0 || id[axis] == size[axis] - 1 {
                        out[index] = 0.0;
                        continue;
                    }
                    if self.colliders.is_empty() {
                        continue;
                    }
                    out[index] = self.face_open_at(dims, axis, id);
                }
            }
        }
        out
    }

    /// The open fraction of interior face `id` of `axis` from the current node distances.
    fn face_open_at(&self, dims: &GridDims, axis: usize, id: [usize; 3]) -> f32 {
        let (a1, a2) = ((axis + 1) % 3, (axis + 2) % 3);
        // The face's four corner nodes.
        let corner = |d1: usize, d2: usize| {
            let mut n = id;
            n[a1] += d1;
            n[a2] += d2;
            self.node(dims, n[0], n[1], n[2])
        };
        let (c00, c10, c01, c11) = (corner(0, 0), corner(1, 0), corner(0, 1), corner(1, 1));
        if c00 >= 0.0 && c10 >= 0.0 && c01 >= 0.0 && c11 >= 0.0 {
            return 1.0;
        }
        let mut open = 0usize;
        for s2 in 0..FACE_SUBSAMPLES {
            let t2 = (s2 as f32 + 0.5) / FACE_SUBSAMPLES as f32;
            for s1 in 0..FACE_SUBSAMPLES {
                let t1 = (s1 as f32 + 0.5) / FACE_SUBSAMPLES as f32;
                let a = c00 * (1.0 - t1) + c10 * t1;
                let b = c01 * (1.0 - t1) + c11 * t1;
                if a * (1.0 - t2) + b * t2 >= 0.0 {
                    open += 1;
                }
            }
        }
        open as f32 / (FACE_SUBSAMPLES * FACE_SUBSAMPLES) as f32
    }
}

/// The inclusive node index box a moving solid can change: its bounding box plus one cell, clamped
/// to the grid.
fn node_box(dims: &GridDims, s: &MovingSolid) -> [[usize; 3]; 2] {
    let (lo, hi) = s.aabb();
    let n = [dims.nx, dims.ny, dims.nz];
    let (ql, qh) = (dims.local(lo), dims.local(hi));
    let mut out = [[0usize; 3]; 2];
    for a in 0..3 {
        let to = |v: f32, round_up: bool| -> usize {
            if !v.is_finite() {
                return if v > 0.0 { n[a] } else { 0 };
            }
            let v = if round_up { v.ceil() + 1.0 } else { v.floor() - 1.0 };
            v.clamp(0.0, n[a] as f32) as usize
        };
        out[0][a] = to(ql[a], false);
        out[1][a] = to(qh[a], true).max(out[0][a]);
    }
    out
}

fn distance_of(colliders: &[Collider], p: Vec3) -> f32 {
    colliders.iter().fold(f32::MAX, |d, c| d.min(c.distance(p)))
}

/// Heightfield ceilings. `PartialEq` ignores the cache: it is derived from the collider list.
///
/// `sync` reuses an entry while its key still matches: the collider variant, and a heightfield's
/// `columns`, `rows`, its `heights` buffer (address and length), a probe of
/// [`HEIGHT_PROBE_SAMPLES`] evenly spaced samples, and the sample the ceiling came from still
/// holding the ceiling. The address alone is not an identity: `colliders.remove(j)` +
/// `colliders.push(new)` can give the new field the freed block (rc1 review finding 4), and a
/// host that recycles its buffers does it every time; the probe and the ceiling sample catch a
/// new field in an old block. An edit inside the same buffer that touches none of the probed
/// samples and raises the field is not seen by the cheap check: call
/// [`SolidField::refresh_heights`] after one.
#[derive(Debug)]
struct HeightCache(std::sync::Mutex<Vec<HeightEntry>>);

/// One collider's cached ceiling.
#[derive(Debug, Clone, Copy, PartialEq)]
struct HeightEntry {
    /// The key the ceiling was computed under.
    key: HeightKey,
    /// The highest usable sample (`0` for a box or sphere, `f32::MIN` without usable samples).
    ceiling: f32,
    /// Index of a sample equal to [`Self::ceiling`], when the collider is a usable heightfield.
    at: Option<usize>,
}

/// Samples of a heightfield's buffer read into the `probe` of [`HeightKey::Field`] on every sync.
const HEIGHT_PROBE_SAMPLES: usize = 16;

/// What identifies one collider's ceiling cache entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeightKey {
    /// Box, sphere, or a heightfield without usable samples: the ceiling is a constant.
    Flat,
    /// A heightfield's sample grid and its buffer, as of the last sync.
    Field {
        /// Samples along x.
        columns: usize,
        /// Samples along z.
        rows: usize,
        /// The buffer's sample count when cached.
        len: usize,
        /// The buffer's address when cached.
        addr: usize,
        /// A hash of [`HEIGHT_PROBE_SAMPLES`] evenly spaced samples (first and last included).
        probe: u64,
    },
}

impl HeightEntry {
    fn of(collider: &Collider) -> Self {
        let (ceiling, at) = height_ceiling(collider);
        Self {
            key: height_key(collider),
            ceiling,
            at,
        }
    }

    /// Whether the entry still answers for `collider`: same key, and the ceiling's sample still
    /// holds the ceiling.
    fn holds_for(&self, collider: &Collider) -> bool {
        if self.key != height_key(collider) {
            return false;
        }
        match (collider, self.at) {
            (Collider::Heightfield { heights, .. }, Some(at)) => heights
                .get(at)
                .is_some_and(|h| h.to_bits() == self.ceiling.to_bits()),
            _ => true,
        }
    }
}

impl HeightCache {
    fn from_colliders(colliders: &[Collider]) -> Self {
        Self(std::sync::Mutex::new(
            colliders.iter().map(HeightEntry::of).collect(),
        ))
    }

    fn sync(&self, colliders: &[Collider]) {
        let mut cache = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if cache.len() != colliders.len() {
            cache.clear();
            cache.extend(colliders.iter().map(HeightEntry::of));
            return;
        }
        for (entry, collider) in cache.iter_mut().zip(colliders) {
            if !entry.holds_for(collider) {
                *entry = HeightEntry::of(collider);
            }
        }
    }

    /// Recomputes every entry from the full sample buffers.
    fn rebuild(&self, colliders: &[Collider]) {
        let mut cache = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.clear();
        cache.extend(colliders.iter().map(HeightEntry::of));
    }

    fn get(&self, index: usize) -> f32 {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(index)
            .map_or(f32::MIN, |entry| entry.ceiling)
    }
}

impl Clone for HeightCache {
    fn clone(&self) -> Self {
        let guard = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        Self(std::sync::Mutex::new(guard.clone()))
    }
}

impl PartialEq for HeightCache {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

/// The cache key of one collider: its variant, and a heightfield's dimensions, buffer and a probe
/// of its samples.
fn height_key(collider: &Collider) -> HeightKey {
    match collider {
        Collider::Heightfield {
            columns,
            rows,
            heights,
            ..
        } => HeightKey::Field {
            columns: *columns,
            rows: *rows,
            len: heights.len(),
            addr: heights.as_ptr() as usize,
            probe: height_probe(heights),
        },
        _ => HeightKey::Flat,
    }
}

/// A multiply-xor hash (FNV-1a's prime, one 32-bit word per step) of the bits of
/// [`HEIGHT_PROBE_SAMPLES`] evenly spaced samples, the first and the last included (every sample
/// of a shorter buffer).
fn height_probe(heights: &[f32]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let n = heights.len();
    if n == 0 {
        return hash;
    }
    let count = n.min(HEIGHT_PROBE_SAMPLES);
    for k in 0..count {
        // Spread `count` indices over `0..n`, the last at `n - 1`.
        let index = if count == 1 { 0 } else { k * (n - 1) / (count - 1) };
        hash = (hash ^ u64::from(heights[index].to_bits())).wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// Highest heightfield sample and its index, or `0` when the collider is not a heightfield. The
/// checked product matches `Collider::problem`: an overflowing or short grid has no usable
/// samples (`f32::MIN`).
fn height_ceiling(collider: &Collider) -> (f32, Option<usize>) {
    match collider {
        Collider::Heightfield {
            columns,
            rows,
            heights,
            ..
        } => {
            if *columns == 0 || *rows == 0 {
                return (f32::MIN, None);
            }
            let Some(samples) = columns.checked_mul(*rows) else {
                return (f32::MIN, None);
            };
            if heights.len() < samples {
                return (f32::MIN, None);
            }
            // `f32::max` skipped NaN samples; so does this (a NaN is never `>` nor `==`).
            let mut max = f32::MIN;
            let mut at = None;
            for (index, h) in heights.iter().take(samples).enumerate() {
                if *h > max || (at.is_none() && *h == max) {
                    max = *h;
                    at = Some(index);
                }
            }
            (max, at)
        }
        _ => (0.0, None),
    }
}

/// A lower bound of [`Collider::distance`]. No square root.
fn collider_lower(collider: &Collider, height_max: f32, p: Vec3) -> f32 {
    match collider {
        Collider::Sphere { center, radius } => (p - *center).abs().max_element() - radius,
        Collider::Box { center, half_extents } => {
            let q = (p - *center).abs() - *half_extents;
            let outside = q.x.max(0.0).max(q.y.max(0.0)).max(q.z.max(0.0));
            let inside = q.x.max(q.y).max(q.z).min(0.0);
            outside + inside
        }
        Collider::Heightfield { .. } => p.y - height_max,
    }
}

/// A lower bound of [`MovingSolid::distance`]: the shape sits inside its bounding sphere, and the
/// infinity norm is at most the Euclidean norm.
fn moving_lower(solid: &MovingSolid, p: Vec3) -> f32 {
    (p - solid.position).abs().max_element() - solid.shape.bounding_radius()
}

/// Metres added to a moving solid's bounding radius before a kernel skips an SDF sample.
///
/// One millimetre covers a one-ulp Newton square root (`crate::math::sqrt_f32`, the shader's
/// `sqrt_det`), so a sample the shape can reach is never treated as out of range.
/// `fluid_flip.wgsl` keeps the same constant (`SOLID_BOUND_PAD`).
#[cfg(test)]
pub(crate) const SOLID_BOUND_PAD: f32 = 0.001;

/// Squared radius of the sphere about the shape's origin that contains it.
///
/// Matches `bound_radius_sq` in `fluid_flip.wgsl`: a box uses `|half extents|²`, a sphere `r²`,
/// a capsule `(half height + radius)²`.
#[cfg(test)]
#[must_use]
pub(crate) fn solid_bound_radius_sq(shape: SolidShape) -> f32 {
    match shape {
        SolidShape::Box { half_extents } => half_extents.length_squared(),
        SolidShape::Sphere { radius } => radius * radius,
        SolidShape::Capsule { half_height, radius } => {
            let r = half_height + radius;
            r * r
        }
    }
}

/// Whether `p` is farther than `margin` from every solid's bounding sphere at time `t`.
///
/// The sphere is centred on `position + linear_velocity * t` and contains the shape at any
/// rotation, so a point this calls outside has a signed distance past `margin`. The GPU kernels
/// (`outside_solid_margin` in `fluid_flip.wgsl`) use the same test to skip SDF work.
#[cfg(test)]
#[must_use]
pub(crate) fn outside_solid_margin(p: Vec3, solids: &[MovingSolid], t: f32, margin: f32) -> bool {
    for s in solids {
        let centre = s.position + s.linear_velocity * t;
        let r = crate::math::sqrt_f32(solid_bound_radius_sq(s.shape)) + margin + SOLID_BOUND_PAD;
        if (p - centre).length_squared() <= r * r {
            return false;
        }
    }
    true
}

/// Interior faces whose corners meet a moving solid's bounding sphere at time `t`.
///
/// The GPU `rasterize` kernel has to evaluate solid SDFs on these faces. Every other interior
/// face stays at its static open fraction (the moving solids cannot reach a corner). Boundary
/// faces of the closed domain are not counted: the kernel leaves them closed. No solids means
/// no SDF faces.
#[cfg(test)]
#[must_use]
pub(crate) fn rasterize_sdf_face_count(dims: &GridDims, solids: &[MovingSolid], t: f32) -> usize {
    if solids.is_empty() {
        return 0;
    }
    let mut count = 0usize;
    for axis in 0..3 {
        let size = dims.face_dims(axis);
        let (a1, a2) = ((axis + 1) % 3, (axis + 2) % 3);
        for k in 0..size[2] {
            for j in 0..size[1] {
                for i in 0..size[0] {
                    let id = [i, j, k];
                    if id[axis] == 0 || id[axis] + 1 >= size[axis] {
                        continue;
                    }
                    let corner = |d1: usize, d2: usize| {
                        let mut c = id;
                        c[a1] += d1;
                        c[a2] += d2;
                        dims.origin + Vec3::new(c[0] as f32, c[1] as f32, c[2] as f32) * dims.dx
                    };
                    let hits = [corner(0, 0), corner(1, 0), corner(0, 1), corner(1, 1)]
                        .into_iter()
                        .any(|p| !outside_solid_margin(p, solids, t, 0.0));
                    if hits {
                        count += 1;
                    }
                }
            }
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dims() -> GridDims {
        GridDims {
            nx: 8,
            ny: 8,
            nz: 8,
            dx: 0.1,
            origin: Vec3::ZERO,
        }
    }

    #[test]
    fn rasterize_closes_faces_inside_and_restores_the_static_field() {
        let d = dims();
        let mut field = SolidField::new(&d, Vec::new());
        let before = field.clone();
        let mut solid = MovingSolid::kinematic(
            SolidShape::Box {
                half_extents: Vec3::splat(0.2),
            },
            Vec3::splat(0.4),
        );
        solid.linear_velocity = Vec3::new(1.0, 0.0, 0.0);
        field.rasterize(&d, &[solid]);
        // The u face through the box centre is closed and moves at 1 m/s.
        let s = d.face_dims(0);
        let f = 4 + s[0] * (3 + s[1] * 3);
        assert_eq!(field.face_open[0][f], 0.0);
        assert_eq!(field.face_velocity[0][f], 1.0);
        assert_eq!(field.face_owner[0][f], 0);
        // The v faces carry the y component (zero).
        assert!(field.face_velocity[1].iter().all(|v| *v == 0.0));
        assert!(field.distance(Vec3::splat(0.4)) < 0.0);
        // Rasterizing nothing restores the static field exactly.
        field.rasterize(&d, &[]);
        assert_eq!(field.node_phi, before.node_phi);
        assert_eq!(field.face_open, before.face_open);
        assert_eq!(field.face_velocity, before.face_velocity);
        assert!(field.moving.is_empty());
    }

    #[test]
    fn rotating_solid_gives_rigid_face_velocities() {
        let d = dims();
        let mut field = SolidField::new(&d, Vec::new());
        let mut solid = MovingSolid::kinematic(
            SolidShape::Capsule {
                half_height: 0.2,
                radius: 0.1,
            },
            Vec3::splat(0.4),
        );
        solid.angular_velocity = Vec3::new(0.0, 0.0, 2.0);
        assert!(solid.is_valid());
        field.rasterize(&d, &[solid]);
        let s = d.face_dims(0);
        for k in 0..s[2] {
            for j in 0..s[1] {
                for i in 0..s[0] {
                    let f = i + s[0] * (j + s[1] * k);
                    if field.face_owner[0][f] == NO_OWNER {
                        continue;
                    }
                    let p = Vec3::new(i as f32, j as f32 + 0.5, k as f32 + 0.5) * d.dx;
                    let expected = -2.0 * (p.y - 0.4);
                    assert!((field.face_velocity[0][f] - expected).abs() < 1e-5);
                }
            }
        }
        let quarter = solid.advanced(std::f32::consts::FRAC_PI_4);
        // A quarter turn about z lays the capsule along x.
        assert!(quarter.distance(Vec3::new(0.65, 0.4, 0.4)) < 0.01);
    }

    /// (Audit G-fluid #7) A heightfield too wide for exact f32 column indices: the far +x edge
    /// rounds the clamped coordinate up to `columns`, which must still index the last column.
    #[test]
    fn heightfield_far_edge_stays_in_range() {
        let columns = 16_777_220usize;
        let field = Collider::Heightfield {
            origin: [0.0, 0.0],
            spacing: 1.0,
            columns,
            rows: 1,
            heights: vec![0.0; columns],
        };
        let d = field.distance(Vec3::new((columns - 1) as f32, 1.0, 0.0));
        assert_eq!(d, 1.0);
    }

    /// (Audit G-fluid #7) A heightfield whose `columns * rows` overflows, or whose heights are
    /// short, is refused with `fluid_bad_param` instead of panicking or colliding with nothing.
    #[test]
    fn heightfield_size_overflow_is_an_error() {
        use crate::sim::{FluidParams, FluidSim};
        let params = FluidParams {
            bounds_min: Vec3::ZERO,
            bounds_max: Vec3::splat(0.4),
            cell_size: 0.1,
            ..FluidParams::default()
        };
        let field = |columns: usize, rows: usize, heights: Vec<f32>, spacing: f32| Collider::Heightfield {
            origin: [0.0, 0.0],
            spacing,
            columns,
            rows,
            heights,
        };
        let code = |c: Collider| {
            FluidSim::with_colliders(params.clone(), vec![c])
                .map(|_| ())
                .map_err(|e| e.code())
        };
        assert_eq!(
            code(field(usize::MAX / 2 + 1, 2, Vec::new(), 1.0)),
            Err("fluid_bad_param")
        );
        assert_eq!(code(field(4, 4, vec![0.0; 15], 1.0)), Err("fluid_bad_param"));
        assert_eq!(code(field(0, 4, Vec::new(), 1.0)), Err("fluid_bad_param"));
        assert_eq!(code(field(2, 2, vec![0.0; 4], f32::NAN)), Err("fluid_bad_param"));
        assert_eq!(code(field(2, 2, vec![0.0; 4], 0.0)), Err("fluid_bad_param"));
        assert_eq!(code(field(2, 2, vec![0.0; 4], 0.1)), Ok(()));
    }

    fn interior_faces(dims: &GridDims) -> usize {
        (0..3)
            .map(|axis| {
                let size = dims.face_dims(axis);
                let (a1, a2) = ((axis + 1) % 3, (axis + 2) % 3);
                size[axis].saturating_sub(2) * size[a1] * size[a2]
            })
            .sum()
    }

    /// One small solid in a wide tank: the rasterize kernel must evaluate SDFs on a bounded set of
    /// faces, not on every interior face.
    #[test]
    fn one_small_solid_leaves_most_faces_without_an_sdf() {
        let wide = GridDims {
            nx: 32,
            ny: 8,
            nz: 8,
            dx: 0.1,
            origin: Vec3::ZERO,
        };
        let solid = MovingSolid::kinematic(SolidShape::Sphere { radius: 0.15 }, Vec3::new(0.2, 0.2, 0.2));
        let touched = rasterize_sdf_face_count(&wide, &[solid], 0.0);
        let all = interior_faces(&wide);
        assert!(touched * 8 < all, "sdf faces {touched} of {all} interior faces");
        assert_eq!(rasterize_sdf_face_count(&wide, &[], 0.0), 0);
    }

    /// A particle grid the same size: only samples inside the inflated bounding sphere need the
    /// solid push (`g2p_advect`).
    #[test]
    fn distant_particles_skip_the_solid_push() {
        let solid = MovingSolid::kinematic(SolidShape::Sphere { radius: 0.15 }, Vec3::new(0.2, 0.2, 0.2));
        let dx = 0.1f32;
        let margin = dx * 0.05;
        let mut need = 0usize;
        let mut all = 0usize;
        for i in 0..32 {
            for j in 0..8 {
                for k in 0..8 {
                    let p = Vec3::new(i as f32 + 0.5, j as f32 + 0.5, k as f32 + 0.5) * dx;
                    all += 1;
                    if !outside_solid_margin(p, &[solid], 0.0, margin) {
                        need += 1;
                    }
                }
            }
        }
        assert!(
            need * 20 < all,
            "solid push would evaluate {need} of {all} particles"
        );
    }

    /// A point the bound calls outside is outside the posed shape, for a box, a sphere and a capsule.
    #[test]
    fn a_point_outside_the_bound_is_outside_the_shape() {
        let shapes = [
            SolidShape::Sphere { radius: 0.3 },
            SolidShape::Box {
                half_extents: Vec3::new(0.2, 0.1, 0.4),
            },
            SolidShape::Capsule {
                half_height: 0.25,
                radius: 0.1,
            },
        ];
        for shape in shapes {
            let mut solid = MovingSolid::kinematic(shape, Vec3::new(1.0, 2.0, -0.5));
            solid.rotation = Quat::from_xyzw(0.1, 0.2, 0.3, 0.9).normalize();
            solid.linear_velocity = Vec3::new(0.4, -0.2, 0.1);
            solid.angular_velocity = Vec3::new(0.0, 1.5, 0.0);
            let t = 0.05f32;
            let posed = solid.advanced(t);
            let (lo, hi) = posed.aabb();
            let step = (hi - lo) / 6.0;
            let mut ix = -1.0f32;
            while ix <= 7.0 {
                let mut iy = -1.0f32;
                while iy <= 7.0 {
                    let mut iz = -1.0f32;
                    while iz <= 7.0 {
                        let p = lo + Vec3::new(ix, iy, iz) * step;
                        for margin in [0.0f32, 0.05] {
                            if outside_solid_margin(p, &[solid], t, margin) {
                                let phi = posed.distance(p);
                                assert!(
                                    phi > margin,
                                    "{shape:?} at {p:?} bound says outside margin {margin}, sdf {phi}"
                                );
                            }
                        }
                        iz += 1.0;
                    }
                    iy += 1.0;
                }
                ix += 1.0;
            }
        }
    }

    /// The GPU kernels must test the bound before any solid SDF. The current shader evaluates
    /// `moving_sdf` on every face and every particle.
    #[test]
    fn gpu_kernels_test_the_solid_bound_before_the_sdf() {
        let src = include_str!("fluid_flip.wgsl");
        assert!(
            src.contains("const SOLID_BOUND_PAD: f32 = 0.001;"),
            "shader pad drifted from SOLID_BOUND_PAD"
        );
        for kernel in ["fn rasterize", "fn solid_liquid", "fn g2p_advect"] {
            let start = src.find(kernel).unwrap_or_else(|| panic!("missing {kernel}"));
            let rest = &src[start + kernel.len()..];
            let end = rest.find("\n@compute").unwrap_or(rest.len());
            let body = &rest[..end];
            let guard = body
                .find("outside_solid_margin")
                .unwrap_or_else(|| panic!("{kernel} has no solid-bounds early-out before the SDF"));
            let sdf = body
                .find("moving_sdf(")
                .unwrap_or_else(|| panic!("{kernel} lost moving_sdf"));
            assert!(
                guard < sdf,
                "{kernel} evaluates moving_sdf before the bounds test"
            );
        }
    }

    /// A heightfield sample that is NaN or infinite used to be stored on the node field and close
    /// faces (or return a non-finite distance). `with_colliders` refuses it.
    #[test]
    fn heightfield_non_finite_heights_are_an_error() {
        use crate::sim::{FluidParams, FluidSim};
        let params = FluidParams {
            bounds_min: Vec3::ZERO,
            bounds_max: Vec3::splat(0.4),
            cell_size: 0.1,
            ..FluidParams::default()
        };
        let field = |heights: Vec<f32>| Collider::Heightfield {
            origin: [0.0, 0.0],
            spacing: 0.2,
            columns: 2,
            rows: 2,
            heights,
        };
        let code = |heights: Vec<f32>| {
            FluidSim::with_colliders(params.clone(), vec![field(heights)])
                .map(|_| ())
                .map_err(|e| e.code())
        };
        assert_eq!(code(vec![0.0, f32::NAN, 0.0, 0.0]), Err("fluid_bad_param"));
        assert_eq!(code(vec![0.0, 0.0, f32::INFINITY, 0.0]), Err("fluid_bad_param"));
        assert_eq!(
            code(vec![0.0, 0.0, f32::NEG_INFINITY, 0.0]),
            Err("fluid_bad_param")
        );
        // A spare sample past `columns * rows` is not read; a finite grid is accepted.
        assert_eq!(code(vec![0.1, 0.2, 0.3, 0.4, f32::NAN]), Ok(()));
    }

    /// corr-low finding 16: `SolidField::colliders` is public, so a host may replace a collider in
    /// place. The cached heightfield ceiling must follow the replacement: a taller second
    /// heightfield becomes the nearest surface, and a stale ceiling would let the lower bound
    /// skip it.
    #[test]
    fn replacing_a_heightfield_updates_the_cached_ceiling() {
        let d = dims();
        let field_of = |max: f32| Collider::Heightfield {
            origin: [0.0, 0.0],
            spacing: 1.0,
            columns: 2,
            rows: 2,
            heights: vec![max; 4],
        };
        let mut field = SolidField::new(&d, vec![field_of(0.25), field_of(0.10)]);
        let p = Vec3::new(0.4, 0.3, 0.4);
        assert!(
            (field.distance(p) - 0.05).abs() < 1e-6,
            "the 0.25 ceiling is nearest"
        );
        // Replace the second collider with a taller field (same shape, new heights buffer).
        field.colliders[1] = field_of(0.40);
        let dist = field.distance(p);
        assert!(
            (dist - (-0.10)).abs() < 1e-6,
            "the taller replacement is nearer; the stale ceiling kept {dist}"
        );
    }

    /// rc1 review finding 4: `colliders.remove(j)` + `colliders.push(new)` can hand the new
    /// heightfield the removed one's `heights` allocation (the global allocator reuses a freed
    /// block of the same size; a host recycling its buffers does it every time). The address
    /// key then matched and the removed field's ceiling stayed: the taller replacement's lower
    /// bound looked too far and the distance skipped it.
    #[test]
    fn a_heightfield_pushed_into_a_reused_buffer_updates_the_cached_ceiling() {
        let d = dims();
        let field_in = |heights: Vec<f32>| Collider::Heightfield {
            origin: [0.0, 0.0],
            spacing: 1.0,
            columns: 2,
            rows: 2,
            heights,
        };
        // A sphere 0.2 below p's surface distance: nearest until the taller field arrives.
        let sphere = Collider::Sphere {
            center: Vec3::new(0.4, 0.8, 0.4),
            radius: 0.3,
        };
        let p = Vec3::new(0.4, 0.3, 0.4);
        let mut field = SolidField::new(&d, vec![sphere, field_in(vec![0.0; 4])]);
        assert!((field.distance(p) - 0.2).abs() < 1e-6, "the sphere is nearest");
        // Recycled buffer: remove the field and push a taller one in the same allocation.
        let Collider::Heightfield { mut heights, .. } = field.colliders.remove(1) else {
            unreachable!("index 1 is the heightfield");
        };
        let addr = heights.as_ptr();
        heights.fill(0.25);
        field.colliders.push(field_in(heights));
        let Collider::Heightfield { heights, .. } = &field.colliders[1] else {
            unreachable!("index 1 is the heightfield");
        };
        assert_eq!(heights.as_ptr(), addr, "the scenario reuses the allocation");
        let dist = field.distance(p);
        assert!(
            (dist - 0.05).abs() < 1e-6,
            "the taller field (0.05 below p) is nearer; the stale ceiling answered {dist}"
        );
        assert!((field.distance_near(p, 0.1) - 0.05).abs() < 1e-6);
        // Freed and allocated again (the allocator may or may not give the same block back).
        drop(field.colliders.remove(1));
        field.colliders.push(field_in(vec![0.28; 4]));
        let dist = field.distance(p);
        assert!((dist - 0.02).abs() < 1e-6, "fresh allocation: {dist}");
    }

    /// A raised sample edited in place, away from the samples the cheap per-query key reads:
    /// `SolidField::refresh_heights` is the documented way to take such an edit.
    #[test]
    fn refresh_heights_takes_an_in_place_heightfield_edit() {
        let d = dims();
        let p = Vec3::new(0.4, 0.3, 0.4);
        let small = Collider::Heightfield {
            origin: [0.0, 0.0],
            spacing: 1.0,
            columns: 2,
            rows: 2,
            heights: vec![0.0; 4],
        };
        let big = |h: f32| {
            let mut heights = vec![0.0; 64 * 64];
            heights[64 * 33 + 17] = h;
            Collider::Heightfield {
                origin: [0.4 - 17.0 * 0.01, 0.4 - 33.0 * 0.01],
                spacing: 0.01,
                columns: 64,
                rows: 64,
                heights,
            }
        };
        let mut field = SolidField::new(&d, vec![small, big(0.0)]);
        assert!((field.distance(p) - 0.3).abs() < 1e-6);
        if let Collider::Heightfield { heights, .. } = &mut field.colliders[1] {
            heights[64 * 33 + 17] = 0.29;
        }
        field.refresh_heights();
        let dist = field.distance(p);
        assert!((dist - 0.01).abs() < 1e-5, "after refresh_heights: {dist}");
    }

    /// corr-low finding 16: a hand-built field with an overflowing `columns * rows` must not
    /// panic in the ceiling cache. (`FluidSim::with_colliders` refuses these; `SolidField` is
    /// public and answers like `Collider::distance` does.)
    #[test]
    fn a_huge_heightfield_does_not_panic_the_ceiling_cache() {
        let d = dims();
        let huge = Collider::Heightfield {
            origin: [0.0, 0.0],
            spacing: 1.0,
            columns: usize::MAX / 2 + 1,
            rows: 2,
            heights: Vec::new(),
        };
        let field = SolidField::new(&d, vec![huge]);
        assert_eq!(field.distance(Vec3::new(0.4, 0.3, 0.4)), f32::MAX);
    }

    /// corr-low finding 17: `is_valid` accepted any finite quaternion with |q|² > 0.5. Only a
    /// unit quaternion is a rotation: a non-unit one silently scales/shears the shape in
    /// `distance` (which applies the quaternion) while `moving_lower` keeps assuming the shape
    /// sits inside its unscaled bounding sphere, so the "lower bound" can skip a solid the point
    /// is actually inside.
    #[test]
    fn a_non_unit_rotation_is_not_a_valid_solid() {
        let mut scaled_up = MovingSolid::kinematic(
            SolidShape::Box {
                half_extents: Vec3::splat(0.2),
            },
            Vec3::ZERO,
        );
        scaled_up.rotation = Quat::from_xyzw(0.0, 0.0, 0.0, 1.5);
        assert!(
            !scaled_up.is_valid(),
            "|q|² = 2.25: the bounding-sphere lower bound no longer holds"
        );
        let mut scaled_down = MovingSolid::kinematic(SolidShape::Sphere { radius: 0.2 }, Vec3::ZERO);
        scaled_down.rotation = Quat::from_xyzw(0.0, 0.0, 0.0, 0.775);
        assert!(!scaled_down.is_valid(), "|q|² = 0.6 is not a rotation either");
        let unit = MovingSolid::kinematic(SolidShape::Sphere { radius: 0.2 }, Vec3::ZERO);
        assert!(unit.is_valid(), "a normalized quaternion stays valid");
        // A quaternion carried through integration stays within the tolerance.
        let mut carried = unit;
        carried.rotation = Quat::from_xyzw(0.1, 0.2, 0.3, 0.9).normalize();
        assert!(carried.is_valid());
    }
}
