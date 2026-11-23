// SPDX-License-Identifier: MPL-2.0
//! The window's frame while frame generation runs (ADR-2067; the cargo feature `frame-gen`, off by
//! default). `render_windows` hands a window here when frame generation is enabled and can run;
//! anything else takes the plain path, untouched.
//!
//! One tick, with `A` and `B` the two swap-chain images it presents:
//!
//! 1. The scene renders into a [`crate::frame_gen::present::SceneImages`] image, not the swap chain.
//! 2. The pacer decides from the time the frame finished.
//! 3. The interpolator runs. `A` shows its output, with the overlay drawn over it and through the
//!    display stage; the worker presents `A` at the generated time.
//! 4. `B` shows the rendered image with the overlay; the worker presents `B` half an interval later
//!    while the main thread starts the next frame.
//!
//! Any error presents the rendered frame plainly and keeps frame generation off until it is turned
//! off and on again (`pacing.present.last_error` on the agent protocol).

use regia_app::prelude::*;
use regia_platform::WindowHandles;

use super::{PlanFields, WindowSurface, configure_surface, encode_screenshot_copies};
use crate::frame_gen::passes::FrameGenFrame;
use crate::frame_gen::present::{BlitSource, FrameGenPresenter, PresentWorker, SceneImages};
use crate::frame_gen::{
    FrameGenCamera, FrameGenDispatch, FrameGenEnvironment, FrameGenFallback, FrameGenPacing,
    FrameGenSettings, PacingDecision, TransferFunction, desktop_gpu, frame_gen_status,
};
use crate::{ExtractedFrame, FrameTarget, RenderDevice, RuntimeRenderer};

/// What the window's frame reads.
pub(super) struct FrameGenWindow<'a> {
    pub(super) device: &'a RenderDevice,
    pub(super) entity: Entity,
    pub(super) surface: &'a mut WindowSurface,
    pub(super) windows: usize,
    pub(super) frame: Option<&'a ExtractedFrame>,
    pub(super) splash_done: bool,
    pub(super) overlay: Option<&'a crate::WindowOverlay>,
    pub(super) calibration: &'a crate::DisplayCalibration,
    pub(super) pattern: Option<&'a crate::HdrTestPattern>,
    pub(super) runtime: &'a mut RuntimeRenderer,
    pub(super) screenshots: &'a mut crate::ScreenshotRequests,
    pub(super) handles: &'a WindowHandles,
    pub(super) focused: bool,
}

/// How a window's frame went.
#[derive(Default)]
pub(super) struct FrameGenOutcome {
    /// A swap-chain image was acquired.
    pub(super) acquired: bool,
    /// The surface was reported lost.
    pub(super) lost: bool,
    /// The scene drew.
    pub(super) drew: bool,
    /// The present thread's ticket of the image that shows the rendered scene
    /// ([`crate::scene_ready::note_present_queued`]). `None` when the scene drew but its image
    /// never reached the present thread (the real image's acquire failed after the generated
    /// image was queued): only this image's present may count as the scene's (ADR-8122).
    pub(super) scene_ticket: Option<u64>,
}

impl FrameGenOutcome {
    /// Queues a lost surface for re-creation; true when this one was.
    pub(super) fn note(&self, entity: Entity, lost: &mut Vec<Entity>) -> bool {
        if self.lost {
            lost.push(entity);
        }
        self.lost
    }

    /// A drawn scene counts for the boot warm, as on the plain path. Its rendered image is
    /// queued on the present thread, not presented yet: an open prepare becomes `presented` when
    /// that thread has presented it (ADR-7883). `(pending_meshes, pipelines)`: what the frame
    /// still uploaded and compiled on this thread.
    pub(super) fn note_warm(
        &self,
        warm: Option<&crate::PipelineWarmQueue>,
        game_warm: Option<&mut crate::GameFrameWarm>,
        tick_start: std::time::Instant,
        (pending_meshes, pipelines): (usize, u64),
    ) {
        if !self.drew {
            return;
        }
        // The scene image's own ticket, never the newest one queued: a generated image queued
        // before a failed acquire of the real one must not present the prepare (ADR-8122).
        let present = match self.scene_ticket {
            Some(ticket) => crate::scene_ready::PresentNote::Queued {
                ticket,
                flushes: crate::scene_ready::present_progress().1,
            },
            None => crate::scene_ready::PresentNote::Dropped,
        };
        let check = crate::scene_ready::SceneFrameCheck {
            pending_meshes,
            pipelines,
            present,
        };
        let first =
            game_warm.is_none_or(|gw| gw.note_frame_with(tick_start.elapsed().as_secs_f32() * 1000.0, check));
        if first && let Some(queue) = warm {
            queue.note_scene_frame();
        }
    }
}

/// [`render_window`] when frame generation's resources exist (a windowed app with the feature).
pub(super) fn take_window(
    resources: &mut super::FrameGenParams<'_>,
    window: FrameGenWindow<'_>,
) -> Option<FrameGenOutcome> {
    let (settings, pacing, presenter) = resources;
    render_window(
        window,
        settings.as_deref()?,
        pacing.as_deref_mut()?,
        presenter.as_deref_mut()?,
    )
}

/// Why this tick cannot use frame generation; `None` when it can.
fn fallback_for(window: &FrameGenWindow<'_>) -> Option<FrameGenFallback> {
    if window.windows != 1 {
        return Some(FrameGenFallback::MultipleWindows);
    }
    if !window.splash_done {
        return Some(FrameGenFallback::Splash);
    }
    let Some(frame) = window.frame else {
        return Some(FrameGenFallback::NoScene);
    };
    let views = frame.window_views();
    match views.as_slice() {
        [] => Some(FrameGenFallback::NoScene),
        [(_, view)] if view.viewport == [0.0, 0.0, 1.0, 1.0] && view.clear => None,
        _ => Some(FrameGenFallback::MultipleViews),
    }
}

