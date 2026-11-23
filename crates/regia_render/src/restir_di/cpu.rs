// SPDX-License-Identifier: MPL-2.0
//! The CPU reference of the many-light sampler (ADR-2470): the brute-force sum over every light,
//! and the resampled estimator pass for pass as `shaders/restir_di.wgsl` runs it (same random
//! streams, same order), so the GPU can be held to it.
//!
//! [`evaluate`] runs many independent seeds over a fixed G-buffer and reports the bias against
//! the brute force with its standard error, and the error of one uniform light sample, of
//! resampling without reuse and of the full spatiotemporal estimator.

use regia_core::glam::{Vec3, Vec4};

use super::{
    CLUSTER_GRID, ManyLight, RestirCamera, RestirDiError, RestirDiSettings, Rng, STREAM_INITIAL,
    STREAM_SPATIAL, STREAM_TEMPORAL, ShadingInputs, Surface, cluster_of, luminance, shade, supports, target,
    world_at_uv,
};

/// A G-buffer: per pixel, row-major from the top-left.
#[derive(Clone, Debug, PartialEq)]
pub struct GBuffer {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Linear view depth; 0 where there is no surface (sky).
    pub depth: Vec<f32>,
    /// Unit world normal.
    pub normal: Vec<[f32; 3]>,
    /// Linear base color.
    pub albedo: Vec<[f32; 3]>,
    /// Perceptual roughness.
    pub roughness: Vec<f32>,
    /// Metallic.
    pub metallic: Vec<f32>,
    /// Screen-UV offset to where the surface was last frame (`prev_uv - uv`).
    pub motion: Vec<[f32; 2]>,
}

impl GBuffer {
    /// Pixel count.
    #[must_use]
    pub fn len(&self) -> usize {
        (self.width * self.height) as usize
    }

    /// No pixels.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn check(&self) -> Result<(), RestirDiError> {
        let n = self.len();
        if [
            self.depth.len(),
            self.normal.len(),
            self.albedo.len(),
            self.roughness.len(),
            self.metallic.len(),
            self.motion.len(),
        ]
        .iter()
        .any(|&l| l != n)
        {
            return Err(RestirDiError::BadInput(format!(
                "G-buffer planes must all hold {n} pixels"
            )));
        }
        Ok(())
    }

    /// The shading points: `None` where there is no surface.
    #[must_use]
    pub fn surfaces(&self, camera: &RestirCamera) -> Vec<Option<Surface>> {
        let world_from_clip = camera.clip_from_world.inverse();
        let eye = Vec3::from(camera.eye);
        let forward = Vec3::from(camera.forward);
        (0..self.len())
            .map(|i| {
                let depth = self.depth[i];
                if depth <= 0.0 {
                    return None;
                }
                let x = i as u32 % self.width;
                let y = i as u32 / self.width;
                let uv = [
                    (x as f32 + 0.5) / self.width as f32,
                    (y as f32 + 0.5) / self.height as f32,
                ];
                Some(Surface {
                    position: world_at_uv(world_from_clip, eye, forward, uv, depth),
                    normal: Vec3::from(self.normal[i]).normalize_or(Vec3::Y),
                    depth,
                })
            })
            .collect()
    }

    fn material(&self, i: usize) -> ShadingInputs {
        ShadingInputs {
            albedo: self.albedo[i],
            roughness: self.roughness[i],
            metallic: self.metallic[i],
        }
    }
}

/// The frame's clustered light lists for a camera (`cluster_build.wgsl`): per froxel, the
/// sampled lights whose influence sphere touches the froxel's view-space box.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ClusterLists {
    /// Lights per cluster (at most `max_per_cluster`).
    pub counts: Vec<u32>,
    /// `cluster × max_per_cluster + slot` → light index.
    pub indices: Vec<u32>,
    /// Slots per cluster.
    pub max_per_cluster: u32,
    /// Clusters that had more lights than slots (their extra lights are never sampled).
    pub overflowed: u32,
    /// Froxel boxes of the last build (view space), reused.
    boxes: Vec<[[f32; 3]; 2]>,
}

impl ClusterLists {
    /// Builds the lists of `lights` for `camera` at aspect ratio `aspect`.
    #[must_use]
    pub fn build(camera: &RestirCamera, aspect: f32, lights: &[ManyLight], max_per_cluster: u32) -> Self {
        let mut lists = Self::default();
        lists.rebuild(camera, aspect, lights, max_per_cluster);
        lists
    }

