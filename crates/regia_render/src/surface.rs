// SPDX-License-Identifier: MPL-2.0
//! Window surfaces: created on resume, dropped on suspend (winit's Android contract), reconfigured on
//! resize, cleared and presented every frame.

use crate::frame_work::counted::*;
use std::collections::{BTreeMap, HashMap};

use regia_app::prelude::*;
use regia_ecs::ecs::message::MessageCursor;
use regia_platform::{
    AppLifecycle, DisplayHandle, LifecycleEvent, OsWindow, SurfaceObservation, Window, WindowHandles,
    WindowResized,
};

use crate::output::{HdrOutput, OutputSettings, PresentPolicy};
use crate::swapchain::{DisplayHdrSummary, SwapchainInfo, SwapchainRequest};
use crate::{
    ClearColor, ExtractedFrame, FrameRenderer, FrameTarget, RenderDevice, RenderError, RuntimeRenderer,
};

#[cfg(feature = "frame-gen")]
mod frame_gen_present;
#[cfg(all(feature = "frame-gen", feature = "test-hooks"))]
pub use frame_gen_present::{FrameGenTick, feed_frame_gen_tick};
#[cfg(feature = "frame-gen")]
pub(crate) use frame_gen_present::{HeldPresentMode, PacerBefore};

/// One window's surface and its configuration.
pub struct WindowSurface {
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    needs_configure: bool,
    /// The surface can be a copy source (screenshots).
    copy_src: bool,
    /// Calibration and HDR encoding of the finished frame (ADR-092).
    display: crate::display_stage::DisplayStage,
    /// What the swapchain plan was made from and what it chose (ADR-525, `render.swapchain`).
    info: SwapchainInfo,
    /// Which plan `info` holds (`next_plan_epoch`): a new one at creation and at every
    /// re-plan, so frame generation's held present mode can tell a host's newer choice from
    /// its own Mailbox override (ADR-7413).
    #[cfg(feature = "frame-gen")]
    plan_epoch: u64,
    /// The paused-scene hold's kept image (ADR-7890): empty for a new surface, so surface loss
    /// renders one scene frame before holding again.
    hold: crate::scene_hold::RetainedScene,
    /// Mappable buffers the window's screenshot copies reuse (ADR-8848); freed with the surface.
    readback: crate::screenshot::ReadbackBuffers,
}

/// A process-wide unique number for a swap-chain plan (`WindowSurface::plan_epoch`).
#[cfg(feature = "frame-gen")]
fn next_plan_epoch() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// The plan-bearing fields of a [`WindowSurface`], borrowed apart from its `wgpu::Surface`, so a
/// re-plan (`apply_plan`) and frame generation's present-mode hold run, and are tested, without
/// a window.
struct PlanFields<'a> {
    config: &'a mut wgpu::SurfaceConfiguration,
    needs_configure: &'a mut bool,
    info: &'a mut SwapchainInfo,
    #[cfg(feature = "frame-gen")]
    plan_epoch: &'a mut u64,
}

impl WindowSurface {
    /// The surface's [`PlanFields`].
    fn plan_fields(&mut self) -> PlanFields<'_> {
        PlanFields {
            config: &mut self.config,
            needs_configure: &mut self.needs_configure,
            info: &mut self.info,
            #[cfg(feature = "frame-gen")]
            plan_epoch: &mut self.plan_epoch,
        }
    }
}

/// Writes a new swap-chain plan into a surface: the configuration (marked for a configure when
/// it changed), the plan info, and with frame generation a new plan epoch, so a present mode
/// held from the previous plan is not put back over the host's new choice (ADR-7413).
fn apply_plan(fields: PlanFields<'_>, info: SwapchainInfo) {
    let plan = &info.plan;
    let config = fields.config;
    if config.format != plan.format
        || config.color_space != plan.color_space
        || config.present_mode != plan.present_mode
        || config.alpha_mode != plan.alpha_mode
    {
        config.format = plan.format;
        config.color_space = plan.color_space;
        config.present_mode = plan.present_mode;
        config.alpha_mode = plan.alpha_mode;
        *fields.needs_configure = true;
    }
    *fields.info = info;
    #[cfg(feature = "frame-gen")]
    {
        *fields.plan_epoch = next_plan_epoch();
    }
}

/// Surfaces by window entity, iterated in entity order (`BTreeMap`, so every per-surface
/// loop is run-to-run deterministic). Empty while the app is suspended.
#[derive(Resource, Default)]
pub struct WindowSurfaces {
    /// A `BTreeMap` (not a `HashMap`): with two or more live windows the iteration order
    /// must not change per launch — the present loop, splash and warm paths all read it.
    surfaces: BTreeMap<Entity, WindowSurface>,
    /// The window with the [`regia_platform::PrimaryWindow`] marker that had its surface
    /// created last; what `primary_entity` prefers while its surface is live.
    primary_window: Option<Entity>,
    /// Log the "first frame presented after resume" marker on the next successful present.
    announce_present: bool,
    /// Surfaces reported `Lost` and must be recreated.
    lost: Vec<Entity>,
    /// Frames in a row that saw a lost surface (or a failed re-creation) and no successful
    /// acquire; at [`SURFACE_LOST_LIMIT`] the device counts as lost.
    lost_streak: u32,
    /// Surfaces created since start (the surface audit, ADR-527).
    created_total: u64,
    /// Surfaces dropped since start (suspend, exit, lost and recreated).
    dropped_total: u64,
}

/// Frames in a row with a lost window surface and no successful acquire after which the
/// device counts as lost (`gpu_health`): a surface that cannot come back is a black window.
const SURFACE_LOST_LIMIT: u32 = 120;

/// Called once, on the frame the render plugin notices the GPU device is lost
/// ([`crate::device_lost`]), before it asks the app to exit with
/// [`crate::DEVICE_LOST_EXIT_CODE`]. The feedback plugin adds one that writes a crash bundle.
pub type DeviceLostHook = Box<dyn Fn(&mut World, &crate::DeviceLostInfo) + Send + Sync>;

/// What runs when the GPU device is lost ([`DeviceLostHook`]), in the order added.
#[derive(Resource, Default)]
pub struct DeviceLostHooks {
    hooks: Vec<DeviceLostHook>,
}

impl DeviceLostHooks {
    /// Adds a hook.
    pub fn add(&mut self, hook: impl Fn(&mut World, &crate::DeviceLostInfo) + Send + Sync + 'static) {
        self.hooks.push(Box::new(hook));
    }

    /// Number of hooks.
    #[must_use]
    pub fn len(&self) -> usize {
        self.hooks.len()
    }

    /// No hooks.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }
}

/// `Render`, while [`crate::device_lost`]: the frame systems no longer run (nothing is drawn
/// on the dead device). Once: logs one error, runs the [`DeviceLostHooks`] (the crash bundle),
/// begins the deliberate exit ([`regia_platform::begin_deliberate_exit`]: the native crash
/// handler is disarmed and an exit watchdog armed) and writes [`AppExit::Error`] with
/// [`crate::DeviceLostInfo::exit_code`] (3 for a lost device, 4 for no window surface); the
/// runner then exits through its normal exit transitions, and the host ends the process with
/// the code without running process-exit handlers ([`regia_platform::finish_deliberate_exit`],
/// ADR-276).
pub(crate) fn handle_device_lost(world: &mut World, mut handled: Local<bool>) {
    if *handled {
        return;
    }
    *handled = true;
    exit_for_lost_device(world, |code| {
        if regia_platform::begin_deliberate_exit(code) {
            // A teardown stuck on the dead device still ends with the documented code.
            let _ = regia_platform::arm_exit_watchdog(DEVICE_LOST_EXIT_WATCHDOG, code);
        }
    });
}

/// After a device loss the whole exit (clean exit, app drop) must finish within this, or the
/// exit watchdog ends the process with the device-lost exit code.
const DEVICE_LOST_EXIT_WATCHDOG: std::time::Duration = std::time::Duration::from_secs(15);

/// [`handle_device_lost`]'s body, in its required order: the hooks (the crash bundle is written),
/// then `begin_exit(code)` (the native crash handler is disarmed only once the bundle is on
/// disk), then the exit request.
fn exit_for_lost_device(world: &mut World, begin_exit: impl FnOnce(i32)) {
    let info = crate::gpu_health::device_lost_info().unwrap_or(crate::DeviceLostInfo {
        reason: "unknown",
        message: String::new(),
    });
    tracing::error!(
        target: "regia_render",
        reason = info.reason,
        gpu_errors = crate::gpu_health::gpu_error_count(),
        "{}: GPU device lost or no window surface ({}): {}. Rendering stopped; writing a crash report and \
         exiting with code {}. Update or reinstall the GPU driver if this repeats.",
        info.kind(),
        info.reason,
        info.message,
        info.exit_code()
    );
    // A crash report the next launch uploads (`regia_crash`), naming the passes the GPU was most
    // likely inside (ADR-2307); written only when the platform configured a crash directory.
    let _ = regia_platform::crash::write_device_lost_report(
        info.kind(),
        info.reason,
        &info.message,
        info.exit_code(),
    );
    if let Some(hooks) = world.remove_resource::<DeviceLostHooks>() {
        for hook in &hooks.hooks {
            hook(world, &info);
        }
        world.insert_resource(hooks);
    }
    let code = std::num::NonZeroU8::new(info.exit_code()).unwrap_or(std::num::NonZeroU8::MIN);
    begin_exit(i32::from(code.get()));
    world.write_message(AppExit::Error(code));
}

impl WindowSurfaces {
    /// Number of live surfaces.
    pub fn len(&self) -> usize {
        self.surfaces.len()
    }
    /// True when no surface exists (for example while suspended).
    pub fn is_empty(&self) -> bool {
        self.surfaces.is_empty()
    }

    /// The entity whose surface drives shared per-frame state: [`Self::primary_size`],
    /// [`Self::primary_format`], the published `LodViewport` and the local-capture bake
    /// (audit G, render5 determinism: never map iteration order).
    fn primary_entity(&self) -> Option<Entity> {
        pick_primary(self.primary_window, self.surfaces.keys().copied())
    }

    /// Physical size of the primary window's surface (`None` while suspended / before create).
    ///
    /// UI layout should prefer this over window-resize messages alone: on some compositors the
    /// `WindowResized` size can drift from the wgpu surface config the overlay actually draws into.
    pub fn primary_size(&self) -> Option<(u32, u32)> {
        self.primary_entity()
            .and_then(|entity| self.surfaces.get(&entity))
            .map(|s| (s.config.width, s.config.height))
    }

    /// Surfaces created since start (every resume creates one per window).
    #[must_use]
    pub fn created_total(&self) -> u64 {
        self.created_total
    }

    /// Surfaces dropped since start (suspend, exit, a lost surface before its re-creation).
    #[must_use]
    pub fn dropped_total(&self) -> u64 {
        self.dropped_total
    }

    /// Frames in a row that saw a lost surface and no successful acquire (0 when healthy).
    #[must_use]
    pub fn lost_streak(&self) -> u32 {
        self.lost_streak
    }

    /// Swapchain format of the primary window's surface (warm jobs render in it).
    pub fn primary_format(&self) -> Option<wgpu::TextureFormat> {
        self.primary_entity()
            .and_then(|entity| self.surfaces.get(&entity))
            .map(|s| s.config.format)
    }

    /// Physical size of a specific window's surface.
    pub fn size_of(&self, entity: Entity) -> Option<(u32, u32)> {
        self.surfaces
            .get(&entity)
            .map(|s| (s.config.width, s.config.height))
    }

    /// The present modes of `entity`'s live swapchain by name (ADR-7880); `None` without a surface
    /// (headless, suspended, before the first resume). Allocates nothing, so a per-frame reader may
    /// call it.
    #[must_use]
    pub fn present_summary(&self, entity: Entity) -> Option<crate::swapchain::PresentSummary> {
        self.surfaces.get(&entity).map(|s| s.info.present_summary())
    }

    /// Every live swapchain by window: what its plan was made from and what it chose
    /// ([`SwapchainInfo`], ADR-525). The size is the configured one. Ordered by window entity.
    pub fn swapchains(&self) -> Vec<(Entity, SwapchainInfo)> {
        let mut list: Vec<(Entity, SwapchainInfo)> = self
            .surfaces
            .iter()
            .map(|(&entity, s)| {
                let mut info = s.info.clone();
                info.size = (s.config.width, s.config.height);
                (entity, info)
            })
            .collect();
        list.sort_by_key(|(entity, _)| *entity);
        list
    }

    /// Every live window's scene hold (ADR-7890): its newest frame and its running counts, in
    /// window entity order (`render.scene_hold`).
    pub fn scene_holds(
        &self,
    ) -> Vec<(
        Entity,
        crate::scene_hold::SceneHoldFrame,
        crate::scene_hold::SceneHoldCounts,
    )> {
        self.surfaces
            .iter()
            .map(|(&entity, s)| (entity, s.hold.last_frame(), s.hold.counts()))
            .collect()
    }
}

/// The window that drives shared per-frame state: `primary` when it is among the live
/// `entities`, else the lowest entity — the same rule `regia_ui::scale::pick_startup_window`
/// uses for the startup scale, and deliberately never map/hash iteration order (audit G,
/// render5 determinism finding 1: two live windows used to make the UI scale, VFX targets,
/// mesh LOD and the local-capture bake a function of the `HashMap` seed).
fn pick_primary(primary: Option<Entity>, entities: impl Iterator<Item = Entity>) -> Option<Entity> {
    let mut lowest: Option<Entity> = None;
    let mut primary_live = false;
    for entity in entities {
        primary_live |= Some(entity) == primary;
        lowest = match lowest {
            Some(current) if current.to_bits() <= entity.to_bits() => Some(current),
            _ => Some(entity),
        };
    }
    if primary_live { primary } else { lowest }
}

/// A normalized camera viewport (x, y, w, h in 0..1) as a pixel rectangle on a
/// `width`×`height` target.
///
/// The window path composites split-screen views with this rule; a host that draws the
/// same views elsewhere (the editor's play window) must land on the same pixels, so it
/// is public.
#[must_use]
pub fn pixel_viewport(viewport: [f32; 4], width: u32, height: u32) -> [u32; 4] {
    // An origin of 1.0 would leave no pixels; keep it on the last pixel so the size clamp below
    // always has a non-empty range.
    let x = ((viewport[0].clamp(0.0, 1.0) * width as f32).floor() as u32).min(width.max(1) - 1);
    let y = ((viewport[1].clamp(0.0, 1.0) * height as f32).floor() as u32).min(height.max(1) - 1);
    let w = ((viewport[2].clamp(0.0, 1.0) * width as f32).ceil() as u32).clamp(1, width.max(1) - x);
    let h = ((viewport[3].clamp(0.0, 1.0) * height as f32).ceil() as u32).clamp(1, height.max(1) - y);
    [x, y, w, h]
}

