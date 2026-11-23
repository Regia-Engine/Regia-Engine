// SPDX-License-Identifier: MPL-2.0
//! Stereo rendering for the XR frame loop (spec 58 § 2.6, ADR-1918).
//!
//! [`XrRenderPlugin`] adds [`xr_render_frame`] to the `Render` schedule, after the extraction and
//! the texture views and before the window: it runs the XR frame there (`wait`, `begin`, locate,
//! render, late latch, `end`), so the eyes draw this frame's scene. [`crate::plugin::xr_frame`] then
//! only reads the mailbox. Without a renderer (headless logic, no GPU) the plugin's own frame loop
//! keeps running with nothing drawn.
//!
//! Per frame:
//! 1. The eye targets follow the recommended size times the render scale (re-created between
//!    frames, never inside one).
//! 2. [`crate::stereo::resolve_stereo_mode`] picks the strategy for the device.
//! 3. Both eyes draw: the whole frame twice ([`regia_render::FrameRenderer::render_eye`], with the
//!    vignette and the hidden-area paint in [`EyeCompositePass`]) or the reduced forward set in
//!    one pass ([`StereoForward`]).
//! 4. Panels over the runtime's layer budget are drawn into the eye images.
//! 5. The layer list goes to `end_frame`: a projection layer with each eye's rendered pose,
//!    sub-image and depth information, and the quad layers.
//! 6. The simulator composes what a headset would show, for `xr.capture_eye` and the mirror. A real
//!    runtime composes itself, and the mirror shows its eye images.

pub mod eye_composite;
pub mod mirror;
pub mod multiview;
pub mod quad;

use std::sync::Arc;

use regia_app::prelude::*;
use regia_core::glam::{Mat4, UVec2};
use regia_platform::xr::{
    DepthInfo, Eye, LayerList, Pose, ProjectionLayer, ProjectionView, QuadLayer, SubImage, View,
    VisibilityMask, XrError, XrRuntime, split_quads,
};
use regia_render::{
    ExtractedFrame, EyeCamera, EyeFrame, EyeHiddenArea, FramePass, FrameRenderer, FrameTarget, PostSettings,
    RenderDevice, RuntimeRenderer, ViewTables, WindowOverlay, hidden_area_clip_vertices, sync_view_tables,
};
use regia_scene::Transform;

pub use eye_composite::{
    EYE_COMPOSITE_PASS, EyeComposite, EyeCompositeInputs, EyeCompositePass, EyeCompositor, vignette_alpha,
};
pub use mirror::{
    MIRROR_LAYER, MIRROR_ORDER, MirrorPainter, MirrorRect, MirrorSource, SharedMirror, mirror_rects,
};
pub use multiview::{StereoForward, StereoForwardReport};
pub use quad::{MAX_PANELS, PanelDraw, PanelImage, QuadPainter, XrPanel, XrPanels, panel_in_tracking};

use crate::config::{Mirror, XrConfig};
use crate::eye_targets::{EyeTargets, MAX_SIM_EYE_EXTENT, SIM_EYE_FORMAT, SimEyeTargets};
use crate::mailbox::XrFrameState;
use crate::plugin::{RigFilter, XrSession, eye_projection, record_error};
use crate::state::{XrRenderProfile, XrState};
use crate::stereo::{
    StereoMode, apply_xr_quality, eye_camera, resolve_stereo_mode, union_cull_matrix, world_from_tracking,
};

/// Counters and the last frame's choices, as `xr.get` reports under `render`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct XrRenderStats {
    /// The strategy the last frame ran.
    pub mode: StereoMode,
    /// The device can run one pass over both layers.
    pub multiview_supported: bool,
    /// Stereo frames rendered.
    pub frames: u64,
    /// Objects drawn per eye in the last frame, after the shared cull.
    pub drawn: [usize; 2],
    /// Render passes of the scene in the last frame's strategy (2: one per eye).
    pub scene_passes: u32,
    /// Triangles masked per eye by the visibility mask.
    pub hidden_triangles: [u32; 2],
    /// Eyes whose depth reached the runtime in the last frame.
    pub depth_layers: u32,
    /// Panels the runtime composes.
    pub quads_composed: u32,
    /// Panels over its layer budget, drawn into the eye images.
    pub quads_overflow: u32,
    /// CPU time of the last stereo frame, milliseconds.
    pub last_cpu_ms: f32,
}