    /// Rebuilds the lists in place (no allocation once the sizes are reached).
    pub fn rebuild(
        &mut self,
        camera: &RestirCamera,
        aspect: f32,
        lights: &[ManyLight],
        max_per_cluster: u32,
    ) {
        let [gx, gy, gz] = CLUSTER_GRID;
        let clusters = (gx * gy * gz) as usize;
        let max_per = max_per_cluster.max(1);
        self.max_per_cluster = max_per;
        self.counts.clear();
        self.counts.resize(clusters, 0);
        self.indices.resize(clusters * max_per as usize, 0);
        self.boxes.clear();
        let tan_y = (camera.fov_y * 0.5).tan();
        let slice = |s: f32| camera.near * (camera.far / camera.near).powf(s / gz as f32);
        for cz in 0..gz {
            let (z0, z1) = (slice(cz as f32), slice(cz as f32 + 1.0));
            for cy in 0..gy {
                for cx in 0..gx {
                    let mut lo = Vec3::splat(1e30);
                    let mut hi = Vec3::splat(-1e30);
                    for z in [z0, z1] {
                        for uy in [cy as f32 / gy as f32, (cy + 1) as f32 / gy as f32] {
                            for ux in [cx as f32 / gx as f32, (cx + 1) as f32 / gx as f32] {
                                let p = Vec3::new(
                                    (ux * 2.0 - 1.0) * aspect * tan_y * z,
                                    (1.0 - uy * 2.0) * tan_y * z,
                                    -z,
                                );
                                lo = lo.min(p);
                                hi = hi.max(p);
                            }
                        }
                    }
                    self.boxes.push([lo.to_array(), hi.to_array()]);
                }
            }
        }
        for (li, light) in lights.iter().enumerate() {
            if !light.sampled() {
                continue;
            }
            let c = (camera.view_from_world
                * Vec4::new(
                    light.position_kind[0],
                    light.position_kind[1],
                    light.position_kind[2],
                    1.0,
                ))
            .truncate();
            let r = light.direction_radius[3];
            for (cid, [lo, hi]) in self.boxes.iter().enumerate() {
                if c.clamp(Vec3::from(*lo), Vec3::from(*hi)).distance(c) <= r {
                    let n = self.counts[cid];
                    if n < max_per {
                        self.indices[cid * max_per as usize + n as usize] = li as u32;
                    }
                    self.counts[cid] = n + 1;
                }
            }
        }
        self.overflowed = 0;
        for n in &mut self.counts {
            if *n > max_per {
                self.overflowed += 1;
                *n = max_per;
            }
        }
    }

    /// Mean and largest list length over the clusters.
    #[must_use]
    pub fn occupancy(&self) -> (f32, u32) {
        let max = self.counts.iter().copied().max().unwrap_or(0);
        let mean = self.counts.iter().map(|&c| c as f32).sum::<f32>() / self.counts.len().max(1) as f32;
        (mean, max)
    }
}

/// Where a pixel's candidates come from.
#[derive(Clone, Copy, Debug)]
pub enum Candidates<'a> {
    /// Every light, uniformly.
    All,
    /// The pixel's cluster list, uniformly.
    Clusters(&'a ClusterLists),
}

impl Candidates<'_> {
    /// The list of pixel `(x, y)`: `(count, base, clustered)`.
    fn list(&self, x: u32, y: u32, g: &GBuffer, camera: &RestirCamera, lights: usize) -> (u32, usize, bool) {
        match self {
            Self::All => (lights as u32, 0, false),
            Self::Clusters(c) => {
                let i = (y * g.width + x) as usize;
                let uv = [
                    (x as f32 + 0.5) / g.width as f32,
                    (y as f32 + 0.5) / g.height as f32,
                ];
                let cid = cluster_of(uv, g.depth[i], camera.near, camera.far) as usize;
                (
                    c.counts[cid].min(c.max_per_cluster),
                    cid * c.max_per_cluster as usize,
                    true,
                )
            }
        }
    }

    fn light(&self, base: usize, j: u32, clustered: bool) -> u32 {
        match self {
            Self::Clusters(c) if clustered => c.indices[base + j as usize],
            _ => j,
        }
    }
}

