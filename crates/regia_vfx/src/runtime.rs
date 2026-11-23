// SPDX-License-Identifier: MPL-2.0
//! The ECS runtime bridge: draws VFX through `regia_render`'s runtime path.
//!
//! [`crate::VfxPlugin`] owns the CPU state (scene scheduling, decal pool, shake). This plugin owns
//! the GPU side: once the [`regia_render::RenderDevice`] exists it creates one
//! [`VfxGpu`], installs the [`regia_render::TranslucentLayer`] implementation on the
//! [`regia_render::RuntimeRenderer`], and every frame uploads what the CPU state produced
//! (emitter records, spawn jobs, decals, trails) plus the frame's delta time and view.
//!
//! System ordering (all `Update`): runs after the plugin's tickers so the jobs scheduled this
//! frame reach the GPU before the renderer's `Render` systems call the layer.
//!
//! ```text
//! Update:  VfxPlugin (tick scene/decals/shake) → VfxRuntimePlugin (ensure, upload)
//! Render:  extract_frame → render_windows → TranslucentLayer (simulate + draw)
//! ```

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_render::{
    ExtractedFrame, LabLocalLight, LabPointLight, LayerCamera, RenderDevice, RuntimeRenderer,
    TranslucentLayer, WindowSurfaces,
};

use crate::ambient::{AmbientLifeGpu, AmbientLifeHandle};
use crate::decal_cull::DecalCullStats;
use crate::decal_forward::ForwardFeedState;
use crate::gpu::{TrailVertex, VfxCamera, VfxError, VfxGpu, VfxLighting, VfxProfile};
use crate::plugin::VfxView;
use crate::precip::{PrecipitationGpu, PrecipitationHandle};
use crate::scene::VfxScene;
use crate::{DecalPool, VfxSettings};

/// Trail vertices appended by game systems during `Update`; uploaded and consumed by this
/// plugin each frame (see [`TrailRig::build_vertices`](crate::trails::TrailRig::build_vertices)).
#[derive(Resource, Debug, Default)]
pub struct VfxTrails(pub Vec<TrailVertex>);

/// The shared GPU context: one `VfxGpu` behind the render hook, created on first update
/// after the render device exists.
#[derive(Resource, Clone)]
pub struct VfxGpuHandle {
    gpu: Arc<Mutex<VfxGpu>>,
    dt: Arc<Mutex<f32>>,
    /// The camera the frame hook last rendered with (the LOD fallback when the ECS
    /// extracted no camera view).
    rendered: Arc<Mutex<Option<VfxCamera>>>,
    /// The game frame's step has been taken by a render (ADR-2687): set by the first render
    /// that steps, cleared by the frame's upload.
    stepped: Arc<AtomicBool>,
    /// The forward-decal hand-off with the runtime layer (ADR-3255).
    forward: Arc<Mutex<ForwardFeedState>>,
}

impl VfxGpuHandle {
    /// Locks the GPU context. A poisoned lock (a panic while held) yields `None` — the
    /// frame is skipped rather than cascading the panic.
    pub fn lock(&self) -> Option<std::sync::MutexGuard<'_, VfxGpu>> {
        self.gpu.lock().ok()
    }

    /// Seconds the next rendered frame advances the particle simulation by (set from the game
    /// clock every frame).
    pub fn step_seconds(&self) -> f32 {
        self.dt.lock().map_or(1.0 / 60.0, |dt| *dt)
    }

    /// Overrides [`Self::step_seconds`] until the next frame's upload. The simulation steps
    /// once per game frame (ADR-2687): the first render of the frame that simulates takes the
    /// step, and every later render of the same frame (an offscreen render-out next to the
    /// window's own, a second window) draws that state with a step of 0. A render with 0 set
    /// here takes nothing, so the frame's step is still there for the next render.
    pub fn set_step_seconds(&self, seconds: f32) {
        if let Ok(mut dt) = self.dt.lock() {
            *dt = seconds;
        }
    }

    /// A new game frame whose step is `dt`: it is there for the first render that simulates
    /// (ADR-2687).
    fn begin_frame(&self, dt: f32) {
        if let Ok(mut step) = self.dt.lock() {
            *step = dt;
        }
        self.stepped.store(false, Ordering::Release);
    }

    /// Whether a render of this game frame has already taken its step ([`Self::set_step_seconds`]).
    /// A window that did not draw this frame (minimized, occluded, its surface not ready) took
    /// none, so the next render steps.
    #[must_use]
    pub fn frame_stepped(&self) -> bool {
        self.stepped.load(Ordering::Acquire)
    }

    /// A [`regia_render::PipelineWarmQueue`] job that builds this context's effect pipelines
    /// for `formats` ([`crate::vfx_warm_job`]; [`VfxGpu::warm_formats`] of the window format),
    /// so the first emitter, decal or trail compiles nothing on the frame path.
    pub fn warm_job(&self, formats: Vec<wgpu::TextureFormat>) -> regia_render::pipeline_warm::WarmJob {
        crate::gpu::vfx_warm_job(Arc::clone(&self.gpu), formats)
    }
}

/// The [`TranslucentLayer`] implementation: drives the shared [`VfxGpu`] from the host
/// renderer's camera + depth and draws decals, particles and trails.
struct VfxLayer {
    gpu: Arc<Mutex<VfxGpu>>,
    dt: Arc<Mutex<f32>>,
    /// The runtime's once-per-frame mark ([`VfxGpuHandle::frame_stepped`]); `None` for a layer
    /// whose host sets the step itself and steps every render.
    stepped: Option<Arc<AtomicBool>>,
    rendered: Arc<Mutex<Option<VfxCamera>>>,
    /// The rain and snow; `None` where the host owns the layer without them.
    precip: Option<PrecipitationHandle>,
    /// The queue the precipitation's one uniform goes through.
    queue: Option<wgpu::Queue>,
    /// The effects context was idle ([`VfxGpu::is_idle`]) when this frame simulated: only the
    /// rain or snow keeps the layer live, so its simulation, soft layer and particle and trail
    /// draws are left out (ADR-3249). Nothing of them can draw a pixel then.
    effects_idle: AtomicBool,
    /// The runtime's forward-decal hand-off (ADR-3255); `None` for a layer whose host owns the
    /// [`VfxGpu`] and sets forward decals itself.
    forward: Option<Arc<Mutex<ForwardFeedState>>>,
    /// Ambient life (ADR-3024); `None` where the host owns the layer without it.
    ambient: Option<AmbientLifeHandle>,
}

impl VfxLayer {
    /// Uploads this frame's precipitation, whose target and lighting the VFX context just set, and
    /// records its reduced-resolution pass when the frame draws it at half or quarter size.
    fn prepare_precipitation(
        &self,
        gpu: &VfxGpu,
        depth: &wgpu::TextureView,
        camera: &LayerCamera,
        encoder: &mut wgpu::CommandEncoder,
    ) {
        let (Some(handle), Some(queue)) = (&self.precip, &self.queue) else {
            return;
        };
        let (Ok(mut state), Ok(mut precip)) = (handle.state.lock(), handle.gpu.lock()) else {
            return;
        };
        let Some(precip) = precip.as_mut() else {
            return;
        };
        let _span = tracing::trace_span!("vfx_precipitation_prepare").entered();
        let eye = camera.eye.map(f64::from);
        let forward = [
            camera.target[0] - camera.eye[0],
            camera.target[1] - camera.eye[1],
            camera.target[2] - camera.eye[2],
        ];
        let mut frame = state.frame_at(eye, forward);
        if !state.active {
            frame.counts = crate::precip::PrecipCounts::default();
        }
        precip.bind_depth(depth);
        precip.prepare(
            queue,
            &frame,
            &crate::precip::PrecipView {
                camera: VfxCamera {
                    eye: Vec3::from_array(camera.eye),
                    target: Vec3::from_array(camera.target),
                    fov_y: camera.fov_y,
                    near: camera.near,
                    far: camera.far,
                },
                lighting: gpu.lighting(),
                target: crate::precip::PrecipTarget {
                    width: camera.width,
                    height: camera.height,
                    sky_rows: gpu.sky_light_rows(),
                    target_mode: if camera.hdr { 2.0 } else { gpu.target_mode() },
                    cloud_coverage: state.cloud_coverage,
                },
            },
        );
        precip.record_reduced(encoder);
    }
}