/// What the display behind `surface` reports (wgpu `display_hdr_info`). Not per frame: on
/// Windows it walks the DXGI adapters and outputs.
fn display_report(surface: &wgpu::Surface<'_>, adapter: &wgpu::Adapter) -> DisplayHdrSummary {
    let _span = tracing::info_span!("display_hdr_info").entered();
    DisplayHdrSummary::from_info(&surface.display_hdr_info(adapter))
}

/// Plans a swapchain ([`SwapchainInfo::plan`]) and logs an HDR request that ended as SDR.
fn plan_surface(
    caps: &wgpu::SurfaceCapabilities,
    backend: wgpu::Backend,
    output: &OutputSettings,
    window: &Window,
    display: DisplayHdrSummary,
    size: (u32, u32),
) -> Option<SwapchainInfo> {
    let request = SwapchainRequest {
        backend,
        present: output.present,
        window_present: window.present_mode,
        hdr: output.hdr,
        display_hdr: display.hdr_mode,
    };
    let info = SwapchainInfo::plan(caps, request, display, size)?;
    if let Some(fallback) = info.plan.hdr_fallback {
        tracing::warn!(
            target: "regia_render",
            "HDR output {:?} not used ({}); using SDR {:?}",
            output.hdr,
            fallback.code(),
            info.plan.format
        );
    }
    Some(info)
}

/// While HDR is requested, the display's HDR mode is read again every this many frames: Windows
/// HDR can be switched on or off while the game runs.
const DISPLAY_POLL_FRAMES: u64 = 120;

/// A display report fills the calibration's display luminance where the platform left it
/// unknown (desktop has no platform report; Windows has DXGI through wgpu). While the calibration
/// is still the platform default, it takes the new peak and minimum, as
/// `calibration::query_display_luminance` does for the platform's report.
fn adopt_display_luminance(world: &mut World, display: &DisplayHdrSummary) {
    let current = world.get_resource::<crate::DisplayLuminance>().copied();
    let Some(merged) = crate::swapchain::merge_display_luminance(current, display) else {
        return;
    };
    world.insert_resource(merged);
    let platform = world
        .get_resource::<crate::DisplayPlatform>()
        .copied()
        .unwrap_or_else(crate::DisplayPlatform::current);
    let default = crate::DisplayCalibration::platform_default(platform);
    let previous = default.with_display(&current.unwrap_or_default());
    if let Some(mut calibration) = world.get_resource_mut::<crate::DisplayCalibration>()
        && (*calibration == default || *calibration == previous)
    {
        *calibration = default.with_display(&merged);
    }
}

fn create_surface(
    world: &mut World,
    entity: Entity,
    os_window: &std::sync::Arc<OsWindow>,
) -> Result<(), RenderError> {
    let window = world.get::<Window>(entity).cloned().unwrap_or_default();
    if world.get_resource::<RenderDevice>().is_none() {
        let display = world
            .get_resource::<DisplayHandle>()
            .cloned()
            .ok_or(RenderError::NoDisplay)?;
        let mut descriptor = wgpu::InstanceDescriptor::new_with_display_handle(Box::new(display.0));
        descriptor.backends = crate::device::instance_backends();
        descriptor.flags = crate::device::instance_flags();
        // The windowed app keeps its pipeline cache in `cache://pipelines` (`pipeline_cache`).
        if crate::pipeline_cache::directory().is_none()
            && let Some(dir) = world.get_resource::<regia_platform::Vfs>().and_then(|vfs| {
                regia_platform::VfsPath::parse("cache://pipelines")
                    .ok()
                    .and_then(|p| vfs.host_path(&p))
            })
        {
            crate::pipeline_cache::set_directory(Some(dir));
        }
        let instance = crate::device::create_instance(descriptor);
        crate::boot::mark_named("instance");
        let surface = instance
            .create_surface(os_window.clone())
            .map_err(RenderError::CreateSurface)?;
        // The render profile's traced sun or ReSTIR GI asks the device for ray queries.
        if let Some(settings) = world.get_resource::<crate::RenderSettings>() {
            crate::hwrt::set_device_request(if settings.profile.restir_gi {
                crate::hwrt::HwrtRequest::On
            } else {
                settings.profile.sun_shadow_trace.device_request()
            });
            // The single-pass visibility resolve needs the texture table (ADR-2071).
            if settings.profile.visibility_path.single_pass_resolve {
                crate::texture_table::set_device_level_request(crate::texture_table::TEXTURE_TABLE_LEVEL);
            }
        }
        let device = RenderDevice::new(instance, Some(&surface))?;
        // Loss, OOM and error counting work without the telemetry plugin (which replaces these).
        crate::gpu_health::install_device_hooks(&device.device);
        world.insert_resource(device);
        return insert_surface(world, entity, surface, &window, os_window);
    }
    let surface = world
        .resource::<RenderDevice>()
        .instance
        .create_surface(os_window.clone());
    let surface = surface.map_err(RenderError::CreateSurface)?;
    insert_surface(world, entity, surface, &window, os_window)
}

fn insert_surface(
    world: &mut World,
    entity: Entity,
    surface: wgpu::Surface<'static>,
    window: &Window,
    os_window: &OsWindow,
) -> Result<(), RenderError> {
    let device = world.resource::<RenderDevice>();
    let caps = surface.get_capabilities(&device.adapter);
    let output = world
        .get_resource::<OutputSettings>()
        .cloned()
        .unwrap_or_default();
    let display = display_report(&surface, &device.adapter);
    let size = os_window.inner_size();
    let backend = device.capabilities.backend;
    let Some(info) = plan_surface(
        &caps,
        backend,
        &output,
        window,
        display,
        (size.width, size.height),
    ) else {
        return Err(RenderError::SurfaceUnsupported(format!(
            "the surface of window {entity} reports no formats on this adapter"
        )));
    };
    let config = info.plan.configuration(size.width, size.height);
    let copy_src = info.plan.copy_src;
    tracing::info!(
        target: "regia_render",
        "surface created: {}x{} {:?} {:?} {:?} ({backend:?}, alpha {:?}, latency {})",
        config.width,
        config.height,
        config.format,
        config.color_space,
        config.present_mode,
        config.alpha_mode,
        config.desired_maximum_frame_latency
    );
    adopt_display_luminance(world, &display);
    let is_primary_window = world.get::<regia_platform::PrimaryWindow>(entity).is_some();
    let mut surfaces = world.resource_mut::<WindowSurfaces>();
    surfaces.created_total += 1;
    let replaced = surfaces.surfaces.insert(
        entity,
        WindowSurface {
            surface,
            config,
            needs_configure: true,
            copy_src,
            display: Default::default(),
            info,
            #[cfg(feature = "frame-gen")]
            plan_epoch: next_plan_epoch(),
            hold: Default::default(),
            readback: Default::default(),
        },
    );
    if is_primary_window {
        surfaces.primary_window = Some(entity);
    }
    if replaced.is_some() {
        surfaces.dropped_total += 1;
    }
    Ok(())
}

/// One more frame with a lost surface; at [`SURFACE_LOST_LIMIT`] the device counts as lost.
fn note_lost_frame(surfaces: &mut WindowSurfaces) {
    surfaces.lost_streak += 1;
    if surfaces.lost_streak == SURFACE_LOST_LIMIT {
        crate::gpu_health::mark_device_lost(
            crate::gpu_health::SURFACE_UNAVAILABLE,
            &format!("a window surface stayed lost for {SURFACE_LOST_LIMIT} frames"),
        );
    }
}

/// Runs `create`, and once more if it fails; the second error is returned.
fn with_one_retry<E: std::fmt::Display>(mut create: impl FnMut() -> Result<(), E>) -> Result<(), E> {
    create().or_else(|first| {
        tracing::warn!(target: "regia_render", "surface creation failed ({first}); retrying once");
        create()
    })
}

fn create_all_surfaces(world: &mut World) {
    let handles: Vec<_> = world
        .resource::<WindowHandles>()
        .iter()
        .map(|(entity, os)| (entity, os.clone()))
        .collect();
    for (entity, os_window) in handles {
        if world.resource::<WindowSurfaces>().surfaces.contains_key(&entity) {
            continue;
        }
        // One retry (a transient failure right after resume is cheap to retry); then the game
        // has nothing to present to and exits with a report instead of spinning behind a blank
        // window (`gpu_health::SURFACE_UNAVAILABLE`).
        if let Err(err) = with_one_retry(|| create_surface(world, entity, &os_window)) {
            tracing::error!(target: "regia_render", "cannot create surface for window {entity} (after a retry): {err}");
            crate::gpu_health::mark_device_lost(
                crate::gpu_health::SURFACE_UNAVAILABLE,
                &format!("cannot create a surface for window {entity}: {err}"),
            );
        }
    }
}

/// `Lifecycle`: drop surfaces on suspend/terminate (before the OS callback returns), recreate on resume.
pub(crate) fn on_lifecycle(world: &mut World) {
    let event = world.get_resource::<AppLifecycle>().and_then(|l| l.last_event);
    match event {
        Some(LifecycleEvent::Suspended | LifecycleEvent::WillTerminate) => {
            // audit-g-render3-platform: frame generation's present worker owns its own thread
            // and may still hold the last swap-chain image (it presents at the pacer's time).
            // The surfaces drop below, and an image must not be presented onto, or outlive, its
            // surface: cancel the worker first so it drops the image without presenting. The
            // worker restarts on first use after the resume.
            #[cfg(feature = "frame-gen")]
            if let Some(mut presenter) =
                world.get_resource_mut::<crate::frame_gen::present::FrameGenPresenter>()
            {
                presenter.cancel_present();
                // What it held was dropped unpresented: a scene prepare waiting on it re-arms
                // (ADR-7883).
                crate::scene_ready::note_present_flush();
            }
            let mut surfaces = world.resource_mut::<WindowSurfaces>();
            let n = surfaces.surfaces.len();
            if crate::gpu_health::device_lost() {
                // A lost device can leave a frame holding its swapchain acquire semaphore, and
                // wgpu-hal panics destroying the swapchain then (Adreno 643 after a GPU timeout,
                // ADR-1749). The deliberate exit follows, so the swapchains are leaked.
                let leaked: Vec<_> = std::mem::take(&mut surfaces.surfaces).into_values().collect();
                for surface in leaked {
                    std::mem::forget(surface);
                }
            } else {
                surfaces.surfaces.clear();
            }
            surfaces.primary_window = None;
            surfaces.dropped_total += n as u64;
            // The window captures' pooled buffers go with the surfaces (ADR-8848).
            crate::screenshot::release_screenshot_buffers();
            surfaces.lost.clear();
            // Lost frames before a pause do not count toward the streak after it.
            surfaces.lost_streak = 0;
            tracing::info!(target: "regia_render", "dropped {n} surface(s); device kept");
            // In the clean exit the later steps stop the warm, drop the device and save the
            // cache (`crate::exit`); a bare `WillTerminate` (a custom runner) does it here.
            if event == Some(LifecycleEvent::WillTerminate) {
                crate::gpu_health::note_exiting();
                if !world.contains_resource::<regia_platform::CleanExitInProgress>() {
                    release_gpu_for_exit(world);
                }
            }
        }
        Some(LifecycleEvent::Resumed) => {
            crate::boot::mark(crate::boot::BootMark::Resumed);
            // Spec 54 § 9: completion callbacks of work in flight at the suspension may never
            // arrive; the first completion after the resume is a barrier, so a later stall is
            // not blamed on the passes of that lost work. Harmless on the first resume (nothing
            // submitted yet).
            crate::breadcrumbs::global().note_resumed();
            create_all_surfaces(world);
            if !world.resource::<WindowSurfaces>().is_empty() {
                crate::boot::mark(crate::boot::BootMark::Surface);
            }
            world.resource_mut::<WindowSurfaces>().announce_present = true;
            present_splash_now(world);
            start_pipeline_warm(world);
        }
        // Idle GPU memory (a dormant reflection pass's targets) goes at the next frame.
        Some(LifecycleEvent::LowMemory) => {
            crate::gpu_health::signal_pressure(crate::gpu_health::PressureSource::OsLowMemory);
        }
        _ => {}
    }
    if event.is_some() {
        let observed = surface_observation(world);
        regia_platform::record_surface_observation(world, observed);
    }
}

/// What the surface audit checks after a transition (`regia_platform::SurfaceAudit`).
fn surface_observation(world: &World) -> SurfaceObservation {
    let windows = world
        .get_resource::<WindowHandles>()
        .map_or(0, |h| h.iter().count());
    let surfaces = world.resource::<WindowSurfaces>();
    SurfaceObservation {
        live: surfaces.surfaces.len(),
        windows,
        created_total: surfaces.created_total,
        dropped_total: surfaces.dropped_total,
    }
}

/// A bare `WillTerminate` (a custom runner without the clean exit, `crate::exit`): stops the
/// pipeline warm workers (releasing their device), saves the
/// persistent pipeline cache (not after a device loss: `pipeline_cache::save_installed`), and
/// drops the warmed renderers, so the device is destroyed when the world drops instead of
/// living into the process's exit handlers (NVIDIA's Linux driver can deadlock there with a live
/// Vulkan device). The [`RenderDevice`] resource itself stays until
/// the world drops; its last clone ends the cache install.
fn release_gpu_for_exit(world: &mut World) {
    let _span = tracing::info_span!("release_gpu_for_exit").entered();
    if let Some(queue) = world.get_resource::<crate::PipelineWarmQueue>() {
        queue.shutdown();
    }
    if let Some(Err(err)) = crate::pipeline_cache::save_installed() {
        tracing::warn!(target: "regia_render", "pipeline cache save at exit failed: {err}");
    }
}

