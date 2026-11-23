// SPDX-License-Identifier: MPL-2.0
//! Visibility-buffer instances in the shadow maps (`vis_shadow.wgsl`): per shadow job the
//! casters are culled against the job's light-space region and every cluster takes the LOD cut
//! for the job's texel size; the result is one indirect depth-only draw per job inside the
//! renderer's shadow passes.

use super::{CreateLedger, VisBufferPass, buffer_entry, ensure_capacity, module, storage, whole};
use crate::RenderError;
use crate::device::{TrackedDevice, TrackedQueue};
use crate::frame_work::counted::*;

/// Work lists a frame with swaying instances reserves: four cascades of `SUN_PAGES_PER_AXIS`² pages.
const SWAY_SHADOW_LISTS: u64 = 4 * (crate::paged_shadow::SUN_PAGES_PER_AXIS as u64).pow(2);
/// The cap on that reserve, in work items (8 MiB). Past it a frame has at least the usual eight lists,
/// and what still overflows counts as dropped work items.
const SWAY_SHADOW_WORK_ITEMS: u64 = 1 << 20;

/// Bytes between two jobs' uniforms (the dynamic-offset alignment every adapter accepts).
const JOB_STRIDE: u64 = 256;
/// `jobs` header words (`vis_shadow.wgsl`).
const HEADER_WORDS: u64 = 16;
/// Words per job record (DrawIndirect, work start, work count, cluster base, double-sided
/// cluster base, the double-sided DrawIndirect).
const RECORD_WORDS: u64 = 12;
/// Word of a record where the double-sided clusters' DrawIndirect starts.
const DS_ARGS_IN_RECORD: u64 = 8;
/// Words per job's dispatch arguments: the cluster passes' dispatch and the work-item scan's
/// (`vis_shadow.wgsl` `args_base`, eight words per job so both read from the `args` buffer,
/// which is never bound as storage).
const ARGS_WORDS: u64 = 8;
/// The perspective (local-light) jobs' nearest distance for the LOD test, metres.
pub(crate) const PERSPECTIVE_NEAR: f32 = 0.05;

/// One shadow job as the cluster cull sees it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct VisShadowJob {
    /// The job's view-projection (the whole cascade's for a sun page).
    pub view_proj: [[f32; 4]; 4],
    /// The region of the job in NDC: x0, y0, x1, y1 (a sun page is a part of its cascade).
    pub ndc_rect: [f32; 4],
    /// Texels across the whole view (the cascade or tile edge).
    pub resolution: f32,
    /// Sun cascade index; `None` for a local-light atlas tile (every caster).
    pub cascade: Option<u32>,
}

