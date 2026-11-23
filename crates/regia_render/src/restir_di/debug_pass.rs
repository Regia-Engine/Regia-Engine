// SPDX-License-Identifier: MPL-2.0
//! The evaluation path of the many-light sampler on real frames (ADR-2470): a
//! [`crate::FramePass`] at [`crate::GraphSlot::AfterOpaque`] that rebuilds a G-buffer from the
//! scene depth (normals from neighboring depths, one flat material, camera motion), runs
//! [`RestirDiGpu`] on the lights of a [`ManyLightFeed`] with clustered candidates built on the
//! CPU, and writes the estimate (or the brute-force reference) over the view's HDR image.
//!
//! It replaces what the frame drew in the view rectangle: it is a look at the sampler, not
//! lighting. The production hookup into the forward pass or the visibility resolve is in
//! ADR-2470. With the pass not registered (the default) nothing of it runs.

use crate::frame_work::counted::*;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use regia_core::glam::Vec3;

use super::cpu::ClusterLists;
use super::depth_gbuffer::params_entry;
use super::{
    DepthGBuffer, DepthGBufferInputs, ManyLight, RestirCamera, RestirClusters, RestirDiGpu, RestirDiSettings,
    RestirInputs,
};
use crate::{FramePass, FramePassContext, GraphSlot, TrackedDevice};

/// The pass's name in the frame graph (`render.graph`).
pub const RESTIR_DI_PASS_NAME: &str = "restir_di_debug";
/// Cluster slots of the evaluation path's lists.
const DEBUG_MAX_PER_CLUSTER: u32 = 256;
const COMPOSITE: &str = include_str!("../../shaders/restir_di_composite.wgsl");

/// `render.set_many_light_sampling` modes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ManyLightMode {
    /// Nothing runs (the default).
    #[default]
    Off,
    /// The evaluation pass draws over the frame.
    Debug,
    /// The frame's opaque forward pass shades the sampled light (ADR-2652): the profile's
    /// [`crate::RenderProfile::many_light_sampling`] is on.
    Production,
}

impl ManyLightMode {
    /// `"off"`, `"debug"` or `"production"`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Debug => "debug",
            Self::Production => "production",
        }
    }

    /// The mode named `name`.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "off" => Some(Self::Off),
            "debug" => Some(Self::Debug),
            "production" => Some(Self::Production),
            _ => None,
        }
    }
}

/// What the evaluation pass shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RestirDebugView {
    /// The resampled estimate (one light per pixel).
    #[default]
    Estimate,
    /// The brute-force sum over each pixel's cluster list.
    Reference,
}

impl RestirDebugView {
    /// `"estimate"` or `"reference"`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Estimate => "estimate",
            Self::Reference => "reference",
        }
    }

    /// The view named `name`.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "estimate" => Some(Self::Estimate),
            "reference" => Some(Self::Reference),
            _ => None,
        }
    }
}

#[derive(Debug)]
struct FeedState {
    lights: Vec<ManyLight>,
    generation: u64,
    view: RestirDebugView,
    settings: RestirDiSettings,
    settings_generation: u64,
    material: [f32; 4],
}

/// What the evaluation pass reads each frame, shared with the host that fills it (the agent
/// method snapshots the world's lights into it).
#[derive(Clone, Debug)]
pub struct ManyLightFeed(Arc<Mutex<FeedState>>);

impl Default for ManyLightFeed {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(FeedState {
            lights: Vec::new(),
            generation: 0,
            view: RestirDebugView::Estimate,
            settings: RestirDiSettings::default(),
            settings_generation: 0,
            material: [0.8, 0.8, 0.8, 0.6],
        })))
    }
}

impl ManyLightFeed {
    fn lock(&self) -> MutexGuard<'_, FeedState> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Replaces the lights (uploaded on the next frame).
    pub fn set_lights(&self, lights: Vec<ManyLight>) {
        let mut s = self.lock();
        s.lights = lights;
        s.generation += 1;
    }

    /// Lights held.
    #[must_use]
    pub fn light_count(&self) -> usize {
        self.lock().lights.len()
    }

    /// Sets what the pass shows.
    pub fn set_view(&self, view: RestirDebugView) {
        self.lock().view = view;
    }

    /// What the pass shows.
    #[must_use]
    pub fn view(&self) -> RestirDebugView {
        self.lock().view
    }