impl VfxLayer {
    /// Uploads this frame's ambient life, whose target and lighting the VFX context just set.
    fn prepare_ambient(&self, gpu: &VfxGpu, depth: &wgpu::TextureView, camera: &LayerCamera) {
        let (Some(handle), Some(queue)) = (&self.ambient, &self.queue) else {
            return;
        };
        let (Ok(state), Ok(mut ambient)) = (handle.state.lock(), handle.gpu.lock()) else {
            return;
        };
        let Some(ambient) = ambient.as_mut() else {
            return;
        };
        let _span = tracing::trace_span!("vfx_ambient_life_prepare").entered();
        let frame = state.frame_at(camera.eye.map(f64::from));
        let cloud_coverage = state.cloud_coverage;
        drop(state);
        ambient.bind_depth(depth);
        ambient.prepare(
            queue,
            &frame,
            &crate::ambient::AmbientView {
                camera: VfxCamera {
                    eye: Vec3::from_array(camera.eye),
                    target: Vec3::from_array(camera.target),
                    fov_y: camera.fov_y,
                    near: camera.near,
                    far: camera.far,
                },
                lighting: gpu.lighting(),
                target: crate::precip::PrecipTarget {
                    width: camera.width,
                    height: camera.height,
                    sky_rows: gpu.sky_light_rows(),
                    target_mode: if camera.hdr { 2.0 } else { gpu.target_mode() },
                    cloud_coverage,
                },
            },
        );
    }
}

impl TranslucentLayer for VfxLayer {
    fn simulate(&self, depth: &wgpu::TextureView, camera: &LayerCamera, encoder: &mut wgpu::CommandEncoder) {
        // A frame that simulates the layer beside an active one: nothing of it to step.
        if !self.live() {
            return;
        }
        let Ok(mut gpu) = self.gpu.lock() else { return };
        let dt = take_frame_step(
            self.dt.lock().map(|d| *d).unwrap_or(1.0 / 60.0),
            self.stepped.as_deref(),
        );
        // Live only for the precipitation (ADR-3249): no particle, spawn, decal or trail, and past
        // the warm frames. The context's depth binding, frame upload, simulation and soft passes
        // would record work that draws nothing, so only the lighting the drops read is set. Its
        // clock stops while idle, as `VfxGpu::is_idle` allows; the next spawn starts a new window.
        let idle = gpu.is_idle();
        self.effects_idle.store(idle, Ordering::Relaxed);
        if !idle {
            gpu.set_depth_view(Some(depth));
        }
        let vfx_camera = VfxCamera {
            eye: Vec3::from_array(camera.eye),
            target: Vec3::from_array(camera.target),
            fov_y: camera.fov_y,
            near: camera.near,
            far: camera.far,
        };
        if let Ok(mut rendered) = self.rendered.lock() {
            *rendered = Some(vfx_camera);
        }
        // Only an HDR frame divides by exposure: the display pass is display-referred.
        gpu.set_exposure(if camera.hdr { camera.exposure } else { 1.0 });
        // The HDR pass re-copies the scene after decals, so heat haze has a source.
        gpu.set_haze(camera.hdr);
        // Decal relight and glint, and particle lighting, follow the frame's own sun, at the
        // brightness a white surface shows under it: lux through the post exposure itself,
        // not `exposure` (post ÷ pre-exposure on a pre-exposed HDR target, which put the
        // particle sun thousands of times too bright, clamped at 4: washed-out smoke).
        gpu.set_sun(
            camera.sun_direction,
            camera.sun_color,
            camera.sun_illuminance,
            camera.hdr.then_some(camera.display_exposure),
        );
        // Lit puffs take the frame's sky light (ADR-1425) through the same exposure mapping.
        gpu.set_sky_light(camera.sky_ambient, camera.hdr.then_some(camera.display_exposure));
        // The low-resolution soft layer composites into the HDR target only.
        gpu.set_soft_pass(camera.hdr);
        if !idle {
            gpu.set_frame(&vfx_camera, dt, camera.width, camera.height);
        }
        self.prepare_precipitation(&gpu, depth, camera, encoder);
        // Ambient life draws whether or not the effects context is idle (ADR-3024 over ADR-3249).
        self.prepare_ambient(&gpu, depth, camera);
        if idle {
            return;
        }
        gpu.simulate(encoder);
        if let Err(err) = gpu.record_soft_pass(encoder, camera.width, camera.height) {
            tracing::error!(target: "regia_vfx", "soft particle layer: {err}");
        }
    }

    fn bind_decal_targets(&self, receiver: Option<&wgpu::TextureView>, scene: Option<&wgpu::TextureView>) {
        let Ok(mut gpu) = self.gpu.lock() else { return };
        gpu.set_material_views(receiver, scene);
    }

    fn precipitation_occlusion_request(&self) -> Option<regia_render::PrecipOcclusionRequest> {
        self.precip.as_ref()?.state.lock().ok()?.occlusion_request()
    }

    fn sun_occlusion_request(&self) -> Option<regia_render::SunOcclusionRequest> {
        let state = self.ambient.as_ref()?.state.lock().ok()?;
        if !state.wants_sun_view {
            return None;
        }
        let (texels, extent_m) = crate::precip::PrecipBudget::for_quality(state.quality).occlusion?;
        Some(regia_render::SunOcclusionRequest { texels, extent_m })
    }

    fn bind_precipitation_occlusion(&self, binding: Option<&regia_render::PrecipOcclusionBinding<'_>>) {
        let Some(handle) = &self.precip else { return };
        let Ok(mut precip) = handle.gpu.lock() else { return };
        if let Some(precip) = precip.as_mut() {
            precip.bind_occlusion(binding);
        }
    }

    fn bind_sun_occlusion(&self, binding: Option<&regia_render::PrecipOcclusionBinding<'_>>) {
        let Some(handle) = &self.ambient else { return };
        let Ok(mut ambient) = handle.gpu.lock() else {
            return;
        };
        if let Some(ambient) = ambient.as_mut() {
            ambient.bind_occlusion(binding);
        }
    }

    fn note_colour_format(&self, format: wgpu::TextureFormat) {
        let Ok(mut gpu) = self.gpu.lock() else { return };
        gpu.set_colour_format(format);
    }

    fn draw_decals(&self, pass: &mut wgpu::RenderPass<'_>, format: wgpu::TextureFormat) {
        if self.effects_idle.load(Ordering::Relaxed) {
            return;
        }
        let Ok(mut gpu) = self.gpu.lock() else { return };
        if let Err(err) = gpu.draw_decals(pass, format) {
            tracing::error!(target: "regia_vfx", "decals: {err}");
        }
    }

