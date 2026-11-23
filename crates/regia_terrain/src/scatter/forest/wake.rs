// SPDX-License-Identifier: MPL-2.0
//! Trees swaying in a fast camera's wake (ADR-1503).
//!
//! Two parts:
//!
//! - A gust source ([`GustField`]): air velocity at a point. [`WakeModel`] is the built-in one, a
//!   body's axisymmetric far wake laid along the camera's recent track: behind the body the air
//!   is dragged along its path, with a centreline speed that decays as `(x / D)^(-2/3)` and a
//!   width that grows as `(x / D)^(1/3)` with the distance `x` behind it (the self-similar
//!   axisymmetric wake: Tennekes & Lumley, *A First Course in Turbulence*, 1972, § 4.2; Pope,
//!   *Turbulent Flows*, 2000, § 5.4) and a Gaussian profile across. Any other gust model (the
//!   camera-body wake of ADR-1502 when it lands, a wind field) plugs in by implementing the trait.
//! - [`SwayField`]: each tree near the gusts is a damped oscillator. Its lean `θ` (a horizontal
//!   vector, radians) follows `θ'' = ω²(θ_eq − θ) − 2ζω θ'`, where `θ_eq` is the steady lean the
//!   gust at its crown would hold (`max_tilt · g² / (g² + g½²)`, drag grows with the square of the
//!   gust, saturating), `ω = 2π f` with `f` falling as `1 / height` (a cantilever of fixed
//!   slenderness), and `ζ` the damping ratio. Field measurements put tree sway frequencies around
//!   0.2-1 Hz and damping ratios around 0.05-0.2 (Moore & Maguire, "Natural sway frequencies and
//!   damping ratios of trees", *Trees* 18, 2004), which the defaults follow. Fixed steps, so a
//!   scripted flight sways the same way on every run.
//!
//! The lean is applied to the emitted draws ([`SwayField::apply_frame`],
//! [`SwayField::apply_extracted`]) as a rotation about the instance's base pivot: every draw of
//! the instance (material parts, a LOD cross-fade's two copies, shadow-only draws) shares the
//! instance's model matrix, so they are found by it and lean together. The renderer keys the
//! previous model matrix by object, so motion vectors (TAA, motion blur) follow the lean. The
//! whole instance leans rigidly: a per-vertex bend (trunk stiff, crown flexible) needs a vertex
//! shader input and is a follow-up.

use std::collections::{HashMap, VecDeque};

use regia_core::glam::{Mat4, Quat, Vec3};
use regia_ecs::prelude::Entity;
use regia_render::{ExtractedMesh, FrameMesh, LabVertex};

/// Air velocity at a point, metres per second.
pub trait GustField {
    /// Gust at world point `p`.
    fn gust(&self, p: [f32; 3]) -> [f32; 3];
    /// World box outside which the gust is zero (`None`: anywhere).
    fn bounds(&self) -> Option<([f32; 3], [f32; 3])>;
}

/// No gust anywhere.
#[derive(Clone, Copy, Debug, Default)]
pub struct StillAir;

impl GustField for StillAir {
    fn gust(&self, _p: [f32; 3]) -> [f32; 3] {
        [0.0; 3]
    }

    fn bounds(&self) -> Option<([f32; 3], [f32; 3])> {
        Some(([0.0; 3], [0.0; 3]))
    }
}

/// The built-in camera wake: an axisymmetric far wake along the camera's recent track.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct WakeModel {
    /// Radius of the body making the wake, metres (`D` = twice this).
    pub body_radius_m: f32,
    /// No wake below this speed; full strength from twice it, metres per second.
    pub min_speed_m_s: f32,
    /// Multiplier on the wake speed (1 = the similarity law with unit constants).
    pub strength: f32,
    /// Seconds of track the wake remembers.
    pub memory_s: f32,
    /// Ceiling of the gust, metres per second.
    pub max_gust_m_s: f32,
}

