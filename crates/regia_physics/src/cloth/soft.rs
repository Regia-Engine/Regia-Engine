// SPDX-License-Identifier: MPL-2.0
// Source: M. Müller, B. Heidelberger, M. Hennix, J. Ratcliff, "Position Based Dynamics", JVCIR
// 18(2), 2007 (closed-mesh volume constraint, § 4.4); M. Macklin, M. Müller, N. Chentanez,
// "XPBD", MIG 2016 (compliant form, tetrahedral volume constraint); M. Müller, B. Heidelberger,
// M. Teschner, M. Gross, "Meshless Deformations Based on Shape Matching", SIGGRAPH 2005 (best-fit
// rotation by polar decomposition of A_pq, linear blend with beta); N. J. Higham, "Computing the
// polar decomposition — with applications", SIAM J. Sci. Stat. Comput. 7(4), 1986 (the Newton
// iteration R <- (R + R^-T) / 2). Gradients re-derived from the volume definitions. Original
// code (spec 55 § 2.8).

//! Soft bodies (spec 55 § 2.8): a [`SoftSolver`] is a [`ClothSolver`] particle store (substeps,
//! colliders, wind, digest) plus one volume model:
//!
//! - [`SoftModel::Pressure`]: a closed triangle surface with one XPBD volume constraint
//!   `C = V - k V0` (`k` = pressure, live, clamped to `[0.05, 8]`) and its edges as distance
//!   links. `V` is the sum of signed tetrahedra to the centroid; the gradient for vertex `i` is
//!   the sum over its faces of `(x_j × x_k) / 6`.
//! - [`SoftModel::Tetra`]: per-tetrahedron volume constraints `C = V_t - V_t0` and the tet
//!   edges as distance links. Inverted tets at runtime are allowed and counted; a flat or
//!   inverted rest tet is refused.
//! - [`SoftModel::ShapeMatch`]: clusters pulled toward the goal `R q + c` (or the linear blend
//!   `beta A + (1 - beta) R`) after prediction, `R` from a fixed-count polar decomposition.
//!   Particles have equal weight in the fit. With [`Plasticity`] (ADR-9070) a cluster whose
//!   stretch `S = Rᵀ A` is past the yield moves its rest shape toward the current shape:
//!   `S_p <- (I + h·creep·(S - I)) S_p`, normalized to unit determinant and capped at
//!   `‖S_p - I‖ <= max`; the rest offsets become `S_p q0`.
//!
//! Volume constraints are projected after the distance and bend links in each iteration, so a
//! step ends at the target volume up to linearization error.

use std::collections::BTreeSet;

use regia_app::prelude::*;
use regia_core::math::{Mat3, Vec3, det_cbrt};
use tracing::{debug_span, warn};

use super::{
    ClothConfig, ClothError, ClothParticle, ClothSolver, EPSILON, finite, inv_mass_of, non_negative,
};

/// Smallest and largest live pressure.
pub const PRESSURE_RANGE: (f32, f32) = (0.05, 8.0);

/// Newton iterations of the polar decomposition (fixed: deterministic instruction sequence).
const POLAR_ITERATIONS: usize = 16;

/// The volume model of a soft body (spec 55 § 2.8).
#[derive(Debug, Clone, PartialEq)]
pub enum SoftModel {
    /// A closed, outward-wound triangle surface held at `pressure` times its rest volume.
    Pressure {
        /// Surface triangles (counter-clockwise seen from outside).
        triangles: Vec<[usize; 3]>,
        /// Target volume over rest volume (clamped to [`PRESSURE_RANGE`]).
        pressure: f32,
        /// Compliance of the volume constraint (0 = incompressible).
        volume_compliance: f32,
    },
    /// A tetrahedral mesh with per-tet volume constraints.
    Tetra {
        /// Tetrahedra with positive rest volume (`(x1 - x0) · ((x2 - x0) × (x3 - x0)) > 0`).
        tets: Vec<[usize; 4]>,
        /// Compliance of the volume constraints (0 = incompressible).
        volume_compliance: f32,
    },
    /// Shape-matching clusters.
    ShapeMatch {
        /// Particle indices per cluster (at least 4, not coplanar, no repeats).
        clusters: Vec<Vec<usize>>,
        /// Pull toward the goal per substep, in `[0, 1]`.
        stiffness: f32,
        /// Blend of the linear (affine) fit into the rotation, in `[0, 1]` (0 = rigid).
        beta: f32,
    },
}

/// Plasticity of a shape-matching soft body (spec 55 § 2.8, ADR-9070): past the yield, the rest
/// shape creeps toward the current shape and keeps that deformation. Norms are Frobenius norms
/// of 3 x 3 matrices.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Plasticity {
    /// `‖S - I‖` above which the rest shape creeps (`S` the cluster's stretch against its
    /// current rest shape; 0 creeps on any deformation). At least 0.
    pub yield_threshold: f32,
    /// Fraction of the excess stretch taken into the rest shape per second (`h·creep` per
    /// substep, at most 1). At least 0; 0 never creeps.
    pub creep: f32,
    /// Cap of the plastic strain `‖S_p - I‖` (0 keeps the rest shape). At least 0.
    pub max: f32,
}