/// Frame-wide inputs of the shadow cull.
#[derive(Clone, Copy, Debug)]
pub(crate) struct VisShadowFrame {
    /// Eye the GPU cascade masks of persistent sets are taken from: the camera eye snapped to
    /// a grid, so the masks (and the cached shadow pages drawn with them) only change when the
    /// snapped eye does.
    pub camera_eye: [f32; 3],
    /// Added to every GPU-masked caster's reach: the largest distance between the camera and
    /// `camera_eye`, which keeps the cascade choice conservative.
    pub reach_slack: f32,
    /// Far distance of each active sun cascade.
    pub splits: [f32; 4],
    /// Active sun cascades.
    pub cascade_count: u32,
    /// No back-face culling (the scene's double-sided flag, as the forward shadow passes).
    pub double_sided: bool,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct JobParams {
    view_proj: [[f32; 4]; 4],
    planes: [[f32; 4]; 4],
    lod: [f32; 4],
    light: [f32; 4],
    camera: [f32; 4],
    splits: [f32; 4],
    info: [u32; 4],
    caps: [u32; 4],
    /// The scan buffer's regions (`vis_shadow.wgsl`): x = first vec4 of the per-element values,
    /// y = first vec4 of the block sums; the work items' class words come before both.
    scan: [u32; 4],
    pad: [u32; 4],
}

struct ShadowPipes {
    cull_bgl: wgpu::BindGroupLayout,
    raster_bgl: wgpu::BindGroupLayout,
    raster_layout: wgpu::PipelineLayout,
    raster_module: wgpu::ShaderModule,
    reset: wgpu::ComputePipeline,
    job_begin: wgpu::ComputePipeline,
    /// The count pass: every instance's work-item count for the scan.
    job_instances: wgpu::ComputePipeline,
    scan_instances: wgpu::ComputePipeline,
    /// The instance scan's second level; sets the job's work range and the cluster passes'
    /// dispatches.
    scan_top_instances: wgpu::ComputePipeline,
    place_instances: wgpu::ComputePipeline,
    /// The record pass: one workgroup per work item writes its lanes' classes for the scan.
    job_clusters: wgpu::ComputePipeline,
    scan_work_items: wgpu::ComputePipeline,
    /// The cluster scan's second level; sets what each list keeps (the capacity split).
    scan_top_clusters: wgpu::ComputePipeline,
    place_clusters: wgpu::ComputePipeline,
}

/// The shadow cull's resources (inside [`VisBufferPass`]).
#[derive(Default)]
pub(super) struct ShadowRes {
    pipes: Option<ShadowPipes>,
    /// Keyed by (cull none, reversed list: the double-sided clusters).
    raster_pipes: std::collections::HashMap<(bool, bool), wgpu::RenderPipeline>,
    params: Option<(wgpu::Buffer, u64)>,
    jobs: Option<(wgpu::Buffer, u64)>,
    args: Option<(wgpu::Buffer, u64)>,
    work: Option<(wgpu::Buffer, u64)>,
    clusters: Option<(wgpu::Buffer, u64)>,
    /// The prefix-sum scratch: work-item class words, then the scan's per-element values and
    /// its block sums (`vis_shadow.wgsl`).
    scan: Option<(wgpu::Buffer, u64)>,
    /// Bumped whenever a shadow buffer is replaced.
    generation: u64,
    cull_bg: Option<((u64, u64), wgpu::BindGroup)>,
    raster_bg: Option<((u64, u64), wgpu::BindGroup)>,
    /// The pipeline of this frame's jobs (`None`: no cluster shadows this frame).
    current: Option<wgpu::RenderPipeline>,
    /// The cull-none pipeline of this frame's double-sided clusters (`None`: there are none).
    current_ds: Option<wgpu::RenderPipeline>,
    /// Jobs culled this frame.
    jobs_this_frame: usize,
    /// CPU bytes of the job parameters, reused every frame.
    job_bytes: Vec<u8>,
}

impl ShadowRes {
    /// Takes `warmed`'s pipelines, where this holds none (`VisBufferPass::adopt_pipelines`).
    pub(super) fn adopt_pipelines(&mut self, warmed: &mut Self) {
        if self.pipes.is_none() {
            self.pipes = warmed.pipes.take();
            self.raster_pipes = std::mem::take(&mut warmed.raster_pipes);
        }
    }
}

/// Row `i` of a column-major matrix.
fn row(m: &[[f32; 4]; 4], i: usize) -> [f32; 4] {
    [m[0][i], m[1][i], m[2][i], m[3][i]]
}

fn length3(v: [f32; 4]) -> f32 {
    (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt()
}

/// Side planes of the NDC rectangle `rect` under `view_proj` (Gribb & Hartmann's plane
/// extraction, "Fast Extraction of Viewing Frustum Planes from the World-View-Projection
/// Matrix", 2001): `x ≥ x0` is `row0 − x0·row3 ≥ 0`, and so on; normalised so a sphere test
/// reads distances.
pub(crate) fn region_planes(view_proj: &[[f32; 4]; 4], rect: [f32; 4]) -> [[f32; 4]; 4] {
    let (r0, r1, r3) = (row(view_proj, 0), row(view_proj, 1), row(view_proj, 3));
    let combine = |a: [f32; 4], sa: f32, b: [f32; 4], sb: f32| {
        let p = [
            a[0] * sa + b[0] * sb,
            a[1] * sa + b[1] * sb,
            a[2] * sa + b[2] * sb,
            a[3] * sa + b[3] * sb,
        ];
        let len = length3(p).max(1e-20);
        [p[0] / len, p[1] / len, p[2] / len, p[3] / len]
    };
    [
        combine(r0, 1.0, r3, -rect[0]),
        combine(r0, -1.0, r3, rect[2]),
        combine(r1, 1.0, r3, -rect[1]),
        combine(r1, -1.0, r3, rect[3]),
    ]
}

/// LOD inputs of a job: (texels per world unit — at unit distance for a perspective view —,
/// perspective, light position).
pub(crate) fn job_lod(view_proj: &[[f32; 4]; 4], resolution: f32) -> (f32, bool, [f32; 3]) {
    let (r0, r1, r3) = (row(view_proj, 0), row(view_proj, 1), row(view_proj, 3));
    // An orthographic row 0 is `2 / width` along the light's x axis; a perspective one is
    // `cot(fov / 2)` along the view's x axis. Either way `resolution / 2 · |row|` is the texel
    // density (per metre, or per metre at unit distance). The larger axis keeps it conservative.
    let texels = resolution * 0.5 * length3(r0).max(length3(r1));
    let perspective = length3(r3) > 1e-6;
    let mut light = [0.0; 3];
    if perspective {
        // The centre of projection maps to clip (0, 0, z, 0): solve `VP · p = (0, 0, 1, 0)`
        // homogeneously.
        let inv = regia_core::glam::Mat4::from_cols_array_2d(view_proj).inverse();
        let h = inv * regia_core::glam::Vec4::new(0.0, 0.0, 1.0, 0.0);
        if h.w.abs() > 1e-12 {
            light = (h.truncate() / h.w).to_array();
        }
    }
    (texels, perspective, light)
}

impl VisBufferPass {
    pub(super) fn ensure_shadow_pipes(&mut self, d: &TrackedDevice) -> Result<(), RenderError> {
        if self.shadow.pipes.is_some() {
            return Ok(());
        }
        let compute = wgpu::ShaderStages::COMPUTE;
        let job_uniform = |binding: u32, visibility: wgpu::ShaderStages| wgpu::BindGroupLayoutEntry {
            binding,
            visibility,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: true,
                min_binding_size: wgpu::BufferSize::new(std::mem::size_of::<JobParams>() as u64),
            },
            count: None,
        };
        let cull_bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vis shadow cull"),
            entries: &[
                job_uniform(0, compute),
                buffer_entry(1, compute, storage(true)),
                buffer_entry(2, compute, storage(true)),
                buffer_entry(3, compute, storage(false)),
                buffer_entry(4, compute, storage(false)),
                buffer_entry(5, compute, storage(false)),
                buffer_entry(6, compute, storage(false)),
            ],
        });
        let vertex = wgpu::ShaderStages::VERTEX;
        let raster_bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vis shadow raster"),
            entries: &[
                job_uniform(0, vertex),
                buffer_entry(1, vertex, storage(true)),
                buffer_entry(2, vertex, storage(true)),
                buffer_entry(3, vertex, storage(true)),
                buffer_entry(4, vertex, storage(true)),
                buffer_entry(5, vertex, storage(true)),
                buffer_entry(6, vertex, wgpu::BufferBindingType::Uniform),
            ],
        });
        let layout_of = |bgl: &wgpu::BindGroupLayout, label: &str| {
            d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some(label),
                bind_group_layouts: &[Some(bgl)],
                immediate_size: 0,
            })
        };
        let cull_layout = layout_of(&cull_bgl, "vis shadow cull");
        let raster_layout = layout_of(&raster_bgl, "vis shadow raster");
        let cull_module = module(d, "vis_shadow.wgsl", include_str!("vis_shadow.wgsl"))?;
        let raster_module = module(
            d,
            "vis_shadow_raster.wgsl",
            include_str!("vis_shadow_raster.wgsl"),
        )?;
        let pipe = |entry: &str| {
            d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(entry),
                layout: Some(&cull_layout),
                module: &cull_module,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: crate::pipeline_cache::cache_for(&d).as_deref(),
            })
        };
        self.shadow.pipes = Some(ShadowPipes {
            reset: pipe("shadow_reset"),
            job_begin: pipe("job_begin"),
            job_instances: pipe("job_instances"),
            scan_instances: pipe("scan_instances"),
            scan_top_instances: pipe("scan_top_instances"),
            place_instances: pipe("place_instances"),
            job_clusters: pipe("job_clusters"),
            scan_work_items: pipe("scan_work_items"),
            scan_top_clusters: pipe("scan_top_clusters"),
            place_clusters: pipe("place_clusters"),
            cull_bgl,
            raster_bgl,
            raster_layout,
            raster_module,
        });
        Ok(())
    }

    /// `reversed`: the double-sided clusters at the end of the list (`vs_shadow_reversed`).
    pub(super) fn shadow_raster_pipeline(
        &mut self,
        d: &TrackedDevice,
        double_sided: bool,
        reversed: bool,
    ) -> Option<wgpu::RenderPipeline> {
        if let Some(pipe) = self.shadow.raster_pipes.get(&(double_sided, reversed)) {
            return Some(pipe.clone());
        }
        let pipes = self.shadow.pipes.as_ref()?;
        let pipe = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(if reversed {
                "vis shadow raster (double-sided)"
            } else {
                "vis shadow raster"
            }),
            layout: Some(&pipes.raster_layout),
            vertex: wgpu::VertexState {
                module: &pipes.raster_module,
                entry_point: Some(if reversed {
                    "vs_shadow_reversed"
                } else {
                    "vs_shadow"
                }),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: None,
            // The forward shadow pipelines' state: back faces culled unless the scene (or, for
            // the reversed list, the instance) is double-sided, `Less`, no pipeline bias
            // (receivers bias in WGSL).
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: if double_sided {
                    None
                } else {
                    Some(wgpu::Face::Back)
                },
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::Less),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: crate::pipeline_cache::cache_for(&d).as_deref(),
        });
        self.shadow
            .raster_pipes
            .insert((double_sided, reversed), pipe.clone());
        Some(pipe)
    }

    /// Culls the clusters of every shadow job (`jobs[i]` is the renderer's job `i`). After
    /// this, [`Self::draw_shadow_job`] draws job `i` inside the renderer's shadow pass. Needs
    /// [`Self::prepare`] this frame; false (and no draws) when there is nothing to cull.
    #[tracing::instrument(skip_all, name = "visbuffer_shadow_cull")]
    pub(crate) fn record_shadow_cull(
        &mut self,
        device: &wgpu::Device,
        queue: &TrackedQueue,
        encoder: &mut wgpu::CommandEncoder,
        jobs: &[VisShadowJob],
        frame: &VisShadowFrame,
    ) -> Result<bool, RenderError> {
        let before = CreateLedger::mark();
        let out = self.shadow_cull_passes(device, queue, encoder, jobs, frame);
        self.creates.charge(before);
        out
    }

    fn shadow_cull_passes(
        &mut self,
        device: &wgpu::Device,
        queue: &TrackedQueue,
        encoder: &mut wgpu::CommandEncoder,
        jobs: &[VisShadowJob],
        frame: &VisShadowFrame,
    ) -> Result<bool, RenderError> {
        // The jobs' cut feeds the next frame's residency demand (ADR-3708), before the early
        // exits (no jobs: the recorded needs clear).
        self.residency_note_shadow_demand(jobs);
        self.shadow.current = None;
        self.shadow.current_ds = None;
        self.shadow.jobs_this_frame = 0;
        if jobs.is_empty() || !self.prepared || self.instance_count() == 0 {
            return Ok(false);
        }
        let d = &TrackedDevice::observe(device);
        self.ensure_shadow_pipes(d)?;
        let Some(raster) = self.shadow_raster_pipeline(d, frame.double_sided, false) else {
            return Ok(false);
        };
        let raster_ds = if self.any_double_sided() {
            self.shadow_raster_pipeline(d, true, true)
        } else {
            None
        };
        let job_count = jobs.len() as u64;
        let instance_count = self.instance_count() as u32;
        let cap = self.cluster_capacity();
        // Work items: one per 64 clusters of every caster a job keeps. Jobs of one cascade
        // split its casters, so a few lists' worth covers the frame; overflow is counted.
        let chunks = (self.persistent_chunks + self.total_chunks).max(1);
        // The warm's reserve is a floor (ADR-2689), at the most jobs a frame lists: how many
        // shadow jobs a frame has changes with the pages and cascades it redraws.
        let reserve = self.list_reserve().chunks * 8;
        // A swaying plant's grown sphere can reach every page its dynamic class redraws each frame, so
        // "a few lists' worth" does not hold (ADR-3916). The floor does not read the job count or the
        // wind: a scene's work buffer has one size from its first frame, and wind creates nothing.
        let sway_instances = self.sway.has_instances();
        let sway_floor = if sway_instances {
            (chunks * SWAY_SHADOW_LISTS).min(SWAY_SHADOW_WORK_ITEMS)
        } else {
            0
        };
        let work_cap = (chunks * job_count.min(8))
            .max(reserve)
            .max(sway_floor)
            .min(u64::from(u32::MAX / 2));
        // The prefix-sum scratch (`vis_shadow.wgsl`): one vec4 of class words per work item,
        // then one vec4 per scan element (instances, then a job's work items; both never beyond
        // the work-item capacity and the instance count), then one per 256-element block. The
        // region offsets are u32 in the job parameters.
        let max_elems = u64::from(instance_count).max(work_cap);
        let blocks = max_elems.div_ceil(256);
        let scan_vec4s = work_cap + max_elems + blocks;
        let (Some(scan_counts), Some(scan_blocks_at)) = (
            u32::try_from(work_cap).ok(),
            u32::try_from(work_cap + max_elems).ok(),
        ) else {
            return Ok(false);
        };
        let sh = &mut self.shadow;
        let usage_uniform = wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST;
        // The first gust sends a plant's pages to the dynamic class, so a frame with sway holds room for
        // them from its first frame: wind creates nothing (ADR-3916).
        let room_jobs = if sway_instances {
            job_count.max(SWAY_SHADOW_LISTS)
        } else {
            job_count
        };
        let mut replaced = ensure_capacity(
            d,
            &mut sh.params,
            "vis shadow jobs",
            room_jobs * JOB_STRIDE,
            usage_uniform,
        );
        let jobs_words = HEADER_WORDS + room_jobs * (RECORD_WORDS + ARGS_WORDS);
        replaced |= ensure_capacity(
            d,
            &mut sh.jobs,
            "vis shadow job records",
            jobs_words * 4,
            wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::INDIRECT | wgpu::BufferUsages::COPY_SRC,
        );
        replaced |= ensure_capacity(
            d,
            &mut sh.args,
            "vis shadow dispatch args",
            room_jobs * ARGS_WORDS * 4,
            wgpu::BufferUsages::INDIRECT | wgpu::BufferUsages::COPY_DST,
        );
        replaced |= ensure_capacity(
            d,
            &mut sh.work,
            "vis shadow work",
            work_cap * 8,
            // COPY_SRC: the read_shadow_lists test hook reads the buffer back.
            wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        );
        replaced |= ensure_capacity(
            d,
            &mut sh.clusters,
            "vis shadow clusters",
            u64::from(cap) * 8,
            // COPY_SRC: the read_shadow_lists test hook reads the buffer back.
            wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        );
        replaced |= ensure_capacity(
            d,
            &mut sh.scan,
            "vis shadow scan",
            scan_vec4s * 16,
            wgpu::BufferUsages::STORAGE,
        );
        if replaced {
            sh.generation += 1;
        }
        // The record capacity the shader indexes dispatch arguments with: the buffer's.
        let job_capacity = sh.jobs.as_ref().map_or(0, |(_, bytes)| {
            (bytes / 4 - HEADER_WORDS) / (RECORD_WORDS + ARGS_WORDS)
        });
        let (
            Some(pipes),
            Some((params, _)),
            Some((jobs_buf, _)),
            Some((args, _)),
            Some((work, _)),
            Some((clusters, _)),
            Some((scan, _)),
            Some((instances, _)),
            Some((meshlets, _)),
            Some((geo, _)),
            Some(sway),
        ) = (
            sh.pipes.as_ref(),
            &sh.params,
            &sh.jobs,
            &sh.args,
            &sh.work,
            &sh.clusters,
            &sh.scan,
            &self.buffers.instances,
            &self.buffers.meshlets,
            &self.buffers.geo,
            self.sway.buffer(),
        )
        else {
            return Ok(false);
        };
        let key = (self.generation, sh.generation);
        let uniform = |binding: u32| wgpu::BindGroupEntry {
            binding,
            resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                buffer: params,
                offset: 0,
                size: wgpu::BufferSize::new(std::mem::size_of::<JobParams>() as u64),
            }),
        };
        if sh.cull_bg.as_ref().is_none_or(|(k, _)| *k != key) {
            let bg = d.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("vis shadow cull"),
                layout: &pipes.cull_bgl,
                entries: &[
                    uniform(0),
                    whole(1, instances),
                    whole(2, meshlets),
                    whole(3, work),
                    whole(4, jobs_buf),
                    whole(5, clusters),
                    whole(6, scan),
                ],
            });
            sh.cull_bg = Some((key, bg));
        }
        if sh.raster_bg.as_ref().is_none_or(|(k, _)| *k != key) {
            let bg = d.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("vis shadow raster"),
                layout: &pipes.raster_bgl,
                entries: &[
                    uniform(0),
                    whole(1, instances),
                    whole(2, meshlets),
                    whole(3, geo),
                    whole(4, clusters),
                    whole(5, jobs_buf),
                    whole(6, sway),
                ],
            });
            sh.raster_bg = Some((key, bg));
        }
        let Some((_, cull_bg)) = sh.cull_bg.as_ref() else {
            return Ok(false);
        };

        let threshold = self.settings.shadow_lod_threshold_px.max(0.0);
        let sway_moving = self.sway.moving();
        // The job parameters' CPU bytes are kept between frames (no allocation per frame).
        let mut bytes = std::mem::take(&mut sh.job_bytes);
        bytes.clear();
        bytes.resize((job_count * JOB_STRIDE) as usize, 0);
        for (i, job) in jobs.iter().enumerate() {
            let (texels, perspective, light) = job_lod(&job.view_proj, job.resolution);
            let params = JobParams {
                view_proj: job.view_proj,
                planes: region_planes(&job.view_proj, job.ndc_rect),
                lod: [
                    texels,
                    threshold,
                    if perspective { 1.0 } else { 0.0 },
                    PERSPECTIVE_NEAR,
                ],
                light: [light[0], light[1], light[2], frame.reach_slack],
                camera: [
                    frame.camera_eye[0],
                    frame.camera_eye[1],
                    frame.camera_eye[2],
                    frame.cascade_count as f32,
                ],
                splits: frame.splits,
                info: [
                    i as u32,
                    job.cascade.map_or(255, |c| c.min(7)),
                    instance_count,
                    cap,
                ],
                caps: [work_cap as u32, job_capacity as u32, u32::from(sway_moving), 0],
                scan: [scan_counts, scan_blocks_at, 0, 0],
                pad: [0; 4],
            };
            let at = i * JOB_STRIDE as usize;
            bytes[at..at + std::mem::size_of::<JobParams>()].copy_from_slice(bytemuck::bytes_of(&params));
        }
        queue.write_buffer(params, 0, &bytes);
        sh.job_bytes = bytes;

        let groups = instance_count.div_ceil(64);
        let (gx, gy) = (groups.min(65535), groups.div_ceil(65535).max(1));
        let scan_groups = instance_count.div_ceil(256);
        let (sx, sy) = (scan_groups.min(65535), scan_groups.div_ceil(65535).max(1));
        let offset = |i: usize| (i as u64 * JOB_STRIDE) as u32;
        {
            let mut pass = encoder.counted_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("vis shadow instances"),
                timestamp_writes: None,
            });
            pass.counted_set_bind_group(0, cull_bg, &[0]);
            pass.counted_set_pipeline(&pipes.reset);
            pass.counted_dispatch(1, 1, 1);
            for i in 0..jobs.len() {
                pass.counted_set_bind_group(0, cull_bg, &[offset(i)]);
                pass.counted_set_pipeline(&pipes.job_begin);
                pass.counted_dispatch(1, 1, 1);
                pass.counted_set_pipeline(&pipes.job_instances);
                pass.counted_dispatch(gx, gy, 1);
                pass.counted_set_pipeline(&pipes.scan_instances);
                pass.counted_dispatch(sx, sy, 1);
                pass.counted_set_pipeline(&pipes.scan_top_instances);
                pass.counted_dispatch(1, 1, 1);
                pass.counted_set_pipeline(&pipes.place_instances);
                pass.counted_dispatch(gx, gy, 1);
            }
        }
        let args_at = (HEADER_WORDS + job_capacity * RECORD_WORDS) * 4;
        encoder.copy_buffer_to_buffer(jobs_buf, args_at, args, 0, job_count * ARGS_WORDS * 4);
        {
            let mut pass = encoder.counted_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("vis shadow clusters"),
                timestamp_writes: None,
            });
            for i in 0..jobs.len() {
                pass.counted_set_bind_group(0, cull_bg, &[offset(i)]);
                pass.counted_set_pipeline(&pipes.job_clusters);
                pass.counted_dispatch_indirect(args, i as u64 * ARGS_WORDS * 4);
                pass.counted_set_pipeline(&pipes.scan_work_items);
                pass.counted_dispatch_indirect(args, i as u64 * ARGS_WORDS * 4 + 16);
                pass.counted_set_pipeline(&pipes.scan_top_clusters);
                pass.counted_dispatch(1, 1, 1);
                pass.counted_set_pipeline(&pipes.place_clusters);
                pass.counted_dispatch_indirect(args, i as u64 * ARGS_WORDS * 4);
            }
        }
        self.shadow.current = Some(raster);
        self.shadow.current_ds = raster_ds;
        self.shadow.jobs_this_frame = jobs.len();
        Ok(true)
    }

    /// Draws shadow job `job`'s clusters (after [`Self::record_shadow_cull`]) into the bound
    /// depth target, with the pass's current viewport and scissor (the double-sided clusters with
    /// the cull-none pipeline). Leaves one of its pipelines and its group 0 bound.
    pub(crate) fn draw_shadow_job(&self, pass: &mut wgpu::RenderPass<'_>, job: usize) {
        let (Some(pipe), Some((_, bg)), Some((jobs_buf, _))) = (
            self.shadow.current.as_ref(),
            self.shadow.raster_bg.as_ref(),
            self.shadow.jobs.as_ref(),
        ) else {
            return;
        };
        if job >= self.shadow.jobs_this_frame {
            return;
        }
        pass.counted_set_pipeline(pipe);
        pass.counted_set_bind_group(0, bg, &[(job as u64 * JOB_STRIDE) as u32]);
        let record = HEADER_WORDS + job as u64 * RECORD_WORDS;
        pass.counted_draw_indirect(jobs_buf, record * 4);
        if let Some(pipe) = self.shadow.current_ds.as_ref() {
            pass.counted_set_pipeline(pipe);
            pass.counted_draw_indirect(jobs_buf, (record + DS_ARGS_IN_RECORD) * 4);
        }
    }

    /// Reads the shadow cull's header counters back (work items, clusters, dropped clusters,
    /// dropped work items). Blocks on the GPU: tests and benches only.
    ///
    /// # Errors
    ///
    /// The poll or the map failed.
    pub(crate) fn read_shadow_counters(
        &self,
        device: &crate::RenderDevice,
    ) -> Result<Option<[u32; 4]>, RenderError> {
        let Some((jobs, _)) = self.shadow.jobs.as_ref() else {
            return Ok(None);
        };
        let words = super::read_words(device, jobs, 4)?;
        let word = |i: usize| words.get(i).copied().unwrap_or(0);
        Ok(Some([word(0), word(1), word(2), word(3)]))
    }
}

