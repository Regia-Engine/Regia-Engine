// SPDX-License-Identifier: MPL-2.0
//! Probe baking: the path tracer's kernels run from probe positions instead of a camera and
//! fold the radiance of every ray into per-probe L1 SH and octahedral distance moments, per
//! separable light component. [`bake_gi_volume`] adds relocation, validity and the 3×3×3
//! visibility-weighted filter and encodes a [`BakedGiVolume`] (`regia bake-gi`;
//! `docs/research/strategic-review-20261012.md` § 3, phase 1).
//!
//! What is baked: indirect diffuse light arriving at the probe from the sun (after at least one
//! bounce), the sky, sky bodies (planet-shine fill) and emissive surfaces, each component
//! separately so the host may rescale it ([`BakeLights`]). What stays live: the sun seen
//! directly, point/spot/sphere lights, stars.
//!
//! The traces run on the GPU ([`PathTracer`]) or on the CPU ([`crate::cpu_probe`]), both
//! behind [`ProbeTracer`].
//!
//! Algorithms (see `docs/provenance.md`): irradiance volumes (Greger et al. 1998; Tatarchuk
//! 2005); L1 SH projection (Ramamoorthi & Hanrahan 2001; Sloan 2008); randomly rotated
//! spherical Fibonacci ray sets (Keinert et al. 2015; Shoemake 1992 for the rotation);
//! octahedral distance moments with a cosine-power lobe, Chebyshev visibility, back-face probe
//! classification and relocation (Majercik et al. 2019 and 2021).
//!
//! Leak control (ADR-366, [`crate::leak`]): after the trace, every segment between two grid
//! neighbors is tested against the scene's triangles and terrain on the CPU. Where a wall crosses
//! a segment toward a +X, +Y or +Z neighbor, its position goes into the probe flags
//! ([`ProbeFlags::WALLS`]) for the runtime lookup, and the filter never averages across a blocked
//! segment.
//!
//! Quality controls ([`crate::bake_quality`], ADR-1096): the bounce count and the Russian
//! roulette start, per-contribution luminance clamps (`clamp_direct`, `clamp_indirect`), sample
//! groups with a noise estimate, and median-of-means firefly suppression.

use std::time::Instant;

use regia_asset::baked_gi::{
    BakeInfo, BakeLights, BakedEmitter, BakedGiVolume, DetailBrick, GI_COMPONENTS, ProbeData, ProbeFlags,
    ProbeMeta, SH_Y00, ShRgb, TerrainLayer, VolumeLayout, chebyshev, sample_moments,
};
use regia_core::glam::Vec3;

use crate::PtError;
use crate::bake_quality::{
    BounceBudget, GroupSums, MAX_SAMPLE_GROUPS, SH_ROWS, ShSums, bounce_budget, combine_groups, group_range,
    l00_luminance, median,
};
use crate::checkpoint::{
    BakeCheckpointKind, BakeCheckpointPlan, BakeCheckpointStats, Checkpointer, DEFAULT_GI_BATCH_PROBES,
    GiCheckpointState, InputHasher,
};
use crate::leak::{LeakGeometry, NeighborWalls};
use crate::pack::{FLAG_JITTER, FLAG_PROBE, FLAG_PROBE_GEOMETRY};
use crate::placement::ProbePlacement;
use crate::scene::{PtHeightfield, PtScene, PtSky};
use crate::tracer::{PathTracer, ProbeNeed, QUEUE_ARGS_OFFSET};

/// vec4s per probe before the moment texels (`PROBE_MOMENTS` in `pt.wgsl`): the SH sums, then
/// stats, the closest back face, the closest front face and the clamp sums.
const PROBE_HEADER_VEC4S: usize = 16;
/// Ray stats: finite rays, back-face hits, hits, rejected rays (`PROBE_STATS`).
const PROBE_STATS: usize = SH_ROWS;
/// Closest back-face hit: direction, distance (`PROBE_BACK`).
const PROBE_BACK: usize = SH_ROWS + 1;
/// Closest front-face hit (`PROBE_FRONT`).
const PROBE_FRONT: usize = SH_ROWS + 2;
/// Clamp sums: luminance removed, contributions clamped (`PROBE_CLAMP`).
const PROBE_CLAMP: usize = SH_ROWS + 3;
const WORKGROUP: u32 = 64;
/// Probe records carry their global index as an f32 (exact below 2^24).
const MAX_PROBES: usize = 1 << 24;
/// Probes per brick axis [`detail_brick`] will produce (an oversize brick is capped here and
/// rejected by [`check_probe_budget`]).
const MAX_BRICK_AXIS: u32 = 1 << 20;
/// Rays one probe accumulates: the GPU counts them in an f32, exact up to 2^24.
const MAX_PROBE_RAYS: u64 = 1 << 24;

/// Most samples per probe for `rays` rays per sample, so `samples × rays` stays within
/// `MAX_PROBE_RAYS`.
pub(crate) fn max_samples(rays: u32) -> u32 {
    (MAX_PROBE_RAYS / u64::from(rays.max(1))).max(1) as u32
}

/// Checks that one batch's dispatch sizes (`batch × rays` threads, `batch × (1 + texels)`
/// accumulators) fit the `u32` thread count the dispatch is computed in.
fn check_dispatch(batch: usize, rays: u32, texels: usize) -> Result<(), PtError> {
    let limit = u64::from(u32::MAX);
    let trace = batch as u64 * u64::from(rays);
    let gather = batch as u64 * (1 + texels as u64);
    if trace > limit || gather > limit {
        return Err(PtError::Scene(format!(
            "{batch} probes per batch with {rays} rays overflow one dispatch; lower probes_per_batch"
        )));
    }
    Ok(())
}

/// Checks that `terrain` plus `bricks` hold fewer than one bake can trace.
///
/// # Errors
///
/// [`PtError::Scene`] when the total reaches the 2^24 probe limit.
pub fn check_probe_budget(terrain: Option<&TerrainLayer>, bricks: &[DetailBrick]) -> Result<(), PtError> {
    let mut total: u128 = terrain.map_or(0, |t| {
        u128::from(t.counts[0]) * u128::from(t.counts[1]) * t.levels.len() as u128
    });
    for b in bricks {
        total += b.counts.iter().map(|&c| u128::from(c)).product::<u128>();
    }
    if total >= MAX_PROBES as u128 {
        return Err(PtError::Scene(format!(
            "the layout holds {total} probes; one bake traces at most {MAX_PROBES}"
        )));
    }
    Ok(())
}

fn too_many_probes(n: usize) -> PtError {
    PtError::Scene(format!("{n} probes; one bake traces at most {MAX_PROBES}"))
}

/// Settings of one probe trace ([`PathTracer::trace_probes`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProbeTraceSettings {
    /// Rays per probe per sample (one rotated spherical Fibonacci set). Both tracers trace
    /// `clamp(rays_per_probe, 1, 4096)`. A larger value still passes [`crate::bake_desc::validate_bake_settings`]
    /// and is what the bake summary prints.
    pub rays_per_probe: u32,
    /// Samples (rotated ray sets) per probe. The GPU tracer caps this so
    /// `samples × rays_per_probe` is at most 2^24 (its ray counter is an f32). The CPU tracer
    /// ([`crate::cpu_probe`]) applies the same cap.
    pub samples: u32,
    /// Maximum path vertices after the probe (1 = light seen directly and its first bounce
    /// off the first surface hit). Values outside `1..=64` are clamped to that range by both
    /// the GPU and the CPU tracer (the struct is not changed and no error is returned).
    pub max_bounces: u32,
    /// Seed: the same scene, positions, settings and device give the same result bit for bit,
    /// however the probes are split into batches.
    pub seed: u32,
    /// Path vertex from which Russian roulette may end a path.
    pub rr_start: u32,
    /// Probes traced per GPU batch (bounds memory: `probes × rays × 192` bytes of paths).
    pub probes_per_batch: u32,
    /// Samples recorded per GPU submission.
    pub samples_per_submit: u32,
    /// Octahedral moment texels per edge.
    pub moment_resolution: u32,
    /// Exponent of the cosine lobe that weights a ray into a moment texel.
    pub moment_exponent: f32,
    /// Distances are clamped to this for the moments (and escaped rays count as it), metres.
    pub max_distance: f32,
    /// Luminance cap, cd/m², on each light contribution a probe ray sees without scattering (the
    /// sky, a sky body, an emissive surface at the first hit); 0 = off. A capped contribution
    /// keeps its color.
    pub clamp_direct: f32,
    /// Luminance cap, cd/m², on each contribution after at least one scattering event (a
    /// next-event sample, an emitter or the sky reached by a bounce); 0 = off. The usual
    /// firefly clamp: biased, it trades energy for noise.
    pub clamp_indirect: f32,
    /// Sample groups summed and read back separately (1 = one sum; at most `samples` and
    /// [`MAX_SAMPLE_GROUPS`]). Two or more give [`ProbeTrace::noise`]; three or more allow
    /// firefly suppression.
    pub sample_groups: u32,
    /// Firefly suppression: a sample group whose mean luminance exceeds this many times the
    /// median group's is scaled down to it (`crate::bake_quality::combine_groups`); 0 = off.
    /// Needs a value above 1 and three groups or more.
    pub firefly_threshold: f32,
    /// Trace each probe's rays in the coherent order of [`crate::probe_rays`] (ADR-862): the
    /// same rays and paths as index order, faster on a GPU; only the order of the sums (so the
    /// last bits) changes.
    pub coherent_rays: bool,
    /// Record only each ray's first hit (distance, face, moments) and end it: the SH stay 0.
    /// What relocation needs, at about the cost of one closest-hit ray per ray (ADR-862).
    pub geometry_only: bool,
    /// Compact the live paths after the first bounce (ADR-862): later bounces dispatch only
    /// the paths still going, from a queue, so a GPU wave is not mostly ended paths. The same
    /// paths and the same results bit for bit.
    pub compact_paths: bool,
}

impl Default for ProbeTraceSettings {
    fn default() -> Self {
        Self {
            rays_per_probe: 256,
            samples: 16,
            max_bounces: 8,
            seed: 1,
            rr_start: 3,
            probes_per_batch: 4096,
            samples_per_submit: 4,
            moment_resolution: 4,
            moment_exponent: 50.0,
            max_distance: 32.0,
            clamp_direct: 0.0,
            clamp_indirect: 0.0,
            sample_groups: 1,
            firefly_threshold: 0.0,
            coherent_rays: true,
            geometry_only: false,
            compact_paths: true,
        }
    }
}

/// One probe's raw trace result.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProbeTrace {
    /// Radiance SH per component ([`regia_asset::baked_gi::GiComponent`] order).
    pub sh: [ShRgb; GI_COMPONENTS],
    /// Rays that returned a finite value.
    pub rays: u32,
    /// Rays whose first hit was a back face.
    pub back_faces: u32,
    /// Rays that hit geometry.
    pub hits: u32,
    /// Rays dropped for a non-finite value (a tracer bug when non-zero).
    pub rejected: u32,
    /// Direction and distance of the closest back-face hit.
    pub closest_back: Option<([f32; 3], f32)>,
    /// Direction and distance of the closest front-face hit.
    pub closest_front: Option<([f32; 3], f32)>,
    /// Octahedral (mean, mean²) distance per texel, metres.
    pub moments: Vec<[f32; 2]>,
    /// Light contributions the luminance clamps capped.
    pub clamped: u32,
    /// Fraction of the probe's luminance (all components, before clamping) the clamps removed.
    pub clamp_loss: f32,
    /// Sample groups firefly suppression scaled down.
    pub suppressed_groups: u32,
    /// Fraction of the probe's (clamped) luminance firefly suppression removed.
    pub firefly_loss: f32,
    /// Relative standard error of the probe's mean luminance, from the spread of its sample
    /// groups: `None` with one group or a black probe.
    pub noise: Option<f32>,
}

impl ProbeTrace {
    /// Fraction of the rays that hit a back face.
    #[must_use]
    pub fn back_face_fraction(&self) -> f32 {
        if self.rays == 0 {
            0.0
        } else {
            self.back_faces as f32 / self.rays as f32
        }
    }
}

impl PathTracer {
    /// Traces `positions` as probes and returns one [`ProbeTrace`] each. `progress(done,
    /// total)` runs after every batch; returning `false` cancels (an error).
    ///
    /// # Errors
    ///
    /// The scene is malformed, a buffer exceeds the device limits, a GPU error, or cancellation.
    pub fn trace_probes(
        &self,
        scene: &PtScene,
        positions: &[[f32; 3]],
        settings: &ProbeTraceSettings,
        progress: impl FnMut(usize, usize) -> bool,
    ) -> Result<Vec<ProbeTrace>, PtError> {
        if positions.len() >= MAX_PROBES {
            return Err(too_many_probes(positions.len()));
        }
        let streams: Vec<u32> = (0..positions.len() as u32).collect();
        self.trace_probe_streams(scene, positions, &streams, settings, progress)
    }