/// Presents the boot splash on every surface right away, from the lifecycle event that created
/// them: the first image does not wait for the first game frame (`boot`).
fn present_splash_now(world: &mut World) {
    let Some(device) = world.get_resource::<RenderDevice>().cloned() else {
        return;
    };
    let Some(mut splash) = world.remove_resource::<crate::BootSplash>() else {
        return;
    };
    // Its pipeline is the first boot-tier job (ADR-5131): this present shows the background.
    splash.attach_warm_queue(world.get_resource::<crate::PipelineWarmQueue>());
    let phase = splash.advance(std::time::Instant::now());
    if phase != crate::boot::SplashPhase::Done {
        let _span = tracing::info_span!("boot_splash_present").entered();
        let windows: Vec<(Entity, std::sync::Arc<OsWindow>)> = world
            .resource::<WindowHandles>()
            .iter()
            .map(|(e, w)| (e, w.clone()))
            .collect();
        // Before the first focus event no window counts as focused, so the splash keeps the callback then.
        let focused_window = world
            .get_resource::<regia_platform::FocusedWindow>()
            .and_then(|f| f.0);
        let mut surfaces = world.resource_mut::<WindowSurfaces>();
        for (entity, s) in &mut surfaces.surfaces {
            if s.config.width == 0 || s.config.height == 0 {
                continue;
            }
            if s.needs_configure {
                if configure_surface(&device.device, &s.surface, &s.config) == ConfigureOutcome::Retry {
                    continue;
                }
                s.needs_configure = false;
            }
            let frame_tex = match s.surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(frame)
                | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
                _ => continue,
            };
            let view = frame_tex
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default());
            let mut encoder = device
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("boot splash"),
                });
            splash.record(
                &device,
                &mut encoder,
                &view,
                s.config.format,
                (s.config.width, s.config.height),
                phase,
            );
            device.queue.submit([encoder.finish()]);
            // The same rule as the frame's present (ADR-8170).
            if let Some((_, os)) = windows.iter().find(|(e, _)| e == entity)
                && crate::swapchain::wants_frame_callback(
                    focused_window == Some(*entity),
                    s.info.effective_present_mode(),
                )
            {
                os.pre_present_notify();
            }
            device.queue.present(frame_tex);
            crate::boot::mark(crate::boot::BootMark::FirstPresent);
        }
    }
    world.insert_resource(splash);
}

/// Starts the pipeline warm queue once the device exists (after the first present), with the
/// engine's boot-tier job: the canonical lit frame in the window's format.
fn start_pipeline_warm(world: &mut World) {
    let Some(device) = world.get_resource::<RenderDevice>().cloned() else {
        return;
    };
    let Some(queue) = world.get_resource::<crate::PipelineWarmQueue>().cloned() else {
        return;
    };
    if queue.is_started() {
        return;
    }
    if let Some(sets) = world.get_resource::<regia_asset::LoadSets>() {
        queue.attach_load_sets(sets.clone());
    }
    // The primary window's format (with two windows, not whichever entity sorts first).
    let format = world
        .resource::<WindowSurfaces>()
        .primary_format()
        .unwrap_or(wgpu::TextureFormat::Bgra8UnormSrgb);
    // The overlay layers (runtime UI, quick draw, 2D) compile their pipelines first in the boot
    // tier: for the window format and both display-stage intermediates, whichever the frame draws
    // into. A HUD or the performance overlay appearing later then creates nothing on a game frame.
    // Layers installed after this (a plugin's `Startup` runs after the first resume) are queued
    // by `warm_late_overlay_layers`.
    let layers = world
        .get_resource::<crate::WindowOverlay>()
        .filter(|overlay| queue.engine_jobs() && overlay.is_set())
        .map(crate::WindowOverlay::shared_layers);
    if let Some(layers) = layers {
        world
            .get_resource_or_insert_with(OverlayLayersWarmed::default)
            .record(&layers);
        push_overlay_warm(&queue, layers, format);
    }
    // After the overlay layers: the lobby's UI pipelines are built first, well before the
    // splash's cap (ADR-173), even with one warm worker.
    if queue.engine_jobs() {
        queue.push(
            crate::WarmTier::Boot,
            "canonical frame",
            crate::pipeline_warm::frame_job_handoff(
                crate::pipeline_warm::canonical_frame(),
                format,
                queue.clone(),
            ),
        );
    }
    queue.start(&device);
}

/// The overlay layers whose pipelines a boot-tier warm job was queued for: by the queue's start
/// and by `warm_late_overlay_layers`. A resource, not the system's `Local`, so the record holds
/// however the system is run.
///
/// Keyed by the layer itself, not its name: `WindowOverlay::set` (and `insert` with a name in use)
/// replaces a layer under the same name, and the replacement still needs its job.
#[derive(Resource, Default)]
pub(crate) struct OverlayLayersWarmed {
    /// Weak handles: the record keeps no removed layer alive, and a held allocation is never
    /// reused, so a new layer cannot be mistaken for a freed one at the same address.
    layers: Vec<std::sync::Weak<std::sync::Mutex<Box<dyn crate::overlay::OverlayDraw>>>>,
}

impl OverlayLayersWarmed {
    /// Whether a warm job was queued for `layer`.
    fn contains(&self, layer: &crate::overlay::SharedOverlay) -> bool {
        self.layers
            .iter()
            .any(|warmed| std::ptr::eq(warmed.as_ptr(), std::sync::Arc::as_ptr(layer)))
    }

    /// Records that a warm job was queued for `layers` (and forgets layers that are gone).
    fn record(&mut self, layers: &[crate::overlay::SharedOverlay]) {
        self.layers.retain(|warmed| warmed.strong_count() > 0);
        self.layers.extend(layers.iter().map(std::sync::Arc::downgrade));
    }
}

/// Queues the boot-tier job that builds `layers`' pipelines for the window `format` and both
/// display-stage intermediates.
fn push_overlay_warm(
    queue: &crate::PipelineWarmQueue,
    layers: Vec<crate::overlay::SharedOverlay>,
    format: wgpu::TextureFormat,
) {
    queue.push(
        crate::WarmTier::Boot,
        "overlay layers",
        move |device: &RenderDevice| {
            let formats = [
                format,
                crate::display_stage::SDR_INTERMEDIATE,
                crate::display_stage::HDR_INTERMEDIATE,
            ];
            for layer in &layers {
                let mut draw = layer.lock().map_err(|_| "overlay lock poisoned".to_owned())?;
                for f in formats {
                    // Quitting: stop between the layers' pipeline builds.
                    if crate::pipeline_warm::warm_cancelled() {
                        return Err(crate::pipeline_warm::CANCELLED_AT_SHUTDOWN.to_owned());
                    }
                    draw.warm(device, f);
                }
            }
            Ok(())
        },
    );
}

/// `PreUpdate`: an overlay layer installed after the warm queue started (the runtime UI sets its
/// layer in `Startup`, which runs after the first resume started the queue) gets its own
/// boot-tier warm job, so its first draw compiles nothing on the frame thread (ADR-5131).
///
/// The layers change only through `ResMut<WindowOverlay>`, so once it has looked at them with the
/// queue started it looks again only when the overlay changed: a steady overlay costs no
/// allocation per frame.
pub(crate) fn warm_late_overlay_layers(
    queue: Option<Res<crate::PipelineWarmQueue>>,
    overlay: Option<Res<crate::WindowOverlay>>,
    surfaces: Option<Res<WindowSurfaces>>,
    warmed: Option<ResMut<OverlayLayersWarmed>>,
    mut commands: Commands,
    mut looked: Local<bool>,
) {
    let (Some(queue), Some(overlay)) = (queue, overlay) else {
        *looked = false;
        return;
    };
    if !queue.is_started() || !queue.engine_jobs() || !overlay.is_set() {
        *looked = false;
        return;
    }
    if *looked && !overlay.is_changed() {
        return;
    }
    *looked = true;
    let done = |layer: &crate::overlay::SharedOverlay| warmed.as_ref().is_some_and(|w| w.contains(layer));
    if overlay.layers().all(|(_, layer)| done(layer)) {
        return;
    }
    let (names, layers): (Vec<&'static str>, Vec<crate::overlay::SharedOverlay>) = overlay
        .layers()
        .filter(|(_, layer)| !done(layer))
        .map(|(name, layer)| (name, std::sync::Arc::clone(layer)))
        .unzip();
    let format = surfaces
        .as_ref()
        .and_then(|s| s.primary_format())
        .unwrap_or(wgpu::TextureFormat::Bgra8UnormSrgb);
    tracing::debug!(target: "regia_render", "overlay layers {names:?} installed after the warm queue started: queued");
    match warmed {
        Some(mut warmed) => warmed.record(&layers),
        None => {
            let mut record = OverlayLayersWarmed::default();
            record.record(&layers);
            commands.insert_resource(record);
        }
    }
    push_overlay_warm(&queue, layers, format);
}

/// `PreUpdate`: boot timeline marks for the warm queue's boot and menu tiers.
pub(crate) fn track_boot_tiers(queue: Option<Res<crate::PipelineWarmQueue>>, mut marked: Local<u8>) {
    let Some(queue) = queue else {
        return;
    };
    if *marked >= 2 || !queue.is_started() {
        return;
    }
    if *marked == 0 && queue.is_tier_ready(crate::WarmTier::Boot) {
        crate::boot::mark(crate::boot::BootMark::BootTierReady);
        *marked = 1;
    }
    if *marked == 1 && queue.is_tier_ready(crate::WarmTier::Menu) {
        crate::boot::mark(crate::boot::BootMark::MenuTierReady);
        *marked = 2;
    }
}

/// The inputs of [`sync_surface_output`]'s choice; capabilities are re-read only when one changes.
/// The last one is the display's HDR mode (`DisplayHdrSummary::hdr_mode`).
type SyncKey = (
    PresentPolicy,
    HdrOutput,
    regia_platform::PresentMode,
    Option<bool>,
);

/// Frames between two capability queries while a window's plan finds no usable format.
const REPLAN_RETRY_FRAMES: u64 = 30;

/// What [`sync_surface_output`] last planned for a window: its inputs, and for a plan that found
/// no usable format the frame from which it asks the driver again.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Planned<K> {
    key: K,
    retry_at: Option<u64>,
}

/// Whether [`sync_surface_output`] asks the driver for the surface's capabilities on `frame`: an
/// input changed since the last plan, or that plan failed and its retry frame has come.
fn wants_replan<K: PartialEq>(last: Option<&Planned<K>>, key: &K, frame: u64) -> bool {
    match last {
        Some(planned) if planned.key == *key => planned.retry_at.is_some_and(|at| frame >= at),
        _ => true,
    }
}

/// The record of a plan of `key` on `frame` (`ok`: it found a format). A plan that found none
/// (the surface listed no format) is not latched as applied: the driver is asked again after
/// [`REPLAN_RETRY_FRAMES`] (audit G render5 #11), not every frame, since the query is a driver
/// round trip that blocks on some drivers.
fn after_plan<K>(key: K, ok: bool, frame: u64) -> Planned<K> {
    Planned {
        key,
        retry_at: (!ok).then(|| frame.saturating_add(REPLAN_RETRY_FRAMES)),
    }
}

/// `PreUpdate`: push [`OutputSettings`] onto live swapchains.
///
/// The default (vsync, SDR) reproduces the pre-M15a mapping, so an untouched
/// window is not reconfigured.
pub(crate) fn sync_surface_output(
    device: Option<Res<RenderDevice>>,
    output: Option<Res<OutputSettings>>,
    mut surfaces: ResMut<WindowSurfaces>,
    windows: Query<(Entity, &Window)>,
    mut applied: Local<HashMap<Entity, Planned<SyncKey>>>,
    mut frame: Local<u64>,
) {
    let _span = tracing::debug_span!("sync_surface_output").entered();
    let (Some(device), Some(output)) = (device, output) else {
        return;
    };
    applied.retain(|e, _| surfaces.surfaces.contains_key(e));
    *frame += 1;
    // Only an HDR request depends on the display's mode, so only then is it read again.
    let poll_display = output.hdr != HdrOutput::Sdr && frame.is_multiple_of(DISPLAY_POLL_FRAMES);
    for (entity, window) in &windows {
        let Some(surface) = surfaces.surfaces.get_mut(&entity) else {
            continue;
        };
        if poll_display {
            surface.info.display = display_report(&surface.surface, &device.adapter);
        }
        let display = surface.info.display;
        // `get_capabilities` is a driver round trip (and blocks for good on the Android
        // emulator's gfxstream after the first frame), so ask only when an input changed.
        let key = (output.present, output.hdr, window.present_mode, display.hdr_mode);
        if !wants_replan(applied.get(&entity), &key, *frame) {
            continue;
        }
        let caps = surface.surface.get_capabilities(&device.adapter);
        let size = (surface.config.width, surface.config.height);
        let backend = surface.info.request.backend;
        let planned = plan_surface(&caps, backend, &output, window, display, size);
        applied.insert(entity, after_plan(key, planned.is_some(), *frame));
        let Some(info) = planned else {
            continue;
        };
        apply_plan(surface.plan_fields(), info);
    }
}

/// Resizes written since `cursor` last read. The runner writes them between frames, before
/// `First` swaps the message buffers, so by `PreUpdate` they are no longer the "current update"
/// messages: reading those missed every resize, and after a rotation the phone kept presenting
/// a portrait swapchain stretched across the landscape screen.
fn new_resizes(world: &World, cursor: &mut MessageCursor<WindowResized>) -> Vec<WindowResized> {
    world
        .get_resource::<Messages<WindowResized>>()
        .map(|messages| cursor.read(messages).copied().collect())
        .unwrap_or_default()
}

/// `PreUpdate`: follow window size changes; recreate surfaces reported lost.
pub(crate) fn track_resizes(world: &mut World, mut cursor: Local<MessageCursor<WindowResized>>) {
    let _span = tracing::debug_span!("track_resizes").entered();
    let resizes = new_resizes(world, &mut cursor);
    let mut surfaces = world.resource_mut::<WindowSurfaces>();
    for r in resizes {
        if let Some(s) = surfaces.surfaces.get_mut(&r.window)
            && (s.config.width, s.config.height) != (r.width, r.height)
        {
            s.config.width = r.width;
            s.config.height = r.height;
            s.needs_configure = true;
        }
    }
    let lost = std::mem::take(&mut surfaces.lost);
    for entity in &lost {
        if surfaces.surfaces.remove(entity).is_some() {
            surfaces.dropped_total += 1;
        }
    }
    if !lost.is_empty() {
        // The window captures' pooled buffers go with the surfaces (ADR-8848).
        crate::screenshot::release_screenshot_buffers();
        tracing::warn!(target: "regia_render", "recreating {} lost surface(s)", lost.len());
        create_all_surfaces(world);
        let mut surfaces = world.resource_mut::<WindowSurfaces>();
        if lost.iter().any(|e| !surfaces.surfaces.contains_key(e)) {
            note_lost_frame(&mut surfaces);
        }
    }
}