/// The last frame's shadow cull output (test hook, [`VisBufferPass::read_shadow_lists`]).
#[cfg(test)]
pub(crate) struct ShadowLists {
    /// `jobs` header words (`vis_shadow.wgsl` lists the layout): [0] work items and [1]
    /// clusters admitted over all jobs, [2] clusters and [3] work items dropped at capacity.
    pub(crate) header: [u32; 4],
    /// Every job record of the last frame (`jobs_this_frame` times 12 words): the single and
    /// double-sided DrawIndirect counts at 1 and 9, this job's work start and count at 4 and 5,
    /// its single and double-sided cluster bases at 6 and 7.
    pub(crate) records: Vec<u32>,
    /// The whole work-item buffer, two words per item (instance, first cluster).
    pub(crate) work: Vec<u32>,
    /// The whole cluster buffer, two words per entry (instance, meshlet).
    pub(crate) clusters: Vec<u32>,
    /// `VisBufferSettings.max_clusters`, the cluster list capacity.
    pub(crate) capacity: u32,
}

#[cfg(test)]
impl VisBufferPass {
    /// Test hook: reads the shadow cull's job records and its work-item and cluster lists
    /// back. Blocks on the GPU: tests only. `None` before the first frame with cluster shadows.
    ///
    /// # Errors
    ///
    /// The poll or the map failed.
    pub(crate) fn read_shadow_lists(
        &self,
        device: &crate::RenderDevice,
    ) -> Result<Option<ShadowLists>, RenderError> {
        let (Some((jobs, jobs_bytes)), Some((work, work_bytes)), Some((clusters, clusters_bytes))) = (
            self.shadow.jobs.as_ref(),
            self.shadow.work.as_ref(),
            self.shadow.clusters.as_ref(),
        ) else {
            return Ok(None);
        };
        let words = super::read_words(device, jobs, jobs_bytes / 4)?;
        let header = [
            words.first().copied().unwrap_or(0),
            words.get(1).copied().unwrap_or(0),
            words.get(2).copied().unwrap_or(0),
            words.get(3).copied().unwrap_or(0),
        ];
        let records = HEADER_WORDS as usize + self.shadow.jobs_this_frame * RECORD_WORDS as usize;
        Ok(Some(ShadowLists {
            header,
            records: words.get(HEADER_WORDS as usize..records).unwrap_or(&[]).to_vec(),
            work: super::read_words(device, work, work_bytes / 4)?,
            clusters: super::read_words(device, clusters, clusters_bytes / 4)?,
            capacity: self.cluster_capacity(),
        }))
    }
}

