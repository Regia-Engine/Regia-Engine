// SPDX-License-Identifier: MPL-2.0
//! A CPU probe tracer ([`CpuProbeTracer`]): the same probe estimate as the GPU kernels
//! ([`crate::probe`]), on the CPU, for bakes without a GPU (`regia bake-gi --cpu`, CI, the
//! `cargo xtask bake-check` fixture). It is a reference for small scenes, not a second production
//! baker: it is exact for the cases the end-to-end checks need (an open probe under a uniform sky,
//! a sealed room) and close elsewhere.
//!
//! What it traces, per probe and sample (ADR-365):
//! - one randomly rotated spherical Fibonacci ray set (Keinert et al. 2015; Shoemake 1992 for
//!   the rotation), so the ray directions and the SH and moment folds are the GPU's;
//! - first hits against every instance's triangles and the terrain heightfield's triangles
//!   (cells split like `PtHeightfield::height_at`), through the binned SAH [`Bvh`] and
//!   Möller–Trumbore ray/triangle tests (Möller & Trumbore 1997);
//! - the sky and sky bodies seen straight from the probe (sky and fill components), emissive
//!   surfaces seen at any vertex (emissive component), and at every front-face vertex next-event
//!   estimation of the sun (sun-bounce component) and of each sky body as a distant light of its
//!   disc irradiance (fill component), with a shadow ray each;
//! - Lambertian bounces, cosine-sampled (albedo = base colour × (1 − metallic)), up to
//!   `max_bounces` vertices with Russian roulette from `rr_start`; a bounce ray that escapes
//!   adds the sky.
//!
//! Differences from the GPU tracer, by design: no specular lobe, textures, participating media,
//! sky tables or emissive next-event estimation (an emitter is found by the bounce rays only), and
//! a sky body lights a vertex as a direction, not a cone. A sky table, an equirect map or a cloud
//! field is refused (bake those on the GPU): the CPU tracer does not apply cloud extinction, so
//! it will not return a clear-sky bake for a scene that has one. A back face ends the path (a
//! probe inside geometry sees nothing). The GPU quality controls of ADR-1096 are not implemented,
//! so a description that sets `clamp_direct`, `clamp_indirect` or `firefly_threshold` (finite
//! and above 0) or `sample_groups` above 1 is refused with a [`PtError::Scene`] message: an
//! explicit error, not an unclamped mean with no noise estimate (ADR-5517). With them at their
//! defaults, [`ProbeTrace::clamped`], `clamp_loss`, `suppressed_groups`, `firefly_loss` and
//! `noise` stay at their defaults. `rays_per_probe` is clamped to 1..=4096 before the rays are
//! traced, and `samples` is capped so that `samples × rays_per_probe` stays within 2^24, as on
//! the GPU (whose ray counter is an f32). The random streams differ, so a CPU and a
//! GPU bake agree in expectation, not bit for bit. The result does not depend on the thread count.

#[cfg(test)]
use std::cell::Cell;
use std::cell::RefCell;
use std::f32::consts::PI;
use std::sync::Arc;

use regia_asset::baked_gi::{GI_COMPONENTS, SH_Y00, SH_Y1, ShRgb, texel_direction};
use regia_core::glam::Vec3;

use crate::PtError;
use crate::bvh::{Aabb, Bvh};
use crate::probe::{ProbeTrace, ProbeTraceSettings, ProbeTracer};
use crate::scene::{PtScene, PtSky};

/// Component slots (`regia_asset::baked_gi::GiComponent` order).
const SUN: usize = 0;
const SKY: usize = 1;
const FILL: usize = 2;
const EMISSIVE: usize = 3;

#[cfg(test)]
thread_local! {
    /// Worlds [`World::build`] has constructed on this thread.
    static WORLD_BUILDS: Cell<u64> = const { Cell::new(0) };
}

thread_local! {
    /// The last world built on this thread, keyed by [`PtScene::content_hash`].
    static WORLD_CACHE: RefCell<Option<(u64, Arc<World>)>> = const { RefCell::new(None) };
}

/// CPU probe worlds built on this thread. A cache hit does not add one.
#[cfg(test)]
#[must_use]
pub(crate) fn cpu_probe_world_builds() -> u64 {
    WORLD_BUILDS.with(Cell::get)
}

/// The CPU probe tracer. `threads` 0 uses every core; the result is the same for any count.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuProbeTracer {
    /// Worker threads (0 = `std::thread::available_parallelism`).
    pub threads: usize,
}

impl CpuProbeTracer {
    /// A tracer on every core.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A tracer on `threads` worker threads (0 = every core).
    #[must_use]
    pub fn with_threads(threads: usize) -> Self {
        Self { threads }
    }

    fn thread_count(&self, probes: usize) -> usize {
        let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
        let want = if self.threads == 0 { cores } else { self.threads };
        want.clamp(1, probes.max(1))
    }
}

/// The CPU tracer has no per-contribution clamps, sample groups or firefly suppression: a
/// request for them is refused rather than traced unclamped with `noise: None`.
fn check_quality_controls(s: &ProbeTraceSettings) -> Result<(), PtError> {
    let set = |v: f32| v.is_finite() && v > 0.0;
    if set(s.clamp_direct) || set(s.clamp_indirect) || set(s.firefly_threshold) || s.sample_groups > 1 {
        return Err(PtError::Scene(
            "the CPU probe tracer does not implement clamp_direct, clamp_indirect, sample_groups or \
             firefly_threshold: leave them at their defaults or bake on the GPU"
                .into(),
        ));
    }
    Ok(())
}