    /// [`Self::trace_probes`] with an explicit random stream per probe: `streams[k]` keys the
    /// rays of `positions[k]` (below 2^24; [`Self::trace_probes`] uses `k`). A probe traced
    /// with the same stream, position and settings gives the same result in any call, whatever
    /// else is traced with it, so a bake can retrace a subset of its probes and get exactly
    /// what a full trace gives them (ADR-862).
    ///
    /// # Errors
    ///
    /// See [`Self::trace_probes`]; also a stream count that does not match the positions, or
    /// a stream of 2^24 or more.
    pub fn trace_probe_streams(
        &self,
        scene: &PtScene,
        positions: &[[f32; 3]],
        streams: &[u32],
        settings: &ProbeTraceSettings,
        mut progress: impl FnMut(usize, usize) -> bool,
    ) -> Result<Vec<ProbeTrace>, PtError> {
        let _span = tracing::info_span!("pt_trace_probes", probes = positions.len()).entered();
        if positions.is_empty() {
            return Ok(Vec::new());
        }
        if positions.len() >= MAX_PROBES {
            return Err(too_many_probes(positions.len()));
        }
        if streams.len() != positions.len() {
            return Err(PtError::Scene(format!(
                "{} probe streams for {} probes",
                streams.len(),
                positions.len()
            )));
        }
        if streams.iter().any(|&s| s as usize >= MAX_PROBES) {
            return Err(PtError::Scene(format!("a probe stream is {MAX_PROBES} or more")));
        }
        if positions.iter().flatten().any(|v| !v.is_finite()) {
            return Err(PtError::Scene("a probe position is not finite".into()));
        }
        let rays = settings.rays_per_probe.clamp(1, 4096);
        // The per-probe ray count is an f32 incremented by one on the GPU: past 2^24 it sticks
        // and the SH normalization (4π/N) is wrong, so `samples × rays` is capped there.
        let samples = settings.samples.max(1).min(max_samples(rays));
        let sample_groups = settings.sample_groups.clamp(1, MAX_SAMPLE_GROUPS).min(samples);
        let cap = |v: f32| if v.is_finite() && v > 0.0 { v } else { 0.0 };
        // Geometry-only rays end at their first hit: one bounce dispatch is all there is.
        let bounces = if settings.geometry_only {
            1
        } else {
            settings.max_bounces.clamp(1, 64)
        };
        // Compacted bounces need a second one to compact into.
        let compact = settings.compact_paths && bounces > 1;
        let per_submit = settings.samples_per_submit.clamp(1, 256);
        let res = settings.moment_resolution.clamp(1, 32);
        let texels = (res * res) as usize;
        let stride = PROBE_HEADER_VEC4S + texels;
        let far = if settings.max_distance > 0.0 {
            settings.max_distance
        } else {
            32.0
        };
        let batch = (settings.probes_per_batch.max(1) as usize).min(positions.len());
        check_dispatch(batch, rays, texels)?;

        // One pack and one upload for a scene (ADR-5154). Relocation rounds and the main trace
        // share the buffers; a larger later batch uploads again.
        let slots = batch as u64 * u64::from(rays);
        let accum_bytes = (batch * stride * 16) as u64;
        let lease = self.lease_probe_scene(
            scene,
            ProbeNeed {
                batch,
                rays,
                paths: slots,
                accum_bytes,
                per_submit,
                bounces,
                coherent: settings.coherent_rays,
            },
        )?;
        // Bakes run the base kernels: a material graph bakes with its standard factors (ADR-2725).
        if lease.graph_materials() > 0 {
            tracing::warn!(
                target: "regia_pt",
                materials = lease.graph_materials(),
                "probe bake: material graphs are baked with their standard factors"
            );
        }
        let env_offset = lease.env_base;
        let order_offset = lease.order_offset;
        let coherent = lease.coherent;
        let mut probe_globals = lease.scene_globals();
        probe_globals.dims = [rays, batch as u32, bounces, settings.seed];
        probe_globals.counts[2] = settings.rr_start.max(1);
        probe_globals.counts[3] = (probe_globals.counts[3] & !FLAG_JITTER) | FLAG_PROBE;
        if settings.geometry_only {
            probe_globals.counts[3] |= FLAG_PROBE_GEOMETRY;
        }
        probe_globals.probe = [rays, batch as u32, env_offset as u32, res];
        probe_globals.probe_f = [
            far,
            settings.moment_exponent.max(1.0),
            cap(settings.clamp_direct),
            cap(settings.clamp_indirect),
        ];
        // The coherent order table's offset (ADR-862; 0 = index order).
        probe_globals.misc[2] = if coherent { order_offset as f32 } else { 0.0 };
        let up = lease.uploaded();
        let d = &self.device.device;
        let q = &self.device.queue;
        let staging = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pt probe readback"),
            size: accum_bytes,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let stride_bytes = self.param_stride as usize;
        // One staging buffer for every submission of this call (the used prefix is rewritten).
        let mut pass_params = vec![0u8; per_submit as usize * bounces as usize * stride_bytes];
        let mut out = Vec::with_capacity(positions.len());
        let started = Instant::now();
        for (b, chunk) in positions.chunks(batch).enumerate() {
            let n = chunk.len();
            let mut globals = probe_globals;
            globals.dims[1] = n as u32;
            globals.probe[1] = n as u32;
            q.write_buffer(&up.globals, 0, bytemuck::bytes_of(&globals));
            let records: Vec<f32> = chunk
                .iter()
                .zip(&streams[b * batch..b * batch + n])
                .flat_map(|(p, &s)| [p[0], p[1], p[2], s as f32])
                .collect();
            q.write_buffer(&up.env, (env_offset * 4) as u64, bytemuck::cast_slice(&records));
            let dispatch = |count: u32| {
                let groups = count.div_ceil(WORKGROUP).max(1);
                let gx = groups.clamp(1, 65_535);
                (gx, groups.div_ceil(gx))
            };
            let (tx, ty) = dispatch(n as u32 * rays);
            let (ax, ay) = dispatch((n * (1 + texels)) as u32);
            let used = (n * stride * 16) as u64;
            // One accumulation (cleared, traced, read back) per sample group.
            let mut parts: Vec<Vec<[f32; 4]>> = Vec::with_capacity(sample_groups as usize);
            for group in 0..sample_groups {
                let (start, end) = group_range(samples, sample_groups, group);
                let mut first = true;
                let mut done = start;
                while done < end {
                    let count = (end - done).min(per_submit);
                    let used_bytes = count as usize * bounces as usize * stride_bytes;
                    for k in 0..count {
                        for bounce in 0..bounces {
                            let at = (k * bounces + bounce) as usize * stride_bytes;
                            pass_params[at..at + 4].copy_from_slice(&(done + k).to_le_bytes());
                            pass_params[at + 4..at + 8].copy_from_slice(&bounce.to_le_bytes());
                            // `Pass::queue`: the first bounce runs every slot and fills a queue,
                            // the later ones run the queue (ADR-862).
                            let queue = match (compact, bounce) {
                                (false, _) => 0u32,
                                (true, 0) => 1,
                                (true, _) => 2,
                            };
                            pass_params[at + 8..at + 12].copy_from_slice(&queue.to_le_bytes());
                        }
                    }
                    q.write_buffer(&up.params, 0, &pass_params[..used_bytes]);
                    let scope = d.push_error_scope(wgpu::ErrorFilter::Validation);
                    let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                        label: Some("regia_pt probes"),
                    });
                    if first {
                        encoder.clear_buffer(&up.accum, 0, None);
                        first = false;
                    }
                    let pass_desc = wgpu::ComputePassDescriptor {
                        label: Some("regia_pt probe samples"),
                        timestamp_writes: None,
                    };
                    for k in 0..count {
                        let offset = |bounce: u32| ((k * bounces + bounce) as usize * stride_bytes) as u32;
                        let mut pass = encoder.begin_compute_pass(&pass_desc);
                        pass.set_bind_group(0, &up.bind_group, &[offset(0)]);
                        pass.set_pipeline(&self.generate);
                        pass.dispatch_workgroups(tx, ty, 1);
                        for bounce in 0..bounces {
                            pass.set_bind_group(0, &up.bind_group, &[offset(bounce)]);
                            if compact {
                                // Size this bounce's dispatch from the queue the last one filled,
                                // and empty the queue this one fills.
                                pass.set_pipeline(&self.queue_dispatch);
                                pass.dispatch_workgroups(1, 1, 1);
                            }
                            if compact && bounce > 0 {
                                // The arguments go through a copy (outside a pass) into a buffer
                                // the bind group does not hold.
                                drop(pass);
                                encoder.copy_buffer_to_buffer(
                                    &up.queue_count,
                                    QUEUE_ARGS_OFFSET,
                                    &up.indirect,
                                    0,
                                    12u64,
                                );
                                pass = encoder.begin_compute_pass(&pass_desc);
                                pass.set_bind_group(0, &up.bind_group, &[offset(bounce)]);
                                pass.set_pipeline(&self.intersect);
                                pass.dispatch_workgroups_indirect(&up.indirect, 0);
                                pass.set_pipeline(&self.shade);
                                pass.dispatch_workgroups_indirect(&up.indirect, 0);
                            } else {
                                pass.set_pipeline(&self.intersect);
                                pass.dispatch_workgroups(tx, ty, 1);
                                pass.set_pipeline(&self.shade);
                                pass.dispatch_workgroups(tx, ty, 1);
                            }
                        }
                        pass.set_bind_group(0, &up.bind_group, &[offset(0)]);
                        pass.set_pipeline(&self.probe_accumulate);
                        pass.dispatch_workgroups(ax, ay, 1);
                    }
                    q.submit([encoder.finish()]);
                    if let Some(err) = regia_core::tasks::block_on(scope.pop()) {
                        return Err(PtError::Gpu(err.to_string()));
                    }
                    // No wait here (ADR-862): the next submission's `write_buffer` is ordered
                    // after this one, so the GPU runs the group's submissions back to back and
                    // the group's readback below waits once.
                    done += count;
                }
                parts.push(read_back(self, &up.accum, &staging, used)?);
            }
            for p in 0..n {
                let slices: Vec<&[[f32; 4]]> = parts
                    .iter()
                    .map(|raw| &raw[p * stride..(p + 1) * stride])
                    .collect();
                out.push(decode_groups(&slices, texels, far, settings.firefly_threshold));
            }
            if !progress(out.len(), positions.len()) {
                return Err(PtError::Scene("probe trace canceled".into()));
            }
        }
        tracing::info!(
            target: "regia_pt",
            probes = positions.len(),
            rays,
            samples,
            seconds = started.elapsed().as_secs_f64(),
            "probe trace done"
        );
        Ok(out)
    }
}

/// Something that traces probes: the GPU path tracer ([`PathTracer`]) or the CPU tracer
/// ([`crate::cpu_probe::CpuProbeTracer`]). [`bake_gi_volume`] runs on either.
pub trait ProbeTracer {
    /// Traces `positions` as probes and returns one [`ProbeTrace`] each, in order.
    /// `progress(done, total)` runs after every batch; returning `false` cancels (an error).
    ///
    /// # Errors
    ///
    /// The scene is malformed or not supported by this tracer, a device error, or cancellation.
    fn trace(
        &self,
        scene: &PtScene,
        positions: &[[f32; 3]],
        settings: &ProbeTraceSettings,
        progress: &mut dyn FnMut(usize, usize) -> bool,
    ) -> Result<Vec<ProbeTrace>, PtError> {
        let streams: Vec<u32> = (0..positions.len() as u32).collect();
        self.trace_streams(scene, positions, &streams, settings, progress)
    }

    /// [`Self::trace`] with an explicit random stream per probe: `streams[k]` keys the rays of
    /// `positions[k]` (below 2^24; [`Self::trace`] uses `k`). A probe traced with the same
    /// stream, position and settings gives the same result in any call, whatever else is
    /// traced with it, so a bake retraces a subset of its probes and gets what a trace of every
    /// probe gives them (ADR-862).
    ///
    /// # Errors
    ///
    /// See [`Self::trace`]; also a stream count that does not match the positions, or a stream
    /// of 2^24 or more.
    fn trace_streams(
        &self,
        scene: &PtScene,
        positions: &[[f32; 3]],
        streams: &[u32],
        settings: &ProbeTraceSettings,
        progress: &mut dyn FnMut(usize, usize) -> bool,
    ) -> Result<Vec<ProbeTrace>, PtError>;

    /// The backend's name for reports: `gpu` or `cpu`.
    fn backend(&self) -> &'static str;
}

impl ProbeTracer for PathTracer {
    fn trace_streams(
        &self,
        scene: &PtScene,
        positions: &[[f32; 3]],
        streams: &[u32],
        settings: &ProbeTraceSettings,
        progress: &mut dyn FnMut(usize, usize) -> bool,
    ) -> Result<Vec<ProbeTrace>, PtError> {
        self.trace_probe_streams(scene, positions, streams, settings, progress)
    }

    fn backend(&self) -> &'static str {
        "gpu"
    }
}