/// Hands the renderer the frame state `extracted` holds beside the meshes and the per-instance
/// side tables: what the window frame ([`render_windows`]) and the hidden-window screenshot
/// ([`render_views_offscreen`]) both draw with. Every value is written every frame, including the
/// "off" ones, so a state the host removed is dropped.
pub(crate) fn sync_window_frame(renderer: &mut FrameRenderer, extracted: &ExtractedFrame) {
    renderer.set_outdoor(extracted.outdoor.clone());
    renderer.set_outdoor_portals(Some(&extracted.outdoor_portals));
    renderer.set_valley_mist(extracted.valley_mist.clone());
    renderer.set_glass(extracted.glass.clone());
    renderer.set_mask_decals(&extracted.mask_decals);
    renderer.set_mesh_occluders(extracted.mesh_occluders.clone());
    renderer.set_temporal_upscaler(extracted.temporal_upscaler);
    renderer.set_gizmos(Some(extracted.gizmos.clone()));
    renderer.set_morph_instances(&extracted.morphs);
    renderer.set_gi_proxy_disabled(extracted.gi_proxy_disabled_keys());
    renderer.set_dynamic_gi_tier(extracted.dynamic_gi_tier);
    renderer.set_radiance_cache(extracted.radiance_cache);
    renderer.set_two_phase_occlusion(extracted.two_phase_occlusion);
    renderer.set_shadow_occlusion(extracted.shadow_occlusion);
    renderer.set_masked_occlusion(extracted.masked_occlusion);
    renderer.set_lights_views(&extracted.lights_views);
    // The daylit interior bake reads the enclosure shell (`None` drops a removed one).
    renderer.sync_interior_scene(extracted.enclosure.as_ref());
    renderer.release_meshes(&extracted.released_mesh_keys);
    renderer.hold_mesh_identities(extracted.emissive_meshes());
}

/// Renders every extracted window view into one offscreen LDR image and reads
/// it back (screenshots of hidden windows).
///
/// Views composite exactly like the visible path: viewport rectangles, the
/// first view clearing what it covers, later views loading.
#[tracing::instrument(skip_all)]
pub(crate) fn render_views_offscreen(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    extracted: &ExtractedFrame,
    format: wgpu::TextureFormat,
    (width, height): (u32, u32),
    overlay: Option<&crate::WindowOverlay>,
) -> Result<crate::CapturedImage, String> {
    let srgb = matches!(format, wgpu::TextureFormat::Rgba8UnormSrgb);
    let target = renderer.offscreen_target(device, width, height, srgb);
    // Mirror the visible path: clear once when the first view does not cover
    // the whole target, so no region is left stale.
    sync_window_frame(renderer, extracted);
    let views = extracted.window_views();
    // Views that went away take their occlusion history with them (ADR-7329).
    renderer.retain_occlusion_views(extracted.views.len());
    let first_full = views
        .first()
        .is_some_and(|(_, view)| view.viewport == [0.0, 0.0, 1.0, 1.0] && view.clear);
    let pre_cleared = !first_full;
    if pre_cleared {
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("hidden screenshot clear"),
            });
        encode_clear(&mut encoder, &target.view, [0.02, 0.02, 0.025, 1.0]);
        device.queue.submit([encoder.finish()]);
    }
    renderer.set_light_extras(extracted.light_extras.clone());
    for (index, (view_index, view)) in views.iter().enumerate() {
        let rect = pixel_viewport(view.viewport, width, height);
        let scene = extracted.frame_scene(*view_index, rect[2], rect[3]);
        let load = pre_cleared || index > 0;
        let frame_target = FrameTarget::rect(&target.view, format, width, height, rect, load)
            .with_color(Some(&target.texture));
        if index == 0 {
            renderer
                .bake_pending_local_captures(device, &scene)
                .map_err(|e| e.to_string())?;
        }
        // Each view's occlusion readback has its own ring (ADR-7286), and its own outdoor list
        // (split-screen inside a cell, ADR-8843).
        renderer.set_occlusion_view(*view_index);
        renderer.set_outdoor_portals(extracted.outdoor_portals_for(*view_index));
        renderer
            .render_into(device, &scene, frame_target)
            .map_err(|e| e.to_string())?;
    }
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("hidden screenshot overlay+copy"),
        });
    if let Some(overlay) = overlay {
        crate::overlay::record_overlay_on(
            overlay,
            device,
            &mut encoder,
            Some(&target.texture),
            &target.view,
            format,
            width,
            height,
        );
    }
    let readback = crate::screenshot::encode_copy(device, &mut encoder, &target.texture, format);
    device.queue.submit([encoder.finish()]);
    crate::screenshot::finish(device, readback)
        .map(|image| crate::CapturedImage {
            width: image.width,
            height: image.height,
            rgba: image.rgba,
        })
        .map_err(|e: RenderError| e.to_string())
}

/// [`render_windows`]'s view of window visibility: hidden by the runner, the focused window, and the
/// count of ticks the OS refused every surface as occluded (ADR-7335).
type VisibilityParams<'w> = (
    Option<Res<'w, regia_platform::SurfacesHidden>>,
    Option<Res<'w, regia_platform::FocusedWindow>>,
    Option<ResMut<'w, regia_platform::OccludedAcquires>>,
);

/// [`render_windows`]'s display inputs: the calibration, the HDR test pattern and the paused-scene
/// hold (ADR-7890).
type DisplayParams<'w> = (
    Option<Res<'w, crate::DisplayCalibration>>,
    Option<Res<'w, crate::HdrTestPattern>>,
    Option<Res<'w, crate::scene_hold::SceneHold>>,
);

/// [`render_windows`]'s boot inputs: the splash, the warm queue and the game-frame warm.
type BootParams<'w> = (
    Option<ResMut<'w, crate::BootSplash>>,
    Option<Res<'w, crate::PipelineWarmQueue>>,
    Option<ResMut<'w, crate::GameFrameWarm>>,
    Option<ResMut<'w, crate::QualityProbe>>,
);

/// [`render_windows`]'s frame generation inputs (ADR-2067).
#[cfg(feature = "frame-gen")]
type FrameGenParams<'w> = (
    Option<Res<'w, crate::frame_gen::FrameGenSettings>>,
    Option<ResMut<'w, crate::frame_gen::FrameGenPacing>>,
    Option<ResMut<'w, crate::frame_gen::present::FrameGenPresenter>>,
);

/// What serving a window's screenshots without its swapchain needs: a hidden window, or one whose
/// surface the OS refused as occluded this tick (ADR-7335).
struct HiddenCapture<'a> {
    device: &'a RenderDevice,
    clear: [f64; 4],
    frame: Option<&'a ExtractedFrame>,
    overlay: Option<&'a crate::WindowOverlay>,
    calibration: &'a crate::DisplayCalibration,
}

impl HiddenCapture<'_> {
    /// Answers `entity`'s pending screenshots from an offscreen render at the surface's size.
    fn serve(
        &self,
        entity: Entity,
        s: &WindowSurface,
        screenshots: &mut crate::ScreenshotRequests,
        runtime: &mut FrameRenderer,
    ) {
        let callbacks = screenshots.take_with_options(entity);
        if callbacks.is_empty() {
            return;
        }
        let format = if s.config.format.is_srgb() {
            wgpu::TextureFormat::Rgba8UnormSrgb
        } else {
            wgpu::TextureFormat::Rgba8Unorm
        };
        let (w, h) = (s.config.width.max(1), s.config.height.max(1));
        // Hidden windows still serve agent screenshots of what they would show: the
        // composited ECS views (full pipeline, rendered offscreen at the surface size)
        // plus the UI overlay — or clear + overlay.
        let result = if let Some(frame) = self.frame {
            render_views_offscreen(self.device, runtime, frame, format, (w, h), self.overlay)
        } else {
            crate::offscreen::clear_to_pixels_format(self.device, self.clear, w, h, format, self.overlay)
                .map(|rgba| crate::CapturedImage {
                    width: w,
                    height: h,
                    rgba,
                })
                .map_err(|e| e.to_string())
        };
        // No swapchain, so no display stage: the calibrated capture applies the SDR
        // curve on the CPU (the same function as `display.wgsl`).
        for (options, callback) in callbacks {
            let mut image = result.clone();
            if !options.neutral
                && let Ok(image) = image.as_mut()
            {
                self.calibration.apply_sdr_rgba8(&mut image.rgba);
            }
            callback(image);
        }
    }
}