impl Plasticity {
    /// Checks every field is finite and not negative.
    ///
    /// # Errors
    /// [`ClothError::NonFinite`] or [`ClothError::Negative`] naming the field.
    pub fn validate(self) -> Result<Self, ClothError> {
        non_negative(self.yield_threshold, "plasticity.yield")?;
        non_negative(self.creep, "plasticity.creep")?;
        non_negative(self.max, "plasticity.max")?;
        Ok(self)
    }
}

/// Frobenius norm of `m - I`.
fn off_identity(m: Mat3) -> f32 {
    (m - Mat3::IDENTITY)
        .to_cols_array()
        .iter()
        .map(|v| v * v)
        .sum::<f32>()
        .sqrt()
}

/// Particle and link settings of a soft body.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SoftParams {
    /// Baseline settings (substeps, damping, gravity, thickness, ...).
    pub config: ClothConfig,
    /// Mass of every particle in kilograms (positive).
    pub particle_mass: f32,
    /// Compliance of the edge links (pressure and tetra models).
    pub edge_compliance: f32,
}

impl Default for SoftParams {
    fn default() -> Self {
        Self {
            config: ClothConfig::default(),
            particle_mass: 0.01,
            edge_compliance: 1e-4,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Cluster {
    ids: Vec<usize>,
    /// Rest offsets from the rest centroid (the plastic rest shape `S_p q0`).
    rest: Vec<Vec3>,
    /// The authored rest offsets `q0`.
    rest0: Vec<Vec3>,
    /// Plastic deformation `S_p` (identity until the cluster yields).
    plastic: Mat3,
    /// Inverse of `Σ q qᵀ`.
    aqq_inv: Mat3,
    /// Last extracted rotation (warm start and diagnostics).
    rotation: Mat3,
}

/// The model state a [`ClothSolver`] carries for a soft body.
#[derive(Debug, Clone)]
pub(crate) enum SoftState {
    Pressure {
        triangles: Vec<[usize; 3]>,
        pressure: f32,
        compliance: f32,
        rest_volume: f32,
        lambda: f32,
        grads: Vec<Vec3>,
    },
    Tetra {
        tets: Vec<[usize; 4]>,
        rest: Vec<f32>,
        compliance: f32,
        lambdas: Vec<f32>,
        inverted: usize,
    },
    ShapeMatch {
        clusters: Vec<Cluster>,
        stiffness: f32,
        beta: f32,
        plasticity: Option<Plasticity>,
        /// Plasticity was on at some point: the plastic state is part of the digest.
        plastic_used: bool,
    },
}

fn tet_volume(p: [Vec3; 4]) -> f32 {
    (p[1] - p[0]).dot((p[2] - p[0]).cross(p[3] - p[0])) / 6.0
}

/// Signed volume enclosed by `triangles`, measured to the centroid of `particles`.
fn surface_volume(particles: &[ClothParticle], triangles: &[[usize; 3]]) -> f32 {
    let c = centroid(particles);
    triangles
        .iter()
        .map(|t| {
            let [a, b, d] = t.map(|i| particles[i].position - c);
            a.dot(b.cross(d)) / 6.0
        })
        .sum()
}

fn centroid(particles: &[ClothParticle]) -> Vec3 {
    if particles.is_empty() {
        return Vec3::ZERO;
    }
    particles.iter().map(|p| p.position).sum::<Vec3>() / particles.len() as f32
}

/// `Σ a bᵀ` helper.
fn outer(a: Vec3, b: Vec3) -> Mat3 {
    Mat3::from_cols(a * b.x, a * b.y, a * b.z)
}

/// Rotational part of `a` by the fixed-count Newton iteration `R <- (R + R^-T) / 2`, warm
/// started from `a` itself; `fallback` when `a` is (near) singular.
fn polar_rotation(a: Mat3, fallback: Mat3) -> Mat3 {
    let det = a.determinant();
    if !det.is_finite() || det.abs() <= 1e-12 {
        return fallback;
    }
    let mut r = a;
    for _ in 0..POLAR_ITERATIONS {
        let d = r.determinant();
        if !d.is_finite() || d.abs() <= 1e-12 {
            return fallback;
        }
        r = (r + r.inverse().transpose()) * 0.5;
    }
    if r.is_finite() { r } else { fallback }
}

impl SoftState {
    pub(crate) fn encode(&self, write: &mut impl FnMut(&[u8])) {
        match self {
            Self::Pressure { pressure, lambda, .. } => {
                write(&[1]);
                write(&pressure.to_bits().to_le_bytes());
                write(&lambda.to_bits().to_le_bytes());
            }
            Self::Tetra { inverted, .. } => {
                write(&[2]);
                write(&(*inverted as u64).to_le_bytes());
            }
            Self::ShapeMatch {
                stiffness,
                beta,
                plasticity,
                plastic_used,
                clusters,
            } => {
                write(&[3]);
                write(&stiffness.to_bits().to_le_bytes());
                write(&beta.to_bits().to_le_bytes());
                // Only once plasticity was used, so an elastic body keeps its digest.
                if *plastic_used {
                    write(b"plastic");
                    let p = plasticity.map_or([0.0; 3], |p| [p.yield_threshold, p.creep, p.max]);
                    for v in p {
                        write(&v.to_bits().to_le_bytes());
                    }
                    for c in clusters {
                        for v in c.plastic.to_cols_array() {
                            write(&v.to_bits().to_le_bytes());
                        }
                    }
                }
            }
        }
    }

    pub(crate) fn reset_lambdas(&mut self) {
        match self {
            Self::Pressure { lambda, .. } => *lambda = 0.0,
            Self::Tetra { lambdas, .. } => lambdas.fill(0.0),
            Self::ShapeMatch { .. } => {}
        }
    }

    /// Shape matching after prediction (other models do nothing here); `h` is the substep in
    /// seconds (plastic creep).
    pub(crate) fn after_predict(&mut self, particles: &mut [ClothParticle], h: f32) {
        let Self::ShapeMatch {
            clusters,
            stiffness,
            beta,
            plasticity,
            ..
        } = self
        else {
            return;
        };
        let _span = debug_span!("cloth_soft_step").entered();
        for cluster in clusters.iter_mut() {
            let n = cluster.ids.len() as f32;
            let c = cluster.ids.iter().map(|&i| particles[i].position).sum::<Vec3>() / n;
            let mut apq = Mat3::ZERO;
            for (&i, q) in cluster.ids.iter().zip(&cluster.rest) {
                apq += outer(particles[i].position - c, *q);
            }
            let rotation = polar_rotation(apq, cluster.rotation);
            cluster.rotation = rotation;
            let linear = apq * cluster.aqq_inv;
            let goal_matrix = shape_match_goal(linear, rotation, *beta);
            // Move each particle by `d_i = G q_i - (x_i - c)`, with the mean of `d` removed:
            // the goals' centroid is `c`, and removing the rounding residue keeps the cluster's
            // center of mass from drifting through the derived velocities.
            let offset = |i: usize, q: Vec3, particles: &[ClothParticle]| {
                goal_matrix * q - (particles[i].position - c)
            };
            let mut mean = Vec3::ZERO;
            for (&i, q) in cluster.ids.iter().zip(&cluster.rest) {
                mean += offset(i, *q, particles);
            }
            let mean = mean / n;
            for (&i, q) in cluster.ids.iter().zip(&cluster.rest) {
                let d = offset(i, *q, particles) - mean;
                let p = &mut particles[i];
                if p.inv_mass > 0.0 {
                    p.position += d * *stiffness;
                }
            }
            if let Some(plasticity) = plasticity {
                cluster.creep(linear, rotation, *plasticity, h);
            }
        }
    }

    /// Volume constraints, once per iteration after the links.
    pub(crate) fn project(&mut self, particles: &mut [ClothParticle], inv_h2: f32) {
        match self {
            Self::Pressure {
                triangles,
                pressure,
                compliance,
                rest_volume,
                lambda,
                grads,
            } => {
                let _span = debug_span!("cloth_soft_step").entered();
                let c = centroid(particles);
                grads.fill(Vec3::ZERO);
                let mut volume = 0.0;
                for t in triangles.iter() {
                    let [a, b, d] = t.map(|i| particles[i].position - c);
                    volume += a.dot(b.cross(d)) / 6.0;
                    grads[t[0]] += b.cross(d) / 6.0;
                    grads[t[1]] += d.cross(a) / 6.0;
                    grads[t[2]] += a.cross(b) / 6.0;
                }
                let Some(alpha) = super::xpbd_alpha(*compliance, inv_h2) else {
                    return;
                };
                let weight: f32 = particles
                    .iter()
                    .zip(grads.iter())
                    .map(|(p, g)| p.inv_mass * g.length_squared())
                    .sum();
                let denom = weight + alpha;
                if denom <= EPSILON * EPSILON {
                    return;
                }
                let cval = volume - *pressure * *rest_volume;
                let dlambda = (-cval - alpha * *lambda) / denom;
                *lambda += dlambda;
                for (p, g) in particles.iter_mut().zip(grads.iter()) {
                    p.position += *g * (p.inv_mass * dlambda);
                }
            }
            Self::Tetra {
                tets,
                rest,
                compliance,
                lambdas,
                ..
            } => {
                let _span = debug_span!("cloth_soft_step").entered();
                let Some(alpha) = super::xpbd_alpha(*compliance, inv_h2) else {
                    return;
                };
                for ((t, &v0), lambda) in tets.iter().zip(rest.iter()).zip(lambdas.iter_mut()) {
                    let x = t.map(|i| particles[i].position);
                    let w = t.map(|i| particles[i].inv_mass);
                    let g1 = (x[2] - x[0]).cross(x[3] - x[0]) / 6.0;
                    let g2 = (x[3] - x[0]).cross(x[1] - x[0]) / 6.0;
                    let g3 = (x[1] - x[0]).cross(x[2] - x[0]) / 6.0;
                    let g = [-(g1 + g2 + g3), g1, g2, g3];
                    let weight: f32 = (0..4).map(|k| w[k] * g[k].length_squared()).sum();
                    let denom = weight + alpha;
                    if denom <= EPSILON * EPSILON {
                        continue;
                    }
                    let cval = tet_volume(x) - v0;
                    let dlambda = (-cval - alpha * *lambda) / denom;
                    *lambda += dlambda;
                    for k in 0..4 {
                        particles[t[k]].position += g[k] * (w[k] * dlambda);
                    }
                }
            }
            Self::ShapeMatch { .. } => {}
        }
    }

    /// Counts inverted tets after a step.
    pub(crate) fn after_step(&mut self, particles: &[ClothParticle]) {
        if let Self::Tetra { tets, inverted, .. } = self {
            *inverted = tets
                .iter()
                .filter(|t| tet_volume(t.map(|i| particles[i].position)) < 0.0)
                .count();
        }
    }
}

impl Cluster {
    /// Plastic flow (Müller et al. 2005, the plasticity extension): past the yield, the rest shape takes
    /// `h·creep` of the excess stretch, with unit determinant (volume kept) and the strain cap.
    fn creep(&mut self, linear: Mat3, rotation: Mat3, p: Plasticity, h: f32) {
        let stretch = rotation.transpose() * linear;
        // `Rᵀ A` is symmetric up to rounding and the polar iteration's error.
        let stretch = (stretch + stretch.transpose()) * 0.5;
        if !stretch.is_finite() || off_identity(stretch) <= p.yield_threshold {
            return;
        }
        let k = (h * p.creep).clamp(0.0, 1.0);
        let mut plastic = (Mat3::IDENTITY + (stretch - Mat3::IDENTITY) * k) * self.plastic;
        let det = plastic.determinant();
        if !det.is_finite() || det <= EPSILON {
            return;
        }
        plastic *= 1.0 / det_cbrt(det);
        let strain = off_identity(plastic);
        if strain > p.max {
            plastic = Mat3::IDENTITY + (plastic - Mat3::IDENTITY) * (p.max / strain);
        }
        let rest: Vec<Vec3> = self.rest0.iter().map(|q| plastic * *q).collect();
        let aqq = rest.iter().fold(Mat3::ZERO, |m, q| m + outer(*q, *q));
        if !plastic.is_finite() || aqq.determinant().abs() <= 1e-12 {
            return;
        }
        self.plastic = plastic;
        self.aqq_inv = aqq.inverse();
        self.rest = rest;
    }
}

/// A soft body: a [`ClothSolver`] particle store plus a [`SoftModel`]. See the
/// [module documentation](self).
#[derive(Debug, Clone)]
pub struct SoftSolver {
    cloth: ClothSolver,
}

/// The shape-matching goal matrix for a linear map `apq·aqq⁻¹`: the volume-preserving blend of
/// the linear map (scaled by `1/cbrt(det)`) and its polar rotation. The cube root is the
/// fixed-iteration kernel, not libm `cbrt`: the goal matrix feeds the particle positions,
/// which must agree across lockstep peers (ADR-4773). A non-positive `beta` or a collapsed map
/// (`det ≤ EPSILON`) blends from the rotation alone.
fn shape_match_goal(linear: Mat3, rotation: Mat3, beta: f32) -> Mat3 {
    if beta > 0.0 {
        let det = linear.determinant();
        if det > EPSILON {
            linear * (1.0 / det_cbrt(det)) * beta + rotation * (1.0 - beta)
        } else {
            rotation
        }
    } else {
        rotation
    }
}

impl SoftSolver {
    /// Builds a soft body from rest `positions` and a model.
    ///
    /// # Errors
    /// [`ClothError::NotClosed`] for a pressure surface whose edges are not each shared by
    /// exactly two triangles; [`ClothError::BadParam`] for a surface wound inside out, a
    /// stiffness or beta outside `[0, 1]`, or a non-positive mass;
    /// [`ClothError::InvertedRest`] for a flat or inverted rest tet;
    /// [`ClothError::BadCluster`] for a bad shape-matching cluster; the particle and config
    /// errors of [`ClothSolver`].
    pub fn new(positions: &[Vec3], model: SoftModel, params: SoftParams) -> Result<Self, ClothError> {
        let mut cloth = ClothSolver::new(params.config)?;
        if finite(params.particle_mass, "particle mass")? <= 0.0 {
            return Err(ClothError::BadParam {
                what: "particle_mass",
            });
        }
        let inv_mass = inv_mass_of(params.particle_mass)?;
        let edge_compliance = non_negative(params.edge_compliance, "compliance")?;
        for &p in positions {
            cloth.add_particle(p, inv_mass)?;
        }
        let n = positions.len();
        let check = |i: usize| {
            if i < n {
                Ok(())
            } else {
                Err(ClothError::ParticleOutOfRange { index: i, len: n })
            }
        };
        let state = match model {
            SoftModel::Pressure {
                triangles,
                pressure,
                volume_compliance,
            } => {
                let compliance = non_negative(volume_compliance, "volume_compliance")?;
                let pressure = finite(pressure, "pressure")?.clamp(PRESSURE_RANGE.0, PRESSURE_RANGE.1);
                let mut edges: BTreeSet<(usize, usize)> = BTreeSet::new();
                let mut uses: std::collections::BTreeMap<(usize, usize), u32> =
                    std::collections::BTreeMap::new();
                for t in &triangles {
                    for &i in t {
                        check(i)?;
                    }
                    if t[0] == t[1] || t[1] == t[2] || t[0] == t[2] {
                        return Err(ClothError::RepeatedParticle { index: t[0] });
                    }
                    for k in 0..3 {
                        let (a, b) = (t[k], t[(k + 1) % 3]);
                        let key = (a.min(b), a.max(b));
                        edges.insert(key);
                        *uses.entry(key).or_default() += 1;
                    }
                }
                if triangles.is_empty() || uses.values().any(|&u| u != 2) {
                    return Err(ClothError::NotClosed);
                }
                for &(a, b) in &edges {
                    cloth.add_distance(a, b, edge_compliance)?;
                }
                for t in &triangles {
                    cloth.add_triangle(t[0], t[1], t[2])?;
                }
                let rest_volume = surface_volume(cloth.particles(), &triangles);
                if rest_volume <= EPSILON {
                    return Err(ClothError::BadParam { what: "winding" });
                }
                SoftState::Pressure {
                    triangles,
                    pressure,
                    compliance,
                    rest_volume,
                    lambda: 0.0,
                    grads: vec![Vec3::ZERO; n],
                }
            }
            SoftModel::Tetra {
                tets,
                volume_compliance,
            } => {
                let compliance = non_negative(volume_compliance, "volume_compliance")?;
                let mut edges: BTreeSet<(usize, usize)> = BTreeSet::new();
                let mut rest = Vec::with_capacity(tets.len());
                for (index, t) in tets.iter().enumerate() {
                    for &i in t {
                        check(i)?;
                    }
                    let v = tet_volume(t.map(|i| positions[i]));
                    if v.is_nan() || v <= EPSILON {
                        return Err(ClothError::InvertedRest { tet: index });
                    }
                    rest.push(v);
                    for a in 0..4 {
                        for b in a + 1..4 {
                            edges.insert((t[a].min(t[b]), t[a].max(t[b])));
                        }
                    }
                }
                for &(a, b) in &edges {
                    cloth.add_distance(a, b, edge_compliance)?;
                }
                let count = tets.len();
                SoftState::Tetra {
                    tets,
                    rest,
                    compliance,
                    lambdas: vec![0.0; count],
                    inverted: 0,
                }
            }
            SoftModel::ShapeMatch {
                clusters,
                stiffness,
                beta,
            } => {
                for (what, v) in [("stiffness", stiffness), ("beta", beta)] {
                    if !(0.0..=1.0).contains(&finite(v, what)?) {
                        return Err(ClothError::BadParam { what });
                    }
                }
                let mut built = Vec::with_capacity(clusters.len());
                for (index, ids) in clusters.into_iter().enumerate() {
                    let bad = ClothError::BadCluster { cluster: index };
                    let mut sorted = ids.clone();
                    sorted.sort_unstable();
                    sorted.dedup();
                    if ids.len() < 4 || sorted.len() != ids.len() || ids.iter().any(|&i| i >= n) {
                        return Err(bad);
                    }
                    let c = ids.iter().map(|&i| positions[i]).sum::<Vec3>() / ids.len() as f32;
                    let rest: Vec<Vec3> = ids.iter().map(|&i| positions[i] - c).collect();
                    let aqq = rest.iter().fold(Mat3::ZERO, |m, q| m + outer(*q, *q));
                    if aqq.determinant().abs() <= 1e-12 {
                        return Err(bad);
                    }
                    built.push(Cluster {
                        ids,
                        rest0: rest.clone(),
                        rest,
                        plastic: Mat3::IDENTITY,
                        aqq_inv: aqq.inverse(),
                        rotation: Mat3::IDENTITY,
                    });
                }
                SoftState::ShapeMatch {
                    clusters: built,
                    stiffness,
                    beta,
                    plasticity: None,
                    plastic_used: false,
                }
            }
        };
        cloth.soft = Some(Box::new(state));
        Ok(Self { cloth })
    }

    /// Turns plasticity of a shape-matching body on (`Some`) or off (ADR-9070). Off stops the
    /// creep; the rest shape keeps what it already took ([`Self::reset_plastic`] restores it).
    ///
    /// # Errors
    /// [`ClothError::BadParam`] (`plasticity`) for another model; [`ClothError::NonFinite`] or
    /// [`ClothError::Negative`] for a NaN, infinite or negative field. Nothing changes then.
    pub fn set_plasticity(&mut self, plasticity: Option<Plasticity>) -> Result<(), ClothError> {
        let plasticity = plasticity.map(Plasticity::validate).transpose()?;
        match self.cloth.soft.as_deref_mut() {
            Some(SoftState::ShapeMatch {
                plasticity: slot,
                plastic_used,
                ..
            }) => {
                *plastic_used |= plasticity.is_some();
                *slot = plasticity;
                Ok(())
            }
            _ => Err(ClothError::BadParam { what: "plasticity" }),
        }
    }

    /// The plasticity of a shape-matching body (`None`: elastic, or another model).
    #[must_use]
    pub fn plasticity(&self) -> Option<Plasticity> {
        match self.state() {
            Some(SoftState::ShapeMatch { plasticity, .. }) => *plasticity,
            _ => None,
        }
    }

    /// The plastic strain `‖S_p - I‖` of shape-matching cluster `index` (0 for a cluster that
    /// never yielded); `None` for another model or a missing cluster.
    #[must_use]
    pub fn plastic_strain(&self, index: usize) -> Option<f32> {
        match self.state() {
            Some(SoftState::ShapeMatch { clusters, .. }) => {
                clusters.get(index).map(|c| off_identity(c.plastic))
            }
            _ => None,
        }
    }

    /// The largest plastic strain over the clusters (0 for an elastic body or another model).
    #[must_use]
    pub fn max_plastic_strain(&self) -> f32 {
        match self.state() {
            Some(SoftState::ShapeMatch { clusters, .. }) => clusters
                .iter()
                .map(|c| off_identity(c.plastic))
                .fold(0.0, f32::max),
            _ => 0.0,
        }
    }

    /// Gives every shape-matching cluster its authored rest shape back (the plastic strain
    /// becomes 0). Does nothing for other models.
    pub fn reset_plastic(&mut self) {
        if let Some(SoftState::ShapeMatch { clusters, .. }) = self.cloth.soft.as_deref_mut() {
            for c in clusters {
                let aqq = c.rest0.iter().fold(Mat3::ZERO, |m, q| m + outer(*q, *q));
                c.plastic = Mat3::IDENTITY;
                c.rest.clone_from(&c.rest0);
                c.aqq_inv = aqq.inverse();
            }
        }
    }

    /// The particle store (stats, colliders, wind, pins, digest).
    #[must_use]
    pub fn cloth(&self) -> &ClothSolver {
        &self.cloth
    }

    /// The particle store, mutably.
    pub fn cloth_mut(&mut self) -> &mut ClothSolver {
        &mut self.cloth
    }

    /// Advances by `dt` seconds (see [`ClothSolver::step`]).
    ///
    /// # Errors
    /// [`ClothSolver::step`]'s errors.
    pub fn step(&mut self, dt: f32) -> Result<u32, ClothError> {
        self.cloth.step(dt)
    }

    /// Current enclosed volume (pressure model; summed tet volume for tetra; 0 for shape
    /// matching).
    #[must_use]
    pub fn volume(&self) -> f32 {
        let particles = self.cloth.particles();
        match self.state() {
            Some(SoftState::Pressure { triangles, .. }) => surface_volume(particles, triangles),
            Some(SoftState::Tetra { tets, .. }) => tets
                .iter()
                .map(|t| tet_volume(t.map(|i| particles[i].position)))
                .sum(),
            _ => 0.0,
        }
    }

    /// Rest volume (pressure: the surface's; tetra: the summed tets'; 0 for shape matching).
    #[must_use]
    pub fn rest_volume(&self) -> f32 {
        match self.state() {
            Some(SoftState::Pressure { rest_volume, .. }) => *rest_volume,
            Some(SoftState::Tetra { rest, .. }) => rest.iter().sum(),
            _ => 0.0,
        }
    }

    /// The live pressure (`None` for other models).
    #[must_use]
    pub fn pressure(&self) -> Option<f32> {
        match self.state() {
            Some(SoftState::Pressure { pressure, .. }) => Some(*pressure),
            _ => None,
        }
    }

    /// Sets the live pressure, clamped to [`PRESSURE_RANGE`].
    ///
    /// # Errors
    /// [`ClothError::NonFinite`]; [`ClothError::BadParam`] when the model is not
    /// [`SoftModel::Pressure`].
    pub fn set_pressure(&mut self, k: f32) -> Result<(), ClothError> {
        let k = finite(k, "pressure")?;
        match self.cloth.soft.as_deref_mut() {
            Some(SoftState::Pressure { pressure, .. }) => {
                *pressure = k.clamp(PRESSURE_RANGE.0, PRESSURE_RANGE.1);
                Ok(())
            }
            _ => Err(ClothError::BadParam { what: "pressure" }),
        }
    }

    /// Tets with negative volume after the last step (tetra model; 0 otherwise).
    #[must_use]
    pub fn inverted_tets(&self) -> usize {
        match self.state() {
            Some(SoftState::Tetra { inverted, .. }) => *inverted,
            _ => 0,
        }
    }

    /// The rotation last extracted for shape-matching cluster `index`.
    #[must_use]
    pub fn cluster_rotation(&self, index: usize) -> Option<Mat3> {
        match self.state() {
            Some(SoftState::ShapeMatch { clusters, .. }) => clusters.get(index).map(|c| c.rotation),
            _ => None,
        }
    }

    /// Stable lowercase model name (`pressure`, `tetra`, `shape_match`).
    #[must_use]
    pub fn model_name(&self) -> &'static str {
        match self.state() {
            Some(SoftState::Pressure { .. }) => "pressure",
            Some(SoftState::Tetra { .. }) => "tetra",
            _ => "shape_match",
        }
    }

    fn state(&self) -> Option<&SoftState> {
        self.cloth.soft.as_deref()
    }

    /// The particle store's digest, which includes the model state (pressure, multipliers).
    #[must_use]
    pub fn state_digest(&self) -> u64 {
        self.cloth.state_digest()
    }
}

/// Named soft bodies stepped by the cloth plugin after the cloth solvers, in insertion order.
#[derive(Resource, Debug, Clone, Default)]
pub struct SoftSet {
    entries: Vec<(String, SoftSolver)>,
}

impl SoftSet {
    /// Adds `solver` under `name`, replacing (and returning) one with the same name.
    pub fn insert(&mut self, name: impl Into<String>, solver: SoftSolver) -> Option<SoftSolver> {
        let name = name.into();
        if let Some((_, slot)) = self.entries.iter_mut().find(|(n, _)| *n == name) {
            return Some(std::mem::replace(slot, solver));
        }
        self.entries.push((name, solver));
        None
    }

    /// The soft body named `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&SoftSolver> {
        self.entries.iter().find(|(n, _)| n == name).map(|(_, s)| s)
    }

    /// The soft body named `name`, mutably.
    pub fn get_mut(&mut self, name: &str) -> Option<&mut SoftSolver> {
        self.entries.iter_mut().find(|(n, _)| n == name).map(|(_, s)| s)
    }

    /// Index of the soft body named `name` (one scan). [`Self::get_slot`] is O(1) after that.
    #[must_use]
    pub fn slot_of(&self, name: &str) -> Option<usize> {
        self.entries.iter().position(|(n, _)| n == name)
    }

    /// Name stored at `index`.
    #[must_use]
    pub fn slot_name(&self, index: usize) -> Option<&str> {
        self.entries.get(index).map(|(n, _)| n.as_str())
    }

    /// The soft body at `index`.
    #[must_use]
    pub fn get_slot(&self, index: usize) -> Option<&SoftSolver> {
        self.entries.get(index).map(|(_, solver)| solver)
    }

    /// The soft body at `index`, mutably.
    pub fn get_slot_mut(&mut self, index: usize) -> Option<&mut SoftSolver> {
        self.entries.get_mut(index).map(|(_, solver)| solver)
    }

    /// Removes and returns the soft body named `name`.
    pub fn remove(&mut self, name: &str) -> Option<SoftSolver> {
        let index = self.entries.iter().position(|(n, _)| n == name)?;
        Some(self.entries.remove(index).1)
    }

    /// Every `(name, solver)` pair in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &SoftSolver)> {
        self.entries.iter().map(|(n, s)| (n.as_str(), s))
    }