impl ProbeTracer for CpuProbeTracer {
    /// Refuses `clamp_direct`, `clamp_indirect`, `sample_groups` above 1 and `firefly_threshold`
    /// (GPU-only quality controls). `samples × rays_per_probe` is capped at 2^24 as on the GPU.
    /// The coherent order and path compaction are GPU schedules ([`ProbeTraceSettings`],
    /// ADR-862) with nothing to change here. A geometry-only trace records the first hit and
    /// leaves the SH at zero, the same contract as the GPU flag.
    fn trace_streams(
        &self,
        scene: &PtScene,
        positions: &[[f32; 3]],
        streams: &[u32],
        settings: &ProbeTraceSettings,
        progress: &mut dyn FnMut(usize, usize) -> bool,
    ) -> Result<Vec<ProbeTrace>, PtError> {
        let _span = tracing::info_span!("pt_cpu_trace_probes", probes = positions.len()).entered();
        if streams.len() != positions.len() {
            return Err(PtError::Scene(format!(
                "{} probe streams for {} probes",
                streams.len(),
                positions.len()
            )));
        }
        if streams.iter().any(|&s| s >= 1 << 24) {
            return Err(PtError::Scene("a probe stream is 2^24 or more".into()));
        }
        if positions.is_empty() {
            return Ok(Vec::new());
        }
        if positions.iter().flatten().any(|v| !v.is_finite()) {
            return Err(PtError::Scene("a probe position is not finite".into()));
        }
        check_quality_controls(settings)?;
        let hash = scene.content_hash();
        let cached = WORLD_CACHE.with(|slot| {
            slot.borrow()
                .as_ref()
                .filter(|(cached_hash, _)| *cached_hash == hash)
                .map(|(_, world)| Arc::clone(world))
        });
        let world = match cached {
            Some(world) => world,
            None => {
                let built = Arc::new(World::build(scene)?);
                #[cfg(test)]
                WORLD_BUILDS.with(|c| c.set(c.get() + 1));
                WORLD_CACHE.with(|slot| *slot.borrow_mut() = Some((hash, Arc::clone(&built))));
                built
            }
        };
        let job = Job::new(settings);
        let batch = (settings.probes_per_batch.max(1) as usize).min(positions.len());
        let mut out = Vec::with_capacity(positions.len());
        for (b, chunk) in positions.chunks(batch).enumerate() {
            let first = b * batch;
            let threads = self.thread_count(chunk.len());
            let per = chunk.len().div_ceil(threads);
            let results: Vec<Vec<ProbeTrace>> = std::thread::scope(|s| {
                let handles: Vec<_> = chunk
                    .chunks(per)
                    .enumerate()
                    .map(|(t, part)| {
                        let (world, job) = (&world, &job);
                        let keys = &streams[first + t * per..first + t * per + part.len()];
                        s.spawn(move || {
                            part.iter()
                                .zip(keys)
                                .map(|(p, &key)| world.trace_probe(job, Vec3::from_array(*p), key))
                                .collect::<Vec<_>>()
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().unwrap_or_default())
                    .collect()
            });
            for part in results {
                out.extend(part);
            }
            if out.len() < first + chunk.len() {
                return Err(PtError::Scene("a CPU probe worker failed".into()));
            }
            if !progress(out.len(), positions.len()) {
                return Err(PtError::Scene("probe trace canceled".into()));
            }
        }
        Ok(out)
    }

    fn backend(&self) -> &'static str {
        "cpu"
    }
}

/// The clamped settings of one trace.
struct Job {
    rays: u32,
    samples: u32,
    bounces: u32,
    rr_start: u32,
    seed: u32,
    res: u32,
    exponent: f32,
    far: f32,
    /// Record the first hit only. SH stays zero (the GPU `FLAG_PROBE_GEOMETRY` contract).
    geometry_only: bool,
}

impl Job {
    fn new(s: &ProbeTraceSettings) -> Self {
        Self {
            rays: s.rays_per_probe.clamp(1, 4096),
            // The GPU's cap, so `samples × rays` stays exact in the f32 normalization.
            samples: s
                .samples
                .max(1)
                .min(crate::probe::max_samples(s.rays_per_probe.clamp(1, 4096))),
            bounces: s.max_bounces.clamp(1, 64),
            rr_start: s.rr_start.max(1),
            seed: s.seed,
            res: s.moment_resolution.clamp(1, 32),
            exponent: s.moment_exponent.max(1.0),
            far: if s.max_distance > 0.0 {
                s.max_distance
            } else {
                32.0
            },
            geometry_only: s.geometry_only,
        }
    }
}

/// One world-space triangle: corner, two edges, geometric normal (from the winding, unit) and
/// its surface.
struct Tri {
    a: Vec3,
    e1: Vec3,
    e2: Vec3,
    n: Vec3,
    albedo: Vec3,
    emissive: Vec3,
}

/// A distant light the vertices sample: direction toward it and its irradiance at normal
/// incidence (lux × colour), credited to `component`.
struct Distant {
    toward: Vec3,
    irradiance: Vec3,
    component: usize,
}

/// A sky body seen from the probe: centre, cosine of its radius, radiance × light scale.
struct Disc {
    centre: Vec3,
    cos_radius: f32,
    radiance: Vec3,
}

struct World {
    tris: Vec<Tri>,
    bvh: Bvh,
    sky: Vec3,
    discs: Vec<Disc>,
    lights: Vec<Distant>,
    /// Ray offset off a surface, metres (scaled to the scene).
    eps: f32,
}

/// A ray's first hit: distance, triangle, back face.
#[derive(Clone, Copy)]
struct Hit {
    t: f32,
    tri: usize,
    back: bool,
}

impl World {
    fn build(scene: &PtScene) -> Result<Self, PtError> {
        let _span = tracing::info_span!("pt_cpu_world").entered();
        let sky = match &scene.sky {
            PtSky::None => Vec3::ZERO,
            PtSky::Uniform(c) => Vec3::from_array(*c),
            PtSky::Table { .. } | PtSky::Equirect { .. } => {
                return Err(PtError::Scene(
                    "the CPU probe tracer takes a uniform sky or none, not a sky table or an HDRI map: bake on the GPU"
                        .into(),
                ));
            }
        };
        if scene.media.is_active() {
            tracing::warn!(target: "regia_pt", "the CPU probe tracer ignores participating media");
        }
        // The GPU probe shade attenuates sun and sky by cloud transmittance. This tracer does
        // not, so a success would be a clear-sky bake. Refuse, as a sky table is refused.
        if scene.clouds.is_some() {
            return Err(PtError::Scene(
                "the CPU probe tracer does not trace a cloud field: bake on the GPU".into(),
            ));
        }
        let mut tris = Vec::new();
        let surface = |m: usize| -> (Vec3, Vec3) {
            scene
                .materials
                .get(m)
                .map_or((Vec3::splat(0.5), Vec3::ZERO), |m| {
                    let b = m.base_color;
                    let k = (1.0 - m.metallic).clamp(0.0, 1.0);
                    (
                        Vec3::new(b[0], b[1], b[2]).clamp(Vec3::ZERO, Vec3::ONE) * k,
                        Vec3::from_array(m.emissive).max(Vec3::ZERO),
                    )
                })
        };
        let mut push = |a: Vec3, b: Vec3, c: Vec3, albedo: Vec3, emissive: Vec3| {
            let (e1, e2) = (b - a, c - a);
            let n = e1.cross(e2);
            if n.length_squared() > 0.0 && n.is_finite() {
                tris.push(Tri {
                    a,
                    e1,
                    e2,
                    n: n.normalize(),
                    albedo,
                    emissive,
                });
            }
        };
        for inst in &scene.instances {
            let Some(mesh) = scene.meshes.get(inst.mesh) else {
                return Err(PtError::Scene(format!(
                    "instance mesh {} is out of range",
                    inst.mesh
                )));
            };
            let (albedo, emissive) = surface(inst.material);
            let world: Vec<Vec3> = mesh
                .positions
                .iter()
                .map(|p| inst.transform.transform_point3(Vec3::from_array(*p)))
                .collect();
            for t in mesh.indices.as_chunks::<3>().0 {
                let corner = |k: usize| world.get(t[k] as usize).copied();
                let (Some(a), Some(b), Some(c)) = (corner(0), corner(1), corner(2)) else {
                    return Err(PtError::Scene("a mesh index is out of range".into()));
                };
                push(a, b, c, albedo, emissive);
            }
        }
        if let Some(hf) = &scene.terrain {
            // The same two checks as `pack`: a `u32` product wraps (65536 × 65536 is 0), and a
            // short buffer used to skip the ground and still return success.
            let samples = u64::from(hf.width).checked_mul(u64::from(hf.depth));
            if hf.width < 2 || hf.depth < 2 || samples.is_none_or(|n| hf.heights.len() as u64 != n) {
                return Err(PtError::Scene(
                    "terrain heightfield has the wrong sample count".into(),
                ));
            }
            if !hf.spacing.iter().all(|s| s.is_finite() && *s > 0.0)
                || !hf.origin.iter().all(|o| o.is_finite())
            {
                return Err(PtError::Scene(
                    "terrain spacing must be positive and finite, and the origin finite".into(),
                ));
            }
            let (albedo, emissive) = surface(hf.material);
            let at = |i: u32, j: u32| {
                Vec3::new(
                    hf.origin[0] + i as f32 * hf.spacing[0],
                    hf.height(i, j),
                    hf.origin[1] + j as f32 * hf.spacing[1],
                )
            };
            for j in 0..hf.depth - 1 {
                for i in 0..hf.width - 1 {
                    let (p00, p10, p01, p11) = (at(i, j), at(i + 1, j), at(i, j + 1), at(i + 1, j + 1));
                    // A cut corner drops the cell (a hole), as the tracer does.
                    if ![p00, p10, p01, p11].iter().all(|p| p.y.is_finite()) {
                        continue;
                    }
                    // Split along the (i, j+1)–(i+1, j) diagonal, both triangles facing up.
                    push(p00, p01, p10, albedo, emissive);
                    push(p11, p10, p01, albedo, emissive);
                }
            }
        }
        let bounds: Vec<Aabb> = tris
            .iter()
            .map(|t| Aabb::from_points(&[t.a.to_array(), (t.a + t.e1).to_array(), (t.a + t.e2).to_array()]))
            .collect();
        let bvh = Bvh::build(&bounds);
        let extent = bvh.bounds();
        let size = if extent.is_empty() {
            1.0
        } else {
            (0..3)
                .map(|k| extent.max[k].abs().max(extent.min[k].abs()))
                .fold(1.0f32, f32::max)
        };

        let mut lights = Vec::new();
        if let Some(sun) = &scene.sun
            && sun.illuminance > 0.0
        {
            lights.push(Distant {
                toward: Vec3::from_array(sun.toward).normalize_or(Vec3::Y),
                irradiance: Vec3::from_array(sun.color) * sun.illuminance,
                component: SUN,
            });
        }
        // The frame's extra directional lights (ADR-7254): sunlight as well, in the sun component.
        for extra in scene.extra_suns.iter().filter(|s| s.illuminance > 0.0) {
            lights.push(Distant {
                toward: Vec3::from_array(extra.toward).normalize_or(Vec3::Y),
                irradiance: Vec3::from_array(extra.color) * extra.illuminance,
                component: SUN,
            });
        }
        let mut discs = Vec::new();
        for body in &scene.sky_bodies {
            let mean = if body.radiance.is_empty() {
                Vec3::ZERO
            } else {
                body.radiance.iter().map(|c| Vec3::from_array(*c)).sum::<Vec3>() / body.radiance.len() as f32
            };
            let r = body.angular_radius.clamp(0.0, 1.5);
            let centre = Vec3::from_array(body.direction).normalize_or(Vec3::Y);
            let scaled = mean * body.light_scale.max(0.0);
            discs.push(Disc {
                centre,
                cos_radius: r.cos(),
                radiance: scaled,
            });
            // A uniform disc's irradiance at normal incidence: π·L·sin²r.
            lights.push(Distant {
                toward: centre,
                irradiance: scaled * PI * r.sin() * r.sin(),
                component: FILL,
            });
        }
        Ok(Self {
            tris,
            bvh,
            sky,
            discs,
            lights,
            eps: 1e-4 * size,
        })
    }

    /// The closest hit along `o + t·d`, `t` in `(0, t_max)`.
    fn closest(&self, o: Vec3, d: Vec3, t_max: f32) -> Option<Hit> {
        let mut best: Option<Hit> = None;
        let mut limit = t_max;
        self.walk(o, d, &mut limit, &mut |tri, t, back| {
            best = Some(Hit { t, tri, back });
            false
        });
        best
    }

    /// Whether anything lies along `o + t·d` before `t_max`.
    fn occluded(&self, o: Vec3, d: Vec3, t_max: f32) -> bool {
        let mut hit = false;
        let mut limit = t_max;
        self.walk(o, d, &mut limit, &mut |_, _, _| {
            hit = true;
            true
        });
        hit
    }

    /// Visits the triangles hit before `*limit` (shrinking it to each hit); `found` returns
    /// `true` to stop.
    fn walk(&self, o: Vec3, d: Vec3, limit: &mut f32, found: &mut dyn FnMut(usize, f32, bool) -> bool) {
        if self.tris.is_empty() {
            return;
        }
        let inv = Vec3::ONE / d;
        let mut stack = [0u32; 64];
        let mut top = 1usize;
        while top > 0 {
            top -= 1;
            let Some(node) = self.bvh.nodes.get(stack[top] as usize) else {
                continue;
            };
            if !slab(node.min, node.max, o, inv, *limit) {
                continue;
            }
            if node.is_leaf() {
                for k in node.a..node.a + node.count {
                    let Some(&tri) = self.bvh.order.get(k as usize) else {
                        continue;
                    };
                    let t = &self.tris[tri as usize];
                    if let Some(dist) = intersect(t, o, d)
                        && dist < *limit
                    {
                        *limit = dist;
                        if found(tri as usize, dist, t.n.dot(d) > 0.0) {
                            return;
                        }
                    }
                }
            } else if top + 2 <= stack.len() {
                stack[top] = node.a;
                stack[top + 1] = node.a + 1;
                top += 2;
            }
        }
    }

    fn escaped(&self, d: Vec3, first: bool, comps: &mut [Vec3; GI_COMPONENTS], thr: Vec3) {
        comps[SKY] += thr * self.sky;
        if first {
            // Seen straight from the probe: a body at its lighting scale (later vertices sample
            // the bodies as lights instead).
            for disc in &self.discs {
                if d.dot(disc.centre) >= disc.cos_radius {
                    comps[FILL] += thr * disc.radiance;
                }
            }
        }
    }

    /// Traces probe `index` at `p`: every sample's ray set, folded into SH, stats and moments.
    fn trace_probe(&self, job: &Job, p: Vec3, index: u32) -> ProbeTrace {
        let rays = job.rays;
        let texels = (job.res * job.res) as usize;
        let texel_dirs: Vec<Vec3> = (0..texels)
            .map(|t| Vec3::from_array(texel_direction(t, job.res)))
            .collect();
        let mut sh = [[[0.0f32; 4]; 3]; GI_COMPONENTS];
        let mut moments = vec![[0.0f32; 3]; texels];
        let mut out = ProbeTrace {
            sh: [[[0.0; 4]; 3]; GI_COMPONENTS],
            rays: 0,
            back_faces: 0,
            hits: 0,
            rejected: 0,
            closest_back: None,
            closest_front: None,
            moments: Vec::new(),
            // The CPU tracer has no clamps or sample groups (ADR-1096 is the GPU kernel's).
            ..ProbeTrace::default()
        };
        let mut total_rays = 0u32;
        for sample in 0..job.samples {
            let mut rot = Rng::new(index, sample, 0xfffe, job.seed);
            let q = random_quat([rot.uniform(), rot.uniform(), rot.uniform()]);
            for ray in 0..rays {
                let dir = quat_rotate(q, fibonacci_dir(ray, rays)).normalize_or(Vec3::Y);
                let mut rng = Rng::new(index, sample, ray, job.seed ^ 0x9e37_79b9);
                let (comps, first) = self.path(job, p, dir, &mut rng);
                if !comps.iter().all(|c| c.is_finite()) {
                    out.rejected += 1;
                    continue;
                }
                total_rays += 1;
                let y = [SH_Y00, SH_Y1 * dir.x, SH_Y1 * dir.y, SH_Y1 * dir.z];
                for (dst, c) in sh.iter_mut().zip(comps) {
                    for (channel, value) in dst.iter_mut().zip(c.to_array()) {
                        for (s, ym) in channel.iter_mut().zip(y) {
                            *s += value * ym;
                        }
                    }
                }
                let dist = match first {
                    Some(hit) => {
                        out.hits += 1;
                        let slot = if hit.back {
                            out.back_faces += 1;
                            &mut out.closest_back
                        } else {
                            &mut out.closest_front
                        };
                        if slot.is_none_or(|(_, d)| hit.t < d) {
                            *slot = Some((dir.to_array(), hit.t));
                        }
                        hit.t.min(job.far)
                    }
                    None => job.far,
                };
                for (m, t) in moments.iter_mut().zip(&texel_dirs) {
                    let w = t.dot(dir).max(0.0).powf(job.exponent);
                    if w > 0.0 {
                        m[0] += w * dist;
                        m[1] += w * dist * dist;
                        m[2] += w;
                    }
                }
            }
        }
        // Monte Carlo over uniformly distributed directions: ∫ L·Y dω ≈ 4π/N · Σ L·Y.
        let inv = if total_rays > 0 {
            4.0 * PI / total_rays as f32
        } else {
            0.0
        };
        for (dst, src) in out.sh.iter_mut().zip(&sh) {
            *dst = scale(src, inv);
        }
        out.rays = total_rays;
        out.moments = moments
            .iter()
            .map(|m| {
                if m[2] > 0.0 {
                    [m[0] / m[2], m[1] / m[2]]
                } else {
                    [job.far, job.far * job.far]
                }
            })
            .collect();
        out
    }

    /// One path from the probe along `dir`: the light it brings back per component, and its
    /// first hit.
    fn path(&self, job: &Job, p: Vec3, dir: Vec3, rng: &mut Rng) -> ([Vec3; GI_COMPONENTS], Option<Hit>) {
        let mut comps = [Vec3::ZERO; GI_COMPONENTS];
        let first = self.closest(p, dir, f32::INFINITY);
        // Relocation consumes the first hit, not the shade. Matching the GPU flag keeps the
        // spherical-harmonic coefficients at zero instead of shading a bounce that is discarded.
        if job.geometry_only {
            return (comps, first);
        }
        let mut hit = first;
        let mut origin = p;
        let mut d = dir;
        let mut thr = Vec3::ONE;
        let mut vertex = 0u32;
        loop {
            let Some(h) = hit else {
                self.escaped(d, vertex == 0, &mut comps, thr);
                break;
            };
            if h.back {
                break;
            }
            let tri = &self.tris[h.tri];
            comps[EMISSIVE] += thr * tri.emissive;
            if vertex >= job.bounces {
                break;
            }
            let x = origin + d * h.t;
            let n = tri.n;
            let lifted = x + n * self.eps;
            let reflect = thr * tri.albedo / PI;
            if reflect.max_element() > 0.0 {
                for light in &self.lights {
                    let cos = n.dot(light.toward);
                    if cos > 0.0 && !self.occluded(lifted, light.toward, f32::INFINITY) {
                        comps[light.component] += reflect * light.irradiance * cos;
                    }
                }
            }
            vertex += 1;
            thr *= tri.albedo;
            if vertex >= job.rr_start {
                let keep = thr.max_element().clamp(0.05, 1.0);
                if rng.uniform() >= keep {
                    break;
                }
                thr /= keep;
            }
            if thr.max_element() <= 0.0 {
                break;
            }
            d = cosine_dir(n, rng.uniform(), rng.uniform());
            origin = lifted;
            hit = self.closest(origin, d, f32::INFINITY);
            if vertex >= job.bounces {
                // The last vertex's bounce ray: the light it reaches, no further vertex.
                match hit {
                    None => self.escaped(d, false, &mut comps, thr),
                    Some(h) if !h.back => comps[EMISSIVE] += thr * self.tris[h.tri].emissive,
                    Some(_) => {}
                }
                break;
            }
        }
        (comps, first)
    }
}

fn scale(sh: &ShRgb, k: f32) -> ShRgb {
    sh.map(|c| c.map(|v| v * k))
}

/// Ray/box slab test (Kay & Kajiya 1986): the ray enters the box before `t_max`.
fn slab(min: [f32; 3], max: [f32; 3], o: Vec3, inv: Vec3, t_max: f32) -> bool {
    let (lo, hi) = (Vec3::from_array(min), Vec3::from_array(max));
    let t0 = (lo - o) * inv;
    let t1 = (hi - o) * inv;
    let near = t0.min(t1).max_element().max(0.0);
    let far = t0.max(t1).min_element().min(t_max);
    // NaN (a zero direction component on a slab plane) fails the compare: treat it as a hit.
    near <= far || near.is_nan() || far.is_nan()
}

/// Möller–Trumbore ray/triangle intersection, both faces; the distance when `t > 0`.
fn intersect(t: &Tri, o: Vec3, d: Vec3) -> Option<f32> {
    let p = d.cross(t.e2);
    let det = t.e1.dot(p);
    if det.abs() < 1e-12 {
        return None;
    }
    let inv = 1.0 / det;
    let s = o - t.a;
    let u = s.dot(p) * inv;
    if !(0.0..=1.0).contains(&u) {
        return None;
    }
    let q = s.cross(t.e1);
    let v = d.dot(q) * inv;
    if v < 0.0 || u + v > 1.0 {
        return None;
    }
    let dist = t.e2.dot(q) * inv;
    (dist > 1e-6).then_some(dist)
}

/// Spherical Fibonacci direction `i` of `n`, Y up (`pt.wgsl` `fibonacci_dir`).
fn fibonacci_dir(i: u32, n: u32) -> Vec3 {
    let golden = PI * (3.0 - 5.0f32.sqrt());
    let y = 1.0 - (2.0 * i as f32 + 1.0) / n.max(1) as f32;
    let r = (1.0 - y * y).max(0.0).sqrt();
    let theta = golden * i as f32;
    Vec3::new(r * theta.cos(), y, r * theta.sin())
}

/// A uniformly distributed unit quaternion `(x, y, z, w)` from three uniforms (Shoemake 1992).
fn random_quat(u: [f32; 3]) -> [f32; 4] {
    let a = (1.0 - u[0]).sqrt();
    let b = u[0].sqrt();
    let tau = 2.0 * PI;
    [
        a * (tau * u[1]).sin(),
        a * (tau * u[1]).cos(),
        b * (tau * u[2]).sin(),
        b * (tau * u[2]).cos(),
    ]
}

fn quat_rotate(q: [f32; 4], v: Vec3) -> Vec3 {
    let axis = Vec3::new(q[0], q[1], q[2]);
    let t = 2.0 * axis.cross(v);
    v + q[3] * t + axis.cross(t)
}

/// A cosine-distributed direction around `n` (Malley's method on Duff et al. 2017's basis).
fn cosine_dir(n: Vec3, u1: f32, u2: f32) -> Vec3 {
    let sign = if n.z >= 0.0 { 1.0 } else { -1.0 };
    let a = -1.0 / (sign + n.z);
    let b = n.x * n.y * a;
    let t = Vec3::new(1.0 + sign * n.x * n.x * a, sign * b, -sign * n.x);
    let s = Vec3::new(b, sign + n.y * n.y * a, -n.y);
    let r = u1.sqrt();
    let phi = 2.0 * PI * u2;
    (t * (r * phi.cos()) + s * (r * phi.sin()) + n * (1.0 - u1).max(0.0).sqrt()).normalize_or(n)
}

/// The PCG output hash (Jarzynski & Olano 2020, "Hash Functions for GPU Rendering").
fn pcg(v: u32) -> u32 {
    let s = v.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    let w = ((s >> ((s >> 28) + 4)) ^ s).wrapping_mul(277_803_737);
    (w >> 22) ^ w
}

/// A per-(probe, sample, ray) random stream, independent of how the probes are split.
struct Rng(u32);

impl Rng {
    fn new(a: u32, b: u32, c: u32, d: u32) -> Self {
        Self(pcg(a ^ pcg(b ^ pcg(c ^ pcg(d)))))
    }

    /// A uniform in `[0, 1)`.
    fn uniform(&mut self) -> f32 {
        self.0 = pcg(self.0);
        (self.0 >> 8) as f32 / 16_777_216.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::{PtCamera, PtMaterial, PtMesh};
    use regia_asset::baked_gi::irradiance;
    use regia_core::glam::Mat4;

    fn scene() -> PtScene {
        PtScene::new(PtCamera::pinhole([0.0, 0.0, 5.0], [0.0; 3], 1.0, 8, 8))
    }

    fn settings() -> ProbeTraceSettings {
        ProbeTraceSettings {
            rays_per_probe: 128,
            samples: 2,
            max_bounces: 3,
            ..ProbeTraceSettings::default()
        }
    }

    #[test]
    fn gpu_only_quality_controls_are_refused() {
        let positions = [[0.0f32; 3]];
        for bad in [
            ProbeTraceSettings {
                clamp_indirect: 4.0,
                ..settings()
            },
            ProbeTraceSettings {
                clamp_direct: 4.0,
                ..settings()
            },
            ProbeTraceSettings {
                sample_groups: 4,
                ..settings()
            },
            ProbeTraceSettings {
                firefly_threshold: 6.0,
                ..settings()
            },
        ] {
            let e = CpuProbeTracer::with_threads(1)
                .trace_streams(&scene(), &positions, &[0], &bad, &mut |_, _| true)
                .expect_err("refused");
            assert!(e.to_string().contains("CPU probe tracer"), "{e}");
        }
        assert!(check_quality_controls(&settings()).is_ok());
    }

    /// An axis-aligned box `min..max` as 12 triangles, faces outward (`inward` flips them).
    fn box_mesh(min: [f32; 3], max: [f32; 3], inward: bool) -> PtMesh {
        let c = |i: usize| {
            [
                if i & 1 == 0 { min[0] } else { max[0] },
                if i & 2 == 0 { min[1] } else { max[1] },
                if i & 4 == 0 { min[2] } else { max[2] },
            ]
        };
        let positions: Vec<[f32; 3]> = (0..8).map(c).collect();
        // Each face as two triangles, counter-clockwise seen from outside.
        let faces: [[u32; 4]; 6] = [
            [0, 4, 6, 2], // -X
            [1, 3, 7, 5], // +X
            [0, 1, 5, 4], // -Y
            [2, 6, 7, 3], // +Y
            [0, 2, 3, 1], // -Z
            [4, 5, 7, 6], // +Z
        ];
        let mut indices = Vec::new();
        for f in faces {
            let tri = [f[0], f[1], f[2], f[0], f[2], f[3]];
            if inward {
                for t in tri.chunks(3) {
                    indices.extend_from_slice(&[t[0], t[2], t[1]]);
                }
            } else {
                indices.extend_from_slice(&tri);
            }
        }
        PtMesh {
            normals: vec![[0.0; 3]; positions.len()],
            positions,
            indices,
            ..PtMesh::default()
        }
    }

    /// A sealed room: an inward shell inside an outward one, `t` metres thick.
    fn room(scene: &mut PtScene, half: f32, t: f32, albedo: f32) {
        let m = scene.add_material(PtMaterial::lambert([albedo; 3]));
        let inner = scene.add_mesh(box_mesh([-half; 3], [half; 3], true));
        let outer = scene.add_mesh(box_mesh([-half - t; 3], [half + t; 3], false));
        scene.add_instance(inner, m, Mat4::IDENTITY);
        scene.add_instance(outer, m, Mat4::IDENTITY);
    }

    fn trace(scene: &PtScene, at: [f32; 3]) -> ProbeTrace {
        CpuProbeTracer::with_threads(1)
            .trace(scene, &[at], &settings(), &mut |_, _| true)
            .expect("trace")
            .remove(0)
    }

    /// Under a uniform sky with nothing around, every ray brings the sky back: the irradiance
    /// is π·L for every normal, the moments are the far distance, no component but sky.
    #[test]
    fn an_open_probe_sees_the_uniform_sky_exactly() {
        let mut s = scene();
        s.sky = PtSky::Uniform([2.0, 1.0, 0.5]);
        let t = trace(&s, [0.0; 3]);
        assert_eq!(t.rays, 256);
        assert_eq!(t.hits, 0);
        for n in [[0.0, 1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, -1.0]] {
            let e = irradiance(&t.sh[SKY], n);
            for (got, want) in e.iter().zip([2.0, 1.0, 0.5]) {
                assert!((got / PI - want).abs() < 0.02 * want, "{e:?} for {n:?}");
            }
        }
        for c in [SUN, FILL, EMISSIVE] {
            assert!(t.sh[c].iter().flatten().all(|v| *v == 0.0));
        }
        let far = settings().max_distance;
        assert!(t.moments.iter().all(|m| (m[0] - far).abs() < 1e-3));
    }

    /// `rays_per_probe` above 4096 is traced as 4096, and `samples` is capped so that
    /// `samples × rays` stays within 2^24 as on the GPU. With the GPU-only quality controls at
    /// their defaults nothing is clamped: a bright sky stays in the volume and the quality fields
    /// stay at their defaults. The same oversized request with any quality control set is refused
    /// (ADR-5517), never traced with the control silently dropped.
    #[test]
    fn rays_above_4096_are_traced_as_4096_and_quality_controls_are_refused() {
        let mut s = scene();
        s.sky = PtSky::Uniform([100.0; 3]);
        let settings = ProbeTraceSettings {
            rays_per_probe: 8192,
            samples: 1,
            max_bounces: 1,
            ..ProbeTraceSettings::default()
        };
        let t = CpuProbeTracer::with_threads(1)
            .trace(&s, &[[0.0; 3]], &settings, &mut |_, _| true)
            .expect("trace")
            .remove(0);
        assert_eq!(t.rays, 4096);
        assert_eq!(t.clamped, 0);
        assert_eq!(t.clamp_loss, 0.0);
        assert_eq!(t.suppressed_groups, 0);
        assert_eq!(t.firefly_loss, 0.0);
        assert!(t.noise.is_none());
        let e = irradiance(&t.sh[SKY], [0.0, 1.0, 0.0]);
        assert!(e[0] > 50.0, "unclamped sky irradiance {e:?}");

        // The cap: 4096 rays leave room for 2^24 / 4096 = 4096 samples, whatever is asked.
        let job = Job::new(&ProbeTraceSettings {
            samples: u32::MAX,
            ..settings
        });
        assert_eq!((job.rays, job.samples), (4096, 4096));
        assert_eq!(Job::new(&settings).samples, 1);

        for bad in [
            ProbeTraceSettings {
                clamp_direct: 1.0,
                ..settings
            },
            ProbeTraceSettings {
                clamp_indirect: 1.0,
                ..settings
            },
            ProbeTraceSettings {
                sample_groups: 8,
                ..settings
            },
            ProbeTraceSettings {
                firefly_threshold: 4.0,
                ..settings
            },
        ] {
            let e = CpuProbeTracer::with_threads(1)
                .trace(&s, &[[0.0; 3]], &bad, &mut |_, _| true)
                .expect_err("a GPU-only quality control is refused");
            assert!(matches!(e, PtError::Scene(_)), "{e}");
            assert!(e.to_string().contains("CPU probe tracer"), "{e}");
        }
    }

    /// The GPU-only quality controls are refused one by one (the module's contract), never
    /// silently ignored.
    #[test]
    fn each_gpu_quality_control_is_refused() {
        let s = scene();
        let base = ProbeTraceSettings {
            rays_per_probe: 16,
            samples: 1,
            max_bounces: 1,
            ..ProbeTraceSettings::default()
        };
        let cases = [
            ProbeTraceSettings {
                clamp_direct: 1.0,
                ..base
            },
            ProbeTraceSettings {
                clamp_indirect: 1.0,
                ..base
            },
            ProbeTraceSettings {
                sample_groups: 8,
                ..base
            },
            ProbeTraceSettings {
                firefly_threshold: 4.0,
                ..base
            },
        ];
        for (k, c) in cases.iter().enumerate() {
            let r = CpuProbeTracer::with_threads(1).trace(&s, &[[0.0; 3]], c, &mut |_, _| true);
            assert!(
                matches!(&r, Err(PtError::Scene(m)) if m.contains("does not implement")),
                "case {k}: {r:?}"
            );
        }
    }

    /// `samples × rays_per_probe` is capped at 2^24 as on the GPU (the module docs say so).
    #[test]
    fn cpu_samples_are_capped_like_the_gpu() {
        for (rays, samples) in [(4096, 8192), (8192, 8192), (64, 1 << 20), (1, u32::MAX)] {
            let job = Job::new(&ProbeTraceSettings {
                rays_per_probe: rays,
                samples,
                ..ProbeTraceSettings::default()
            });
            let traced = u64::from(job.rays) * u64::from(job.samples);
            assert!(traced <= 1 << 24, "{rays} × {samples} traced as {traced}");
            assert_eq!(job.samples, crate::probe::max_samples(rays.clamp(1, 4096)));
        }
    }

    /// Inside a sealed room no sky, sun or body light arrives: every component is exactly 0,
    /// every ray hits a front face, and the moments see the walls.
    #[test]
    fn a_sealed_room_is_black_inside_and_its_walls_are_front_faces() {
        let mut s = scene();
        s.sky = PtSky::Uniform([1.0; 3]);
        s.sun = Some(crate::PtSun {
            toward: [0.3, 1.0, 0.2],
            color: [1.0; 3],
            illuminance: 1e5,
            angular_radius: 0.0,
        });
        room(&mut s, 2.0, 0.2, 0.8);
        let t = trace(&s, [0.0; 3]);
        assert_eq!(t.hits, t.rays);
        assert_eq!(t.back_faces, 0);
        assert!(t.sh.iter().flatten().flatten().all(|v| *v == 0.0), "{:?}", t.sh);
        assert!(
            t.moments.iter().all(|m| m[0] > 1.9 && m[0] < 3.5),
            "{:?}",
            t.moments
        );
        // A probe inside the wall sees back faces.
        let wall = trace(&s, [2.1, 0.0, 0.0]);
        assert!(wall.back_face_fraction() > 0.25, "{}", wall.back_face_fraction());
    }

    /// A probe over a lit floor gets the sun's bounce: one vertex, an albedo-ρ Lambertian floor
    /// under a sun at elevation θ, seen over the lower hemisphere: irradiance up ≈ ρ·E·sin θ
    /// (the floor's radiance ρ·E·sinθ/π over the half sphere below).
    #[test]
    fn a_floor_bounces_the_sun_into_the_sun_component() {
        let mut s = scene();
        let m = s.add_material(PtMaterial::lambert([0.5; 3]));
        let floor = s.add_mesh(PtMesh {
            positions: vec![
                [-500.0, 0.0, -500.0],
                [500.0, 0.0, -500.0],
                [-500.0, 0.0, 500.0],
                [500.0, 0.0, 500.0],
            ],
            normals: vec![[0.0, 1.0, 0.0]; 4],
            indices: vec![0, 2, 1, 1, 2, 3],
            ..PtMesh::default()
        });
        s.add_instance(floor, m, Mat4::IDENTITY);
        let elevation = 30.0f32.to_radians();
        s.sun = Some(crate::PtSun {
            toward: [elevation.cos(), elevation.sin(), 0.0],
            color: [1.0; 3],
            illuminance: 1000.0,
            angular_radius: 0.0,
        });
        let t = trace(&s, [0.0, 1.0, 0.0]);
        assert_eq!(t.back_faces, 0);
        let down = irradiance(&t.sh[SUN], [0.0, -1.0, 0.0])[0];
        let want = 0.5 * 1000.0 * elevation.sin();
        assert!((down - want).abs() < 0.08 * want, "{down} vs {want}");
        assert!(t.sh[SKY].iter().flatten().all(|v| *v == 0.0));
    }

    /// A probe over a floor lit only by one of the frame's extra directional lights (ADR-7254):
    /// the floor bounces it into the sun component exactly as it bounces the same light as the
    /// sun (the same analytic ρ·E·sin θ the forward's Lambert floor sends up).
    #[test]
    fn a_floor_lit_only_by_an_extra_light_bounces_it_like_the_sun() {
        let mut s = scene();
        let m = s.add_material(PtMaterial::lambert([0.5; 3]));
        let floor = s.add_mesh(PtMesh {
            positions: vec![
                [-500.0, 0.0, -500.0],
                [500.0, 0.0, -500.0],
                [-500.0, 0.0, 500.0],
                [500.0, 0.0, 500.0],
            ],
            normals: vec![[0.0, 1.0, 0.0]; 4],
            indices: vec![0, 2, 1, 1, 2, 3],
            ..PtMesh::default()
        });
        s.add_instance(floor, m, Mat4::IDENTITY);
        let elevation = 30.0f32.to_radians();
        let light = crate::PtSun {
            toward: [elevation.cos(), elevation.sin(), 0.0],
            color: [1.0; 3],
            illuminance: 1000.0,
            angular_radius: 0.0,
        };
        let mut extra = s.clone();
        extra.extra_suns = vec![light];
        let t = trace(&extra, [0.0, 1.0, 0.0]);
        let down = irradiance(&t.sh[SUN], [0.0, -1.0, 0.0])[0];
        let want = 0.5 * 1000.0 * elevation.sin();
        assert!(down > 0.0, "the extra light reaches the probe");
        assert!((down - want).abs() < 0.08 * want, "{down} vs {want}");
        let mut as_sun = s;
        as_sun.sun = Some(light);
        assert_eq!(
            t,
            trace(&as_sun, [0.0, 1.0, 0.0]),
            "the same probe as with that light as the sun"
        );
    }

    /// The thread count changes nothing: the streams are keyed by probe index.
    #[test]
    fn the_result_does_not_depend_on_threads_or_batches() {
        let mut s = scene();
        s.sky = PtSky::Uniform([1.0; 3]);
        room(&mut s, 1.0, 0.1, 0.6);
        let positions: Vec<[f32; 3]> = (0..7).map(|k| [k as f32 - 3.0, 0.5, 2.5]).collect();
        let one = CpuProbeTracer::with_threads(1)
            .trace(&s, &positions, &settings(), &mut |_, _| true)
            .expect("one thread");
        let mut batched = settings();
        batched.probes_per_batch = 3;
        let many = CpuProbeTracer::with_threads(4)
            .trace(&s, &positions, &batched, &mut |_, _| true)
            .expect("four threads");
        assert_eq!(one, many);
        let mut calls = 0;
        let canceled = CpuProbeTracer::with_threads(2).trace(&s, &positions, &batched, &mut |_, _| {
            calls += 1;
            false
        });
        assert!(canceled.is_err());
        assert_eq!(calls, 1);
    }

    fn keyed_result(
        tracer: CpuProbeTracer,
        scene: &PtScene,
        positions: &[[f32; 3]],
        keys: &[u32],
        batch: u32,
    ) -> Vec<ProbeTrace> {
        let mut trace_settings = settings();
        trace_settings.probes_per_batch = batch;
        tracer
            .trace_streams(scene, positions, keys, &trace_settings, &mut |_, _| true)
            .expect("keyed trace")
    }

    /// Stable keys preserve every per-probe result across reorder, subset, worker and batch
    /// scheduling. CPU equality is bit equality; this does not promise CPU/GPU equality.
    #[test]
    fn explicit_stream_identity_survives_permutations_subsets_and_scheduling() {
        let mut s = scene();
        s.sky = PtSky::Uniform([0.17, 0.43, 0.91]);
        s.sun = Some(crate::PtSun {
            toward: [0.2, 0.9, 0.3],
            color: [1.0, 0.7, 0.4],
            illuminance: 1600.0,
            angular_radius: 0.04,
        });
        room(&mut s, 3.0, 0.15, 0.63);
        let panel = s.add_mesh(PtMesh {
            positions: vec![
                [-0.7, 2.7, -0.5],
                [0.7, 2.7, -0.5],
                [-0.7, 2.7, 0.5],
                [0.7, 2.7, 0.5],
            ],
            normals: vec![[0.0, -1.0, 0.0]; 4],
            indices: vec![0, 2, 1, 1, 2, 3],
            ..PtMesh::default()
        });
        let emitter = s.add_material(PtMaterial::emitter([8.0, 3.5, 1.2]));
        s.add_instance(panel, emitter, Mat4::IDENTITY);
        let positions = [
            [-1.7, -1.1, 0.2],
            [-0.7, -0.6, 0.5],
            [0.1, 0.0, -0.2],
            [0.8, 0.4, 0.7],
            [1.6, 1.3, -0.8],
            [0.2, -1.9, 1.1],
            [1.1, -0.3, -1.4],
        ];
        let keys = [17, 1_048_577, 3, 65_539, 5, 901, 42];
        let baseline = keyed_result(CpuProbeTracer::with_threads(1), &s, &positions, &keys, 7);
        assert!(baseline.iter().any(|trace| trace.hits > 0));
        assert!(
            baseline
                .iter()
                .any(|trace| trace.moments.iter().any(|m| m[0] < 30.0))
        );

        let permutation = [6, 2, 0, 5, 1, 4, 3];
        let permuted_positions = permutation.map(|i| positions[i]);
        let permuted_keys = permutation.map(|i| keys[i]);
        for workers in [1, 2, 6] {
            for batch in [1, 3, 8, 32] {
                let result = keyed_result(
                    CpuProbeTracer::with_threads(workers),
                    &s,
                    &permuted_positions,
                    &permuted_keys,
                    batch,
                );
                for (index, &key) in permuted_keys.iter().enumerate() {
                    let original = keys
                        .iter()
                        .position(|&candidate| candidate == key)
                        .expect("permuted key belongs to the full trace");
                    assert_eq!(
                        result[index], baseline[original],
                        "worker={workers}, batch={batch}, key={}",
                        keys[original]
                    );
                }
            }
        }

        for subset in [&[0, 3, 5][..], &[6, 1, 4, 2][..]] {
            let subset_positions: Vec<_> = subset.iter().map(|&i| positions[i]).collect();
            let subset_keys: Vec<_> = subset.iter().map(|&i| keys[i]).collect();
            let result = keyed_result(
                CpuProbeTracer::with_threads(2),
                &s,
                &subset_positions,
                &subset_keys,
                2,
            );
            for (index, &original) in subset.iter().enumerate() {
                assert_eq!(result[index], baseline[original], "subset key={}", keys[original]);
            }
        }

        // Equal keys at equal positions intentionally repeat a stream. In this non-uniform
        // scene, distinct keys at one position select different estimates.
        let duplicate_positions = [[0.25, -0.5, 0.75]; 4];
        let duplicate_keys = [11, 11, 12, 12];
        let repeated = keyed_result(
            CpuProbeTracer::with_threads(6),
            &s,
            &duplicate_positions,
            &duplicate_keys,
            1,
        );
        assert_eq!(repeated[0], repeated[1]);
        assert_eq!(repeated[2], repeated[3]);
        assert_ne!(repeated[0], repeated[2]);

        let boundary_positions = [[0.0, 0.0, 0.0], [0.0, 0.0, 0.0]];
        let boundaries = keyed_result(
            CpuProbeTracer::with_threads(2),
            &s,
            &boundary_positions,
            &[0, (1 << 24) - 1],
            9,
        );
        assert_eq!(boundaries.len(), 2);

        let mut trace_settings = settings();
        assert!(
            CpuProbeTracer::new()
                .trace_streams(
                    &s,
                    &boundary_positions[..1],
                    &[1 << 24],
                    &trace_settings,
                    &mut |_, _| true
                )
                .expect_err("2^24 key refused")
                .to_string()
                .contains("2^24")
        );
        assert!(
            CpuProbeTracer::new()
                .trace_streams(&s, &boundary_positions, &[0], &trace_settings, &mut |_, _| true)
                .expect_err("length mismatch refused")
                .to_string()
                .contains("streams for 2 probes")
        );

        // Cancellation happens only at a completed batch boundary. No sleeps or timing assumptions.
        trace_settings.probes_per_batch = 2;
        let mut progress = Vec::new();
        let canceled = CpuProbeTracer::with_threads(2).trace_streams(
            &s,
            &positions,
            &keys,
            &trace_settings,
            &mut |done, total| {
                progress.push((done, total));
                progress.len() < 2
            },
        );
        assert_eq!(progress, [(2, 7), (4, 7)]);
        assert!(
            progress
                .iter()
                .all(|&(done, total)| done <= total && total == positions.len())
        );
        assert!(progress.windows(2).all(|pair| pair[0].0 <= pair[1].0));
        assert!(
            canceled
                .expect_err("cancel after second completed batch")
                .to_string()
                .contains("probe trace canceled")
        );
        let tracer = CpuProbeTracer::with_threads(2);
        let fresh = keyed_result(tracer, &s, &positions, &keys, 2);
        assert_eq!(fresh, baseline);

        let mut progress_calls = 0;
        let mut empty_scene = PtScene::new(PtCamera::pinhole([0.0, 0.0, 0.0], [0.0; 3], 1.0, 1, 1));
        empty_scene.sky = PtSky::Table {
            width: 0,
            height: 0,
            texels: vec![],
            sun_direction: [0.0, 1.0, 0.0],
            scale: 1.0,
        };
        let empty = CpuProbeTracer::new()
            .trace_streams(&empty_scene, &[], &[], &trace_settings, &mut |_, _| {
                progress_calls += 1;
                false
            })
            .expect("empty trace skips scene validation");
        assert!(empty.is_empty());
        assert_eq!(progress_calls, 0);
    }

    /// A sky table is refused with a message (the GPU bakes it); a uniform sky is not.
    #[test]
    fn a_sky_table_is_refused() {
        let mut s = scene();
        s.sky = PtSky::Table {
            width: 1,
            height: 1,
            texels: vec![[1.0; 4]],
            sun_direction: [0.0, 1.0, 0.0],
            scale: 1.0,
        };
        let err = CpuProbeTracer::new()
            .trace(&s, &[[0.0; 3]], &settings(), &mut |_, _| true)
            .expect_err("table");
        assert!(err.to_string().contains("sky table"), "{err}");
    }

    /// One probe, few rays: enough to see a large ground, small enough to stay cheap.
    fn few_rays() -> ProbeTraceSettings {
        ProbeTraceSettings {
            rays_per_probe: 64,
            samples: 1,
            max_bounces: 1,
            ..ProbeTraceSettings::default()
        }
    }

    fn trace_at(scene: &PtScene, at: [f32; 3]) -> Result<ProbeTrace, PtError> {
        CpuProbeTracer::with_threads(1)
            .trace(scene, &[at], &few_rays(), &mut |_, _| true)
            .map(|mut v| v.remove(0))
    }

    /// A cloud field is refused, as a sky table is. The CPU tracer does not apply cloud
    /// extinction, so a success would be a clear-sky bake.
    #[test]
    fn a_cloud_field_is_refused() {
        let mut s = scene();
        s.sky = PtSky::Uniform([1.0; 3]);
        s.sun = Some(crate::PtSun {
            toward: [0.0, 1.0, 0.0],
            color: [1.0; 3],
            illuminance: 1000.0,
            angular_radius: 0.0,
        });
        let mut settings = regia_render::CloudSettings::coverage_layer(0.95);
        settings.density = 0.08;
        s.clouds = Some(crate::scene::PtCloudField {
            density: crate::scene::PtCloudDensity::Uniform(1.0),
            ..crate::scene::PtCloudField::from_settings(settings, 6.36e7)
        });
        match trace_at(&s, [0.0, 1.0, 0.0]) {
            Err(err) => assert!(err.to_string().contains("cloud"), "{err}"),
            Ok(_) => panic!("a cloud field was baked as a clear sky"),
        }
    }

    fn grid(width: u32, depth: u32, heights: Vec<f32>) -> crate::scene::PtHeightfield {
        crate::scene::PtHeightfield {
            width,
            depth,
            origin: [0.0, 0.0],
            spacing: [1_000.0, 1_000.0],
            heights,
            material: 0,
        }
    }

    /// A short heightfield and one whose dimensions wrap a `u32` product are refused with the
    /// packer's sample-count error. A 2×2 grid of four heights still occludes.
    #[test]
    fn a_bad_heightfield_is_refused_and_a_full_grid_occludes() {
        let mut open = scene();
        open.sky = PtSky::Uniform([1.0; 3]);
        let sky_only = trace_at(&open, [500.0, 1.0, 500.0]).expect("open");
        assert_eq!(sky_only.hits, 0);

        let mut ground = open.clone();
        ground.terrain = Some(grid(2, 2, vec![0.0; 4]));
        let over = trace_at(&ground, [500.0, 1.0, 500.0]).expect("ground");
        assert!(
            over.hits > over.rays / 4,
            "a 2×2 heightfield must occlude the lower hemisphere, hits {} of {}",
            over.hits,
            over.rays
        );
        assert!(over.hits < over.rays, "the sky above the ground stays open");

        let mut short = ground.clone();
        short.terrain = Some(grid(4, 4, vec![0.0; 15]));
        match trace_at(&short, [500.0, 1.0, 500.0]) {
            Err(err) => assert!(err.to_string().contains("wrong sample count"), "{err}"),
            Ok(_) => panic!("a 4×4 heightfield with 15 samples was baked with no ground"),
        }

        // 65536 × 65536 wraps to 0 as a u32 product. Empty on purpose: the refusal must happen
        // before any cell walk.
        let mut wrapped = ground.clone();
        wrapped.terrain = Some(grid(65_536, 65_536, Vec::new()));
        match trace_at(&wrapped, [500.0, 1.0, 500.0]) {
            Err(err) => assert!(err.to_string().contains("wrong sample count"), "{err}"),
            Ok(_) => panic!("a wrapping heightfield was baked with no ground"),
        }

        let mut spacing = ground.clone();
        if let Some(t) = &mut spacing.terrain {
            t.spacing = [0.0, 1.0];
        }
        match trace_at(&spacing, [500.0, 1.0, 500.0]) {
            Err(err) => assert!(err.to_string().contains("spacing must be positive"), "{err}"),
            Ok(_) => panic!("a heightfield with zero spacing was baked"),
        }

        let mut origin = ground.clone();
        if let Some(t) = &mut origin.terrain {
            t.origin = [f32::NAN, 0.0];
        }
        match trace_at(&origin, [500.0, 1.0, 500.0]) {
            Err(err) => assert!(err.to_string().contains("origin finite"), "{err}"),
            Ok(_) => panic!("a heightfield with a non-finite origin was baked"),
        }
    }

    /// A default two-round bake of one brick builds the CPU world once. Relocation and the
    /// main trace share it.
    #[test]
    fn a_two_mesh_bake_builds_the_world_once() {
        use crate::probe::{GiBakeSettings, bake_gi_volume, detail_brick};
        use regia_asset::baked_gi::VolumeLayout;

        let mut s = scene();
        s.sky = PtSky::Uniform([0.4, 0.5, 0.6]);
        let grey = s.add_material(PtMaterial::lambert([0.5; 3]));
        let floor = s.add_mesh(PtMesh {
            positions: vec![
                [-4.0, 0.0, -4.0],
                [4.0, 0.0, -4.0],
                [-4.0, 0.0, 4.0],
                [4.0, 0.0, 4.0],
            ],
            normals: vec![[0.0, 1.0, 0.0]; 4],
            indices: vec![0, 2, 1, 1, 2, 3],
            ..PtMesh::default()
        });
        s.add_instance(floor, grey, Mat4::IDENTITY);
        let cube = s.add_mesh(box_mesh([-0.5, 0.0, -0.5], [0.5, 1.0, 0.5], false));
        s.add_instance(cube, grey, Mat4::IDENTITY);
        let mut layout = VolumeLayout {
            terrain: None,
            bricks: vec![detail_brick([0.2, 1.2, 0.2], [0.8, 1.8, 0.8], 1.0)],
            moment_resolution: 1,
            max_distance: 4.0,
        };
        layout.assign_brick_offsets();
        let bake_settings = GiBakeSettings {
            trace: ProbeTraceSettings {
                rays_per_probe: 16,
                samples: 1,
                max_bounces: 2,
                probes_per_batch: 8,
                coherent_rays: false,
                compact_paths: false,
                ..ProbeTraceSettings::default()
            },
            relocation_iterations: 2,
            relocation_samples: 1,
            filter: false,
            wall_test: false,
            geometry_only_relocation: true,
            ..GiBakeSettings::default()
        };
        let before = cpu_probe_world_builds();
        let baked = bake_gi_volume(
            &CpuProbeTracer::with_threads(1),
            &s,
            &layout,
            &bake_settings,
            |_, _, _| true,
        )
        .expect("bake");
        let builds = cpu_probe_world_builds() - before;
        assert!(baked.report.probes >= 1, "{:?}", baked.report);
        assert_eq!(builds, 1, "world builds {builds}");
    }

    /// A geometry-only trace records the same first hit as a shaded one and leaves every SH
    /// component at zero (the GPU flag and `ProbeTraceSettings::geometry_only`).
    #[test]
    fn geometry_only_keeps_the_closest_hit_and_skips_shading() {
        let mut s = scene();
        s.sky = PtSky::None;
        s.sun = Some(crate::PtSun {
            toward: [0.0, 1.0, 0.0],
            color: [1.0; 3],
            illuminance: 1000.0,
            angular_radius: 0.0,
        });
        let mut mat = PtMaterial::lambert([0.6; 3]);
        mat.emissive = [3.0, 0.0, 0.0];
        let material = s.add_material(mat);
        // Faces up, so a probe above it records a front hit. The old triangle wound
        // downward and every hit was a back face, which this tracer does not shade.
        let mesh = s.add_mesh(PtMesh {
            positions: vec![
                [-8.0, 0.0, -8.0],
                [8.0, 0.0, -8.0],
                [-8.0, 0.0, 8.0],
                [8.0, 0.0, 8.0],
            ],
            normals: vec![[0.0, 1.0, 0.0]; 4],
            indices: vec![0, 2, 1, 1, 2, 3],
            ..PtMesh::default()
        });
        s.add_instance(mesh, material, Mat4::IDENTITY);
        let shaded_set = ProbeTraceSettings {
            rays_per_probe: 32,
            samples: 1,
            max_bounces: 2,
            geometry_only: false,
            ..ProbeTraceSettings::default()
        };
        let geo = ProbeTraceSettings {
            geometry_only: true,
            ..shaded_set
        };
        let at = [0.0, 0.5, 0.0];
        let tracer = CpuProbeTracer::with_threads(1);
        let g = tracer
            .trace(&s, &[at], &geo, &mut |_, _| true)
            .expect("geometry")
            .remove(0);
        let shaded = tracer
            .trace(&s, &[at], &shaded_set, &mut |_, _| true)
            .expect("shaded")
            .remove(0);
        assert!(
            shaded.sh[EMISSIVE].iter().flatten().any(|v| *v != 0.0),
            "shaded emissive"
        );
        assert!(shaded.sh[SUN].iter().flatten().any(|v| *v != 0.0), "shaded sun");
        assert_eq!(
            (g.rays, g.hits, g.back_faces, g.closest_front, g.closest_back),
            (
                shaded.rays,
                shaded.hits,
                shaded.back_faces,
                shaded.closest_front,
                shaded.closest_back
            )
        );
        assert_eq!(g.moments, shaded.moments);
        assert!(
            g.sh.iter().flatten().flatten().all(|v| *v == 0.0),
            "geometry-only still shaded: {:?}",
            g.sh
        );
    }
}