fn read_back(
    tracer: &PathTracer,
    src: &wgpu::Buffer,
    staging: &wgpu::Buffer,
    size: u64,
) -> Result<Vec<[f32; 4]>, PtError> {
    let d = &tracer.device.device;
    let q = &tracer.device.queue;
    let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("pt probe readback"),
    });
    encoder.copy_buffer_to_buffer(src, 0, staging, 0, size);
    q.submit([encoder.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    staging.slice(..size).map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    d.poll(wgpu::PollType::wait_indefinitely())
        .map_err(|e| PtError::Gpu(e.to_string()))?;
    rx.recv()
        .map_err(|_| PtError::Gpu("map callback dropped".into()))?
        .map_err(|e| PtError::Gpu(e.to_string()))?;
    let mapped = staging
        .slice(..size)
        .get_mapped_range()
        .map_err(|e| PtError::Gpu(e.to_string()))?;
    let raw: Vec<[f32; 4]> = bytemuck::cast_slice(&mapped).to_vec();
    drop(mapped);
    staging.unmap();
    Ok(raw)
}

/// The SH sums of a probe record (its first [`SH_ROWS`] vec4s).
fn sh_sums(raw: &[[f32; 4]]) -> ShSums {
    let mut sh = [[0.0f32; 4]; SH_ROWS];
    sh.copy_from_slice(&raw[..SH_ROWS]);
    sh
}

/// SH coefficients from sums over `rays` uniformly distributed directions: ∫ L·Y dω ≈
/// 4π/N · Σ L·Y.
fn sh_from_sums(sums: &ShSums, rays: f32) -> [ShRgb; GI_COMPONENTS] {
    let inv = if rays > 0.0 {
        4.0 * std::f32::consts::PI / rays
    } else {
        0.0
    };
    let mut sh = [[[0.0f32; 4]; 3]; GI_COMPONENTS];
    for (c, comp) in sh.iter_mut().enumerate() {
        for (ch, coeffs) in comp.iter_mut().enumerate() {
            let v = sums[c * 3 + ch];
            *coeffs = [v[0] * inv, v[1] * inv, v[2] * inv, v[3] * inv];
        }
    }
    sh
}

/// One probe's records of every sample group folded into one [`ProbeTrace`]: stats, clamp sums
/// and moments add up, the closest hits take the nearest, and the SH sums go through
/// [`combine_groups`] (firefly suppression and the noise estimate). One group decodes exactly as
/// a trace without groups.
fn decode_groups(groups: &[&[[f32; 4]]], texels: usize, far: f32, firefly_threshold: f32) -> ProbeTrace {
    let Some(first) = groups.first() else {
        return decode_probe(&vec![[0.0; 4]; PROBE_HEADER_VEC4S + texels], texels, far);
    };
    let mut raw = first.to_vec();
    for g in &groups[1..] {
        for (i, (a, v)) in raw.iter_mut().zip(g.iter()).enumerate() {
            if i == PROBE_BACK || i == PROBE_FRONT {
                if v[3] > 0.0 && (a[3] == 0.0 || v[3] < a[3]) {
                    *a = *v;
                }
            } else {
                for (x, y) in a.iter_mut().zip(v) {
                    *x += y;
                }
            }
        }
    }
    let mut t = decode_probe(&raw, texels, far);
    let sums: Vec<GroupSums> = groups
        .iter()
        .map(|g| GroupSums {
            sh: sh_sums(g),
            rays: g[PROBE_STATS][0].max(0.0),
        })
        .collect();
    let c = combine_groups(&sums, firefly_threshold);
    if c.suppressed > 0 {
        t.sh = sh_from_sums(&c.sh, c.rays);
    }
    t.suppressed_groups = c.suppressed;
    t.firefly_loss = c.removed;
    t.noise = c.noise;
    t
}

fn decode_probe(raw: &[[f32; 4]], texels: usize, far: f32) -> ProbeTrace {
    let stats = raw[PROBE_STATS];
    let rays = stats[0].max(0.0);
    let sums = sh_sums(raw);
    let sh = sh_from_sums(&sums, rays);
    // What the clamps removed against what they kept, in summed ray luminance (the L00 sums
    // carry it times Y00).
    let removed = raw[PROBE_CLAMP][0].max(0.0);
    let kept = (l00_luminance(&sums) / SH_Y00).max(0.0);
    let clamp_loss = if removed + kept > 0.0 {
        removed / (removed + kept)
    } else {
        0.0
    };
    let closest = |v: [f32; 4]| (v[3] > 0.0).then_some(([v[0], v[1], v[2]], v[3]));
    let moments = raw[PROBE_HEADER_VEC4S..PROBE_HEADER_VEC4S + texels]
        .iter()
        .map(|m| {
            if m[2] > 0.0 {
                [m[0] / m[2], m[1] / m[2]]
            } else {
                [far, far * far]
            }
        })
        .collect();
    ProbeTrace {
        sh,
        rays: rays as u32,
        back_faces: stats[1].max(0.0) as u32,
        hits: stats[2].max(0.0) as u32,
        rejected: stats[3].max(0.0) as u32,
        closest_back: closest(raw[PROBE_BACK]),
        closest_front: closest(raw[PROBE_FRONT]),
        moments,
        clamped: raw[PROBE_CLAMP][1].max(0.0) as u32,
        clamp_loss,
        suppressed_groups: 0,
        firefly_loss: 0.0,
        noise: None,
    }
}

/// Settings of a full volume bake ([`bake_gi_volume`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GiBakeSettings {
    /// The main trace.
    pub trace: ProbeTraceSettings,
    /// Relocation rounds before the main trace (0 = keep the nominal positions).
    pub relocation_iterations: u32,
    /// Samples per relocation round (distances only need a few).
    pub relocation_samples: u32,
    /// A probe whose rays hit back faces more often than this sits inside geometry: relocation
    /// moves it out, and if it stays inside it is invalid (Majercik et al. 2021 use 0.25).
    pub back_face_invalid_fraction: f32,
    /// Relocation pushes a probe away from a front face closer than this fraction of its
    /// smallest cell edge.
    pub min_front_face_fraction: f32,
    /// Run the 3×3×3 visibility-weighted SH filter.
    pub filter: bool,
    /// Test the segment between every pair of grid neighbors against the scene's geometry on
    /// the CPU ([`crate::leak`], ADR-366): a wall toward a face neighbor becomes a wall code in
    /// the probe flags, and the filter skips every blocked neighbor. `false` bakes as before (no
    /// codes).
    pub wall_test: bool,
    /// Relocation rounds trace geometry only ([`ProbeTraceSettings::geometry_only`]): the
    /// same distances and faces, without shading (ADR-862).
    pub geometry_only_relocation: bool,
    /// A probe whose last relocation trace, at its final position and with at least
    /// [`SKIP_INSIDE_MIN_RAYS`] rays, saw back faces on this fraction of its rays or more is
    /// marked inside geometry without the main trace (ADR-862). Effective only above
    /// [`Self::back_face_invalid_fraction`]; any value above 1 traces every probe.
    pub skip_inside_fraction: f32,
}

/// Fewest rays a relocation trace needs before [`GiBakeSettings::skip_inside_fraction`] may
/// skip its probe: with 64 rays a fraction of 0.5 is four standard errors above 0.25.
pub const SKIP_INSIDE_MIN_RAYS: u32 = 64;

/// Whether a bake marks a probe inside geometry without its main trace (ADR-862): `last` is
/// the (back-face fraction, rays) of the probe's latest relocation trace at its final position
/// (`None` when it moved after it or relocation is off). Needs [`SKIP_INSIDE_MIN_RAYS`] rays
/// and a fraction at or above [`GiBakeSettings::skip_inside_fraction`] and above
/// [`GiBakeSettings::back_face_invalid_fraction`].
#[must_use]
pub fn skips_main_trace(last: Option<(f32, u32)>, settings: &GiBakeSettings) -> bool {
    last.is_some_and(|(fraction, rays)| {
        rays >= SKIP_INSIDE_MIN_RAYS
            && fraction > settings.back_face_invalid_fraction
            && fraction >= settings.skip_inside_fraction
    })
}

impl Default for GiBakeSettings {
    fn default() -> Self {
        Self {
            trace: ProbeTraceSettings::default(),
            relocation_iterations: 2,
            relocation_samples: 2,
            back_face_invalid_fraction: 0.25,
            min_front_face_fraction: 0.1,
            filter: true,
            wall_test: true,
            geometry_only_relocation: true,
            skip_inside_fraction: 0.5,
        }
    }
}

impl GiBakeSettings {
    /// These settings with every ADR-862 shortcut off (index-order rays, no path compaction,
    /// shaded relocation, every probe traced): the reference a speed-up is measured and
    /// checked against.
    #[must_use]
    pub fn without_shortcuts(self) -> Self {
        Self {
            trace: ProbeTraceSettings {
                coherent_rays: false,
                compact_paths: false,
                ..self.trace
            },
            geometry_only_relocation: false,
            skip_inside_fraction: f32::INFINITY,
            ..self
        }
    }
}

/// What a bake did.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct GiBakeReport {
    /// Probes in the volume.
    pub probes: usize,
    /// Valid probes.
    pub valid: usize,
    /// Probes below the terrain.
    pub under_terrain: usize,
    /// Probes inside geometry after relocation.
    pub inside_geometry: usize,
    /// Probes moved by relocation.
    pub relocated: usize,
    /// Rays dropped for non-finite values (should be 0).
    pub rejected_rays: u64,
    /// Rays traced in the main pass.
    pub rays_traced: u64,
    /// Probes whose back-face fraction lies within [`BORDERLINE_BACK_FACE_BAND`] of the
    /// invalid threshold: a few rays more or less (another GPU, another driver) flips their
    /// validity, so a bake on another device may differ there (ADR-357).
    pub borderline_back_face: usize,
    /// Cells of INTERIOR bricks whose eight corner probes are all invalid: the runtime reads
    /// them black at full coverage ([`dead_interior_cells`]). Non-zero asks for a finer brick,
    /// a moved wall or a looser interior box.
    pub dead_interior_cells: usize,
    /// Grid-neighbor segments tested for walls ([`GiBakeSettings::wall_test`]; 0 when off).
    pub wall_pairs_tested: usize,
    /// Grid-neighbor segments that cross geometry (the filter skips them).
    pub wall_pairs_blocked: usize,
    /// Probes with a wall code ([`ProbeFlags::WALLS`]): a wall toward a +X, +Y or +Z neighbor.
    pub wall_probes: usize,
    /// Bricks with [`regia_asset::baked_gi::BrickFlags::INTERIOR`].
    pub interior_bricks: usize,
    /// Valid probes inside interior bricks (off the brick's faces, which may lie outside the
    /// walls) that saw the sky: more than 1 % of their rays escaped. A sealed room reports 0; a
    /// room with an opening reports the probes that see out of it.
    pub interior_sky_probes: usize,
    /// Light contributions the luminance clamps capped in the main pass.
    pub clamped_contributions: u64,
    /// Mean over the valid probes of [`ProbeTrace::clamp_loss`].
    pub clamp_loss_mean: f32,
    /// Largest [`ProbeTrace::clamp_loss`] of a valid probe.
    pub clamp_loss_max: f32,
    /// Sample groups firefly suppression scaled down.
    pub suppressed_groups: u64,
    /// Valid probes with at least one suppressed group.
    pub probes_with_fireflies: usize,
    /// Mean over the valid probes of [`ProbeTrace::firefly_loss`].
    pub firefly_loss_mean: f32,
    /// Largest [`ProbeTrace::firefly_loss`] of a valid probe.
    pub firefly_loss_max: f32,
    /// Median over the valid probes of [`ProbeTrace::noise`] (`None` with one sample group).
    pub noise_median: Option<f32>,
    /// 95th percentile (nearest rank) of the same.
    pub noise_p95: Option<f32>,
    /// What the bounce count can miss in this scene.
    pub bounces: BounceBudget,
    /// Wall time, seconds.
    pub seconds: f64,
    /// Probe traces the relocation rounds ran (a round retraces only the probes the round
    /// before moved), plus one classification trace per probe the last round moved, for
    /// [`GiBakeSettings::skip_inside_fraction`] (ADR-862).
    pub relocation_traces: usize,
    /// Probes marked inside geometry from their relocation trace, without the main trace
    /// ([`GiBakeSettings::skip_inside_fraction`]).
    pub inside_skipped: usize,
    /// Wall time of the relocation rounds, seconds.
    pub relocation_seconds: f64,
    /// Wall time of the main trace, seconds.
    pub trace_seconds: f64,
    /// Wall time of the filter and the encoding, seconds.
    pub encode_seconds: f64,
}

/// Half-width of the band around [`GiBakeSettings::back_face_invalid_fraction`] in which a
/// probe counts as [`GiBakeReport::borderline_back_face`]: 1 % of its rays.
pub const BORDERLINE_BACK_FACE_BAND: f32 = 0.01;

/// `true` when a back-face `fraction` is within [`BORDERLINE_BACK_FACE_BAND`] of `threshold`.
#[must_use]
pub fn back_face_borderline(fraction: f32, threshold: f32) -> bool {
    (fraction - threshold).abs() <= BORDERLINE_BACK_FACE_BAND
}

/// Cells of the INTERIOR bricks of `layout` whose eight corner probes are all invalid
/// (`flags[i]` is probe `i`'s [`ProbeFlags`]; a missing entry counts as invalid). Inside an
/// interior brick the runtime skips the terrain layer and does not blend, so such a cell
/// reads black (ADR-281). A brick with one probe on an axis has one cell layer on it.
#[must_use]
pub fn dead_interior_cells(layout: &VolumeLayout, flags: &[u16]) -> usize {
    let valid = |i: usize| flags.get(i).is_some_and(|f| f & ProbeFlags::VALID != 0);
    let mut dead = 0;
    for b in layout.bricks.iter().filter(|b| b.is_interior()) {
        let cells = b.counts.map(|c| c.saturating_sub(1).max(1));
        let step = b.counts.map(|c| u32::from(c > 1));
        for z in 0..cells[2] {
            for y in 0..cells[1] {
                for x in 0..cells[0] {
                    let any = (0..8u32).any(|k| {
                        let (dx, dy, dz) = (k & 1, (k >> 1) & 1, (k >> 2) & 1);
                        valid(b.index(x + dx * step[0], y + dy * step[1], z + dz * step[2]))
                    });
                    dead += usize::from(!any);
                }
            }
        }
    }
    dead
}

/// A finished bake: the volume, the unfiltered probe results and a report.
#[derive(Clone, Debug)]
pub struct GiBake {
    /// The encoded volume.
    pub volume: BakedGiVolume,
    /// Full-precision unfiltered result per probe (validation and debugging).
    pub raw: Vec<ProbeData>,
    /// Final probe positions (nominal + relocation).
    pub positions: Vec<[f32; 3]>,
    /// Summary.
    pub report: GiBakeReport,
}

/// The [`BakeInfo`] a [`bake_gi_volume`] of `scene` with `settings` records (seed, ray counts,
/// filter, scene hash): what a staleness check compares a volume against.
#[must_use]
pub fn bake_info(scene: &PtScene, settings: &GiBakeSettings) -> BakeInfo {
    BakeInfo {
        seed: settings.trace.seed,
        rays_per_probe: settings.trace.rays_per_probe,
        samples: settings.trace.samples,
        max_bounces: settings.trace.max_bounces,
        scene_hash: scene.content_hash(),
        filtered: settings.filter,
    }
}

/// The emitters a bake of `scene` traces into the emissive component (ADR-1386): per placed mesh
/// with an emitting material, the object-space bounds of the vertices its triangles use under
/// the instance's transform ([`BakedEmitter::from_object_bounds`]). A renderer
/// that draws emissive surfaces as area lights matches them against these
/// ([`BakedEmitter::matches`]) and leaves the matched ones out where the volume covers, so
/// their light is not counted twice. Analytic lights stay live and are not listed.
#[must_use]
pub fn baked_emitters(scene: &PtScene) -> Vec<BakedEmitter> {
    scene
        .instances
        .iter()
        .filter_map(|inst| {
            let material = scene.materials.get(inst.material)?;
            if !material.is_emissive() {
                return None;
            }
            let mesh = scene.meshes.get(inst.mesh)?;
            let object = BakedEmitter::from_points(
                mesh.indices
                    .iter()
                    .filter_map(|&i| mesh.positions.get(i as usize).copied()),
            )?;
            BakedEmitter::from_object_bounds(object.min, object.max, &inst.transform.to_cols_array_2d())
        })
        .collect()
}