    fn draw(&self, pass: &mut wgpu::RenderPass<'_>, format: wgpu::TextureFormat) {
        // Rain and snow are unsorted, low-alpha and thin: sorted effects composite over them.
        if let Some(handle) = &self.precip
            && let Ok(mut precip) = handle.gpu.lock()
            && let Some(precip) = precip.as_mut()
        {
            precip.draw(pass, format);
        }
        // Ambient life: small, mostly covering, unsorted; sorted effects composite over it.
        if let Some(handle) = &self.ambient
            && let Ok(mut ambient) = handle.gpu.lock()
            && let Some(ambient) = ambient.as_mut()
        {
            ambient.draw(pass, format);
        }
        if self.effects_idle.load(Ordering::Relaxed) {
            return;
        }
        let Ok(mut gpu) = self.gpu.lock() else { return };
        // Errors here are shader/usage bugs; the layer records what it can and never panics.
        if let Err(err) = gpu.draw_particles(pass, format) {
            tracing::error!(target: "regia_vfx", "particles: {err}");
        }
        gpu.draw_trails(pass, format);
    }

    fn participates_in_hdr(&self) -> bool {
        true
    }

    fn forward_decal_feed(&self, binding: bool) -> Option<regia_render::ForwardDecalFeed> {
        crate::decal_forward::feed_for_frame(self.forward.as_deref()?, binding)
    }

    fn is_active(&self) -> bool {
        self.live()
    }

    fn wants_scene_after_decals(&self) -> bool {
        self.gpu.lock().is_ok_and(|gpu| gpu.wants_haze())
    }

    fn has_decals(&self) -> bool {
        self.gpu.lock().is_ok_and(|gpu| gpu.has_decals())
    }

    fn draw_hdr(&self, pass: &mut wgpu::RenderPass<'_>, format: wgpu::TextureFormat) {
        // Not simulated this frame (idle beside an active layer): its soft layer is last
        // frame's, so nothing of it draws.
        if !self.live() {
            return;
        }
        self.draw(pass, format);
        if self.effects_idle.load(Ordering::Relaxed) {
            return;
        }
        // The soft layer (dust, smoke, billows at reduced resolution) over the full-res lists.
        let Ok(mut gpu) = self.gpu.lock() else { return };
        if let Err(err) = gpu.draw_soft_composite(pass, format) {
            tracing::error!(target: "regia_vfx", "soft particle composite: {err}");
        }
    }
}

/// The step a render simulates by (ADR-2687). Without a once-per-frame mark every render takes
/// `dt`. With one, the first render of the game frame that steps takes `dt` and sets the mark;
/// later renders of the frame take 0. A render with no step (0, a held meter render) leaves the
/// mark alone, so the frame's step waits for the next render.
fn take_frame_step(dt: f32, stepped: Option<&AtomicBool>) -> f32 {
    let Some(stepped) = stepped else { return dt };
    if dt.is_nan() || dt <= 0.0 {
        return dt;
    }
    if stepped.swap(true, Ordering::AcqRel) {
        0.0
    } else {
        dt
    }
}

/// The layer [`VfxRuntimePlugin`] installs for `handle`: it shares the handle's context, step and
/// once-per-frame mark.
fn runtime_layer(
    handle: &VfxGpuHandle,
    precip: Option<PrecipitationHandle>,
    queue: Option<wgpu::Queue>,
) -> VfxLayer {
    VfxLayer {
        gpu: Arc::clone(&handle.gpu),
        dt: Arc::clone(&handle.dt),
        stepped: Some(Arc::clone(&handle.stepped)),
        rendered: Arc::clone(&handle.rendered),
        precip,
        queue,
        effects_idle: AtomicBool::new(false),
        forward: Some(Arc::clone(&handle.forward)),
        ambient: None,
    }
}

/// The translucent layer [`VfxRuntimePlugin`] installs, for a host that already owns the
/// [`VfxGpu`] (tests, and any renderer that is not the ECS runtime).
///
/// The frame renderer calls [`TranslucentLayer::bind_decal_targets`] before `simulate`,
/// then stamps the receiver and copies scene colour before `draw_decals`.
pub fn translucent_layer(gpu: Arc<Mutex<VfxGpu>>) -> Arc<dyn TranslucentLayer> {
    translucent_layer_with_dt(gpu, Arc::new(Mutex::new(1.0 / 60.0)))
}

/// [`translucent_layer`] whose simulation step is `dt` seconds per rendered frame, set by
/// the host (a look-dev harness that renders a few frames at a coarse step).
pub fn translucent_layer_with_dt(gpu: Arc<Mutex<VfxGpu>>, dt: Arc<Mutex<f32>>) -> Arc<dyn TranslucentLayer> {
    Arc::new(VfxLayer {
        gpu,
        dt,
        stepped: None,
        rendered: Arc::new(Mutex::new(None)),
        precip: None,
        queue: None,
        effects_idle: AtomicBool::new(false),
        forward: None,
        ambient: None,
    })
}

/// [`translucent_layer`] that also draws the rain and snow of `precip`: the layer the runtime
/// installs, for a host that owns its [`VfxGpu`] and steps the state itself (tests).
pub fn translucent_layer_with_precipitation(
    gpu: Arc<Mutex<VfxGpu>>,
    precip: PrecipitationHandle,
    queue: wgpu::Queue,
) -> Arc<dyn TranslucentLayer> {
    Arc::new(VfxLayer {
        gpu,
        dt: Arc::new(Mutex::new(1.0 / 60.0)),
        stepped: None,
        rendered: Arc::new(Mutex::new(None)),
        precip: Some(precip),
        queue: Some(queue),
        effects_idle: AtomicBool::new(false),
        forward: None,
        ambient: None,
    })
}

/// [`translucent_layer`] that also draws ambient life (and the rain and snow of `precip`, if
/// any): the layer the runtime installs, for a host that owns its [`VfxGpu`] and steps the state
/// itself (tests).
pub fn translucent_layer_with_ambient_life(
    gpu: Arc<Mutex<VfxGpu>>,
    precip: Option<PrecipitationHandle>,
    ambient: AmbientLifeHandle,
    queue: wgpu::Queue,
) -> Arc<dyn TranslucentLayer> {
    Arc::new(VfxLayer {
        gpu,
        dt: Arc::new(Mutex::new(1.0 / 60.0)),
        stepped: None,
        rendered: Arc::new(Mutex::new(None)),
        precip,
        queue: Some(queue),
        effects_idle: AtomicBool::new(false),
        forward: None,
        ambient: Some(ambient),
    })
}

/// Installs the VFX GPU side into the ECS runtime path. Add after [`crate::VfxPlugin`].
///
/// Requires `regia_render`'s runtime path (`RenderPlugin`); the layer is installed the
/// first `Update` that sees a [`RenderDevice`] (surfaces exist by then, so their format is
/// known — pipelines are built lazily per format anyway).
pub struct VfxRuntimePlugin;

impl Plugin for VfxRuntimePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<VfxTrails>()
            .init_resource::<DecalCullStats>()
            .init_resource::<PrecipitationHandle>()
            .init_resource::<AmbientLifeHandle>()
            .add_systems(
                Update,
                (
                    ensure_runtime_layer,
                    upload_runtime_frame,
                    drive_precipitation_runtime,
                    drive_ambient_runtime,
                )
                    .chain()
                    .after(crate::plugin::tick_vfx_scene)
                    .after(crate::decal::update_decals),
            )
            .add_systems(
                Render,
                push_particle_lights
                    .after(regia_render::extract_frame)
                    .before(regia_render::render_texture_views),
            );
    }
}