/// Renders the window's frame through frame generation, or returns `None` for the plain path
/// (which then finds the worker idle: it needs the swap chain free).
fn render_window(
    mut window: FrameGenWindow<'_>,
    settings: &FrameGenSettings,
    pacing: &mut FrameGenPacing,
    presenter: &mut FrameGenPresenter,
) -> Option<FrameGenOutcome> {
    if !settings.enabled {
        // Nothing per frame while it is off, past leaving the last run behind once.
        if presenter.was_active {
            leave(&mut window, presenter);
        }
        pacing.present.fallback = None;
        pacing.present.last_error = None;
        presenter.failed = None;
        return None;
    }
    let capabilities = &window.device.capabilities;
    let env = FrameGenEnvironment::current(
        false,
        Some(desktop_gpu(
            capabilities.backend,
            capabilities.max_storage_textures_per_shader_stage,
            capabilities.max_storage_buffers_per_shader_stage,
        )),
    );
    let fallback = if presenter.failed.is_some() {
        Some(FrameGenFallback::Failed)
    } else if !frame_gen_status(settings, env, None).can_enable() {
        Some(FrameGenFallback::Blocked)
    } else {
        fallback_for(&window)
    };
    pacing.present.fallback = fallback;
    if fallback.is_some() {
        leave(&mut window, presenter);
        return None;
    }
    if !presenter.was_active {
        presenter.was_active = true;
        presenter.force_reset = true;
        window.runtime.0.set_frame_gen_capture(true);
        if let Some(worker) = presenter.worker.as_ref() {
            worker.reset_gaps();
        }
    }
    use_mailbox_under_vsync(window.surface, presenter);
    let _span = tracing::info_span!("frame_gen_window").entered();
    match render_frame(&mut window, settings, pacing, presenter) {
        Ok(outcome) => Some(outcome),
        Err(failure) => {
            tracing::error!(target: "regia_render", "frame generation failed, presenting plainly: {failure}");
            pacing.present.last_error = Some(failure.clone());
            pacing.present.fallback = Some(FrameGenFallback::Failed);
            presenter.failed = Some(failure);
            leave(&mut window, presenter);
            None
        }
    }
}

/// A vsync swap chain shows every present on its own refresh, so two presents per tick would
/// halve the rendered frame rate. While frame generation runs the window uses Mailbox when the
/// surface offers it: it does not tear either. `render.swapchain` reports the mode in effect.
fn use_mailbox_under_vsync(surface: &mut WindowSurface, presenter: &mut FrameGenPresenter) {
    hold_mailbox(surface.plan_fields(), &mut presenter.host_present_mode);
}

/// [`use_mailbox_under_vsync`] on a surface's plan fields: the hold reads the surface's current
/// plan epoch.
fn hold_mailbox(fields: PlanFields<'_>, held: &mut HeldPresentMode) {
    if let Some(mode) = held.hold(
        fields.info.plan.present_mode,
        fields.info.effective_present_mode(),
        &fields.info.offered_present_modes,
        *fields.plan_epoch,
    ) {
        set_present_mode(fields, mode);
    }
}

/// Puts the host's held present mode back when frame generation stops, unless the surface was
/// re-planned since (the release reads the surface's current plan epoch).
fn restore_host_mode(fields: PlanFields<'_>, held: &mut HeldPresentMode) {
    if let Some(mode) = held.release(*fields.plan_epoch) {
        set_present_mode(fields, mode);
    }
}

/// The host's present mode while frame generation holds a window on Mailbox, with the surface
/// plan (`WindowSurface::plan_epoch`) it was read from.
///
/// `sync_surface_output` re-plans the swap chain when the host's present mode, the output's
/// HDR request or the display's HDR mode changes, frame generation or not, and writes the
/// host's new mode into the plan. A held mode from an older plan is then stale: the new plan is
/// the host's current choice, so it is held instead, and a release after the re-plan puts
/// nothing back (ADR-7413; the stale snapshot used to revert a no-vsync switch made while frame
/// generation ran).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HeldPresentMode {
    held: Option<(wgpu::PresentMode, u64)>,
}

impl HeldPresentMode {
    /// The mode to configure while frame generation runs, if it differs from the plan: Mailbox
    /// for a vsync window that offers it (`wants_mailbox`), remembering the host's `plan_mode`.
    fn hold(
        &mut self,
        plan_mode: wgpu::PresentMode,
        effective: wgpu::PresentMode,
        offered: &[wgpu::PresentMode],
        epoch: u64,
    ) -> Option<wgpu::PresentMode> {
        if self.held.is_some_and(|(_, held_epoch)| held_epoch != epoch) {
            self.held = None;
        }
        if !wants_mailbox(effective, offered) {
            return None;
        }
        self.held.get_or_insert((plan_mode, epoch));
        Some(wgpu::PresentMode::Mailbox)
    }

    /// The host's mode to put back when frame generation stops: the held one, unless the
    /// surface was re-planned since (it then has the host's mode already).
    fn release(&mut self, epoch: u64) -> Option<wgpu::PresentMode> {
        self.held
            .take()
            .filter(|(_, held_epoch)| *held_epoch == epoch)
            .map(|(mode, _)| mode)
    }
}

/// Whether the time between two rendered frames is too long to interpolate across.
fn is_pause(last_ready_ns: u64, ready_ns: u64) -> bool {
    ready_ns.saturating_sub(last_ready_ns) > crate::frame_gen::present::MAX_PRESENT_WAIT_NS * 2
}

/// Whether a window on `effective` should move to Mailbox while frame generation runs.
fn wants_mailbox(effective: wgpu::PresentMode, offered: &[wgpu::PresentMode]) -> bool {
    matches!(
        effective,
        wgpu::PresentMode::Fifo | wgpu::PresentMode::FifoRelaxed
    ) && offered.contains(&wgpu::PresentMode::Mailbox)
}

fn set_present_mode(fields: PlanFields<'_>, mode: wgpu::PresentMode) {
    fields.info.plan.present_mode = mode;
    fields.config.present_mode = mode;
    *fields.needs_configure = true;
}

/// The plain path takes over: the swap chain must be free, the capture off and the host's present
/// mode back.
fn leave(window: &mut FrameGenWindow<'_>, presenter: &mut FrameGenPresenter) {
    presenter.drain();
    if presenter.was_active {
        presenter.was_active = false;
        window.runtime.0.set_frame_gen_capture(false);
        presenter.passes.invalidate();
    }
    restore_host_mode(window.surface.plan_fields(), &mut presenter.host_present_mode);
}