/// The light levels of `scene` the components are proportional to.
#[must_use]
pub fn bake_lights(scene: &PtScene) -> BakeLights {
    let lum = |c: [f32; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
    let mut lights = BakeLights::default();
    if let Some(sun) = &scene.sun {
        lights.sun_illuminance = sun.illuminance;
        lights.sun_color = sun.color;
        lights.sun_direction = Vec3::from_array(sun.toward).normalize_or(Vec3::Y).to_array();
    }
    lights.sky_scale = match &scene.sky {
        PtSky::None => 0.0,
        PtSky::Uniform(c) => lum(*c),
        PtSky::Table { scale, .. } | PtSky::Equirect { scale, .. } => *scale,
    };
    lights.fill_lux = scene
        .sky_bodies
        .iter()
        .map(|b| b.normal_irradiance() * b.light_scale)
        .sum();
    lights
}

/// A terrain-following layer over `terrain`: columns every `spacing` metres (the heightfield's
/// extent, inset by half a spacing), one probe per height in `levels` above the ground under
/// the column.
///
/// # Errors
///
/// A non-positive spacing, no or too many levels, or a degenerate heightfield.
pub fn terrain_layer(
    terrain: &PtHeightfield,
    spacing: [f32; 2],
    levels: &[f32],
) -> Result<TerrainLayer, PtError> {
    if !(spacing[0] > 0.0 && spacing[1] > 0.0) {
        return Err(PtError::Scene("terrain layer spacing must be positive".into()));
    }
    let extent = [
        (terrain.width.saturating_sub(1)) as f32 * terrain.spacing[0],
        (terrain.depth.saturating_sub(1)) as f32 * terrain.spacing[1],
    ];
    if !(extent[0] > 0.0 && extent[1] > 0.0) {
        return Err(PtError::Scene("the heightfield has no extent".into()));
    }
    // Count in f64 and bound the grid before walking it: a spacing far below the extent would
    // otherwise saturate the u32 count and allocate until the process aborts.
    let wanted = [
        (f64::from(extent[0]) / f64::from(spacing[0])).floor().max(1.0),
        (f64::from(extent[1]) / f64::from(spacing[1])).floor().max(1.0),
    ];
    let total = wanted[0] * wanted[1] * levels.len().max(1) as f64;
    if total >= MAX_PROBES as f64 {
        return Err(PtError::Scene(format!(
            "the terrain layer would hold about {total:.3e} probes; one bake traces at most {MAX_PROBES}: use a coarser spacing"
        )));
    }
    let counts = [wanted[0] as u32, wanted[1] as u32];
    // Centre the columns in the extent.
    let origin = [
        terrain.origin[0] + 0.5 * (extent[0] - (counts[0] - 1) as f32 * spacing[0]),
        terrain.origin[1] + 0.5 * (extent[1] - (counts[1] - 1) as f32 * spacing[1]),
    ];
    let mut ground = Vec::with_capacity((counts[0] * counts[1]) as usize);
    let mut holed = Vec::new();
    for j in 0..counts[1] {
        for i in 0..counts[0] {
            let x = origin[0] + i as f32 * spacing[0];
            let z = origin[1] + j as f32 * spacing[1];
            let h = terrain.height_at(x, z);
            if h.is_none() && in_grid(terrain, x, z) {
                holed.push((i, j));
            }
            ground.push(h.unwrap_or(0.0));
        }
    }
    hole_column_ground(&mut ground, counts, &holed);
    let layer = TerrainLayer {
        origin,
        spacing,
        counts,
        levels: levels.to_vec(),
        ground,
    };
    let check = VolumeLayout {
        terrain: Some(layer),
        bricks: Vec::new(),
        moment_resolution: 4,
        max_distance: 1.0,
    };
    check.validate().map_err(|e| PtError::Scene(e.to_string()))?;
    check
        .terrain
        .ok_or_else(|| PtError::Scene("terrain layer lost".into()))
}

/// `true` when world `(x, z)` lies on the heightfield's grid (a `None` height there is a cut
/// cell, a hole).
fn in_grid(terrain: &PtHeightfield, x: f32, z: f32) -> bool {
    let gx = (x - terrain.origin[0]) / terrain.spacing[0];
    let gz = (z - terrain.origin[1]) / terrain.spacing[1];
    gx >= 0.0
        && gz >= 0.0
        && gx <= (terrain.width.saturating_sub(1)) as f32
        && gz <= (terrain.depth.saturating_sub(1)) as f32
}

#[cfg(test)]
thread_local! {
    /// Cells `hole_column_ground` dequeued on this thread (test-only: proves the pass is linear).
    static HOLE_PASS_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A terrain-layer column over a hole has no ground: it takes the lowest ground of the nearest
/// ring of columns around it that has any, so its probes stand in the mouth from the rim's
/// low side up, instead of under a surface that is not there (or at 0 m).
///
/// "Nearest ring" is the Chebyshev distance to the closest column with ground. One multi-source
/// breadth-first pass over the 8-connected grid finds it for every column at once: a column at
/// distance `d` takes the lowest value of its neighbours at `d − 1` (each column with ground at
/// distance `d` is `d − 1` from one of them), so the cost is linear in the grid however much of
/// it is holed.
fn hole_column_ground(ground: &mut [f32], counts: [u32; 2], holed: &[(u32, u32)]) {
    if holed.is_empty() {
        return;
    }
    let (nx, nz) = (counts[0] as usize, counts[1] as usize);
    let cells = nx * nz;
    if cells == 0 || ground.len() < cells {
        return;
    }
    let mut is_hole = vec![false; cells];
    for &(i, j) in holed {
        if (i as usize) < nx && (j as usize) < nz {
            is_hole[j as usize * nx + i as usize] = true;
        }
    }
    let mut dist = vec![u32::MAX; cells];
    let mut best = vec![0.0f32; cells];
    let mut queue = std::collections::VecDeque::with_capacity(cells);
    for (c, &hole) in is_hole.iter().enumerate() {
        if !hole {
            dist[c] = 0;
            best[c] = ground[c];
            queue.push_back(c);
        }
    }
    while let Some(c) = queue.pop_front() {
        #[cfg(test)]
        HOLE_PASS_VISITS.with(|v| v.set(v.get() + 1));
        let (ci, cj) = ((c % nx) as i64, (c / nx) as i64);
        for dj in -1..=1i64 {
            for di in -1..=1i64 {
                let (i, j) = (ci + di, cj + dj);
                if (di, dj) == (0, 0) || i < 0 || j < 0 || i >= nx as i64 || j >= nz as i64 {
                    continue;
                }
                let n = j as usize * nx + i as usize;
                if dist[n] == u32::MAX {
                    dist[n] = dist[c] + 1;
                    best[n] = best[c];
                    queue.push_back(n);
                } else if dist[n] == dist[c] + 1 {
                    best[n] = best[n].min(best[c]);
                }
            }
        }
    }
    for ((g, &hole), (&d, &b)) in ground.iter_mut().zip(&is_hole).zip(dist.iter().zip(&best)) {
        if hole {
            // No column has ground at all: 0 m, as before.
            *g = if d == u32::MAX { 0.0 } else { b };
        }
    }
}

/// A detail brick covering the box `min..=max` with probes every `spacing` metres or closer
/// (at least two per axis). `first_probe` is assigned by [`VolumeLayout::assign_brick_offsets`].
#[must_use]
pub fn detail_brick(min: [f32; 3], max: [f32; 3], spacing: f32) -> DetailBrick {
    let mut counts = [2u32; 3];
    let mut step = [spacing; 3];
    for a in 0..3 {
        let len = (max[a] - min[a]).max(0.0);
        // Bounded so the +1 and the three-axis product cannot wrap; `check_probe_budget` then
        // rejects the oversize layout.
        let cells = (len / spacing.max(1e-3)).ceil().min(MAX_BRICK_AXIS as f32) as u32;
        counts[a] = (cells + 1).max(2);
        step[a] = len / (counts[a] - 1) as f32;
        if step[a] <= 0.0 {
            step[a] = spacing.max(1e-3);
        }
    }
    DetailBrick {
        origin: min,
        spacing: step,
        counts,
        first_probe: 0,
        flags: 0,
    }
}

/// Bakes `layout` in `scene`: under-terrain test, relocation rounds, the main trace,
/// back-face classification, the optional 3×3×3 filter, encoding. `progress(stage, done,
/// total)` reports each trace; returning `false` cancels. `tracer` is the GPU [`PathTracer`]
/// or the CPU [`crate::cpu_probe::CpuProbeTracer`] ([`ProbeTracer`]); everything but the traces
/// is the same code for both.
///
/// It is [`bake_probes`] of every probe followed by [`finish_bake`]; an incremental bake
/// ([`crate::bake_incremental`]) runs the same two steps with only the probes it has to trace.
///
/// # Errors
///
/// An inconsistent layout, a trace error, or cancellation.
pub fn bake_gi_volume<T: ProbeTracer + ?Sized>(
    tracer: &T,
    scene: &PtScene,
    layout: &VolumeLayout,
    settings: &GiBakeSettings,
    progress: impl FnMut(&str, usize, usize) -> bool,
) -> Result<GiBake, PtError> {
    bake_gi_volume_placed(tracer, scene, layout, settings, None, progress)
}

/// [`bake_gi_volume`] starting from a CPU probe placement ([`crate::placement::place_probes`]
/// for this layout, ADR-418): its offsets seed the relocation rounds, and the probes it culled
/// (`INSIDE_GEOMETRY`) or found under the terrain are never traced. `None` is
/// [`bake_gi_volume`].
///
/// # Errors
///
/// A placement for another layout, an inconsistent layout, a trace error, or cancellation.
pub fn bake_gi_volume_placed<T: ProbeTracer + ?Sized>(
    tracer: &T,
    scene: &PtScene,
    layout: &VolumeLayout,
    settings: &GiBakeSettings,
    placement: Option<&ProbePlacement>,
    progress: impl FnMut(&str, usize, usize) -> bool,
) -> Result<GiBake, PtError> {
    let _span = tracing::info_span!("pt_bake_gi").entered();
    let started = Instant::now();
    let mut layout = layout.clone();
    layout.assign_brick_offsets();
    let all: Vec<usize> = (0..layout.probe_count()).collect();
    let (probes, stats) =
        bake_probes_with_stats(tracer, scene, &layout, settings, &all, placement, progress)?;
    let mut bake = finish_bake(scene, &layout, settings, &probes)?;
    stats.apply(&mut bake.report);
    bake.report.seconds = started.elapsed().as_secs_f64();
    tracing::info!(
        target: "regia_pt",
        probes = bake.report.probes,
        valid = bake.report.valid,
        relocated = bake.report.relocated,
        relocation_traces = bake.report.relocation_traces,
        inside_skipped = bake.report.inside_skipped,
        wall_probes = bake.report.wall_probes,
        interior_sky_probes = bake.report.interior_sky_probes,
        seconds = bake.report.seconds,
        "gi volume baked"
    );
    Ok(bake)
}

/// The input hash of a probe bake (ADR-3095): the scene's content hash, the layout, every
/// setting, the placement, the probe subset and the tracer's backend. Two bakes with the same
/// hash trace the same probes bit for bit.
#[must_use]
pub fn gi_input_hash(
    scene: &PtScene,
    layout: &VolumeLayout,
    settings: &GiBakeSettings,
    placement: Option<&ProbePlacement>,
    indices: &[usize],
    backend: &str,
) -> [u8; 32] {
    let _span = tracing::info_span!("gi_input_hash").entered();
    let mut h = InputHasher::new("regia-bake-checkpoint/gi/1");
    h.bytes(backend.as_bytes());
    h.bytes(&scene.content_hash().to_le_bytes());
    h.debug(layout);
    h.debug(settings);
    h.debug(&placement);
    h.debug(&indices);
    h.finish()
}

/// The checkpoint kind of a probe bake on `backend` (`cpu` or anything else, the GPU).
#[must_use]
pub fn gi_checkpoint_kind(backend: &str) -> BakeCheckpointKind {
    if backend == "cpu" {
        BakeCheckpointKind::GiCpu
    } else {
        BakeCheckpointKind::GiGpu
    }
}

/// [`bake_gi_volume_placed`] with a checkpoint (ADR-3095): the relocation's outcome is written
/// once it is done, then the main trace's probes in batches of `plan.batch_units`
/// ([`DEFAULT_GI_BATCH_PROBES`] by default) as the plan's cadence asks. With `plan.resume` and a
/// checkpoint of this bake at `plan.path`, the bake continues after its last traced probe. The
/// checkpoint is removed when the bake finishes. Every probe keeps its random stream, so the volume
/// is byte-identical to [`bake_gi_volume_placed`]'s, resumed or not (on the GPU, for the same
/// device and driver).
///
/// # Errors
///
/// As [`bake_gi_volume_placed`]; `bake_checkpoint_mismatch`, `bake_checkpoint_damaged`,
/// `bake_checkpoint_version`, `bake_checkpoint_io` or `bake_interrupted` starting the message
/// ([`crate::checkpoint::bake_checkpoint_error_code`]).
pub fn bake_gi_volume_checkpointed<T: ProbeTracer + ?Sized>(
    tracer: &T,
    scene: &PtScene,
    layout: &VolumeLayout,
    settings: &GiBakeSettings,
    placement: Option<&ProbePlacement>,
    plan: &BakeCheckpointPlan,
    progress: impl FnMut(&str, usize, usize) -> bool,
) -> Result<(GiBake, BakeCheckpointStats), PtError> {
    let _span = tracing::info_span!("pt_bake_gi_checkpointed").entered();
    let started = Instant::now();
    let mut layout = layout.clone();
    layout.assign_brick_offsets();
    let all: Vec<usize> = (0..layout.probe_count()).collect();
    let hash = gi_input_hash(scene, &layout, settings, placement, &all, tracer.backend());
    let (mut ck, found) = Checkpointer::open(plan, gi_checkpoint_kind(tracer.backend()), hash)?;
    let texels = (layout.moment_resolution * layout.moment_resolution) as usize;
    let state = match found {
        Some(c) => {
            let state = crate::checkpoint::decode_gi(&c.payload, all.len(), texels, c.header.units_done)?;
            Some((state, c.header.units_done))
        }
        None => None,
    };
    let mut resume = GiResume {
        ck: &mut ck,
        state,
        batch: plan.batch_or(DEFAULT_GI_BATCH_PROBES),
    };
    let (probes, stats) = bake_probes_core(
        tracer,
        scene,
        &layout,
        settings,
        &all,
        placement,
        progress,
        Some(&mut resume),
    )?;
    let mut bake = finish_bake(scene, &layout, settings, &probes)?;
    stats.apply(&mut bake.report);
    bake.report.seconds = started.elapsed().as_secs_f64();
    Ok((bake, ck.finish()))
}

/// One probe's bake result before the filter: what [`bake_probes`] returns and an incremental
/// bake keeps between bakes ([`crate::bake_incremental`]).
#[derive(Clone, Debug, PartialEq)]
pub struct BakedProbe {
    /// Full-precision unfiltered SH and moments, and the meta record (offset, flags).
    pub data: ProbeData,
    /// Final position (nominal + relocation offset).
    pub position: [f32; 3],
    /// Rays of the main trace that returned a finite value (0 for a probe under the terrain,
    /// which is not traced).
    pub rays: u32,
    /// Rays of the main trace whose first hit was a back face.
    pub back_faces: u32,
    /// Rays of the main trace dropped for a non-finite value.
    pub rejected: u32,
    /// Rays of the main trace that hit nothing (escaped to the sky).
    pub escaped: u32,
    /// What the quality controls did to the main trace (ADR-1096).
    pub quality: ProbeQuality,
}

/// What the bake quality controls ([`crate::bake_quality`], ADR-1096) did to one probe's main
/// trace: the [`ProbeTrace`] fields the bake report sums.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ProbeQuality {
    /// [`ProbeTrace::clamped`].
    pub clamped: u32,
    /// [`ProbeTrace::clamp_loss`].
    pub clamp_loss: f32,
    /// [`ProbeTrace::suppressed_groups`].
    pub suppressed_groups: u32,
    /// [`ProbeTrace::firefly_loss`].
    pub firefly_loss: f32,
    /// [`ProbeTrace::noise`].
    pub noise: Option<f32>,
}

impl ProbeQuality {
    /// The quality fields of `t`.
    #[must_use]
    pub fn of(t: &ProbeTrace) -> Self {
        Self {
            clamped: t.clamped,
            clamp_loss: t.clamp_loss,
            suppressed_groups: t.suppressed_groups,
            firefly_loss: t.firefly_loss,
            noise: t.noise,
        }
    }
}

impl BakedProbe {
    /// The probe was traced (it is not under the terrain).
    #[must_use]
    pub fn traced(&self) -> bool {
        self.data.meta.flags & ProbeFlags::UNDER_TERRAIN == 0
    }

    /// Fraction of the main trace's rays that hit a back face.
    #[must_use]
    pub fn back_face_fraction(&self) -> f32 {
        if self.rays == 0 {
            0.0
        } else {
            self.back_faces as f32 / self.rays as f32
        }
    }
}

/// Bakes the probes `indices` (volume indices of `layout`) without the filter: the
/// under-terrain test, the relocation rounds, the main trace and the back-face classification
/// of [`bake_gi_volume`], over those probes only. Returns one [`BakedProbe`] per index, in
/// order. With `indices` = every probe this is the first half of
/// [`bake_gi_volume`] bit for bit.
///
/// The tracers key each probe's random streams by its index among the subset's traced probes
/// (those not under the terrain or culled by the placement; ADR-862 keeps that index through
/// every retrace), so a probe baked in a subset gets other (equally distributed) noise than in
/// a full bake, unless the subset is a prefix of the volume.
///
/// # Errors
///
/// An index past the layout, a trace error, or cancellation.
pub fn bake_probes<T: ProbeTracer + ?Sized>(
    tracer: &T,
    scene: &PtScene,
    layout: &VolumeLayout,
    settings: &GiBakeSettings,
    indices: &[usize],
    progress: impl FnMut(&str, usize, usize) -> bool,
) -> Result<Vec<BakedProbe>, PtError> {
    bake_probes_placed(tracer, scene, layout, settings, indices, None, progress)
}

/// [`bake_probes`] starting from a CPU probe placement of the whole layout
/// ([`crate::placement::place_probes`], ADR-418): the placement's offsets and flags of the
/// probes in `indices` seed the relocation rounds, and the probes it culled (`INSIDE_GEOMETRY`)
/// are never traced. `None` is [`bake_probes`].
///
/// # Errors
///
/// A placement for another layout, an index past the layout, a trace error, or cancellation.
pub fn bake_probes_placed<T: ProbeTracer + ?Sized>(
    tracer: &T,
    scene: &PtScene,
    layout: &VolumeLayout,
    settings: &GiBakeSettings,
    indices: &[usize],
    placement: Option<&ProbePlacement>,
    progress: impl FnMut(&str, usize, usize) -> bool,
) -> Result<Vec<BakedProbe>, PtError> {
    bake_probes_with_stats(tracer, scene, layout, settings, indices, placement, progress).map(|(p, _)| p)
}

/// What the probe half of a bake ([`bake_probes_with_stats`]) counted and timed, for the
/// [`GiBakeReport`] fields [`finish_bake`] cannot derive from the probes (ADR-862).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct ProbeBakeStats {
    /// [`GiBakeReport::relocation_traces`].
    pub relocation_traces: usize,
    /// [`GiBakeReport::inside_skipped`].
    pub inside_skipped: usize,
    /// [`GiBakeReport::relocation_seconds`].
    pub relocation_seconds: f64,
    /// [`GiBakeReport::trace_seconds`].
    pub trace_seconds: f64,
}