/// Luminous power (lumens) of one unit of [`ParticleLightDef::intensity`](crate::format::ParticleLightDef)
/// on the runtime path. A fireball light of 14 at full envelope is 7e6 lm (≈5.6e5 cd): about
/// 60 klx on the ground 3 m below, so the glow reads next to a 100 klx sun at EV 15 and
/// dominates at night. The offscreen harness shades the same light in its own units.
pub const PARTICLE_LIGHT_LUMENS_PER_UNIT: f32 = 5.0e5;

/// A particle light this wide in radius of influence (metres) is a flash, not a fire: the
/// crash flash reaches `80 R` (ADR-1454), so its source is the fireball, radius
/// `radius / FLASH_RADIUS_PER_SOURCE`. Narrower lights stay punctual.
pub const FLASH_MIN_RADIUS_M: f32 = 400.0;

/// Influence radius ÷ source radius of a flash light (`crash::crash_fireball`'s `80 R`).
pub const FLASH_RADIUS_PER_SOURCE: f32 = 80.0;

/// The sphere source of a particle light of influence `radius`: the fireball behind a flash
/// ([`FLASH_MIN_RADIUS_M`] and wider), `None` (a point) for fires and sparks. A sphere source
/// bounds the near-field of the inverse square (`distance_attenuation_source`), so the ground
/// under a fireball no longer lights up thousands of times brighter than the ground beside it.
#[must_use]
pub fn particle_light_source_radius(radius: f32) -> Option<f32> {
    (radius.is_finite() && radius >= FLASH_MIN_RADIUS_M).then_some(radius / FLASH_RADIUS_PER_SOURCE)
}

/// The frame's particle lights as renderer point lights (no shadows).
///
/// `color × intensity × envelope` becomes a normalised linear colour and
/// [`PARTICLE_LIGHT_LUMENS_PER_UNIT`] lumens per unit of its brightest channel.
pub fn particle_point_lights(lights: &crate::lights::VfxFrameLights) -> Vec<LabLocalLight> {
    particle_point_lights_scaled(lights, 1.0)
}

/// [`particle_point_lights`] with every intensity multiplied by `scale` (host look).
pub fn particle_point_lights_scaled(
    lights: &crate::lights::VfxFrameLights,
    scale: f32,
) -> Vec<LabLocalLight> {
    let scale = scale.max(0.0);
    lights
        .lights
        .iter()
        .filter_map(|light| {
            let peak = light.color[0].max(light.color[1]).max(light.color[2]);
            (peak > 0.0 && light.radius > 0.0).then(|| {
                LabLocalLight::Point(LabPointLight {
                    position: light.position.to_array(),
                    color: [
                        light.color[0] / peak,
                        light.color[1] / peak,
                        light.color[2] / peak,
                    ],
                    intensity: peak * scale * PARTICLE_LIGHT_LUMENS_PER_UNIT,
                    radius: light.radius,
                    cast_shadows: false,
                    source_radius: particle_light_source_radius(light.radius),
                })
            })
        })
        .collect()
}

/// Appends this frame's particle lights to the extracted frame's local lights, so a
/// fireball lights the ground and nearby meshes through the renderer's clustered lights.
pub fn push_particle_lights(
    lights: Option<Res<crate::lights::VfxFrameLights>>,
    look: Option<Res<crate::plugin::VfxLook>>,
    frame: Option<ResMut<ExtractedFrame>>,
) {
    let (Some(lights), Some(mut frame)) = (lights, frame) else {
        return;
    };
    let look = look.as_deref().copied().unwrap_or_default();
    if !look.fire_light {
        return;
    }
    let _span = tracing::info_span!("vfx_particle_lights").entered();
    frame
        .local_lights
        .extend(particle_point_lights_scaled(&lights, look.fire_light_scale));
}

impl VfxLayer {
    /// Whether the layer has anything to simulate or draw ([`TranslucentLayer::is_active`]):
    /// falling rain or snow, or a context that is not idle ([`VfxGpu::is_idle`]: its warm
    /// frames, a staged spawn, decal or trail, a particle that may still live). An empty layer
    /// is then free: the frame skips its simulate call, targets, copies and passes (ADR-2585).
    fn live(&self) -> bool {
        let falling = self.precip.as_ref().is_some_and(|handle| {
            handle
                .state
                .lock()
                .is_ok_and(|state| state.active && !state.rates.is_none())
        });
        // Ambient life on a tier that has it keeps the layer live (ADR-3024).
        let ambient = self
            .ambient
            .as_ref()
            .is_some_and(|handle| handle.state.lock().is_ok_and(|state| state.active));
        // A poisoned lock: the layer records nothing (its own methods do the same).
        falling || ambient || self.gpu.lock().is_ok_and(|gpu| !gpu.is_idle())
    }
}

/// A [`VfxGpu`] being built on a [`regia_render::PipelineWarmQueue`] worker
/// ([`ensure_runtime_layer`]): the context's compute pipelines (simulation, sort, radix sort)
/// and its render sets for the window and HDR formats compile there, not on the main thread.
#[derive(Resource, Default)]
struct PendingVfxGpu {
    /// The device the job builds on ([`RenderDevice::device_id`]).
    device: Option<u64>,
    /// Filled by the job.
    slot: Arc<Mutex<Option<Result<VfxGpu, VfxError>>>>,
    /// When the job was queued.
    since: Option<std::time::Instant>,
}

/// The [`VfxGpu`] build for this device and profile failed ([`ensure_runtime_layer`] logged
/// why): effects stay off, and the build is not retried (or re-queued) every frame.
#[derive(Resource)]
struct VfxGpuFailed {
    device: u64,
    profile: VfxProfile,
}

/// A worker build not back after this (a queue shut down or stuck) is replaced by a build on
/// the main thread, so effects never stay off.
const VFX_WORKER_BUILD_LIMIT: std::time::Duration = std::time::Duration::from_secs(30);

/// The formats the runtime draws effects into: the window's, the HDR pass's (the profile's
/// [`regia_render::RenderProfile::hdr_color_format`]) and the soft layer's.
fn runtime_formats(world: &World, device: &RenderDevice) -> Vec<wgpu::TextureFormat> {
    let window = world
        .get_resource::<WindowSurfaces>()
        .and_then(WindowSurfaces::primary_format)
        .unwrap_or(wgpu::TextureFormat::Bgra8UnormSrgb);
    let mut formats = VfxGpu::warm_formats(window);
    if let Some(settings) = world.get_resource::<regia_render::RenderSettings>() {
        let hdr = settings.profile.hdr_color_format(&device.capabilities);
        if !formats.contains(&hdr) {
            formats.push(hdr);
        }
    }
    formats
}

/// Creates the [`VfxGpu`] once the render device exists and installs the render hook.
///
/// With a started [`regia_render::PipelineWarmQueue`] on the same device, the context is
/// built on a warm worker (map tier), with its render pipelines for the window and HDR
/// formats ([`VfxGpu::warm`]), and installed on the first `Update` after the job returns: the
/// main thread compiles none of the effect pipelines (DO27 INTEGRATE_005 § 3, ADR-195). Without
/// one it is built here, as before.
fn ensure_runtime_layer(world: &mut World) {
    install_runtime_layer(world, false);
}

/// Installs the context now when a render-out job starts and this app's [`VfxRuntimePlugin`] has
/// none yet: a job started before the first `Update`, or while the context still builds on a warm
/// worker. Otherwise the context arrives during the job: the job's earlier effects are lost, a held
/// job has no pool to put back, and the new context's warm frames move the particle clock (ADR-2814).
/// The worker's context is used if it is back; if not, it is built here, as after
/// [`VFX_WORKER_BUILD_LIMIT`], and the worker's result is dropped. A job is not a live frame: the
/// compile only makes it start later. An app without the plugin stays without a context.
pub(crate) fn install_context_for_job(world: &mut World) {
    if world.contains_resource::<PrecipitationHandle>()
        && world.contains_resource::<VfxSettings>()
        && world.contains_resource::<RuntimeRenderer>()
    {
        install_runtime_layer(world, true);
    }
}