/// The render side's state.
#[derive(Resource)]
pub struct XrRenderState {
    composite: EyeCompositeInputs,
    registered: bool,
    forward: StereoForward,
    compositor: EyeCompositor,
    quads: QuadPainter,
    depth: Option<[wgpu::Texture; 2]>,
    display: Option<DisplayImages>,
    /// Eye-image views for the mirror, kept while those textures stay.
    mirror_eyes: Option<MirrorEyes>,
    mirror: SharedMirror,
    masks: [Arc<Vec<[f32; 4]>>; 2],
    /// The runtime's meshes, read when the session's mask generation moves.
    mask_source: [Option<VisibilityMask>; 2],
    mask_source_generation: Option<u64>,
    mask_key: Option<(u64, [u32; 8])>,
    mask_serial: u64,
    stats: XrRenderStats,
    warned_fallback: bool,
    /// The eyes hold GPU state.
    active: bool,
}

impl XrRenderState {
    fn new(mirror: SharedMirror) -> Self {
        XrRenderState {
            composite: EyeCompositeInputs::default(),
            registered: false,
            forward: StereoForward::default(),
            compositor: EyeCompositor::default(),
            quads: QuadPainter::default(),
            depth: None,
            display: None,
            mirror_eyes: None,
            mirror,
            masks: [Arc::default(), Arc::default()],
            mask_source: [None, None],
            mask_source_generation: None,
            mask_key: None,
            mask_serial: 0,
            stats: XrRenderStats::default(),
            warned_fallback: false,
            active: false,
        }
    }

    /// The last frame's report.
    pub fn stats(&self) -> XrRenderStats {
        self.stats
    }

    /// What a headset shows for `eye` on the simulator: the eye image with the panels composed.
    /// `None` on a real runtime, and on a simulator frame that composed no panel (the eye image
    /// is then what the headset shows).
    pub fn display_texture(&self, eye: Eye) -> Option<&wgpu::Texture> {
        self.display.as_ref().map(|images| &images.textures[eye.index()])
    }

    /// The depth image sent to the runtime for `eye`.
    pub fn depth_texture(&self, eye: Eye) -> Option<&wgpu::Texture> {
        self.depth.as_ref().map(|images| &images[eye.index()])
    }

    /// The mirror's shared source.
    pub fn mirror(&self) -> &SharedMirror {
        &self.mirror
    }

    /// The hidden-area triangles of `eye` in clip space.
    pub fn hidden_area(&self, eye: Eye) -> &[[f32; 4]] {
        &self.masks[eye.index()]
    }
}

/// Renders the XR frame. Add after [`crate::XrPlugin`] and the renderer's plugin.
#[derive(Default)]
pub struct XrRenderPlugin;

impl Plugin for XrRenderPlugin {
    fn build(&self, app: &mut App) {
        let mirror = SharedMirror::default();
        app.init_resource::<XrPanels>()
            .insert_resource(XrRenderState::new(mirror.clone()))
            .add_systems(
                Render,
                xr_render_frame
                    .after(regia_render::render_texture_views)
                    .before(regia_render::WindowRenderSet),
            );
        if !cfg!(target_os = "android")
            && let Some(mut overlay) = app.world_mut().get_resource_mut::<WindowOverlay>()
        {
            overlay.insert(MIRROR_ORDER, MIRROR_LAYER, Box::new(MirrorPainter::new(mirror)));
        }
    }
}

fn scaled_extent(recommended: UVec2, scale: f32) -> UVec2 {
    let scaled = recommended.as_vec2() * scale;
    UVec2::new(
        (scaled.x.round() as u32).clamp(1, MAX_SIM_EYE_EXTENT),
        (scaled.y.round() as u32).clamp(1, MAX_SIM_EYE_EXTENT),
    )
}

fn depth_images(d: &wgpu::Device, extent: UVec2) -> [wgpu::Texture; 2] {
    [0, 1].map(|_| {
        d.create_texture(&wgpu::TextureDescriptor {
            label: Some("xr eye depth"),
            size: extent_3d(extent),
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Depth32Float,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        })
    })
}