/// A swap-chain image, or the reason there is none (`Err(true)` = the surface is lost).
fn acquire(surface: &mut WindowSurface) -> Result<wgpu::SurfaceTexture, bool> {
    use regia_core::telemetry::{self, Counter};
    let _acquire = tracing::info_span!("acquire").entered();
    match surface.surface.get_current_texture() {
        wgpu::CurrentSurfaceTexture::Success(frame) => Ok(frame),
        wgpu::CurrentSurfaceTexture::Suboptimal(frame) => {
            surface.needs_configure = true;
            Ok(frame)
        }
        wgpu::CurrentSurfaceTexture::Outdated => {
            telemetry::add(Counter::SurfaceOutdated, 1);
            surface.needs_configure = true;
            Err(false)
        }
        wgpu::CurrentSurfaceTexture::Lost => {
            telemetry::add(Counter::SurfaceLost, 1);
            Err(true)
        }
        wgpu::CurrentSurfaceTexture::Timeout => {
            telemetry::add(Counter::SurfaceTimeout, 1);
            Err(false)
        }
        wgpu::CurrentSurfaceTexture::Occluded | wgpu::CurrentSurfaceTexture::Validation => {
            telemetry::add(Counter::SurfaceOther, 1);
            Err(false)
        }
    }
}

/// The image the overlay and the display stage work on: the swap-chain image, or the stage's
/// intermediate.
struct Composition {
    view: wgpu::TextureView,
    texture: Option<wgpu::Texture>,
    format: wgpu::TextureFormat,
}

fn composition(
    window: &mut FrameGenWindow<'_>,
    swap_image: &wgpu::SurfaceTexture,
    swap_view: &wgpu::TextureView,
    stage_format: Option<wgpu::TextureFormat>,
) -> Composition {
    let s = &mut *window.surface;
    match stage_format {
        Some(format) => {
            let stage =
                s.display
                    .intermediate(&window.device.device, (s.config.width, s.config.height), format);
            Composition {
                view: stage.view.clone(),
                texture: Some(stage.texture.clone()),
                format,
            }
        }
        None => Composition {
            view: swap_view.clone(),
            texture: s
                .config
                .usage
                .contains(wgpu::TextureUsages::COPY_SRC)
                .then(|| swap_image.texture.clone()),
            format: s.config.format,
        },
    }
}