/// [`ensure_runtime_layer`]; `now` does not wait for a warm worker ([`install_context_for_job`]).
fn install_runtime_layer(world: &mut World, now: bool) {
    let _span = tracing::debug_span!("ensure_runtime_layer").entered();
    if world.contains_resource::<VfxGpuHandle>() {
        return;
    }
    let Some(device) = world.get_resource::<RenderDevice>().cloned() else {
        return;
    };
    let profile = world.resource::<VfxSettings>().profile;
    // A build that failed is retried only for another device or profile, not every frame.
    if world
        .get_resource::<VfxGpuFailed>()
        .is_some_and(|failed| failed.device == device.device_id() && failed.profile == profile)
    {
        return;
    }
    let queue = world
        .get_resource::<regia_render::PipelineWarmQueue>()
        .filter(|q| q.is_started() && q.live_workers() > 0 && q.device_id() == Some(device.device_id()))
        .cloned();
    let built = if now {
        let finished = world
            .remove_resource::<PendingVfxGpu>()
            .filter(|pending| pending.device == Some(device.device_id()))
            .and_then(|pending| pending.slot.lock().ok().and_then(|mut slot| slot.take()));
        finished.unwrap_or_else(|| VfxGpu::new(&device, profile))
    } else if let Some(queue) = queue {
        let formats = runtime_formats(world, &device);
        let pending = world.get_resource_or_insert_with(PendingVfxGpu::default);
        if pending.device != Some(device.device_id()) {
            let slot = Arc::new(Mutex::new(None));
            let job_slot = Arc::clone(&slot);
            // Map tier, not boot: about 25 compiles must not hold back the first present or
            // the menu's jobs on a one- or two-worker device. Queued at the first `Update`, it
            // still runs before any game-frame warm (first in, first out within a tier).
            queue.push(
                regia_render::WarmTier::Map,
                "vfx context",
                move |device: &RenderDevice| {
                    let result = match VfxGpu::new_warmed(device, profile, &formats) {
                        Ok(Some(gpu)) => Ok(gpu),
                        // The app is quitting: nothing to install.
                        Ok(None) => return Err("canceled at shutdown".into()),
                        Err(err) => Err(err),
                    };
                    let error = result.as_ref().err().map(|e| format!("vfx context: {e}"));
                    if let Ok(mut slot) = job_slot.lock() {
                        *slot = Some(result);
                    }
                    error.map_or(Ok(()), Err)
                },
            );
            // The layer is installed when the job returns; a game-frame warm that runs first
            // still builds the decal receiver.
            if let Some(mut renderer) = world.get_resource_mut::<RuntimeRenderer>() {
                renderer.0.set_warm_decals(true);
            }
            let mut pending = world.resource_mut::<PendingVfxGpu>();
            pending.device = Some(device.device_id());
            pending.slot = slot;
            pending.since = Some(std::time::Instant::now());
            return;
        }
        let late = pending
            .since
            .is_some_and(|since| since.elapsed() > VFX_WORKER_BUILD_LIMIT);
        match pending.slot.lock().ok().and_then(|mut slot| slot.take()) {
            Some(result) => {
                world.remove_resource::<PendingVfxGpu>();
                result
            }
            None if late => {
                tracing::warn!(target: "regia_vfx", "vfx context build on a warm worker not back after {} s; building it here", VFX_WORKER_BUILD_LIMIT.as_secs());
                world.remove_resource::<PendingVfxGpu>();
                VfxGpu::new(&device, profile)
            }
            None => return,
        }
    } else {
        VfxGpu::new(&device, profile)
    };
    let gpu = match built {
        Ok(gpu) => gpu,
        Err(err) => {
            match err {
                VfxError::Shader(err) => {
                    tracing::error!(target: "regia_vfx", "vfx GPU init failed: {err}");
                }
                VfxError::Capacity(cap) => {
                    tracing::error!(target: "regia_vfx", "vfx pool capacity {cap} out of range");
                }
                err @ VfxError::TextureSize { .. } => {
                    tracing::error!(target: "regia_vfx", "vfx GPU init failed: {err}");
                }
            }
            world.insert_resource(VfxGpuFailed {
                device: device.device_id(),
                profile,
            });
            return;
        }
    };
    let handle = VfxGpuHandle {
        gpu: Arc::new(Mutex::new(gpu)),
        dt: Arc::new(Mutex::new(1.0 / 60.0)),
        rendered: Arc::new(Mutex::new(None)),
        stepped: Arc::new(AtomicBool::new(false)),
        forward: Arc::default(),
    };
    let precip = world.get_resource::<PrecipitationHandle>().cloned();
    if let Some(precip) = &precip
        && let Ok(mut slot) = precip.gpu.lock()
    {
        match PrecipitationGpu::new(&device, &profile) {
            Ok(built) => *slot = Some(built),
            Err(err) => tracing::error!(target: "regia_vfx", "precipitation GPU init failed: {err:?}"),
        }
    }
    let ambient = world.get_resource::<AmbientLifeHandle>().cloned();
    if let Some(ambient) = &ambient
        && let Ok(mut slot) = ambient.gpu.lock()
    {
        match AmbientLifeGpu::new(&device, &profile) {
            Ok(built) => *slot = Some(built),
            Err(err) => tracing::error!(target: "regia_vfx", "ambient life GPU init failed: {err:?}"),
        }
    }
    let mut layer = runtime_layer(&handle, precip, Some(device.queue.clone()));
    layer.ambient = ambient;
    world
        .resource_mut::<RuntimeRenderer>()
        .0
        .set_translucent_layers(vec![Arc::new(layer)]);
    world.insert_resource(handle);
    tracing::info!(target: "regia_vfx", "vfx runtime layer installed");
}