/// Rust and WGSL struct rows of this module (`shader_contract`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<crate::ContractRow> {
    vec![
        crate::contract_row!(
            "vis_shadow.wgsl,vis_shadow_raster.wgsl",
            "ShadowJob",
            JobParams {
                view_proj,
                planes,
                lod,
                light,
                camera,
                splits,
                info,
                caps
            }
        )
        .partial()
        .host_may_extend(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::glam::camera::rh::proj::directx::{orthographic, perspective};
    use regia_core::glam::camera::rh::view::look_at_mat4;

    #[test]
    fn job_params_fit_the_stride() {
        assert_eq!(std::mem::size_of::<JobParams>(), 256);
        assert_eq!(std::mem::size_of::<super::super::GpuInstance>(), 144);
    }

    #[test]
    fn region_planes_bound_an_orthographic_page() {
        // Orthographic view of x, y in [-10, 10]: the right half (NDC x in [0, 1]).
        let vp = orthographic(-10.0, 10.0, -10.0, 10.0, 0.1, 100.0).to_cols_array_2d();
        let planes = region_planes(&vp, [0.0, -1.0, 1.0, 1.0]);
        let inside = |p: [f32; 3], r: f32| {
            planes
                .iter()
                .all(|pl| pl[0] * p[0] + pl[1] * p[1] + pl[2] * p[2] + pl[3] >= -r)
        };
        assert!(inside([5.0, 0.0, -5.0], 0.0));
        assert!(!inside([-5.0, 0.0, -5.0], 1.0));
        // A sphere straddling the page edge is kept.
        assert!(inside([-0.5, 0.0, -5.0], 1.0));
        assert!(!inside([5.0, 11.0, -5.0], 0.5));
    }

    #[test]
    fn job_lod_reads_texel_density_and_light_position() {
        // 20 m wide orthographic cascade at 2048 texels: 102.4 texels per metre.
        let ortho = orthographic(-10.0, 10.0, -10.0, 10.0, 0.1, 100.0);
        let (texels, is_perspective, _) = job_lod(&ortho.to_cols_array_2d(), 2048.0);
        assert!(!is_perspective);
        assert!((texels - 102.4).abs() < 1e-2, "{texels}");
        // 90° spot at (3, 4, 5), 512 texels: 256 texels per metre at unit distance.
        let eye = regia_core::glam::Vec3::new(3.0, 4.0, 5.0);
        let view = look_at_mat4(eye, regia_core::glam::Vec3::ZERO, regia_core::glam::Vec3::Y);
        let proj = perspective(std::f32::consts::FRAC_PI_2, 1.0, 0.1, 50.0);
        let (texels, is_perspective, light) = job_lod(&(proj * view).to_cols_array_2d(), 512.0);
        assert!(is_perspective);
        assert!((texels - 256.0).abs() < 0.5, "{texels}");
        assert!(
            (regia_core::glam::Vec3::from_array(light) - eye).length() < 1e-3,
            "{light:?}"
        );
    }
}