    /// Replaces the sampler settings (the history restarts).
    ///
    /// # Errors
    /// [`super::RestirDiError::InvalidSetting`]; nothing changes.
    pub fn set_settings(&self, settings: RestirDiSettings) -> Result<(), super::RestirDiError> {
        settings.validate()?;
        let mut s = self.lock();
        s.settings = settings;
        s.settings_generation += 1;
        Ok(())
    }

    /// The sampler settings.
    #[must_use]
    pub fn settings(&self) -> RestirDiSettings {
        self.lock().settings.clone()
    }

    /// Sets the flat material of the rebuilt G-buffer (albedo, roughness).
    pub fn set_material(&self, albedo: [f32; 3], roughness: f32) {
        self.lock().material = [albedo[0], albedo[1], albedo[2], roughness.clamp(0.0, 1.0)];
    }
}

/// Process-wide counters of the evaluation pass (`render.many_light_stats`).
#[derive(Debug)]
pub struct RestirDiCounters {
    /// Frames the pass recorded.
    pub frames_recorded: AtomicU64,
    /// Frames it skipped (multisampled depth, an empty view rectangle).
    pub frames_skipped: AtomicU64,
    /// Light-buffer uploads (one per new light set).
    pub light_uploads: AtomicU64,
    /// CPU cluster-list builds (camera or lights changed).
    pub cluster_builds: AtomicU64,
    /// Lights in the last frame.
    pub last_lights: AtomicU64,
    /// Width of the last frame's sampler input.
    pub last_width: AtomicU64,
    /// Height of the last frame's sampler input.
    pub last_height: AtomicU64,
    /// Largest cluster list of the last build.
    pub last_cluster_max: AtomicU64,
    /// Clusters that dropped lights in the last build.
    pub last_cluster_overflowed: AtomicU64,
    /// Times the sampler's per-size resources were built.
    pub resource_builds: AtomicU64,
}

static COUNTERS: RestirDiCounters = RestirDiCounters {
    frames_recorded: AtomicU64::new(0),
    frames_skipped: AtomicU64::new(0),
    light_uploads: AtomicU64::new(0),
    cluster_builds: AtomicU64::new(0),
    last_lights: AtomicU64::new(0),
    last_width: AtomicU64::new(0),
    last_height: AtomicU64::new(0),
    last_cluster_max: AtomicU64::new(0),
    last_cluster_overflowed: AtomicU64::new(0),
    resource_builds: AtomicU64::new(0),
};

/// The evaluation pass's counters.
#[must_use]
pub fn restir_di_counters() -> &'static RestirDiCounters {
    &COUNTERS
}

struct Composite {
    layout: wgpu::BindGroupLayout,
    pipeline_layout: wgpu::PipelineLayout,
    module: wgpu::ShaderModule,
    pipelines: Vec<(wgpu::TextureFormat, wgpu::RenderPipeline)>,
    /// Keyed by the sampler output and the G-buffer stage's parameter block.
    group: Option<(wgpu::TextureView, wgpu::BindGroup)>,
}

/// The evaluation pass (see the module docs). Register it with
/// [`crate::FrameRenderer::register_pass`]; remove it by [`RESTIR_DI_PASS_NAME`].
pub struct RestirDiDebugPass {
    feed: ManyLightFeed,
    gbuffer: Option<DepthGBuffer>,
    composite: Option<Composite>,
    sampler: Option<RestirDiGpu>,
    settings_generation: u64,
    lights: Option<(wgpu::Buffer, u64)>,
    light_count: u32,
    uploaded_generation: Option<u64>,
    clusters: ClusterLists,
    cluster_buffers: Option<(wgpu::Buffer, wgpu::Buffer)>,
    cluster_key: Option<([f32; 16], u64)>,
}

impl RestirDiDebugPass {
    /// A pass reading `feed`.
    #[must_use]
    pub fn new(feed: ManyLightFeed) -> Self {
        Self {
            feed,
            gbuffer: None,
            composite: None,
            sampler: None,
            settings_generation: u64::MAX,
            lights: None,
            light_count: 0,
            uploaded_generation: None,
            clusters: ClusterLists::default(),
            cluster_buffers: None,
            cluster_key: None,
        }
    }

    /// The feed it reads.
    #[must_use]
    pub fn feed(&self) -> &ManyLightFeed {
        &self.feed
    }