/// Tells the renderer the tier's occlusion view, and warms the precipitation's pipelines once the
/// project has it on, off the frame path, so the first drop compiles nothing.
#[allow(clippy::too_many_arguments)] // ECS systems pull their inputs as parameters
fn drive_precipitation_runtime(
    settings: Option<Res<crate::precip::PrecipitationSettings>>,
    config: Option<Res<crate::weather::WeatherConfig>>,
    surface: Option<Res<regia_scene::weather::SurfaceWeather>>,
    renderer: Option<ResMut<RuntimeRenderer>>,
    device: Option<Res<RenderDevice>>,
    render_settings: Option<Res<regia_render::RenderSettings>>,
    surfaces: Option<Res<WindowSurfaces>>,
    handle: Option<Res<PrecipitationHandle>>,
    queue: Option<Res<regia_render::PipelineWarmQueue>>,
    mut warmed: Local<bool>,
    mut report_out: Option<ResMut<crate::precip::PrecipitationReport>>,
) {
    let (Some(settings), Some(mut renderer), Some(device), Some(handle)) =
        (settings, renderer, device, handle)
    else {
        return;
    };
    let wanted = settings.enabled || surface.is_some();
    renderer
        .0
        .set_warm_surface_weather(surface.is_some() || config.as_deref().is_some_and(|c| c.effects.surface));
    let budget = report_out.as_deref().and_then(|r| r.budget.occlusion);
    renderer.0.set_precipitation_occlusion_budget(
        budget.filter(|_| wanted && settings.occlusion),
        device.device.limits().max_texture_dimension_2d,
    );
    if let Some(report) = report_out.as_deref_mut() {
        let occlusion = renderer.0.precipitation_occlusion();
        if report.occlusion != occlusion {
            report.occlusion = occlusion;
        }
    }
    if !*warmed && settings.enabled {
        *warmed = true;
        let window = surfaces
            .as_deref()
            .and_then(WindowSurfaces::primary_format)
            .unwrap_or(wgpu::TextureFormat::Bgra8UnormSrgb);
        let mut formats = VfxGpu::warm_formats(window);
        if let Some(hdr) = render_settings
            .as_deref()
            .map(|settings| settings.profile.hdr_color_format(&device.capabilities))
            .filter(|hdr| !formats.contains(hdr))
        {
            formats.push(hdr);
        }
        let gpu = Arc::clone(&handle.gpu);
        // A tier (or a run-time choice) at half or quarter size draws through its own target
        // and a composite (ADR-3249): warm those too.
        let tier_resolution = report_out
            .as_deref()
            .map_or(crate::precip::PrecipResolution::Full, |r| r.budget.resolution);
        let reduced = settings.resolution.unwrap_or(tier_resolution) != crate::precip::PrecipResolution::Full;
        let warm = move |gpu: &mut crate::precip::PrecipitationGpu| {
            gpu.warm(&formats);
            if reduced {
                gpu.warm_reduced(&formats);
            }
        };
        match queue
            .as_deref()
            .filter(|q| q.is_started() && q.live_workers() > 0)
        {
            Some(queue) => {
                queue.push(
                    regia_render::WarmTier::Map,
                    "precipitation",
                    move |_device: &RenderDevice| {
                        if let Ok(mut gpu) = gpu.lock()
                            && let Some(gpu) = gpu.as_mut()
                        {
                            warm(gpu);
                        }
                        Ok(())
                    },
                );
            }
            None => {
                if let Ok(mut gpu) = gpu.lock()
                    && let Some(gpu) = gpu.as_mut()
                {
                    warm(gpu);
                }
            }
        }
    }
}

/// Uploads this frame's CPU state into the GPU context: emitter records, spawn jobs, decals,
/// trails, delta time and the viewer (LOD/shake read it one frame early — documented in
/// `plugin`).
#[allow(clippy::too_many_arguments)] // ECS systems pull their inputs as parameters
pub(crate) fn upload_runtime_frame(
    time: Res<Time>,
    handle: Option<Res<VfxGpuHandle>>,
    mut scene: ResMut<VfxScene>,
    decals: Res<DecalPool>,
    library: Res<crate::decal_format::DecalLibrary>,
    mut trails: ResMut<VfxTrails>,
    extracted: Option<Res<ExtractedFrame>>,
    surfaces: Option<Res<WindowSurfaces>>,
    mut view: ResMut<VfxView>,
    mut cull: Option<ResMut<DecalCullStats>>,
    lights: Option<Res<crate::lights::VfxFrameLights>>,
    look: Option<Res<crate::plugin::VfxLook>>,
    (hidden, mut decal_scratch): (
        Option<Res<crate::decal::HiddenStaticDecals>>,
        Local<crate::decal::DecalDrawScratch>,
    ),
) {
    let _span = tracing::debug_span!("upload_runtime_frame").entered();
    let look = look.as_deref().copied().unwrap_or_default();
    let Some(handle) = handle else { return };
    let Some(mut gpu) = handle.lock() else { return };
    // Previous frame's GPU cull (this upload happens before `set_frame` bins the new list).
    if let Some(cull) = cull.as_deref_mut() {
        let (_pending, culled, tiles, _drawn) = gpu.decal_budget();
        *cull = DecalCullStats {
            capacity: decals.capacity(),
            live: u32::try_from(decals.len()).unwrap_or(u32::MAX),
            culled,
            tiles_touched: tiles,
        };
    }
    handle.begin_frame(time.delta_secs());

    gpu.set_time_held(time.delta_secs() <= 0.0);
    scene.upload(&mut gpu);
    if let Some(res) = look.soft_particle_res {
        gpu.set_soft_particle_res(res);
    }
    // The strongest particle light also lights the smoke (lit puffs).
    gpu.set_haze_look(look.heat_haze, look.haze_scale);
    let scale = look.fire_light_scale.max(0.0);
    gpu.set_fire_light(
        lights
            .as_deref()
            .filter(|_| look.fire_light)
            .and_then(|l| l.lights.first())
            .map(|l| (l.position, l.color.map(|c| c * scale), l.radius)),
    );
    // Hidden static decals are left out here, at draw time; the pool keeps them (ADR-7255).
    let hidden_ids = hidden.as_deref().map_or(&[][..], |hidden| hidden.ids());
    let (staged, staged_surfaces) = decals.visible_in_spawn_order(hidden_ids, &mut decal_scratch);
    // With the frame's forward-decal binding the forward pass takes what it can shade
    // (ADR-3255); without it every decal stays on the projected pass, as before.
    gpu.set_decal_texture_atlas(Arc::clone(library.texture_atlas()));
    crate::decal_forward::set_texture_atlas(&handle.forward, library.texture_atlas());
    match crate::decal_forward::route_for_upload(&handle.forward, &staged, &staged_surfaces) {
        Some(route) => gpu.write_decals_with(&route.post, &route.post_surfaces),
        None => gpu.write_decals_with(&staged, &staged_surfaces),
    }
    // `.vfx.ron` v7 ribbon emitters ride the trail pipelines after the game's own trails.
    trails.0.extend_from_slice(scene.ribbon_vertices());
    gpu.write_trails(&trails.0);
    trails.0.clear();

    // Viewer from the last extracted camera (one frame of latency: fine for LOD + shake
    // falloff). M5 stores cameras on `ExtractedFrame::views` (window views first);
    // the primary view drives LOD and shake. Viewport from the primary surface.
    let (width, height) = surfaces
        .as_deref()
        .and_then(|s| s.primary_size())
        .unwrap_or((1280, 720));
    // A new window size: the layer is live for a few frames so the frame builds its targets at
    // that size, and the first effect after the resize creates none (ADR-2585).
    if (view.width, view.height) != (width, height) {
        gpu.set_layer_warm_frames(crate::gpu::LAYER_WARM_FRAMES);
    }
    if let Some(frame) = extracted.as_deref()
        && let Some(sun) = frame.directional.as_ref()
    {
        // `direction` travels from the sun toward the scene. Particles want the opposite.
        let ambient = (frame.environment.ambient_lux / 20_000.0).clamp(0.15, 1.0);
        gpu.set_lighting(VfxLighting {
            ambient,
            exposure: 1.0,
            ..VfxLighting::default()
        });
        gpu.set_sun(
            [-sun.direction[0], -sun.direction[1], -sun.direction[2]],
            sun.color,
            sun.illuminance,
            None,
        );
    }
    if let Some(camera) = extracted
        .as_deref()
        .and_then(|frame| frame.views.first())
        .map(|view| view.camera)
    {
        view.camera = VfxCamera {
            eye: Vec3::from_array(camera.eye),
            target: Vec3::from_array(camera.target),
            fov_y: camera.fov_y,
            near: camera.near,
            far: camera.far,
        };
    } else if let Some(camera) = handle.rendered.lock().ok().and_then(|rendered| *rendered) {
        // No ECS camera: a host rendering its own camera through the frame hook. LOD and
        // shake follow the camera the effects were last drawn with, not the default view.
        view.camera = camera;
    }
    view.width = width;
    view.height = height;
}