impl ProbeBakeStats {
    /// Writes these into `report`.
    pub(crate) fn apply(self, report: &mut GiBakeReport) {
        report.relocation_traces = self.relocation_traces;
        report.inside_skipped = self.inside_skipped;
        report.relocation_seconds = self.relocation_seconds;
        report.trace_seconds = self.trace_seconds;
    }
}

/// [`bake_probes_placed`] with its [`ProbeBakeStats`].
pub(crate) fn bake_probes_with_stats<T: ProbeTracer + ?Sized>(
    tracer: &T,
    scene: &PtScene,
    layout: &VolumeLayout,
    settings: &GiBakeSettings,
    indices: &[usize],
    placement: Option<&ProbePlacement>,
    progress: impl FnMut(&str, usize, usize) -> bool,
) -> Result<(Vec<BakedProbe>, ProbeBakeStats), PtError> {
    bake_probes_core(
        tracer, scene, layout, settings, indices, placement, progress, None,
    )
}

/// The checkpoint of a probe bake in progress ([`bake_gi_volume_checkpointed`]): the running
/// checkpoint, the state it resumed (taken when the bake starts), and the main trace's batch.
struct GiResume<'c, 'p> {
    ck: &'c mut Checkpointer<'p>,
    state: Option<(GiCheckpointState, u64)>,
    batch: usize,
}

/// [`bake_probes_with_stats`], checkpointed when `resume` is given (ADR-3095): the relocation's
/// outcome is checkpointed once it is done (batch 1) and the main trace in batches of probes,
/// each probe keeping its stream, so a resumed bake traces exactly what an uninterrupted one does.
#[allow(clippy::too_many_arguments)] // ADR-3095: the six inputs of `bake_probes_placed`, the progress and the checkpoint.
fn bake_probes_core<T: ProbeTracer + ?Sized>(
    tracer: &T,
    scene: &PtScene,
    layout: &VolumeLayout,
    settings: &GiBakeSettings,
    indices: &[usize],
    placement: Option<&ProbePlacement>,
    mut progress: impl FnMut(&str, usize, usize) -> bool,
    mut resume: Option<&mut GiResume<'_, '_>>,
) -> Result<(Vec<BakedProbe>, ProbeBakeStats), PtError> {
    let _span = tracing::info_span!("pt_bake_probes", probes = indices.len()).entered();
    let all_nominal = layout.nominal_positions();
    let all_half = layout.half_cells();
    if let Some(&bad) = indices.iter().find(|&&i| i >= all_nominal.len()) {
        return Err(PtError::Scene(format!(
            "probe {bad} is past the layout's {} probes",
            all_nominal.len()
        )));
    }
    if indices.is_empty() {
        return Ok((Vec::new(), ProbeBakeStats::default()));
    }
    let nominal: Vec<[f32; 3]> = indices.iter().map(|&i| all_nominal[i]).collect();
    let half: Vec<[f32; 3]> = indices.iter().map(|&i| all_half[i]).collect();
    let n = indices.len();
    let mut offsets = vec![[0.0f32; 3]; n];
    let mut flags = vec![ProbeFlags::VALID; n];
    if let Some(p) = placement {
        // The placement covers the whole layout; take the traced probes' entries.
        let mut all_offsets = vec![[0.0f32; 3]; all_nominal.len()];
        let mut all_flags = vec![ProbeFlags::VALID; all_nominal.len()];
        p.seed(&mut all_offsets, &mut all_flags)?;
        for (k, &i) in indices.iter().enumerate() {
            offsets[k] = all_offsets[i];
            flags[k] = all_flags[i];
        }
    }
    // Probes the placement culled stay out of every trace, like those under the terrain.
    let untraced = ProbeFlags::UNDER_TERRAIN | ProbeFlags::INSIDE_GEOMETRY;
    let trace_settings = ProbeTraceSettings {
        moment_resolution: layout.moment_resolution,
        max_distance: layout.max_distance,
        geometry_only: false,
        ..settings.trace
    };
    let position = |i: usize, offsets: &[[f32; 3]]| {
        [
            nominal[i][0] + offsets[i][0],
            nominal[i][1] + offsets[i][1],
            nominal[i][2] + offsets[i][2],
        ]
    };
    let under = |p: [f32; 3]| {
        scene
            .terrain
            .as_ref()
            .and_then(|t| t.height_at(p[0], p[2]))
            .is_some_and(|h| p[1] < h)
    };
    for (i, f) in flags.iter_mut().enumerate() {
        if under(nominal[i]) {
            *f = ProbeFlags::UNDER_TERRAIN;
        }
    }

    // Relocation (Majercik et al. 2021): out through the closest back face when the probe is
    // inside, else away from a front face that is too close; never past 0.45 of the cell.
    // A probe keeps its random stream (its index among the live probes) in every trace, so a
    // round retraces only the probes whose position the round before changed: the others
    // would trace exactly as before and decide the same (ADR-862).
    let relocation_started = Instant::now();
    let live: Vec<usize> = (0..n).filter(|&i| flags[i] & untraced == 0).collect();
    // Per probe: (back-face fraction, rays) of its latest relocation trace at its current
    // position.
    let mut last_trace: Vec<Option<(f32, u32)>> = vec![None; n];
    let mut relocation_traces = 0usize;
    let texels = (layout.moment_resolution * layout.moment_resolution) as usize;
    // A resumed checkpoint holds the relocation's outcome: it is not run again.
    let resumed = match resume.as_mut().and_then(|r| r.state.take()) {
        Some((state, done)) => {
            let state = if state.offsets.len() == n {
                state
            } else {
                return Err(crate::checkpoint::damaged(format!(
                    "the checkpoint holds {} probes, the bake {n}",
                    state.offsets.len()
                )));
            };
            offsets.clone_from(&state.offsets);
            flags.clone_from(&state.flags);
            last_trace.clone_from(&state.last_trace);
            relocation_traces = state.relocation_traces as usize;
            Some((state, done))
        }
        None => None,
    };
    if resumed.is_none() {
        let mut pending: Vec<usize> = (0..live.len()).collect();
        let quick = ProbeTraceSettings {
            samples: settings.relocation_samples.max(1),
            max_bounces: 1,
            sample_groups: 1,
            geometry_only: settings.geometry_only_relocation,
            ..trace_settings
        };
        for round in 0..settings.relocation_iterations {
            if pending.is_empty() {
                break;
            }
            let pos: Vec<[f32; 3]> = pending.iter().map(|&k| position(live[k], &offsets)).collect();
            let streams: Vec<u32> = pending.iter().map(|&k| k as u32).collect();
            let traces = tracer.trace_streams(scene, &pos, &streams, &quick, &mut |d, t| {
                progress(&format!("relocate {}", round + 1), d, t)
            })?;
            relocation_traces += pending.len();
            let mut retrace = Vec::new();
            let mut moved = 0usize;
            for (&k, t) in pending.iter().zip(&traces) {
                let i = live[k];
                last_trace[i] = Some((t.back_face_fraction(), t.rays));
                let h = half[i];
                let min_front = settings.min_front_face_fraction * 2.0 * h[0].min(h[1]).min(h[2]);
                let mut step = Vec3::ZERO;
                if t.back_face_fraction() > settings.back_face_invalid_fraction {
                    if let Some((dir, dist)) = t.closest_back {
                        step = Vec3::from_array(dir) * (dist + min_front.max(0.01));
                    }
                } else if let Some((dir, dist)) = t.closest_front
                    && dist < min_front
                {
                    step = -Vec3::from_array(dir) * (min_front - dist);
                }
                if step == Vec3::ZERO {
                    continue;
                }
                let o = Vec3::from_array(offsets[i]) + step;
                let limit = Vec3::from_array(h) * 0.9;
                let clamped = o.clamp(-limit, limit);
                if under((Vec3::from_array(nominal[i]) + clamped).to_array()) {
                    continue;
                }
                if offsets[i] != clamped.to_array() {
                    offsets[i] = clamped.to_array();
                    last_trace[i] = None;
                    retrace.push(k);
                }
                flags[i] |= ProbeFlags::RELOCATED;
                moved += 1;
            }
            tracing::debug!(target: "regia_pt", round, moved, "probe relocation round");
            if moved == 0 {
                break;
            }
            pending = retrace;
        }
        // A probe the last round moved has no trace at its final position: one more relocation
        // trace there (it moves nothing) lets the inside skip judge it (ADR-862). Probes inside
        // geometry keep moving to the edge of their cell, so this is most of them.
        let quick_rays = quick.rays_per_probe.clamp(1, 4096).saturating_mul(quick.samples);
        if settings.relocation_iterations > 0
            && settings.skip_inside_fraction <= 1.0
            && quick_rays >= SKIP_INSIDE_MIN_RAYS
        {
            let unjudged: Vec<usize> = (0..live.len())
                .filter(|&k| last_trace[live[k]].is_none())
                .collect();
            if !unjudged.is_empty() {
                let pos: Vec<[f32; 3]> = unjudged.iter().map(|&k| position(live[k], &offsets)).collect();
                let streams: Vec<u32> = unjudged.iter().map(|&k| k as u32).collect();
                let traces = tracer.trace_streams(scene, &pos, &streams, &quick, &mut |d, t| {
                    progress("classify", d, t)
                })?;
                relocation_traces += unjudged.len();
                for (&k, t) in unjudged.iter().zip(&traces) {
                    last_trace[live[k]] = Some((t.back_face_fraction(), t.rays));
                }
            }
        }
    }
    let relocation_seconds = resumed.as_ref().map_or_else(
        || relocation_started.elapsed().as_secs_f64(),
        |(s, _)| s.relocation_seconds,
    );

    // Main trace. A probe deep inside geometry by its last relocation trace (at its final
    // position) is left out (ADR-862); the others keep their streams, so their results are
    // the same as in a trace of every probe.
    let trace_started = Instant::now();
    let skip_inside = |i: usize| skips_main_trace(last_trace[i], settings);
    let traced: Vec<usize> = (0..live.len()).filter(|&k| !skip_inside(live[k])).collect();
    let positions: Vec<[f32; 3]> = (0..n).map(|i| position(i, &offsets)).collect();
    let pos: Vec<[f32; 3]> = traced.iter().map(|&k| positions[live[k]]).collect();
    let streams: Vec<u32> = traced.iter().map(|&k| k as u32).collect();
    let traces = match resume {
        None => tracer.trace_streams(scene, &pos, &streams, &trace_settings, &mut |d, t| {
            progress("bake", d, t)
        })?,
        Some(r) => {
            let total = pos.len();
            let state = |traces: &[ProbeTrace]| {
                crate::checkpoint::encode_gi(
                    &offsets,
                    &flags,
                    &last_trace,
                    relocation_traces as u64,
                    relocation_seconds,
                    total as u64,
                    traces,
                )
            };
            let mut traces = match resumed {
                Some((s, _)) if s.traced != total as u64 => {
                    return Err(crate::checkpoint::damaged(format!(
                        "the checkpoint traces {} probes, the bake {total}",
                        s.traced
                    )));
                }
                Some((s, _)) => s.traces,
                None => {
                    // Batch 1: the relocation is done.
                    r.ck.batch_done(1, total as u64, 0, false, || state(&[]))?;
                    Vec::with_capacity(total)
                }
            };
            while traces.len() < total {
                let start = traces.len();
                let end = (start + r.batch.max(1)).min(total);
                let part = tracer.trace_streams(
                    scene,
                    &pos[start..end],
                    &streams[start..end],
                    &trace_settings,
                    &mut |d, _| progress("bake", start + d, total),
                )?;
                if part.len() != end - start {
                    return Err(PtError::Scene(format!(
                        "the tracer returned {} probes for {}",
                        part.len(),
                        end - start
                    )));
                }
                traces.extend(part);
                r.ck.batch_done(1, total as u64, end as u64, end == total, || state(&traces))?;
            }
            traces
        }
    };
    if traces.len() != traced.len() {
        return Err(PtError::Scene(format!(
            "the tracer returned {} probes for {}",
            traces.len(),
            traced.len()
        )));
    }
    let mut stats = ProbeBakeStats {
        relocation_traces,
        inside_skipped: 0,
        relocation_seconds,
        trace_seconds: trace_started.elapsed().as_secs_f64(),
    };
    for &i in &live {
        if skip_inside(i) {
            flags[i] = (flags[i] & !ProbeFlags::VALID) | ProbeFlags::INSIDE_GEOMETRY;
            stats.inside_skipped += 1;
        }
    }
    let far = layout.max_distance;
    let mut out: Vec<BakedProbe> = (0..n)
        .map(|i| BakedProbe {
            data: ProbeData {
                sh: [[[0.0; 4]; 3]; GI_COMPONENTS],
                moments: vec![[far, far * far]; texels],
                meta: ProbeMeta {
                    offset: offsets[i],
                    flags: flags[i],
                },
            },
            position: positions[i],
            rays: 0,
            back_faces: 0,
            rejected: 0,
            escaped: 0,
            quality: ProbeQuality::default(),
        })
        .collect();
    for (i, t) in traced.iter().map(|&k| live[k]).zip(&traces) {
        let p = &mut out[i];
        p.data.sh = t.sh;
        p.data.moments.clone_from(&t.moments);
        if t.back_face_fraction() > settings.back_face_invalid_fraction {
            p.data.meta.flags = (p.data.meta.flags & !ProbeFlags::VALID) | ProbeFlags::INSIDE_GEOMETRY;
        }
        p.rays = t.rays;
        p.back_faces = t.back_faces;
        p.rejected = t.rejected;
        p.escaped = t.rays.saturating_sub(t.hits);
        p.quality = ProbeQuality::of(t);
    }
    Ok((out, stats))
}