/// One pixel's reservoir: the chosen light, its unbiased contribution weight `W` and the number
/// of candidates `M` it summarizes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Reservoir {
    /// Chosen light, [`Reservoir::NONE`] when none.
    pub light: u32,
    /// Contribution weight: the estimate is `f(light) × w`.
    pub w: f32,
    /// Candidates summarised.
    pub m: f32,
}

impl Reservoir {
    /// No light chosen.
    pub const NONE: u32 = u32::MAX;
    const EMPTY: Self = Self {
        light: Self::NONE,
        w: 0.0,
        m: 0.0,
    };
}

/// A shading point with what the target reads beside it.
#[derive(Clone, Copy)]
struct Point<'a> {
    surface: &'a Surface,
    view: Vec3,
    material: ShadingInputs,
}

fn target_of(lights: &[ManyLight], light: u32, p: &Point<'_>) -> f32 {
    lights
        .get(light as usize)
        .map_or(0.0, |l| target(l, p.surface, p.view, &p.material))
}

fn supported(lights: &[ManyLight], light: u32, surface: &Surface) -> bool {
    lights.get(light as usize).is_some_and(|l| supports(l, surface))
}

/// Whether the surface `q` is close enough to `p` to share samples (ADR-864's rules).
pub(crate) fn similar(p: &Surface, q: &Surface, settings: &RestirDiSettings) -> bool {
    p.position.distance(q.position) <= settings.depth_tolerance * p.depth
        && p.normal.dot(q.normal) >= settings.normal_tolerance
}

/// Brute force: every light's unshadowed contribution summed per pixel.
#[must_use]
pub fn brute_force(g: &GBuffer, camera: &RestirCamera, lights: &[ManyLight]) -> Vec<[f32; 3]> {
    let eye = Vec3::from(camera.eye);
    g.surfaces(camera)
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let Some(s) = s else { return [0.0; 3] };
            let view = (eye - s.position).normalize_or(Vec3::Y);
            let material = g.material(i);
            let mut sum = [0.0f32; 3];
            for l in lights {
                for (a, c) in sum.iter_mut().zip(shade(l, s, view, &material)) {
                    *a += c;
                }
            }
            sum
        })
        .collect()
}

/// One uniformly chosen light from the pixel's list, weighted by the list length: the plain
/// Monte Carlo baseline at one shading evaluation per pixel.
#[must_use]
pub fn uniform_one_sample(
    g: &GBuffer,
    camera: &RestirCamera,
    lights: &[ManyLight],
    candidates: Candidates<'_>,
    frame: u32,
    seed: u32,
) -> Vec<[f32; 3]> {
    let eye = Vec3::from(camera.eye);
    g.surfaces(camera)
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let Some(s) = s else { return [0.0; 3] };
            let (x, y) = (i as u32 % g.width, i as u32 / g.width);
            let (count, base, clustered) = candidates.list(x, y, g, camera, lights.len());
            if count == 0 {
                return [0.0; 3];
            }
            let mut rng = Rng::new(i as u32, frame, STREAM_INITIAL, seed);
            let j = ((rng.next() * count as f32) as u32).min(count - 1);
            let li = candidates.light(base, j, clustered);
            let view = (eye - s.position).normalize_or(Vec3::Y);
            let c = shade(&lights[li as usize], s, view, &g.material(i));
            c.map(|v| v * count as f32)
        })
        .collect()
}

/// The resampled estimator with its history: one call per frame.
#[derive(Clone, Debug)]
pub struct ReferenceSampler {
    settings: RestirDiSettings,
    seed: u32,
    frame: u32,
    prev: Vec<Reservoir>,
    prev_surfaces: Vec<Option<Surface>>,
    has_history: bool,
}

/// What one frame of the sampler did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReferenceFrame {
    /// Estimated radiance per pixel.
    pub radiance: Vec<[f32; 3]>,
    /// The reservoirs kept for the next frame.
    pub reservoirs: Vec<Reservoir>,
    /// Pixels whose previous reservoir passed the reprojection checks.
    pub temporal_accepted: u32,
    /// Neighbor reservoirs combined over the frame.
    pub spatial_accepted: u32,
}

impl ReferenceSampler {
    /// A sampler with no history.
    ///
    /// # Errors
    /// [`RestirDiError::InvalidSetting`] when `settings` fails validation.
    pub fn new(settings: RestirDiSettings, seed: u32) -> Result<Self, RestirDiError> {
        settings.validate()?;
        Ok(Self {
            settings,
            seed,
            frame: 0,
            prev: Vec::new(),
            prev_surfaces: Vec::new(),
            has_history: false,
        })
    }