    fn ensure_pipelines(&mut self, device: &TrackedDevice, format: wgpu::TextureFormat) {
        if self.gbuffer.is_none() {
            self.gbuffer = Some(DepthGBuffer::new(device));
        }
        let c = self.composite.get_or_insert_with(|| build_composite(device));
        if !c.pipelines.iter().any(|(f, _)| *f == format) {
            let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("restir di composite"),
                layout: Some(&c.pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &c.module,
                    entry_point: Some("composite_vs"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &c.module,
                    entry_point: Some("composite_fs"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: crate::pipeline_cache::cache_for(device).as_deref(),
            });
            c.pipelines.push((format, pipeline));
        }
    }

    fn ensure_sampler(&mut self, device: &TrackedDevice) -> Result<(), String> {
        let generation = self.feed.lock().settings_generation;
        if self.sampler.is_some() && self.settings_generation == generation {
            return Ok(());
        }
        let settings = self.feed.settings();
        match self.sampler.as_mut() {
            Some(sampler) => sampler.set_settings(settings).map_err(|e| e.to_string())?,
            None => {
                self.sampler = Some(RestirDiGpu::new(device, settings, 0x5eed).map_err(|e| e.to_string())?)
            }
        }
        self.settings_generation = generation;
        Ok(())
    }

    /// Uploads a new light set and rebuilds the cluster lists when the camera or the lights
    /// changed. Returns the view and the flat material to use.
    fn update_lights(
        &mut self,
        ctx: &FramePassContext<'_>,
        camera: &RestirCamera,
        size: (u32, u32),
    ) -> (RestirDebugView, [f32; 4]) {
        let device = ctx.device;
        let counters = restir_di_counters();
        let state = self.feed.lock();
        if self.uploaded_generation != Some(state.generation) {
            let n = state.lights.len() as u64;
            let need = n.max(1) * super::MANY_LIGHT_BYTES as u64;
            if self.lights.as_ref().is_none_or(|(_, cap)| *cap < need) {
                let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("restir di lights"),
                    size: need,
                    usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                self.lights = Some((buffer, need));
            }
            if let Some((buffer, _)) = &self.lights
                && n > 0
            {
                ctx.write_buffer(buffer, 0, bytemuck::cast_slice(&state.lights));
            }
            self.light_count = n as u32;
            self.uploaded_generation = Some(state.generation);
            self.cluster_key = None;
            counters.light_uploads.fetch_add(1, Ordering::Relaxed);
        }
        let key = (camera.clip_from_world.to_cols_array(), state.generation);
        if self.cluster_key != Some(key) {
            let aspect = size.0 as f32 / size.1 as f32;
            self.clusters
                .rebuild(camera, aspect, &state.lights, DEBUG_MAX_PER_CLUSTER);
            let counts = bytemuck::cast_slice::<u32, u8>(&self.clusters.counts);
            let indices = bytemuck::cast_slice::<u32, u8>(&self.clusters.indices);
            if self.cluster_buffers.is_none() {
                let make = |label: &str, bytes: usize| {
                    device.create_buffer(&wgpu::BufferDescriptor {
                        label: Some(label),
                        size: bytes as u64,
                        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                        mapped_at_creation: false,
                    })
                };
                self.cluster_buffers = Some((
                    make("restir di cluster counts", counts.len()),
                    make("restir di cluster indices", indices.len()),
                ));
            }
            if let Some((c, i)) = &self.cluster_buffers {
                ctx.write_buffer(c, 0, counts);
                ctx.write_buffer(i, 0, indices);
            }
            self.cluster_key = Some(key);
            counters.cluster_builds.fetch_add(1, Ordering::Relaxed);
            let (_, max) = self.clusters.occupancy();
            counters.last_cluster_max.store(u64::from(max), Ordering::Relaxed);
            counters
                .last_cluster_overflowed
                .store(u64::from(self.clusters.overflowed), Ordering::Relaxed);
        }
        (state.view, state.material)
    }
}

fn build_composite(device: &TrackedDevice) -> Composite {
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("restir di composite"),
        source: crate::wgsl_source(COMPOSITE),
    });
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("restir di composite bgl"),
        entries: &[
            params_entry(0, wgpu::ShaderStages::FRAGMENT),
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
        ],
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("restir di composite"),
        bind_group_layouts: &[Some(&layout)],
        immediate_size: 0,
    });
    Composite {
        layout,
        pipeline_layout,
        module,
        pipelines: Vec::new(),
        group: None,
    }
}