/// The second half of [`bake_gi_volume`]: the report, the optional 3×3×3 filter over every
/// probe, and the encoding. `probes[i]` is probe `i` of `layout` (its brick offsets are
/// assigned here), as [`bake_probes`] returns it. [`GiBakeReport::seconds`] is left at 0 for the caller, and
/// so are the fields of the probe half ([`GiBakeReport::relocation_traces`], `inside_skipped`,
/// `relocation_seconds`, `trace_seconds`), which [`bake_gi_volume`] fills in.
///
/// # Errors
///
/// Another probe count than the layout's, or an inconsistent layout.
pub fn finish_bake(
    scene: &PtScene,
    layout: &VolumeLayout,
    settings: &GiBakeSettings,
    probes: &[BakedProbe],
) -> Result<GiBake, PtError> {
    let _span = tracing::info_span!("pt_finish_bake", probes = probes.len()).entered();
    let mut layout = layout.clone();
    layout.assign_brick_offsets();
    let layout = &layout;
    let n = layout.probe_count();
    if probes.len() != n {
        return Err(PtError::Scene(format!(
            "{} baked probes for a layout of {n}",
            probes.len()
        )));
    }
    let mut report = GiBakeReport {
        probes: n,
        interior_bricks: layout.bricks.iter().filter(|b| b.is_interior()).count(),
        bounces: bounce_budget(scene, settings.trace.max_bounces),
        ..GiBakeReport::default()
    };
    let in_interior = interior_probes(layout);
    let (mut clamp_sum, mut firefly_sum, mut counted) = (0.0f64, 0.0f64, 0usize);
    let mut noise = Vec::new();
    for (i, p) in probes.iter().enumerate().filter(|(_, p)| p.traced()) {
        report.borderline_back_face += usize::from(back_face_borderline(
            p.back_face_fraction(),
            settings.back_face_invalid_fraction,
        ));
        if p.data.meta.is_valid() && in_interior[i] && saw_sky(p.escaped, p.rays) {
            report.interior_sky_probes += 1;
        }
        report.rejected_rays += u64::from(p.rejected);
        report.rays_traced += u64::from(p.rays);
        let q = &p.quality;
        report.clamped_contributions += u64::from(q.clamped);
        report.suppressed_groups += u64::from(q.suppressed_groups);
        if p.data.meta.is_valid() {
            counted += 1;
            clamp_sum += f64::from(q.clamp_loss);
            firefly_sum += f64::from(q.firefly_loss);
            report.clamp_loss_max = report.clamp_loss_max.max(q.clamp_loss);
            report.firefly_loss_max = report.firefly_loss_max.max(q.firefly_loss);
            report.probes_with_fireflies += usize::from(q.suppressed_groups > 0);
            noise.extend(q.noise);
        }
    }
    if counted > 0 {
        report.clamp_loss_mean = (clamp_sum / counted as f64) as f32;
        report.firefly_loss_mean = (firefly_sum / counted as f64) as f32;
    }
    if !noise.is_empty() {
        noise.sort_by(f32::total_cmp);
        report.noise_median = Some(median(&noise));
        let rank = (noise.len() * 95).div_ceil(100).max(1);
        report.noise_p95 = noise.get(rank - 1).copied();
    }
    let mut raw: Vec<ProbeData> = probes.iter().map(|p| p.data.clone()).collect();
    let positions: Vec<[f32; 3]> = probes.iter().map(|p| p.position).collect();
    // Leak control (ADR-366): walls between grid neighbors, at the final positions.
    let walls = if settings.wall_test {
        let geometry = LeakGeometry::from_scene(scene);
        NeighborWalls::scan(layout, &positions, &geometry)
    } else {
        NeighborWalls::none(n)
    };
    for (i, p) in raw.iter_mut().enumerate() {
        p.meta.flags |= walls.probe_flags(i);
    }
    report.wall_pairs_tested = walls.tested_pairs();
    report.wall_pairs_blocked = walls.blocked_pairs();
    for p in &raw {
        let f = p.meta.flags;
        report.valid += usize::from(f & ProbeFlags::VALID != 0);
        report.under_terrain += usize::from(f & ProbeFlags::UNDER_TERRAIN != 0);
        report.inside_geometry += usize::from(f & ProbeFlags::INSIDE_GEOMETRY != 0);
        report.relocated += usize::from(f & ProbeFlags::RELOCATED != 0);
        report.wall_probes += usize::from(f & ProbeFlags::WALLS != 0);
    }
    let meta_flags: Vec<u16> = raw.iter().map(|p| p.meta.flags).collect();
    report.dead_interior_cells = dead_interior_cells(layout, &meta_flags);
    if report.dead_interior_cells > 0 {
        tracing::warn!(
            target: "regia_pt",
            cells = report.dead_interior_cells,
            "interior brick cells with no valid probe read black at runtime"
        );
    }
    let encode_started = Instant::now();
    let filtered = if settings.filter {
        filter_probes_with(layout, &positions, &raw, |i, step| walls.blocked(i, step))
    } else {
        raw.clone()
    };
    let info = bake_info(scene, settings);
    let mut volume = BakedGiVolume::from_probes(layout, bake_lights(scene), info, &filtered)
        .map_err(|e| PtError::Scene(e.to_string()))?;
    // ADR-1386: name the emitters whose light the emissive component holds.
    volume.emitters = Some(baked_emitters(scene));
    report.encode_seconds = encode_started.elapsed().as_secs_f64();
    Ok(GiBake {
        volume,
        raw,
        positions,
        report,
    })
}

/// Probe `i` lies inside a brick with [`regia_asset::baked_gi::BrickFlags::INTERIOR`], off
/// the brick's faces.
fn interior_probes(layout: &VolumeLayout) -> Vec<bool> {
    let mut out = vec![false; layout.probe_count()];
    for b in layout.bricks.iter().filter(|b| b.is_interior()) {
        let first = b.first_probe as usize;
        for local in 0..b.probe_count() {
            let (x, y, z) = b.coords(local);
            let inner = [x, y, z].iter().zip(b.counts).all(|(&c, n)| c > 0 && c + 1 < n);
            if let Some(flag) = out.get_mut(first + local) {
                *flag = inner;
            }
        }
    }
    out
}

/// More than 1 % of a probe's `rays` escaped to the sky (`escaped` of them hit nothing).
fn saw_sky(escaped: u32, rays: u32) -> bool {
    escaped > 0 && (escaped as f32) > 0.01 * (rays as f32)
}

/// The 3×3×3 visibility-weighted SH filter: every valid probe becomes the weighted mean of
/// itself and its valid grid neighbors, weights `2^(3 − |dx| − |dy| − |dz|)` (a separable
/// [1 2 1] kernel) times the smaller of the two probes' Chebyshev visibility of each other
/// from their distance moments, so light does not bleed through walls between probes.
/// Invalid probes are left as they are. [`filter_probes_with`] also skips neighbors behind
/// geometry.
#[must_use]
pub fn filter_probes(layout: &VolumeLayout, positions: &[[f32; 3]], probes: &[ProbeData]) -> Vec<ProbeData> {
    filter_probes_core(layout, positions, probes, |_, _, _| 1.0)
}

/// [`filter_probes`] that never averages probe `i` with its neighbor at grid step `step` when
/// `blocked(i, step)` ([`NeighborWalls::blocked`]: the segment between them crosses geometry,
/// ADR-366). The Chebyshev weight alone lets a thin wall through when the moment texels average
/// it with the open space beside it.
#[must_use]
pub fn filter_probes_with(
    layout: &VolumeLayout,
    positions: &[[f32; 3]],
    probes: &[ProbeData],
    blocked: impl Fn(usize, [i32; 3]) -> bool,
) -> Vec<ProbeData> {
    filter_probes_core(layout, positions, probes, |i, _, step| {
        if blocked(i, step) { 0.0 } else { 1.0 }
    })
}

/// [`filter_probes`] with an extra visibility factor per neighbor pair: `visibility(i, j)`
/// (clamped to `0..=1`) multiplies the weight probe `j` gets in probe `i`'s mean, on top of the
/// moments' Chebyshev term. The leak-control hook for what coarse moments miss (a door frame or
/// a wall edge inside one moment texel, ADR-357): a bake that knows which neighbor pairs a wall
/// separates returns 0 for them. A factor of 1 everywhere is [`filter_probes`] bit for bit.
#[must_use]
pub fn filter_probes_weighted(
    layout: &VolumeLayout,
    positions: &[[f32; 3]],
    probes: &[ProbeData],
    visibility: impl Fn(usize, usize) -> f32,
) -> Vec<ProbeData> {
    filter_probes_core(layout, positions, probes, |i, j, _| visibility(i, j))
}