/// The simulator's composed eye images and the views taken of them once.
#[derive(Clone)]
struct DisplayImages {
    textures: [wgpu::Texture; 2],
    views: [wgpu::TextureView; 2],
}

/// Mirror views of the raw eye textures, rebuilt when those textures change.
struct MirrorEyes {
    textures: [wgpu::Texture; 2],
    views: [wgpu::TextureView; 2],
}

fn display_images(d: &wgpu::Device, extent: UVec2) -> DisplayImages {
    let textures = [0, 1].map(|_| {
        d.create_texture(&wgpu::TextureDescriptor {
            label: Some("xr display"),
            size: extent_3d(extent),
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: SIM_EYE_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        })
    });
    let views = [0, 1].map(|index| {
        crate::perf::texture_view();
        textures[index].create_view(&wgpu::TextureViewDescriptor::default())
    });
    DisplayImages { textures, views }
}

fn extent_3d(extent: UVec2) -> wgpu::Extent3d {
    wgpu::Extent3d {
        width: extent.x,
        height: extent.y,
        depth_or_array_layers: 1,
    }
}

/// `Render`: runs the XR frame when frames flow and there is a scene to draw.
pub fn xr_render_frame(world: &mut World) {
    let _span = tracing::trace_span!("xr_render_frame").entered();
    // Set again below when the eyes draw: a frame without them shows the desktop view, not an old
    // eye image.
    world
        .resource::<XrRenderState>()
        .mirror
        .set(MirrorSource::default());
    release_forward_meshes(world);
    release_ended_session(world);
    if !frame_can_run(world) {
        return;
    }
    let Some(device) = world.get_resource::<RenderDevice>().cloned() else {
        return;
    };
    let extent = prepare_targets(world, &device);
    let mask_generation = world.resource::<XrSession>().mask_generation();
    let result = world.resource_scope(|world, mut rt: Mut<XrRuntime>| {
        let backend = BackendFacts::read(&rt);
        world.resource_scope(|world, mut renderer: Mut<RuntimeRenderer>| {
            world.resource_scope(|world, mut render: Mut<XrRenderState>| {
                register_composite(&mut renderer.0, &mut render);
                if render.mask_source_generation != Some(mask_generation) {
                    let backend = rt.backend();
                    render.mask_source = [
                        backend.visibility_mask(Eye::Left),
                        backend.visibility_mask(Eye::Right),
                    ];
                    render.mask_source_generation = Some(mask_generation);
                }
                render.active = true;
                let mut failure = None;
                let report = rt.frame_layers(|_, views| {
                    match render_eyes(
                        world,
                        &device,
                        &mut renderer.0,
                        &mut render,
                        views,
                        &backend,
                        extent,
                    ) {
                        Ok(layers) => layers,
                        Err(e) => {
                            failure = Some(e);
                            LayerList::default()
                        }
                    }
                });
                (report, failure)
            })
        })
    });
    let (report, failure) = result;
    if let Some(e) = failure {
        world.resource_mut::<XrState>().stats.dropped_frames += 1;
        record_error(world, &e);
    }
    match report {
        Ok(report) => {
            let frame = XrFrameState {
                frame_index: report.timing.frame_index,
                predicted_display_time_ns: report.timing.predicted_display_time_ns,
                period_ns: report.timing.predicted_display_period_ns,
                should_render: report.timing.should_render,
            };
            world.resource_mut::<XrSession>().publish_rendered_frame(frame);
            world.resource_mut::<XrState>().stats.frames += 1;
            if report.rendered {
                // The eyes replace the desktop view; the window keeps its overlay layers.
                world.resource_mut::<ExtractedFrame>().drop_window_views();
            }
        }
        Err(e) => {
            world.resource_mut::<XrState>().stats.dropped_frames += 1;
            record_error(world, &e);
        }
    }
}

/// Hands this frame's released mesh keys to the multiview cache. It runs on every frame, also
/// when the eyes do not draw (paused, not flowing, warming): `extract_frame` hands each key out
/// once, so a key missed here would keep its buffers for the life of the cache.
fn release_forward_meshes(world: &mut World) {
    let Some(extracted) = world.get_resource::<ExtractedFrame>() else {
        return;
    };
    if extracted.released_mesh_keys.is_empty() {
        return;
    }
    let keys = extracted.released_mesh_keys.clone();
    if let Some(mut render) = world.get_resource_mut::<XrRenderState>() {
        render.forward.release_meshes(&keys);
    }
}