    /// Drops the history (a camera cut).
    pub fn reset(&mut self) {
        self.has_history = false;
    }

    /// Frames run.
    #[must_use]
    pub fn frames(&self) -> u32 {
        self.frame
    }

    /// Runs one frame: initial candidates and temporal reuse, spatial reuse, shading.
    ///
    /// # Errors
    /// [`RestirDiError::BadInput`] when the G-buffer planes differ in length.
    pub fn frame(
        &mut self,
        g: &GBuffer,
        camera: &RestirCamera,
        lights: &[ManyLight],
        candidates: Candidates<'_>,
    ) -> Result<ReferenceFrame, RestirDiError> {
        g.check()?;
        let s = &self.settings;
        let n = g.len();
        let surfaces = g.surfaces(camera);
        let history = self.has_history && s.temporal && self.prev.len() == n && self.prev_surfaces.len() == n;
        let frame = self.frame;
        let m_initial = s.initial_candidates as f32;
        let mut out = ReferenceFrame {
            radiance: vec![[0.0; 3]; n],
            reservoirs: vec![Reservoir::EMPTY; n],
            ..ReferenceFrame::default()
        };
        // Pass 1: initial candidates, then the temporal combine.
        let mut tmp = vec![Reservoir::EMPTY; n];
        let eye = Vec3::from(camera.eye);
        for i in 0..n {
            let Some(surface) = surfaces[i] else { continue };
            let point = Point {
                surface: &surface,
                view: (eye - surface.position).normalize_or(Vec3::Y),
                material: g.material(i),
            };
            let (x, y) = (i as u32 % g.width, i as u32 / g.width);
            let (count, base, clustered) = candidates.list(x, y, g, camera, lights.len());
            let mut rng = Rng::new(i as u32, frame, STREAM_INITIAL, self.seed);
            let mut w_sum = 0.0f32;
            let mut chosen = Reservoir::NONE;
            for _ in 0..s.initial_candidates {
                let u = rng.next();
                let u_pick = rng.next();
                if count == 0 {
                    continue;
                }
                let j = ((u * count as f32) as u32).min(count - 1);
                let li = candidates.light(base, j, clustered);
                let w = target_of(lights, li, &point) * count as f32;
                w_sum += w;
                if w > 0.0 && u_pick * w_sum < w {
                    chosen = li;
                }
            }
            let p = target_of(lights, chosen, &point);
            let mut r = Reservoir {
                light: chosen,
                w: if p > 0.0 { w_sum / (m_initial * p) } else { 0.0 },
                m: m_initial,
            };
            if history {
                let mut rng = Rng::new(i as u32, frame, STREAM_TEMPORAL, self.seed);
                let u_pick = rng.next();
                let uv = [
                    (x as f32 + 0.5) / g.width as f32,
                    (y as f32 + 0.5) / g.height as f32,
                ];
                let prev_uv = [uv[0] + g.motion[i][0], uv[1] + g.motion[i][1]];
                let px = (prev_uv[0] * g.width as f32).floor();
                let py = (prev_uv[1] * g.height as f32).floor();
                if px >= 0.0 && py >= 0.0 && px < g.width as f32 && py < g.height as f32 {
                    let q = py as usize * g.width as usize + px as usize;
                    if let Some(ps) = self.prev_surfaces[q].filter(|ps| similar(&surface, ps, s)) {
                        let rp = self.prev[q];
                        let mp = rp.m.min(s.history_cap * r.m);
                        let wc = p * r.w * r.m;
                        let wp = target_of(lights, rp.light, &point) * rp.w * mp;
                        let sum = wc + wp;
                        let y_sel = if wp > 0.0 && u_pick * sum < wp {
                            rp.light
                        } else {
                            r.light
                        };
                        let p_sel = target_of(lights, y_sel, &point);
                        let mut z = 0.0;
                        if p_sel > 0.0 {
                            z += r.m;
                        }
                        if supported(lights, y_sel, &ps) {
                            z += mp;
                        }
                        r = Reservoir {
                            light: y_sel,
                            w: if p_sel > 0.0 && z > 0.0 {
                                sum / (z * p_sel)
                            } else {
                                0.0
                            },
                            m: r.m + mp,
                        };
                        out.temporal_accepted += 1;
                    }
                }
            }
            tmp[i] = r;
        }
        // Pass 2: spatial reuse.
        let mut accepted: Vec<usize> = Vec::with_capacity(s.spatial_taps as usize + 1);
        for i in 0..n {
            let Some(surface) = surfaces[i] else { continue };
            let point = Point {
                surface: &surface,
                view: (eye - surface.position).normalize_or(Vec3::Y),
                material: g.material(i),
            };
            let (x, y) = (i as i64 % i64::from(g.width), i as i64 / i64::from(g.width));
            let own = tmp[i];
            let mut w_sum = target_of(lights, own.light, &point) * own.w * own.m;
            let mut chosen = own.light;
            let mut m_total = own.m;
            accepted.clear();
            accepted.push(i);
            let mut rng = Rng::new(i as u32, frame, STREAM_SPATIAL, self.seed);
            for _ in 0..s.spatial_taps {
                let angle = rng.next() * std::f32::consts::TAU;
                let radius = s.spatial_radius_px * rng.next().sqrt();
                let u_pick = rng.next();
                let qx = x + (radius * angle.cos()).round() as i64;
                let qy = y + (radius * angle.sin()).round() as i64;
                if (qx == x && qy == y)
                    || qx < 0
                    || qy < 0
                    || qx >= i64::from(g.width)
                    || qy >= i64::from(g.height)
                {
                    continue;
                }
                let q = (qy * i64::from(g.width) + qx) as usize;
                let Some(qs) = surfaces[q] else { continue };
                if !similar(&surface, &qs, s) || accepted.contains(&q) {
                    continue;
                }
                let rq = tmp[q];
                let wq = target_of(lights, rq.light, &point) * rq.w * rq.m;
                w_sum += wq;
                m_total += rq.m;
                accepted.push(q);
                out.spatial_accepted += 1;
                if wq > 0.0 && u_pick * w_sum < wq {
                    chosen = rq.light;
                }
            }
            let p_sel = target_of(lights, chosen, &point);
            let mut z = 0.0;
            if p_sel > 0.0 {
                for &q in &accepted {
                    if let Some(qs) = surfaces[q]
                        && supported(lights, chosen, &qs)
                    {
                        z += tmp[q].m;
                    }
                }
            }
            let r = Reservoir {
                light: chosen,
                w: if p_sel > 0.0 && z > 0.0 {
                    w_sum / (z * p_sel)
                } else {
                    0.0
                },
                m: m_total,
            };
            out.reservoirs[i] = r;
            // Pass 3: shading with the chosen light.
            if p_sel > 0.0 && r.w > 0.0 {
                let c = shade(&lights[chosen as usize], &surface, point.view, &point.material);
                out.radiance[i] = c.map(|v| v * r.w);
            }
        }
        self.prev.clone_from(&out.reservoirs);
        self.prev_surfaces = surfaces;
        self.has_history = true;
        self.frame += 1;
        Ok(out)
    }
}