/// The filter behind [`filter_probes`], [`filter_probes_with`] and [`filter_probes_weighted`]:
/// `factor(i, j, step)` (clamped to `0..=1`) scales neighbor `j`'s weight in probe `i`'s mean.
fn filter_probes_core(
    layout: &VolumeLayout,
    positions: &[[f32; 3]],
    probes: &[ProbeData],
    factor: impl Fn(usize, usize, [i32; 3]) -> f32,
) -> Vec<ProbeData> {
    let _span = tracing::info_span!("pt_filter_probes", probes = probes.len()).entered();
    let res = layout.moment_resolution;
    let mut out = probes.to_vec();
    for (i, p) in probes.iter().enumerate() {
        if !p.meta.is_valid() || i >= positions.len() {
            continue;
        }
        let mut acc = scale_sh(&p.sh, 8.0);
        let mut wsum = 8.0f32;
        let here = Vec3::from_array(positions[i]);
        for (j, step) in layout.neighbors(i) {
            let Some(q) = probes.get(j) else { continue };
            if !q.meta.is_valid() || j >= positions.len() {
                continue;
            }
            let extra = factor(i, j, step).clamp(0.0, 1.0);
            if extra <= 0.0 {
                continue;
            }
            let v = Vec3::from_array(positions[j]) - here;
            let dist = v.length();
            if dist <= 0.0 {
                continue;
            }
            let dir = v / dist;
            let (m0, s0) = sample_moments(&p.moments, res, dir.to_array());
            let (m1, s1) = sample_moments(&q.moments, res, (-dir).to_array());
            let vis = chebyshev(m0, s0, dist).min(chebyshev(m1, s1, dist)) * extra;
            let spatial = (3 - step.iter().map(|s| s.abs()).sum::<i32>()).max(0);
            let w = (1 << spatial) as f32 * vis;
            if w > 0.0 {
                add_sh(&mut acc, &q.sh, w);
                wsum += w;
            }
        }
        out[i].sh = scale_sh(&acc, 1.0 / wsum);
    }
    out
}

fn scale_sh(sh: &[ShRgb; GI_COMPONENTS], k: f32) -> [ShRgb; GI_COMPONENTS] {
    let mut out = *sh;
    for v in out.iter_mut().flatten().flatten() {
        *v *= k;
    }
    out
}