/// Releases the eyes' GPU state when no session is left to draw for.
fn release_ended_session(world: &mut World) {
    let flowing = world
        .get_resource::<XrRuntime>()
        .is_some_and(|rt| rt.backend().state().frames_flow());
    if flowing || !world.resource::<XrRenderState>().active {
        return;
    }
    if let Some(mut renderer) = world.get_resource_mut::<RuntimeRenderer>() {
        renderer.0.drop_stereo_views();
    }
    world.remove_resource::<SimEyeTargets>();
    let mut render = world.resource_mut::<XrRenderState>();
    render.active = false;
    render.depth = None;
    render.display = None;
    render.mirror_eyes = None;
    render.forward.clear_meshes();
    render.mask_key = None;
    render.mask_source_generation = None;
}

fn frame_can_run(world: &World) -> bool {
    let flowing = world
        .get_resource::<XrRuntime>()
        .is_some_and(|rt| rt.backend().state().frames_flow());
    let unpaused = world
        .get_resource::<XrSession>()
        .is_some_and(|session| !session.is_frame_paused());
    let renderable = world
        .get_resource::<ExtractedFrame>()
        .is_some_and(ExtractedFrame::is_renderable);
    let warming = world
        .get_resource::<regia_render::GameFrameWarm>()
        .is_some_and(regia_render::GameFrameWarm::is_warming);
    flowing
        && unpaused
        && renderable
        && !warming
        && !regia_render::device_lost()
        && world.contains_resource::<RuntimeRenderer>()
}

/// Re-creates the eye images at a frame boundary when the size changed. Returns the eye size.
fn prepare_targets(world: &mut World, device: &RenderDevice) -> UVec2 {
    let scale = world.resource::<XrConfig>().render_scale;
    let extent = scaled_extent(world.resource::<XrRenderProfile>().recommended_extent, scale);
    world.resource_mut::<XrRenderProfile>().extent = extent;
    let stale = world
        .get_resource::<SimEyeTargets>()
        .is_none_or(|t| t.extent() != extent);
    if stale {
        match SimEyeTargets::new(device, extent) {
            Ok(targets) => {
                world.insert_resource(targets);
                let mut render = world.resource_mut::<XrRenderState>();
                render.depth = None;
                render.display = None;
                render.mirror_eyes = None;
            }
            Err(e) => record_error(world, &e),
        }
    }
    extent
}

fn register_composite(renderer: &mut FrameRenderer, render: &mut XrRenderState) {
    if render.registered {
        return;
    }
    let pass: Box<dyn FramePass> = Box::new(EyeCompositePass::new(render.composite.clone()));
    if let Err(e) = renderer.register_pass(pass) {
        tracing::warn!(error = %e, "xr: the eye composite pass did not register");
    }
    render.registered = true;
}

/// What the runtime offers, read before the frame borrows it.
struct BackendFacts {
    simulator: bool,
    max_layers: usize,
    depth_layer: bool,
}

impl BackendFacts {
    fn read(rt: &XrRuntime) -> Self {
        let backend = rt.backend();
        BackendFacts {
            simulator: backend.name() == "null",
            max_layers: backend.max_layers(),
            depth_layer: backend.supports_depth_layer(),
        }
    }
}

fn fov_bits(views: &[View; 2]) -> [u32; 8] {
    let f = |v: &View| [v.fov.left, v.fov.right, v.fov.up, v.fov.down].map(f32::to_bits);
    let (a, b) = (f(&views[0]), f(&views[1]));
    [a[0], a[1], a[2], a[3], b[0], b[1], b[2], b[3]]
}

/// Rebuilds the clip-space masks when the runtime's mesh or the eyes' fields of view changed.
fn refresh_masks(
    render: &mut XrRenderState,
    generation: u64,
    views: &[View; 2],
    near_m: f32,
) -> Result<(), XrError> {
    let key = (generation, fov_bits(views));
    if render.mask_key == Some(key) {
        return Ok(());
    }
    for eye in Eye::BOTH {
        let clip = match &render.mask_source[eye.index()] {
            Some(mask) if mask.is_valid() => {
                let projection = eye_projection(&views[eye.index()].fov, near_m)?;
                hidden_area_clip_vertices(&mask.vertices, &mask.indices, &projection)
            }
            _ => Vec::new(),
        };
        render.masks[eye.index()] = Arc::new(clip);
    }
    render.mask_key = Some(key);
    render.mask_serial += 1;
    Ok(())
}