impl Default for WakeModel {
    fn default() -> Self {
        Self {
            body_radius_m: 2.0,
            min_speed_m_s: 10.0,
            strength: 1.0,
            memory_s: 3.0,
            max_gust_m_s: 40.0,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct TrackPoint {
    time: f64,
    position: Vec3,
    speed: f32,
    /// Path length from the first remembered point.
    along: f32,
}

/// One camera position on the wake's track.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WakeSample {
    /// Seconds (any monotonic clock: the frame index over the frame rate for offline renders).
    pub time_s: f64,
    /// Camera position, world metres.
    pub position: [f32; 3],
}

/// The camera's recent track and the wake it leaves ([`WakeModel`]).
#[derive(Clone, Debug)]
pub struct TrackWake {
    model: WakeModel,
    track: VecDeque<TrackPoint>,
    /// The box the wake can reach (empty while there is no wake), cached per push.
    bounds: Option<([f32; 3], [f32; 3])>,
}

impl TrackWake {
    /// An empty track with `model`.
    #[must_use]
    pub fn new(model: WakeModel) -> Self {
        Self {
            model,
            track: VecDeque::new(),
            bounds: Some(([0.0; 3], [0.0; 3])),
        }
    }

    /// The model.
    #[must_use]
    pub fn model(&self) -> WakeModel {
        self.model
    }

    /// Appends the camera at `sample` (a time not after the last is ignored; a jump longer than
    /// `max_gust_m_s × memory_s` restarts the track: a cut, not a flight).
    pub fn push(&mut self, sample: WakeSample) {
        let p = Vec3::from_array(sample.position);
        if !p.is_finite() || !sample.time_s.is_finite() {
            return;
        }
        if let Some(last) = self.track.back().copied() {
            if sample.time_s <= last.time {
                return;
            }
            let step = p.distance(last.position);
            let dt = (sample.time_s - last.time) as f32;
            if step > 2000.0 || step / dt.max(1e-6) > 5000.0 {
                self.track.clear();
            } else {
                self.track.push_back(TrackPoint {
                    time: sample.time_s,
                    position: p,
                    speed: step / dt.max(1e-6),
                    along: last.along + step,
                });
            }
        }
        if self.track.is_empty() {
            self.track.push_back(TrackPoint {
                time: sample.time_s,
                position: p,
                speed: 0.0,
                along: 0.0,
            });
        }
        let keep_after = sample.time_s - f64::from(self.model.memory_s.max(0.0));
        while self.track.len() > 2 && self.track.get(1).is_some_and(|t| t.time < keep_after) {
            self.track.pop_front();
        }
        self.bounds = self.compute_bounds();
    }

    /// Largest half-width the wake reaches over the remembered track, metres.
    fn max_width(&self) -> f32 {
        let d = 2.0 * self.model.body_radius_m.max(0.01);
        let span = self.track.back().map_or(0.0, |b| b.along) - self.track.front().map_or(0.0, |f| f.along);
        // Three widths take the Gaussian below 1.2 %.
        3.0 * 0.5 * d * (span / d + 1.0).cbrt()
    }

    fn compute_bounds(&self) -> Option<([f32; 3], [f32; 3])> {
        if self.track.len() < 2 || self.track.iter().all(|t| t.speed < self.model.min_speed_m_s) {
            return Some(([0.0; 3], [0.0; 3]));
        }
        let pad = Vec3::splat(self.max_width());
        let (mut lo, mut hi) = (Vec3::splat(f32::INFINITY), Vec3::splat(f32::NEG_INFINITY));
        for t in &self.track {
            lo = lo.min(t.position);
            hi = hi.max(t.position);
        }
        Some(((lo - pad).to_array(), (hi + pad).to_array()))
    }
}

impl GustField for TrackWake {
    fn gust(&self, p: [f32; 3]) -> [f32; 3] {
        let m = &self.model;
        let (Some(head), true) = (self.track.back(), self.track.len() >= 2) else {
            return [0.0; 3];
        };
        let p = Vec3::from_array(p);
        // Nearest point of the track.
        let mut best: Option<(f32, f32, Vec3, f32)> = None;
        for (a, b) in self.track.iter().zip(self.track.iter().skip(1)) {
            let ab = b.position - a.position;
            let len2 = ab.length_squared();
            let t = if len2 > 0.0 {
                ((p - a.position).dot(ab) / len2).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let q = a.position + ab * t;
            let r2 = p.distance_squared(q);
            if best.is_none_or(|cur| r2 < cur.0) {
                let along = a.along + (b.along - a.along) * t;
                let dir = if len2 > 0.0 { ab / len2.sqrt() } else { Vec3::ZERO };
                best = Some((r2, along, dir, b.speed));
            }
        }
        let Some((r2, along, dir, speed)) = best else {
            return [0.0; 3];
        };
        let ramp = smoothstep(m.min_speed_m_s, 2.0 * m.min_speed_m_s, speed);
        if ramp <= 0.0 {
            return [0.0; 3];
        }
        let d = 2.0 * m.body_radius_m.max(0.01);
        let x = (head.along - along).max(0.0);
        let s = x / d + 1.0;
        let centre = (m.strength * speed * s.powf(-2.0 / 3.0)).min(m.max_gust_m_s);
        let width = 0.5 * d * s.cbrt();
        let profile = (-r2 / (2.0 * width * width)).exp();
        (dir * centre * profile * ramp).to_array()
    }

    fn bounds(&self) -> Option<([f32; 3], [f32; 3])> {
        self.bounds
    }
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    if e1 <= e0 {
        return if x >= e1 { 1.0 } else { 0.0 };
    }
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// How trees answer a gust ([`SwayField`]).
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SwaySettings {
    /// Natural sway frequency of a 10 m tree, hertz (others scale as `10 m / height`, clamped to
    /// 0.1-3 Hz).
    pub frequency_10m_hz: f32,
    /// Damping ratio `ζ` (0..1).
    pub damping_ratio: f32,
    /// Largest steady lean, radians.
    pub max_tilt_rad: f32,
    /// Gust at which the steady lean is half `max_tilt_rad`, metres per second.
    pub half_tilt_gust_m_s: f32,
    /// Instances shorter than this do not sway (stones), metres.
    pub min_height_m: f32,
    /// Most instances swaying at once (new ones wait while full).
    pub max_instances: usize,
}

impl Default for SwaySettings {
    fn default() -> Self {
        Self {
            frequency_10m_hz: 0.6,
            damping_ratio: 0.1,
            max_tilt_rad: 0.12,
            half_tilt_gust_m_s: 10.0,
            min_height_m: 1.0,
            max_instances: 20_000,
        }
    }
}

impl SwaySettings {
    /// Values in range.
    #[must_use]
    pub fn sanitized(self) -> Self {
        let d = Self::default();
        let fin = |v: f32, def: f32| if v.is_finite() { v } else { def };
        Self {
            frequency_10m_hz: fin(self.frequency_10m_hz, d.frequency_10m_hz).clamp(0.05, 5.0),
            damping_ratio: fin(self.damping_ratio, d.damping_ratio).clamp(0.0, 1.0),
            max_tilt_rad: fin(self.max_tilt_rad, d.max_tilt_rad).clamp(0.0, 0.6),
            half_tilt_gust_m_s: fin(self.half_tilt_gust_m_s, d.half_tilt_gust_m_s).max(0.1),
            min_height_m: fin(self.min_height_m, d.min_height_m).max(0.0),
            max_instances: self.max_instances,
        }
    }

    /// Natural frequency (hertz) of a tree `height` metres tall.
    #[must_use]
    pub fn frequency(&self, height: f32) -> f32 {
        (self.frequency_10m_hz * 10.0 / height.max(0.5)).clamp(0.1, 3.0)
    }

    /// Steady lean (radians) a horizontal gust of `gust` m/s holds.
    #[must_use]
    pub fn steady_tilt(&self, gust: f32) -> f32 {
        let g2 = gust * gust;
        self.max_tilt_rad * g2 / (g2 + self.half_tilt_gust_m_s * self.half_tilt_gust_m_s)
    }
}

#[derive(Clone, Copy, Debug)]
struct SwayState {
    /// Lean (horizontal vector: x, z), radians.
    tilt: [f32; 2],
    rate: [f32; 2],
    /// Where the gust is read (the crown), world metres.
    probe: [f32; 3],
    omega: f32,
}

/// What one [`SwayField::step`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize)]
pub struct SwayStats {
    /// Instances swaying after the step.
    pub swaying: u32,
    /// Instances that started swaying this step.
    pub started: u32,
    /// Instances that came to rest and were dropped this step.
    pub settled: u32,
    /// Instances that would have started but the field was full.
    pub refused: u32,
    /// Largest lean, milliradians.
    pub max_tilt_mrad: u32,
    /// Draws examined.
    pub examined: u32,
}

/// Damped sway of the trees in a gust (see the module docs).
#[derive(Clone, Debug, Default)]
pub struct SwayField {
    settings: SwaySettings,
    states: HashMap<u64, SwayState>,
    /// Top of each mesh at scale 1 (its highest vertex), by mesh key.
    mesh_top: HashMap<u64, f32>,
}

/// One draw sway may start: model, mesh key, vertices.
type SwayCandidate<'v> = ([[f32; 4]; 4], u64, &'v [LabVertex]);

/// Leans smaller than this (radians) with rates below it count as rest.
const REST_RAD: f32 = 2.0e-4;

/// Identity of an instance: its pivot (the model's translation), to a millimetre.
fn pivot_key(model: &[[f32; 4]; 4]) -> u64 {
    let q = |v: f32| (v * 1000.0).round() as i64 as u64;
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for v in [q(model[3][0]), q(model[3][1]), q(model[3][2])] {
        h ^= v;
        h = h.wrapping_mul(0x0100_0000_01b3).rotate_left(23);
    }
    h
}

/// One semi-implicit Euler step of `x'' = ω²(target − x) − 2ζω x'`.
fn spring(x: &mut f32, v: &mut f32, target: f32, omega: f32, zeta: f32, h: f32) {
    let acc = omega * omega * (target - *x) - 2.0 * zeta * omega * *v;
    *v += acc * h;
    *x += *v * h;
}

fn inside(p: [f32; 3], b: Option<([f32; 3], [f32; 3])>) -> bool {
    b.is_none_or(|(lo, hi)| (0..3).all(|k| p[k] >= lo[k] && p[k] <= hi[k]))
}

/// Height scale of a model (length of its Y column).
fn column_len(model: &[[f32; 4]; 4], c: usize) -> f32 {
    (model[c][0] * model[c][0] + model[c][1] * model[c][1] + model[c][2] * model[c][2]).sqrt()
}

impl SwayField {
    /// An empty field with `settings`.
    #[must_use]
    pub fn new(settings: SwaySettings) -> Self {
        Self {
            settings: settings.sanitized(),
            states: HashMap::new(),
            mesh_top: HashMap::new(),
        }
    }

    /// The settings.
    #[must_use]
    pub fn settings(&self) -> SwaySettings {
        self.settings
    }

    /// Instances swaying.
    #[must_use]
    pub fn len(&self) -> usize {
        self.states.len()
    }

    /// True when nothing sways.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.states.is_empty()
    }

    /// Advances every tree by `dt` seconds in `gusts`, starting trees among `draws` that the
    /// gust reaches. Call once per frame, before [`Self::apply_frame`].
    pub fn step(&mut self, gusts: &dyn GustField, draws: &[FrameMesh<'_>], dt: f32) -> SwayStats {
        let _span = tracing::info_span!(target: "regia_terrain", "forest_sway_step").entered();
        let candidates = draws
            .iter()
            .filter(|d| d.mesh_key >> 56 != regia_render::SHADOW_LOD_MESH_TAG >> 56)
            .map(|d| (d.model, d.mesh_key, d.vertices));
        self.step_with(gusts, candidates, dt)
    }

    /// [`Self::step`] for the ECS frame's extracted meshes. Only scatter draws sway (the
    /// scatter field extracts them without an entity); characters and props are left alone.
    pub fn step_extracted(&mut self, gusts: &dyn GustField, draws: &[ExtractedMesh], dt: f32) -> SwayStats {
        let _span = tracing::info_span!(target: "regia_terrain", "forest_sway_step").entered();
        let candidates = draws
            .iter()
            .filter(|d| d.entity == Entity::PLACEHOLDER && d.lod_key != regia_render::SHADOW_LOD_MESH_TAG)
            .map(|d| {
                (
                    d.model,
                    std::sync::Arc::as_ptr(&d.mesh) as u64,
                    d.mesh.vertices.as_slice(),
                )
            });
        self.step_with(gusts, candidates, dt)
    }

    fn step_with<'v>(
        &mut self,
        gusts: &dyn GustField,
        candidates: impl Iterator<Item = SwayCandidate<'v>>,
        dt: f32,
    ) -> SwayStats {
        let s = self.settings;
        let mut stats = SwayStats::default();
        let bounds = gusts.bounds();
        let empty = bounds.is_some_and(|(lo, hi)| (0..3).any(|k| hi[k] <= lo[k]));
        if !empty {
            for (model, mesh_key, vertices) in candidates {
                stats.examined += 1;
                let pivot = [model[3][0], model[3][1], model[3][2]];
                let key = pivot_key(&model);
                if self.states.contains_key(&key) {
                    continue;
                }
                let top = *self
                    .mesh_top
                    .entry(mesh_key)
                    .or_insert_with(|| vertices.iter().map(|v| v.position[1]).fold(0.0f32, f32::max));
                let height = top * column_len(&model, 1);
                if height < s.min_height_m {
                    continue;
                }
                let up = Vec3::new(model[1][0], model[1][1], model[1][2]).normalize_or(Vec3::Y);
                let probe = (Vec3::from_array(pivot) + up * (0.7 * height)).to_array();
                if !inside(probe, bounds) {
                    continue;
                }
                let g = gusts.gust(probe);
                if (g[0] * g[0] + g[2] * g[2]).sqrt() < 0.05 {
                    continue;
                }
                if self.states.len() >= s.max_instances {
                    stats.refused += 1;
                    continue;
                }
                self.states.insert(
                    key,
                    SwayState {
                        tilt: [0.0; 2],
                        rate: [0.0; 2],
                        probe,
                        omega: std::f32::consts::TAU * s.frequency(height),
                    },
                );
                stats.started += 1;
            }
        }
        let dt = if dt.is_finite() { dt.clamp(0.0, 0.25) } else { 0.0 };
        let mut max_tilt = 0.0f32;
        let mut settled = Vec::new();
        for (key, st) in &mut self.states {
            let g = if inside(st.probe, bounds) && !empty {
                gusts.gust(st.probe)
            } else {
                [0.0; 3]
            };
            let gh = (g[0] * g[0] + g[2] * g[2]).sqrt();
            let target = if gh > 1e-6 {
                let t = s.steady_tilt(gh) / gh;
                [g[0] * t, g[2] * t]
            } else {
                [0.0; 2]
            };
            // Semi-implicit Euler in substeps short against the period (stable for ζ ≥ 0).
            let n = ((st.omega * dt) / 0.25).ceil().max(1.0) as u32;
            let h = dt / n as f32;
            for _ in 0..n {
                let [tx, tz] = &mut st.tilt;
                let [rx, rz] = &mut st.rate;
                spring(tx, rx, target[0], st.omega, s.damping_ratio, h);
                spring(tz, rz, target[1], st.omega, s.damping_ratio, h);
            }
            let mag = (st.tilt[0] * st.tilt[0] + st.tilt[1] * st.tilt[1]).sqrt();
            let rate = (st.rate[0] * st.rate[0] + st.rate[1] * st.rate[1]).sqrt();
            max_tilt = max_tilt.max(mag);
            if gh < 0.05 && mag < REST_RAD && rate < REST_RAD {
                settled.push(*key);
            }
        }
        for key in settled {
            self.states.remove(&key);
            stats.settled += 1;
        }
        stats.swaying = self.states.len() as u32;
        stats.max_tilt_mrad = (max_tilt * 1000.0).round() as u32;
        stats
    }

    /// The lean of the instance whose model is `model`, as a rotation to apply about its pivot
    /// (`None` when it does not sway).
    #[must_use]
    pub fn lean(&self, model: &[[f32; 4]; 4]) -> Option<Quat> {
        let st = self.states.get(&pivot_key(model))?;
        let (tx, tz) = (st.tilt[0], st.tilt[1]);
        let mag = (tx * tx + tz * tz).sqrt();
        if mag <= 0.0 {
            return None;
        }
        // Leaning toward +X tips the top toward +X: a rotation about (z, 0, −x) / |θ|, i.e.
        // −Z for +X.
        let axis = Vec3::new(tz, 0.0, -tx) / mag;
        Some(Quat::from_axis_angle(axis, mag))
    }

    fn leaned(&self, model: &[[f32; 4]; 4]) -> Option<[[f32; 4]; 4]> {
        let q = self.lean(model)?;
        // Rotate the basis about the pivot; the pivot itself (the translation) stays exact.
        let mut out = (Mat4::from_quat(q) * Mat4::from_cols_array_2d(model)).to_cols_array_2d();
        out[3] = model[3];
        Some(out)
    }

    /// Leans every draw of a swaying instance (all its parts and LOD copies share its model).
    /// Returns the draws changed.
    pub fn apply_frame(&self, draws: &mut [FrameMesh<'_>]) -> u32 {
        if self.states.is_empty() {
            return 0;
        }
        let mut changed = 0;
        for d in draws.iter_mut() {
            if let Some(m) = self.leaned(&d.model) {
                d.model = m;
                changed += 1;
            }
        }
        changed
    }

    /// [`Self::apply_frame`] for the ECS frame's extracted meshes (scatter draws only, as
    /// [`Self::step_extracted`]).
    pub fn apply_extracted(&self, draws: &mut [ExtractedMesh]) -> u32 {
        if self.states.is_empty() {
            return 0;
        }
        let mut changed = 0;
        for d in draws.iter_mut().filter(|d| d.entity == Entity::PLACEHOLDER) {
            if let Some(m) = self.leaned(&d.model) {
                d.model = m;
                changed += 1;
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pass(wake: &mut TrackWake, speed: f32, frames: u32) {
        for f in 0..frames {
            let t = f64::from(f) / 60.0;
            wake.push(WakeSample {
                time_s: t,
                position: [-200.0 + speed * t as f32, 10.0, 0.0],
            });
        }
    }

    #[test]
    fn the_wake_trails_the_camera_and_fades_across_and_behind() {
        let mut wake = TrackWake::new(WakeModel::default());
        pass(&mut wake, 100.0, 180);
        let head = wake.track.back().map(|t| t.position).expect("track");
        // Just behind, on the track: strong, along the flight direction (+X).
        let near = wake.gust([head.x - 20.0, 10.0, 0.0]);
        assert!(
            near[0] > 10.0 && near[1].abs() < 1e-3 && near[2].abs() < 1e-3,
            "{near:?}"
        );
        // Farther behind: weaker.
        let far = wake.gust([head.x - 200.0, 10.0, 0.0]);
        assert!(far[0] > 0.0 && far[0] < near[0], "{far:?} vs {near:?}");
        // Off the track: Gaussian falloff.
        let side = wake.gust([head.x - 20.0, 10.0, 8.0]);
        assert!(side[0] < near[0] * 0.5, "{side:?}");
        // Ahead of the camera: the nearest track point is the head (x = 0) but 50 m away.
        let ahead = wake.gust([head.x + 50.0, 10.0, 0.0]);
        assert!(ahead[0] < 1e-3, "{ahead:?}");
        // Below the speed threshold: nothing.
        let mut slow = TrackWake::new(WakeModel::default());
        pass(&mut slow, 5.0, 180);
        assert_eq!(slow.gust([-200.0 + 5.0 * 2.0, 10.0, 0.0]), [0.0; 3]);
    }

    #[test]
    fn a_tree_leans_with_the_gust_and_rings_down() {
        struct Constant([f32; 3], bool);
        impl GustField for Constant {
            fn gust(&self, _p: [f32; 3]) -> [f32; 3] {
                if self.1 { self.0 } else { [0.0; 3] }
            }
            fn bounds(&self) -> Option<([f32; 3], [f32; 3])> {
                None
            }
        }
        let vertices = vec![
            LabVertex::from_pos_normal_uv([0.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0]),
            LabVertex::from_pos_normal_uv([0.0, 10.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0]),
        ];
        let material = regia_render::LabMaterial::default();
        let model = Mat4::from_translation(Vec3::new(5.0, 0.0, 5.0)).to_cols_array_2d();
        let mut draws = vec![FrameMesh {
            is_static: false,
            mesh_key: 1,
            material_key: 1,
            object_key: 1,
            vertices: &vertices,
            indices: &[0, 1, 0],
            material: &material,
            model,
            joints: &[],
            receiver_layer: 0,
            shadow: regia_render::ShadowCast::Off,
            uv1: &[],
            lod_dither: 0.0,
        }];
        let mut sway = SwayField::new(SwaySettings::default());
        let gust = Constant([20.0, 0.0, 0.0], true);
        let mut peak = 0.0f32;
        for _ in 0..120 {
            sway.step(&gust, &draws, 1.0 / 60.0);
            peak = peak.max(sway.states.values().map(|s| s.tilt[0]).fold(0.0, f32::max));
        }
        let steady = SwaySettings::default().steady_tilt(20.0);
        // Underdamped: overshoots the steady lean, toward +X.
        assert!(
            peak > steady && peak < 2.0 * steady,
            "peak {peak} steady {steady}"
        );
        let mut leaned = draws.clone();
        assert_eq!(sway.apply_frame(&mut leaned), 1);
        let top = Mat4::from_cols_array_2d(&leaned[0].model).transform_point3(Vec3::new(0.0, 10.0, 0.0));
        assert!(top.x > 5.0 + 0.5 && (top.z - 5.0).abs() < 1e-3, "{top:?}");
        // The pivot stays put.
        assert_eq!(leaned[0].model[3], model[3]);
        // Gust off: it rings down and is dropped.
        let calm = Constant([0.0; 3], false);
        for _ in 0..60 * 60 {
            sway.step(&calm, &draws, 1.0 / 60.0);
        }
        assert!(sway.is_empty());
        assert_eq!(sway.apply_frame(&mut draws), 0);
        assert_eq!(draws[0].model, model);
    }

    #[test]
    fn still_air_changes_nothing() {
        let mut sway = SwayField::new(SwaySettings::default());
        let stats = sway.step(&StillAir, &[], 1.0 / 60.0);
        assert_eq!(stats, SwayStats::default());
        assert!(sway.is_empty());
    }
}