fn add_sh(acc: &mut [ShRgb; GI_COMPONENTS], sh: &[ShRgb; GI_COMPONENTS], w: f32) {
    for (a, b) in acc
        .iter_mut()
        .flatten()
        .flatten()
        .zip(sh.iter().flatten().flatten())
    {
        *a += w * b;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-1386: the bake lists each placed mesh with an emitting material once, as the world
    /// bounds of the vertices its triangles use (a rotated, scaled instance included), and no
    /// other surface.
    #[test]
    fn the_bake_lists_its_emitters_in_world_space() {
        use crate::scene::{PtCamera, PtMaterial};
        use regia_core::glam::{Mat4, Quat};
        let mut scene = PtScene::new(PtCamera {
            eye: [0.0, 1.0, 3.0],
            target: [0.0; 3],
            fov_y: 0.8,
            width: 8,
            height: 8,
            aperture_radius: 0.0,
            focus_distance: 1.0,
        });
        // A 0.4 m panel facing down, plus a vertex no triangle uses (left out of the bounds).
        let mut panel = crate::scenes::quad([
            [-0.2, 0.0, -0.2],
            [-0.2, 0.0, 0.2],
            [0.2, 0.0, 0.2],
            [0.2, 0.0, -0.2],
        ]);
        panel.positions.push([9.0, 9.0, 9.0]);
        panel.normals.push([0.0, 1.0, 0.0]);
        let panel = scene.add_mesh(panel);
        let floor = scene.add_mesh(crate::scenes::quad([
            [-1.0, 0.0, -1.0],
            [-1.0, 0.0, 1.0],
            [1.0, 0.0, 1.0],
            [1.0, 0.0, -1.0],
        ]));
        let light = scene.add_material(PtMaterial::emitter([1000.0; 3]));
        let white = scene.add_material(PtMaterial::diffuse([0.8; 3]));
        scene.add_instance(floor, white, Mat4::IDENTITY);
        scene.add_instance(panel, light, Mat4::from_translation(Vec3::new(0.0, 2.0, 0.0)));
        // Turned 90° about Y, stretched 2× along its own X, at (3, 1, 0).
        scene.add_instance(
            panel,
            light,
            Mat4::from_scale_rotation_translation(
                Vec3::new(2.0, 1.0, 1.0),
                Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
                Vec3::new(3.0, 1.0, 0.0),
            ),
        );
        let emitters = baked_emitters(&scene);
        assert_eq!(emitters.len(), 2, "{emitters:?}");
        let close = |a: [f32; 3], b: [f32; 3]| (0..3).all(|k| (a[k] - b[k]).abs() < 1e-5);
        assert!(close(emitters[0].min, [-0.2, 2.0, -0.2]), "{:?}", emitters[0]);
        assert!(close(emitters[0].max, [0.2, 2.0, 0.2]), "{:?}", emitters[0]);
        // Local X (0.8 m after the stretch) now runs along world Z.
        assert!(close(emitters[1].min, [2.8, 1.0, -0.4]), "{:?}", emitters[1]);
        assert!(close(emitters[1].max, [3.2, 1.0, 0.4]), "{:?}", emitters[1]);
        assert!(emitters[1].matches([2.8, 1.0, -0.4], [3.2, 1.0, 0.4]));
        assert!(!emitters[0].matches(emitters[1].min, emitters[1].max));
    }

    /// The ring search `hole_column_ground` replaced (review holes #5), as the reference.
    fn hole_column_ground_rings(ground: &mut [f32], counts: [u32; 2], holed: &[(u32, u32)]) {
        let is_hole: std::collections::HashSet<(u32, u32)> = holed.iter().copied().collect();
        let (nx, nz) = (counts[0] as i64, counts[1] as i64);
        let filled: Vec<((u32, u32), f32)> = holed
            .iter()
            .map(|&(i, j)| {
                let lowest = (1..nx.max(nz)).find_map(|r| {
                    let mut lowest: Option<f32> = None;
                    for dj in -r..=r {
                        for di in -r..=r {
                            if di.abs() != r && dj.abs() != r {
                                continue;
                            }
                            let (ci, cj) = (i64::from(i) + di, i64::from(j) + dj);
                            if !(0..nx).contains(&ci) || !(0..nz).contains(&cj) {
                                continue;
                            }
                            if is_hole.contains(&(ci as u32, cj as u32)) {
                                continue;
                            }
                            let g = ground[(cj * nx + ci) as usize];
                            lowest = Some(lowest.map_or(g, |l| l.min(g)));
                        }
                    }
                    lowest
                });
                ((i, j), lowest.unwrap_or(0.0))
            })
            .collect();
        for ((i, j), g) in filled {
            ground[(j * counts[0] + i) as usize] = g;
        }
    }

    /// Review holes #5: the linear pass gives the ring search's answer on holed grids (blobs,
    /// stripes, a fully holed grid), and a 300 × 300 grid holed but for one corner column,
    /// which took the ring search billions of steps, finishes at once.
    #[test]
    fn hole_columns_take_the_nearest_ring_in_linear_time() {
        let counts = [23u32, 17];
        let ground: Vec<f32> = (0..counts[0] * counts[1])
            .map(|c| ((c * 7919) % 101) as f32 * 0.5 - 20.0)
            .collect();
        let patterns: [&dyn Fn(u32, u32) -> bool; 4] = [
            &|i, j| (i as i32 - 11).pow(2) + (j as i32 - 8).pow(2) < 30,
            &|i, _| i % 5 == 2,
            &|i, j| (i + 2 * j) % 7 < 4,
            &|_, _| true,
        ];
        for (k, pattern) in patterns.iter().enumerate() {
            let holed: Vec<(u32, u32)> = (0..counts[1])
                .flat_map(|j| (0..counts[0]).map(move |i| (i, j)))
                .filter(|&(i, j)| pattern(i, j))
                .collect();
            let (mut fast, mut slow) = (ground.clone(), ground.clone());
            hole_column_ground(&mut fast, counts, &holed);
            hole_column_ground_rings(&mut slow, counts, &holed);
            assert_eq!(fast, slow, "pattern {k}");
        }
        let counts = [300u32, 300];
        let mut ground = vec![1.0f32; 90_000];
        ground[0] = -3.0;
        let holed: Vec<(u32, u32)> = (0..300u32)
            .flat_map(|j| (0..300u32).map(move |i| (i, j)))
            .filter(|&c| c != (0, 0))
            .collect();
        // Linear: every cell is dequeued exactly once (a count, not a wall-clock budget, which a
        // loaded test machine exceeded: audit-v035-flaky M7).
        HOLE_PASS_VISITS.with(|v| v.set(0));
        let start = std::time::Instant::now();
        hole_column_ground(&mut ground, counts, &holed);
        eprintln!("300 x 300 hole pass: {:?}", start.elapsed());
        assert_eq!(HOLE_PASS_VISITS.with(std::cell::Cell::get), 90_000);
        assert!(ground.iter().all(|&g| g == -3.0));
    }

    fn data(value: f32, moments: [f32; 2]) -> ProbeData {
        ProbeData {
            sh: [[[value, 0.0, 0.0, 0.0]; 3]; GI_COMPONENTS],
            moments: vec![moments; 16],
            meta: ProbeMeta {
                offset: [0.0; 3],
                flags: ProbeFlags::VALID,
            },
        }
    }

    fn line_layout(n: u32) -> VolumeLayout {
        let mut l = VolumeLayout {
            terrain: None,
            bricks: vec![detail_brick([0.0; 3], [(n - 1) as f32, 0.0, 0.0], 1.0)],
            moment_resolution: 4,
            max_distance: 10.0,
        };
        l.bricks[0].counts = [n, 1, 1];
        l.bricks[0].spacing = [1.0; 3];
        l.assign_brick_offsets();
        l
    }

    #[test]
    fn filter_keeps_a_uniform_field() {
        let l = line_layout(5);
        let pos = l.nominal_positions();
        let probes: Vec<ProbeData> = (0..5).map(|_| data(2.0, [10.0, 100.0])).collect();
        let out = filter_probes(&l, &pos, &probes);
        for p in &out {
            assert!((p.sh[0][0][0] - 2.0).abs() < 1e-6);
        }
    }

    #[test]
    fn filter_smooths_noise_but_not_through_a_wall() {
        let l = line_layout(4);
        let pos = l.nominal_positions();
        // Open space: probe 1 is noisy, its neighbors see 10 m free.
        let open: Vec<ProbeData> = [1.0, 3.0, 1.0, 1.0]
            .iter()
            .map(|&v| data(v, [10.0, 100.0]))
            .collect();
        let out = filter_probes(&l, &pos, &open);
        assert!(out[1].sh[0][0][0] < 2.5 && out[1].sh[0][0][0] > 1.0);
        // Every probe sees a wall 0.3 m away in every direction: the neighbors 1 m away are
        // behind it and get (almost) no weight.
        let walled: Vec<ProbeData> = [1.0, 3.0, 1.0, 1.0]
            .iter()
            .map(|&v| data(v, [0.3, 0.09]))
            .collect();
        let out = filter_probes(&l, &pos, &walled);
        assert!((out[1].sh[0][0][0] - 3.0).abs() < 0.05, "{}", out[1].sh[0][0][0]);
    }

    /// Moments of a probe at `x` with a plane at world `wall` (no wall when `None`), built as
    /// `pt.wgsl` accumulates them: every direction of a fixed spherical Fibonacci set weighted
    /// into each texel by `max(cos, 0)^exponent`, distances clamped to `far`.
    fn plane_moments(x: f32, wall: Option<f32>, res: u32, far: f32) -> Vec<[f32; 2]> {
        let n = 8192;
        let golden = std::f32::consts::PI * (3.0 - 5.0f32.sqrt());
        let dirs: Vec<Vec3> = (0..n)
            .map(|k| {
                let y = 1.0 - 2.0 * (k as f32 + 0.5) / n as f32;
                let r = (1.0 - y * y).max(0.0).sqrt();
                let phi = golden * k as f32;
                Vec3::new(r * phi.cos(), y, r * phi.sin())
            })
            .collect();
        (0..(res * res) as usize)
            .map(|t| {
                let td = Vec3::from_array(regia_asset::baked_gi::texel_direction(t, res));
                let (mut m, mut m2, mut wsum) = (0.0f32, 0.0f32, 0.0f32);
                for d in &dirs {
                    let w = td.dot(*d).max(0.0).powf(50.0);
                    if w <= 0.0 {
                        continue;
                    }
                    let dist = match wall {
                        Some(wx) if d.x * (wx - x) > 0.0 => ((wx - x) / d.x).min(far),
                        _ => far,
                    };
                    m += w * dist;
                    m2 += w * dist * dist;
                    wsum += w;
                }
                if wsum > 0.0 {
                    [m / wsum, m2 / wsum]
                } else {
                    [far, far * far]
                }
            })
            .collect()
    }

    /// Review baked-light #2 (checked, not reproduced): a 0.2 m partition spanning the space
    /// between two probes 1 m apart blocks the filter with the default 4 × 4 moments. Every
    /// bilinear tap toward the neighbor sees the partition, so its mean is the partition's
    /// distance and Chebyshev gives the far probe almost no weight. Without the partition the
    /// two probes mix. (What the 4 × 4 moments can miss is an edge inside one texel, a door
    /// frame: `filter_probes_weighted` is the hook for that, ADR-357.)
    #[test]
    fn a_thin_partition_between_probes_blocks_the_filter() {
        let l = line_layout(2);
        let pos = l.nominal_positions();
        let far = 10.0;
        let with = |m0: Vec<[f32; 2]>, m1: Vec<[f32; 2]>| {
            let mut a = data(1.0, [0.0; 2]);
            a.moments = m0;
            let mut b = data(3.0, [0.0; 2]);
            b.moments = m1;
            filter_probes(&l, &pos, &[a, b])
        };
        // The partition's faces at x = 0.4 and 0.6.
        let walled = with(
            plane_moments(0.0, Some(0.4), 4, far),
            plane_moments(1.0, Some(0.6), 4, far),
        );
        assert!(
            (walled[0].sh[0][0][0] - 1.0).abs() < 0.01,
            "{}",
            walled[0].sh[0][0][0]
        );
        assert!(
            (walled[1].sh[0][0][0] - 3.0).abs() < 0.01,
            "{}",
            walled[1].sh[0][0][0]
        );
        let open = with(plane_moments(0.0, None, 4, far), plane_moments(1.0, None, 4, far));
        assert!(open[0].sh[0][0][0] > 1.5, "{}", open[0].sh[0][0][0]);
        assert!(open[1].sh[0][0][0] < 2.5, "{}", open[1].sh[0][0][0]);
    }

    /// The leak-control hook: a factor of 1 is the plain filter bit for bit, 0 for a pair keeps
    /// the two probes apart even where the moments see open space, and only that pair.
    #[test]
    fn the_filter_visibility_hook_cuts_chosen_pairs() {
        let l = line_layout(3);
        let pos = l.nominal_positions();
        let probes: Vec<ProbeData> = [1.0, 3.0, 5.0].iter().map(|&v| data(v, [10.0, 100.0])).collect();
        assert_eq!(
            filter_probes_weighted(&l, &pos, &probes, |_, _| 1.0),
            filter_probes(&l, &pos, &probes)
        );
        // A wall between probes 1 and 2 that the moments do not see.
        let cut = |i: usize, j: usize| if (i.min(j), i.max(j)) == (1, 2) { 0.0 } else { 1.0 };
        let out = filter_probes_weighted(&l, &pos, &probes, cut);
        // Probe 1 mixes with 0 only: (8 · 3 + 4 · 1) / 12.
        assert!(
            (out[1].sh[0][0][0] - 28.0 / 12.0).abs() < 1e-5,
            "{}",
            out[1].sh[0][0][0]
        );
        assert_eq!(out[2].sh[0][0][0], 5.0, "probe 2 has no other neighbor");
        // Out-of-range factors clamp: 7 counts as 1.
        assert_eq!(
            filter_probes_weighted(&l, &pos, &probes, |_, _| 7.0),
            filter_probes(&l, &pos, &probes)
        );
    }

    /// Review baked-light #7: an INTERIOR brick cell with all eight corners invalid is counted
    /// (it reads black at runtime); one valid corner, or a brick without the flag, is not.
    #[test]
    fn dead_interior_cells_are_counted() {
        use regia_asset::baked_gi::BrickFlags;
        let mut l = VolumeLayout {
            terrain: None,
            bricks: vec![detail_brick([0.0; 3], [2.0; 3], 1.0)],
            moment_resolution: 4,
            max_distance: 4.0,
        };
        l.assign_brick_offsets();
        let b = l.bricks[0];
        assert_eq!(b.counts, [3, 3, 3]);
        let mut flags = vec![ProbeFlags::VALID; 27];
        assert_eq!(dead_interior_cells(&l, &flags), 0, "not an interior brick");
        l.bricks[0].flags = BrickFlags::INTERIOR;
        assert_eq!(dead_interior_cells(&l, &flags), 0);
        // The eight corners of cell (0, 0, 0) inside geometry.
        for k in 0..8u32 {
            flags[b.index(k & 1, (k >> 1) & 1, (k >> 2) & 1)] = ProbeFlags::INSIDE_GEOMETRY;
        }
        assert_eq!(dead_interior_cells(&l, &flags), 1);
        // The probe at (1, 1, 1) is a corner of all eight cells: every cell is dead without it.
        flags = vec![ProbeFlags::INSIDE_GEOMETRY; 27];
        flags[b.index(1, 1, 1)] = ProbeFlags::VALID;
        assert_eq!(dead_interior_cells(&l, &flags), 0);
        flags[b.index(1, 1, 1)] = ProbeFlags::INSIDE_GEOMETRY;
        assert_eq!(dead_interior_cells(&l, &flags), 8);
        // Missing flags count as invalid; a flat brick has one cell layer.
        assert_eq!(dead_interior_cells(&l, &[]), 8);
        let mut flat = l.clone();
        flat.bricks[0].counts = [3, 1, 3];
        flat.assign_brick_offsets();
        assert_eq!(dead_interior_cells(&flat, &[0; 9]), 4);
        assert!(back_face_borderline(0.255, 0.25));
        assert!(!back_face_borderline(0.3, 0.25));
    }

    #[test]
    fn invalid_neighbors_are_ignored() {
        let l = line_layout(3);
        let pos = l.nominal_positions();
        let mut probes: Vec<ProbeData> = [1.0, 1.0, 50.0].iter().map(|&v| data(v, [10.0, 100.0])).collect();
        probes[2].meta.flags = ProbeFlags::INSIDE_GEOMETRY;
        let out = filter_probes(&l, &pos, &probes);
        assert!((out[1].sh[0][0][0] - 1.0).abs() < 1e-6);
        assert_eq!(out[2].sh[0][0][0], 50.0);
    }

    #[test]
    fn terrain_layer_follows_the_ground() {
        let hf = PtHeightfield {
            width: 9,
            depth: 9,
            origin: [0.0, 0.0],
            spacing: [1.0, 1.0],
            heights: (0..81).map(|k| (k % 9) as f32 * 0.5).collect(),
            material: 0,
        };
        let layer = terrain_layer(&hf, [2.0, 2.0], &[1.0, 2.0, 4.0]).expect("layer");
        assert_eq!(layer.counts, [4, 4]);
        for j in 0..4 {
            for i in 0..4 {
                let p = layer.nominal_position(i, j, 0);
                let h = hf.height_at(p[0], p[2]).expect("inside");
                assert!((p[1] - h - 1.0).abs() < 1e-5);
            }
        }
        assert!(terrain_layer(&hf, [0.0, 1.0], &[1.0]).is_err());
        assert!(terrain_layer(&hf, [1.0, 1.0], &[]).is_err());
    }

    #[test]
    fn heightfield_height_follows_the_split_triangles() {
        let hf = PtHeightfield {
            width: 2,
            depth: 2,
            origin: [0.0, 0.0],
            spacing: [1.0, 1.0],
            heights: vec![0.0, 1.0, 1.0, 0.0],
            material: 0,
        };
        // (i, j+1)–(i+1, j) diagonal is the ridge at height 1.
        assert!((hf.height_at(0.5, 0.5).expect("in") - 1.0).abs() < 1e-6);
        assert!((hf.height_at(0.0, 0.0).expect("in")).abs() < 1e-6);
        assert!((hf.height_at(1.0, 1.0).expect("in")).abs() < 1e-6);
        assert!(hf.height_at(-0.1, 0.5).is_none());
    }

    /// Index of the sky component in `ProbeTrace::sh`.
    const SKY: usize = regia_asset::baked_gi::GiComponent::Sky as usize;

    /// A synthetic probe record: grey sky light summing to `lum_sum` over `rays` rays (L00 rows
    /// of the sky component), the given stats, closest hits, clamp sums and moment texels.
    fn record(
        rays: f32,
        lum_sum: f32,
        stats: [f32; 3],
        hits: [[f32; 4]; 2],
        clamp: [f32; 2],
        moments: &[[f32; 4]],
    ) -> Vec<[f32; 4]> {
        let mut r = vec![[0.0f32; 4]; PROBE_HEADER_VEC4S];
        for row in &mut r[3..6] {
            row[0] = lum_sum * SH_Y00;
        }
        r[PROBE_STATS] = [rays, stats[0], stats[1], stats[2]];
        r[PROBE_BACK] = hits[0];
        r[PROBE_FRONT] = hits[1];
        r[PROBE_CLAMP] = [clamp[0], clamp[1], 0.0, 0.0];
        r.extend_from_slice(moments);
        r
    }

    /// Sample-group records fold into one trace: stats, clamp sums and moments add up, the closest
    /// hits take the nearest, the clamp loss is removed over removed + kept luminance, and two
    /// groups give the noise of their means. One group decodes exactly as a plain record.
    #[test]
    fn group_records_fold_into_one_trace() {
        let far = 8.0;
        let empty = [0.0f32; 4];
        let a = record(
            10.0,
            10.0,
            [1.0, 5.0, 0.0],
            [[1.0, 0.0, 0.0, 2.0], [0.0, 0.0, 1.0, 0.5]],
            [3.0, 2.0],
            &[[2.0, 4.0, 1.0, 0.0], empty, empty, empty],
        );
        let b = record(
            10.0,
            20.0,
            [2.0, 6.0, 1.0],
            [[0.0, 1.0, 0.0, 1.0], empty],
            [1.0, 1.0],
            &[[4.0, 16.0, 1.0, 0.0], empty, empty, empty],
        );
        let t = decode_groups(&[&a[..], &b[..]], 4, far, 0.0);
        assert_eq!((t.rays, t.back_faces, t.hits, t.rejected), (20, 3, 11, 1));
        assert_eq!(t.closest_back, Some(([0.0, 1.0, 0.0], 1.0)));
        assert_eq!(t.closest_front, Some(([0.0, 0.0, 1.0], 0.5)));
        assert_eq!(t.moments[0], [3.0, 10.0]);
        assert_eq!(t.moments[1], [far, far * far]);
        assert_eq!(t.clamped, 3);
        assert!((t.clamp_loss - 4.0 / 34.0).abs() < 1e-6, "{}", t.clamp_loss);
        // Group means 1 and 2: standard error 0.5 of a mean of 1.5.
        assert!(
            (t.noise.expect("two groups") - 1.0 / 3.0).abs() < 1e-5,
            "{:?}",
            t.noise
        );
        // Sky L00 = 4π/N · Σ: the mean luminance 1.5 times 4π·Y00.
        let want = 4.0 * std::f32::consts::PI * SH_Y00 * 1.5;
        assert!((t.sh[SKY][0][0] - want).abs() < 1e-4, "{:?}", t.sh);
        assert_eq!(decode_groups(&[&a[..]], 4, far, 4.0), decode_probe(&a, 4, far));
        assert_eq!(decode_groups(&[], 4, far, 4.0).rays, 0);
    }

    /// A firefly group is scaled to `k ×` the median group before the SH are decoded; the stats
    /// and the moments are not touched.
    #[test]
    fn suppression_rescales_the_decoded_sh() {
        let empty = [0.0f32; 4];
        let quiet = record(16.0, 16.0, [0.0, 0.0, 0.0], [empty, empty], [0.0, 0.0], &[empty]);
        let loud = record(
            16.0,
            16_000.0,
            [0.0, 0.0, 0.0],
            [empty, empty],
            [0.0, 0.0],
            &[empty],
        );
        let groups = [&quiet[..], &quiet[..], &loud[..], &quiet[..]];
        let t = decode_groups(&groups, 1, 4.0, 4.0);
        assert_eq!((t.suppressed_groups, t.rays), (1, 64));
        // Means 1, 1, 1000, 1 → the loud group is capped at 4: (1 + 1 + 4 + 1) / 4.
        let want = 4.0 * std::f32::consts::PI * SH_Y00 * 7.0 / 4.0;
        assert!((t.sh[SKY][0][0] - want).abs() < 1e-3, "{:?}", t.sh);
        assert!(
            (t.firefly_loss - (1003.0 - 7.0) / 1003.0).abs() < 1e-4,
            "{}",
            t.firefly_loss
        );
        let plain = decode_groups(&groups, 1, 4.0, 0.0);
        assert_eq!(plain.suppressed_groups, 0);
        assert!(plain.sh[SKY][0][0] > 100.0 * t.sh[SKY][0][0]);
    }

    #[test]
    fn probe_ray_count_and_dispatch_are_bounded() {
        assert_eq!(max_samples(256), 1 << 16);
        assert_eq!(max_samples(4096), 1 << 12);
        assert_eq!(max_samples(1), 1 << 24);
        assert!(u64::from(max_samples(256)) * 256 <= MAX_PROBE_RAYS);
        assert!(check_dispatch(4096, 256, 64).is_ok());
        assert!(check_dispatch(1_048_576, 4096, 64).is_err());
        assert!(check_dispatch(1 << 24, 256, 64).is_err());
        assert!(check_dispatch(1 << 24, 128, 100).is_ok());
    }

    #[test]
    fn brick_covers_its_box() {
        let b = detail_brick([0.0, 0.0, 0.0], [3.0, 1.0, 0.0], 1.0);
        assert_eq!(b.counts, [4, 2, 2]);
        assert_eq!(b.nominal_position(3, 1, 0), [3.0, 1.0, 0.0]);
    }

    #[test]
    fn too_fine_terrain_spacing_is_an_error_not_an_abort() {
        let hf = PtHeightfield {
            width: 2,
            depth: 2,
            origin: [0.0, 0.0],
            spacing: [16_000.0, 16_000.0],
            heights: vec![0.0; 4],
            material: 0,
        };
        assert!(terrain_layer(&hf, [0.01, 0.01], &[1.0]).is_err());
        assert!(terrain_layer(&hf, [1e-30, 1e-30], &[1.0]).is_err());
    }

    #[test]
    fn oversize_brick_axis_is_capped_and_the_budget_rejects_it() {
        let b = detail_brick([0.0; 3], [1.0e9, 1.0e9, 1.0e9], 1e-3);
        assert_eq!(b.counts, [MAX_BRICK_AXIS + 1; 3]);
        assert!(check_probe_budget(None, &[b]).is_err());
        let small = detail_brick([0.0; 3], [3.0, 1.0, 0.0], 1.0);
        assert!(check_probe_budget(None, &[small]).is_ok());
    }
}