/// `Render`: render every window and present.
///
/// The full pipeline extracted from the ECS ([`crate::runtime`]) draws into each
/// window: one render per camera view, inside that camera's viewport rectangle
/// (split-screen), in camera `order`. Without a camera the window is cleared to
/// [`ClearColor`].
#[allow(clippy::too_many_arguments)] // clear + surfaces + screenshots + overlay + extracted
pub(crate) fn render_windows(
    device: Option<Res<RenderDevice>>,
    mut surfaces: ResMut<WindowSurfaces>,
    clear: Res<ClearColor>,
    handles: Res<WindowHandles>,
    mut screenshots: ResMut<crate::ScreenshotRequests>,
    (hidden, focus, mut occluded_acquires): VisibilityParams<'_>,
    overlay: Option<Res<crate::WindowOverlay>>,
    extracted: Option<Res<ExtractedFrame>>,
    mut runtime: ResMut<RuntimeRenderer>,
    (calibration, pattern, scene_hold): DisplayParams<'_>,
    (mut splash, warm, mut game_warm, mut probe): BootParams<'_>,
    mut lod_viewport: Option<ResMut<crate::mesh_lod::LodViewport>>,
    overrides: Option<Res<crate::ExtractedRenderOverrides>>,
    scalar_overlays: Option<Res<crate::ExtractedScalarOverlays>>,
    (shading, skinning): (
        Option<Res<crate::ExtractedMaterialShading>>,
        Option<Res<crate::ExtractedSkinning>>,
    ),
    #[cfg(feature = "frame-gen")] mut frame_gen: FrameGenParams<'_>,
) {
    let _mem = regia_core::memory::enter_owner(&crate::heap_owners::FRAME);
    let Some(device) = device else { return };
    let tick_start = std::time::Instant::now();
    let calibration = calibration.map(|c| c.clamped()).unwrap_or_default();
    // Game-frame warm (`GameFrameWarm`): take a warmed runtime renderer back, turn a finishing
    // splash into a request, start a requested warm on this tick's frame. While it runs the
    // splash holds and no scene is drawn (the runtime renderer is on a worker).
    let mut warming = false;
    if let Some(gw) = game_warm.as_deref_mut() {
        gw.begin_tick();
        gw.poll(&mut runtime.0);
        if let Some(queue) = warm.as_deref() {
            gw.watchdog(queue, &mut runtime.0);
        }
        gw.note_splash(
            splash
                .as_deref()
                .is_some_and(|s| s.is_finishing() && !s.is_done()),
            splash.as_deref().is_none_or(crate::BootSplash::is_done),
        );
        // A large new scene on a window that showed none (a map spawned behind a UI-only
        // menu): warm it on a worker while the window keeps presenting the UI (ADR-190).
        gw.note_scene(
            warm.as_deref(),
            &runtime.0,
            extracted.as_deref(),
            splash.as_deref().is_none_or(crate::BootSplash::is_done),
        );
        if let (Some(queue), Some(frame)) = (warm.as_deref(), extracted.as_deref())
            && let Some(s) = surfaces
                .surfaces
                .values()
                .find(|s| s.config.width > 0 && s.config.height > 0)
        {
            let output =
                crate::display_stage::StageOutput::for_surface(s.config.format, s.config.color_space);
            let (format, readable) = if output.is_hdr() {
                (crate::display_stage::HDR_INTERMEDIATE, true)
            } else if !calibration.is_sdr_neutral() {
                (crate::display_stage::SDR_INTERMEDIATE, true)
            } else {
                (
                    s.config.format,
                    s.config.usage.contains(wgpu::TextureUsages::COPY_SRC),
                )
            };
            gw.start(
                queue,
                &mut runtime.0,
                frame,
                (format, readable),
                device.device_id(),
                Some((s.config.width, s.config.height)),
            );
        }
        warming = gw.is_warming();
    }
    // A running warm, or the host's open scene prepare (ADR-7883), holds the scene back: the
    // window keeps presenting the UI.
    let holding = warming
        || game_warm
            .as_deref()
            .is_some_and(crate::GameFrameWarm::holds_scene);
    // The quality probe (ADR-173) starts once the splash is gone and no game-frame warm is
    // pending: a probe renderer on a warm worker draws this frame offscreen at the window's
    // size. It never holds the splash and never touches the runtime renderer.
    if let Some(p) = probe.as_deref_mut() {
        p.poll(std::time::Instant::now());
        if p.wants_start()
            && crate::quality::probe_start_allowed(
                splash.as_deref().is_none_or(crate::BootSplash::is_done),
                game_warm.as_deref().is_some_and(crate::GameFrameWarm::is_pending),
                holding,
            )
            && let (Some(queue), Some(frame)) = (warm.as_deref(), extracted.as_deref())
            && frame.is_renderable()
            && let Some(s) = surfaces
                .surfaces
                .values()
                .find(|s| s.config.width > 0 && s.config.height > 0)
        {
            let output =
                crate::display_stage::StageOutput::for_surface(s.config.format, s.config.color_space);
            let format = if output.is_hdr() {
                crate::display_stage::HDR_INTERMEDIATE
            } else if !calibration.is_sdr_neutral() {
                crate::display_stage::SDR_INTERMEDIATE
            } else {
                s.config.format
            };
            p.start(
                queue,
                &runtime.0,
                frame,
                format,
                (s.config.width, s.config.height),
            );
        }
    }
    // A running game-frame warm holds the fade-out, up to the splash's cap (ADR-173).
    if game_warm.is_some()
        && let Some(s) = splash.as_deref_mut()
    {
        s.set_held(warming);
    }
    // The boot splash covers the window until it fades: no scene or overlay is drawn under it,
    // so the main renderer builds nothing while the warm queue compiles (`boot`).
    if let Some(s) = splash.as_deref_mut() {
        s.attach_warm_queue(warm.as_deref());
    }
    let splash_phase = match splash.as_deref_mut() {
        Some(s) if !s.is_done() => s.advance(std::time::Instant::now()),
        _ => crate::boot::SplashPhase::Done,
    };
    let splash_covers = matches!(splash_phase, crate::boot::SplashPhase::Showing { .. });
    // The full-pipeline frame extracted from the ECS this render tick.
    let frame = extracted
        .as_ref()
        .filter(|e| e.is_renderable() && !splash_covers && !holding);
    // The first scene frame after a warm (or of a prepare) checks what it still uploads: the
    // frame's meshes the runtime renderer does not hold yet (ADR-7883).
    let pending_meshes = frame
        .filter(|_| {
            game_warm
                .as_deref()
                .is_some_and(crate::GameFrameWarm::wants_frame_check)
        })
        .map_or(0, |f| runtime.0.pending_mesh_uploads(f, (u64::MAX, usize::MAX)).1);
    // Warmed renderers (`frame_job_handoff`) hand their pipelines over before the frames that
    // would build them.
    if frame.is_some()
        && let Some(warm) = warm.as_ref()
    {
        // Pipelines built on another device (the host replaced the device) cannot be used here.
        let same_device = warm.device_id().is_none_or(|id| id == device.device_id());
        while let Some(mut warmed) = warm.take_warmed_renderer() {
            if !same_device {
                tracing::warn!(target: "regia_render", "warmed renderer from another device dropped");
                continue;
            }
            if runtime.0.adopt_warm_pipelines(&mut warmed) {
                tracing::info!(target: "regia_render", "runtime renderer adopted warmed pipelines");
            }
        }
    }
    if let Some(extracted_frame) = frame {
        sync_window_frame(&mut runtime.0, extracted_frame);
        runtime
            .0
            .set_scalar_overlays(scalar_overlays.iter().flat_map(|o| o.entries.iter().cloned()));
        runtime
            .0
            .set_render_overrides(overrides.iter().flat_map(|o| o.keys.iter().copied()));
        runtime
            .0
            .set_material_shading(shading.iter().flat_map(|s| s.entries()));
        if let Some(skinning) = skinning.as_deref() {
            runtime.0.set_compute_skinning(skinning.compute_all);
            runtime.0.set_skinning_options(skinning.keys.iter().cloned());
        }
    }

    if hidden.is_some_and(|h| h.0) {
        // Nothing is presented: a resident prepare whose scene could draw is ready (ADR-7883).
        if let Some(gw) = game_warm.as_deref_mut() {
            gw.note_unpresented_scene(frame.is_some());
        }
        // Occluded / silent workspace: presenting can block on vsync. Serve screenshots from an
        // offscreen target and still run the UI overlay so agent captures include runtime UI.
        if !screenshots.is_empty() {
            for (entity, s) in &surfaces.surfaces {
                let offscreen = HiddenCapture {
                    device: &device,
                    clear: clear.0,
                    frame: frame.map(|f| &**f),
                    overlay: overlay.as_deref(),
                    calibration: &calibration,
                };
                offscreen.serve(*entity, s, &mut screenshots, &mut runtime.0);
            }
        }
        return;
    }
    let surfaces = &mut *surfaces;
    let primary = surfaces.primary_entity();
    // The paused-scene hold (ADR-7890): off unless the host turned it on.
    let scene_hold = scene_hold.as_deref().copied().unwrap_or_default();
    let mut hold_report = crate::scene_hold::SceneHoldFrame::default();
    // Frame generation does not run while the scene hold is on.
    #[cfg(feature = "frame-gen")]
    if scene_hold.is_on() {
        frame_gen.0 = None;
    }
    let (mut lost_this_frame, mut acquired_any) = (false, false);
    // Surfaces that asked for a texture this tick, and the ones the OS refused as occluded (ADR-7335).
    let mut attempted = 0u32;
    let mut refused: Vec<Entity> = Vec::new();
    #[cfg(feature = "frame-gen")]
    let window_count = surfaces.surfaces.len();
    // Pipelines this thread created so far (a drawn scene frame's own count, ADR-7883).
    #[cfg(feature = "frame-gen")]
    let tick_counters = crate::frame_stats::counters();
    // Main-thread split of this frame for telemetry (`frame_stats::window_phases`).
    let mut phases = crate::frame_stats::WindowPhases::default();
    // Blocking screenshot readbacks of this frame (`frame_stats::frame_waits`).
    let mut readback_ms = 0.0f32;

    for (entity, s) in &mut surfaces.surfaces {
        if s.config.width == 0 || s.config.height == 0 {
            continue;
        }
        // Last frame's primary window size: extraction already ran, so LOD selection sees this
        // next frame. One window defines it — with several windows the last map entry used to
        // win, which made the answer the `HashMap` seed (audit G, render5 determinism).
        if Some(*entity) == primary
            && let Some(vp) = lod_viewport.as_deref_mut()
        {
            vp.width = s.config.width.max(1);
            vp.height = s.config.height.max(1);
        }
        // Frame generation owns the window's frame while it runs (ADR-2067; default off).
        #[cfg(feature = "frame-gen")]
        if let Some(done) = frame_gen_present::take_window(
            &mut frame_gen,
            frame_gen_present::FrameGenWindow {
                device: &device,
                entity: *entity,
                surface: &mut *s,
                windows: window_count,
                frame: frame.map(|f| &**f),
                splash_done: splash_phase == crate::boot::SplashPhase::Done,
                overlay: overlay.as_deref(),
                calibration: &calibration,
                pattern: pattern.as_deref(),
                runtime: &mut runtime,
                screenshots: &mut screenshots,
                handles: &handles,
                focused: focus.as_ref().is_some_and(|f| f.0 == Some(*entity)),
            },
        ) {
            acquired_any |= done.acquired;
            lost_this_frame |= done.note(*entity, &mut surfaces.lost);
            done.note_warm(
                warm.as_deref(),
                game_warm.as_deref_mut(),
                tick_start,
                (
                    pending_meshes,
                    crate::frame_stats::counters().since(tick_counters).pipelines,
                ),
            );
            continue;
        }
        if s.needs_configure {
            tracing::debug!(target: "regia_render", "configure {}x{}", s.config.width, s.config.height);
            if configure_surface(&device.device, &s.surface, &s.config) == ConfigureOutcome::Retry {
                continue;
            }
            s.needs_configure = false;
        }
        tracing::trace!(target: "regia_render", "acquire");
        attempted += 1;
        let acquire_started = std::time::Instant::now();
        let acquired = {
            let _acquire = tracing::info_span!("acquire").entered();
            s.surface.get_current_texture()
        };
        phases.acquire_ms += acquire_started.elapsed().as_secs_f32() * 1000.0;
        tracing::trace!(target: "regia_render", "acquired");
        let frame_tex = match acquired {
            wgpu::CurrentSurfaceTexture::Success(frame) => {
                acquired_any = true;
                frame
            }
            wgpu::CurrentSurfaceTexture::Suboptimal(frame) => {
                acquired_any = true;
                s.needs_configure = true;
                frame
            }
            wgpu::CurrentSurfaceTexture::Outdated => {
                regia_core::telemetry::add(regia_core::telemetry::Counter::SurfaceOutdated, 1);
                s.needs_configure = true;
                continue;
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                regia_core::telemetry::add(regia_core::telemetry::Counter::SurfaceLost, 1);
                surfaces.lost.push(*entity);
                lost_this_frame = true;
                continue;
            }
            wgpu::CurrentSurfaceTexture::Timeout => {
                regia_core::telemetry::add(regia_core::telemetry::Counter::SurfaceTimeout, 1);
                continue;
            }
            wgpu::CurrentSurfaceTexture::Occluded => {
                regia_core::telemetry::add(regia_core::telemetry::Counter::SurfaceOther, 1);
                refused.push(*entity);
                continue;
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                regia_core::telemetry::add(regia_core::telemetry::Counter::SurfaceOther, 1);
                continue;
            }
        };
        let surface_view = frame_tex
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        // ---- Display stage: draw into an intermediate when calibration or HDR needs it ----
        let output = crate::display_stage::StageOutput::for_surface(s.config.format, s.config.color_space);
        let staged = output.is_hdr() || !calibration.is_sdr_neutral();
        let (view, draw_format, draw_texture) = if staged {
            let format = if output.is_hdr() {
                crate::display_stage::HDR_INTERMEDIATE
            } else {
                crate::display_stage::SDR_INTERMEDIATE
            };
            let stage = s
                .display
                .intermediate(&device.device, (s.config.width, s.config.height), format);
            (stage.view.clone(), format, Some(stage.texture.clone()))
        } else {
            s.display.release();
            let color = s
                .config
                .usage
                .contains(wgpu::TextureUsages::COPY_SRC)
                .then(|| frame_tex.texture.clone());
            (surface_view.clone(), s.config.format, color)
        };
        runtime
            .0
            .set_display_headroom(output.is_hdr().then(|| calibration.hdr_headroom()));

        // ---- Scene hold (ADR-7890): keep the scene image, or skip the scene and reuse it ----
        // An open scene prepare (ADR-7883) already dropped `frame`; both gates meet in one place.
        let hold_key = draw_texture.is_some().then_some(crate::scene_hold::HoldKey {
            width: s.config.width,
            height: s.config.height,
            format: draw_format,
        });
        let (hold_step, draw_scene) = crate::scene_hold::plan_window_scene(
            &mut s.hold,
            &scene_hold,
            frame.is_some_and(|f| !f.window_views().is_empty()) && !splash_covers,
            holding,
            hold_key,
        );

        // ---- Draw: every extracted window view, in camera order ----
        let draw_started = std::time::Instant::now();
        let draw_counters = crate::frame_stats::counters();
        let mut draw_error: Option<String> = None;
        let mut drew = false;
        if let Some(frame) = frame.filter(|_| draw_scene) {
            let views = frame.window_views();
            if !views.is_empty() {
                drew = true;
                let (width, height) = (s.config.width, s.config.height);
                // When the first view does not cover the whole window (or does
                // not want to clear), clear once so no region is left stale.
                let first_full = views
                    .first()
                    .is_some_and(|(_, v)| v.viewport == [0.0, 0.0, 1.0, 1.0] && v.clear);
                let mut pre_encoder = device
                    .device
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                        label: Some("frame clear"),
                    });
                let mut pre_cleared = false;
                if !first_full {
                    encode_clear(&mut pre_encoder, &view, clear.0);
                    pre_cleared = true;
                }
                if pre_cleared {
                    device.queue.submit([pre_encoder.finish()]);
                }
                runtime.0.set_light_extras(frame.light_extras.clone());
                // Views that went away take their occlusion history with them (ADR-7329).
                runtime.0.retain_occlusion_views(frame.views.len());
                for (index, (view_index, window_view)) in views.iter().enumerate() {
                    let rect = pixel_viewport(window_view.viewport, width, height);
                    let scene = frame.frame_scene(*view_index, rect[2], rect[3]);
                    let load = pre_cleared || index > 0;
                    let frame_target = FrameTarget::rect(&view, draw_format, width, height, rect, load)
                        .with_color(draw_texture.as_ref());
                    tracing::trace!(target: "regia_render", "drawing ECS view {index} into {:?}", rect);
                    // The local captures bake once, from the primary window's first view:
                    // the pending set clears on the first call, so with several windows the
                    // baking window used to be whichever hashed first (audit G).
                    if index == 0
                        && Some(*entity) == primary
                        && let Err(err) = runtime.0.bake_pending_local_captures(&device, &scene)
                    {
                        tracing::error!(target: "regia_render", "local capture bake failed: {err}");
                    }
                    // Each view's occlusion readback has its own ring (ADR-7286), and its own
                    // outdoor list (split-screen inside a cell, ADR-8843).
                    runtime.0.set_occlusion_view(*view_index);
                    runtime
                        .0
                        .set_outdoor_portals(frame.outdoor_portals_for(*view_index));
                    if let Err(err) = runtime.0.render_into(&device, &scene, frame_target) {
                        tracing::error!(target: "regia_render", "ECS view {index} draw failed for window {entity}: {err}");
                        draw_error = Some(err.to_string());
                    }
                    s.hold.note_scene_views(1);
                }
            }
        }

        let draw_pipelines = crate::frame_stats::counters().since(draw_counters).pipelines;

        // ---- Overlay + present (shared by every draw mode) ----
        let tail_started = std::time::Instant::now();
        phases.render_into_ms += tail_started.saturating_duration_since(draw_started).as_secs_f32() * 1000.0;
        let mut present_encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame tail"),
            });
        if draw_error.is_some() {
            s.hold.cancel_capture();
        }
        let held = hold_step == crate::scene_hold::HoldStep::Held;
        if let Err(err) = s.hold.record(
            &device,
            &mut present_encoder,
            &view,
            draw_texture.as_ref(),
            draw_format,
        ) {
            tracing::error!(target: "regia_render", "scene hold failed for window {entity}: {err}");
            draw_error = Some(err.to_string());
        }
        if (!(drew || held) || draw_error.is_some()) && !splash_covers {
            // Clear-only path (or a failed draw): never leave stale content.
            // Clear + UI overlay share one encoder/submit so the surface stays a colour
            // target across both passes (required for correct screenshots on Vulkan).
            encode_clear(&mut present_encoder, &view, clear.0);
        }
        if let Some(o) = overlay.as_deref()
            && !splash_covers
        {
            crate::overlay::record_overlay_on(
                o,
                &device,
                &mut present_encoder,
                draw_texture.as_ref(),
                &view,
                draw_format,
                s.config.width,
                s.config.height,
            );
        }
        if staged
            && let Err(err) = s.display.record(
                &device.device,
                &device.queue,
                &mut present_encoder,
                &surface_view,
                s.config.format,
                output,
                &calibration,
                pattern.as_deref(),
                crate::display_stage::StageSlot::Present,
            )
        {
            tracing::error!(target: "regia_render", "display stage failed for window {entity}: {err}");
        }
        if let Some(splash) = splash.as_deref_mut()
            && splash_phase != crate::boot::SplashPhase::Done
        {
            // Showing: clears to the splash background; fading out: blends over the frame.
            splash.record(
                &device,
                &mut present_encoder,
                &surface_view,
                s.config.format,
                (s.config.width, s.config.height),
                splash_phase,
            );
        }
        s.hold.finish_ui(&mut present_encoder);
        device.queue.submit([present_encoder.finish()]);
        s.hold.after_submit(&device);
        // Telemetry reads the primary window's frame (any window's without a primary).
        if Some(*entity) == primary || primary.is_none() {
            hold_report = s.hold.last_frame();
        }
        // The frame tail (UI overlay, display stage) is a submission of the frame's breadcrumbs
        // too (ADR-2307); the frame's own submit counters stay as they were.
        crate::breadcrumbs::note_untracked_submit(&device.queue);

        // Screenshot readback (separate encoder, after the draw).
        let callbacks = if screenshots.is_empty() {
            Vec::new()
        } else {
            screenshots.take_with_options(*entity)
        };
        let (neutral, applied): (Vec<_>, Vec<_>) = callbacks.into_iter().partition(|(o, _)| o.neutral);
        let neutral: Vec<_> = neutral.into_iter().map(|(_, cb)| cb).collect();
        let applied: Vec<_> = applied.into_iter().map(|(_, cb)| cb).collect();
        let readbacks = encode_screenshot_copies(
            &device,
            s,
            &frame_tex.texture,
            (staged, output, &calibration),
            (!neutral.is_empty(), !applied.is_empty()),
        );

        let focused = focus.as_ref().is_some_and(|f| f.0 == Some(*entity));
        if let Some(os) = handles.get(*entity)
            && crate::swapchain::wants_frame_callback(focused, s.info.effective_present_mode())
        {
            os.pre_present_notify();
        }
        let present_started = std::time::Instant::now();
        phases.tail_ms += present_started
            .saturating_duration_since(tail_started)
            .as_secs_f32()
            * 1000.0;
        {
            let _present = tracing::info_span!("present").entered();
            device.queue.present(frame_tex);
        }
        phases.present_ms += present_started.elapsed().as_secs_f32() * 1000.0;
        crate::boot::mark(crate::boot::BootMark::FirstPresent);
        if drew && draw_error.is_none() {
            // Booted: the cores belong to the game now. With a `GameFrameWarm` this waits for
            // its first presented game frame; without one any drawn scene frame counts. The
            // present above returned: an open prepare's scene is presented (ADR-7883).
            let check = crate::scene_ready::SceneFrameCheck {
                pending_meshes,
                pipelines: draw_pipelines,
                present: crate::scene_ready::PresentNote::Presented,
            };
            let first = game_warm
                .as_deref_mut()
                .is_none_or(|gw| gw.note_frame_with(tick_start.elapsed().as_secs_f32() * 1000.0, check));
            if first && let Some(queue) = warm.as_deref() {
                queue.note_scene_frame();
            }
        }
        for (mut callbacks, readback) in [(neutral, readbacks.0), (applied, readbacks.1)] {
            if callbacks.is_empty() {
                continue;
            }
            let result = match readback {
                Some(readback) => {
                    let _readback = tracing::info_span!("screenshot_readback").entered();
                    let started = std::time::Instant::now();
                    let result = crate::screenshot::finish_with(&device, readback, Some(&mut s.readback))
                        .map_err(|e| e.to_string());
                    readback_ms += started.elapsed().as_secs_f32() * 1000.0;
                    result
                }
                None => Err("this surface cannot be read back (no COPY_SRC support)".to_string()),
            };
            // The last request takes the capture itself: no frame-sized copy for a lone one.
            let last = callbacks.pop();
            for callback in callbacks {
                callback(result.clone());
            }
            if let Some(callback) = last {
                callback(result);
            }
        }

        tracing::trace!(target: "regia_render", "presented");
        if surfaces.announce_present {
            surfaces.announce_present = false;
            tracing::info!(target: "regia_render", "regia render: first frame presented after resume");
        }
    }
    // The OS refused these surfaces as occluded (a locked console, a sleeping display): tell the
    // runner, which steps at the hidden rate from the third such tick, and answer their screenshots
    // offscreen like a hidden window's, or they wait forever (ADR-7335).
    if let Some(o) = occluded_acquires.as_deref_mut() {
        o.note_tick(
            attempted,
            u32::try_from(refused.len()).unwrap_or(u32::MAX),
            acquired_any,
        );
    }
    if !refused.is_empty() && !screenshots.is_empty() {
        let offscreen = HiddenCapture {
            device: &device,
            clear: clear.0,
            frame: frame.map(|f| &**f),
            overlay: overlay.as_deref(),
            calibration: &calibration,
        };
        for entity in &refused {
            if let Some(s) = surfaces.surfaces.get(entity) {
                offscreen.serve(*entity, s, &mut screenshots, &mut runtime.0);
            }
        }
    }
    crate::frame_stats::set_window_phases(phases, readback_ms);
    crate::scene_hold::set_last_window_frame(hold_report);
    // The display pacer keeps the swapchain's own blocking out of the frame cost (ADR-2320).
    regia_platform::frame_pacing::note_swapchain_wait(std::time::Duration::from_secs_f32(
        ((phases.acquire_ms + phases.present_ms) / 1000.0).max(0.0),
    ));
    if acquired_any {
        surfaces.lost_streak = 0;
    } else if lost_this_frame {
        note_lost_frame(surfaces);
    }
}