fn rig_pose(world: &mut World) -> Pose {
    let mut query = world.query_filtered::<&Transform, RigFilter>();
    query.single(world).map_or(Pose::IDENTITY, |t| Pose {
        position: t.translation,
        orientation: t.rotation,
    })
}

/// Draws both eyes and returns the frame's layers. The eye images leave the world for the frame,
/// so the scene can stay borrowed from it.
fn render_eyes(
    world: &mut World,
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    render: &mut XrRenderState,
    views: &[View; 2],
    facts: &BackendFacts,
    extent: UVec2,
) -> Result<LayerList, XrError> {
    let Some(mut sim_targets) = world.remove_resource::<SimEyeTargets>() else {
        return Err(XrError::Unsupported("eye targets"));
    };
    let rig = rig_pose(world);
    let result = eye_pass(
        world,
        EyeWork {
            device,
            renderer,
            render,
            targets: &mut sim_targets,
            rig,
            views,
            facts,
            extent,
        },
    );
    world.insert_resource(sim_targets);
    result
}

struct EyeWork<'a> {
    device: &'a RenderDevice,
    renderer: &'a mut FrameRenderer,
    render: &'a mut XrRenderState,
    targets: &'a mut dyn EyeTargets,
    rig: Pose,
    views: &'a [View; 2],
    facts: &'a BackendFacts,
    extent: UVec2,
}