/// Relative RMSE of `estimate` against `truth` over the pixels where the truth is lit, in
/// luminance, divided by the mean lit luminance.
#[must_use]
pub fn relative_rmse(estimate: &[[f32; 3]], truth: &[[f32; 3]]) -> f64 {
    let mut se = 0.0f64;
    let mut sum = 0.0f64;
    let mut n = 0u32;
    for (e, t) in estimate.iter().zip(truth) {
        let lt = f64::from(luminance(*t));
        if lt <= 0.0 {
            continue;
        }
        let d = f64::from(luminance(*e)) - lt;
        se += d * d;
        sum += lt;
        n += 1;
    }
    if n == 0 || sum <= 0.0 {
        return 0.0;
    }
    (se / f64::from(n)).sqrt() / (sum / f64::from(n))
}

/// Mean luminance of an image.
#[must_use]
pub fn mean_luminance(image: &[[f32; 3]]) -> f64 {
    image.iter().map(|c| f64::from(luminance(*c))).sum::<f64>() / image.len().max(1) as f64
}

/// What [`evaluate`] measured.
#[derive(Clone, Debug, PartialEq)]
pub struct ReferenceReport {
    /// Pixels with a surface.
    pub surface_pixels: u32,
    /// Lights in the scene.
    pub lights: u32,
    /// Mean and largest cluster list length (0 for [`Candidates::All`]).
    pub cluster_mean: f32,
    /// Largest cluster list length.
    pub cluster_max: u32,
    /// Clusters whose lights did not all fit.
    pub cluster_overflowed: u32,
    /// Independent seeds.
    pub seeds: u32,
    /// Frames per seed (the estimate scored is the last frame's).
    pub frames: u32,
    /// Mean luminance of the brute-force image.
    pub truth_mean: f64,
    /// Mean over seeds of the estimator's mean luminance.
    pub estimate_mean: f64,
    /// `(estimate_mean − truth_mean) / truth_mean`.
    pub relative_bias: f64,
    /// `|estimate_mean − truth_mean|` in standard errors of the seed mean: below 4 means no bias
    /// is detectable at these seeds.
    pub bias_sigma: f64,
    /// Relative RMSE (per pixel, mean over seeds) of one uniform light sample.
    pub rmse_uniform: f64,
    /// Relative RMSE of resampling without reuse at the same candidate count.
    pub rmse_candidates: f64,
    /// Relative RMSE of the spatiotemporal estimator at its last frame.
    pub rmse_restir: f64,
    /// `(rmse_uniform / rmse_restir)²`: variance removed against the uniform baseline.
    pub variance_reduction: f64,
    /// Relative RMSE per frame, mean over seeds (history building up).
    pub convergence: Vec<f64>,
}