/// The overlay over the composed image, then the display stage onto the swap-chain image.
fn finish_composition(
    window: &mut FrameGenWindow<'_>,
    encoder: &mut wgpu::CommandEncoder,
    image: &Composition,
    swap_view: &wgpu::TextureView,
    staged: bool,
) -> Result<(), String> {
    let device = window.device;
    let s = &mut *window.surface;
    if let Some(overlay) = window.overlay {
        crate::overlay::record_overlay_on(
            overlay,
            device,
            encoder,
            image.texture.as_ref(),
            &image.view,
            image.format,
            s.config.width,
            s.config.height,
        );
    }
    if staged {
        let output = crate::display_stage::StageOutput::for_surface(s.config.format, s.config.color_space);
        s.display
            .record(
                &device.device,
                &device.queue,
                encoder,
                swap_view,
                s.config.format,
                output,
                window.calibration,
                window.pattern,
                crate::display_stage::StageSlot::Present,
            )
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// The pacer as it was before a tick fed it. One lives in the [`FrameGenPresenter`] and is
/// refilled every tick with `clone_from`, which reuses its interval history: a fresh clone
/// allocated on every frame-generation tick (rev-56 shader-present finding 1).
#[derive(Debug, Default)]
pub(crate) struct PacerBefore {
    pacer: crate::frame_gen::FramePacer,
    last: Option<PacingDecision>,
}

/// What one tick on this path told the pacer, kept apart from the device work so the accounting
/// is testable without a window (deep-zai-present finding 3, ADR-8930). The decision comes before
/// the presents, so the tick is fed first and settled once it is known whether a present was
/// queued.
#[derive(Default)]
struct TickFeed {
    /// [`Self::observe`] ran: the [`PacerBefore`] holds the pacer as it was before this tick.
    fed: bool,
    /// A present of this tick was queued on the present thread.
    presented: bool,
}

impl TickFeed {
    /// Feeds the tick's frame (finished at `ready_ns`) and returns the decision for its presents,
    /// keeping the pacer as it was in `before`. `paused`: the frame follows a long gap, so the
    /// history starts again first.
    fn observe(
        &mut self,
        before: &mut PacerBefore,
        pacing: &mut FrameGenPacing,
        ready_ns: u64,
        reset: bool,
        paused: bool,
        min_base_fps: f32,
    ) -> PacingDecision {
        before.pacer.clone_from(&pacing.pacer);
        before.last = pacing.last;
        self.fed = true;
        if paused {
            // A hidden window or a long stall: the last image is no neighbor of this one.
            pacing.pacer.reset_history();
        }
        pacing.observe(ready_ns, reset, true, min_base_fps)
    }

    /// A present of this tick was queued.
    fn presented(&mut self) {
        self.presented = true;
    }

    /// The tick is over; `failed`: it ended with an error, so the plain path presents its frame.
    ///
    /// A tick that queued a present keeps the frame it fed. One that did not takes it back from
    /// `before`, so no decision is left for presents that never came. Then the plain observer
    /// (`observe_presents`) feeds the frame of a failed tick, which the plain path presents, and
    /// skips a tick that showed nothing (a configure retry, a failed acquire).
    fn settle(self, before: &PacerBefore, pacing: &mut FrameGenPacing, failed: bool) {
        if !self.presented && self.fed {
            pacing.pacer.clone_from(&before.pacer);
            pacing.last = before.last;
        }
        pacing.fed_this_tick = self.presented || !failed;
    }
}

/// Test hook (`test-hooks`): one tick of the frame-generation present path as far as the pacer
/// sees it, without a window: [`feed_frame_gen_tick`] feeds the tick's frame and settles it the way
/// the window's frame does around its device work.
#[cfg(feature = "test-hooks")]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameGenTick {
    /// When the tick's frame finished rendering, on the pacer's clock (nanoseconds).
    pub ready_ns: u64,
    /// A camera cut or a forced history reset.
    pub reset: bool,
    /// The frame follows a long gap: the pacer's history starts again.
    pub paused: bool,
    /// The settings' lowest base frame rate for generation.
    pub min_base_fps: f32,
    /// A present of the tick was queued on the present thread.
    pub presented: bool,
    /// The tick ended with an error (the plain path presents its frame).
    pub failed: bool,
}

/// Test hook (`test-hooks`): runs the pacer accounting of one frame-generation tick (`TickFeed`,
/// as the window's frame runs it) on `presenter` and `pacing`, and returns the tick's decision.
/// Steady-allocation tests drive the per-tick CPU work of the frame-generation present path with
/// it; the device work needs a window.
#[cfg(feature = "test-hooks")]
pub fn feed_frame_gen_tick(
    presenter: &mut FrameGenPresenter,
    pacing: &mut FrameGenPacing,
    tick: FrameGenTick,
) -> PacingDecision {
    let mut feed = TickFeed::default();
    let decision = feed.observe(
        &mut presenter.pacer_before,
        pacing,
        tick.ready_ns,
        tick.reset,
        tick.paused,
        tick.min_base_fps,
    );
    if tick.presented {
        feed.presented();
    }
    feed.settle(&presenter.pacer_before, pacing, tick.failed);
    decision
}

fn render_frame(
    window: &mut FrameGenWindow<'_>,
    settings: &FrameGenSettings,
    pacing: &mut FrameGenPacing,
    presenter: &mut FrameGenPresenter,
) -> Result<FrameGenOutcome, String> {
    let mut tick = TickFeed::default();
    let result = render_tick(window, settings, pacing, presenter, &mut tick);
    tick.settle(&presenter.pacer_before, pacing, result.is_err());
    result
}

fn render_tick(
    window: &mut FrameGenWindow<'_>,
    settings: &FrameGenSettings,
    pacing: &mut FrameGenPacing,
    presenter: &mut FrameGenPresenter,
    tick: &mut TickFeed,
) -> Result<FrameGenOutcome, String> {
    let device = window.device;
    let mut outcome = FrameGenOutcome::default();
    if window.surface.needs_configure {
        // The worker may still hold an image of the old configuration.
        presenter.drain();
        let s = &*window.surface;
        if configure_surface(&device.device, &s.surface, &s.config) == super::ConfigureOutcome::Retry {
            return Ok(outcome);
        }
        window.surface.needs_configure = false;
        presenter.force_reset = true;
    }
    let (width, height) = (window.surface.config.width, window.surface.config.height);
    let output = crate::display_stage::StageOutput::for_surface(
        window.surface.config.format,
        window.surface.config.color_space,
    );
    let stage_format = if output.is_hdr() {
        Some(crate::display_stage::HDR_INTERMEDIATE)
    } else if !window.calibration.is_sdr_neutral() {
        Some(crate::display_stage::SDR_INTERMEDIATE)
    } else {
        None
    };
    if stage_format.is_none() {
        window.surface.display.release();
    }
    let draw_format = stage_format.unwrap_or(window.surface.config.format);
    let stale = presenter
        .scene
        .as_ref()
        .is_none_or(|images| !images.matches((width, height), draw_format));
    if stale {
        presenter.drain();
        presenter.scene = Some(SceneImages::new(&device.device, (width, height), draw_format));
        presenter.blit.clear_groups();
        presenter.force_reset = true;
    }
    let scene_images = presenter
        .scene
        .as_ref()
        .ok_or_else(|| "frame generation: no scene images".to_string())?;

    // ---- The scene, into the frame's own image ----
    let frame = window
        .frame
        .ok_or_else(|| "frame generation: no scene".to_string())?;
    let views = frame.window_views();
    let (view_index, _) = views
        .first()
        .ok_or_else(|| "frame generation: no view".to_string())?;
    window
        .runtime
        .0
        .set_display_headroom(output.is_hdr().then(|| window.calibration.hdr_headroom()));
    let current = scene_images.current();
    window.runtime.0.set_light_extras(frame.light_extras.clone());
    let scene = frame.frame_scene(*view_index, width, height);
    let target = FrameTarget::rect(
        &current.view,
        draw_format,
        width,
        height,
        [0, 0, width, height],
        false,
    )
    .with_color(Some(&current.texture));
    if let Err(err) = window.runtime.0.bake_pending_local_captures(device, &scene) {
        tracing::error!(target: "regia_render", "local capture bake failed: {err}");
    }
    window.runtime.0.retain_occlusion_views(frame.views.len());
    window.runtime.0.set_occlusion_view(*view_index);
    window
        .runtime
        .0
        .render_into(device, &scene, target)
        .map_err(|e| format!("render: {e}"))?;
    outcome.drew = true;

    // ---- The pacer's decision ----
    let cuts = window.runtime.0.camera_cut_count();
    let cut = presenter.cuts.is_some_and(|before| before != cuts);
    presenter.cuts = Some(cuts);
    let inputs = window.runtime.0.frame_gen_inputs();
    let ready = pacing.now_ns();
    let paused = presenter.last_ready_ns.is_some_and(|last| is_pause(last, ready));
    presenter.last_ready_ns = Some(ready);
    let reset = std::mem::take(&mut presenter.force_reset) || cut || paused || inputs.is_none();
    if reset {
        pacing.present.resets += 1;
    }
    let decision = tick.observe(
        &mut presenter.pacer_before,
        pacing,
        ready,
        reset,
        paused,
        settings.min_base_fps,
    );
    let (generated_at, real_at) = match decision {
        PacingDecision::Generate {
            generated_at_ns,
            real_at_ns,
            ..
        } => (Some(generated_at_ns), real_at_ns),
        PacingDecision::RealOnly { present_at_ns, .. } => (None, present_at_ns),
    };

    // ---- The interpolator ----
    let (transfer, min_max_luminance) = if output.is_hdr() {
        (
            TransferFunction::ScRgb,
            [0.0, 80.0 * window.calibration.hdr_headroom().max(1.0)],
        )
    } else {
        (TransferFunction::Srgb, [0.0, 80.0])
    };
    let scope = device.device.push_error_scope(wgpu::ErrorFilter::Validation);
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("frame_gen"),
        });
    if let Some(inputs) = inputs {
        // The numbers first: a size or camera the interpolator cannot take stops it here.
        let frame_time_ms = pacing
            .pacer
            .average_interval_ns()
            .map_or(16.0, |ns| ns as f32 / 1.0e6);
        FrameGenDispatch::new(
            (width, height),
            inputs.render_size,
            FrameGenCamera {
                near: inputs.near,
                far: inputs.far,
                fov_vertical: inputs.fov_y,
                view_space_to_meters: 1.0,
            },
            frame_time_ms.max(1.0e-3),
            reset,
        )
        .validate()
        .map_err(|e| format!("dispatch: {}", e.code()))?;
        let input = FrameGenFrame {
            current_color: &current.raw_view,
            previous_color: &scene_images.previous().raw_view,
            depth: &inputs.depth,
            velocity: inputs.velocity.as_ref(),
            reactive: inputs.reactive.as_ref(),
            render_size: inputs.render_size,
            display_size: (width, height),
            view_proj: inputs.view_proj,
            unjittered_view_proj: inputs.unjittered_view_proj,
            velocity_prev_view_proj: inputs.velocity_prev_view_proj,
            eye: inputs.eye,
            fov_y: inputs.fov_y,
            transfer,
            min_max_luminance,
            reset,
            debug_tear_lines: settings.debug_tear_lines,
            debug_view: settings.debug_view,
        };
        presenter
            .passes
            .record(
                &device.device,
                &device.queue,
                &mut encoder,
                &input,
                generated_at.is_some(),
            )
            .map_err(|e| format!("interpolator: {e}"))?;
    }
    device.queue.submit([encoder.finish()]);
    crate::breadcrumbs::note_untracked_submit(&device.queue);
    if let Some(error) = regia_core::tasks::block_on(scope.pop()) {
        return Err(format!("interpolator: {error}"));
    }

    // ---- The worker (started on first use) ----
    if presenter.worker.is_none() {
        let queue = device.queue.clone();
        let worker = PresentWorker::new(pacing.epoch(), move |image: wgpu::SurfaceTexture| {
            queue.present(image);
            crate::scene_ready::note_present_done();
        })
        .map_err(|e| format!("present thread: {e}"))?;
        presenter.worker = Some(worker);
    }
    let Some(worker) = presenter.worker.as_ref() else {
        return Err("frame generation: no present thread".to_string());
    };
    // The real image of the last tick is on its way; the swap chain gives one image at a time.
    worker.wait_idle();

    // ---- The generated present ----
    let output_view = presenter.passes.output_view().cloned();
    let debug = settings.debug_view && generated_at.is_some();
    if let (Some(at), Some(generated), false) = (generated_at, output_view.as_ref(), debug) {
        let image = match acquire(window.surface) {
            Ok(image) => image,
            Err(lost) => {
                outcome.lost = lost;
                pacing.present.acquire_failures += 1;
                presenter.force_reset = true;
                presenter.advance_scene();
                return Ok(outcome);
            }
        };
        outcome.acquired = true;
        let swap_view = image.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let composed = composition(window, &image, &swap_view, stage_format);
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame_gen generated"),
            });
        presenter
            .blit
            .record(
                &device.device,
                &mut encoder,
                generated,
                BlitSource::Interpolated,
                &composed.view,
                composed.format,
            )
            .map_err(|e| format!("blit: {e}"))?;
        finish_composition(
            window,
            &mut encoder,
            &composed,
            &swap_view,
            stage_format.is_some(),
        )?;
        device.queue.submit([encoder.finish()]);
        crate::breadcrumbs::note_untracked_submit(&device.queue);
        if worker.submit(image, at).is_err() {
            return Err("the present thread stopped".to_string());
        }
        tick.presented();
        crate::scene_ready::note_present_queued();
        worker.wait_idle();
        pacing.present.generated_presents += 1;
    }

    // ---- The rendered frame, or under the debug view the generated one again ----
    let image = match acquire(window.surface) {
        Ok(image) => image,
        Err(lost) => {
            outcome.lost = lost;
            pacing.present.acquire_failures += 1;
            presenter.force_reset = true;
            presenter.advance_scene();
            return Ok(outcome);
        }
    };
    outcome.acquired = true;
    let swap_view = image.texture.create_view(&wgpu::TextureViewDescriptor::default());
    let composed = composition(window, &image, &swap_view, stage_format);
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("frame_gen real"),
        });
    let (source, kind) = match (debug, output_view.as_ref()) {
        (true, Some(generated)) => (generated, BlitSource::Interpolated),
        _ => (&current.view, BlitSource::SameFormat),
    };
    presenter
        .blit
        .record(
            &device.device,
            &mut encoder,
            source,
            kind,
            &composed.view,
            composed.format,
        )
        .map_err(|e| format!("blit: {e}"))?;
    finish_composition(
        window,
        &mut encoder,
        &composed,
        &swap_view,
        stage_format.is_some(),
    )?;
    device.queue.submit([encoder.finish()]);
    crate::breadcrumbs::note_untracked_submit(&device.queue);

    // Screenshots see this image (the agent asks for the debug view to see a generated one).
    let callbacks = if window.screenshots.is_empty() {
        Vec::new()
    } else {
        window.screenshots.take_with_options(window.entity)
    };
    let readbacks = encode_screenshot_copies(
        device,
        window.surface,
        &image.texture,
        (stage_format.is_some(), output, window.calibration),
        (
            callbacks.iter().any(|(o, _)| o.neutral),
            callbacks.iter().any(|(o, _)| !o.neutral),
        ),
    );
    if let Some(os) = window.handles.get(window.entity)
        && crate::swapchain::wants_frame_callback(
            window.focused,
            window.surface.info.effective_present_mode(),
        )
    {
        os.pre_present_notify();
    }
    let (callbacks, scene_ticket) = submit_or_requeue(
        worker,
        (image, real_at),
        window.screenshots,
        window.entity,
        callbacks,
    )?;
    tick.presented();
    outcome.scene_ticket = Some(scene_ticket);
    let (neutral, applied): (Vec<_>, Vec<_>) = callbacks.into_iter().partition(|(o, _)| o.neutral);
    let neutral: Vec<_> = neutral.into_iter().map(|(_, cb)| cb).collect();
    let applied: Vec<_> = applied.into_iter().map(|(_, cb)| cb).collect();
    pacing.present.real_presents += 1;
    pacing.present.gaps = worker.gaps();
    crate::boot::mark(crate::boot::BootMark::FirstPresent);
    for (callbacks, readback) in [(neutral, readbacks.0), (applied, readbacks.1)] {
        if callbacks.is_empty() {
            continue;
        }
        let result = match readback {
            Some(readback) => crate::screenshot::finish(device, readback).map_err(|e| e.to_string()),
            None => Err("this surface cannot be read back (no COPY_SRC support)".to_string()),
        };
        for callback in callbacks {
            callback(result.clone());
        }
    }
    presenter.advance_scene();
    Ok(outcome)
}