fn eye_pass(world: &mut World, work: EyeWork<'_>) -> Result<LayerList, XrError> {
    let EyeWork {
        device,
        renderer,
        render,
        targets,
        rig,
        views,
        facts,
        extent,
    } = work;
    let started = std::time::Instant::now();
    let d = &device.device;
    let q = &device.queue;
    let config = world.resource::<XrConfig>().clone();
    let mut profile = *world.resource::<XrRenderProfile>();
    let near = config.near_m;
    let session = world.resource::<XrSession>();
    let world_pose = world_from_tracking(&rig, &session.offset);
    let vignette = session.vignette();
    let mask_generation = session.mask_generation();
    let far = regia_render::view_math::INFINITE_FAR_STANDIN.max(near * 2.0);
    let multiview_supported = StereoForward::multiview_supported(d);
    let mode = resolve_stereo_mode(profile.stereo_mode, multiview_supported);
    profile.multiview = mode == StereoMode::Multiview;
    *world.resource_mut::<XrRenderProfile>() = profile;
    if mode != profile.stereo_mode && !render.warned_fallback {
        render.warned_fallback = true;
        tracing::warn!("xr: this device has no multiview; rendering two passes");
    }
    if profile.hidden_area {
        refresh_masks(render, mask_generation, views, near)?;
    } else if render.mask_key.take().is_some() {
        render.masks = [Arc::default(), Arc::default()];
        render.mask_serial += 1;
    }
    // A union that does not exist (a very wide canted view) leaves each eye its own frustum.
    let cull_tracking = union_cull_matrix(views, far).ok();
    let camera = |view: &View| -> Result<EyeCamera, XrError> {
        let cull = match cull_tracking {
            Some(cull) => cull,
            None => eye_projection(&view.fov, near)? * view.view_matrix(),
        };
        eye_camera(view, &world_pose, near, cull)
    };
    let cameras = [camera(&views[0])?, camera(&views[1])?];
    let depth_on = profile.depth_layer && facts.depth_layer;
    if !depth_on {
        render.depth = None;
    } else if render.depth.is_none() {
        render.depth = Some(depth_images(d, extent));
    }
    let composites = [0, 1].map(|eye| EyeComposite {
        strength: vignette,
        inner: config.comfort.vignette_inner,
        outer: config.comfort.vignette_outer,
        tangents: cameras[eye].tangents(),
        mask_generation: render.mask_serial,
        mask_clip: render.masks[eye].clone(),
    });
    render.composite.set(composites.clone());

    let world: &World = world;
    let extracted = world.resource::<ExtractedFrame>();
    sync_view_tables(
        renderer,
        extracted,
        ViewTables {
            overrides: world.get_resource(),
            scalar_overlays: world.get_resource(),
            shading: world.get_resource(),
            skinning: world.get_resource(),
        },
    );
    let view_index = extracted.window_views().first().map_or(0, |(index, _)| *index);
    let mut scene = extracted.frame_scene(view_index, extent.x, extent.y);
    let mut post: Option<PostSettings> = scene.post.cloned();
    apply_xr_quality(&mut scene.quality, post.as_mut(), profile.per_eye_history);
    scene.post = post.as_ref();
    scene.near = near;
    scene.far = far;
    scene.depth_prepass = true;

    let panels = world.resource::<XrPanels>();
    let ordered = panels.ordered_layers();
    let (composed, overflow) = split_quads(&ordered, facts.max_layers);
    let images = [targets.image(Eye::Left), targets.image(Eye::Right)];
    let mut stats = XrRenderStats {
        mode,
        multiview_supported,
        frames: render.stats.frames + 1,
        quads_composed: composed.len() as u32,
        quads_overflow: overflow.len() as u32,
        ..XrRenderStats::default()
    };
    match mode {
        StereoMode::TwoPass => {
            renderer.begin_stereo_frame();
            let mut pending = Some(scene);
            for eye in Eye::BOTH {
                let index = eye.index();
                let _eye_span = tracing::trace_span!("xr_render_eye", eye = index).entered();
                let view = targets.acquire(eye)?;
                let clip = composites[index].mask_clip.as_slice();
                let scene = if eye == Eye::BOTH[1] {
                    pending.take()
                } else {
                    pending.as_ref().map(|scene| {
                        crate::perf::scene_cloned();
                        scene.clone()
                    })
                };
                let Some(scene) = scene else {
                    return Err(XrError::GpuInteropUnavailable {
                        reason: "the stereo scene was already handed to an eye".into(),
                    });
                };
                let frame = EyeFrame {
                    eye: index,
                    camera: cameras[index],
                    scene,
                    target: FrameTarget::full(&view, SIM_EYE_FORMAT, extent.x, extent.y)
                        .with_color(images[index].as_ref()),
                    hidden_area: (!clip.is_empty()).then_some(EyeHiddenArea {
                        generation: render.mask_serial,
                        clip_vertices: clip,
                    }),
                    depth_target: render.depth.as_ref().map(|depth| &depth[index]),
                };
                let drawn = renderer.render_eye(device, frame);
                targets.release(eye)?;
                let report = drawn.map_err(|e| XrError::GpuInteropUnavailable {
                    reason: e.to_string(),
                })?;
                stats.drawn[index] = report.drawn;
                stats.hidden_triangles[index] = report.hidden_triangles;
                stats.depth_layers += u32::from(report.depth_copied);
            }
            renderer.end_stereo_frame();
            stats.scene_passes = 2;
        }
        StereoMode::Multiview => {
            let [Some(left), Some(right)] = images.clone() else {
                return Err(XrError::Unsupported("multiview needs copyable eye images"));
            };
            let eye_images = [left, right];
            let report = render.forward.render(d, q, &scene, &cameras, &eye_images, true);
            stats.drawn = [report.drawn as usize; 2];
            stats.scene_passes = report.passes;
            let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("xr multiview composite"),
            });
            let write = |buffer: &wgpu::Buffer, bytes: &[u8]| q.write_buffer(buffer, 0, bytes);
            for eye in Eye::BOTH {
                let _eye_span = tracing::trace_span!("xr_render_eye", eye = eye.index()).entered();
                let view = targets.acquire(eye)?;
                render.compositor.record(
                    d,
                    &write,
                    &mut encoder,
                    (&view, SIM_EYE_FORMAT),
                    eye.index(),
                    &composites[eye.index()],
                );
                targets.release(eye)?;
            }
            if let (Some(depth), Some(source)) = (render.depth.as_ref(), render.forward.depth_texture()) {
                for (layer, image) in depth.iter().enumerate() {
                    encoder.copy_texture_to_texture(
                        wgpu::TexelCopyTextureInfo {
                            texture: source,
                            mip_level: 0,
                            origin: wgpu::Origin3d {
                                x: 0,
                                y: 0,
                                z: layer as u32,
                            },
                            aspect: wgpu::TextureAspect::All,
                        },
                        image.as_image_copy(),
                        extent_3d(extent),
                    );
                }
                stats.depth_layers = 2;
            }
            q.submit([encoder.finish()]);
        }
    }

    let head = Pose {
        position: (views[0].pose.position + views[1].pose.position) * 0.5,
        orientation: views[0].pose.orientation.slerp(views[1].pose.orientation, 0.5),
    };
    let draw_panels = |render: &mut XrRenderState,
                       encoder: &mut wgpu::CommandEncoder,
                       target: &wgpu::TextureView,
                       eye: usize,
                       layers: &[QuadLayer],
                       slot_base: u32|
     -> Result<(), XrError> {
        let view = &views[eye];
        let clip_from_tracking: Mat4 = eye_projection(&view.fov, near)? * view.view_matrix();
        let draws: Vec<PanelDraw<'_>> = layers
            .iter()
            .filter_map(|layer| {
                Some(PanelDraw {
                    clip_from_tracking,
                    pose: panel_in_tracking(layer, &head, &world_pose),
                    layer,
                    image: &panels.get(layer.id)?.image,
                })
            })
            .collect();
        render
            .quads
            .draw_from(d, q, encoder, (target, SIM_EYE_FORMAT), slot_base, &draws);
        Ok(())
    };
    if !overflow.is_empty() {
        for eye in Eye::BOTH {
            // One submit per eye: both eyes' matrices go through the painter's one buffer.
            let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("xr panels over the layer budget"),
            });
            let view = targets.acquire(eye)?;
            draw_panels(render, &mut encoder, &view, eye.index(), overflow, 0)?;
            targets.release(eye)?;
            q.submit([encoder.finish()]);
        }
    }
    if facts.simulator {
        if composed.is_empty() {
            render.display = None;
        } else if render.display.is_none() {
            render.display = Some(display_images(d, extent));
        }
    }
    let mut mirror_views = None;
    if let Some(display) = render.display.clone() {
        let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("xr display"),
        });
        for eye in Eye::BOTH {
            let index = eye.index();
            if let Some(image) = images[index].as_ref() {
                encoder.copy_texture_to_texture(
                    image.as_image_copy(),
                    display.textures[index].as_image_copy(),
                    extent_3d(extent),
                );
                crate::perf::display_copy();
            }
            draw_panels(
                render,
                &mut encoder,
                &display.views[index],
                index,
                composed,
                composed.len() as u32 * index as u32,
            )?;
        }
        q.submit([encoder.finish()]);
        crate::perf::display_submit();
        mirror_views = Some(display.views.clone());
    } else if let [Some(left), Some(right)] = &images {
        let current = [left.clone(), right.clone()];
        let same = render
            .mirror_eyes
            .as_ref()
            .is_some_and(|cached| cached.textures == current);
        if !same {
            let views = [0, 1].map(|index| {
                crate::perf::texture_view();
                current[index].create_view(&wgpu::TextureViewDescriptor::default())
            });
            render.mirror_eyes = Some(MirrorEyes {
                textures: current,
                views,
            });
        }
        mirror_views = render.mirror_eyes.as_ref().map(|cached| cached.views.clone());
    }
    render.mirror.set(MirrorSource {
        views: mirror_views,
        extent: extent.to_array(),
        mode: if cfg!(target_os = "android") {
            Mirror::Off
        } else {
            config.mirror
        },
    });
    stats.last_cpu_ms = started.elapsed().as_secs_f32() * 1000.0;
    render.stats = stats;
    let rect = [0, 0, extent.x, extent.y];
    let image = SubImage { array_index: 0, rect };
    let projection_view = |index: usize| ProjectionView {
        pose: views[index].pose,
        fov: views[index].fov,
        image,
        depth: render.depth.as_ref().map(|_| DepthInfo {
            image,
            min_depth: 0.0,
            max_depth: 1.0,
            near_m: near,
            far_m: far,
        }),
    };
    Ok(LayerList {
        projection: Some(ProjectionLayer {
            views: [projection_view(0), projection_view(1)],
        }),
        quads: composed.to_vec(),
    })
}