    /// Number of soft bodies.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Steps every soft body by `dt` in insertion order; returns the fixed steps run.
    ///
    /// # Errors
    /// [`ClothError::NonFinite`] or [`ClothError::Negative`] for `dt`, checked first.
    pub fn step_all(&mut self, dt: f32) -> Result<u32, ClothError> {
        non_negative(dt, "dt")?;
        let mut total = 0u32;
        for (_, solver) in &mut self.entries {
            total = total.saturating_add(solver.step(dt)?);
        }
        Ok(total)
    }
}

/// Steps every soft body in [`SoftSet`] by one `FixedTime` step (installed by the cloth plugin).
pub fn step_soft_set(fixed: Res<FixedTime>, set: Option<ResMut<SoftSet>>) {
    let _span = tracing::debug_span!("step_soft_set").entered();
    let Some(mut set) = set else {
        return;
    };
    if set.is_empty() {
        return;
    }
    let dt = fixed.step().as_secs_f32();
    if let Err(err) = set.step_all(dt) {
        warn!(target: "regia_physics", "soft body step skipped: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The nine-point cluster of `tests/suite/cloth_spec55.rs`'s shape-match test.
    fn cluster() -> Vec<Vec3> {
        [
            (0.0, 0.0, 0.0),
            (1.0, 0.0, 0.0),
            (0.0, 1.0, 0.0),
            (0.0, 0.0, 1.0),
            (1.0, 1.0, 0.0),
            (0.0, 1.0, 1.0),
            (1.0, 0.0, 1.0),
            (1.0, 1.0, 1.0),
            (0.5, 0.2, 0.7),
        ]
        .iter()
        .map(|&(x, y, z)| (Vec3::new(x, y, z) - Vec3::splat(0.5)) * 0.3)
        .collect()
    }

    /// Particle positions (x, y, z per particle) after one step at twice the rest scale with
    /// `beta = 0.5`, pinned (ADR-4773, audit G finding 5): the affine blend's cube root must
    /// come from the fixed-iteration kernel, never platform `cbrtf`.
    const TWICE_SCALE_STEP_BITS: [u32; 27] = [
        3_189_348_762,
        3_190_019_851,
        3_188_901_370,
        1_041_865_114,
        3_190_019_851,
        3_188_901_370,
        3_189_348_762,
        1_041_194_025,
        3_188_901_370,
        3_189_348_762,
        3_190_019_851,
        1_042_312_506,
        1_041_865_114,
        1_041_194_025,
        3_188_901_370,
        3_189_348_762,
        1_041_194_025,
        1_042_312_506,
        1_041_865_114,
        3_190_019_851,
        1_042_312_506,
        1_041_865_114,
        1_041_194_025,
        1_042_312_506,
        0,
        3_184_315_596,
        1_032_358_025,
    ];

    #[test]
    fn affine_blend_cube_root_is_deterministic() {
        let rest = cluster();
        let mut solver = SoftSolver::new(
            &rest,
            SoftModel::ShapeMatch {
                clusters: vec![(0..rest.len()).collect()],
                stiffness: 1.0,
                beta: 0.5,
            },
            SoftParams {
                config: ClothConfig {
                    gravity: Vec3::ZERO,
                    substeps: 1,
                    ..ClothConfig::default()
                },
                ..SoftParams::default()
            },
        )
        .expect("cluster");
        // Predicted at twice the rest shape: the goal matrix's det ≈ 8 exercises the cbrt.
        for (i, p) in rest.iter().enumerate() {
            solver.cloth_mut().set_position(i, *p * 2.0).expect("set");
        }
        solver.step(1.0 / 60.0).expect("step");
        let bits: Vec<u32> = solver
            .cloth()
            .particles()
            .iter()
            .flat_map(|p| p.position.to_array().map(f32::to_bits))
            .collect();
        assert_eq!(
            bits, TWICE_SCALE_STEP_BITS,
            "goal positions feed the particles; the cube root must be the deterministic kernel"
        );
        // Sanity: the pull moved the particles back toward the rest shape.
        let moved = solver.cloth().particles()[0].position;
        assert!(
            (moved - rest[0]).length() < (rest[0] * 2.0 - rest[0]).length(),
            "shape matching pulls toward the goal: {moved}"
        );
    }

    /// The goal matrix's blend scale, pinned bit-exact (ADR-4773; muse fix-review,
    /// fix-physics1-determinism finding 6). The pinned particle positions above are absorbed
    /// by downstream `f32` rounding when the cube root moves by one ulp, so they stay green
    /// with a reverted libm `cbrt`; this pin is not: it binds the fixed-iteration kernel's
    /// exact bits, so reverting `shape_match_goal` to the platform `cbrt` — whose last ulp
    /// differs on some targets — or changing the kernel itself, fails here.
    #[test]
    fn affine_blend_goal_scale_pins_the_cbrt_kernel_bits() {
        // A generic non-cube determinant (2·3·0.7 = 4.2), unlike the twice-rest cluster's
        // det ≈ 8 whose root is exactly 2.
        let linear = Mat3::from_diagonal(Vec3::new(2.0, 3.0, 0.7));
        let det = linear.determinant();
        assert!(det > EPSILON);
        let goal = shape_match_goal(linear, Mat3::IDENTITY, 0.5);
        let want = linear * (1.0 / det_cbrt(det)) * 0.5 + Mat3::IDENTITY * 0.5;
        assert_eq!(
            goal.to_cols_array().map(f32::to_bits),
            want.to_cols_array().map(f32::to_bits),
            "the goal matrix must scale by the deterministic kernel's cube root"
        );
        let scale_bits = (1.0 / det_cbrt(det)).to_bits();
        assert_eq!(
            scale_bits, 0x3f1e_ab17,
            "1/cbrt(4.2) from the fixed-iteration kernel, pinned"
        );
        assert_eq!(
            goal.to_cols_array().map(f32::to_bits),
            [1_066_358_156, 0, 0, 0, 1_068_957_777, 0, 0, 0, 1_060_604_078,],
            "the goal matrix bits, pinned against any kernel or blend change"
        );
    }
}