/// Records the screenshot copies a window frame needs: `(neutral, applied)`.
///
/// Without a display stage both are the swapchain image. With an SDR stage the neutral image
/// is the intermediate and the applied one the swapchain; an HDR frame is captured as an SDR
/// rendition of the intermediate (black-level curve only for the applied capture).
fn encode_screenshot_copies(
    device: &crate::RenderDevice,
    s: &mut WindowSurface,
    surface_texture: &wgpu::Texture,
    (staged, output, calibration): (
        bool,
        crate::display_stage::StageOutput,
        &crate::DisplayCalibration,
    ),
    (want_neutral, want_applied): (bool, bool),
) -> (
    Option<crate::screenshot::PendingReadback>,
    Option<crate::screenshot::PendingReadback>,
) {
    if !want_neutral && !want_applied {
        return (None, None);
    }
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("screenshot copy"),
        });
    let WindowSurface {
        copy_src,
        config,
        display,
        readback,
        ..
    } = s;
    let (copy_src, surface_format) = (*copy_src, config.format);
    let surface_copy = |encoder: &mut wgpu::CommandEncoder,
                        readback: &mut crate::screenshot::ReadbackBuffers| {
        copy_src.then(|| {
            crate::screenshot::encode_copy_with(
                device,
                encoder,
                surface_texture,
                surface_format,
                Some(readback),
            )
        })
    };
    let readbacks = if !staged {
        let neutral = if want_neutral {
            surface_copy(&mut encoder, readback)
        } else {
            None
        };
        let applied = if want_applied {
            surface_copy(&mut encoder, readback)
        } else {
            None
        };
        (neutral, applied)
    } else if !output.is_hdr() {
        let applied = if want_applied {
            surface_copy(&mut encoder, readback)
        } else {
            None
        };
        let neutral = if want_neutral {
            display.intermediate_texture().map(|(texture, format)| {
                crate::screenshot::encode_copy_with(device, &mut encoder, texture, format, Some(readback))
            })
        } else {
            None
        };
        (neutral, applied)
    } else {
        let mut hdr_capture =
            |cal: &crate::DisplayCalibration,
             encoder: &mut wgpu::CommandEncoder,
             readback: &mut crate::screenshot::ReadbackBuffers| {
                let sdr_only = crate::DisplayCalibration {
                    gamma: crate::calibration::NEUTRAL_GAMMA,
                    ..*cal
                };
                match display.record_capture(&device.device, &device.queue, encoder, &sdr_only) {
                    Ok(texture) => Some(crate::screenshot::encode_copy_with(
                        device,
                        encoder,
                        texture,
                        crate::display_stage::SDR_INTERMEDIATE,
                        Some(readback),
                    )),
                    Err(err) => {
                        tracing::warn!(target: "regia_render", "HDR screenshot conversion failed: {err}");
                        None
                    }
                }
            };
        // One capture texture: the neutral and applied renditions are separate submits.
        let neutral = if want_neutral {
            let rb = hdr_capture(&crate::DisplayCalibration::NEUTRAL, &mut encoder, readback);
            device.queue.submit([std::mem::replace(
                &mut encoder,
                device
                    .device
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                        label: Some("screenshot copy"),
                    }),
            )
            .finish()]);
            rb
        } else {
            None
        };
        let applied = if want_applied {
            hdr_capture(calibration, &mut encoder, readback)
        } else {
            None
        };
        (neutral, applied)
    };
    device.queue.submit([encoder.finish()]);
    readbacks
}

/// Records a render pass that clears `view` to `color` (linear RGBA).
pub fn encode_clear(encoder: &mut wgpu::CommandEncoder, view: &wgpu::TextureView, color: [f64; 4]) {
    let pass = encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("clear"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color {
                    r: color[0],
                    g: color[1],
                    b: color[2],
                    a: color[3],
                }),
                store: wgpu::StoreOp::Store,
            },
        })],
        ..Default::default()
    });
    drop(pass);
}

/// The text of the error `Surface::configure` gives when the queue was not empty after wgpu
/// waited for it (wgpu-core `ConfigureSurfaceError::GpuWaitTimeout`): another thread submitted
/// work meanwhile. The boot pipeline warm renders its canonical frame on a worker thread, so the
/// first resize after launch can meet it (ADR-1749: a Pixel 6 cold start rotating to landscape).
pub(crate) const CONFIGURE_RACE: &str = "Failed to wait for GPU to come idle";

/// What [`configure_surface`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigureOutcome {
    /// The surface is configured (or the error went to the device's error handler as before).
    Done,
    /// Another thread submitted while wgpu waited for the queue: configure again next frame.
    Retry,
}

/// Classifies the error a scoped `Surface::configure` produced: `None` is done, the queue race
/// is a retry, and anything else is `Some(false)`: repeat the call outside the scope so the error
/// reaches the uncaptured-error handler unchanged.
pub(crate) fn classify_configure_error(error: Option<&str>) -> Option<ConfigureOutcome> {
    match error {
        None => Some(ConfigureOutcome::Done),
        Some(text) if text.contains(CONFIGURE_RACE) => Some(ConfigureOutcome::Retry),
        Some(_) => None,
    }
}

/// `Surface::configure` inside a validation error scope on this thread. The queue race (see
/// [`CONFIGURE_RACE`]) leaves the configure pending for the next frame instead of reaching the
/// uncaptured-error handler, which panics in debug builds; any other error is reproduced by a
/// second, unscoped configure so the handler sees it exactly as before.
pub(crate) fn configure_surface(
    device: &wgpu::Device,
    surface: &wgpu::Surface<'static>,
    config: &wgpu::SurfaceConfiguration,
) -> ConfigureOutcome {
    let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
    surface.configure(device, config);
    let error = regia_core::tasks::block_on(scope.pop()).map(|e| e.to_string());
    match classify_configure_error(error.as_deref()) {
        Some(ConfigureOutcome::Retry) => {
            regia_core::telemetry::add(regia_core::telemetry::Counter::SurfaceOther, 1);
            tracing::info!(
                target: "regia_render",
                "surface configure {}x{} met work submitted by another thread; retrying next frame",
                config.width,
                config.height
            );
            ConfigureOutcome::Retry
        }
        Some(done) => done,
        None => {
            surface.configure(device, config);
            ConfigureOutcome::Done
        }
    }
}

#[cfg(test)]
mod tests {
    /// Audit G render5 #11: a plan that found no format (an empty capability list) must not be
    /// latched as applied, or the window keeps its old swap chain until another input changes.
    /// It is asked again after [`super::REPLAN_RETRY_FRAMES`], not every frame (the query is a
    /// driver round trip that can block).
    #[test]
    fn a_failed_swapchain_plan_is_retried() {
        use super::{REPLAN_RETRY_FRAMES, after_plan, wants_replan};
        let key = 7u32;
        assert!(wants_replan(None, &key, 1), "a new window plans");
        let failed = after_plan(key, false, 1);
        assert!(
            !wants_replan(Some(&failed), &key, 2),
            "a failed plan is not re-queried every frame"
        );
        assert!(
            wants_replan(Some(&failed), &key, 1 + REPLAN_RETRY_FRAMES),
            "a failed plan was latched as applied"
        );
        let ok = after_plan(key, true, 1 + REPLAN_RETRY_FRAMES);
        assert!(!wants_replan(Some(&ok), &key, 10_000), "a good plan is kept");
        assert!(wants_replan(Some(&ok), &8, 10_001), "a changed input plans again");
        // A changed input plans at once, also after a failure.
        assert!(wants_replan(Some(&failed), &8, 2));
    }

    /// Audit G render5 #2 and #9: the window handoff installs the enclosure shell (the daylit
    /// interior bake reads it) and the shadow casters' two-phase occlusion flag every frame, and
    /// clears both when the extracted frame drops them.
    #[test]
    fn the_window_handoff_installs_the_interior_shell_and_shadow_occlusion() {
        let shell = crate::SdfScene {
            shapes: vec![crate::SdfShape::Box {
                center: [0.0, 1.5, 0.0],
                half: [4.0, 1.5, 4.0],
                albedo: [0.6; 3],
                emissive: [0.0; 3],
            }],
        };
        let mut renderer = crate::FrameRenderer::new();
        let on = crate::ExtractedFrame {
            enclosure: Some(shell),
            shadow_occlusion: true,
            ..crate::ExtractedFrame::default()
        };
        super::sync_window_frame(&mut renderer, &on);
        assert_eq!(
            renderer.interior_scene().map(|scene| scene.shapes.len()),
            Some(1),
            "the window frame did not install the enclosure shell"
        );
        assert!(
            renderer.shadow_occlusion(),
            "Some(true) did not reach the renderer"
        );
        super::sync_window_frame(&mut renderer, &crate::ExtractedFrame::default());
        assert!(
            renderer.interior_scene().is_none(),
            "a removed shell stayed installed"
        );
        assert!(!renderer.shadow_occlusion(), "Some(false) did not clear the flag");
    }

    /// ADR-1749: the queue race of a surface configure retries next frame; other errors are
    /// repeated unscoped for the uncaptured-error handler; no error is done.
    #[test]
    fn a_configure_that_meets_another_threads_submission_is_retried() {
        use super::{ConfigureOutcome, classify_configure_error};
        let race = "Validation Error\n\nCaused by:\n  In Surface::configure\n    \
                    Failed to wait for GPU to come idle before reconfiguring the Surface\n";
        assert_eq!(
            classify_configure_error(Some(race)),
            Some(ConfigureOutcome::Retry)
        );
        assert_eq!(classify_configure_error(None), Some(ConfigureOutcome::Done));
        let other = "Validation Error\n\nCaused by:\n  In Surface::configure\n    \
                     Requested format Rgba16Float is not in list of supported formats\n";
        assert_eq!(classify_configure_error(Some(other)), None);
    }

    #[test]
    fn pixel_viewport_origin_at_one_does_not_panic() {
        let rect = super::pixel_viewport([1.0, 1.0, 0.5, 0.5], 1920, 1080);
        assert_eq!(rect, [1919, 1079, 1, 1]);
        assert_eq!(
            super::pixel_viewport([0.0, 0.0, 1.0, 1.0], 100, 50),
            [0, 0, 100, 50]
        );
        assert_eq!(super::pixel_viewport([1.0, 0.0, 1.0, 1.0], 0, 0)[2], 1);
    }

    use std::sync::atomic::{AtomicUsize, Ordering};