fn camera_of(ctx: &FramePassContext<'_>) -> RestirCamera {
    let view_from_world = ctx.clip_from_view.inverse() * ctx.clip_from_world;
    let world_from_view = view_from_world.inverse();
    let forward = world_from_view
        .transform_vector3(Vec3::NEG_Z)
        .normalize_or(Vec3::NEG_Z);
    let p = ctx.clip_from_view;
    let fov_y = 2.0 * (1.0 / p.y_axis.y.max(1e-6)).atan();
    // Standard depth (`Mat4::perspective_rh`): z.z = f / (n − f), w.z = n·f / (n − f).
    let (zz, wz) = (p.z_axis.z, p.w_axis.z);
    let near = if zz.abs() > 1e-9 { (wz / zz).abs() } else { 0.1 };
    let far = if (zz + 1.0).abs() > 1e-9 {
        (wz / (zz + 1.0)).abs().min(1e5)
    } else {
        1e5
    };
    RestirCamera {
        clip_from_world: ctx.clip_from_world,
        view_from_world,
        eye: ctx.eye,
        forward: forward.to_array(),
        fov_y,
        near: near.max(1e-3),
        far: far.max(near + 1e-3),
    }
}

impl FramePass for RestirDiDebugPass {
    fn name(&self) -> &str {
        RESTIR_DI_PASS_NAME
    }

    fn slot(&self) -> GraphSlot {
        GraphSlot::AfterOpaque
    }

    fn reads(&self) -> &[&'static str] {
        &["depth"]
    }

    fn writes(&self) -> &[&'static str] {
        &["color"]
    }

    fn pipelines(&self) -> &[&'static str] {
        &["restir_di"]
    }

    fn prewarm(&mut self, device: &TrackedDevice) -> Result<usize, String> {
        self.ensure_pipelines(device, wgpu::TextureFormat::Rgba16Float);
        self.ensure_sampler(device)?;
        Ok(6)
    }