/// Warms ambient life's pipelines once the project turns it on, off the frame path, so the first
/// creature compiles nothing.
fn drive_ambient_runtime(
    settings: Option<Res<crate::ambient::AmbientLifeSettings>>,
    device: Option<Res<RenderDevice>>,
    render_settings: Option<Res<regia_render::RenderSettings>>,
    surfaces: Option<Res<WindowSurfaces>>,
    handle: Option<Res<AmbientLifeHandle>>,
    queue: Option<Res<regia_render::PipelineWarmQueue>>,
    mut warmed: Local<bool>,
) {
    let (Some(settings), Some(device), Some(handle)) = (settings, device, handle) else {
        return;
    };
    if *warmed || !settings.enabled {
        return;
    }
    // A tier without ambient life compiles nothing for it.
    if crate::ambient::AmbientBudget::for_quality(crate::ambient::ambient_tier(render_settings.as_deref()))
        .is_off()
    {
        return;
    }
    *warmed = true;
    let window = surfaces
        .as_deref()
        .and_then(WindowSurfaces::primary_format)
        .unwrap_or(wgpu::TextureFormat::Bgra8UnormSrgb);
    let mut formats = VfxGpu::warm_formats(window);
    if let Some(hdr) = render_settings
        .as_deref()
        .map(|settings| settings.profile.hdr_color_format(&device.capabilities))
        .filter(|hdr| !formats.contains(hdr))
    {
        formats.push(hdr);
    }
    let gpu = Arc::clone(&handle.gpu);
    let warm = move || {
        if let Ok(mut gpu) = gpu.lock()
            && let Some(gpu) = gpu.as_mut()
        {
            gpu.warm(&formats);
        }
    };
    match queue
        .as_deref()
        .filter(|q| q.is_started() && q.live_workers() > 0)
    {
        Some(queue) => {
            queue.push(
                regia_render::WarmTier::Map,
                "ambient life",
                move |_device: &RenderDevice| {
                    warm();
                    Ok(())
                },
            );
        }
        None => warm(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_step_is_taken_once_and_a_still_render_takes_none() {
        let mark = AtomicBool::new(false);
        assert_eq!(take_frame_step(0.0, Some(&mark)), 0.0);
        assert!(!mark.load(Ordering::Acquire), "a still render leaves the step");
        assert_eq!(take_frame_step(0.25, Some(&mark)), 0.25);
        assert_eq!(take_frame_step(0.25, Some(&mark)), 0.0, "the second render draws");
        // A host that sets its own step steps every render.
        assert_eq!(take_frame_step(0.25, None), 0.25);
        assert_eq!(take_frame_step(0.25, None), 0.25);
    }

    /// ADR-2687 (audit renderout-state finding 8): the particle simulation steps once per game
    /// frame in whichever render of the frame comes first. Before, a render-out job in a windowed
    /// app drew with a step of 0, assuming the window had stepped: a window that did not draw
    /// (minimized, occluded, its surface not ready) froze the job's particles, and every render
    /// beside the window's (a second window) stepped them again.
    #[test]
    fn the_particles_step_once_per_game_frame_whichever_render_comes_first() {
        let device = match RenderDevice::shared_headless() {
            Ok(device) => device,
            Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
        };
        let mut gpu = VfxGpu::new(&device, VfxProfile::desktop_high()).expect("vfx gpu");
        // Live with nothing in it, so every render simulates.
        gpu.set_layer_warm_frames(1000);
        let handle = VfxGpuHandle {
            gpu: Arc::new(Mutex::new(gpu)),
            dt: Arc::new(Mutex::new(1.0 / 60.0)),
            rendered: Arc::new(Mutex::new(None)),
            stepped: Arc::new(AtomicBool::new(false)),
            forward: Arc::default(),
        };
        let layer = runtime_layer(&handle, None, None);
        let depth = device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("vfx step test depth"),
            size: wgpu::Extent3d {
                width: 32,
                height: 32,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Depth32Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let depth = depth.create_view(&wgpu::TextureViewDescriptor::default());
        let camera = LayerCamera {
            eye: [0.0, 2.0, -10.0],
            target: [0.0; 3],
            fov_y: 1.0,
            near: 0.1,
            far: 500.0,
            width: 32,
            height: 32,
            exposure: 1.0,
            display_exposure: 1.0,
            hdr: false,
            sun_direction: [0.0, 1.0, 0.0],
            sun_color: [1.0; 3],
            sun_illuminance: 0.0,
            sky_ambient: None,
        };
        let render = || {
            let mut encoder = device
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            layer.simulate(&depth, &camera, &mut encoder);
            device.queue.submit([encoder.finish()]);
        };
        let clock = || handle.lock().expect("context").sim_clock();
        let dt = 0.125;

        // A frame the window drew, then a render-out job drew: one step.
        handle.begin_frame(dt);
        render();
        assert!(handle.frame_stepped());
        render();
        assert_eq!(clock(), dt, "the second render of the frame did not step again");

        // A frame the window did not draw: the job's render takes the step.
        handle.begin_frame(dt);
        assert!(!handle.frame_stepped());
        render();
        assert_eq!(clock(), 2.0 * dt);

        // A still render (a meter render, step 0 set) takes nothing; the frame's render steps.
        handle.begin_frame(dt);
        handle.set_step_seconds(0.0);
        render();
        assert_eq!(clock(), 2.0 * dt);
        assert!(!handle.frame_stepped());
        handle.set_step_seconds(dt);
        render();
        assert_eq!(clock(), 3.0 * dt);
    }

    /// ADR-2741: a render-out job gives the context no warm frames. A host frame simulates a layer
    /// that is live (ADR-2585), and every frame it simulates moves the particle clock, the heat-haze
    /// phase. A new context's first job simulated its 8 warm frames with nothing in them, so its
    /// first spawn came at another clock than the same spawn in a later job, whose context had spent
    /// them: Red-Rouge's first held pair on lavapipe differed from the impact on (15 of 24 frames
    /// equal), later pairs did not. The app's warm frames come back when the job ends.
    #[test]
    fn a_job_start_leaves_no_warm_frame_to_move_the_particle_clock() {
        let device = match RenderDevice::shared_headless() {
            Ok(device) => device,
            Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
        };
        let depth = device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("vfx warm test depth"),
            size: wgpu::Extent3d {
                width: 32,
                height: 32,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Depth32Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let depth = depth.create_view(&wgpu::TextureViewDescriptor::default());
        let camera = LayerCamera {
            eye: [0.0, 2.0, -10.0],
            target: [0.0; 3],
            fov_y: 1.0,
            near: 0.1,
            far: 500.0,
            width: 32,
            height: 32,
            exposure: 1.0,
            display_exposure: 1.0,
            hdr: false,
            sun_direction: [0.0, 1.0, 0.0],
            sun_color: [1.0; 3],
            sun_illuminance: 0.0,
            sky_ambient: None,
        };
        let dt = 0.125;
        // A job of 12 game frames with nothing spawned, on a context with `warm` warm frames left:
        // the particle clock at its end, and the warm frames the app has after it.
        let job = |warm: u32, resized_on: Option<u32>| {
            let mut gpu = VfxGpu::new(&device, VfxProfile::desktop_high()).expect("vfx gpu");
            gpu.set_layer_warm_frames(warm);
            let handle = VfxGpuHandle {
                gpu: Arc::new(Mutex::new(gpu)),
                dt: Arc::new(Mutex::new(dt)),
                rendered: Arc::new(Mutex::new(None)),
                stepped: Arc::new(AtomicBool::new(false)),
                forward: Arc::default(),
            };
            let layer = runtime_layer(&handle, None, None);
            let mut world = World::new();
            world.insert_resource(handle.clone());
            let start = crate::live_state::EffectsJobStart::restart(&mut world);
            for frame in 0..12 {
                handle.begin_frame(dt);
                // What `upload_runtime_frame` does when the window's size changed.
                if resized_on == Some(frame) {
                    let mut gpu = handle.lock().expect("context");
                    gpu.set_layer_warm_frames(crate::gpu::LAYER_WARM_FRAMES);
                }
                // The host frame: a layer with nothing to draw leaves it (ADR-2585).
                if layer.is_active() {
                    let mut encoder = device
                        .device
                        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
                    layer.simulate(&depth, &camera, &mut encoder);
                    device.queue.submit([encoder.finish()]);
                }
            }
            let clock = handle.lock().expect("context").sim_clock();
            start.finish(&mut world);
            let warm_after = handle.lock().expect("context").layer_warm_frames();
            (clock, warm_after)
        };
        let (first_job_clock, warm_after_first) = job(crate::gpu::LAYER_WARM_FRAMES, None);
        let (later_job_clock, warm_after_later) = job(0, None);
        assert_eq!(
            first_job_clock, later_job_clock,
            "a new context's first job ran the particle clock through its warm frames"
        );
        assert_eq!(first_job_clock, 0.0, "nothing lived, so the clock did not move");
        assert_eq!(
            warm_after_first,
            crate::gpu::LAYER_WARM_FRAMES,
            "the app's warm frames came back"
        );
        assert_eq!(warm_after_later, 0);

        // A window resized during the job arms warm frames for the window's own targets. They wait
        // for the job's end: the job's clock does not depend on when someone dragged the window.
        let (resized_job_clock, warm_after_resize) = job(0, Some(6));
        assert_eq!(
            resized_job_clock, 0.0,
            "a window resized during the job moved the particle clock"
        );
        assert_eq!(
            warm_after_resize,
            crate::gpu::LAYER_WARM_FRAMES,
            "the resize's warm frames reach the app after the job"
        );
    }

    /// `VfxLook::fire_light` off pushes no particle light; the scale multiplies lumens.
    #[test]
    fn fire_light_look_gates_and_scales_the_pushed_lights() {
        let mut lights = crate::lights::VfxFrameLights::default();
        lights.gather(
            vec![crate::lights::ParticleLight {
                position: Vec3::new(0.0, 1.0, 0.0),
                color: [10.0, 6.0, 2.0],
                radius: 8.0,
            }],
            4,
        );
        let lumens = |look: crate::plugin::VfxLook| {
            let mut app = App::new();
            app.insert_resource(lights.clone());
            app.insert_resource(look);
            app.init_resource::<ExtractedFrame>();
            app.add_systems(Update, push_particle_lights);
            app.update();
            app.world()
                .resource::<ExtractedFrame>()
                .local_lights
                .iter()
                .map(|light| match light {
                    LabLocalLight::Point(p) => p.intensity,
                    LabLocalLight::Spot(s) => s.intensity,
                })
                .sum::<f32>()
        };
        let base = lumens(crate::plugin::VfxLook::default());
        assert!(
            (base - 10.0 * PARTICLE_LIGHT_LUMENS_PER_UNIT).abs() < 1.0,
            "{base}"
        );
        let doubled = lumens(crate::plugin::VfxLook {
            fire_light_scale: 2.0,
            ..Default::default()
        });
        assert!((doubled - 2.0 * base).abs() < 1.0, "{doubled}");
        let off = lumens(crate::plugin::VfxLook {
            fire_light: false,
            ..Default::default()
        });
        assert_eq!(off, 0.0);
    }

    /// A rate emitter's light is lumens of its brightest linear channel, not candela of `intensity`.
    #[test]
    fn a_rate_light_is_lumens_of_its_brightest_linear_channel() {
        let system = |name: &str, color: &str| {
            let text = format!(
                r#"(
                    version: 1,
                    name: "{name}",
                    system: (budget: 8),
                    emitters: [(
                        name: "puff",
                        spawn: (mode: Rate(1.0), lifetime: (min: 1.0, max: 1.0)),
                        init: (shape: Point, size: (min: 0.1, max: 0.1)),
                        render: (mode: Billboard),
                    )],
                )"#
            );
            let mut def = crate::format::VfxSystemDef::from_ron(&text).expect("ron");
            def.emitters[0].render.light = Some(crate::format::ParticleLightDef {
                color: color.into(),
                intensity: 1.0,
                radius: 4.0,
            });
            def
        };
        let lumens_of = |name: &str, color: &str| {
            let mut lib = crate::scene::VfxLibrary::new();
            lib.add(system(name, color)).expect("def");
            let mut scene = crate::scene::VfxScene::new(lib);
            scene.spawn(name, Vec3::ZERO, Vec3::Z, 1).expect("spawn");
            let mut frame = crate::lights::VfxFrameLights::default();
            scene.gather_lights(&mut frame, 4);
            let packed = particle_point_lights_scaled(&frame, 1.0);
            match packed.first() {
                Some(LabLocalLight::Point(light)) => light.intensity,
                other => panic!("expected one point light, got {other:?}"),
            }
        };
        let full = lumens_of("glow", "#ffffff");
        assert!((full - PARTICLE_LIGHT_LUMENS_PER_UNIT).abs() < 1.0, "{full}");
        let linear = crate::format::parse_color("#808080").expect("hex");
        let peak = linear[0].max(linear[1]).max(linear[2]);
        let dim = lumens_of("dim", "#808080");
        assert!(
            (dim - peak * PARTICLE_LIGHT_LUMENS_PER_UNIT).abs() < 1.0,
            "{dim} peak {peak}"
        );
    }

    #[test]
    fn only_a_flash_has_a_sphere_source() {
        // Fires and sparks (a few metres to tens of metres) stay point lights.
        for r in [0.5, 8.0, 40.0, 399.0, f32::NAN, f32::INFINITY, -3.0] {
            assert_eq!(particle_light_source_radius(r), None, "{r}");
        }
        // The 250 t crash flash reaches 80 R: its source is the fireball.
        let r250 = crate::crash::CrashScale::SHOWCASE.fireball_radius();
        let Some(source) = particle_light_source_radius(80.0 * r250) else {
            panic!("a 250 t flash has a source");
        };
        assert!((source - r250).abs() < 1.0e-3 * r250, "{source} vs {r250}");
        // The smallest flash reach (400 m) has a 5 m source.
        assert_eq!(
            particle_light_source_radius(FLASH_MIN_RADIUS_M),
            Some(FLASH_MIN_RADIUS_M / FLASH_RADIUS_PER_SOURCE)
        );
        // The packed light carries it.
        let mut lights = crate::lights::VfxFrameLights::default();
        lights.gather(
            vec![crate::lights::ParticleLight {
                position: Vec3::ZERO,
                color: [1.0; 3],
                radius: 80.0 * r250,
            }],
            4,
        );
        let packed = particle_point_lights(&lights);
        match packed[0] {
            LabLocalLight::Point(p) => assert_eq!(p.source_radius, Some(source)),
            LabLocalLight::Spot(_) => panic!("a particle light is a point"),
        }
    }

    /// The plugin builds against a headless app and installs the GPU context once the
    /// render device exists (the full frame path needs a real surface; the windowed
    /// `vfx_lab` exercises that).
    #[test]
    fn runtime_plugin_installs_handle() {
        let mut app = App::new();
        app.add_plugins((crate::VfxPlugin, VfxRuntimePlugin));
        app.update();
        if app.world().contains_resource::<RenderDevice>() {
            assert!(app.world().contains_resource::<VfxGpuHandle>());
            assert!(
                app.world().contains_resource::<VfxTrails>(),
                "trail upload resource initialized"
            );
        }
    }
}