    /// ADR-276: the bundle hook runs first, then the deliberate exit begins (the native crash
    /// handler is disarmed), then the exit is requested; the host's finish exits once with 3.
    /// No GPU: the hook, the exit and the handler are recorders.
    #[test]
    fn device_lost_exit_writes_the_bundle_then_disarms_then_exits_once_with_its_code() {
        use std::sync::{Arc, Mutex, PoisonError};
        type Log = Arc<Mutex<Vec<String>>>;
        fn push(log: &Log, line: String) {
            log.lock().unwrap_or_else(PoisonError::into_inner).push(line);
        }
        let log: Log = Arc::default();
        let mut world = World::new();
        world.init_resource::<Messages<AppExit>>();
        let mut hooks = super::DeviceLostHooks::default();
        let bundle_log = log.clone();
        hooks.add(move |_, _| push(&bundle_log, "bundle written".into()));
        world.insert_resource(hooks);
        // 3 unless another test of this binary marked a surface loss (4); either is the info's.
        let want =
            crate::gpu_health::device_lost_info().map_or(crate::DEVICE_LOST_EXIT_CODE, |i| i.exit_code());
        let exit = regia_platform::DeliberateExit::new();
        super::exit_for_lost_device(&mut world, |code| {
            assert!(exit.begin(code));
            push(&log, format!("handler disarmed (deliberate exit {code})"));
        });
        let requested: Vec<AppExit> = world.resource_mut::<Messages<AppExit>>().drain().collect();
        assert_eq!(
            requested,
            [AppExit::Error(
                std::num::NonZeroU8::new(want).unwrap_or(std::num::NonZeroU8::MIN)
            )]
        );
        for _ in 0..2 {
            let (flush_log, exit_log) = (log.clone(), log.clone());
            exit.finish_with(
                move || push(&flush_log, "logs flushed".into()),
                move |code| push(&exit_log, format!("exit {code}")),
            );
        }
        assert_eq!(
            *log.lock().unwrap_or_else(PoisonError::into_inner),
            [
                "bundle written".to_owned(),
                format!("handler disarmed (deliberate exit {want})"),
                "logs flushed".to_owned(),
                format!("exit {want}"),
            ]
        );
    }