    fn record(&mut self, ctx: &mut FramePassContext<'_>) -> Result<(), String> {
        let _span = tracing::trace_span!("restir_di_debug").entered();
        let counters = restir_di_counters();
        if ctx.depth_multisampled {
            counters.frames_skipped.fetch_add(1, Ordering::Relaxed);
            return Err("the evaluation path needs single-sampled depth".into());
        }
        let size = (
            ctx.viewport[2].round().max(0.0) as u32,
            ctx.viewport[3].round().max(0.0) as u32,
        );
        if size.0 == 0 || size.1 == 0 {
            counters.frames_skipped.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        let device = ctx.device;
        self.ensure_pipelines(device, ctx.color_format);
        self.ensure_sampler(device)?;
        let camera = camera_of(ctx);
        let (view, material) = self.update_lights(ctx, &camera, size);
        let (Some(gbuffer), Some(composite), Some(sampler), Some((lights, _)), Some((counts, indices))) = (
            self.gbuffer.as_mut(),
            self.composite.as_mut(),
            self.sampler.as_mut(),
            self.lights.as_ref(),
            self.cluster_buffers.as_ref(),
        ) else {
            return Err("evaluation resources missing".into());
        };
        gbuffer
            .prepare(
                device,
                &DepthGBufferInputs {
                    depth: ctx.depth,
                    viewport: ctx.viewport,
                    color_size: ctx.color_size,
                    clip_from_world: ctx.clip_from_world,
                    eye: ctx.eye,
                    forward: camera.forward,
                    material,
                    pre_exposure: ctx.pre_exposure,
                },
            )
            .map_err(|e| e.to_string())?;
        let (buffer, bytes) = gbuffer.params_upload();
        ctx.write_buffer(buffer, 0, bytes);
        let Some(views) = gbuffer.views() else {
            return Err("G-buffer missing".into());
        };
        let inputs = RestirInputs {
            size,
            depth: &views[0],
            normal_roughness: &views[1],
            albedo_metallic: &views[2],
            motion: &views[3],
            lights,
            light_count: self.light_count,
            clusters: Some(RestirClusters {
                counts,
                indices,
                max_per_cluster: DEBUG_MAX_PER_CLUSTER,
            }),
            camera,
            reference: view == RestirDebugView::Reference,
        };
        let builds = sampler.resource_builds();
        sampler.prepare(device, &inputs).map_err(|e| e.to_string())?;
        if sampler.resource_builds() != builds {
            counters.resource_builds.fetch_add(1, Ordering::Relaxed);
        }
        let (buffer, bytes) = sampler.params_upload();
        ctx.write_buffer(buffer, 0, bytes);
        let Some(output) = sampler.output_view() else {
            return Err("sampler output missing".into());
        };
        if composite.group.as_ref().is_none_or(|(v, _)| v != output) {
            let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("restir di composite"),
                layout: &composite.layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: gbuffer.params_buffer().as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(output),
                    },
                ],
            });
            composite.group = Some((output.clone(), group));
        }
        let (Some(pipeline), Some((_, group))) = (
            composite
                .pipelines
                .iter()
                .find(|(f, _)| *f == ctx.color_format)
                .map(|(_, p)| p),
            composite.group.as_ref(),
        ) else {
            return Err("composite missing".into());
        };
        gbuffer.encode(ctx.encoder);
        sampler.encode(ctx.encoder);
        {
            let mut pass = ctx.encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("restir di composite"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: ctx.color,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            let [x, y, w, h] = ctx.viewport;
            pass.set_viewport(x, y, w, h, 0.0, 1.0);
            pass.counted_set_pipeline(pipeline);
            pass.counted_set_bind_group(0, group, &[]);
            pass.counted_draw(0..3, 0..1);
        }
        counters.frames_recorded.fetch_add(1, Ordering::Relaxed);
        counters
            .last_lights
            .store(u64::from(self.light_count), Ordering::Relaxed);
        counters.last_width.store(u64::from(size.0), Ordering::Relaxed);
        counters.last_height.store(u64::from(size.1), Ordering::Relaxed);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn validate(source: &str) {
        let module = match naga::front::wgsl::parse_str(source) {
            Ok(m) => m,
            Err(e) => panic!("{}", e.emit_to_string(source)),
        };
        let mut v = naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::default(),
        );
        if let Err(e) = v.validate(&module) {
            panic!("{}", e.emit_to_string(source));
        }
    }

    #[test]
    fn the_evaluation_shaders_validate() {
        validate(include_str!("../../shaders/restir_di_debug.wgsl"));
        validate(COMPOSITE);
    }

    #[test]
    fn modes_and_views_round_trip() {
        for m in [ManyLightMode::Off, ManyLightMode::Debug] {
            assert_eq!(ManyLightMode::parse(m.as_str()), Some(m));
        }
        for v in [RestirDebugView::Estimate, RestirDebugView::Reference] {
            assert_eq!(RestirDebugView::parse(v.as_str()), Some(v));
        }
        assert_eq!(ManyLightMode::parse("on"), None);
        assert_eq!(ManyLightMode::default(), ManyLightMode::Off);
    }

    #[test]
    fn the_feed_validates_settings_and_counts_lights() {
        let feed = ManyLightFeed::default();
        assert_eq!(feed.light_count(), 0);
        feed.set_lights(vec![ManyLight::point([0.0; 3], [1.0; 3], 100.0, 3.0, 0.0); 3]);
        assert_eq!(feed.light_count(), 3);
        let bad = RestirDiSettings {
            initial_candidates: 0,
            ..RestirDiSettings::default()
        };
        assert!(feed.set_settings(bad).is_err());
        assert_eq!(feed.settings(), RestirDiSettings::default());
    }

    #[test]
    fn the_camera_is_recovered_from_the_pass_matrices() {
        let cam = RestirCamera::look_at([1.0, 2.0, 3.0], [0.0, 0.0, -5.0], 0.9, 1.5, 0.2, 300.0);
        let proj = regia_core::glam::camera::rh::proj::directx::perspective(0.9, 1.5, 0.2, 300.0);
        let fov = 2.0 * (1.0 / proj.y_axis.y).atan();
        assert!((fov - 0.9).abs() < 1e-4);
        let (zz, wz) = (proj.z_axis.z, proj.w_axis.z);
        assert!(((wz / zz).abs() - 0.2).abs() < 1e-4);
        assert!(((wz / (zz + 1.0)).abs() - 300.0).abs() < 0.5);
        assert!((Vec3::from(cam.forward).length() - 1.0).abs() < 1e-5);
    }
}