/// Runs `seeds` independent seeds of `frames` frames each over a static scene and compares the
/// last frame against the brute force (see [`ReferenceReport`]).
///
/// # Errors
/// [`RestirDiError`] from the settings or the G-buffer.
pub fn evaluate(
    g: &GBuffer,
    camera: &RestirCamera,
    lights: &[ManyLight],
    candidates: Candidates<'_>,
    settings: &RestirDiSettings,
    frames: u32,
    seeds: u32,
) -> Result<ReferenceReport, RestirDiError> {
    let truth = brute_force(g, camera, lights);
    let truth_mean = mean_luminance(&truth);
    let mut means = Vec::with_capacity(seeds as usize);
    let (mut rmse_u, mut rmse_c, mut rmse_r) = (0.0, 0.0, 0.0);
    let mut convergence = vec![0.0f64; frames.max(1) as usize];
    let candidates_only = RestirDiSettings::candidates_only(settings.initial_candidates);
    for seed in 0..seeds {
        let mut sampler = ReferenceSampler::new(settings.clone(), seed)?;
        let mut last = Vec::new();
        for c in &mut convergence {
            last = sampler.frame(g, camera, lights, candidates)?.radiance;
            *c += relative_rmse(&last, &truth);
        }
        means.push(mean_luminance(&last));
        rmse_r += relative_rmse(&last, &truth);
        rmse_u += relative_rmse(
            &uniform_one_sample(g, camera, lights, candidates, 0, seed),
            &truth,
        );
        let mut ris = ReferenceSampler::new(candidates_only.clone(), seed)?;
        rmse_c += relative_rmse(&ris.frame(g, camera, lights, candidates)?.radiance, &truth);
    }
    let s = f64::from(seeds.max(1));
    convergence.iter_mut().for_each(|c| *c /= s);
    let estimate_mean = means.iter().sum::<f64>() / s;
    let var = means.iter().map(|m| (m - estimate_mean).powi(2)).sum::<f64>() / (s - 1.0).max(1.0);
    let se = (var / s).sqrt().max(1e-12);
    let (cluster_mean, cluster_max, cluster_overflowed) = match candidates {
        Candidates::All => (0.0, 0, 0),
        Candidates::Clusters(c) => {
            let (mean, max) = c.occupancy();
            (mean, max, c.overflowed)
        }
    };
    let rmse_restir = rmse_r / s;
    let rmse_uniform = rmse_u / s;
    Ok(ReferenceReport {
        surface_pixels: g.depth.iter().filter(|d| **d > 0.0).count() as u32,
        lights: lights.len() as u32,
        cluster_mean,
        cluster_max,
        cluster_overflowed,
        seeds,
        frames,
        truth_mean,
        estimate_mean,
        relative_bias: (estimate_mean - truth_mean) / truth_mean.max(1e-12),
        bias_sigma: (estimate_mean - truth_mean).abs() / se,
        rmse_uniform,
        rmse_candidates: rmse_c / s,
        rmse_restir,
        variance_reduction: (rmse_uniform / rmse_restir.max(1e-12)).powi(2),
        convergence,
    })
}

#[cfg(test)]
mod tests;