    /// ADR-2307: the lost-device exit also writes a crash report (`crash-*.txt`, which
    /// `regia_crash` uploads on the next launch) carrying the breadcrumb trail, when the platform
    /// configured a crash directory. No GPU: one recorded submission is enough for the trail.
    #[test]
    fn device_lost_exit_writes_a_crash_report_with_the_breadcrumb_trail() {
        let dir = regia_test_utils::unique_temp_dir("surface-loss-report");
        regia_platform::crash::configure(
            Some(dir.clone()),
            regia_platform::crash::BuildInfo::detect(),
            regia_platform::OsInfo::detect(),
        );
        regia_platform::crash::add_report_section("gpu_breadcrumbs", crate::breadcrumbs::crash_trail);
        let mut list = crate::breadcrumbs::CrumbList::new();
        drop(crate::breadcrumbs::global().pass(&mut list, "surface_loss_pass"));
        crate::breadcrumbs::note_submit(&[&list], 1);
        let mut world = World::new();
        world.init_resource::<Messages<AppExit>>();
        super::exit_for_lost_device(&mut world, |_| {});
        let texts: Vec<String> = std::fs::read_dir(&dir)
            .map(|d| {
                d.filter_map(Result::ok)
                    .filter(|e| regia_platform::crash::is_crash_file_name(&e.file_name().to_string_lossy()))
                    .filter_map(|e| std::fs::read_to_string(e.path()).ok())
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            texts.iter().any(|t| t.contains("gpu breadcrumbs: level")),
            "a crash report with the trail: {texts:?}"
        );
        assert!(
            texts.iter().all(|t| !t.contains("\npanic: ")),
            "not a panic report"
        );
        regia_platform::crash::configure(
            None,
            regia_platform::crash::BuildInfo::detect(),
            regia_platform::OsInfo::detect(),
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ADR-276: once the lost-device exit began, the crash handler's gate ends the process with
    /// the exit code and never runs the crash callback (no second bundle). No GPU, no signals.
    #[test]
    fn crash_handler_after_device_lost_exit_ends_with_the_code_and_runs_no_callback() {
        use std::cell::Cell;
        let mut world = World::new();
        world.init_resource::<Messages<AppExit>>();
        let exit = regia_platform::DeliberateExit::new();
        super::exit_for_lost_device(&mut world, |code| assert!(exit.begin(code)));
        let want = exit.code().expect("the exit began");
        let ran = Cell::new(false);
        let ended = exit.run_crash_hook(|| ran.set(true), || ran.set(true));
        assert_eq!(ended, Some(want));
        assert!(
            !ran.get(),
            "the crash callback must not run after a deliberate exit"
        );
        assert!(
            want == 3 || want == 4,
            "lost device 3 or lost surface 4, got {want}"
        );
    }

    #[derive(Resource, Default)]
    struct Seen(Vec<WindowResized>);

    fn collect_resizes(world: &mut World, mut cursor: Local<MessageCursor<WindowResized>>) {
        let resizes = super::new_resizes(world, &mut cursor);
        world.resource_mut::<Seen>().0.extend(resizes);
    }

    /// ADR-527: the renderer's lifecycle handler feeds the surface audit after every transition
    /// (no GPU: a suspend with no window). A counter that does not add up is reported.
    #[test]
    fn lifecycle_handler_feeds_the_surface_audit() {
        use regia_platform::lifecycle_drill::SurfaceViolationKind;
        let mut world = World::new();
        world.init_resource::<super::WindowSurfaces>();
        world.init_resource::<regia_platform::WindowHandles>();
        world.init_resource::<regia_platform::SurfaceAudit>();
        world.insert_resource(regia_platform::AppLifecycle {
            last_event: Some(regia_platform::LifecycleEvent::Suspended),
            ..Default::default()
        });
        super::on_lifecycle(&mut world);
        let audit = world.resource::<regia_platform::SurfaceAudit>();
        assert_eq!(audit.suspends(), 1);
        assert!(audit.is_clean(), "{:?}", audit.violations());

        // A surface counted as created that nothing dropped: the audit sees the leak.
        world.resource_mut::<super::WindowSurfaces>().created_total = 1;
        super::on_lifecycle(&mut world);
        let audit = world.resource::<regia_platform::SurfaceAudit>();
        assert_eq!(audit.suspends(), 2);
        let kinds: Vec<_> = audit.violations().iter().map(|v| v.kind).collect();
        assert_eq!(kinds, [SurfaceViolationKind::CountMismatch]);
    }

    /// Spec 54 § 9: a resume arms the breadcrumb resume barrier on the process ring, so the
    /// callbacks of work in flight at the suspension that never arrive (Android) do not leave
    /// `completed` stuck and a later stall blamed on the wrong passes. No GPU: a resume with no
    /// window and no device. The ring is process-wide and GPU tests in this binary may complete
    /// a submission between the call and the read (that clears the barrier by design), so a
    /// run in which a completion arrived is repeated.
    #[test]
    fn resume_arms_the_breadcrumb_barrier() {
        let mut armed = false;
        for _ in 0..8 {
            let mut world = World::new();
            world.init_resource::<super::WindowSurfaces>();
            world.init_resource::<regia_platform::WindowHandles>();
            world.init_resource::<regia_platform::SurfaceAudit>();
            world.insert_resource(regia_platform::AppLifecycle {
                last_event: Some(regia_platform::LifecycleEvent::Resumed),
                ..Default::default()
            });
            let (_, seen_before) = crate::breadcrumbs::global().resume_barrier();
            super::on_lifecycle(&mut world);
            let (barrier, seen_after) = crate::breadcrumbs::global().resume_barrier();
            if barrier.is_some() {
                armed = true;
                break;
            }
            assert_ne!(
                seen_before, seen_after,
                "no completion arrived, yet the resume left no barrier"
            );
        }
        assert!(armed, "the resume never armed the barrier");
    }

    /// audit-g-render3-platform: on suspend/terminate the frame generation present worker must
    /// be cancelled before the surfaces drop — between frames it can still hold a swap-chain
    /// image (it presents at the pacer's time on its own thread), and an image must not be
    /// presented onto, or outlive, its surface. fixrev-19-k: the test installs a live worker
    /// (no payload, no GPU), so with the `cancel_present()` wiring removed the worker is
    /// still `Some` after `on_lifecycle` and the assertion fails — the pre-fix version
    /// asserted on a `None` worker and passed with the wiring deleted. The drop-without-
    /// present behavior itself is unit-tested in `frame_gen::present`.
    #[cfg(feature = "frame-gen")]
    #[test]
    fn suspend_cancels_the_frame_gen_present_worker_before_surfaces_drop() {
        use regia_platform::LifecycleEvent;
        let mut world = World::new();
        world.init_resource::<super::WindowSurfaces>();
        world.init_resource::<regia_platform::WindowHandles>();
        world.init_resource::<regia_platform::SurfaceAudit>();
        world.insert_resource(crate::frame_gen::present::FrameGenPresenter::default());
        for event in [LifecycleEvent::Suspended, LifecycleEvent::WillTerminate] {
            world
                .resource_mut::<crate::frame_gen::present::FrameGenPresenter>()
                .worker = Some(
                crate::frame_gen::present::PresentWorker::new(std::time::Instant::now(), |_| {})
                    .unwrap_or_else(|e| panic!("{e}")),
            );
            world.insert_resource(regia_platform::AppLifecycle {
                last_event: Some(event),
                ..Default::default()
            });
            super::on_lifecycle(&mut world);
            assert!(
                world
                    .resource::<crate::frame_gen::present::FrameGenPresenter>()
                    .worker
                    .is_none(),
                "{event:?}: the suspend path must leave the presenter without a present worker"
            );
        }
    }

    /// Finding 1 (audit G, render5 determinism): the window that drives shared per-frame
    /// state (the UI scale, VFX targets, mesh-LOD height and the local-capture bake) must be
    /// a function of the window entities, never of map iteration order. `primary_size`,
    /// `primary_format`, the published `LodViewport` and the bake all resolve through
    /// `pick_primary`.
    #[test]
    fn primary_window_selection_never_follows_iteration_order() {
        let (a, b) = (Entity::from_bits(0x100), Entity::from_bits(0x200));
        // Insertion order must not matter: the lowest entity wins either way.
        assert_eq!(super::pick_primary(None, [b, a].into_iter()), Some(a));
        assert_eq!(super::pick_primary(None, [a, b].into_iter()), Some(a));
        // The recorded PrimaryWindow entity wins while its surface is live...
        assert_eq!(super::pick_primary(Some(b), [b, a].into_iter()), Some(b));
        assert_eq!(super::pick_primary(Some(b), [a, b].into_iter()), Some(b));
        // ...and a stale record (its surface is gone) falls back to the lowest entity.
        assert_eq!(
            super::pick_primary(Some(Entity::from_bits(0x999)), [b, a].into_iter()),
            Some(a)
        );
        assert_eq!(super::pick_primary(None, std::iter::empty()), None);
    }

    /// Surface creation is retried exactly once; a second failure is returned (and then ends the
    /// game as `surface_unavailable` instead of spinning behind a blank window).
    #[test]
    fn surface_creation_retries_once() {
        let mut calls = 0;
        let ok_second = super::with_one_retry(|| {
            calls += 1;
            if calls == 1 { Err("transient") } else { Ok(()) }
        });
        assert!(ok_second.is_ok());
        assert_eq!(calls, 2);
        let mut calls = 0;
        let failed = super::with_one_retry(|| {
            calls += 1;
            Err(format!("cannot present ({calls})"))
        });
        assert_eq!(
            failed,
            Err("cannot present (2)".to_owned()),
            "the second error is kept"
        );
        assert_eq!(calls, 2, "no third attempt: a failing surface must not spin");
        let mut calls = 0;
        assert!(
            super::with_one_retry(|| {
                calls += 1;
                Ok::<(), String>(())
            })
            .is_ok()
        );
        assert_eq!(calls, 1);
    }

    #[test]
    fn resizes_written_between_frames_reach_pre_update() {
        let mut app = App::new();
        app.add_message::<WindowResized>()
            .init_resource::<Seen>()
            .add_systems(PreUpdate, collect_resizes);
        app.update();
        let window = app.world_mut().spawn_empty().id();
        // The runner writes the resize from the window event, before the next update.
        let resize = WindowResized {
            window,
            width: 2400,
            height: 1080,
        };
        app.world_mut().write_message(resize);
        app.update();
        assert_eq!(app.world().resource::<Seen>().0, [resize]);
        app.update();
        assert_eq!(app.world().resource::<Seen>().0.len(), 1, "read once");
    }

    use super::*;
    use crate::overlay::{OverlayDraw, WindowOverlay};
    use std::sync::Arc;

    struct CountingOverlay {
        hits: &'static AtomicUsize,
    }

    impl OverlayDraw for CountingOverlay {
        fn draw(
            &mut self,
            _device: &RenderDevice,
            encoder: &mut wgpu::CommandEncoder,
            view: &wgpu::TextureView,
            _format: wgpu::TextureFormat,
            _width: u32,
            _height: u32,
        ) {
            self.hits.fetch_add(1, Ordering::SeqCst);
            let _pass = encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("overlay count"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
        }
    }

    /// A quad mesh for the offscreen overlay test (unit square on the XZ plane).
    fn quad() -> crate::CpuMesh {
        let corners = [
            [-1.0, 0.0, -1.0],
            [1.0, 0.0, -1.0],
            [1.0, 0.0, 1.0],
            [-1.0, 0.0, 1.0],
        ];
        let uvs = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
        crate::CpuMesh {
            vertices: corners
                .into_iter()
                .zip(uvs)
                .map(|(position, uv)| crate::lab_frame::LabVertex {
                    position,
                    normal: [0.0, 1.0, 0.0],
                    uv,
                    tangent: [1.0, 0.0, 0.0, 1.0],
                    color: [1.0; 4],
                    joints: [0; 4],
                    weights: [0.0; 4],
                })
                .collect(),
            indices: vec![0, 2, 1, 0, 3, 2],
        }
    }

    fn extracted_with_quad() -> ExtractedFrame {
        use crate::runtime::{ExtractedCamera, ExtractedTarget, ExtractedView};
        let mut frame = ExtractedFrame::default();
        frame.views.push(ExtractedView {
            camera: ExtractedCamera {
                eye: [0.0, 3.0, 4.0],
                target: [0.0, 0.0, 0.0],
                fov_y: 1.0,
                near: 0.1,
                far: 30.0,
            },
            viewport: [0.0, 0.0, 1.0, 1.0],
            order: 0,
            clear: true,
            target: ExtractedTarget::Window,
            layer_mask: u32::MAX,
            post: crate::PostSettings {
                enabled: false,
                auto_exposure: false,
                exposure_ev: 14.0,
                ..crate::PostSettings::default()
            },
        });
        frame.meshes.push(crate::runtime::ExtractedMesh {
            is_static: false,
            entity: Entity::PLACEHOLDER,
            mesh: Arc::new(quad()),
            material: Arc::new(crate::CpuMaterial::default()),
            layer: 0,
            model: regia_core::math::Mat4::IDENTITY.to_cols_array_2d(),
            joints: Vec::new(),
            decal_layer: 0,
            uv1: None,
            shadow: crate::ShadowCast::All,
            draw_indices: None,
            lod_key: 0,
            object_key_xor: 0,
            gi_proxy: true,
            lod_dither: 0.0,
        });
        frame.directional = Some(crate::DirectionalLightUniform::new(
            [0.3, -0.9, 0.2],
            [1.0, 1.0, 1.0],
            100_000.0,
        ));
        frame.directional_shadows = false;
        frame.environment = crate::runtime::Environment::default();
        frame
    }

    #[test]
    fn hidden_scene_offscreen_invokes_overlay() {
        static HITS: AtomicUsize = AtomicUsize::new(0);
        let device = crate::test_device();
        let format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let (w, h) = (64u32, 64u32);
        let mut runtime = RuntimeRenderer::default();
        let mut overlay = WindowOverlay::default();
        overlay.set(Box::new(CountingOverlay { hits: &HITS }));
        let image = render_views_offscreen(
            &device,
            &mut runtime.0,
            &extracted_with_quad(),
            format,
            (w, h),
            Some(&overlay),
        )
        .expect("scene+overlay screenshot");
        assert_eq!(HITS.load(Ordering::SeqCst), 1, "overlay must run on scene path");
        assert_eq!(image.rgba.len(), (w * h * 4) as usize);
        let max_c = image.rgba.iter().copied().max().unwrap_or(0);
        assert!(max_c > 10, "scene readback too dark: max={max_c}");
    }

    #[test]
    fn the_fade_layer_blends_its_colour_over_the_scene() {
        let device = crate::test_device();
        let format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let render = |fade: Option<crate::FadeState>| {
            let mut runtime = RuntimeRenderer::default();
            let mut overlay = WindowOverlay::default();
            let handle = crate::install_fade(&mut overlay);
            if let Some(state) = fade {
                handle.set(state);
            }
            render_views_offscreen(
                &device,
                &mut runtime.0,
                &extracted_with_quad(),
                format,
                (64, 64),
                Some(&overlay),
            )
            .expect("scene+fade screenshot")
            .rgba
        };
        let baseline = render(None);
        let off = render(Some(crate::FadeState {
            amount: 0.0,
            color: [1.0, 0.0, 0.0],
        }));
        assert_eq!(baseline, off, "an amount of zero draws nothing");
        let full = render(Some(crate::FadeState {
            amount: 1.0,
            color: [1.0, 0.0, 0.0],
        }));
        assert!(
            full.as_chunks::<4>()
                .0
                .iter()
                .all(|p| p[0] == 255 && p[1] == 0 && p[2] == 0),
            "a full fade replaces every pixel with its colour"
        );
        let half = render(Some(crate::FadeState {
            amount: 0.5,
            color: [0.0, 0.0, 0.0],
        }));
        let sum = |pixels: &[u8]| pixels.iter().map(|&b| u64::from(b)).sum::<u64>();
        assert!(
            sum(&half) < sum(&baseline),
            "a half fade to black darkens the picture"
        );
        assert!(sum(&half) > 0, "and leaves part of it");
    }

    #[test]
    fn split_screen_views_composite_into_one_image() {
        let device = crate::test_device();
        let mut frame = extracted_with_quad();
        // View 0: left half, camera far above; view 1: right half, camera close.
        frame.views[0].viewport = [0.0, 0.0, 0.5, 1.0];
        frame.views[0].camera = crate::runtime::ExtractedCamera {
            eye: [0.0, 20.0, 24.0],
            target: [0.0, 0.0, 0.0],
            fov_y: 1.0,
            near: 0.1,
            far: 100.0,
        };
        frame.views.push(crate::runtime::ExtractedView {
            camera: crate::runtime::ExtractedCamera {
                eye: [0.0, 1.0, 3.0],
                target: [0.0, 0.0, 0.0],
                fov_y: 1.0,
                near: 0.1,
                far: 100.0,
            },
            viewport: [0.5, 0.0, 0.5, 1.0],
            order: 1,
            clear: true,
            target: crate::runtime::ExtractedTarget::Window,
            layer_mask: u32::MAX,
            post: crate::PostSettings {
                enabled: false,
                auto_exposure: false,
                exposure_ev: 14.0,
                ..crate::PostSettings::default()
            },
        });
        let mut runtime = RuntimeRenderer::default();
        let image = render_views_offscreen(
            &device,
            &mut runtime.0,
            &frame,
            wgpu::TextureFormat::Rgba8UnormSrgb,
            (128, 64),
            None,
        )
        .expect("split-screen render");
        // The two halves must differ (two different cameras), and both must be
        // drawn (not the plain clear colour).
        let pixel = |x: usize, y: usize| -> [u8; 4] {
            let i = (y * 128 + x) * 4;
            image.rgba[i..i + 4].try_into().expect("pixel")
        };
        let clear = [39, 39, 44, 255]; // ClearColor 0.02/0.02/0.025, sRGB-encoded
        // The quad lands mid-half in both viewports (each camera looks at the
        // origin), so the half centres must show lit floor, not the clear.
        assert_ne!(pixel(32, 32), clear, "left half must be drawn");
        assert_ne!(pixel(96, 32), clear, "right half must be drawn");
        // The close camera fills its half with the quad, the far one does not:
        // the halves' coverage must differ.
        let mut differing = 0usize;
        for y in 0..64 {
            for x in 0..64 {
                if pixel(x, y) != pixel(64 + x, y) {
                    differing += 1;
                }
            }
        }
        assert!(
            differing > 500,
            "the two viewports must show different cameras ({differing} differing pixels)"
        );
    }

    #[test]
    fn split_screen_with_post_draws_both_halves() {
        let device = crate::test_device();
        let post = crate::PostSettings {
            enabled: true,
            bloom: false,
            auto_exposure: false,
            exposure_ev: 14.0,
            film_grain: 0.0,
            vignette: 0.0,
            fxaa: false,
            taa: false,
            ..crate::PostSettings::default()
        };
        let mut frame = extracted_with_quad();
        frame.views[0].viewport = [0.0, 0.0, 0.5, 1.0];
        frame.views[0].post = post.clone();
        frame.views[0].camera = crate::runtime::ExtractedCamera {
            eye: [0.0, 20.0, 24.0],
            target: [0.0, 0.0, 0.0],
            fov_y: 1.0,
            near: 0.1,
            far: 100.0,
        };
        frame.views.push(crate::runtime::ExtractedView {
            camera: crate::runtime::ExtractedCamera {
                eye: [0.0, 1.0, 3.0],
                target: [0.0, 0.0, 0.0],
                fov_y: 1.0,
                near: 0.1,
                far: 100.0,
            },
            viewport: [0.5, 0.0, 0.5, 1.0],
            order: 1,
            clear: true,
            target: crate::runtime::ExtractedTarget::Window,
            layer_mask: u32::MAX,
            post,
        });
        let mut runtime = RuntimeRenderer::default();
        let image = render_views_offscreen(
            &device,
            &mut runtime.0,
            &frame,
            wgpu::TextureFormat::Rgba8UnormSrgb,
            (128, 64),
            None,
        )
        .expect("split-screen post render");
        let pixel = |x: usize, y: usize| -> [u8; 4] {
            let i = (y * 128 + x) * 4;
            image.rgba[i..i + 4].try_into().expect("pixel")
        };
        let clear = [39, 39, 44, 255];
        assert_ne!(pixel(32, 32), clear, "left half must be drawn with post on");
        assert_ne!(pixel(96, 32), clear, "right half must be drawn with post on");
        let mut differing = 0usize;
        for y in 0..64 {
            for x in 0..64 {
                if pixel(x, y) != pixel(64 + x, y) {
                    differing += 1;
                }
            }
        }
        assert!(
            differing > 500,
            "post split-screen halves must differ ({differing} differing pixels)"
        );
    }
}

#[cfg(test)]
mod viewport_tests {
    use super::pixel_viewport;

    #[test]
    fn full_viewport_covers_the_target() {
        assert_eq!(
            pixel_viewport([0.0, 0.0, 1.0, 1.0], 1920, 1080),
            [0, 0, 1920, 1080]
        );
    }

    #[test]
    fn origin_at_one_stays_inside_the_target_instead_of_panicking() {
        let [x, y, w, h] = pixel_viewport([1.0, 1.0, 0.0, 1.0], 1920, 1080);
        assert_eq!((x, y), (1919, 1079));
        assert!(w >= 1 && x + w <= 1920);
        assert!(h >= 1 && y + h <= 1080);
    }

    #[test]
    fn out_of_range_components_are_clamped_and_the_rect_fits() {
        let [x, y, w, h] = pixel_viewport([-3.0, 0.5, 9.0, 9.0], 100, 50);
        assert_eq!((x, y), (0, 25));
        assert_eq!((w, h), (100, 25));
    }

    #[test]
    fn a_zero_sized_target_yields_a_one_pixel_rect_at_the_origin() {
        assert_eq!(pixel_viewport([0.5, 0.5, 0.5, 0.5], 0, 0), [0, 0, 1, 1]);
    }
}

#[cfg(test)]
mod late_overlay_tests {
    use super::{OverlayLayersWarmed, warm_late_overlay_layers};
    use crate::overlay::{OverlayDraw, WindowOverlay};
    use crate::{PipelineWarmQueue, RenderDevice, WarmTier};
    use regia_app::prelude::*;
    use regia_ecs::ecs::system::RunSystemOnce;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Records the formats it was warmed for.
    struct Recorder(Arc<Mutex<Vec<wgpu::TextureFormat>>>);

    impl OverlayDraw for Recorder {
        fn draw(
            &mut self,
            _: &RenderDevice,
            _: &mut wgpu::CommandEncoder,
            _: &wgpu::TextureView,
            _: wgpu::TextureFormat,
            _: u32,
            _: u32,
        ) {
        }

        fn warm(&mut self, _: &RenderDevice, format: wgpu::TextureFormat) {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(format);
        }
    }

    fn boot_jobs(queue: &PipelineWarmQueue) -> u32 {
        queue.progress().tiers[WarmTier::Boot as usize].jobs_total
    }

    fn run(world: &mut World) {
        if let Err(err) = world.run_system_once(warm_late_overlay_layers) {
            panic!("system: {err}");
        }
    }

    /// ADR-5131: the runtime UI installs its layer in `Startup`, after the first resume started the
    /// queue; it still gets a boot-tier warm job (window format and both intermediates), once.
    /// A layer the queue's start already queued is not queued again.
    #[test]
    fn an_overlay_layer_installed_after_the_queue_started_gets_one_boot_job() {
        let device = match RenderDevice::headless() {
            Ok(device) => device,
            Err(err) => {
                eprintln!("note: no GPU adapter ({err}); skipped");
                return;
            }
        };
        let queue = PipelineWarmQueue::new(1);
        queue.start(&device);
        let mut world = World::new();
        world.insert_resource(queue.clone());
        world.insert_resource(WindowOverlay::default());
        run(&mut world);
        assert_eq!(boot_jobs(&queue), 0, "no layer yet");

        let early = Arc::new(Mutex::new(Vec::new()));
        let early_layer =
            world
                .resource_mut::<WindowOverlay>()
                .insert(-1, "early", Box::new(Recorder(early.clone())));
        let mut started = OverlayLayersWarmed::default();
        started.record(&[early_layer]);
        world.insert_resource(started);
        run(&mut world);
        assert_eq!(boot_jobs(&queue), 0, "queued when the queue started");

        let late = Arc::new(Mutex::new(Vec::new()));
        world
            .resource_mut::<WindowOverlay>()
            .set(Box::new(Recorder(late.clone())));
        run(&mut world);
        run(&mut world);
        assert_eq!(boot_jobs(&queue), 1, "one job for the late layer");
        assert!(
            queue.wait_tier(WarmTier::Boot, Duration::from_secs(120)),
            "boot tier"
        );
        let seen = late
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(
            seen,
            vec![
                wgpu::TextureFormat::Bgra8UnormSrgb,
                crate::display_stage::SDR_INTERMEDIATE,
                crate::display_stage::HDR_INTERMEDIATE,
            ]
        );
        assert!(
            early
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "the start's own job warms that one"
        );
        queue.shutdown();
    }

    /// fixrev-demo-warm-k (review finding 4): the record kept layer names, and `set` (and `insert`
    /// with a used name) replaces the layer under the same name. A game that replaced the `"ui"`
    /// layer after the queue's start warmed the old one got no job for its own UI: its pipelines
    /// compiled on its first draw, on the frame thread. The replacement is a new layer: one job.
    #[test]
    fn a_layer_replaced_under_the_same_name_gets_its_own_boot_job() {
        let device = match RenderDevice::headless() {
            Ok(device) => device,
            Err(err) => {
                eprintln!("note: no GPU adapter ({err}); skipped");
                return;
            }
        };
        let queue = PipelineWarmQueue::new(1);
        queue.start(&device);
        let mut world = World::new();
        world.insert_resource(queue.clone());
        world.insert_resource(WindowOverlay::default());

        let plugin_ui = Arc::new(Mutex::new(Vec::new()));
        world
            .resource_mut::<WindowOverlay>()
            .set(Box::new(Recorder(plugin_ui.clone())));
        run(&mut world);
        assert_eq!(boot_jobs(&queue), 1, "the first ui layer");

        let game_ui = Arc::new(Mutex::new(Vec::new()));
        world
            .resource_mut::<WindowOverlay>()
            .set(Box::new(Recorder(game_ui.clone())));
        run(&mut world);
        run(&mut world);
        assert_eq!(boot_jobs(&queue), 2, "one job for the replacement ui layer");
        assert!(
            queue.wait_tier(WarmTier::Boot, Duration::from_secs(120)),
            "boot tier"
        );
        let seen = game_ui
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(
            seen,
            vec![
                wgpu::TextureFormat::Bgra8UnormSrgb,
                crate::display_stage::SDR_INTERMEDIATE,
                crate::display_stage::HDR_INTERMEDIATE,
            ],
            "the game's ui is warmed"
        );
        queue.shutdown();
    }
}