/// A window's screenshot requests with their options ([`crate::ScreenshotRequests::take_with_options`]).
type WindowScreenshots = Vec<(crate::ScreenshotOptions, crate::ScreenshotCallback)>;

/// Hands `image` to the present thread (to show at `at_ns`) and returns `callbacks` to serve from
/// its readback, with the image's present ticket. When the thread has stopped, the requests go
/// back to `screenshots` for `window`: the plain present that follows on this tick serves them
/// (audit G render5 #12).
fn submit_or_requeue<T: Send + 'static>(
    worker: &PresentWorker<T>,
    (image, at_ns): (T, u64),
    screenshots: &mut crate::ScreenshotRequests,
    window: Entity,
    callbacks: WindowScreenshots,
) -> Result<(WindowScreenshots, u64), String> {
    if worker.submit(image, at_ns).is_err() {
        screenshots.requeue(window, callbacks);
        return Err("the present thread stopped".to_string());
    }
    let ticket = crate::scene_ready::note_present_queued();
    Ok((callbacks, ticket))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Audit G render5 #12: when the present thread has stopped, the window's screenshot requests
    /// go back to the queue, so the plain present that follows on the same tick serves them; they
    /// used to be dropped with the error and the agent waited for good.
    #[test]
    fn a_stopped_present_thread_requeues_the_screenshot_requests() {
        let worker = PresentWorker::new(std::time::Instant::now(), |id: u32| {
            assert_ne!(id, 1, "simulated present failure");
        })
        .unwrap_or_else(|e| panic!("{e}"));
        assert!(worker.submit(1, 0).is_ok());
        worker.wait_idle();
        let mut screenshots = crate::ScreenshotRequests::default();
        let window = Entity::PLACEHOLDER;
        let (tx, rx) = std::sync::mpsc::channel();
        screenshots.request_with(
            Some(window),
            crate::ScreenshotOptions { neutral: true },
            Box::new(move |result| {
                let _ = tx.send(result.map(|image| image.width));
            }),
        );
        let taken = screenshots.take_with_options(window);
        assert_eq!(taken.len(), 1);
        let result = submit_or_requeue(&worker, (2, 0), &mut screenshots, window, taken);
        assert!(result.is_err(), "a stopped worker took the frame");
        let back = screenshots.take_with_options(window);
        assert_eq!(back.len(), 1, "the screenshot request was dropped with the frame");
        assert!(back[0].0.neutral, "the request lost its options");
        for (_, callback) in back {
            callback(Ok(crate::CapturedImage {
                width: 3,
                height: 1,
                rgba: vec![0; 12],
            }));
        }
        assert_eq!(rx.try_recv().ok(), Some(Ok(3)));
    }
    use wgpu::PresentMode as Mode;

    const FRAME_NS: u64 = 10_000_000;

    /// A pacer past its warm-up at 100 fps, so a tick's decision is `Generate`.
    fn warm_pacing() -> FrameGenPacing {
        let mut pacing = FrameGenPacing::default();
        for frame in 0..30 {
            pacing.observe(frame * FRAME_NS, false, true, 30.0);
        }
        pacing
    }

    /// What `observe_presents` does after the window present: a tick nobody fed is fed as a
    /// plain frame.
    fn observe_plain(pacing: &mut FrameGenPacing, now_ns: u64) {
        if !std::mem::take(&mut pacing.fed_this_tick) {
            pacing.observe(now_ns, false, false, 30.0);
        }
    }

    /// The pacer as a value to compare (it has no `PartialEq`).
    fn state(pacing: &FrameGenPacing) -> String {
        format!("{:?} {:?}", pacing.pacer, pacing.last)
    }

    /// deep-zai-present finding 3 (b): a tick that fails after the pacer's decision presents its
    /// frame plainly. The pacer must see that frame once, as a plain one: as if the frame
    /// generation tick never happened, not a `Generate` decision whose presents never came (it
    /// moved the next generated target half an interval late).
    #[test]
    fn a_tick_that_fails_after_the_decision_is_fed_once_as_a_plain_frame() {
        let mut pacing = warm_pacing();
        let mut control = warm_pacing();
        let ready = 30 * FRAME_NS;
        let mut before = PacerBefore::default();
        let mut tick = TickFeed::default();
        let decision = tick.observe(&mut before, &mut pacing, ready, false, false, 30.0);
        assert!(
            matches!(decision, PacingDecision::Generate { .. }),
            "{decision:?}"
        );
        // The interpolator, a blit or the worker failed before any present was queued.
        tick.settle(&before, &mut pacing, true);
        observe_plain(&mut pacing, ready);
        observe_plain(&mut control, ready);
        assert_eq!(
            state(&pacing),
            state(&control),
            "the failed tick left its decision in the pacer"
        );
        assert_eq!(pacing.pacer.stats().real_frames, 31, "fed once");
    }

    /// deep-zai-present finding 3 (a), and a failed acquire: a tick that showed no image (a
    /// configure retry before the decision, an acquire that failed after it) is fed to nobody.
    #[test]
    fn a_tick_that_shows_nothing_is_not_fed() {
        let before = state(&warm_pacing());
        // A configure retry returns before the decision.
        let mut pacing = warm_pacing();
        let mut kept = PacerBefore::default();
        TickFeed::default().settle(&kept, &mut pacing, false);
        observe_plain(&mut pacing, 30 * FRAME_NS);
        assert_eq!(state(&pacing), before, "a configure retry was counted as a frame");
        // The swap chain gave no image after the decision.
        let mut tick = TickFeed::default();
        tick.observe(&mut kept, &mut pacing, 30 * FRAME_NS, false, false, 30.0);
        tick.settle(&kept, &mut pacing, false);
        observe_plain(&mut pacing, 30 * FRAME_NS);
        assert_eq!(
            state(&pacing),
            before,
            "a tick without an image was counted as a frame"
        );
    }

    /// A tick that queued a present keeps its decision, and the observer leaves it alone (also
    /// when a later step of the tick failed and the plain path presents again).
    #[test]
    fn a_tick_that_presented_is_fed_once_with_its_decision() {
        for failed_later in [false, true] {
            let mut pacing = warm_pacing();
            let ready = 30 * FRAME_NS;
            let mut before = PacerBefore::default();
            let mut tick = TickFeed::default();
            let decision = tick.observe(&mut before, &mut pacing, ready, false, false, 30.0);
            tick.presented();
            tick.settle(&before, &mut pacing, failed_later);
            observe_plain(&mut pacing, ready);
            assert_eq!(pacing.last, Some(decision), "failed later: {failed_later}");
            assert_eq!(
                pacing.pacer.stats().real_frames,
                31,
                "failed later: {failed_later}"
            );
        }
    }

    #[test]
    fn only_a_vsync_window_with_mailbox_on_offer_moves_to_it() {
        let offered = [Mode::Fifo, Mode::Mailbox, Mode::Immediate];
        assert!(wants_mailbox(Mode::Fifo, &offered));
        assert!(wants_mailbox(Mode::FifoRelaxed, &offered));
        // An uncapped host keeps its choice, and a surface without Mailbox stays on vsync.
        assert!(!wants_mailbox(Mode::Immediate, &offered));
        assert!(!wants_mailbox(Mode::Mailbox, &offered));
        assert!(!wants_mailbox(Mode::Fifo, &[Mode::Fifo, Mode::Immediate]));
    }

    /// deep-zai-present finding 1 (ADR-7413): `sync_surface_output` re-plans the swap chain
    /// while frame generation holds Mailbox (the host switched to no-vsync, toggled HDR, the
    /// display's HDR mode changed). The re-plan is the host's current choice: stopping frame
    /// generation must leave it, not put back the mode held before the re-plan.
    #[test]
    fn a_replan_while_mailbox_is_held_keeps_the_hosts_new_mode() {
        let offered = [Mode::Fifo, Mode::FifoRelaxed, Mode::Mailbox, Mode::Immediate];
        let effective = |mode| crate::swapchain::effective_present_mode(mode, &offered);
        let mut held = HeldPresentMode::default();
        // The host runs vsync; frame generation moves the window to Mailbox.
        let (mut plan, mut epoch) = (Mode::AutoVsync, 1);
        if let Some(mode) = held.hold(plan, effective(plan), &offered, epoch) {
            plan = mode;
        }
        assert_eq!(plan, Mode::Mailbox);
        // A held frame keeps Mailbox and remembers the host's vsync.
        assert_eq!(held.hold(plan, effective(plan), &offered, epoch), None);
        // The host switches to no-vsync: the re-plan writes its mode with a new epoch.
        (plan, epoch) = (Mode::AutoNoVsync, 2);
        // Frame generation keeps running on the uncapped window, then stops.
        assert_eq!(held.hold(plan, effective(plan), &offered, epoch), None);
        if let Some(mode) = held.release(epoch) {
            plan = mode;
        }
        assert_eq!(
            plan,
            Mode::AutoNoVsync,
            "the host's newer present mode was reverted"
        );

        // A re-plan back to vsync (an HDR toggle) is held again, under the new plan, and
        // released to it; a release right after a re-plan restores nothing.
        let mut held = HeldPresentMode::default();
        assert_eq!(
            held.hold(Mode::Fifo, Mode::Fifo, &offered, 3),
            Some(Mode::Mailbox)
        );
        assert_eq!(
            held.hold(Mode::FifoRelaxed, Mode::FifoRelaxed, &offered, 4),
            Some(Mode::Mailbox)
        );
        assert_eq!(held.release(4), Some(Mode::FifoRelaxed));
        assert_eq!(
            held.hold(Mode::Fifo, Mode::Fifo, &offered, 5),
            Some(Mode::Mailbox)
        );
        assert_eq!(
            held.release(6),
            None,
            "a re-planned surface already has the host's mode"
        );
        // Without a re-plan the held mode comes back once.
        assert_eq!(
            held.hold(Mode::AutoVsync, Mode::Fifo, &offered, 7),
            Some(Mode::Mailbox)
        );
        assert_eq!(held.release(7), Some(Mode::AutoVsync));
        assert_eq!(held.release(7), None);
    }

    /// A swap-chain plan presenting with `mode` on a surface that offers `offered`.
    fn plan_info(mode: Mode, offered: &[Mode]) -> crate::swapchain::SwapchainInfo {
        crate::swapchain::SwapchainInfo {
            request: crate::swapchain::SwapchainRequest {
                backend: wgpu::Backend::Vulkan,
                present: crate::output::PresentPolicy::Vsync,
                window_present: regia_platform::PresentMode::Vsync,
                hdr: crate::output::HdrOutput::Sdr,
                display_hdr: None,
            },
            plan: crate::swapchain::SwapchainPlan {
                format: wgpu::TextureFormat::Bgra8UnormSrgb,
                color_space: wgpu::SurfaceColorSpace::Auto,
                present_mode: mode,
                alpha_mode: wgpu::CompositeAlphaMode::Opaque,
                frame_latency: 2,
                copy_src: true,
                hdr_active: false,
                hdr_fallback: None,
            },
            size: (64, 64),
            offered_present_modes: offered.to_vec(),
            offered_hdr: vec![crate::output::HdrOutput::Sdr],
            display: crate::swapchain::DisplayHdrSummary::default(),
        }
    }

    /// rev-zai-rc-b package-present 2 (w9-pack-stamp-and-content): `apply_plan` (what
    /// `sync_surface_output` runs for each re-plan) takes a new plan epoch, and the hold and the
    /// restore read the surface's epoch. Without the bump the restore put the vsync mode held
    /// before a re-plan back over the host's newer no-vsync choice (the P1 revert, ADR-7413); the
    /// `HeldPresentMode` test above passes epochs by hand, so it did not see a missing bump.
    #[test]
    fn a_replan_takes_a_new_epoch_so_the_held_mode_is_not_restored_over_it() {
        let offered = [Mode::Fifo, Mode::FifoRelaxed, Mode::Mailbox, Mode::Immediate];
        let mut info = plan_info(Mode::AutoVsync, &offered);
        let mut config = info.plan.configuration(64, 64);
        let mut needs_configure = false;
        let mut epoch = 0u64;
        let mut held = HeldPresentMode::default();
        macro_rules! fields {
            () => {
                PlanFields {
                    config: &mut config,
                    needs_configure: &mut needs_configure,
                    info: &mut info,
                    plan_epoch: &mut epoch,
                }
            };
        }

        // The window's plan: the host runs vsync. Frame generation moves it to Mailbox.
        super::super::apply_plan(fields!(), plan_info(Mode::AutoVsync, &offered));
        let first = epoch;
        assert_ne!(first, 0, "a plan takes an epoch");
        hold_mailbox(fields!(), &mut held);
        assert_eq!(config.present_mode, Mode::Mailbox);
        assert_eq!(info.plan.present_mode, Mode::Mailbox);
        assert!(needs_configure);

        // The host switches to no-vsync while frame generation runs: the re-plan writes its mode.
        needs_configure = false;
        super::super::apply_plan(fields!(), plan_info(Mode::AutoNoVsync, &offered));
        assert_ne!(epoch, first, "the re-plan kept the old plan's epoch");
        assert_eq!(config.present_mode, Mode::AutoNoVsync);
        assert!(needs_configure, "a changed present mode needs a configure");
        // Frame generation keeps running on the uncapped window, then stops.
        hold_mailbox(fields!(), &mut held);
        assert_eq!(config.present_mode, Mode::AutoNoVsync);
        restore_host_mode(fields!(), &mut held);
        assert_eq!(
            config.present_mode,
            Mode::AutoNoVsync,
            "the vsync mode held before the re-plan was put back over the host's choice"
        );
        assert_eq!(info.plan.present_mode, Mode::AutoNoVsync);

        // Without a re-plan the held mode comes back when frame generation stops.
        super::super::apply_plan(fields!(), plan_info(Mode::AutoVsync, &offered));
        hold_mailbox(fields!(), &mut held);
        assert_eq!(config.present_mode, Mode::Mailbox);
        restore_host_mode(fields!(), &mut held);
        assert_eq!(config.present_mode, Mode::AutoVsync);
        assert_eq!(info.plan.present_mode, Mode::AutoVsync);
    }

    #[test]
    fn half_a_second_between_frames_is_a_pause() {
        assert!(!is_pause(1_000, 1_000 + 16_000_000));
        assert!(!is_pause(1_000, 1_000 + 500_000_000));
        assert!(is_pause(1_000, 1_000 + 500_000_001));
        // A clock that did not move is no pause.
        assert!(!is_pause(5, 5));
    }

    #[test]
    fn a_failed_acquire_says_whether_the_surface_is_lost() {
        let outcome = FrameGenOutcome {
            lost: true,
            ..FrameGenOutcome::default()
        };
        let mut lost = Vec::new();
        let entity = Entity::from_raw_u32(7).unwrap_or_else(|| panic!("a valid index"));
        assert!(outcome.note(entity, &mut lost));
        assert_eq!(lost, [entity]);
        assert!(!FrameGenOutcome::default().note(entity, &mut lost));
        assert_eq!(lost.len(), 1);
    }

    /// An open prepare whose scene is resident (no started warm queue: nothing can warm, so a
    /// renderable frame is resident at once).
    fn resident_prepare(map_id: &str) -> crate::GameFrameWarm {
        let mut gw = crate::GameFrameWarm::default();
        let mut readiness = crate::SceneReadiness::default();
        readiness.prepare(map_id);
        let (seq, map_id) = readiness
            .take_request()
            .unwrap_or_else(|| panic!("the prepare is queued"));
        gw.open_prepare(seq, map_id, std::time::Instant::now());
        let queue = crate::PipelineWarmQueue::new(1);
        let runtime = crate::FrameRenderer::new();
        let scene = crate::pipeline_warm::canonical_frame();
        gw.note_scene(Some(&queue), &runtime, Some(&scene), true);
        assert!(!gw.holds_scene(), "the resident scene draws");
        gw
    }

    fn prepare_state(gw: &crate::GameFrameWarm) -> crate::SceneReadyState {
        gw.prepare_status()
            .unwrap_or_else(|| panic!("an open prepare"))
            .state
    }

    /// Review rev-d row 3 (ADR-8122): the scene drew, the generated image was queued on the
    /// present thread, then the real image's acquire failed. The present thread presenting the
    /// generated image (every ticket queued so far) must not make the prepare `presented`: the
    /// scene's own image never reached the screen.
    #[test]
    fn a_failed_real_acquire_after_a_generated_present_leaves_the_prepare_resident() {
        let mut gw = resident_prepare("fg_failed_acquire");
        // The generated image of this tick is queued (`render_frame`'s generated present).
        let generated = crate::scene_ready::note_present_queued();
        assert!(generated > 0);
        // The real acquire failed: `render_frame` returns the outcome with the scene drawn.
        let outcome = FrameGenOutcome {
            drew: true,
            acquired: true,
            ..FrameGenOutcome::default()
        };
        outcome.note_warm(None, Some(&mut gw), std::time::Instant::now(), (0, 0));
        // The present thread presented everything it was given (the generated image).
        let flushes = crate::scene_ready::present_progress().1;
        gw.begin_tick_with(u64::MAX, flushes, std::time::Instant::now());
        assert_eq!(
            prepare_state(&gw),
            crate::SceneReadyState::Resident,
            "the generated image's present counted as the scene's"
        );
    }

    /// The same prepare's next tick acquires both images: it is `presented` once the present
    /// thread has presented the scene image's own ticket, not before.
    #[test]
    fn the_scene_images_ticket_presents_the_prepare() {
        let mut gw = resident_prepare("fg_scene_ticket");
        let dropped = FrameGenOutcome {
            drew: true,
            acquired: true,
            ..FrameGenOutcome::default()
        };
        dropped.note_warm(None, Some(&mut gw), std::time::Instant::now(), (0, 0));
        let generated = crate::scene_ready::note_present_queued();
        let scene = crate::scene_ready::note_present_queued();
        let outcome = FrameGenOutcome {
            drew: true,
            acquired: true,
            scene_ticket: Some(scene),
            ..FrameGenOutcome::default()
        };
        outcome.note_warm(None, Some(&mut gw), std::time::Instant::now(), (0, 0));
        let flushes = crate::scene_ready::present_progress().1;
        gw.begin_tick_with(generated, flushes, std::time::Instant::now());
        assert_eq!(
            prepare_state(&gw),
            crate::SceneReadyState::Resident,
            "presented before the scene image"
        );
        gw.begin_tick_with(scene, flushes, std::time::Instant::now());
        assert_eq!(prepare_state(&gw), crate::SceneReadyState::Presented);
    }
}
